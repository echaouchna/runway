//! `runway validate`: local validation without credentials.

use crate::cli::{Context, ValidateArgs};
use crate::config::{self, Artifact, Issue, Overrides};
use crate::error::{Error, Result};
use crate::output::{OutputFormat, print_json};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
struct StageReport {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
    errors: Vec<Issue>,
    warnings: Vec<Issue>,
}

#[derive(Serialize)]
struct Report {
    config: String,
    valid: bool,
    stages: BTreeMap<String, StageReport>,
}

pub fn run(ctx: &Context, args: ValidateArgs) -> Result<()> {
    let cfg = config::load(&ctx.config)?;
    let overrides = Overrides::default();
    let results = match &args.stage {
        Some(s) => vec![(s.clone(), config::resolve(&cfg, s, &overrides))],
        None => config::resolve_all(&cfg, &overrides),
    };
    let mut report = Report {
        config: ctx.config.display().to_string(),
        valid: true,
        stages: BTreeMap::new(),
    };
    for (stage, r) in results {
        let entry = match r {
            Ok(res) => StageReport {
                valid: true,
                service: Some(format!(
                    "{} ({}/{})",
                    res.deployments
                        .iter()
                        .map(|d| match d.is_job() {
                            true => format!("job {}", d.service_id),
                            false => d.service_id.clone(),
                        })
                        .chain(res.schedules.iter().map(|s| format!("schedule {}", s.key)))
                        .collect::<Vec<_>>()
                        .join(", "),
                    res.first().project,
                    res.first().region
                )),
                mode: Some(
                    match (
                        res.deployments
                            .iter()
                            .any(|d| matches!(d.artifact, Artifact::Image { .. })),
                        res.deployments
                            .iter()
                            .any(|d| matches!(d.artifact, Artifact::Build(_))),
                    ) {
                        _ if res
                            .deployments
                            .iter()
                            .any(|d| matches!(d.artifact, Artifact::Promote(_))) =>
                        {
                            "promotion"
                        }
                        (true, true) => "images and source builds",
                        (true, false) => "image",
                        _ => "source build",
                    },
                ),
                errors: vec![],
                warnings: res.warnings,
            },
            Err(d) => {
                report.valid = false;
                StageReport {
                    valid: false,
                    service: None,
                    mode: None,
                    errors: d.errors,
                    warnings: d.warnings,
                }
            }
        };
        report.stages.insert(stage, entry);
    }

    match ctx.output {
        OutputFormat::Json => print_json(&report),
        OutputFormat::Text => {
            let p = crate::style::out();
            for (stage, r) in &report.stages {
                if r.valid {
                    println!(
                        "{} stage {}: {} {}",
                        p.green("✓"),
                        p.bold(stage),
                        r.service.as_deref().unwrap_or(""),
                        p.dim(&format!("[{}]", r.mode.unwrap_or("")))
                    );
                } else {
                    println!("{} stage {}:", p.bold_red("✗"), p.bold(stage));
                    for e in &r.errors {
                        println!("    {}   {e}", p.red("error"));
                    }
                }
                for w in &r.warnings {
                    println!("    {} {w}", p.yellow("warning"));
                }
            }
        }
    }
    if report.valid {
        if ctx.output == OutputFormat::Text {
            println!(
                "{}",
                crate::style::out().bold_green(&format!("{} is valid", ctx.config.display()))
            );
        }
        Ok(())
    } else {
        let e = Error::config(format!("{} is invalid", ctx.config.display()));
        Err(if ctx.output == OutputFormat::Json {
            e.reported()
        } else {
            e
        })
    }
}
