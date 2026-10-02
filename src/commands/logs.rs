//! `runway logs`: recent application logs from Cloud Logging.

use crate::build_client;
use crate::cli::{Context, LogsArgs};
use crate::commands::load;
use crate::config::Overrides;
use crate::error::{Error, Result};
use crate::gcp::logging::{self, LogLine, LogQuery};
use crate::output::OutputFormat;
use google_cloud_logging_v2::client::LoggingServiceV2;

fn print_line(l: &LogLine, format: OutputFormat) {
    match format {
        OutputFormat::Json => match serde_json::to_string(l) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("cannot serialize log line: {e}"),
        },
        OutputFormat::Text => {
            let rev = if l.revision.is_empty() {
                String::new()
            } else {
                format!(" [{}]", l.revision)
            };
            let p = crate::style::out();
            println!(
                "{} {}{} {}",
                p.dim(&l.timestamp),
                p.severity(&format!("{:<8}", l.severity)),
                p.dim(&rev),
                l.message
            );
        }
    }
}

pub async fn run(ctx: &Context, args: LogsArgs) -> Result<()> {
    let resolved = load(ctx, &args.stage.stage, &Overrides::default())?;
    let d = &resolved.deployment;
    if args.limit == 0 || args.limit > 10_000 {
        return Err(Error::config("--limit must be between 1 and 10000"));
    }
    if let Some(s) = &args.severity {
        const LEVELS: &[&str] = &[
            "DEFAULT",
            "DEBUG",
            "INFO",
            "NOTICE",
            "WARNING",
            "ERROR",
            "CRITICAL",
            "ALERT",
            "EMERGENCY",
        ];
        if !LEVELS.contains(&s.to_ascii_uppercase().as_str()) {
            return Err(Error::config(format!(
                "--severity must be one of {}",
                LEVELS.join(", ")
            )));
        }
    }
    let q = LogQuery {
        project: d.project.clone(),
        region: d.region.clone(),
        service_id: d.service_id.clone(),
        since: logging::parse_since(&args.since)?,
        limit: args.limit,
        include_requests: args.include_requests,
        min_severity: args.severity.clone(),
    };
    let session = crate::commands::connect(ctx, d).await?;
    let client = build_client!(LoggingServiceV2, session)?;
    let filter = logging::service_filter(&q, chrono::Utc::now());
    tracing::debug!(%filter, "log filter");
    let lines = logging::fetch(&client, &d.project, &filter, q.limit).await?;

    // Text and --follow stream lines; plain JSON prints one array.
    if ctx.output == OutputFormat::Json && !args.follow {
        crate::output::print_json(&lines);
        return Ok(());
    }
    if lines.is_empty() && ctx.output == OutputFormat::Text {
        ctx.progress.info(format!(
            "no log entries for {} in the last {}",
            d.service_id, args.since
        ));
    }
    for l in &lines {
        print_line(l, ctx.output);
    }
    if args.follow {
        ctx.progress.info("following logs; press Ctrl-C to stop");
        let last = lines
            .last()
            .map(|l| l.timestamp.clone())
            .unwrap_or_else(|| {
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
            });
        let mut seen = logging::SeenIds::default();
        for l in &lines {
            seen.insert(&l.insert_id);
        }
        let format = ctx.output;
        logging::follow(&client, &q, last, seen, |l| print_line(l, format)).await?;
    }
    Ok(())
}
