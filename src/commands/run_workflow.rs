//! `runway run-workflow`: start a workflow of the stage now and, with
//! `--wait`, follow the execution and print its result.

use crate::build_client;
use crate::cli::{Context, RunWorkflowArgs};
use crate::commands::load;
use crate::config::Overrides;
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::api_error;
use crate::output::{OutputFormat, Progress, print_json};
use crate::poll::{PollConfig, Poller, Tick};
use google_cloud_workflows_executions_v1::client::Executions;
use google_cloud_workflows_executions_v1::model::{Execution, execution};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct RunResult {
    pub workflow: String,
    pub execution: String,
    /// `started`, `succeeded`, `failed` or `cancelled`.
    pub state: &'static str,
    /// The workflow's return value (JSON), once it succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The error (JSON) it failed with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub console: String,
}

pub async fn run(ctx: &Context, args: RunWorkflowArgs) -> Result<()> {
    let resolved = load(ctx, &args.stage.stage, &Overrides::default())?;
    let w = resolved
        .workflows
        .iter()
        .find(|w| w.key == args.name)
        .ok_or_else(|| {
            let names: Vec<&str> = resolved.workflows.iter().map(|w| w.key.as_str()).collect();
            Error::config(format!(
                "`{}` is not a workflow of stage `{}` ({})",
                args.name,
                args.stage.stage,
                if names.is_empty() {
                    "it has no workflows".to_string()
                } else {
                    format!("workflows: {}", names.join(", "))
                }
            ))
        })?;
    if let Some(data) = &args.data
        && let Err(e) = serde_json::from_str::<serde_json::Value>(data)
    {
        return Err(Error::config(format!("--data is not valid JSON: {e}"))
            .hint(r#"for example --data '{"full": true}'"#));
    }
    let d = resolved.first();
    let p = &ctx.progress;
    let session = crate::commands::connect(ctx, d).await?;
    let workflows = build_client!(google_cloud_workflows_v1::client::Workflows, session)?;
    let name = w.name(&d.project);
    let live = crate::gcp::workflows::get(&workflows, &name)
        .await?
        .ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("workflow {} is not deployed", w.id),
            )
            .hint(format!(
                "deploy it first: runway deploy --stage {}",
                d.stage
            ))
        })?;
    if !crate::gcp::workflows::owned(&live, &d.app, &d.stage) {
        return Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "workflow {} was not created by runway for this app and stage",
                w.id
            ),
        ));
    }
    let executions = build_client!(Executions, session)?;
    let r = start(
        &executions,
        &name,
        args.data.as_deref(),
        args.wait,
        args.timeout,
        p,
    )
    .await?;
    print(ctx, &r);
    match r.state {
        "failed" | "cancelled" => Err(Error::new(
            ErrorKind::Deploy,
            format!("execution {} of {} {}", r.execution, w.id, r.state),
        )
        .hint(format!("details: {}", r.console))),
        _ => Ok(()),
    }
}

/// Starts an execution of `workflow` (full name) and, with `wait`, follows
/// it until it finishes.
pub async fn start(
    executions: &Executions,
    workflow: &str,
    data: Option<&str>,
    wait: bool,
    timeout: std::time::Duration,
    p: &Progress,
) -> Result<RunResult> {
    let mut e = Execution::new();
    if let Some(a) = data {
        e = e.set_argument(a);
    }
    let started = executions
        .create_execution()
        .set_parent(workflow)
        .set_execution(e)
        .send()
        .await
        .map_err(|e| {
            api_error(
                e,
                &format!(
                    "starting workflow {}",
                    crate::gcp::workflows::short(workflow)
                ),
            )
            .hint("the caller needs workflows.executions.create (roles/workflows.invoker)")
        })?;
    let short = crate::gcp::workflows::short(&started.name).to_string();
    p.success(format!("started {short}"));
    let done = match wait {
        false => started,
        true => follow(executions, &started.name, timeout, p).await?,
    };
    Ok(result(workflow, &done))
}

fn console(name: &str) -> String {
    // projects/P/locations/L/workflows/W/executions/E
    let parts: Vec<&str> = name.split('/').collect();
    match parts.as_slice() {
        [
            "projects",
            p,
            "locations",
            l,
            "workflows",
            w,
            "executions",
            e,
        ] => format!(
            "https://console.cloud.google.com/workflows/workflow/{l}/{w}/execution/{e}?project={p}"
        ),
        _ => String::new(),
    }
}

pub fn result(workflow: &str, e: &Execution) -> RunResult {
    let state = match e.state {
        execution::State::Succeeded => "succeeded",
        execution::State::Failed | execution::State::Unavailable => "failed",
        execution::State::Cancelled => "cancelled",
        _ => "started",
    };
    RunResult {
        workflow: crate::gcp::workflows::short(workflow).to_string(),
        execution: crate::gcp::workflows::short(&e.name).to_string(),
        state,
        result: (state == "succeeded" && !e.result.is_empty()).then(|| e.result.clone()),
        error: e.error.as_ref().map(|x| x.payload.clone()),
        console: console(&e.name),
    }
}

async fn follow(
    executions: &Executions,
    name: &str,
    timeout: std::time::Duration,
    p: &Progress,
) -> Result<Execution> {
    let mut poller = Poller::new(PollConfig::default(), timeout);
    let mut last = String::new();
    loop {
        let e = executions
            .get_execution()
            .set_name(name)
            .send()
            .await
            .map_err(|e| api_error(e, "reading the workflow execution"))?;
        if !matches!(
            e.state,
            execution::State::Active | execution::State::Queued | execution::State::Unspecified
        ) {
            return Ok(e);
        }
        let now = e
            .status
            .as_ref()
            .and_then(|s| s.current_steps.last())
            .map(|s| format!("at step {}", s.step))
            .unwrap_or_else(|| "running".into());
        if now != last {
            p.info(&now);
            last = now;
        }
        match poller.wait().await {
            Tick::Continue => {}
            Tick::TimedOut => {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    "the execution is still running (it continues in Workflows)",
                ));
            }
            Tick::Cancelled => {
                return Err(Error::new(
                    ErrorKind::Interrupted,
                    "interrupted (the execution continues in Workflows)",
                ));
            }
        }
    }
}

fn print(ctx: &Context, r: &RunResult) {
    match ctx.output {
        OutputFormat::Json => print_json(r),
        OutputFormat::Text => {
            let p = crate::style::out();
            println!("{} {}: {}", p.bold(&r.workflow), r.execution, r.state);
            if let Some(res) = &r.result {
                println!("  result: {res}");
            }
            if let Some(err) = &r.error {
                println!("  error: {}", p.red(err));
            }
            if !r.console.is_empty() {
                println!("  details: {}", p.cyan(&r.console));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_execution_reports_its_result_or_error() {
        let name = "projects/my-gcp-project/locations/europe-west1/workflows/shop-nightly-prod/executions/abc";
        let ok = Execution::new()
            .set_name(name)
            .set_state(execution::State::Succeeded)
            .set_result(r#"{"rows":3}"#);
        let r = result("x/workflows/shop-nightly-prod", &ok);
        assert_eq!((r.state, r.execution.as_str()), ("succeeded", "abc"));
        assert_eq!(r.result.as_deref(), Some(r#"{"rows":3}"#));
        assert_eq!(
            r.console,
            "https://console.cloud.google.com/workflows/workflow/europe-west1/shop-nightly-prod/execution/abc?project=my-gcp-project"
        );
        let failed = Execution::new()
            .set_name(name)
            .set_state(execution::State::Failed)
            .set_error(execution::Error::new().set_payload(r#"{"message":"boom"}"#));
        let r = result("w", &failed);
        assert_eq!(r.state, "failed");
        assert_eq!(r.error.as_deref(), Some(r#"{"message":"boom"}"#));
        assert!(r.result.is_none());
    }
}
