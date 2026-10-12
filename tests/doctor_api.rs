//! Doctor checks the repository's existence before probing creation permission.
use google_cloud_artifactregistry_v1::client::ArtifactRegistry;
use google_cloud_resourcemanager_v3::client::Projects;
use runway::commands::doctor::{CheckStatus, check_repositories};
use runway::config::{self, Overrides};
use runway::gcp::Session;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CREATE: &str = "artifactregistry.repositories.create";

async fn check(project: &str, exists: bool, create: bool, permission_status: u16, allowed: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("runway.yaml");
    std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(
        &config_path,
        format!(
            r#"
version: 1
app: example
provider:
  project: my-gcp-project
  region: europe-west1
  artifact_project: {project}
  artifact_repository: apps
  create_build_resources: {create}
  source_bucket: my-source-bucket
  build_service_account: build@my-gcp-project.iam.gserviceaccount.com
service:
  source: .
  service_account: runtime@my-gcp-project.iam.gserviceaccount.com
stages: {{prod: {{}}}}
"#
        ),
    )
    .unwrap();
    let (_, resolved) =
        config::load_and_resolve(&config_path, "prod", &Overrides::default()).unwrap();
    let d = &resolved.deployments[0];
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/{project}/locations/europe-west1/repositories/apps"
        )))
        .respond_with(if exists {
            ResponseTemplate::new(200).set_body_json(json!({"format": "DOCKER"}))
        } else {
            ResponseTemplate::new(404).set_body_json(
                json!({"error": {"code":404,"status":"NOT_FOUND","message":"missing"}}),
            )
        })
        .expect(1)
        .mount(&server)
        .await;
    let probes = u64::from(!exists && create);
    Mock::given(method("POST"))
        .and(path(format!("/v3/projects/{project}:testIamPermissions")))
        .and(body_json(json!({"permissions": [CREATE]})))
        .respond_with(if permission_status == 200 {
            ResponseTemplate::new(200)
                .set_body_json(json!({"permissions": if allowed { vec![CREATE] } else { vec![] }}))
        } else {
            ResponseTemplate::new(permission_status).set_body_json(
                json!({"error": {"code":403,"status":"PERMISSION_DENIED","message":"cannot test"}}),
            )
        })
        .expect(probes)
        .mount(&server)
        .await;
    let session = Session::from_static_token("test-token").unwrap();
    let ar = ArtifactRegistry::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let projects = Projects::builder()
        .with_endpoint(server.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let report = check_repositories(&session, d, &ar, &projects)
        .await
        .unwrap();
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1 + probes as usize
    );
    assert_eq!(report.checks.len(), 1);
    let result = &report.checks[0];
    let expected = if exists {
        CheckStatus::Pass
    } else if create && (allowed || permission_status != 200) {
        CheckStatus::Warn
    } else {
        CheckStatus::Fail
    };
    assert_eq!(result.status, expected, "{result:?}");
    if !exists && create && !allowed {
        let hint = result.hint.as_ref().unwrap();
        assert!(
            hint.contains(&format!("gcloud projects add-iam-policy-binding {project}")),
            "{hint}"
        );
    }
}

#[tokio::test]
async fn existing_repositories_need_no_project_creation_permission() {
    for project in ["my-gcp-project", "my-registry-project"] {
        check(project, true, true, 200, false).await;
    }
}

#[tokio::test]
async fn missing_repositories_check_permission_in_their_own_project() {
    for project in ["my-gcp-project", "my-registry-project"] {
        for allowed in [false, true] {
            check(project, false, true, 200, allowed).await;
        }
    }
}

#[tokio::test]
async fn an_unreadable_creation_permission_is_a_warning() {
    check("my-registry-project", false, true, 403, false).await;
}

#[tokio::test]
async fn an_unmanaged_missing_repository_does_not_probe_creation() {
    check("my-registry-project", false, false, 200, false).await;
}
