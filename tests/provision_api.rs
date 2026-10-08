//! Wire-level tests of provisioning steps: real SDK clients against a mock
//! server. A "fresh" project must receive every mutation; a "converged"
//! project must receive none (idempotency without a state file).

use runway::config::{self, Deployment, Overrides};
use runway::gcp::Session;
use runway::output::Progress;
use runway::provision::{Endpoints, Provisioner, StepOutcome, StepState, post_steps, pre_steps};
use runway::retry::{RetryConfig, with_retry};
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SA: &str = "gcptree-run@my-gcp-project.iam.gserviceaccount.com";
const SVC: &str = "projects/my-gcp-project/locations/europe-west1/services/gcptree-prod";
const IAP_RES: &str = "projects/123456/iap_web/cloud_run-europe-west1/services/gcptree-prod";
const AGENT: &str = "serviceAccount:service-123456@gcp-sa-iap.iam.gserviceaccount.com";

fn deployment() -> Deployment {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        format!(
            r#"
version: 1
app: gcptree
provider: {{project: my-gcp-project, region: europe-west1}}
service:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/gcptree:1
  service_account: {SA}
  identity:
    create: true
    display_name: gcptree runtime
    roles:
      - role: roles/bigquery.dataViewer
        dataset: billing-data-1234.billingdata
      - role: roles/bigquery.jobUser
        project: billing-data-1234
      - role: roles/secretmanager.secretAccessor
        secret: api-key
  tags:
    "123456789012/allow-public-access": "true"
  iap:
    members: [group:finops@example.com]
stages: {{prod: {{}}}}
"#
        ),
    )
    .unwrap();
    config::load_and_resolve(&p, "prod", &Overrides::default())
        .unwrap()
        .1
        .deployments[0]
        .clone()
}

fn endpoints(uri: &str) -> Endpoints {
    Endpoints {
        iam: Some(uri.into()),
        resource_manager: Some(uri.into()),
        tag_bindings: Some(uri.into()),
        bigquery: Some(uri.into()),
        iap: Some(uri.into()),
        secret_manager: Some(uri.into()),
        service_usage: Some(uri.into()),
        artifact_registry: Some(uri.into()),
        run: Some(uri.into()),
        scheduler: Some(uri.into()),
    }
}

/// Mocks reads; `converged` decides whether everything is already in place.
async fn mock_project(converged: bool) -> MockServer {
    let s = MockServer::start().await;
    let json200 = |v: Value| ResponseTemplate::new(200).set_body_json(v);
    let not_found = ResponseTemplate::new(404).set_body_json(
        json!({"error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}}),
    );

    // Service account.
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/my-gcp-project/serviceAccounts/{SA}")))
        .respond_with(if converged {
            json200(json!({"email": SA, "name": format!("projects/my-gcp-project/serviceAccounts/{SA}")}))
        } else {
            not_found.clone()
        })
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/my-gcp-project/serviceAccounts"))
        .respond_with(json200(json!({"email": SA})))
        .mount(&s)
        .await;

    // Project number.
    Mock::given(method("GET"))
        .and(path("/v3/projects/my-gcp-project"))
        .respond_with(json200(
            json!({"name": "projects/123456", "projectId": "my-gcp-project"}),
        ))
        .mount(&s)
        .await;

    // IAM policies (project, secret, Cloud Run service, IAP resource).
    let member = format!("serviceAccount:{SA}");
    let policy = |role: &str, m: &str| {
        if converged {
            json!({"version": 1, "etag": "BwX1", "bindings": [{"role": role, "members": [m]}]})
        } else {
            json!({"version": 1, "etag": "BwX1", "bindings": [{"role": "roles/viewer", "members": ["user:keep@example.com"]}]})
        }
    };
    for (res, role, m) in [
        (
            "/v3/projects/billing-data-1234",
            "roles/bigquery.jobUser",
            member.as_str(),
        ),
        (
            "/v1/projects/my-gcp-project/secrets/api-key",
            "roles/secretmanager.secretAccessor",
            member.as_str(),
        ),
        (&format!("/v2/{SVC}") as &str, "roles/run.invoker", AGENT),
        (
            &format!("/v1/{IAP_RES}") as &str,
            "roles/iap.httpsResourceAccessor",
            "group:finops@example.com",
        ),
    ] {
        Mock::given(path(format!("{res}:getIamPolicy")))
            .respond_with(json200(policy(role, m)))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{res}:setIamPolicy")))
            .respond_with(|r: &Request| {
                let b: Value = serde_json::from_slice(&r.body).unwrap();
                ResponseTemplate::new(200).set_body_json(b["policy"].clone())
            })
            .mount(&s)
            .await;
    }

    // BigQuery dataset access list.
    let mut access = vec![json!({"role": "OWNER", "userByEmail": "owner@example.com"})];
    if converged {
        access.push(json!({"role": "READER", "userByEmail": SA}));
    }
    Mock::given(method("GET"))
        .and(path(
            "/bigquery/v2/projects/billing-data-1234/datasets/billingdata",
        ))
        .respond_with(json200(
            json!({"id": "billing-data-1234:billingdata", "access": access}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("PATCH"))
        .and(path(
            "/bigquery/v2/projects/billing-data-1234/datasets/billingdata",
        ))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({"access": b["access"]}))
        })
        .mount(&s)
        .await;

    // Tags.
    Mock::given(method("GET"))
        .and(path("/v3/tagValues/namespaced"))
        .and(query_param("name", "123456789012/allow-public-access/true"))
        .respond_with(json200(
            json!({"name": "tagValues/777", "shortName": "true"}),
        ))
        .mount(&s)
        .await;
    // Effective tags on the service: absent until bound (fresh), present (converged).
    let effective = json!({"effectiveTags": [
        {"namespacedTagValue": "123456789012/allow-public-access/true", "tagValue": "tagValues/777"}
    ]});
    if !converged {
        Mock::given(method("GET"))
            .and(path("/v3/effectiveTags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/v3/effectiveTags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(effective))
        .mount(&s)
        .await;
    let bindings = if converged {
        json!({"tagBindings": [{"parent": "x", "tagValue": "tagValues/777"}]})
    } else {
        json!({})
    };
    Mock::given(method("GET"))
        .and(path("/v3/tagBindings"))
        .respond_with(json200(bindings))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/tagBindings"))
        .respond_with(json200(json!({
            "name": "operations/tb1",
            "done": true,
            "response": {"@type": "type.googleapis.com/google.cloud.resourcemanager.v3.TagBinding", "tagValue": "tagValues/777"}
        })))
        .mount(&s)
        .await;

    // IAP service agent.
    Mock::given(method("POST"))
        .and(path(
            "/v1beta1/projects/my-gcp-project/services/iap.googleapis.com:generateServiceIdentity",
        ))
        .respond_with(json200(json!({"name": "operations/si", "done": true})))
        .mount(&s)
        .await;
    s
}

async fn run_all(server: &MockServer, d: &Deployment) -> Vec<(String, StepOutcome)> {
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = Provisioner::with_endpoints(d, &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap();
    let mut out = Vec::new();
    for step in pre_steps(d).iter().chain(post_steps(d).iter()) {
        let r = prov
            .apply(step)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:?}", step.describe(d)));
        out.push((r.step, r.outcome));
    }
    out
}

fn mutations(reqs: &[Request]) -> Vec<String> {
    reqs.iter()
        .filter(|r| {
            let p = r.url.path();
            (r.method.as_str() == "POST" && !p.ends_with(":getIamPolicy"))
                || r.method.as_str() == "PATCH"
        })
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

#[tokio::test]
async fn fresh_project_gets_every_step() {
    let d = deployment();
    let server = mock_project(false).await;
    let results = run_all(&server, &d).await;
    assert_eq!(results.len(), 7, "{results:?}");
    assert!(
        results.iter().all(|(_, o)| *o == StepOutcome::Changed),
        "{results:?}"
    );

    let reqs = server.received_requests().await.unwrap();
    let muts = mutations(&reqs);
    let has = |m: &str| muts.iter().any(|x| x == m);
    assert!(
        has("POST /v1/projects/my-gcp-project/serviceAccounts"),
        "{muts:?}"
    );
    assert!(has(
        "PATCH /bigquery/v2/projects/billing-data-1234/datasets/billingdata"
    ));
    assert!(has("POST /v3/projects/billing-data-1234:setIamPolicy"));
    assert!(has(
        "POST /v1/projects/my-gcp-project/secrets/api-key:setIamPolicy"
    ));
    assert!(has("POST /v3/tagBindings"));
    assert!(has(&format!("POST /v2/{SVC}:setIamPolicy")));
    assert!(has(&format!("POST /v1/{IAP_RES}:setIamPolicy")));

    let body = |p: &str| -> Value {
        let r = reqs
            .iter()
            .find(|r| r.url.path() == p && r.method.as_str() != "GET")
            .unwrap();
        serde_json::from_slice(&r.body).unwrap()
    };
    let sa = body("/v1/projects/my-gcp-project/serviceAccounts");
    assert_eq!(sa["accountId"], "gcptree-run");
    assert_eq!(sa["serviceAccount"]["displayName"], "gcptree runtime");
    assert_eq!(
        sa["serviceAccount"]["description"],
        "managed-by=runway app=gcptree stage=prod role=runtime",
        "ownership marker used by undeploy"
    );

    // Existing bindings are preserved, the etag is sent back.
    let proj = body("/v3/projects/billing-data-1234:setIamPolicy");
    assert_eq!(proj["policy"]["etag"], "BwX1");
    let bindings = proj["policy"]["bindings"].as_array().unwrap();
    assert!(
        bindings.contains(&json!({"role": "roles/viewer", "members": ["user:keep@example.com"]}))
    );
    assert!(bindings.contains(
        &json!({"role": "roles/bigquery.jobUser", "members": [format!("serviceAccount:{SA}")]})
    ));

    let ds = body("/bigquery/v2/projects/billing-data-1234/datasets/billingdata");
    let access = ds["access"].as_array().unwrap();
    assert_eq!(access.len(), 2, "existing entries kept");
    assert!(access.contains(&json!({"role": "roles/bigquery.dataViewer", "userByEmail": SA})));

    let tag = body("/v3/tagBindings");
    assert_eq!(
        tag["parent"],
        "//run.googleapis.com/projects/my-gcp-project/locations/europe-west1/services/gcptree-prod"
    );
    assert_eq!(tag["tagValue"], "tagValues/777");

    let run_policy = body(&format!("/v2/{SVC}:setIamPolicy"));
    assert!(
        run_policy["policy"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["role"] == "roles/run.invoker"
                && b["members"].as_array().unwrap().contains(&json!(AGENT)))
    );
    let iap = body(&format!("/v1/{IAP_RES}:setIamPolicy"));
    assert!(
        iap["policy"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["role"] == "roles/iap.httpsResourceAccessor"
                && b["members"] == json!(["group:finops@example.com"]))
    );
}

#[tokio::test]
async fn converged_project_receives_no_writes() {
    let d = deployment();
    let server = mock_project(true).await;
    let results = run_all(&server, &d).await;
    assert!(
        results.iter().all(|(_, o)| *o == StepOutcome::Unchanged),
        "{results:?}"
    );
    let reqs = server.received_requests().await.unwrap();
    let muts: Vec<String> = mutations(&reqs)
        .into_iter()
        // Ensuring the IAP agent exists is an idempotent call made on every run.
        .filter(|m| !m.contains("generateServiceIdentity"))
        .collect();
    assert!(
        muts.is_empty(),
        "unexpected writes on a converged project: {muts:?}"
    );
}

#[tokio::test]
async fn plan_checks_report_state_without_writing() {
    let d = deployment();
    for (converged, want) in [(false, StepState::Pending), (true, StepState::InSync)] {
        let server = mock_project(converged).await;
        let session = Session::from_static_token("t").unwrap();
        let run = google_cloud_run_v2::client::Services::builder()
            .with_endpoint(server.uri())
            .with_credentials(session.credentials.clone())
            .build()
            .await
            .unwrap();
        let prov = Provisioner::with_endpoints(&d, &session, &run, &endpoints(&server.uri()))
            .await
            .unwrap();
        for step in pre_steps(&d).iter().chain(post_steps(&d).iter()) {
            let c = prov.check(step, true).await;
            assert_eq!(c.state, want, "{}: {}", c.step, c.detail);
        }
        assert!(mutations(&server.received_requests().await.unwrap()).is_empty());
    }
}

#[tokio::test]
async fn new_service_account_propagation_is_retried() {
    // Right after creation, IAM may reject the new member; the step succeeds on retry.
    let d = deployment();
    let server = mock_project(false).await;
    Mock::given(method("POST"))
        .and(path("/v3/projects/billing-data-1234:setIamPolicy"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "status": "INVALID_ARGUMENT",
            "message": format!("Service account {SA} does not exist.")
        }})))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    let session = Session::from_static_token("t").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = Provisioner::with_endpoints(&d, &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap();
    let step = pre_steps(&d)
        .into_iter()
        .find(|s| s.describe(&d).contains("bigquery.jobUser"))
        .unwrap();
    let retry = RetryConfig {
        attempts: 3,
        delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(10),
    };
    let r = with_retry(&retry, &Progress::silent(), "grant", |_| prov.apply(&step))
        .await
        .unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed);

    // With retries disabled the same failure surfaces immediately.
    let server2 = mock_project(false).await;
    Mock::given(method("POST"))
        .and(path("/v3/projects/billing-data-1234:setIamPolicy"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "status": "INVALID_ARGUMENT", "message": "Service account does not exist."
        }})))
        .with_priority(1)
        .mount(&server2)
        .await;
    let run2 = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server2.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov2 = Provisioner::with_endpoints(&d, &session, &run2, &endpoints(&server2.uri()))
        .await
        .unwrap();
    let once = RetryConfig {
        attempts: 1,
        ..retry
    };
    assert!(
        with_retry(&once, &Progress::silent(), "grant", |_| prov2.apply(&step))
            .await
            .is_err()
    );
}

fn build_deployment(dir: &std::path::Path) -> Deployment {
    std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
    let p = dir.join("runway.yaml");
    std::fs::write(
        &p,
        r#"
version: 1
app: gcptree
provider:
  project: my-gcp-project
  region: europe-west1
  enable_apis: true
  apis: [telemetry.googleapis.com]
  create_build_resources: true
  artifact_repository: runway
  source_bucket: "${project}-runway-sources"
  build_service_account: "runway-build@${project}.iam.gserviceaccount.com"
service:
  source: .
  service_account: "gcptree-run@${project}.iam.gserviceaccount.com"
stages: {prod: {}}
"#,
    )
    .unwrap();
    config::load_and_resolve(&p, "prod", &Overrides::default())
        .unwrap()
        .1
        .deployments[0]
        .clone()
}

#[tokio::test]
async fn enables_only_missing_apis_and_creates_the_repository_once() {
    use runway::provision::Step;
    let dir = tempfile::tempdir().unwrap();
    let d = build_deployment(dir.path());
    let server = MockServer::start().await;
    // Everything enabled except Cloud Build and telemetry.
    Mock::given(method("GET"))
        .and(path("/v1/projects/my-gcp-project/services:batchGet"))
        .respond_with(|r: &Request| {
            let services: Vec<Value> = r
                .url
                .query_pairs()
                .filter(|(k, _)| k == "names")
                .map(|(_, v)| {
                    let disabled = v.ends_with("cloudbuild.googleapis.com")
                        || v.ends_with("telemetry.googleapis.com");
                    json!({"name": v, "state": if disabled { "DISABLED" } else { "ENABLED" }})
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "services": services }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/my-gcp-project/services:batchEnable"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "operations/enable", "done": true,
            "response": {"@type": "type.googleapis.com/google.api.serviceusage.v1.BatchEnableServicesResponse"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/projects/my-gcp-project/locations/europe-west1/repositories/runway",
        ))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(
                json!({"error": {"code": 404, "status": "NOT_FOUND", "message": "no"}}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/my-gcp-project/locations/europe-west1/repositories"))
        .and(query_param("repositoryId", "runway"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "operations/repo", "done": true,
            "response": {"@type": "type.googleapis.com/google.devtools.artifactregistry.v1.Repository", "format": "DOCKER"}})))
        .expect(1)
        .mount(&server)
        .await;

    let session = Session::from_static_token("t").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = Provisioner::with_endpoints(&d, &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap();
    let steps = runway::provision::all_steps(&d);
    let apis = steps
        .iter()
        .find(|s| matches!(s, Step::EnableApis(_)))
        .unwrap();
    let repo = steps
        .iter()
        .find(|s| matches!(s, Step::CreateRepository { .. }))
        .unwrap();

    let c = prov.check(apis, false).await;
    assert_eq!(c.state, StepState::Pending);
    assert!(
        c.detail.contains("cloudbuild.googleapis.com")
            && c.detail.contains("telemetry.googleapis.com"),
        "{}",
        c.detail
    );
    let r = prov.apply(apis).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed);
    let r = prov.apply(repo).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed);

    let reqs = server.received_requests().await.unwrap();
    let enable: Value = serde_json::from_slice(
        &reqs
            .iter()
            .find(|r| r.url.path().ends_with(":batchEnable"))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(
        enable["serviceIds"],
        json!(["cloudbuild.googleapis.com", "telemetry.googleapis.com"])
    );
    let create = reqs
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/repositories"))
        .unwrap();
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(body["labels"]["managed-by"], "runway");
    assert!(
        body["format"] == json!("DOCKER") || body["format"] == json!(1),
        "{body}"
    );
}

#[tokio::test]
async fn existing_apis_and_repository_are_left_alone() {
    use runway::provision::Step;
    let dir = tempfile::tempdir().unwrap();
    let d = build_deployment(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/my-gcp-project/services:batchGet"))
        .respond_with(|r: &Request| {
            let services: Vec<Value> = r
                .url
                .query_pairs()
                .filter(|(k, _)| k == "names")
                .map(|(_, v)| json!({"name": v, "state": "ENABLED"}))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "services": services }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/projects/my-gcp-project/locations/europe-west1/repositories/runway",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "x", "format": "DOCKER"})),
        )
        .mount(&server)
        .await;
    let session = Session::from_static_token("t").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = Provisioner::with_endpoints(&d, &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap();
    for s in runway::provision::all_steps(&d)
        .iter()
        .filter(|s| matches!(s, Step::EnableApis(_) | Step::CreateRepository { .. }))
    {
        assert_eq!(prov.check(s, false).await.state, StepState::InSync);
        assert_eq!(prov.apply(s).await.unwrap().outcome, StepOutcome::Unchanged);
    }
    assert!(mutations(&server.received_requests().await.unwrap()).is_empty());
}

fn tagged_deployment(dir: &std::path::Path) -> Deployment {
    let p = dir.join("runway.yaml");
    std::fs::write(
        &p,
        r#"
version: 1
app: gcptree
provider:
  project: my-gcp-project
  region: europe-west1
  tags:
    "210987654321/allowIngressAllForCloudRun": allow-ingress-all
buckets:
  cache: { name: "${project}-gcptree-cache" }
service:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/gcptree:1
  service_account: "gcptree-run@${project}.iam.gserviceaccount.com"
stages: {prod: {}}
"#,
    )
    .unwrap();
    config::load_and_resolve(&p, "prod", &Overrides::default())
        .unwrap()
        .1
        .deployments[0]
        .clone()
}

#[tokio::test]
async fn project_tag_is_bound_first_and_awaited_until_effective() {
    use runway::provision::Step;
    let dir = tempfile::tempdir().unwrap();
    let d = tagged_deployment(dir.path());
    let steps = runway::provision::pre_steps(&d);
    assert!(
        matches!(steps[0], Step::ProjectTag { .. }),
        "project tags come before buckets"
    );

    let server = MockServer::start().await;
    let effective = json!({"effectiveTags": [
        {"namespacedTagValue": "210987654321/env/sdx", "inherited": true},
        {"namespacedTagValue": "210987654321/allowIngressAllForCloudRun/allow-ingress-all", "tagValue": "tagValues/281470000000001"}
    ]});
    let not_yet = json!({"effectiveTags": [{"namespacedTagValue": "210987654321/env/sdx", "inherited": true}]});
    // Not effective before the binding, and once more right after it (propagation).
    Mock::given(method("GET"))
        .and(path("/v3/effectiveTags"))
        .and(query_param(
            "parent",
            "//cloudresourcemanager.googleapis.com/projects/123456",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(not_yet))
        .up_to_n_times(3)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/effectiveTags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(effective))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/projects/my-gcp-project"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "projects/123456"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/tagValues/namespaced"))
        .and(query_param(
            "name",
            "210987654321/allowIngressAllForCloudRun/allow-ingress-all",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "tagValues/281470000000001"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/tagBindings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "operations/tb", "done": true,
            "response": {"@type": "type.googleapis.com/google.cloud.resourcemanager.v3.TagBinding"}})))
        .expect(1)
        .mount(&server)
        .await;

    let session = Session::from_static_token("t").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = Provisioner::with_endpoints(&d, &session, &run, &endpoints(&server.uri()))
        .await
        .unwrap();
    assert_eq!(prov.check(&steps[0], false).await.state, StepState::Pending);
    let r = prov.apply(&steps[0]).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed);
    assert!(r.detail.contains("effective"), "{}", r.detail);

    let reqs = server.received_requests().await.unwrap();
    let post = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(
        body["parent"],
        "//cloudresourcemanager.googleapis.com/projects/123456"
    );
    assert_eq!(body["tagValue"], "tagValues/281470000000001");
    let polls_after_binding = reqs
        .iter()
        .skip_while(|r| r.method.as_str() != "POST")
        .filter(|r| r.url.path() == "/v3/effectiveTags")
        .count();
    assert!(
        polls_after_binding >= 2,
        "waited until the tag was effective ({polls_after_binding} polls)"
    );

    // Converged: effective already (bound here or inherited), nothing is written.
    assert_eq!(prov.check(&steps[0], false).await.state, StepState::InSync);
    assert_eq!(
        prov.apply(&steps[0]).await.unwrap().outcome,
        StepOutcome::Unchanged
    );
}

/// Mocks for revocations: the IAP policy and a project policy, each with a
/// configured entry, a recorded one removed from the configuration, and (IAP)
/// one granted by hand. `owned`: the runtime account carries runway's marker.
async fn revocation_project(owned: bool) -> MockServer {
    let s = MockServer::start().await;
    let json200 = |v: Value| ResponseTemplate::new(200).set_body_json(v);
    Mock::given(method("GET"))
        .and(path("/v3/projects/my-gcp-project"))
        .respond_with(json200(
            json!({"name": "projects/123456", "projectId": "my-gcp-project"}),
        ))
        .mount(&s)
        .await;
    let description = if owned {
        "managed-by=runway app=gcptree stage=prod role=runtime"
    } else {
        "created by hand"
    };
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/my-gcp-project/serviceAccounts/{SA}"
        )))
        .respond_with(json200(json!({"email": SA, "description": description})))
        .mount(&s)
        .await;
    let sa = format!("serviceAccount:{SA}");
    for (res, bindings) in [
        (
            format!("/v1/{IAP_RES}"),
            json!([{"role": "roles/iap.httpsResourceAccessor",
                    "members": ["group:finops@example.com", "group:old@example.com", "group:manual@example.com"]}]),
        ),
        (
            "/v3/projects/billing-data-1234".to_string(),
            json!([{"role": "roles/bigquery.jobUser", "members": [sa]},
                   {"role": "roles/bigquery.dataEditor", "members": [sa]}]),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("{res}:getIamPolicy")))
            .respond_with(json200(
                json!({"version": 1, "etag": "BwX1", "bindings": bindings}),
            ))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{res}:setIamPolicy")))
            .respond_with(json200(json!({"version": 1, "etag": "BwX2"})))
            .mount(&s)
            .await;
    }
    s
}

fn removed_from_config() -> Vec<runway::provision::ManagedGrant> {
    use runway::config::RoleTarget;
    use runway::provision::ManagedGrant;
    vec![
        ManagedGrant {
            member: "group:old@example.com".into(),
            role: "roles/iap.httpsResourceAccessor".into(),
            target: None,
            runtime: false,
            service: None,
        },
        ManagedGrant {
            member: format!("serviceAccount:{SA}"),
            role: "roles/bigquery.dataEditor".into(),
            target: Some(RoleTarget::Project {
                project: "billing-data-1234".into(),
            }),
            runtime: true,
            service: None,
        },
    ]
}

async fn provisioner_for<'a>(
    server: &MockServer,
    d: &'a Deployment,
    session: &'a Session,
    run: &'a google_cloud_run_v2::client::Services,
) -> Provisioner<'a> {
    Provisioner::with_endpoints(d, session, run, &endpoints(&server.uri()))
        .await
        .unwrap()
}

fn set_policy_bodies(reqs: &[Request], res: &str) -> Vec<Value> {
    reqs.iter()
        .filter(|r| r.url.path() == format!("{res}:setIamPolicy"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn removed_grants_are_revoked_and_plans_show_only_what_changes() {
    use runway::provision::{managed_grants, revoke_steps};
    let mut d = deployment();
    d.service.iap.members.push("group:new@example.com".into());
    let server = revocation_project(true).await;
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;

    let mut recorded = managed_grants(&d);
    recorded.extend(removed_from_config());
    let revokes = revoke_steps(&recorded, &d);
    assert_eq!(revokes.len(), 2, "only what left the configuration");

    // Plan: the new member alone, then the two removals.
    let iap = prov.check(&runway::provision::Step::IapAccess, true).await;
    assert_eq!(iap.state, StepState::Pending);
    assert_eq!(iap.detail, "grant to group:new@example.com");
    for step in &revokes {
        let c = prov.check(step, true).await;
        assert_eq!(c.state, StepState::PendingRemoval, "{c:?}");
    }

    // Deploy: both revoked, nothing else touched.
    let retry = RetryConfig {
        attempts: 1,
        ..Default::default()
    };
    let done = prov
        .apply_all(&revokes, &retry, &Progress::silent(), &|_| {})
        .await
        .unwrap();
    assert!(
        done.iter().all(|r| r.outcome == StepOutcome::Changed),
        "{done:?}"
    );
    let reqs = server.received_requests().await.unwrap();
    let iap_set = set_policy_bodies(&reqs, &format!("/v1/{IAP_RES}"));
    assert_eq!(
        iap_set[0]["policy"]["bindings"][0]["members"],
        json!(["group:finops@example.com", "group:manual@example.com"]),
        "the member granted by hand stays"
    );
    let project_set = set_policy_bodies(&reqs, "/v3/projects/billing-data-1234");
    assert_eq!(
        project_set[0]["policy"]["bindings"],
        json!([{"role": "roles/bigquery.jobUser", "members": [format!("serviceAccount:{SA}")]}])
    );
}

#[tokio::test]
async fn roles_of_an_account_runway_did_not_create_are_kept() {
    use runway::provision::{managed_grants, revoke_steps};
    let d = deployment();
    let server = revocation_project(false).await;
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let mut recorded = managed_grants(&d);
    recorded.extend(removed_from_config());
    let revokes = revoke_steps(&recorded, &d);
    let role = revokes
        .iter()
        .find(|s| s.describe(&d).contains("dataEditor"))
        .unwrap();
    let check = prov.check(role, true).await;
    assert_eq!(check.state, StepState::InSync);
    assert!(check.detail.starts_with("kept: "), "{}", check.detail);
    let r = prov.apply(role).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Unchanged);
    let reqs = server.received_requests().await.unwrap();
    assert!(
        set_policy_bodies(&reqs, "/v3/projects/billing-data-1234").is_empty(),
        "a shared or user-provided account keeps its roles"
    );
}

#[tokio::test]
async fn members_already_present_are_not_recorded_as_runways() {
    use runway::provision::{Step, grant_record, managed_grants};
    // finops already has access (perhaps granted by hand); new does not.
    let mut d = deployment();
    d.service.iap.members.push("group:new@example.com".into());
    let server = revocation_project(true).await;
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let r = prov.apply(&Step::IapAccess).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed);
    assert_eq!(r.detail, "granted to group:new@example.com");
    let added: Vec<String> = prov.granted().into_iter().map(|g| g.member).collect();
    assert_eq!(added, ["group:new@example.com"]);

    let record = grant_record(&[], &managed_grants(&d), &prov.granted(), true);
    let members: Vec<&str> = record.iter().map(|g| g.member.as_str()).collect();
    assert!(members.contains(&"group:new@example.com"));
    assert!(
        !members.contains(&"group:finops@example.com"),
        "removing finops from runway.yaml later must not revoke access runway did not grant"
    );
}

#[tokio::test]
async fn recorded_iap_members_are_revoked_after_iap_is_disabled() {
    use runway::provision::{Step, managed_grants, revoke_steps};
    let enabled = deployment();
    let recorded = managed_grants(&enabled);
    let mut d = enabled.clone();
    d.service.iap.enabled = false;
    d.service.iap.members.clear();
    let server = revocation_project(true).await;
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let revokes = revoke_steps(&recorded, &d);
    let iap = revokes
        .iter()
        .find(|s| matches!(s, Step::Revoke(g) if g.target.is_none()))
        .expect("the IAP member is revoked");
    let r = prov.apply(iap).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed, "{r:?}");
}

/// IAP policy reads answer without `group:new` for the first `before` reads,
/// then with it; every policy write answers 503 (its outcome is unknown).
async fn lost_answer_project(before: u64) -> MockServer {
    let s = MockServer::start().await;
    let json200 = |v: Value| ResponseTemplate::new(200).set_body_json(v);
    Mock::given(method("GET"))
        .and(path("/v3/projects/my-gcp-project"))
        .respond_with(json200(
            json!({"name": "projects/123456", "projectId": "my-gcp-project"}),
        ))
        .mount(&s)
        .await;
    let policy = |members: Vec<&str>| {
        json!({"version": 1, "etag": "BwX1",
               "bindings": [{"role": "roles/iap.httpsResourceAccessor", "members": members}]})
    };
    Mock::given(method("POST"))
        .and(path(format!("/v1/{IAP_RES}:getIamPolicy")))
        .respond_with(json200(policy(vec!["group:finops@example.com"])))
        .up_to_n_times(before)
        .with_priority(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/{IAP_RES}:getIamPolicy")))
        .respond_with(json200(policy(vec![
            "group:finops@example.com",
            "group:new@example.com",
        ])))
        .with_priority(2)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/{IAP_RES}:setIamPolicy")))
        .respond_with(ResponseTemplate::new(503).set_body_json(
            json!({"error": {"code": 503, "message": "deadline", "status": "UNAVAILABLE"}}),
        ))
        .mount(&s)
        .await;
    s
}

async fn iap_provisioner_run(
    server: &MockServer,
) -> (Deployment, Session, google_cloud_run_v2::client::Services) {
    let mut d = deployment();
    d.service.iap.members.push("group:new@example.com".into());
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    (d, session, run)
}

#[tokio::test]
async fn a_write_whose_answer_was_lost_but_committed_is_runways() {
    use runway::provision::Step;
    // The write answers 503, yet the next read shows the member: it committed.
    let server = lost_answer_project(1).await;
    let (d, session, run) = iap_provisioner_run(&server).await;
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let r = prov.apply(&Step::IapAccess).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed, "{r:?}");
    let added: Vec<String> = prov.granted().into_iter().map(|g| g.member).collect();
    assert_eq!(added, ["group:new@example.com"]);
}

#[tokio::test]
async fn a_lost_final_answer_is_confirmed_before_the_step_fails() {
    use runway::provision::{Step, grant_record, managed_grants};
    // One step attempt (`--retries 0`), a fresh provisioner (a new CLI run):
    // the four writes answer 503 and the last one committed. Without a
    // confirming read, the deploy would exit and the next run would take the
    // member for pre-existing, never revoking it.
    let server = lost_answer_project(4).await;
    let (d, session, run) = iap_provisioner_run(&server).await;
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let retry = RetryConfig {
        attempts: 1,
        ..Default::default()
    };
    let done = prov
        .apply_all(&[Step::IapAccess], &retry, &Progress::silent(), &|_| {})
        .await
        .unwrap();
    assert_eq!(done[0].outcome, StepOutcome::Changed, "the write committed");
    let record = grant_record(&[], &managed_grants(&d), &prov.granted(), true);
    let members: Vec<&str> = record.iter().map(|g| g.member.as_str()).collect();
    assert_eq!(members, ["group:new@example.com"]);
}

#[tokio::test]
async fn a_write_that_did_not_commit_is_not_claimed() {
    use runway::provision::Step;
    // Every write answers 503 and none committed: the step fails and nothing
    // is claimed as runway's.
    let server = lost_answer_project(100).await;
    let (d, session, run) = iap_provisioner_run(&server).await;
    let prov = provisioner_for(&server, &d, &session, &run).await;
    assert!(prov.apply(&Step::IapAccess).await.is_err());
    assert!(prov.granted().is_empty());
}

#[tokio::test]
async fn a_lost_dataset_patch_that_committed_is_runways() {
    use runway::provision::{Step, pre_steps};
    let server = MockServer::start().await;
    let ds = "/bigquery/v2/projects/billing-data-1234/datasets/billingdata";
    let access = |with_sa: bool| {
        let mut a = vec![json!({"role": "READER", "userByEmail": "keep@example.com"})];
        if with_sa {
            a.push(json!({"role": "roles/bigquery.dataViewer", "userByEmail": SA}));
        }
        json!({"access": a})
    };
    Mock::given(method("GET"))
        .and(path(ds))
        .respond_with(ResponseTemplate::new(200).set_body_json(access(false)))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(ds))
        .respond_with(ResponseTemplate::new(200).set_body_json(access(true)))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(ds))
        .respond_with(ResponseTemplate::new(503).set_body_json(
            json!({"error": {"code": 503, "message": "deadline", "status": "UNAVAILABLE"}}),
        ))
        .mount(&server)
        .await;
    let d = deployment();
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let step = pre_steps(&d)
        .into_iter()
        .find(|s| matches!(s, Step::Grant { binding, .. } if binding.role == "roles/bigquery.dataViewer"))
        .unwrap();
    let r = prov.apply(&step).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed, "{r:?}");
    let granted = prov.granted();
    assert_eq!(granted.len(), 1);
    assert_eq!(granted[0].role, "roles/bigquery.dataViewer");
    assert!(granted[0].runtime);
}

#[tokio::test]
async fn grants_made_before_a_failure_are_recorded() {
    use runway::config::RoleTarget;
    use runway::deploy::Reconciler;
    use runway::poll::PollConfig;
    use runway::provision::{ManagedGrant, Step, grant_record, managed_grants, recorded_grants};
    let server = revocation_project(true).await;
    // A later step fails: revoking on a project whose policy cannot be read.
    Mock::given(method("POST"))
        .and(path("/v3/projects/locked-project:getIamPolicy"))
        .respond_with(ResponseTemplate::new(403).set_body_json(
            json!({"error": {"code": 403, "message": "denied", "status": "PERMISSION_DENIED"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{SVC}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": SVC, "etag": "\"e1\"",
            "labels": {"managed-by": "runway", "runway-app": "gcptree", "runway-stage": "prod"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(format!("/v2/{SVC}")))
        .and(query_param("updateMask", "annotations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "op"})))
        .expect(1)
        .mount(&server)
        .await;

    let mut d = deployment();
    d.service.iap.members.push("group:new@example.com".into());
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let steps = [
        Step::IapAccess,
        Step::Revoke(ManagedGrant {
            member: "group:x@example.com".into(),
            role: "roles/viewer".into(),
            target: Some(RoleTarget::Project {
                project: "locked-project".into(),
            }),
            runtime: false,
            service: None,
        }),
    ];
    let retry = RetryConfig {
        attempts: 1,
        ..Default::default()
    };
    assert!(
        prov.apply_all(&steps, &retry, &Progress::silent(), &|_| {})
            .await
            .is_err()
    );
    let record = grant_record(&[], &managed_grants(&d), &prov.granted(), true);
    let members: Vec<&str> = record.iter().map(|g| g.member.as_str()).collect();
    assert_eq!(
        members,
        ["group:new@example.com"],
        "granted before the failure"
    );

    let progress = Progress::silent();
    let rec = Reconciler {
        run: &run,
        revisions: None,
        progress: &progress,
        poll: PollConfig::fast(),
        timeout: Duration::from_secs(5),
    };
    let live = rec.get(SVC).await.unwrap();
    assert!(
        rec.save_grant_record(SVC, live.as_ref(), &record)
            .await
            .unwrap()
    );
    let reqs = server.received_requests().await.unwrap();
    let patch = reqs.iter().find(|r| r.method.as_str() == "PATCH").unwrap();
    let body: Value = serde_json::from_slice(&patch.body).unwrap();
    let saved = recorded_grants(
        &body["annotations"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect(),
    );
    assert_eq!(saved, record);
}

fn authoritative_deployment() -> Deployment {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        format!(
            r#"
version: 1
app: gcptree
provider: {{project: my-gcp-project, region: europe-west1}}
secrets:
  apikey:
    name: gcptree-prod-api-key
    adders: [group:devops@example.com]
service:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/gcptree:1
  service_account: {SA}
  identity:
    create: true
    roles:
      - role: roles/bigquery.dataViewer
        dataset: billing-data-1234.billingdata
      - role: roles/bigquery.jobUser
        project: billing-data-1234
  env:
    API_KEY: "${{secrets.apikey}}"
  tags:
    "123456789012/allow-public-access": "true"
  iap:
    members: [group:finops@example.com]
stages: {{prod: {{}}}}
"#
        ),
    )
    .unwrap();
    config::load_and_resolve(&p, "prod", &Overrides::default())
        .unwrap()
        .1
        .deployments[0]
        .clone()
}

/// Live state with access runway.yaml does not list, in each scope; the
/// runtime account and the secret are runway's when `owned`.
async fn unlisted_project(owned: bool) -> MockServer {
    let s = MockServer::start().await;
    let json200 = |v: Value| ResponseTemplate::new(200).set_body_json(v);
    let sa = format!("serviceAccount:{SA}");
    Mock::given(method("GET"))
        .and(path("/v3/projects/my-gcp-project"))
        .respond_with(json200(
            json!({"name": "projects/123456", "projectId": "my-gcp-project"}),
        ))
        .mount(&s)
        .await;
    let (description, labels) = if owned {
        (
            "managed-by=runway app=gcptree stage=prod role=runtime",
            json!({"managed-by": "runway", "runway-app": "gcptree", "runway-stage": "prod"}),
        )
    } else {
        ("created by hand", json!({}))
    };
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/my-gcp-project/serviceAccounts/{SA}"
        )))
        .respond_with(json200(json!({"email": SA, "description": description})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v1/projects/my-gcp-project/secrets/gcptree-prod-api-key",
        ))
        .respond_with(json200(json!({
            "name": "projects/my-gcp-project/secrets/gcptree-prod-api-key",
            "labels": labels
        })))
        .mount(&s)
        .await;
    let cond = json!({"expression": "request.time < timestamp('2030-01-01T00:00:00Z')", "title": "temporary"});
    for (res, bindings) in [
        (
            format!("/v1/{IAP_RES}"),
            json!([
                {"role": "roles/iap.httpsResourceAccessor",
                 "members": ["group:finops@example.com", "user:manual@example.com"]},
                {"role": "roles/iap.httpsResourceAccessor", "members": ["user:temp@example.com"], "condition": cond},
                {"role": "roles/iap.admin", "members": ["user:admin@example.com"]}
            ]),
        ),
        (
            "/v1/projects/my-gcp-project/secrets/gcptree-prod-api-key".to_string(),
            json!([
                {"role": "roles/secretmanager.secretVersionAdder",
                 "members": ["group:devops@example.com", "user:intruder@example.com"]},
                {"role": "roles/secretmanager.secretAccessor", "members": [sa]}
            ]),
        ),
        (
            "/v3/projects/my-gcp-project".to_string(),
            json!([
                {"role": "roles/editor", "members": [sa, "user:owner@example.com"]},
                {"role": "roles/cloudsql.client", "members": [sa], "condition": cond}
            ]),
        ),
        (
            "/v3/projects/billing-data-1234".to_string(),
            json!([
                {"role": "roles/bigquery.jobUser", "members": [sa]},
                {"role": "roles/bigquery.dataEditor", "members": [sa]}
            ]),
        ),
    ] {
        // Any method: Secret Manager reads policies with GET.
        Mock::given(path(format!("{res}:getIamPolicy")))
            .respond_with(json200(
                json!({"version": 3, "etag": "BwX1", "bindings": bindings}),
            ))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("{res}:setIamPolicy")))
            .respond_with(json200(json!({"version": 3, "etag": "BwX2"})))
            .mount(&s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(
            "/bigquery/v2/projects/billing-data-1234/datasets/billingdata",
        ))
        .respond_with(json200(json!({"access": [
            {"role": "READER", "userByEmail": SA},
            {"role": "WRITER", "userByEmail": SA},
            {"role": "READER", "userByEmail": "keep@example.com"}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/tagValues/namespaced"))
        .and(query_param("name", "123456789012/allow-public-access/true"))
        .respond_with(json200(json!({"name": "tagValues/777"})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/v3/tagBindings"))
        .respond_with(json200(json!({"tagBindings": [
            {"name": "tagBindings/keep", "tagValue": "tagValues/777"},
            {"name": "tagBindings/extra", "tagValue": "tagValues/888",
             "tagValueNamespacedName": "123456789012/env/dev"}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v3/tagBindings/extra"))
        .respond_with(json200(json!({
            "name": "operations/untag",
            "done": true,
            "response": {"@type": "type.googleapis.com/google.protobuf.Empty"}
        })))
        .mount(&s)
        .await;
    s
}

fn removal_names(steps: &[runway::provision::Step]) -> Vec<String> {
    use runway::provision::Step;
    let mut out: Vec<String> = steps
        .iter()
        .map(|s| match s {
            Step::Revoke(g) => format!(
                "{} {} {}",
                g.member,
                g.role,
                g.target.as_ref().map_or("iap".into(), |t| t.to_string())
            ),
            Step::Untag { value, .. } => format!("untag {value}"),
            other => format!("{other:?}"),
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn access_runway_yaml_does_not_list_is_removed_from_what_runway_owns() {
    let d = authoritative_deployment();
    let server = unlisted_project(true).await;
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let found = prov.unlisted(&[]).await;
    assert!(found.unchecked.is_empty(), "{:?}", found.unchecked);
    let steps = found.steps;
    let sa = format!("serviceAccount:{SA}");
    let mut expected = vec![
        "user:manual@example.com roles/iap.httpsResourceAccessor iap".to_string(),
        "user:intruder@example.com roles/secretmanager.secretVersionAdder secret projects/my-gcp-project/secrets/gcptree-prod-api-key".into(),
        format!("{sa} roles/editor project my-gcp-project"),
        format!("{sa} roles/bigquery.dataEditor project billing-data-1234"),
        format!("{sa} roles/bigquery.dataEditor dataset billing-data-1234.billingdata"),
        "untag 123456789012/env/dev".into(),
    ];
    expected.sort();
    assert_eq!(removal_names(&steps), expected);

    // Applied: only the unlisted entries go, everything else stays.
    let retry = RetryConfig {
        attempts: 1,
        ..Default::default()
    };
    let iap_and_tag: Vec<_> = steps
        .iter()
        .filter(|s| {
            matches!(s, runway::provision::Step::Untag { .. })
                || matches!(s, runway::provision::Step::Revoke(g) if g.target.is_none())
        })
        .cloned()
        .collect();
    let done = prov
        .apply_all(&iap_and_tag, &retry, &Progress::silent(), &|_| {})
        .await
        .unwrap();
    assert!(
        done.iter().all(|r| r.outcome == StepOutcome::Changed),
        "{done:?}"
    );
    let reqs = server.received_requests().await.unwrap();
    let iap_set = set_policy_bodies(&reqs, &format!("/v1/{IAP_RES}"));
    let bindings = &iap_set[0]["policy"]["bindings"];
    assert_eq!(bindings[0]["members"], json!(["group:finops@example.com"]));
    assert_eq!(
        bindings[1]["members"],
        json!(["user:temp@example.com"]),
        "conditional bindings are not runway's"
    );
    assert_eq!(bindings[2]["role"], "roles/iap.admin", "other roles stay");
    assert!(
        reqs.iter()
            .any(|r| r.method.as_str() == "DELETE" && r.url.path() == "/v3/tagBindings/extra")
    );
    assert!(
        !reqs
            .iter()
            .any(|r| r.method.as_str() == "DELETE" && r.url.path() == "/v3/tagBindings/keep")
    );
}

#[tokio::test]
async fn an_account_or_secret_runway_did_not_create_keeps_its_access() {
    let d = authoritative_deployment();
    let server = unlisted_project(false).await;
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let found = prov.unlisted(&[]).await;
    assert!(found.unchecked.is_empty(), "{:?}", found.unchecked);
    let steps = found.steps;
    assert_eq!(
        removal_names(&steps),
        [
            "untag 123456789012/env/dev",
            "user:manual@example.com roles/iap.httpsResourceAccessor iap",
        ]
    );
}

#[test]
fn removals_found_twice_are_planned_once() {
    use runway::config::RoleTarget;
    use runway::provision::{ManagedGrant, Step, merge_removals};
    let grant = |member: &str, role: &str| {
        Step::Revoke(ManagedGrant {
            member: member.into(),
            role: role.into(),
            target: Some(RoleTarget::Dataset {
                project: "billing-data-1234".into(),
                dataset: "billingdata".into(),
            }),
            runtime: true,
            service: None,
        })
    };
    let recorded = vec![grant("group:Devs@example.com", "WRITER")];
    let merged = merge_removals(
        recorded,
        vec![
            grant("group:devs@example.com", "roles/bigquery.dataEditor"),
            grant("group:devs@example.com", "roles/bigquery.dataViewer"),
        ],
    );
    assert_eq!(merged.len(), 2, "{merged:?}");
}

#[tokio::test]
async fn what_cannot_be_read_is_reported_and_the_rest_still_removed() {
    let d = authoritative_deployment();
    let server = unlisted_project(true).await;
    Mock::given(path("/v3/projects/my-gcp-project:getIamPolicy"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
            "code": 403, "message": "permission denied", "status": "PERMISSION_DENIED"
        }})))
        .with_priority(1)
        .mount(&server)
        .await;
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let found = prov.unlisted(&[]).await;
    assert_eq!(found.unchecked.len(), 1, "{:?}", found.unchecked);
    assert!(found.unchecked[0].starts_with("runtime account roles: "));
    let names = removal_names(&found.steps);
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.iter().all(|n| !n.starts_with("serviceAccount:")));
}

#[tokio::test]
async fn revoking_a_dataset_role_keeps_conditional_entries() {
    use runway::config::RoleTarget;
    use runway::provision::{ManagedGrant, Step};
    let server = MockServer::start().await;
    let ds = "/bigquery/v2/projects/billing-data-1234/datasets/billingdata";
    let cond = json!({"expression": "request.time < timestamp('2030-01-01T00:00:00Z')", "title": "temporary"});
    Mock::given(method("GET"))
        .and(path(ds))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access": [
            {"role": "WRITER", "userByEmail": SA},
            {"role": "WRITER", "userByEmail": SA, "condition": cond},
            {"role": "READER", "userByEmail": SA},
            {"role": "WRITER", "userByEmail": "keep@example.com"}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(ds))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b)
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/my-gcp-project/serviceAccounts/{SA}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "email": SA,
            "description": "managed-by=runway app=gcptree stage=prod role=runtime"
        })))
        .mount(&server)
        .await;
    let d = authoritative_deployment();
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    let step = Step::Revoke(ManagedGrant {
        member: format!("serviceAccount:{SA}"),
        role: "roles/bigquery.dataEditor".into(),
        target: Some(RoleTarget::Dataset {
            project: "billing-data-1234".into(),
            dataset: "billingdata".into(),
        }),
        runtime: true,
        service: None,
    });
    let r = prov.apply(&step).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed, "{r:?}");
    let reqs = server.received_requests().await.unwrap();
    let patch = reqs
        .iter()
        .find(|r| r.method.as_str() == "PATCH")
        .expect("one patch");
    let body: Value = serde_json::from_slice(&patch.body).unwrap();
    let access = body["access"].as_array().unwrap();
    assert_eq!(access.len(), 3, "{access:?}");
    assert!(
        access
            .iter()
            .any(|a| a["role"] == "WRITER" && a["userByEmail"] == SA && !a["condition"].is_null()),
        "the conditional entry is not runway's: {access:?}"
    );
    assert!(
        !access
            .iter()
            .any(|a| a["role"] == "WRITER" && a["userByEmail"] == SA && a["condition"].is_null())
    );
}

fn iap_error(reason: Option<&str>) -> ResponseTemplate {
    let details = match reason {
        Some(r) => json!([{
            "@type": "type.googleapis.com/google.rpc.ErrorInfo",
            "reason": r,
            "domain": "googleapis.com",
            "metadata": {"service": "iap.googleapis.com"}
        }]),
        None => json!([]),
    };
    ResponseTemplate::new(403).set_body_json(json!({"error": {
        "code": 403, "message": "denied", "status": "PERMISSION_DENIED", "details": details
    }}))
}

/// Unlisted removals with IAP disabled in runway.yaml and nothing recorded;
/// `error`: what reading the IAP policy answers instead of the policy.
async fn unlisted_without_iap(error: Option<ResponseTemplate>) -> runway::provision::Unlisted {
    let mut d = authoritative_deployment();
    d.service.iap.enabled = false;
    d.service.iap.members.clear();
    let server = unlisted_project(true).await;
    if let Some(e) = error {
        Mock::given(path(format!("/v1/{IAP_RES}:getIamPolicy")))
            .respond_with(e)
            .with_priority(1)
            .mount(&server)
            .await;
    }
    let session = Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let prov = provisioner_for(&server, &d, &session, &run).await;
    prov.unlisted(&[]).await
}

#[tokio::test]
async fn iap_members_left_after_iap_is_disabled_are_removed_without_a_record() {
    let found = unlisted_without_iap(None).await;
    assert!(found.unchecked.is_empty(), "{:?}", found.unchecked);
    let iap: Vec<String> = removal_names(&found.steps)
        .into_iter()
        .filter(|n| n.ends_with(" iap"))
        .collect();
    assert_eq!(
        iap,
        [
            "group:finops@example.com roles/iap.httpsResourceAccessor iap",
            "user:manual@example.com roles/iap.httpsResourceAccessor iap",
        ],
        "nothing is listed once IAP is disabled, recorded or not"
    );
}

#[tokio::test]
async fn iap_never_used_is_not_a_warning_but_an_unreadable_policy_is() {
    let disabled = unlisted_without_iap(Some(iap_error(Some("SERVICE_DISABLED")))).await;
    assert!(disabled.unchecked.is_empty(), "{:?}", disabled.unchecked);
    assert!(
        removal_names(&disabled.steps)
            .iter()
            .all(|n| !n.ends_with(" iap"))
    );

    let denied = unlisted_without_iap(Some(iap_error(None))).await;
    assert_eq!(denied.unchecked.len(), 1, "{:?}", denied.unchecked);
    assert!(denied.unchecked[0].starts_with("IAP access: "));
}
