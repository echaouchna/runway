//! `runway info`: service URL, revision and status.

use crate::build_client;
use crate::cli::{Context, InfoArgs};
use crate::commands::{load, not_deployed};
use crate::config::Overrides;
use crate::deploy::Reconciler;
use crate::error::Result;
use crate::gcp::run::{self, Ownership, Readiness, ServiceStatus};
use crate::output::{OutputFormat, print_json};
use crate::poll::PollConfig;
use google_cloud_run_v2::client::Services;
use serde::Serialize;
use std::time::Duration;

#[derive(Serialize)]
struct Info {
    app: String,
    stage: String,
    project: String,
    region: String,
    #[serde(flatten)]
    status: ServiceStatus,
    /// `null` when the IAM policy cannot be read.
    public: Option<bool>,
    iap: bool,
}

/// `Traffic:` lines: percentage, revision, and the URL of tagged revisions.
pub(crate) fn print_traffic(lines: &[run::TrafficLine]) {
    if lines.is_empty() {
        return;
    }
    let p = crate::style::out();
    println!("Traffic:");
    for t in lines {
        let tag = if t.tag.is_empty() {
            String::new()
        } else {
            format!("  {} {}", p.bold(&t.tag), p.cyan(&t.uri))
        };
        println!("  {:>3}% {}{tag}", t.percent, t.revision);
    }
}

pub async fn run(ctx: &Context, args: InfoArgs) -> Result<()> {
    let resolved = load(ctx, &args.stage.stage, &Overrides::default())?;
    let d = &resolved.deployment;
    let session = crate::commands::connect(ctx, d).await?;
    let client = build_client!(Services, session)?;
    let rec = Reconciler {
        run: &client,
        revisions: None,
        progress: &ctx.progress,
        poll: PollConfig::default(),
        timeout: Duration::from_secs(30),
    };
    let name = d.service_name();
    // Service and IAM policy are independent reads: fetch them concurrently.
    let (svc, public) = tokio::join!(rec.get(&name), rec.current_public(&name));
    let svc = svc?.ok_or_else(|| not_deployed(&d.service_id, &d.stage))?;
    let public = public?;
    let info = Info {
        app: d.app.clone(),
        stage: d.stage.clone(),
        project: d.project.clone(),
        region: d.region.clone(),
        status: run::status(&svc, &d.app, &d.stage),
        public,
        iap: svc.iap_enabled,
    };
    match ctx.output {
        OutputFormat::Json => print_json(&info),
        OutputFormat::Text => {
            let s = &info.status;
            let p = crate::style::out();
            println!(
                "Service:   {} ({}/{})",
                p.bold(&d.service_id),
                d.project,
                d.region
            );
            println!(
                "URL:       {}",
                p.bold_cyan(s.url.as_deref().unwrap_or("(none)"))
            );
            let state = match &s.readiness {
                Readiness::Ready => p.bold_green("ready"),
                Readiness::Reconciling { message } => p.yellow(&format!("deploying ({message})")),
                Readiness::Failed { message } => p.bold_red(&format!("FAILED: {message}")),
            };
            println!("Status:    {state}");
            println!(
                "Revision:  {} (latest created: {})",
                s.latest_ready_revision, s.latest_created_revision
            );
            println!("Image:     {}", s.image.as_deref().unwrap_or("(unknown)"));
            if let Some(r) = &s.image_ref {
                println!("From:      {r}");
            }
            if let Some(rel) = &s.release {
                println!("Release:   {}", p.bold_green(rel));
            }
            println!(
                "Access:    {}",
                match info.public {
                    _ if info.iap => "Identity-Aware Proxy (signed-in IAP members only)",
                    Some(true) => "public (allUsers can invoke)",
                    Some(false) => "private (IAM authentication required)",
                    None => "unknown (cannot read IAM policy)",
                }
            );
            print_traffic(&s.traffic);
            if let Some(t) = &s.update_time {
                println!(
                    "Updated:   {t} by {}",
                    if s.last_modifier.is_empty() {
                        "unknown"
                    } else {
                        &s.last_modifier
                    }
                );
            }
            match &s.ownership {
                Ownership::Owned => {}
                Ownership::Unmanaged => {
                    println!("Note:      not managed by runway (no runway labels)")
                }
                Ownership::OtherOwner { app, stage } => {
                    println!("Note:      managed by runway for app `{app}` stage `{stage}`")
                }
            }
        }
    }
    Ok(())
}
