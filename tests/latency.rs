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
