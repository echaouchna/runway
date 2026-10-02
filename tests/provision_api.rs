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
        .deployment
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
        .deployment
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
        .deployment
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
