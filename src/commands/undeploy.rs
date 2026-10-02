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
use google_cloud_lro::Poller;
use google_cloud_run_v2::client::Services;
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

/// Other services in the region that run as `email`.
async fn services_using(run: &Services, d: &Deployment, email: &str) -> Result<Vec<String>> {
    let mut users = Vec::new();
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
            if s.name != d.service_name() && sa.eq_ignore_ascii_case(email) {
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
    prov: &Provisioner<'_>,
    delete_images: bool,
) -> Result<(Vec<Item>, bool, bool)> {
    let s = &d.service;
    let mut items = Vec::new();
    let name = d.service_name();

    // Service.
    let svc = match run.get_service().set_name(&name).send().await {
        Ok(svc) => Some(svc),
        Err(e) if is_not_found(&e) => None,
        Err(e) => return Err(api_error(e, &format!("reading Cloud Run service {name}"))),
    };
    let delete_service = match &svc {
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
                let users = services_using(run, d, email).await?;
                if users.is_empty() {
                    delete_sa = true;
                    items.push(item(
                        Action::Delete,
                        format!("service account {email}"),
                        "created by runway for this app and stage, not used by another service",
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
                    b.artifact_location, d.project, b.artifact_repository, d.app
                ),
                "--delete-images",
            )
        } else {
            item(
                Action::Keep,
                format!(
                    "images {}-docker.pkg.dev/{}/{}/{}",
                    b.artifact_location, d.project, b.artifact_repository, d.app
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
    let d = &resolved.deployment;
    let p: &Progress = &ctx.progress;
    let mut retry = d.retry;
    if let Some(n) = args.retries {
        retry.attempts = n.saturating_add(1);
    }
    let session = connect(ctx, d).await?;
    let run_client = build_client!(Services, session)?;
    if let Some(name) = &args.preview {
        let rec = crate::deploy::Reconciler {
            run: &run_client,
            revisions: None,
            progress: p,
            poll: crate::poll::PollConfig::default(),
            timeout: args.timeout,
        };
        return crate::commands::traffic::remove_preview(ctx, d, &rec, name, args.yes).await;
    }
    let prov = Provisioner::for_teardown(d, &session, &run_client).await?;

    let (mut items, delete_service, delete_sa) =
        with_retry(&retry, p, "inspect current state", |_| {
            plan(d, &run_client, &prov, args.delete_images)
        })
        .await?;

    if !args.yes {
        match ctx.output {
            OutputFormat::Json => print_json(&Report {
                service: d.service_id.clone(),
                executed: false,
                items,
            }),
            OutputFormat::Text => {
                println!(
                    "Undeploy plan for {} (stage {}) in {}/{}:",
                    d.service_id, d.stage, d.project, d.region
                );
                print_items(&items);
                println!();
                println!("Nothing was deleted. Re-run with --yes to apply.");
            }
        }
        return Ok(());
    }

    execute(
        d,
        &run_client,
        &prov,
        &retry,
        p,
        &mut items,
        Teardown {
            delete_service,
            delete_sa,
            delete_images: args.delete_images,
            timeout: args.timeout,
        },
    )
    .await?;

    match ctx.output {
        OutputFormat::Json => print_json(&Report {
            service: d.service_id.clone(),
            executed: true,
            items,
        }),
        OutputFormat::Text => {
            p.success(format!("{} undeployed", d.service_id));
            println!("Removed and kept resources:");
            print_items(&items);
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
pub async fn execute(
    d: &Deployment,
    run: &Services,
    prov: &Provisioner<'_>,
    retry: &crate::retry::RetryConfig,
    progress: &Progress,
    items: &mut [Item],
    what: Teardown,
) -> Result<()> {
    let retry = *retry;
    // 1. Service (and everything attached to it).
    let name = d.service_name();
    let p = progress;
    if what.delete_service {
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
        p.step("Deleting images");
        let o = with_retry(&retry, p, "delete images", |_| {
            prov.delete_images(&b.artifact_location, &b.artifact_repository, &d.app)
        })
        .await?;
        let res = format!(
            "images {}-docker.pkg.dev/{}/{}/{}",
            b.artifact_location, d.project, b.artifact_repository, d.app
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
