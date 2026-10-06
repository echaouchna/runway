//! `runway describe`: diagram and explanation of the stack (offline).

use crate::cli::{Context, DescribeArgs, DiagramFormat};
use crate::config::{self, Overrides};
use crate::describe::{
    ascii, ascii_with, explain, explain_job, explain_schedules, explanation_text, mermaid,
};
use crate::error::{Error, Result};
use crate::output::{OutputFormat, print_json};
use serde::Serialize;

#[derive(Serialize)]
struct Description {
    app: String,
    stage: String,
    format: &'static str,
    diagram: String,
    explanation: Vec<crate::describe::Section>,
    /// Every selected service and job (the fields above are the first one's).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    workloads: Vec<Workload>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    schedules: Vec<crate::describe::Section>,
}

#[derive(Serialize)]
struct Workload {
    name: String,
    kind: &'static str,
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    diagram: String,
    explanation: Vec<crate::describe::Section>,
}

pub fn run(ctx: &Context, args: DescribeArgs) -> Result<()> {
    let cfg = config::load(&ctx.config)?;
    let stage = match args.stage {
        Some(s) => s,
        None => {
            let stages = cfg.stage_names();
            match stages.as_slice() {
                [only] => only.clone(),
                [] => return Err(Error::config("no stages are defined")),
                many => {
                    return Err(Error::new(
                        crate::error::ErrorKind::Config,
                        "several stages are defined",
                    )
                    .hint(format!("pass --stage ({})", many.join(", "))));
                }
            }
        }
    };
    let resolved = config::resolve(&cfg, &stage, &Overrides::default()).map_err(|d| {
        d.into_error(&format!(
            "{} is invalid for stage `{stage}`",
            ctx.config.display()
        ))
    })?;
    let selected = resolved.select(&args.only)?;
    let several = selected.len() > 1 || !resolved.schedules.is_empty();
    let diagram_of = |d: &config::Deployment| match (d.is_job(), args.format) {
        (true, _) => String::new(),
        (false, DiagramFormat::Ascii) => ascii(d),
        (false, DiagramFormat::Mermaid) => mermaid(d),
    };
    let explanation_of = |d: &config::Deployment| match d.is_job() {
        true => explain_job(d, &resolved),
        false => explain(d),
    };
    let format = match args.format {
        DiagramFormat::Ascii => "ascii",
        DiagramFormat::Mermaid => "mermaid",
    };
    let d = selected[0];
    let schedules = explain_schedules(&resolved);
    match ctx.output {
        OutputFormat::Json => print_json(&Description {
            app: d.app.clone(),
            stage: d.stage.clone(),
            format,
            diagram: diagram_of(d),
            explanation: explanation_of(d),
            workloads: match several {
                false => Vec::new(),
                true => selected
                    .iter()
                    .map(|w| Workload {
                        name: w.name().to_string(),
                        kind: if w.is_job() { "job" } else { "service" },
                        id: w.service_id.clone(),
                        diagram: diagram_of(w),
                        explanation: explanation_of(w),
                    })
                    .collect(),
            },
            schedules,
        }),
        OutputFormat::Text => {
            let p = crate::style::out();
            // Plain text stays Markdown; colors replace the heading marker.
            if p.enabled {
                println!(
                    "{} {}\n",
                    p.bold_cyan(&d.app),
                    p.dim(&format!("(stage {})", d.stage))
                );
            } else {
                println!("# {} (stage {})\n", d.app, d.stage);
            }
            for w in &selected {
                if several {
                    let title = format!("{} ({})", w.what(), w.service_id);
                    match p.enabled {
                        true => println!("{}\n", p.bold(&title)),
                        false => println!("## {title}\n"),
                    }
                }
                if !w.is_job() {
                    if args.format == DiagramFormat::Mermaid {
                        println!("```mermaid\n{}```\n", diagram_of(w));
                    } else {
                        println!("{}", ascii_with(w, p));
                    }
                }
                if !args.diagram_only || w.is_job() {
                    print!("{}", explanation_text(&explanation_of(w)));
                }
                if several {
                    println!();
                }
            }
            if !schedules.is_empty() && !args.diagram_only {
                print!("{}", explanation_text(&schedules));
            }
        }
    }
    Ok(())
}
