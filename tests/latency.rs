//! Round-trip tests: every mocked Google endpoint answers after a fixed delay,
//! so elapsed time reveals whether independent requests run concurrently.

use google_cloud_auth::credentials::anonymous;
use runway::commands::plan::{Remote, compute};
use runway::config::{self, Overrides};
use runway::gcp::registry::RegistryClient;
use runway::output::Progress;
use runway::plan::ServiceAction;
use serde_json::json;
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DELAY: Duration = Duration::from_millis(300);
const DIGEST: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_plan_costs_one_round_trip_not_three() {
    let server = MockServer::start().await;
    let name = "projects/my-gcp-project/locations/europe-west1/services/hello-dev";
    // 1. Cloud Run service.
    Mock::given(method("GET"))
        .and(path(format!("/v2/{name}")))
        .respond_with(ResponseTemplate::new(200).set_delay(DELAY).set_body_json(json!({
            "name": name,
            "generation": "1",
            "labels": {"managed-by": "runway", "runway-app": "hello", "runway-stage": "dev"},
            "template": {"containers": [{"image": "old@sha256:1"}]}
        })))
        .mount(&server)
        .await;
    // 2. Service IAM policy.
    Mock::given(method("GET"))
        .and(path(format!("/v2/{name}:getIamPolicy")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(DELAY)
                .set_body_json(json!({})),
        )
        .mount(&server)
        .await;
    // 3. Registry manifest.
    Mock::given(method("HEAD"))
        .and(path("/v2/my-gcp-project/apps/hello/manifests/1.0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(DELAY)
                .insert_header("Docker-Content-Digest", DIGEST),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("runway.yaml");
    std::fs::write(
        &cfg_path,
        "version: 1\napp: hello\nprovider: {project: my-gcp-project, region: europe-west1}\nservice:\n  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/hello:1.0\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\nstages: {dev: {}}\n",
    )
    .unwrap();
    let (_, resolved) = config::load_and_resolve(&cfg_path, "dev", &Overrides::default()).unwrap();

    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    let registry =
        RegistryClient::new(reqwest::Client::new(), Some("t".into())).with_base_url(server.uri());
    let progress = Progress::silent();

    let started = Instant::now();
    let (plan, _) = compute(
        &resolved.deployment,
        Some(Remote {
            run: &run,
            revisions: None,
            resolver: &registry,
            provisioner: None,
        }),
        &Default::default(),
        &progress,
    )
    .await
    .unwrap();
    let elapsed = started.elapsed();

    eprintln!("remote plan took {elapsed:?}");
    assert_eq!(plan.action, ServiceAction::Update);
    assert!(plan.exact, "digest, service and policy all known");
    // Sequential would take >= 3 x 300 ms.
    assert!(
        elapsed < DELAY * 2 + Duration::from_millis(150),
        "plan took {elapsed:?}; the three reads should overlap"
    );
}

/// Grants of one service account on three project policies, two on the same.
fn grants_deployment(dir: &std::path::Path) -> config::Deployment {
    let p = dir.join("runway.yaml");
    std::fs::write(
        &p,
        r#"
version: 1
app: hello
provider: {project: my-gcp-project, region: europe-west1}
service:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/hello@sha256:3333333333333333333333333333333333333333333333333333333333333333
  service_account: hello-run@my-gcp-project.iam.gserviceaccount.com
  identity:
    create: true
    roles:
      - { role: roles/viewer, project: project-a }
      - { role: roles/viewer, project: project-b }
      - { role: roles/viewer, project: project-c }
      - { role: roles/browser, project: project-a }
stages: {dev: {}}
"#,
    )
    .unwrap();
    config::load_and_resolve(&p, "dev", &Overrides::default())
        .unwrap()
        .1
        .deployment
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provisioning_runs_independent_steps_together_and_one_policy_at_a_time() {
    const RT: Duration = Duration::from_millis(200);
    let server = MockServer::start().await;
    let sa = "hello-run@my-gcp-project.iam.gserviceaccount.com";
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/my-gcp-project/serviceAccounts/{sa}"
        )))
        .respond_with(ResponseTemplate::new(404).set_delay(RT).set_body_json(
            json!({"error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/my-gcp-project/serviceAccounts"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(RT)
                .set_body_json(json!({"email": sa})),
        )
        .mount(&server)
        .await;
    for project in ["project-a", "project-b", "project-c"] {
        let policy = json!({"version": 1, "etag": "BwX1", "bindings": []});
        Mock::given(method("POST"))
            .and(path(format!("/v3/projects/{project}:getIamPolicy")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(RT)
                    .set_body_json(policy.clone()),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/v3/projects/{project}:setIamPolicy")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(RT)
                    .set_body_json(policy),
            )
            .mount(&server)
            .await;
    }

    let dir = tempfile::tempdir().unwrap();
    let d = grants_deployment(dir.path());
    let session = runway::gcp::Session::from_static_token("test-token").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let endpoints = runway::provision::Endpoints {
        iam: Some(server.uri()),
        resource_manager: Some(server.uri()),
        ..Default::default()
    };
    let prov = runway::provision::Provisioner::with_endpoints(&d, &session, &run, &endpoints)
        .await
        .unwrap();
    let steps = runway::provision::pre_steps(&d);
    assert_eq!(steps.len(), 5, "an account and four grants");
    let retry = runway::retry::RetryConfig {
        attempts: 1,
        ..Default::default()
    };
    let reported = std::sync::Mutex::new(Vec::new());
    let started = Instant::now();
    let done = prov
        .apply_all(&steps, &retry, &Progress::silent(), &|r| {
            reported.lock().unwrap().push(r.step.clone())
        })
        .await
        .unwrap();
    let elapsed = started.elapsed();
    eprintln!("provisioning took {elapsed:?}");

    assert_eq!(done.len(), 5);
    let names: Vec<String> = steps.iter().map(|s| s.describe(&d)).collect();
    assert_eq!(*reported.lock().unwrap(), names, "reported in step order");
    // One after another: 2 + 4 x 2 = 10 round trips. In waves: the account
    // (2), then the longest lane, project-a's two grants (4).
    assert!(
        elapsed >= RT * 6,
        "took {elapsed:?}: project-a's grants must not overlap"
    );
    assert!(
        elapsed < RT * 8,
        "took {elapsed:?}: independent steps should overlap"
    );

    let calls: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| p.starts_with("/v3/projects/project-a:"))
        .collect();
    assert_eq!(
        calls,
        [
            "/v3/projects/project-a:getIamPolicy",
            "/v3/projects/project-a:setIamPolicy",
            "/v3/projects/project-a:getIamPolicy",
            "/v3/projects/project-a:setIamPolicy",
        ],
        "one read-modify-write of the policy at a time"
    );
}
