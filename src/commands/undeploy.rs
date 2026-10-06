//! `runway undeploy`: remove what `deploy` created for a stage, keep data.
//!
//! Stateless: what may be deleted is decided from live ownership markers.
//! - The Cloud Run service is deleted only if its runway labels match the app
//!   and stage (its revisions, tag bindings and IAP/invoker policy go with it).
//! - The runtime service account is deleted only if runway created it for
//!   this app and stage (description marker) and no other service in the
//!   region runs as it; its grants are revoked first.
//! - Never deleted: buckets (data), enabled APIs, build infrastructure shared
//!   by stages (repository, source bucket, build service account), accounts
//!   and grants that runway did not create, tag keys/values. Images are only
//!   deleted with `--delete-images`.

use crate::build_client;
use crate::cli::{Context, UndeployArgs};
use crate::commands::{connect, load};
use crate::config::{Artifact, Deployment, Overrides};
use crate::deploy::check_ownership;
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::{api_error, is_not_found};
use crate::output::{OutputFormat, Progress, print_json};
use crate::provision::{Provisioner, StepOutcome, has_sa_marker};
use crate::retry::with_retry;
use google_cloud_gax::error::rpc::Code;
use google_cloud_lro::Poller;
use google_cloud_run_v2::client::{Jobs, Services};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Delete,
    Revoke,
    Keep,
    /// Already gone.
    Absent,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub action: Action,
    pub resource: String,
    pub reason: String,
    /// Filled in after execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

fn item(action: Action, resource: impl Into<String>, reason: impl Into<String>) -> Item {
    Item {
        action,
        resource: resource.into(),
        reason: reason.into(),
        outcome: None,
    }
}

#[derive(Serialize)]
struct Report {
    service: String,
    executed: bool,
    items: Vec<Item>,
}

/// Other services and jobs in the region that run as `email` (those being
/// removed, `removing`, aside).
/// Live jobs count even when runway.yaml lists none (a job removed from it
/// may still run as the account). Jobs that cannot be listed keep it too.
async fn services_using(
    run: &Services,
    jobs: &Jobs,
    d: &Deployment,
    email: &str,
    removing: &[String],
) -> Result<Vec<String>> {
    let mut users = Vec::new();
    let mut token = String::new();
    loop {
        let resp = match jobs
            .list_jobs()
            .set_parent(d.parent())
            .set_page_token(token.clone())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if crate::gcp::status_code(&e) == Some(Code::PermissionDenied) => {
                users.push("jobs runway cannot list (run.jobs.list denied)".into());
                break;
            }
            Err(e) => return Err(api_error(e, "listing Cloud Run jobs")),
        };
        for j in resp.jobs {
            let sa = j
                .template
                .as_ref()
                .and_then(|t| t.template.as_ref())
                .map(|t| t.service_account.as_str())
                .unwrap_or("");
            if !removing.contains(&j.name) && sa.eq_ignore_ascii_case(email) {
                users.push(format!(
                    "job {}",
                    j.name.rsplit('/').next().unwrap_or(&j.name)
                ));
            }
        }
        if resp.next_page_token.is_empty() {
            break;
        }
        token = resp.next_page_token;
    }
    let mut token = String::new();
    loop {
        let resp = run
            .list_services()
            .set_parent(d.parent())
            .set_page_token(token.clone())
            .send()
            .await
            .map_err(|e| api_error(e, "listing Cloud Run services"))?;
        for s in resp.services {
            let sa = s
                .template
                .as_ref()
                .map(|t| t.service_account.as_str())
                .unwrap_or("");
            if !removing.contains(&s.name) && sa.eq_ignore_ascii_case(email) {
                users.push(s.name.rsplit('/').next().unwrap_or(&s.name).to_string());
            }
        }
        if resp.next_page_token.is_empty() {
            return Ok(users);
        }
        token = resp.next_page_token;
    }
}

/// Builds the teardown plan from live state (read-only). Returns the items
/// and whether the service and the runtime account are to be deleted.
pub async fn plan(
    d: &Deployment,
    run: &Services,
    jobs: &Jobs,
    prov: &Provisioner<'_>,
    delete_images: bool,
    removing: &[String],
) -> Result<(Vec<Item>, bool, bool)> {
    let s = &d.service;
    let mut items = Vec::new();
    let name = d.service_name();

    // Job.
    let delete_job = if d.is_job() {
        let res = format!("Cloud Run job {}", d.service_id);
        match jobs.get_job().set_name(&name).send().await {
            Ok(job) => {
                crate::gcp::jobs::check_job_ownership(&job, &d.app, &d.stage)?;
                items.push(item(
                    Action::Delete,
                    res,
                    "created by runway (labels match); its executions go with it",
                ));
                true
            }
            Err(e) if is_not_found(&e) => {
                items.push(item(Action::Absent, res, "does not exist"));
                false
            }
            Err(e) => return Err(api_error(e, &format!("reading Cloud Run job {name}"))),
        }
    } else {
        false
    };

    // Service.
    let svc = match d.is_job() {
        true => Ok(None),
        false => run.get_service().set_name(&name).send().await.map(Some),
    };
    let svc = match svc {
        Ok(svc) => svc,
        Err(e) if is_not_found(&e) => None,
        Err(e) => return Err(api_error(e, &format!("reading Cloud Run service {name}"))),
    };
    let delete_service = delete_job
        || match &svc {
            None if d.is_job() => false,
            Some(svc) => {
                check_ownership(svc, &d.app, &d.stage, false)?;
                items.push(item(
                Action::Delete,
                format!("Cloud Run service {}", d.service_id),
                "created by runway (labels match); revisions, tag bindings and IAP/invoker policy go with it",
            ));
                true
            }
            None => {
                items.push(item(
                    Action::Absent,
                    format!("Cloud Run service {}", d.service_id),
                    "does not exist",
                ));
                false
            }
        };

    // Runtime service account and its grants.
    let email = &s.service_account;
    let mut delete_sa = false;
    let mut sa_absent = false;
    if s.identity.create {
        match prov.service_account_description(email).await? {
            None => {
                sa_absent = true;
                items.push(item(Action::Absent, format!("service account {email}"), "does not exist"));
            }
            Some(desc) if has_sa_marker(&desc, &d.app, Some(&d.stage), "runtime") => {
                let users = services_using(run, jobs, d, email, removing).await?;
                if users.is_empty() {
                    delete_sa = true;
                    items.push(item(
                        Action::Delete,
                        format!("service account {email}"),
                        "created by runway for this app and stage, not used by another service or job",
                    ));
                } else {
                    items.push(item(
                        Action::Keep,
                        format!("service account {email}"),
                        format!("still used by {}", users.join(", ")),
                    ));
                }
            }
            Some(_) => items.push(item(
                Action::Keep,
                format!("service account {email}"),
                "not created by runway for this app and stage (no ownership marker): it existed before",
            )),
        }
    } else {
        items.push(item(
            Action::Keep,
            format!("service account {email}"),
            "provided (identity.create is false)",
        ));
    }
    for r in &s.identity.roles {
        let res = format!("grant {} on {} to {}", r.role, r.target, email);
        if delete_sa {
            items.push(item(Action::Revoke, res, "the service account is deleted"));
        } else if sa_absent {
            items.push(item(
                Action::Absent,
                res,
                "the service account does not exist",
            ));
        } else {
            items.push(item(
                Action::Keep,
                res,
                "the service account is kept; runway cannot tell whether the grant predates it",
            ));
        }
    }

    // Data, APIs, shared infrastructure: always kept.
    for b in d.buckets.values() {
        items.push(item(
            Action::Keep,
            format!("bucket gs://{}", b.name),
            "holds data; undeploy never deletes buckets",
        ));
    }
    for sec in d.secrets.values() {
        items.push(item(
            Action::Keep,
            format!("secret {}", sec.name),
            "holds a value people added; undeploy never deletes secrets",
        ));
    }
    for (name, v) in &s.volumes {
        items.push(item(
            Action::Keep,
            format!("bucket gs://{} (volume {name})", v.bucket),
            "holds data",
        ));
    }
    if let Artifact::Build(b) = &d.artifact {
        items.push(item(
            Action::Keep,
            format!(
                "Artifact Registry repository {}/{}",
                b.artifact_location, b.artifact_repository
            ),
            "shared build infrastructure",
        ));
        items.push(item(
            Action::Keep,
            format!("bucket gs://{}", b.source_bucket),
            "build sources (shared; archives expire through the lifecycle rule when runway created it)",
        ));
        items.push(item(
            Action::Keep,
            format!("service account {}", b.build_service_account),
            "shared build identity (used by every stage)",
        ));
        items.push(if delete_images {
            item(
                Action::Delete,
                format!(
                    "images {}-docker.pkg.dev/{}/{}/{}",
                    b.artifact_location,
                    d.project,
                    b.artifact_repository,
                    d.image_package()
                ),
                "--delete-images",
            )
        } else {
            item(
                Action::Keep,
                format!(
                    "images {}-docker.pkg.dev/{}/{}/{}",
                    b.artifact_location,
                    d.project,
                    b.artifact_repository,
                    d.image_package()
                ),
                "kept for a fast redeploy (use --delete-images to remove them)",
            )
        });
    }
    for (k, v) in &s.tags {
        items.push(item(
            Action::Keep,
            format!("tag value {k}/{v}"),
            "organization resource; only its binding to the service goes away",
        ));
    }
    if s.iap.enabled {
        items.push(item(
            Action::Keep,
            "IAP service agent",
            "Google-managed project identity",
        ));
    }
    items.push(item(
        Action::Keep,
        format!("APIs on {}", d.project),
        "never disabled by undeploy",
    ));
    Ok((items, delete_service, delete_sa))
}

fn print_items(items: &[Item]) {
    let p = crate::style::out();
    for i in items {
        let mark = match i.action {
            Action::Delete => p.red("- delete"),
            Action::Revoke => p.red("- revoke"),
            Action::Keep => p.green("= keep  "),
            Action::Absent => p.dim("  absent"),
        };
        let outcome = i
            .outcome
            .as_deref()
            .map(|o| format!(" [{o}]"))
            .unwrap_or_default();
        println!(
            "  {mark} {} {}{outcome}",
            i.resource,
            p.dim(&format!("({})", i.reason))
        );
    }
}

pub async fn run(ctx: &Context, args: UndeployArgs) -> Result<()> {
    let resolved = load(ctx, &args.stage.stage, &Overrides::default())?;
    let first = resolved.first();
    let p: &Progress = &ctx.progress;
    let mut retry = first.retry;
    if let Some(n) = args.retries {
        retry.attempts = n.saturating_add(1);
    }
    let selected = resolved.select(&args.stage.only)?;
    let session = connect(ctx, first).await?;
    let run_client = build_client!(Services, session)?;
    let jobs_client = build_client!(Jobs, session)?;
    if let Some(name) = &args.preview {
        return remove_previews(
            ctx,
            &resolved,
            &selected,
            &run_client,
            &jobs_client,
            name,
            &args,
        )
        .await;
    }
    let prov = Provisioner::for_teardown(first, &session, &run_client)
        .await?
        .with_stack(&resolved);

    // The workloads to remove: those selected, or those runway.yaml dropped.
    let orphans;
    let targets: Vec<&Deployment> = if args.orphans {
        orphans = orphan_workloads(&resolved, &run_client, &jobs_client).await?;
        orphans.iter().collect()
    } else {
        selected.clone()
    };
    let removing: Vec<String> = targets.iter().map(|d| d.service_name()).collect();
    let mut items: Vec<Item> = Vec::new();
    let mut plans = Vec::new();
    for d in &targets {
        let (its, delete_service, delete_sa) =
            with_retry(&retry, p, "inspect current state", |_| {
                plan(
                    d,
                    &run_client,
                    &jobs_client,
                    &prov,
                    args.delete_images,
                    &removing,
                )
            })
            .await?;
        for i in its {
            if !items.iter().any(|x| x.resource == i.resource) {
                items.push(i);
            }
        }
        plans.push((*d, delete_service, delete_sa));
    }
    settle_accounts(&mut plans, &mut items);
    // Schedules: all of them with the whole stage, else those calling what is
    // removed. The scheduler account goes with the whole stage.
    let whole = args.stage.only.is_empty() && !args.orphans;
    // A stage without schedules makes no Cloud Scheduler call (as before
    // schedules existed), unless --orphans looks for those left behind.
    let (schedules, delete_invoker) = match resolved.scheduler.is_some() || args.orphans {
        false => (Vec::new(), None),
        true => {
            plan_schedules(
                &resolved,
                &prov,
                &targets,
                whole || args.orphans,
                args.orphans,
                &mut items,
            )
            .await?
        }
    };
    let label = targets
        .iter()
        .map(|d| d.service_id.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    if !args.yes {
        match ctx.output {
            OutputFormat::Json => print_json(&Report {
                service: label.clone(),
                executed: false,
                items,
            }),
            OutputFormat::Text => {
                println!(
                    "Undeploy plan for {} (stage {}) in {}/{}:",
                    if label.is_empty() { "nothing" } else { &label },
                    first.stage,
                    first.project,
                    first.region
                );
                print_items(&items);
                println!();
                println!("Nothing was deleted. Re-run with --yes to apply.");
            }
        }
        return Ok(());
    }

    // Triggers first, so nothing calls a target being deleted.
    let scheduler = prov.scheduler_client()?;
    for name in &schedules {
        let id = name.rsplit('/').next().unwrap_or(name).to_string();
        p.step(format!("Deleting schedule {id}"));
        let o = with_retry(&retry, p, "delete schedule", |_| async {
            let (app, stage) = (&first.app, &first.stage);
            Ok(
                match crate::gcp::scheduler::delete_owned(&scheduler, name, app, stage).await? {
                    true => StepOutcome::Changed,
                    false => StepOutcome::Unchanged,
                },
            )
        })
        .await?;
        mark(&mut items, &format!("schedule {id}"), o);
    }
    execute_all(
        &plans,
        &run_client,
        &jobs_client,
        &prov,
        &retry,
        p,
        &mut items,
        args.delete_images,
        args.timeout,
    )
    .await?;
    if let Some(email) = delete_invoker {
        p.step(format!("Deleting service account {email}"));
        let o = with_retry(&retry, p, "delete service account", |_| {
            prov.delete_service_account(&email)
        })
        .await?;
        mark(&mut items, &format!("service account {email}"), o);
    }

    match ctx.output {
        OutputFormat::Json => print_json(&Report {
            service: label.clone(),
            executed: true,
            items,
        }),
        OutputFormat::Text => {
            p.success(format!("{label} undeployed"));
            println!("Removed and kept resources:");
            print_items(&items);
        }
    }
    Ok(())
}

/// `--preview NAME`: the preview's URL on every selected service, and the
/// preview copy of every selected job.
async fn remove_previews(
    ctx: &Context,
    resolved: &crate::config::Resolved,
    selected: &[&Deployment],
    run_client: &Services,
    jobs: &Jobs,
    name: &str,
    args: &UndeployArgs,
) -> Result<()> {
    let rec = crate::deploy::Reconciler {
        run: run_client,
        revisions: None,
        progress: &ctx.progress,
        poll: crate::poll::PollConfig::default(),
        timeout: args.timeout,
    };
    let services: Vec<&&Deployment> = selected.iter().filter(|d| !d.is_job()).collect();
    let single = services.len() == 1 && selected.len() == 1;
    let mut reports = Vec::new();
    for d in services {
        reports.push(
            crate::commands::traffic::remove_preview(ctx, d, &rec, name, args.yes, single).await?,
        );
    }
    let tag = crate::traffic::preview_tag(name, &crate::commands::preview_basis(resolved))
        .map_err(|e| Error::config(format!("--preview: {e}")))?;
    for d in selected.iter().filter(|d| d.is_job()) {
        let id = format!("{}-{tag}", d.service_id);
        let full = format!("{}/jobs/{id}", d.parent());
        let found = match jobs.get_job().set_name(&full).send().await {
            Ok(j) => {
                crate::gcp::jobs::check_job_ownership(&j, &d.app, &d.stage)?;
                j.labels.get(crate::naming::LABEL_PREVIEW) == Some(&tag)
            }
            Err(e) if is_not_found(&e) => false,
            Err(e) => return Err(api_error(e, &format!("reading job {id}"))),
        };
        let action = match (found, args.yes) {
            (false, _) => "not_found",
            (true, false) => "would_remove",
            (true, true) => {
                ctx.progress.step(format!("Deleting preview job {id}"));
                match jobs.delete_job().set_name(&full).send().await {
                    Ok(_) => {}
                    Err(e) if is_not_found(&e) => {}
                    Err(e) => return Err(api_error(e, &format!("deleting job {id}"))),
                }
                "removed"
            }
        };
        if ctx.output == OutputFormat::Text {
            println!("preview job {id}: {}", action.replace('_', " "));
        }
        reports.push(crate::commands::traffic::PreviewRemoval {
            service: id,
            preview: name.to_string(),
            tag: found.then(|| tag.clone()),
            action,
        });
    }
    if ctx.output == OutputFormat::Json && !single {
        print_json(&reports);
    }
    Ok(())
}

/// Services and jobs runway deployed for this stage under a name
/// runway.yaml no longer lists, as deployments to tear down (their runtime
/// identity is the stage default: `identity` is left as provided).
pub(crate) async fn orphan_workloads(
    resolved: &crate::config::Resolved,
    run: &Services,
    jobs: &Jobs,
) -> Result<Vec<Deployment>> {
    let d = resolved.first();
    let known = |key: &str, job: bool| {
        resolved
            .deployments
            .iter()
            .any(|w| w.key.as_deref() == Some(key) && w.is_job() == job)
    };
    let ours = |labels: &std::collections::HashMap<String, String>| {
        crate::gcp::run::ownership_of(labels, &d.app, &d.stage) == crate::gcp::run::Ownership::Owned
            && !labels.contains_key(crate::naming::LABEL_PREVIEW)
    };
    let as_orphan = |key: &str, id: &str, job: bool| {
        let mut o = d.clone();
        o.key = Some(key.to_string());
        o.service_id = id.to_string();
        o.kind = match job {
            true => crate::config::WorkloadKind::Job(crate::config::JobSettings {
                tasks: 1,
                parallelism: 0,
                max_retries: 0,
            }),
            false => crate::config::WorkloadKind::Service,
        };
        o.service.identity.create = false;
        o.service.identity.roles.clear();
        o.service.volumes.clear();
        o.service.tags.clear();
        o.service.iap = Default::default();
        o
    };
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let resp = run
            .list_services()
            .set_parent(d.parent())
            .set_page_token(token.clone())
            .send()
            .await
            .map_err(|e| api_error(e, "listing Cloud Run services"))?;
        for s in resp.services {
            if let Some(key) = s.labels.get(crate::naming::LABEL_NAME)
                && ours(&s.labels)
                && !known(key, false)
            {
                out.push(as_orphan(key, run_short(&s.name), false));
            }
        }
        if resp.next_page_token.is_empty() {
            break;
        }
        token = resp.next_page_token;
    }
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
            if let Some(key) = j.labels.get(crate::naming::LABEL_NAME)
                && ours(&j.labels)
                && !known(key, true)
            {
                out.push(as_orphan(key, run_short(&j.name), true));
            }
        }
        if resp.next_page_token.is_empty() {
            break;
        }
        token = resp.next_page_token;
    }
    Ok(out)
}

fn run_short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Scheduler jobs to delete (full names) and the scheduler account, if it
/// goes too.
pub async fn plan_schedules(
    resolved: &crate::config::Resolved,
    prov: &Provisioner<'_>,
    targets: &[&Deployment],
    whole: bool,
    orphans_only: bool,
    items: &mut Vec<Item>,
) -> Result<(Vec<String>, Option<String>)> {
    let d = resolved.first();
    let mut names = Vec::new();
    if whole {
        let region = d.scheduler_region.clone();
        let parent = crate::gcp::scheduler::parent(&d.project, &region);
        let configured: Vec<String> = resolved
            .schedules
            .iter()
            .map(|s| crate::gcp::scheduler::job_name(&d.project, &region, &s.id))
            .collect();
        for j in
            crate::gcp::scheduler::list_owned(&prov.scheduler_client()?, &parent, &d.app, &d.stage)
                .await?
        {
            // --orphans: only those runway.yaml no longer lists.
            if !orphans_only || !configured.contains(&j.name) {
                names.push(j.name);
            }
        }
    } else {
        // Named in runway.yaml, but only deleted if runway created it.
        let client = prov.scheduler_client()?;
        for s in &resolved.schedules {
            if !targets
                .iter()
                .any(|t| t.service_id == s.target.resource_id())
            {
                continue;
            }
            let name = prov.schedule_name(s)?;
            match crate::gcp::scheduler::ownership(&client, &name, &d.app, &d.stage).await? {
                Some(true) => names.push(name),
                Some(false) => items.push(item(
                    Action::Keep,
                    format!("schedule {}", s.id),
                    "not created by runway for this app and stage (no description marker)",
                )),
                None => items.push(item(
                    Action::Absent,
                    format!("schedule {}", s.id),
                    "does not exist",
                )),
            }
        }
    }
    for n in &names {
        items.push(item(
            Action::Delete,
            format!("schedule {}", run_short(n)),
            "created by runway (description marker); it calls what is removed",
        ));
    }
    let mut invoker = None;
    if whole
        && !orphans_only
        && let Some(sc) = resolved.scheduler.as_ref().filter(|sc| sc.create)
    {
        let email = &sc.service_account;
        match prov.service_account_description(email).await? {
            Some(desc) if has_sa_marker(&desc, &d.app, Some(&d.stage), "scheduler") => {
                items.push(item(
                    Action::Delete,
                    format!("service account {email}"),
                    "the scheduler's account, created by runway for this app and stage",
                ));
                invoker = Some(email.clone());
            }
            Some(_) => items.push(item(
                Action::Keep,
                format!("service account {email}"),
                "not created by runway for this app and stage",
            )),
            None => {}
        }
    }
    Ok((names, invoker))
}

/// Whether an account is deleted is decided per account, not per workload:
/// when one removed workload's plan deletes it (runway created it for this
/// stage and nothing else runs as it), every removed workload using it
/// deletes it, whatever its own `identity.create`. Their plan entries then
/// say so: the account is deleted and every role it was given is revoked.
pub fn settle_accounts(plans: &mut [(&Deployment, bool, bool)], items: &mut [Item]) {
    let deleted: Vec<String> = plans
        .iter()
        .filter(|(_, _, delete_sa)| *delete_sa)
        .map(|(d, _, _)| d.service.service_account.clone())
        .collect();
    for (d, _, delete_sa) in plans.iter_mut() {
        if deleted.contains(&d.service.service_account) {
            *delete_sa = true;
        }
    }
    for email in &deleted {
        let account = format!("service account {email}");
        let grant_suffix = format!(" to {email}");
        for i in items.iter_mut() {
            if i.resource == account && i.action != Action::Delete {
                i.action = Action::Delete;
                i.reason =
                    "created by runway for this app and stage, not used by another service or job"
                        .into();
            } else if i.resource.starts_with("grant ") && i.resource.ends_with(&grant_suffix) {
                i.action = Action::Revoke;
                i.reason = "the service account is deleted".into();
            }
        }
    }
}

/// Tears down several workloads: every service and job first, then the
/// runtime accounts (each once), then images. An account shared by two
/// services is only deleted once neither runs any more, so a failure part
/// way never leaves a running service without its identity.
#[allow(clippy::too_many_arguments)]
pub async fn execute_all(
    plans: &[(&Deployment, bool, bool)],
    run: &Services,
    jobs: &Jobs,
    prov: &Provisioner<'_>,
    retry: &crate::retry::RetryConfig,
    p: &Progress,
    items: &mut [Item],
    delete_images: bool,
    timeout: std::time::Duration,
) -> Result<()> {
    let phase = |delete_service, delete_sa, delete_images| Teardown {
        delete_service,
        delete_sa,
        delete_images,
        timeout,
    };
    for (d, delete_service, _) in plans {
        execute(
            d,
            run,
            jobs,
            prov,
            retry,
            p,
            items,
            phase(*delete_service, false, false),
        )
        .await?;
    }
    // Each account once, with the roles every removed workload gave it.
    let mut done: Vec<&str> = Vec::new();
    for (d, _, delete_sa) in plans {
        let email = d.service.service_account.as_str();
        if *delete_sa && !done.contains(&email) {
            done.push(email);
            let mut merged = (*d).clone();
            for (other, _, _) in plans {
                if other.service.service_account != email {
                    continue;
                }
                for role in &other.service.identity.roles {
                    if !merged.service.identity.roles.contains(role) {
                        merged.service.identity.roles.push(role.clone());
                    }
                }
            }
            execute(
                &merged,
                run,
                jobs,
                prov,
                retry,
                p,
                items,
                phase(false, true, false),
            )
            .await?;
        }
    }
    if delete_images {
        for (d, _, _) in plans {
            execute(
                d,
                run,
                jobs,
                prov,
                retry,
                p,
                items,
                phase(false, false, true),
            )
            .await?;
        }
    }
    Ok(())
}

/// What a confirmed teardown deletes (from [`plan`]).
pub struct Teardown {
    pub delete_service: bool,
    pub delete_sa: bool,
    pub delete_images: bool,
    pub timeout: std::time::Duration,
}

/// Applies a teardown plan in reverse dependency order: service, grants,
/// runtime service account, images. Each step is idempotent and retried.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    d: &Deployment,
    run: &Services,
    jobs: &Jobs,
    prov: &Provisioner<'_>,
    retry: &crate::retry::RetryConfig,
    progress: &Progress,
    items: &mut [Item],
    what: Teardown,
) -> Result<()> {
    let retry = *retry;
    // 1. Service (and everything attached to it), or job.
    let name = d.service_name();
    let p = progress;
    if what.delete_service && d.is_job() {
        p.step(format!("Deleting Cloud Run job {}", d.service_id));
        let outcome = with_retry(&retry, p, "delete job", |_| async {
            let current = match jobs.get_job().set_name(&name).send().await {
                Ok(j) => j,
                Err(e) if is_not_found(&e) => return Ok(StepOutcome::Unchanged),
                Err(e) => return Err(api_error(e, &format!("reading {}", d.service_id))),
            };
            crate::gcp::jobs::check_job_ownership(&current, &d.app, &d.stage)
                .map_err(Error::permanent)?;
            let op = jobs
                .delete_job()
                .set_name(&name)
                .set_etag(&current.etag)
                .poller()
                .until_done();
            match tokio::time::timeout(what.timeout, op).await {
                Err(_) => Err(Error::new(ErrorKind::Timeout, "timed out deleting the job")),
                Ok(Ok(_)) => Ok(StepOutcome::Changed),
                Ok(Err(e)) if is_not_found(&e) => Ok(StepOutcome::Unchanged),
                Ok(Err(e)) => Err(api_error(e, &format!("deleting {}", d.service_id))
                    .hint("the deployer needs run.jobs.delete (roles/run.developer)")),
            }
        })
        .await?;
        mark(items, &format!("Cloud Run job {}", d.service_id), outcome);
    } else if what.delete_service {
        p.step(format!("Deleting Cloud Run service {}", d.service_id));
        let outcome = with_retry(&retry, p, "delete service", |_| async {
            // Every attempt re-reads the service: after an ambiguous failure
            // another process may have recreated it under the same name.
            let current = match run.get_service().set_name(&name).send().await {
                Ok(s) => s,
                Err(e) if is_not_found(&e) => return Ok(StepOutcome::Unchanged),
                Err(e) => return Err(api_error(e, &format!("reading {}", d.service_id))),
            };
            if crate::gcp::run::ownership(&current, &d.app, &d.stage)
                != crate::gcp::run::Ownership::Owned
            {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "{} is no longer owned by runway for app `{}` stage `{}`; not deleting it",
                        d.service_id, d.app, d.stage
                    ),
                )
                .permanent());
            }
            // The etag makes Cloud Run refuse if the service changed since this read.
            let op = run
                .delete_service()
                .set_name(&name)
                .set_etag(&current.etag)
                .poller()
                .until_done();
            match tokio::time::timeout(what.timeout, op).await {
                Err(_) => Err(Error::new(
                    ErrorKind::Timeout,
                    "timed out deleting the service",
                )),
                Ok(Ok(_)) => Ok(StepOutcome::Changed),
                Ok(Err(e)) if is_not_found(&e) => Ok(StepOutcome::Unchanged),
                Ok(Err(e)) => Err(api_error(e, &format!("deleting {}", d.service_id))
                    .hint("the deployer needs run.services.delete (roles/run.developer)")),
            }
        })
        .await?;
        mark(
            items,
            &format!("Cloud Run service {}", d.service_id),
            outcome,
        );
    }

    // 2. Grants, then the runtime service account.
    if what.delete_sa {
        let email = &d.service.service_account;
        for r in &d.service.identity.roles {
            let res = format!("grant {} on {} to {}", r.role, r.target, email);
            p.info(format!("revoking {} on {}", r.role, r.target));
            let o = with_retry(&retry, p, &res, |_| prov.revoke_grant(email, r)).await?;
            mark(items, &res, o);
        }
        p.step(format!("Deleting service account {email}"));
        let o = with_retry(&retry, p, "delete service account", |_| {
            prov.delete_service_account(email)
        })
        .await?;
        mark(items, &format!("service account {email}"), o);
    }

    // 3. Images (opt-in).
    if what.delete_images
        && let Artifact::Build(b) = &d.artifact
    {
        let package = d.image_package();
        p.step("Deleting images");
        let o = with_retry(&retry, p, "delete images", |_| {
            prov.delete_images(&b.artifact_location, &b.artifact_repository, &package)
        })
        .await?;
        let res = format!(
            "images {}-docker.pkg.dev/{}/{}/{}",
            b.artifact_location,
            d.project,
            b.artifact_repository,
            d.image_package()
        );
        mark(items, &res, o);
    }

    Ok(())
}

fn mark(items: &mut [Item], resource: &str, o: StepOutcome) {
    if let Some(i) = items.iter_mut().find(|i| i.resource == resource) {
        i.outcome = Some(
            match o {
                StepOutcome::Changed => "done",
                StepOutcome::Unchanged => "already gone",
            }
            .into(),
        );
    }
}
