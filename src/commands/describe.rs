//! `runway describe`: diagram and explanation of the stack (offline).

use crate::cli::{Context, DescribeArgs, DiagramFormat};
use crate::config::{self, Overrides};
use crate::describe::{ascii, ascii_with, explain, explanation_text, mermaid};
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
    let d = &resolved.deployment;
    let (format, diagram) = match args.format {
        DiagramFormat::Ascii => ("ascii", ascii(d)),
        DiagramFormat::Mermaid => ("mermaid", mermaid(d)),
    };
    let explanation = explain(d);
    match ctx.output {
        OutputFormat::Json => print_json(&Description {
            app: d.app.clone(),
            stage: d.stage.clone(),
            format,
            diagram,
            explanation,
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
            if args.format == DiagramFormat::Mermaid {
                println!("```mermaid\n{diagram}```\n");
            } else {
                println!("{}", ascii_with(d, p));
            }
            if !args.diagram_only {
                print!("{}", explanation_text(&explanation));
            }
        }
    }
    Ok(())
}
