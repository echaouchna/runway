//! Secrets: creation (without a value), who may add values, runtime access
//! implied by the configuration, and the stop-until-a-value-exists step.
//! Real SDK clients against a mock server.

use runway::config::{self, Deployment, Overrides, RoleTarget};
use runway::error::ErrorKind;
use runway::gcp::Session;
use runway::provision::{Endpoints, Provisioner, Step, StepOutcome, StepState, pre_steps};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SA: &str = "gcptree-run@my-gcp-project.iam.gserviceaccount.com";
const SECRET: &str = "projects/my-gcp-project/secrets/gcptree-prod-api-key";

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
secrets:
  api-key:
    name: "${{app}}-${{stage}}-api-key"
    adders: [group:devops@example.com]
service:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/gcptree:1
  service_account: {SA}
  secrets:
    API_KEY: {{ secret: "${{secrets.api-key}}" }}
    tls: {{ secret: tls-cert, path: /secrets/tls/cert.pem }}
  volumes:
    data: {{ bucket: my-data, mount_path: /data }}
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

#[test]
fn configuration_implies_creation_access_and_a_value_check() {
    let d = deployment();
    let steps = pre_steps(&d);
    let names: Vec<String> = steps.iter().map(|s| s.describe(&d)).collect();
    let pos = |needle: &str| {
        names
            .iter()
            .position(|n| n.contains(needle))
            .unwrap_or_else(|| panic!("no step `{needle}` in {names:#?}"))
    };
    let create = pos("secret gcptree-prod-api-key");
    let accessor = pos(
        "grant roles/secretmanager.secretAccessor on secret projects/my-gcp-project/secrets/gcptree-prod-api-key",
    );
    pos(
        "grant roles/secretmanager.secretAccessor on secret projects/my-gcp-project/secrets/tls-cert",
    );
    pos("grant roles/storage.objectUser on bucket gs://my-data");
    let adders = pos("grant roles/secretmanager.secretVersionAdder");
    assert!(names[adders].contains("group:devops@example.com"));
    let values = pos("value of secret(s) gcptree-prod-api-key");
    assert!(create < accessor && accessor < values && adders < values);
    assert_eq!(
        values,
        steps.len() - 1,
        "the value check runs last, after every grant"
    );

    // Defaults: env var pinned at deploy time, file read live.
    assert!(d.service.secrets["API_KEY"].pin_latest);
    assert_eq!(d.service.secrets["API_KEY"].secret, "gcptree-prod-api-key");
    let tls = &d.service.secrets["tls"];
    assert!(!tls.pin_latest);
    assert_eq!(tls.version, "latest");
    assert_eq!(tls.path.as_deref(), Some("/secrets/tls/cert.pem"));
    assert_eq!(d.secrets["api-key"].locations, ["europe-west1"]);

    // An identical explicit role is not granted twice.
    let accessor_grants = steps
        .iter()
        .filter(|s| matches!(s, Step::Grant { binding, .. } if matches!(&binding.target, RoleTarget::Secret { name } if name == SECRET)))
        .count();
    assert_eq!(accessor_grants, 1);
}

fn endpoints(uri: &str) -> Endpoints {
    Endpoints {
        iam: Some(uri.into()),
        resource_manager: Some(uri.into()),
        secret_manager: Some(uri.into()),
        ..Default::default()
    }
}

async fn mock(versions: Value) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/{SECRET}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/my-gcp-project/secrets"))
        .and(query_param("secretId", "gcptree-prod-api-key"))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b)
        })
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/{SECRET}/versions")))
        .and(query_param("filter", "state:ENABLED"))
        .respond_with(ResponseTemplate::new(200).set_body_json(versions))
        .mount(&s)
        .await;
    s
}

#[tokio::test]
async fn creates_the_secret_and_stops_until_a_value_is_added() {
    let d = deployment();
    let server = mock(json!({})).await;
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
    let steps = pre_steps(&d);
    let create = steps
        .iter()
        .find(|s| matches!(s, Step::CreateSecret(_)))
        .unwrap();
    assert_eq!(prov.check(create, false).await.state, StepState::Pending);
    let r = prov.apply(create).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Changed);
    let reqs = server.received_requests().await.unwrap();
    let post = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(
        body["replication"]["userManaged"]["replicas"][0]["location"],
        "europe-west1"
    );
    assert_eq!(body["labels"]["runway-app"], "gcptree");
    assert!(body.get("payload").is_none(), "runway never writes values");

    let values = steps.last().unwrap();
    let check = prov.check(values, false).await;
    assert_eq!(check.state, StepState::Pending, "{}", check.detail);
    let err = prov.apply(values).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Prerequisite);
    assert!(err.permanent, "not retried: a person has to add the value");
    assert!(
        err.message.contains("gcptree-prod-api-key"),
        "{}",
        err.message
    );
    assert!(
        err.hints.iter().any(|h| h.contains(
            "gcloud secrets versions add gcptree-prod-api-key --project my-gcp-project --data-file=-"
        )),
        "{:?}",
        err.hints
    );
    assert!(
        err.hints
            .iter()
            .any(|h| h.contains("group:devops@example.com"))
    );
}

#[tokio::test]
async fn continues_once_a_value_exists_and_pins_it() {
    let d = deployment();
    let server =
        mock(json!({"versions": [{"name": format!("{SECRET}/versions/3"), "state": "ENABLED"}]}))
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
    let r = prov.apply(pre_steps(&d).last().unwrap()).await.unwrap();
    assert_eq!(r.outcome, StepOutcome::Unchanged);
    assert_eq!(
        prov.newest_enabled_version(SECRET)
            .await
            .unwrap()
            .as_deref(),
        Some("3")
    );
}
