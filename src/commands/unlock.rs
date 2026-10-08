//! `runway unlock`: who holds the stage's lease, and removing it when the
//! run that took it is gone (it would otherwise expire on its own).

use crate::build_client;
use crate::cli::{Context, UnlockArgs};
use crate::commands::{connect, lease_target, load};
use crate::config::Overrides;
use crate::error::Result;
use crate::output::{OutputFormat, print_json};
use google_cloud_run_v2::client::{Jobs, Services};
use serde::Serialize;

#[derive(Serialize)]
struct Report {
    stage: String,
    holder: String,
    holders: Vec<crate::lease::Holder>,
    removed: bool,
}

pub async fn run(ctx: &Context, args: UnlockArgs) -> Result<()> {
    let r = load(ctx, &args.stage, &Overrides::default())?;
    let first = r.first();
    let session = connect(ctx, first).await?;
    let run = build_client!(Services, session)?;
    let jobs = build_client!(Jobs, session)?;
    let target = lease_target(&r, &run, Some(&jobs)).expect("jobs client given");
    let h = r.holder();
    let holders = match args.yes {
        true => target.clear(&h.app, &h.stage).await?,
        false => target.holders().await?,
    };
    let report = Report {
        stage: args.stage.clone(),
        holder: r.holder().service_id.clone(),
        holders,
        removed: args.yes,
    };
    let now = chrono::Utc::now();
    match ctx.output {
        OutputFormat::Json => print_json(&report),
        OutputFormat::Text => {
            let c = crate::style::out();
            if report.holders.is_empty() {
                println!(
                    "Stage {} is free: nobody holds the lease on {}.",
                    report.stage, report.holder
                );
                return Ok(());
            }
            println!(
                "{} lease on {} (stage {}):",
                if report.removed { "Removed the" } else { "The" },
                report.holder,
                report.stage
            );
            for h in &report.holders {
                println!("  {} {}", c.yellow("•"), h.describe(now));
            }
            if !report.removed {
                println!();
                println!(
                    "A run that is gone lets its lease expire within {}; `runway unlock --stage {} --yes` removes it now.",
                    crate::lease::short_duration(crate::lease::TTL),
                    report.stage
                );
            }
        }
    }
    Ok(())
}
