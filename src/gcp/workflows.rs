//! Google Cloud Workflows: what runway sends and compares, and the calls
//! that apply it. Ownership is in the labels, as on Cloud Run resources.

use crate::config::{CallLogLevel, MAX_ENV_DEFINITION_BYTES, WorkflowConfig};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::{api_error, is_not_found};
use google_cloud_lro::Poller;
use google_cloud_workflows_v1::client::Workflows;
use google_cloud_workflows_v1::model::{Workflow, workflow};
use std::collections::BTreeMap;

/// Upper bound for a deployment (create, update or delete).
const LRO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// What runway sets and replaces on update; anything else set by hand
/// (encryption key, execution history level, tags) is left alone.
pub const UPDATE_MASK: &[&str] = &[
    "description",
    "labels",
    "source_contents",
    "service_account",
    "call_log_level",
    "user_env_vars",
];

pub fn parent(project: &str, region: &str) -> String {
    format!("projects/{project}/locations/{region}")
}

pub fn labels(app: &str, stage: &str, key: &str) -> BTreeMap<String, String> {
    let mut l = crate::naming::ownership_labels(app, stage);
    l.insert(crate::naming::LABEL_NAME.into(), key.into());
    l
}

pub fn owned(w: &Workflow, app: &str, stage: &str) -> bool {
    crate::naming::ownership_labels(app, stage)
        .iter()
        .all(|(k, v)| w.labels.get(k) == Some(v))
}

/// The source and environment with every service URL filled in; `urls`
/// maps a service ID to its live URL. `Err`: a service with no URL yet.
pub fn render(
    w: &WorkflowConfig,
    urls: &BTreeMap<String, String>,
) -> std::result::Result<(String, BTreeMap<String, String>), String> {
    let url = |id: &str| urls.get(id).cloned();
    let source = w.source.render(&url)?;
    let mut env = BTreeMap::new();
    for (k, v) in &w.env {
        env.insert(k.clone(), v.render(&url)?);
    }
    Ok((source, env))
}

/// Limits only checkable once URLs are in (`desired`: from [`desired`]).
pub fn check_rendered(w: &WorkflowConfig, desired: &Workflow) -> Result<()> {
    for (k, v) in &desired.user_env_vars {
        if k.len() + 1 + v.len() > MAX_ENV_DEFINITION_BYTES {
            return Err(Error::config(format!(
                "workflows.{}.env.{k}: `NAME=value` is longer than 4 KiB, the Workflows limit",
                w.key
            ))
            .permanent());
        }
    }
    Ok(())
}

fn log_level(l: CallLogLevel) -> workflow::CallLogLevel {
    match l {
        CallLogLevel::None => workflow::CallLogLevel::LogNone,
        CallLogLevel::ErrorsOnly => workflow::CallLogLevel::LogErrorsOnly,
        CallLogLevel::AllCalls => workflow::CallLogLevel::LogAllCalls,
    }
}

pub fn desired(
    w: &WorkflowConfig,
    project: &str,
    app: &str,
    stage: &str,
    source: String,
    env: BTreeMap<String, String>,
) -> Workflow {
    Workflow::new()
        .set_name(w.name(project))
        .set_description(&w.description)
        .set_labels(labels(app, stage, &w.key))
        .set_service_account(&w.service_account)
        .set_call_log_level(log_level(w.call_log_level))
        .set_user_env_vars(env)
        .set_source_contents(source)
}

/// A short, stable summary of a source: its size and hash.
pub fn source_summary(source: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(source.as_bytes());
    let hex: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("{} lines, sha256 {hex}", source.lines().count())
}

/// What runway compares on a workflow.
pub fn flat(w: &Workflow) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("description".into(), w.description.clone());
    // Workflows answers `projects/P/serviceAccounts/EMAIL`.
    let sa = w
        .service_account
        .rsplit('/')
        .next()
        .unwrap_or(&w.service_account);
    m.insert("service_account".into(), sa.to_string());
    m.insert(
        "call_log_level".into(),
        match &w.call_log_level {
            workflow::CallLogLevel::LogAllCalls => "all-calls",
            workflow::CallLogLevel::LogErrorsOnly => "errors-only",
            _ => "none",
        }
        .into(),
    );
    let source = match &w.source_code {
        Some(workflow::SourceCode::SourceContents(s)) => s.as_str(),
        _ => "",
    };
    m.insert("source".into(), source_summary(source));
    for (k, v) in &w.user_env_vars {
        m.insert(format!("env.{k}"), v.clone());
    }
    m
}

pub async fn get(client: &Workflows, name: &str) -> Result<Option<Workflow>> {
    match client.get_workflow().set_name(name).send().await {
        Ok(w) => Ok(Some(w)),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(api_error(e, &format!("reading workflow {}", short(name)))
            .hint("the deployer needs workflows.workflows.get (roles/workflows.viewer)")),
    }
}

/// The regions where Workflows is available to the project, plus `known`
/// (the regions runway.yaml names). The configuration does not say where a
/// workflow removed from it, or moved, was deployed: every region is looked
/// at. Without the list (no such API, an endpoint that has none), `known`.
pub async fn regions(client: &Workflows, project: &str, known: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = known.to_vec();
    let mut token = String::new();
    loop {
        let resp = match client
            .list_locations()
            .set_name(format!("projects/{project}"))
            .set_page_token(token.clone())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if is_not_found(&e) || crate::gcp::is_service_disabled(&e) => return Ok(out),
            Err(e) => {
                return Err(api_error(e, "listing the Workflows regions")
                    .hint("the deployer needs workflows.locations.list (roles/workflows.viewer)"));
            }
        };
        for l in resp.locations {
            if !l.location_id.is_empty() && !out.contains(&l.location_id) {
                out.push(l.location_id);
            }
        }
        if resp.next_page_token.is_empty() {
            return Ok(out);
        }
        token = resp.next_page_token;
    }
}

/// Every workflow of the project in `regions`, whoever created it (a few
/// regions at a time).
pub async fn list_all(
    client: &Workflows,
    project: &str,
    regions: &[String],
) -> Result<Vec<Workflow>> {
    use futures::StreamExt;
    let lists: Vec<Result<Vec<Workflow>>> = futures::stream::iter(regions)
        .map(|r| list_in(client, parent(project, r)))
        .buffer_unordered(8)
        .collect()
        .await;
    let mut out = Vec::new();
    for l in lists {
        out.extend(l?);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Workflows runway created for this app and stage, in every region (see
/// [`regions`]).
pub async fn list_owned(
    client: &Workflows,
    project: &str,
    known_regions: &[String],
    app: &str,
    stage: &str,
) -> Result<Vec<Workflow>> {
    let regions = regions(client, project, known_regions).await?;
    Ok(list_all(client, project, &regions)
        .await?
        .into_iter()
        .filter(|w| owned(w, app, stage))
        .collect())
}

/// Whether `w` runs as `email` (Workflows answers with the account's
/// resource name).
pub fn runs_as(w: &Workflow, email: &str) -> bool {
    w.service_account
        .rsplit('/')
        .next()
        .is_some_and(|sa| sa.eq_ignore_ascii_case(email))
}

/// The workflows of one region.
async fn list_in(client: &Workflows, parent: String) -> Result<Vec<Workflow>> {
    let parent = parent.as_str();
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let resp = match client
            .list_workflows()
            .set_parent(parent)
            .set_page_token(token.clone())
            .send()
            .await
        {
            Ok(r) => r,
            // Without the Workflows API there can be no workflow.
            Err(e) if is_not_found(&e) || crate::gcp::is_service_disabled(&e) => return Ok(out),
            Err(e) => {
                return Err(api_error(e, &format!("listing workflows in {parent}"))
                    .hint("the deployer needs workflows.workflows.list (roles/workflows.viewer)"));
            }
        };
        out.extend(resp.workflows);
        if resp.next_page_token.is_empty() {
            return Ok(out);
        }
        token = resp.next_page_token;
    }
}

/// Creates or updates the workflow and waits for the deployment.
pub async fn apply(
    client: &Workflows,
    parent: &str,
    id: &str,
    desired: Workflow,
    exists: bool,
) -> Result<Workflow> {
    let hint = "the deployer needs roles/workflows.editor, and iam.serviceAccounts.actAs on the workflow's account (roles/iam.serviceAccountUser)";
    let op = async {
        match exists {
            true => {
                client
                    .update_workflow()
                    .set_workflow(desired)
                    .set_update_mask(
                        google_cloud_wkt::FieldMask::default()
                            .set_paths(UPDATE_MASK.iter().map(|s| s.to_string())),
                    )
                    .poller()
                    .until_done()
                    .await
            }
            false => {
                client
                    .create_workflow()
                    .set_parent(parent)
                    .set_workflow_id(id)
                    .set_workflow(desired)
                    .poller()
                    .until_done()
                    .await
            }
        }
    };
    let what = if exists { "updating" } else { "creating" };
    match tokio::time::timeout(LRO_TIMEOUT, op).await {
        Err(_) => Err(Error::new(
            ErrorKind::Timeout,
            format!("timed out {what} workflow {id}"),
        )),
        Ok(Ok(w)) => Ok(w),
        Ok(Err(e)) => Err(api_error(e, &format!("{what} workflow {id}")).hint(hint)),
    }
}

/// Deletes a workflow after reading it again: only one runway created for
/// this app and stage (false when it is already gone).
pub async fn delete_owned(client: &Workflows, name: &str, app: &str, stage: &str) -> Result<bool> {
    match get(client, name).await? {
        None => return Ok(false),
        Some(w) if !owned(&w, app, stage) => {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!(
                    "workflow {} was not created by runway for app `{app}` stage `{stage}`; not deleting it",
                    short(name)
                ),
            )
            .permanent());
        }
        Some(_) => {}
    }
    let op = client
        .delete_workflow()
        .set_name(name)
        .poller()
        .until_done();
    match tokio::time::timeout(LRO_TIMEOUT, op).await {
        Err(_) => Err(Error::new(
            ErrorKind::Timeout,
            format!("timed out deleting workflow {}", short(name)),
        )),
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) if is_not_found(&e) => Ok(false),
        Ok(Err(e)) => Err(api_error(e, &format!("deleting workflow {}", short(name)))
            .hint("the deployer needs workflows.workflows.delete (roles/workflows.editor)")),
    }
}

pub fn short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IdentityConfig, Template, TemplatePart};

    fn workflow() -> WorkflowConfig {
        WorkflowConfig {
            key: "nightly".into(),
            id: "shop-nightly-prod".into(),
            region: "europe-west1".into(),
            source_file: "workflows/nightly.yaml".into(),
            source: Template(vec![
                TemplatePart::Text("main:\n  steps:\n    - call:\n        url: ".into()),
                TemplatePart::ServiceUrl("shop-api-prod".into()),
                TemplatePart::Text("/tasks\n".into()),
            ]),
            description: "Nightly report".into(),
            env: [
                (
                    "API_URL".to_string(),
                    Template(vec![TemplatePart::ServiceUrl("shop-api-prod".into())]),
                ),
                ("STAGE".to_string(), Template::text("prod")),
            ]
            .into(),
            call_log_level: CallLogLevel::ErrorsOnly,
            service_account: "shop-prod-wf@my-gcp-project.iam.gserviceaccount.com".into(),
            identity: IdentityConfig::default(),
            invokers: Vec::new(),
            targets: Vec::new(),
        }
    }

    #[test]
    fn urls_are_filled_in_and_a_missing_one_is_named() {
        let w = workflow();
        assert_eq!(w.service_urls(), vec!["shop-api-prod"]);
        assert_eq!(render(&w, &BTreeMap::new()).unwrap_err(), "shop-api-prod");
        let urls = [(
            "shop-api-prod".to_string(),
            "https://shop-api-prod-abc-ew.a.run.app".to_string(),
        )]
        .into();
        let (source, env) = render(&w, &urls).unwrap();
        assert!(source.contains("url: https://shop-api-prod-abc-ew.a.run.app/tasks"));
        assert_eq!(env["API_URL"], "https://shop-api-prod-abc-ew.a.run.app");
        assert_eq!(env["STAGE"], "prod");
    }

    #[test]
    fn what_is_sent_compares_equal_to_what_workflows_answers() {
        let w = workflow();
        let urls = [("shop-api-prod".to_string(), "https://a.run.app".to_string())].into();
        let (source, env) = render(&w, &urls).unwrap();
        let sent = desired(&w, "my-gcp-project", "shop", "prod", source, env);
        assert!(owned(&sent, "shop", "prod") && !owned(&sent, "shop", "dev"));
        assert_eq!(sent.labels["runway-name"], "nightly");
        // Workflows answers with the account's resource name.
        let mut live = sent.clone();
        live.service_account = format!(
            "projects/my-gcp-project/serviceAccounts/{}",
            w.service_account
        );
        assert!(crate::plan::diff(&flat(&live), &flat(&sent)).is_empty());
        let f = flat(&sent);
        assert_eq!(f["call_log_level"], "errors-only");
        assert_eq!(f["env.API_URL"], "https://a.run.app");
        assert!(
            f["source"].starts_with("4 lines, sha256 "),
            "{}",
            f["source"]
        );
        // Logging left unset reads as `none`.
        live.call_log_level = workflow::CallLogLevel::Unspecified;
        assert_eq!(flat(&live)["call_log_level"], "none");
    }

    #[test]
    fn an_environment_variable_too_long_once_rendered_is_refused() {
        let w = workflow();
        let env = [("API_URL".to_string(), "x".repeat(MAX_ENV_DEFINITION_BYTES))].into();
        let sent = desired(&w, "my-gcp-project", "shop", "prod", String::new(), env);
        let e = check_rendered(&w, &sent).unwrap_err();
        assert!(
            e.message.contains("workflows.nightly.env.API_URL"),
            "{}",
            e.message
        );
    }
}
