//! `runway traffic`: show the split, promote a canary, set an explicit split
//! or remove preview tags. Only the service's `traffic` field is written: no
//! new revision is created.

use crate::build_client;
use crate::cli::{Context, TrafficArgs};
use crate::commands::{load, not_deployed};
use crate::config::{Deployment, Overrides};
use crate::deploy::{Applied, Reconciler, ServiceChange, check_ownership};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::{api_error, run};
use crate::output::{OutputFormat, print_json};
use crate::poll::PollConfig;
use crate::traffic::{self, Current, Entry, Target};
use google_cloud_run_v2::client::Services;
use google_cloud_run_v2::model::Service;

pub async fn run(ctx: &Context, args: TrafficArgs) -> Result<()> {
    let resolved = load(ctx, &args.stage.stage, &Overrides::default())?;
    let d = &resolved.deployment;
    let session = crate::commands::connect(ctx, d).await?;
    let client = build_client!(Services, session)?;
    let rec = Reconciler {
        run: &client,
        revisions: None,
        progress: &ctx.progress,
        poll: PollConfig::default(),
        timeout: args.timeout,
    };
    let name = d.service_name();
    let svc = rec
        .get(&name)
        .await?
        .ok_or_else(|| not_deployed(&d.service_id, &d.stage))?;
    check_ownership(&svc, &d.app, &d.stage, false)?;
    let cur = run::current_traffic(&svc);

    let wanted = if args.promote {
        Some(traffic::promote(&cur).ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                "there is no canary revision (tag `canary`)",
            )
            .hint("deploy one with `runway deploy --stage <stage> --traffic 10`")
        })?)
    } else if !args.set.is_empty() {
        let mut split = Vec::new();
        for s in &args.set {
            split.push(parse_split(&cur, s, &d.service_id)?);
        }
        Some(traffic::split(&cur, &split).map_err(|e| Error::config(format!("--set: {e}")))?)
    } else if !args.remove_tag.is_empty() {
        let mut entries = cur.entries.clone();
        for raw in &args.remove_tag {
            let tag = existing_tag(&cur, raw, &d.service_id).ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("{} has no tag `{raw}`", d.service_id),
                )
            })?;
            entries = traffic::remove_tag(
                &Current {
                    entries,
                    latest_ready: cur.latest_ready.clone(),
                },
                &tag,
            );
        }
        Some(entries)
    } else {
        None
    };

    let svc = match wanted {
        Some(entries) if traffic::flat(&entries) != traffic::flat(&cur.entries) => {
            ctx.progress
                .step(format!("Updating traffic of {}", d.service_id));
            update_traffic(&rec, &svc, &entries).await?
        }
        Some(_) => {
            ctx.progress.info("traffic is already as requested");
            svc
        }
        None => svc,
    };
    let lines = run::status(&svc, &d.app, &d.stage).traffic;
    match ctx.output {
        OutputFormat::Json => print_json(&lines),
        OutputFormat::Text => crate::commands::info::print_traffic(&lines),
    }
    Ok(())
}

/// The tag a user named: exact, or the tag `--preview NAME` would produce.
pub fn existing_tag(cur: &Current, raw: &str, service_id: &str) -> Option<String> {
    if cur.has_tag(raw) {
        return Some(raw.to_string());
    }
    traffic::preview_tag(raw, service_id)
        .ok()
        .filter(|t| cur.has_tag(t))
}

fn parse_split(cur: &Current, s: &str, service_id: &str) -> Result<(Target, u32)> {
    let (target, pct) = s
        .rsplit_once('=')
        .ok_or_else(|| Error::config(format!("--set `{s}`: expected TARGET=PERCENT")))?;
    let pct: u32 = pct
        .trim_end_matches('%')
        .parse()
        .ok()
        .filter(|p| *p <= 100)
        .ok_or_else(|| Error::config(format!("--set `{s}`: percentage must be 0-100")))?;
    let target = cur
        .resolve(target, service_id)
        .ok_or_else(|| Error::config(format!("--set `{s}`: unknown target")))?;
    Ok((target, pct))
}

/// Writes only the service's traffic and waits until Cloud Run applied it.
pub async fn update_traffic(
    rec: &Reconciler<'_>,
    svc: &Service,
    entries: &[Entry],
) -> Result<Service> {
    let op = rec
        .run
        .update_service()
        .set_service(run::with_traffic(svc, entries))
        .set_update_mask(google_cloud_wkt::FieldMask::default().set_paths(["traffic"]))
        .send()
        .await
        .map_err(|e| {
            api_error(e, "updating the traffic split")
                .hint("the deploying principal needs run.services.update")
        })?;
    rec.wait_ready(
        &svc.name,
        &Applied {
            change: ServiceChange::Updated,
            operation: Some(op.name),
            target_generation: svc.generation + 1,
        },
    )
    .await
}

/// Result of `undeploy --preview NAME` (text or JSON).
#[derive(Debug, serde::Serialize)]
pub struct PreviewRemoval {
    pub service: String,
    /// The name given on the command line.
    pub preview: String,
    /// The tag it refers to (`null` when the service has no such preview).
    pub tag: Option<String>,
    /// `not_found`, `would_remove` (without `--yes`) or `removed`.
    pub action: &'static str,
}

/// What `undeploy --preview` does, decided from the live traffic.
pub fn preview_removal(cur: &Current, service_id: &str, raw: &str, apply: bool) -> PreviewRemoval {
    let tag = existing_tag(cur, raw, service_id);
    let action = match (&tag, apply) {
        (None, _) => "not_found",
        (Some(_), false) => "would_remove",
        (Some(_), true) => "removed",
    };
    PreviewRemoval {
        service: service_id.to_string(),
        preview: raw.to_string(),
        tag,
        action,
    }
}

/// `undeploy --preview NAME`: removes the preview's tag (its URL). The
/// revision itself is left to Cloud Run's revision garbage collection.
pub async fn remove_preview(
    ctx: &Context,
    d: &Deployment,
    rec: &Reconciler<'_>,
    raw: &str,
    apply: bool,
) -> Result<()> {
    let svc = rec
        .get(&d.service_name())
        .await?
        .ok_or_else(|| not_deployed(&d.service_id, &d.stage))?;
    check_ownership(&svc, &d.app, &d.stage, false)?;
    let cur = run::current_traffic(&svc);
    let report = preview_removal(&cur, &d.service_id, raw, apply);
    if report.action == "removed"
        && let Some(tag) = &report.tag
    {
        ctx.progress
            .step(format!("Removing preview `{tag}` from {}", d.service_id));
        // On failure the error is the single result (printed by main, as
        // JSON with `-o json`).
        update_traffic(rec, &svc, &traffic::remove_tag(&cur, tag)).await?;
    }
    match ctx.output {
        OutputFormat::Json => print_json(&report),
        OutputFormat::Text => match (report.action, &report.tag) {
            ("removed", Some(tag)) => ctx.progress.success(format!("preview `{tag}` removed")),
            ("would_remove", Some(tag)) => println!(
                "Would remove preview `{tag}` (its URL) from {}; nothing is deleted. Re-run with --yes.",
                d.service_id
            ),
            _ => println!("{} has no preview `{raw}`; nothing to do", d.service_id),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_removal_reports_every_outcome() {
        let cur = Current {
            entries: vec![
                Entry::new(Target::Latest, 100, ""),
                Entry::new(Target::Revision("s-2".into()), 0, "feature-login"),
            ],
            latest_ready: "s-3".into(),
        };
        let r = preview_removal(&cur, "s", "feature/login", false);
        assert_eq!(
            (r.tag.as_deref(), r.action),
            (Some("feature-login"), "would_remove")
        );
        let r = preview_removal(&cur, "s", "feature-login", true);
        assert_eq!(r.action, "removed");
        let r = preview_removal(&cur, "s", "nope", true);
        assert_eq!((r.tag, r.action), (None, "not_found"));
        let json = serde_json::to_value(preview_removal(&cur, "s", "feature/login", true)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"service": "s", "preview": "feature/login", "tag": "feature-login", "action": "removed"})
        );
    }
}
