//! `runway preview`: list branch previews, delete some, or prune those of
//! merged or deleted branches.
//!
//! A preview is a traffic tag whose revision carries runway's preview marker
//! (`runway.dev/preview: TAG`, set by `deploy --preview`). Other tags
//! (`canary`, tags created by hand) are never touched by `prune`.

use crate::build_client;
use crate::cli::{Context, PreviewAction, PreviewDeleteArgs, PreviewPruneArgs};
use crate::commands::traffic::{existing_tag, update_traffic};
use crate::commands::{load, not_deployed};
use crate::config::{Deployment, Overrides};
use crate::deploy::{Reconciler, check_ownership};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::{api_error, is_not_found, run};
use crate::output::{OutputFormat, print_json};
use crate::poll::PollConfig;
use crate::traffic::{self, CANARY_TAG, Current, Target};
use google_cloud_lro::Poller;
use google_cloud_run_v2::client::{Jobs, Revisions, Services};
use google_cloud_run_v2::model::Service;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

/// Revision annotation set by `deploy --preview`.
pub const PREVIEW_ANNOTATION: &str = "runway.dev/preview";

#[derive(Debug, Clone, Serialize)]
pub struct PreviewLine {
    pub tag: String,
    pub revision: String,
    pub url: String,
    /// Created by `deploy --preview` (as opposed to `canary` or a manual tag).
    pub preview: bool,
    /// `kept`, `removed`, `would_remove` or `not_found` (delete/prune).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_deleted: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct PreviewReport {
    pub service: String,
    /// `list`, `delete` or `prune`.
    pub action: &'static str,
    /// False without `--yes` (nothing was changed).
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub previews: Vec<PreviewLine>,
}

pub async fn run(ctx: &Context, action: PreviewAction) -> Result<()> {
    let (stage, only) = match &action {
        PreviewAction::List(a) => (&a.stage.stage, &a.stage.only),
        PreviewAction::Delete(a) => (&a.stage.stage, &a.stage.only),
        PreviewAction::Prune(a) => (&a.stage.stage, &a.stage.only),
    };
    let resolved = load(ctx, stage, &Overrides::default())?;
    let selected = resolved.select(only)?;
    let basis = crate::commands::preview_basis(&resolved);
    let session = crate::commands::connect(ctx, resolved.first()).await?;
    let services = build_client!(Services, session)?;
    let revisions = build_client!(Revisions, session)?;
    let jobs = build_client!(Jobs, session)?;
    let timeout = match &action {
        PreviewAction::List(_) => std::time::Duration::from_secs(60),
        PreviewAction::Delete(a) => a.timeout,
        PreviewAction::Prune(a) => a.timeout,
    };
    let rec = Reconciler {
        run: &services,
        revisions: Some(&revisions),
        progress: &ctx.progress,
        poll: PollConfig::default(),
        timeout,
    };
    // Branches are read once, for every service and job.
    let info = match &action {
        PreviewAction::Prune(a) => {
            let dir = ctx
                .config
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Some(branch_info(
                dir,
                &a.remote,
                a.base.as_deref(),
                !a.no_fetch,
                &ctx.progress,
            )?)
        }
        _ => None,
    };
    let several = selected.len() > 1;
    // Deleting previews shares the stage with other previews (each removes
    // only its own tags) and waits for deploys and traffic changes.
    let lock = match &action {
        PreviewAction::Delete(a) if a.yes => Some((&a.lock, "delete")),
        PreviewAction::Prune(a) if a.yes => Some((&a.lock, "prune")),
        _ => None,
    };
    let lease = match lock {
        None => crate::lease::Guard::none(),
        Some((lock, verb)) => {
            crate::commands::take_lease(
                &resolved,
                &services,
                Some(&jobs),
                crate::lease::Mode::Shared,
                &format!("runway preview {verb} --stage {stage}"),
                lock,
                &ctx.progress,
            )
            .await?
        }
    };
    let held = lease.held();
    let outcome = async {
        let mut reports = Vec::new();
        for d in selected.iter().filter(|d| !d.is_job()) {
            held.check()?;
            let svc = match rec.get(&d.service_name()).await? {
                Some(svc) => svc,
                None if several => {
                    ctx.progress
                        .info(format!("{} is not deployed", d.service_id));
                    continue;
                }
                None => return Err(not_deployed(&d.service_id, &d.stage)),
            };
            check_ownership(&svc, &d.app, &d.stage, false)?;
            let lines = tagged_lines(&svc, &revisions).await?;
            reports.push(match &action {
                PreviewAction::List(_) => PreviewReport {
                    service: d.service_id.clone(),
                    action: "list",
                    applied: false,
                    base: None,
                    notes: vec![],
                    previews: lines,
                },
                PreviewAction::Delete(a) => {
                    delete(ctx, d, &rec, &revisions, &svc, lines, a, &basis).await?
                }
                PreviewAction::Prune(a) => {
                    let info = info.as_ref().expect("read for prune");
                    prune(ctx, d, &rec, &revisions, &svc, lines, a, info, &basis).await?
                }
            });
        }
        for d in selected.iter().filter(|d| d.is_job()) {
            held.check()?;
            let r = preview_jobs(ctx, d, &jobs, &action, info.as_ref(), &basis).await?;
            if !r.previews.is_empty() || !several {
                reports.push(r);
            }
        }
        match (ctx.output, reports.len()) {
            (OutputFormat::Json, 1) => print_json(&reports[0]),
            (OutputFormat::Json, _) => print_json(&reports),
            (OutputFormat::Text, _) => reports.iter().for_each(print_text),
        }
        Ok(())
    }
    .await;
    lease.release(&ctx.progress).await;
    outcome
}

/// Preview copies of a job (`deploy --preview` creates `{job}-{tag}`), as
/// report lines: listed, or deleted with their preview.
async fn preview_jobs(
    ctx: &Context,
    d: &Deployment,
    jobs: &Jobs,
    action: &PreviewAction,
    info: Option<&BranchInfo>,
    basis: &str,
) -> Result<PreviewReport> {
    let mut found = Vec::new();
    let mut token = String::new();
    loop {
        let resp = jobs
            .list_jobs()
            .set_parent(d.parent())
            .set_page_token(token.clone())
            .send()
            .await
            .map_err(|e| api_error(e, "listing Cloud Run jobs"))?;
        for j in resp.jobs {
            let l = &j.labels;
            let ours = run::ownership_of(l, &d.app, &d.stage) == run::Ownership::Owned
                && l.get(crate::naming::LABEL_NAME).map(String::as_str) == Some(d.name());
            if let (true, Some(tag)) = (ours, l.get(crate::naming::LABEL_PREVIEW)) {
                found.push((tag.clone(), j.name.clone()));
            }
        }
        if resp.next_page_token.is_empty() {
            break;
        }
        token = resp.next_page_token;
    }
    let (kind, applied, names, base) = match action {
        PreviewAction::List(_) => ("list", false, None, None),
        PreviewAction::Delete(a) => ("delete", a.yes, Some(&a.names), None),
        PreviewAction::Prune(a) => ("prune", a.yes, None, info.map(|i| i.base.clone())),
    };
    let mut previews = Vec::new();
    for (tag, name) in found {
        let decision: Option<(bool, Option<String>)> = match action {
            PreviewAction::List(_) => Some((false, None)),
            PreviewAction::Delete(_) => names.and_then(|ns| {
                ns.iter()
                    .any(|n| {
                        n == &tag || traffic::preview_tag(n, basis).ok().as_ref() == Some(&tag)
                    })
                    .then_some((true, None))
            }),
            PreviewAction::Prune(a) => {
                let info = info.expect("read for prune");
                Some(match prune_reason(&tag, basis, info, a.only_merged) {
                    Some(reason) => (true, Some(reason)),
                    None => (false, Some("branch still open".into())),
                })
            }
        };
        let Some((remove, reason)) = decision else {
            continue;
        };
        let status = match (kind, remove, applied) {
            ("list", _, _) => None,
            (_, false, _) => Some("kept"),
            (_, true, false) => Some("would_remove"),
            (_, true, true) => Some("removed"),
        };
        if status == Some("removed") {
            ctx.progress.step(format!(
                "Deleting preview job {}",
                run::short_revision(&name)
            ));
            let lro = jobs.delete_job().set_name(&name).send().await;
            match lro {
                Ok(_) => {}
                Err(e) if is_not_found(&e) => {}
                Err(e) => return Err(api_error(e, &format!("deleting job {name}"))),
            }
        }
        previews.push(PreviewLine {
            tag,
            revision: run::short_revision(&name).to_string(),
            url: String::new(),
            preview: true,
            status,
            reason,
            revision_deleted: None,
        });
    }
    Ok(PreviewReport {
        service: d.service_id.clone(),
        action: kind,
        applied,
        base,
        notes: vec![],
        previews,
    })
}

/// Every tagged traffic entry, with its revision, URL and preview marker.
async fn tagged_lines(svc: &Service, revisions: &Revisions) -> Result<Vec<PreviewLine>> {
    let cur = run::current_traffic(svc);
    let urls = run::status(svc, "", "").traffic;
    let mut out = Vec::new();
    for e in cur.entries.iter().filter(|e| !e.tag.is_empty()) {
        let revision = revision_of(&cur, &e.target);
        let preview = match revision_annotation(svc, revisions, &revision).await? {
            Some(v) => v == e.tag,
            None => false,
        };
        out.push(PreviewLine {
            tag: e.tag.clone(),
            url: urls
                .iter()
                .find(|l| l.tag == e.tag)
                .map(|l| l.uri.clone())
                .unwrap_or_default(),
            revision,
            preview,
            status: None,
            reason: None,
            revision_deleted: None,
        });
    }
    Ok(out)
}

fn revision_of(cur: &Current, t: &Target) -> String {
    match t {
        Target::Latest => cur.latest_ready.clone(),
        Target::Revision(r) => r.clone(),
    }
}

fn revision_name(svc: &Service, short: &str) -> String {
    format!("{}/revisions/{short}", svc.name)
}

/// The revision's preview marker (`None`: no marker, or revision gone).
async fn revision_annotation(
    svc: &Service,
    revisions: &Revisions,
    short: &str,
) -> Result<Option<String>> {
    if short.is_empty() {
        return Ok(None);
    }
    match revisions
        .get_revision()
        .set_name(revision_name(svc, short))
        .send()
        .await
    {
        Ok(r) => Ok(r.annotations.get(PREVIEW_ANNOTATION).cloned()),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(api_error(e, &format!("reading revision {short}"))
            .hint("the deploying principal needs run.revisions.get")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn delete(
    ctx: &Context,
    d: &Deployment,
    rec: &Reconciler<'_>,
    revisions: &Revisions,
    svc: &Service,
    mut lines: Vec<PreviewLine>,
    a: &PreviewDeleteArgs,
    basis: &str,
) -> Result<PreviewReport> {
    let cur = run::current_traffic(svc);
    let mut remove = BTreeSet::new();
    let mut notes = Vec::new();
    for raw in &a.names {
        match existing_tag(&cur, raw, basis) {
            Some(t) => {
                remove.insert(t);
            }
            None => notes.push(format!("{} has no preview `{raw}`", d.service_id)),
        }
    }
    for l in &mut lines {
        l.status = Some(if remove.contains(&l.tag) {
            if a.yes { "removed" } else { "would_remove" }
        } else {
            "kept"
        });
    }
    lines.retain(|l| l.status != Some("kept"));
    apply_removals(
        ctx,
        d,
        rec,
        revisions,
        svc,
        &mut lines,
        a.yes,
        a.delete_revisions,
    )
    .await?;
    Ok(PreviewReport {
        service: d.service_id.clone(),
        action: "delete",
        applied: a.yes,
        base: None,
        notes,
        previews: lines,
    })
}

#[allow(clippy::too_many_arguments)]
async fn prune(
    ctx: &Context,
    d: &Deployment,
    rec: &Reconciler<'_>,
    revisions: &Revisions,
    svc: &Service,
    mut lines: Vec<PreviewLine>,
    a: &PreviewPruneArgs,
    info: &BranchInfo,
    basis: &str,
) -> Result<PreviewReport> {
    let mut notes = Vec::new();
    if !info.history_complete {
        notes.push(
            "shallow clone: merged branches cannot all be detected (fetch the full history, e.g. GIT_DEPTH: 0); deleted branches are".into(),
        );
    }
    for l in &mut lines {
        if !l.preview || l.tag == CANARY_TAG {
            l.status = Some("kept");
            l.reason = Some(
                if l.preview {
                    "canary"
                } else {
                    "not a runway preview"
                }
                .into(),
            );
            continue;
        }
        match prune_reason(&l.tag, basis, info, a.only_merged) {
            Some(reason) => {
                l.status = Some(if a.yes { "removed" } else { "would_remove" });
                l.reason = Some(reason);
            }
            None => {
                l.status = Some("kept");
                l.reason = Some("branch still open".into());
            }
        }
    }
    apply_removals(
        ctx,
        d,
        rec,
        revisions,
        svc,
        &mut lines,
        a.yes,
        a.delete_revisions,
    )
    .await?;
    Ok(PreviewReport {
        service: d.service_id.clone(),
        action: "prune",
        applied: a.yes,
        base: Some(info.base.clone()),
        notes,
        previews: lines,
    })
}

/// Removes the tags marked `removed` in one traffic update, then optionally
/// deletes their revisions.
#[allow(clippy::too_many_arguments)]
async fn apply_removals(
    ctx: &Context,
    d: &Deployment,
    rec: &Reconciler<'_>,
    revisions: &Revisions,
    svc: &Service,
    lines: &mut [PreviewLine],
    apply: bool,
    delete_revisions: bool,
) -> Result<()> {
    let tags: Vec<String> = lines
        .iter()
        .filter(|l| l.status == Some("removed"))
        .map(|l| l.tag.clone())
        .collect();
    if !apply || tags.is_empty() {
        return Ok(());
    }
    ctx.progress.step(format!(
        "Removing {} preview(s) from {}: {}",
        tags.len(),
        d.service_id,
        tags.join(", ")
    ));
    let mut cur = run::current_traffic(svc);
    for t in &tags {
        cur.entries = traffic::remove_tag(&cur, t);
    }
    let after = update_traffic(rec, svc, &cur.entries).await?;
    if delete_revisions {
        for l in lines
            .iter_mut()
            .filter(|l| l.status == Some("removed") && l.preview)
        {
            l.revision_deleted = Some(
                delete_revision_if_unused(ctx, rec, revisions, &after, &l.revision, &l.tag).await?,
            );
        }
    }
    Ok(())
}

/// Deletes a preview revision once nothing points at it any more. Re-reads
/// the service first; never deletes the latest revisions or a revision that
/// still carries traffic or a tag; the delete is conditioned on the etag.
async fn delete_revision_if_unused(
    ctx: &Context,
    rec: &Reconciler<'_>,
    revisions: &Revisions,
    svc: &Service,
    short: &str,
    tag: &str,
) -> Result<bool> {
    let fresh = rec.get(&svc.name).await?.unwrap_or_else(|| svc.clone());
    let cur = run::current_traffic(&fresh);
    let used: BTreeSet<String> = cur
        .entries
        .iter()
        .map(|e| revision_of(&cur, &e.target))
        .chain([
            run::short_revision(&fresh.latest_ready_revision).to_string(),
            run::short_revision(&fresh.latest_created_revision).to_string(),
        ])
        .collect();
    if short.is_empty() || used.contains(short) {
        ctx.progress
            .info(format!("revision {short} is still in use; kept"));
        return Ok(false);
    }
    let name = revision_name(&fresh, short);
    let rev = match revisions.get_revision().set_name(&name).send().await {
        Ok(r) => r,
        Err(e) if is_not_found(&e) => return Ok(false),
        Err(e) => return Err(api_error(e, &format!("reading revision {short}"))),
    };
    if rev.annotations.get(PREVIEW_ANNOTATION).map(String::as_str) != Some(tag) {
        return Ok(false);
    }
    ctx.progress.info(format!("deleting revision {short}"));
    match revisions
        .delete_revision()
        .set_name(&name)
        .set_etag(&rev.etag)
        .poller()
        .until_done()
        .await
    {
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(api_error(e, &format!("deleting revision {short}"))
            .hint("the deploying principal needs run.revisions.delete")),
    }
}

/// Branches of the remote, as seen by git in the configuration's directory.
#[derive(Debug, Clone, Default)]
pub struct BranchInfo {
    pub base: String,
    /// Branches that exist on the remote.
    pub existing: BTreeSet<String>,
    /// Remote branches merged into the base branch.
    pub merged: BTreeSet<String>,
    /// False in a shallow clone (merge detection may miss branches).
    pub history_complete: bool,
}

/// Why a preview can go: every branch whose name maps to its tag was merged
/// into the base branch, or no such branch exists on the remote any more
/// (unless `only_merged`). Several branches can share a tag (`feature/login`
/// and `feature-login`): if any of them is still open, the preview stays.
/// `None`: keep it.
pub fn prune_reason(
    tag: &str,
    service_id: &str,
    info: &BranchInfo,
    only_merged: bool,
) -> Option<String> {
    let maps = |b: &&String| traffic::preview_tag(b, service_id).ok().as_deref() == Some(tag);
    let existing: Vec<&String> = info.existing.iter().filter(maps).collect();
    let merged: Vec<&String> = info
        .merged
        .iter()
        .filter(maps)
        .filter(|b| **b != info.base)
        .collect();
    // An open branch (or the base branch itself) keeps the preview.
    if existing
        .iter()
        .any(|b| **b == info.base || !merged.contains(b))
    {
        return None;
    }
    if let Some(b) = merged.first() {
        return Some(format!("branch `{b}` merged into `{}`", info.base));
    }
    (!only_merged).then(|| "branch no longer exists on the remote".to_string())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| {
            Error::new(ErrorKind::Prerequisite, format!("cannot run git: {e}"))
                .hint("`runway preview prune` needs git and a clone of the repository")
        })?;
    if !out.status.success() {
        return Err(Error::new(
            ErrorKind::Prerequisite,
            format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        )
        .permanent());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn branch_info(
    dir: &Path,
    remote: &str,
    base: Option<&str>,
    fetch: bool,
    progress: &crate::output::Progress,
) -> Result<BranchInfo> {
    git(dir, &["rev-parse", "--is-inside-work-tree"])?;
    if fetch {
        progress.info(format!("fetching {remote}"));
        git(dir, &["fetch", "--prune", "--quiet", remote])?;
    }
    let base = match base {
        Some(b) => b.to_string(),
        None => git(
            dir,
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                &format!("refs/remotes/{remote}/HEAD"),
            ],
        )
        .ok()
        .and_then(|s| {
            s.trim()
                .strip_prefix(&format!("{remote}/"))
                .map(String::from)
        })
        .or_else(|| std::env::var("CI_DEFAULT_BRANCH").ok())
        .unwrap_or_else(|| "main".into()),
    };
    let existing: BTreeSet<String> = git(dir, &["ls-remote", "--heads", remote])?
        .lines()
        .filter_map(|l| {
            l.split_once("refs/heads/")
                .map(|(_, b)| b.trim().to_string())
        })
        .collect();
    if existing.is_empty() {
        return Err(Error::new(
            ErrorKind::Prerequisite,
            format!("the remote `{remote}` reports no branches; refusing to prune"),
        )
        .permanent());
    }
    if !existing.contains(&base) {
        return Err(
            Error::config(format!("base branch `{base}` does not exist on `{remote}`"))
                .hint("pass the default branch with --base"),
        );
    }
    let merged = git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "--merged",
            &format!("{remote}/{base}"),
            &format!("refs/remotes/{remote}/"),
        ],
    )?
    .lines()
    .filter_map(|l| {
        l.trim()
            .strip_prefix(&format!("{remote}/"))
            .map(String::from)
    })
    .filter(|b| b != "HEAD")
    .collect();
    let shallow = git(dir, &["rev-parse", "--is-shallow-repository"])?.trim() == "true";
    Ok(BranchInfo {
        base,
        existing,
        merged,
        history_complete: !shallow,
    })
}

fn print_text(r: &PreviewReport) {
    let p = crate::style::out();
    for n in &r.notes {
        println!("{}", p.yellow(&format!("note: {n}")));
    }
    if r.previews.is_empty() {
        println!(
            "{}: {}",
            r.service,
            match r.action {
                "list" => "no tagged URLs",
                _ => "nothing to remove",
            }
        );
        return;
    }
    for l in &r.previews {
        let kind = if l.preview { "preview" } else { "tag" };
        let status = match l.status {
            Some("removed") => p.green("removed"),
            Some("would_remove") => p.yellow("would remove"),
            Some(s) => s.to_string(),
            None => kind.to_string(),
        };
        let reason = l
            .reason
            .as_deref()
            .map(|r| format!("  ({r})"))
            .unwrap_or_default();
        let deleted = match l.revision_deleted {
            Some(true) => "  revision deleted",
            _ => "",
        };
        println!(
            "  {:<13} {}  {}  {}{reason}{deleted}",
            status,
            p.bold(&l.tag),
            l.revision,
            p.cyan(&l.url)
        );
    }
    if r.action != "list"
        && !r.applied
        && r.previews.iter().any(|l| l.status == Some("would_remove"))
    {
        println!("Nothing changed. Re-run with --yes to remove them.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(existing: &[&str], merged: &[&str]) -> BranchInfo {
        BranchInfo {
            base: "main".into(),
            existing: existing.iter().map(|s| s.to_string()).collect(),
            merged: merged.iter().map(|s| s.to_string()).collect(),
            history_complete: true,
        }
    }

    #[test]
    fn prunes_merged_and_deleted_branches_only() {
        let i = info(
            &["main", "feature/login", "fix/export"],
            &["main", "fix/export"],
        );
        // Open branch: kept.
        assert_eq!(prune_reason("feature-login", "svc", &i, false), None);
        // Merged (still present on the remote): removed.
        assert!(
            prune_reason("fix-export", "svc", &i, false)
                .unwrap()
                .contains("merged into `main`")
        );
        // Branch deleted after merge: removed, unless only merged ones are pruned.
        assert!(
            prune_reason("old-branch", "svc", &i, false)
                .unwrap()
                .contains("no longer exists")
        );
        assert_eq!(prune_reason("old-branch", "svc", &i, true), None);
        // The base branch's own preview is never "merged".
        assert_eq!(prune_reason("main", "svc", &i, false), None);
    }

    #[test]
    fn ambiguous_tags_are_kept_while_any_matching_branch_is_open() {
        // `feature/login` and `feature-login` both map to `feature-login`.
        let i = info(
            &["main", "feature/login", "feature-login"],
            &["main", "feature/login"],
        );
        assert_eq!(
            prune_reason("feature-login", "svc", &i, false),
            None,
            "feature-login is open"
        );
        // Both merged: pruned.
        let i = info(
            &["main", "feature/login", "feature-login"],
            &["main", "feature/login", "feature-login"],
        );
        assert!(prune_reason("feature-login", "svc", &i, false).is_some());
        // One merged, the other deleted: pruned.
        let i = info(&["main", "feature/login"], &["main", "feature/login"]);
        assert!(prune_reason("feature-login", "svc", &i, false).is_some());
    }

    #[test]
    fn matches_long_branch_names_through_their_shortened_tag() {
        let long = "feature/a-very-long-branch-name-that-goes-on-and-on";
        let tag = traffic::preview_tag(long, "gcptree-prod").unwrap();
        let i = info(&["main", long], &["main", long]);
        assert!(prune_reason(&tag, "gcptree-prod", &i, false).is_some());
        let i = info(&["main", long], &["main"]);
        assert_eq!(prune_reason(&tag, "gcptree-prod", &i, false), None);
    }

    #[test]
    fn reads_branches_from_a_real_git_remote() {
        let root = tempfile::tempdir().unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let remote = root.path().join("remote.git");
        let work = root.path().join("work");
        std::fs::create_dir_all(&remote).unwrap();
        sh(&remote, &["init", "--bare", "--initial-branch=main"]);
        sh(
            root.path(),
            &["clone", "--quiet", remote.to_str().unwrap(), "work"],
        );
        sh(&work, &["checkout", "--quiet", "-b", "main"]);
        sh(&work, &["commit", "--quiet", "--allow-empty", "-m", "init"]);
        sh(&work, &["push", "--quiet", "origin", "main"]);
        for b in ["feature/login", "fix/export", "old/merged-and-deleted"] {
            sh(&work, &["checkout", "--quiet", "-b", b, "main"]);
            sh(&work, &["commit", "--quiet", "--allow-empty", "-m", b]);
            sh(&work, &["push", "--quiet", "origin", b]);
        }
        sh(&work, &["checkout", "--quiet", "main"]);
        for b in ["fix/export", "old/merged-and-deleted"] {
            sh(&work, &["merge", "--quiet", "--no-ff", "-m", "merge", b]);
        }
        sh(&work, &["push", "--quiet", "origin", "main"]);
        sh(
            &work,
            &[
                "push",
                "--quiet",
                "origin",
                "--delete",
                "old/merged-and-deleted",
            ],
        );
        sh(&work, &["remote", "set-head", "origin", "main"]);

        let i = branch_info(
            &work,
            "origin",
            None,
            true,
            &crate::output::Progress::silent(),
        )
        .unwrap();
        assert_eq!(i.base, "main");
        assert!(i.history_complete);
        assert!(
            i.existing.contains("feature/login") && !i.existing.contains("old/merged-and-deleted")
        );
        assert!(i.merged.contains("fix/export"), "{:?}", i.merged);
        assert!(!i.merged.contains("feature/login"));
        assert_eq!(prune_reason("feature-login", "svc", &i, false), None);
        assert!(
            prune_reason("fix-export", "svc", &i, false)
                .unwrap()
                .contains("merged")
        );
        assert!(prune_reason("old-merged-and-deleted", "svc", &i, false).is_some());
        // An unknown base branch is an error, not "everything is gone".
        assert!(
            branch_info(
                &work,
                "origin",
                Some("nope"),
                false,
                &crate::output::Progress::silent()
            )
            .is_err()
        );
    }
}
