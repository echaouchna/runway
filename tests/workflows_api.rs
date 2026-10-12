//! Wire-level tests of Google Cloud Workflows: deployment with the URLs of
//! the stage's services, ownership, updates, leftovers, executions and the
//! scheduler's project role. Real SDK clients against a mock server.

use google_cloud_auth::credentials::anonymous;
use runway::config::{self, Overrides};
use runway::gcp::Session;
use runway::output::Progress;
use runway::provision::{
    Endpoints, Provisioner, Step, StepOutcome, StepState, schedule_grant_steps,
};
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const PARENT: &str = "projects/my-gcp-project/locations/europe-west1";
const WORKFLOW: &str = "projects/my-gcp-project/locations/europe-west1/workflows/shop-etl-prod";
const ACCOUNT: &str = "shop-prod-wf@my-gcp-project.iam.gserviceaccount.com";
const API_URL: &str = "https://shop-api-prod-abc-ew.a.run.app";
const SOURCE: &str = "main:\n  steps:\n    - ping:\n        call: http.get\n        args: {url: \"${runway.services.api.url}/health\", auth: {type: OIDC}}\n";

fn resolved() -> (tempfile::TempDir, config::Resolved) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("etl.yaml"), SOURCE).unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        r#"
version: 1
app: shop
provider: {project: my-gcp-project, region: europe-west1}
defaults:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop@sha256:0000000000000000000000000000000000000000000000000000000000000000
  service_account: shop-runtime@my-gcp-project.iam.gserviceaccount.com
services:
  api: {}
jobs:
  export: {}
workflows:
  etl:
    source: etl.yaml
    description: Nightly export
    call_log_level: errors-only
    env: {API_URL: "${services.api.url}", EXPORT_JOB: "${jobs.export.name}"}
schedules:
  start-etl: {schedule: "0 4 * * *", workflow: etl}
stages: { prod: {} }
"#,
    )
    .unwrap();
    let (_, r) = config::load_and_resolve(&p, "prod", &Overrides::default()).unwrap();
    (dir, r)
}

/// What Workflows answers for the workflow runway deploys (`source`: its
/// rendered source; `labels`: runway's, or none).
fn live(source: &str, owned: bool) -> Value {
    json!({
        "name": WORKFLOW,
        "description": "Nightly export",
        "labels": if owned {
            json!({"managed-by": "runway", "runway-app": "shop", "runway-stage": "prod", "runway-name": "etl"})
        } else {
            json!({})
        },
        "serviceAccount": format!("projects/my-gcp-project/serviceAccounts/{ACCOUNT}"),
        "callLogLevel": "LOG_ERRORS_ONLY",
        "userEnvVars": {
            "API_URL": API_URL,
            "EXPORT_JOB": format!("{PARENT}/jobs/shop-export-prod")
        },
        "sourceContents": source,
        "state": "ACTIVE",
        "revisionId": "000001-abc"
    })
}

fn rendered() -> String {
    SOURCE.replace("${runway.services.api.url}", API_URL)
}

/// A finished operation whose response is `workflow`.
fn done(workflow: Value) -> ResponseTemplate {
    let mut response = workflow;
    response["@type"] = json!("type.googleapis.com/google.cloud.workflows.v1.Workflow");
    ResponseTemplate::new(200).set_body_json(json!({
        "name": format!("{PARENT}/operations/op-1"),
        "done": true,
        "response": response
    }))
}

async fn mock_service(server: &MockServer, exists: bool) {
    let name = format!("/v2/{PARENT}/services/shop-api-prod");
    let resp = match exists {
        true => ResponseTemplate::new(200).set_body_json(json!({
            "name": name.trim_start_matches("/v2/"), "uri": API_URL
        })),
        false => ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}
        })),
    };
    Mock::given(method("GET"))
        .and(path(name))
        .respond_with(resp)
        .mount(server)
        .await;
}

async fn mock_workflow(server: &MockServer, body: Option<Value>) {
    let resp = match body {
        Some(b) => ResponseTemplate::new(200).set_body_json(b),
        None => ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}
        })),
    };
    Mock::given(method("GET"))
        .and(path(format!("/v1/{WORKFLOW}")))
        .respond_with(resp)
        .mount(server)
        .await;
}

async fn session_and_run(server: &MockServer) -> (Session, google_cloud_run_v2::client::Services) {
    let session = Session::from_static_token("t").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    (session, run)
}

fn endpoints(uri: &str) -> Endpoints {
    Endpoints {
        run: Some(uri.into()),
        workflows: Some(uri.into()),
        scheduler: Some(uri.into()),
        iam: Some(uri.into()),
        resource_manager: Some(uri.into()),
        tag_bindings: Some(uri.into()),
        iap: Some(uri.into()),
        ..Default::default()
    }
}

fn workflow_step(r: &config::Resolved) -> Step {
    Step::Workflow(Box::new(r.workflows[0].clone()))
}

#[tokio::test]
async fn a_workflow_is_created_with_the_url_of_the_service_it_calls() {
    let server = MockServer::start().await;
    mock_service(&server, true).await;
    mock_workflow(&server, None).await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/{PARENT}/workflows")))
        .and(query_param("workflowId", "shop-etl-prod"))
        .respond_with(|r: &Request| {
            let mut b: Value = serde_json::from_slice(&r.body).unwrap();
            b["revisionId"] = json!("000001-abc");
            done(b)
        })
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let res = prov.apply(&workflow_step(&r)).await.unwrap();
    assert_eq!(res.outcome, StepOutcome::Changed);
    assert_eq!(res.detail, "created (revision 000001-abc)");

    let reqs = server.received_requests().await.unwrap();
    let create = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let b: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(b["sourceContents"], rendered());
    assert_eq!(b["userEnvVars"]["API_URL"], API_URL);
    assert_eq!(
        b["userEnvVars"]["EXPORT_JOB"],
        format!("{PARENT}/jobs/shop-export-prod")
    );
    assert_eq!(b["serviceAccount"], ACCOUNT);
    // The SDK sends enums as numbers: 2 is LOG_ERRORS_ONLY.
    assert_eq!(b["callLogLevel"], 2);
    assert_eq!(b["labels"]["managed-by"], "runway");
    assert_eq!(b["labels"]["runway-name"], "etl");
    assert_eq!(b["description"], "Nightly export");
}

#[tokio::test]
async fn an_unchanged_workflow_is_left_alone_and_a_changed_one_is_patched() {
    let server = MockServer::start().await;
    mock_service(&server, true).await;
    mock_workflow(&server, Some(live(&rendered(), true))).await;
    Mock::given(method("PATCH"))
        .respond_with(|r: &Request| done(serde_json::from_slice(&r.body).unwrap()))
        .expect(0)
        .mount(&server)
        .await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let check = prov.check(&workflow_step(&r), true).await;
    assert_eq!(check.state, StepState::InSync, "{}", check.detail);
    let res = prov.apply(&workflow_step(&r)).await.unwrap();
    assert_eq!(res.outcome, StepOutcome::Unchanged);
    server.verify().await;

    // The live source differs: only runway's fields are replaced.
    let server = MockServer::start().await;
    mock_service(&server, true).await;
    mock_workflow(&server, Some(live("main: {steps: []}\n", true))).await;
    Mock::given(method("PATCH"))
        .and(path(format!("/v1/{WORKFLOW}")))
        .respond_with(|r: &Request| done(serde_json::from_slice(&r.body).unwrap()))
        .expect(1)
        .mount(&server)
        .await;
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let check = prov.check(&workflow_step(&r), true).await;
    assert_eq!(check.state, StepState::Pending);
    assert!(
        check.detail.contains("~ source: 1 lines"),
        "{}",
        check.detail
    );
    let res = prov.apply(&workflow_step(&r)).await.unwrap();
    assert_eq!(res.outcome, StepOutcome::Changed);
    let reqs = server.received_requests().await.unwrap();
    let patch = reqs.iter().find(|r| r.method.as_str() == "PATCH").unwrap();
    let mask = patch
        .url
        .query_pairs()
        .find(|(k, _)| k == "updateMask")
        .map(|(_, v)| v.to_string())
        .unwrap();
    assert_eq!(
        mask,
        "description,labels,sourceContents,serviceAccount,callLogLevel,userEnvVars"
    );
}

#[tokio::test]
async fn a_workflow_runway_does_not_own_is_refused() {
    let server = MockServer::start().await;
    mock_service(&server, true).await;
    mock_workflow(&server, Some(live(&rendered(), false))).await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let e = prov.apply(&workflow_step(&r)).await.unwrap_err();
    assert_eq!(e.kind, runway::error::ErrorKind::Conflict);
    assert!(e.message.contains("not created by runway"), "{}", e.message);
}

#[tokio::test]
async fn before_the_service_exists_the_plan_says_the_workflow_waits_for_it() {
    let server = MockServer::start().await;
    mock_service(&server, false).await;
    mock_workflow(&server, None).await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let check = prov.check(&workflow_step(&r), true).await;
    assert_eq!(check.state, StepState::Pending);
    assert!(
        check.detail.contains("after shop-api-prod is created"),
        "{}",
        check.detail
    );
    // Deploy creates services first: one still missing is an error, not a guess.
    let e = prov.apply(&workflow_step(&r)).await.unwrap_err();
    assert!(e.message.contains("has no URL yet"), "{}", e.message);
}

#[tokio::test]
async fn workflows_removed_from_runway_yaml_are_found_and_deleted() {
    let server = MockServer::start().await;
    let old = format!("{PARENT}/workflows/shop-old-prod");
    let mine = json!({"name": old, "labels": {"managed-by": "runway", "runway-app": "shop", "runway-stage": "prod"}});
    Mock::given(method("GET"))
        .and(path(format!("/v1/{PARENT}/workflows")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflows": [
            mine.clone(),
            {"name": WORKFLOW, "labels": {"managed-by": "runway", "runway-app": "shop", "runway-stage": "prod"}},
            {"name": format!("{PARENT}/workflows/someone-else"), "labels": {}}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/{old}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(mine))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/{old}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": format!("{PARENT}/operations/op-d"), "done": true,
            "response": {"@type": "type.googleapis.com/google.protobuf.Empty"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let left = prov.orphan_workflows().await.unwrap();
    assert_eq!(left, vec![Step::Unworkflow { name: old.clone() }]);
    let res = prov.apply(&left[0]).await.unwrap();
    assert_eq!(res.outcome, StepOutcome::Changed);
}

#[tokio::test]
async fn run_workflow_waits_for_the_result() {
    let server = MockServer::start().await;
    let exec = format!("{WORKFLOW}/executions/e-1");
    Mock::given(method("POST"))
        .and(path(format!("/v1/{WORKFLOW}/executions")))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(b["argument"], r#"{"full":true}"#);
            ResponseTemplate::new(200).set_body_json(json!({
                "name": format!("{WORKFLOW}/executions/e-1"), "state": "ACTIVE"
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/{exec}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": exec, "state": "SUCCEEDED", "result": "{\"rows\":3}"
        })))
        .mount(&server)
        .await;
    let executions = google_cloud_workflows_executions_v1::client::Executions::builder()
        .with_endpoint(server.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    let r = runway::commands::run_workflow::start(
        &executions,
        WORKFLOW,
        Some(r#"{"full":true}"#),
        true,
        Duration::from_secs(10),
        &Progress::silent(),
    )
    .await
    .unwrap();
    assert_eq!((r.state, r.execution.as_str()), ("succeeded", "e-1"));
    assert_eq!(r.result.as_deref(), Some("{\"rows\":3}"));
}

#[tokio::test]
async fn the_scheduler_gets_workflows_invoker_on_the_project() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/projects/my-gcp-project:getIamPolicy"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"etag": "BwX1"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/projects/my-gcp-project:setIamPolicy"))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b["policy"].clone())
        })
        .expect(1)
        .mount(&server)
        .await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let steps = schedule_grant_steps(&r);
    assert_eq!(steps.len(), 1);
    let res = prov.apply(&steps[0]).await.unwrap();
    assert_eq!(res.outcome, StepOutcome::Changed);
    assert!(
        prov.granted()
            .iter()
            .any(|g| g.role == "roles/workflows.invoker"),
        "recorded: removing the schedule revokes it"
    );
    let reqs = server.received_requests().await.unwrap();
    let set = reqs
        .iter()
        .find(|r| r.url.path().ends_with(":setIamPolicy"))
        .unwrap();
    let b: Value = serde_json::from_slice(&set.body).unwrap();
    assert_eq!(
        b["policy"]["bindings"][0]["role"],
        "roles/workflows.invoker"
    );
    assert_eq!(
        b["policy"]["bindings"][0]["members"],
        json!(["serviceAccount:shop-prod-sched@my-gcp-project.iam.gserviceaccount.com"])
    );
}

/// Mocks the Workflows regions and, in `us-central1`, a workflow runway
/// created for the stage before runway.yaml moved it (or dropped it),
/// running as `account`.
async fn mock_moved_workflow(server: &MockServer, account: &str) -> String {
    let moved = "projects/my-gcp-project/locations/us-central1/workflows/shop-moved-prod";
    Mock::given(method("GET"))
        .and(path("/v1/projects/my-gcp-project/locations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"locations": [
            {"name": "projects/my-gcp-project/locations/europe-west1", "locationId": "europe-west1"},
            {"name": "projects/my-gcp-project/locations/us-central1", "locationId": "us-central1"}
        ]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/projects/my-gcp-project/locations/us-central1/workflows",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflows": [{
            "name": moved,
            "labels": {"managed-by": "runway", "runway-app": "shop", "runway-stage": "prod", "runway-name": "moved"},
            "serviceAccount": format!("projects/my-gcp-project/serviceAccounts/{account}")
        }]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/{PARENT}/workflows")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"workflows": []})))
        .mount(server)
        .await;
    moved.to_string()
}

#[tokio::test]
async fn a_workflow_left_in_a_region_runway_yaml_no_longer_names_is_found() {
    let server = MockServer::start().await;
    let moved = mock_moved_workflow(&server, ACCOUNT).await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    // runway.yaml only names europe-west1.
    assert_eq!(
        prov.orphan_workflows().await.unwrap(),
        vec![Step::Unworkflow { name: moved }]
    );
}

#[tokio::test]
async fn undeploy_keeps_an_account_a_remaining_workflow_runs_as() {
    use runway::commands::undeploy::{
        Action, Item, keep_accounts_used_by_workflows, remaining_workflows, workflow_users,
    };
    let server = MockServer::start().await;
    let runtime = "shop-runtime@my-gcp-project.iam.gserviceaccount.com";
    let moved = mock_moved_workflow(&server, runtime).await;
    let (_dir, r) = resolved();
    let (session, run) = session_and_run(&server).await;
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let remaining = remaining_workflows(&prov, &[]).await.unwrap();
    // `--only api`: its runtime account was to be deleted with its grant.
    let api = r.deployments.iter().find(|d| d.name() == "api").unwrap();
    let mut plans = vec![(api, true, true)];
    let item = |action, resource: &str, reason: &str| Item {
        action,
        resource: resource.into(),
        reason: reason.into(),
        outcome: None,
    };
    let mut items = vec![
        item(
            Action::Delete,
            &format!("service account {runtime}"),
            "created by runway",
        ),
        item(
            Action::Revoke,
            &format!("grant roles/x on project my-gcp-project to {runtime}"),
            "the service account is deleted",
        ),
    ];
    let mut workflow_accounts = Vec::new();
    let users = |email: &str| workflow_users(remaining.as_deref(), email);
    keep_accounts_used_by_workflows(&mut plans, &mut workflow_accounts, &mut items, &users);
    assert!(!plans[0].2, "the account stays");
    assert_eq!(items[0].action, Action::Keep);
    assert_eq!(items[0].reason, "still used by workflow shop-moved-prod");
    assert_eq!(items[1].action, Action::Keep, "and so do its grants");

    // Once that workflow is deleted too, nothing keeps the account.
    let remaining = remaining_workflows(&prov, &[moved]).await.unwrap();
    assert_eq!(remaining.as_deref().map(<[_]>::len), Some(0));
    let mut plans = vec![(api, true, true)];
    let users = |email: &str| workflow_users(remaining.as_deref(), email);
    keep_accounts_used_by_workflows(&mut plans, &mut workflow_accounts, &mut items, &users);
    assert!(plans[0].2);
}

/// A stage without workflows in runway.yaml (one may have been removed
/// from it and still run as the runtime account).
fn resolved_without_workflows() -> (tempfile::TempDir, config::Resolved) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        r#"
version: 1
app: shop
provider: {project: my-gcp-project, region: europe-west1}
defaults:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop@sha256:0000000000000000000000000000000000000000000000000000000000000000
  service_account: shop-runtime@my-gcp-project.iam.gserviceaccount.com
  identity: {create: true}
services:
  api: {}
stages: { prod: {} }
"#,
    )
    .unwrap();
    let (_, r) = config::load_and_resolve(&p, "prod", &Overrides::default()).unwrap();
    (dir, r)
}

fn denied() -> ResponseTemplate {
    ResponseTemplate::new(403).set_body_json(json!({
        "error": {"code": 403, "message": "workflows.workflows.list denied", "status": "PERMISSION_DENIED"}
    }))
}

#[tokio::test]
async fn undeploy_keeps_accounts_when_workflows_cannot_be_listed_even_without_workflows() {
    use runway::commands::undeploy::{
        Action, Item, keep_accounts_used_by_workflows, remaining_workflows, workflow_users,
    };
    let runtime = "shop-runtime@my-gcp-project.iam.gserviceaccount.com";
    let (_dir, r) = resolved_without_workflows();
    assert!(r.workflows.is_empty());
    // Denied listing the regions, or the workflows of a region.
    let at_regions = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/my-gcp-project/locations"))
        .respond_with(denied())
        .mount(&at_regions)
        .await;
    let in_region = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/my-gcp-project/locations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"locations": [
                {"name": PARENT, "locationId": "europe-west1"}
            ]})),
        )
        .mount(&in_region)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/{PARENT}/workflows")))
        .respond_with(denied())
        .mount(&in_region)
        .await;
    for server in [&at_regions, &in_region] {
        let (session, run) = session_and_run(server).await;
        let prov =
            Provisioner::with_endpoints(r.first(), &session, &run, &endpoints(&server.uri()))
                .await
                .unwrap()
                .with_stack(&r);
        let remaining = remaining_workflows(&prov, &[]).await.unwrap();
        assert!(remaining.is_none(), "unknown, not empty");
        let api = r.deployments.iter().find(|d| d.name() == "api").unwrap();
        let mut plans = vec![(api, true, true)];
        let mut items = vec![Item {
            action: Action::Delete,
            resource: format!("service account {runtime}"),
            reason: "created by runway".into(),
            outcome: None,
        }];
        let users = |email: &str| workflow_users(remaining.as_deref(), email);
        keep_accounts_used_by_workflows(&mut plans, &mut Vec::new(), &mut items, &users);
        assert!(!plans[0].2, "the account is not deleted unverified");
        assert_eq!(items[0].action, Action::Keep);
        assert!(
            items[0].reason.contains("workflows runway cannot list"),
            "{}",
            items[0].reason
        );
    }
}
