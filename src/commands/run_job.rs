//! `runway run-job`: run a Cloud Run job of the stage now and, with `--wait`,
//! follow the execution until it finishes.

use crate::build_client;
use crate::cli::{Context, RunJobArgs};
use crate::commands::load;
use crate::config::Overrides;
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::api_error;
use crate::gcp::jobs::{JobReconciler, check_job_ownership};
use crate::output::{OutputFormat, print_json};
use crate::poll::{PollConfig, Poller, Tick};
use google_cloud_run_v2::client::{Executions, Jobs};
use google_cloud_run_v2::model::Execution;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct RunResult {
    job: String,
    execution: String,
    /// `started`, `succeeded` or `failed`.
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    succeeded_tasks: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_tasks: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    log_uri: String,
}

pub async fn run(ctx: &Context, args: RunJobArgs) -> Result<()> {
    let resolved = load(ctx, &args.stage.stage, &Overrides::default())?;
    let d = resolved
        .jobs()
        .find(|d| d.name() == args.name)
        .ok_or_else(|| {
            let jobs: Vec<&str> = resolved.jobs().map(|d| d.name()).collect();
            Error::config(format!(
                "`{}` is not a job of stage `{}` ({})",
                args.name,
                args.stage.stage,
                if jobs.is_empty() {
                    "it has no jobs".to_string()
                } else {
                    format!("jobs: {}", jobs.join(", "))
                }
            ))
        })?;
    let p = &ctx.progress;
    let session = crate::commands::connect(ctx, d).await?;
    let jobs = build_client!(Jobs, session)?;
    let name = d.service_name();
    let rec = JobReconciler {
        jobs: &jobs,
        progress: p,
        poll: PollConfig::default(),
        timeout: args.timeout,
    };
    let job = rec.get(&name).await?.ok_or_else(|| {
        Error::new(
            ErrorKind::NotFound,
            format!("job {} is not deployed", d.service_id),
        )
        .hint(format!(
            "deploy it first: runway deploy --stage {}",
            d.stage
        ))
    })?;
    check_job_ownership(&job, &d.app, &d.stage)?;
    let op = jobs
        .run_job()
        .set_name(&name)
        .send()
        .await
        .map_err(|e| api_error(e, &format!("running job {}", d.service_id)))?;
    let execution = op
        .metadata
        .as_ref()
        .and_then(|m| m.to_msg::<Execution>().ok())
        .map(|e| e.name)
        .unwrap_or_default();
    let short = execution
        .rsplit('/')
        .next()
        .unwrap_or(&execution)
        .to_string();
    p.success(format!("started {short}"));
    let mut result = RunResult {
        job: d.service_id.clone(),
        execution: short.clone(),
        state: "started",
        succeeded_tasks: None,
        failed_tasks: None,
        log_uri: String::new(),
    };
    if args.wait && !execution.is_empty() {
        let executions = build_client!(Executions, session)?;
        let done = wait(&executions, &execution, args.timeout, p).await?;
        result.succeeded_tasks = Some(done.succeeded_count);
        result.failed_tasks = Some(done.failed_count);
        result.log_uri = done.log_uri.clone();
        let ok = done.failed_count == 0 && done.cancelled_count == 0;
        result.state = if ok { "succeeded" } else { "failed" };
        print(ctx, &result);
        if !ok {
            return Err(Error::new(
                ErrorKind::Deploy,
                format!(
                    "{short} failed: {} task(s) failed, {} cancelled",
                    done.failed_count, done.cancelled_count
                ),
            )
            .hint(format!(
                "logs: runway logs --stage {} --only {}",
                d.stage,
                d.name()
            )));
        }
        return Ok(());
    }
    print(ctx, &result);
    Ok(())
}

fn print(ctx: &Context, r: &RunResult) {
    match ctx.output {
        OutputFormat::Json => print_json(r),
        OutputFormat::Text => {
            let p = crate::style::out();
            println!("{} {}: {}", p.bold(&r.job), r.execution, r.state);
            if let (Some(s), Some(f)) = (r.succeeded_tasks, r.failed_tasks) {
                println!("  tasks: {s} succeeded, {f} failed");
            }
            if !r.log_uri.is_empty() {
                println!("  logs: {}", p.cyan(&r.log_uri));
            }
        }
    }
}

/// Polls the execution until it has completed.
async fn wait(
    executions: &Executions,
    name: &str,
    timeout: std::time::Duration,
    p: &crate::output::Progress,
) -> Result<Execution> {
    let mut poller = Poller::new(PollConfig::default(), timeout);
    let mut last = String::new();
    loop {
        let e = executions
            .get_execution()
            .set_name(name)
            .send()
            .await
            .map_err(|e| api_error(e, "reading the job execution"))?;
        if e.completion_time.is_some() {
            return Ok(e);
        }
        let now = format!(
            "{} running, {} succeeded, {} failed",
            e.running_count, e.succeeded_count, e.failed_count
        );
        if now != last {
            p.info(&now);
            last = now;
        }
        match poller.wait().await {
            Tick::Continue => {}
            Tick::TimedOut => {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    "the execution is still running (it continues in Cloud Run)",
                ));
            }
            Tick::Cancelled => {
                return Err(Error::new(
                    ErrorKind::Interrupted,
                    "interrupted (the execution continues in Cloud Run)",
                ));
            }
        }
    }
}
