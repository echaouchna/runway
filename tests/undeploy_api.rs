//! Wire-level tests of `undeploy`: ownership markers decide what is deleted;
//! data, APIs and resources runway did not create are kept and reported.

use runway::commands::undeploy::{Action, Teardown, execute, plan};
use runway::config::{self, Deployment, Overrides};
use runway::gcp::Session;
use runway::output::Progress;
use runway::provision::{Endpoints, Provisioner, sa_marker};
use runway::retry::RetryConfig;
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SA: &str = "gcptree-run@my-gcp-project.iam.gserviceaccount.com";
const SVC: &str = "projects/my-gcp-project/locations/europe-west1/services/gcptree-prod";

fn deployment(dir: &std::path::Path) -> Deployment {
    let p = dir.join("runway.yaml");
    std::fs::write(
        &p,
        format!(
            r#"
version: 1
app: gcptree
provider: {{project: my-gcp-project, region: europe-west1}}
buckets:
  cache: {{ name: "${{project}}-gcptree-cache" }}
service:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/gcptree:1
  service_account: {SA}
  identity:
    create: true
    roles:
      - {{ role: roles/bigquery.jobUser, project: billing-data-1234 }}
      - {{ role: roles/bigquery.dataViewer, dataset: billing-data-1234.billingdata }}
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

async fn server(service_labels: Value, sa_description: &str) -> MockServer {
    let s = MockServer::start().await;
    let ok = |v: Value| ResponseTemplate::new(200).set_body_json(v);
    Mock::given(method("GET"))
        .and(path(format!("/v2/{SVC}")))
        .respond_with(ok(
            json!({"name": SVC, "labels": service_labels, "etag": "\"svc-etag-7\"",
            "template": {"serviceAccount": SA}}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v2/{SVC}")))
        .respond_with(ok(json!({"name": "operations/del", "done": true,
            "response": {"@type": "type.googleapis.com/google.cloud.run.v2.Service", "name": SVC}})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v2/projects/my-gcp-project/locations/europe-west1/services",
        ))
        .respond_with(ok(json!({"services": [
            {"name": SVC, "template": {"serviceAccount": SA}},
            {"name": "projects/my-gcp-project/locations/europe-west1/services/other",
             "template": {"serviceAccount": "someone@my-gcp-project.iam.gserviceaccount.com"}}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/my-gcp-project/serviceAccounts/{SA}"
        )))
        .respond_with(ok(json!({"email": SA, "description": sa_description})))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!(
            "/v1/projects/my-gcp-project/serviceAccounts/{SA}"
        )))
        .respond_with(ok(json!({})))
        .mount(&s)
        .await;
    Mock::given(path("/v3/projects/billing-data-1234:getIamPolicy"))
        .respond_with(ok(json!({"etag": "BwX1", "bindings": [
            {"role": "roles/bigquery.jobUser", "members": [format!("serviceAccount:{SA}"), "user:keep@example.com"]}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/projects/billing-data-1234:setIamPolicy"))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b["policy"].clone())
        })
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/bigquery/v2/projects/billing-data-1234/datasets/billingdata",
        ))
        .respond_with(ok(json!({"access": [
            {"role": "OWNER", "userByEmail": "owner@example.com"},
            {"role": "READER", "userByEmail": SA}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("PATCH"))
        .and(path(
            "/bigquery/v2/projects/billing-data-1234/datasets/billingdata",
        ))
        .respond_with(|r: &Request| {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::from_slice::<Value>(&r.body).unwrap())
        })
        .mount(&s)
        .await;
    s
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

async fn clients(s: &MockServer) -> (Session, google_cloud_run_v2::client::Services) {
    let session = Session::from_static_token("t").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(s.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    (session, run)
}

#[tokio::test]
async fn removes_what_runway_created_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let d = deployment(dir.path());
    let s = server(
        json!({"managed-by": "runway", "runway-app": "gcptree", "runway-stage": "prod"}),
        &sa_marker("gcptree", Some("prod"), "runtime"),
    )
    .await;
    let (session, run) = clients(&s).await;
    let prov = Provisioner::with_options(&d, &session, &run, &endpoints(&s.uri()), true)
        .await
        .unwrap();

    let (mut items, del_svc, del_sa) = plan(&d, &run, &prov, false).await.unwrap();
    assert!(del_svc && del_sa);
    let action = |res: &str| {
        items
            .iter()
            .find(|i| i.resource.starts_with(res))
            .map(|i| i.action)
    };
    assert_eq!(
        action("Cloud Run service gcptree-prod"),
        Some(Action::Delete)
    );
    assert_eq!(
        action(&format!("service account {SA}")),
        Some(Action::Delete)
    );
    assert_eq!(action("grant roles/bigquery.jobUser"), Some(Action::Revoke));
    assert_eq!(
        action("bucket gs://my-gcp-project-gcptree-cache"),
        Some(Action::Keep)
    );
    assert_eq!(action("APIs on my-gcp-project"), Some(Action::Keep));
    assert!(
        s.received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| matches!(r.method.as_str(), "GET" | "POST")
                && !r.url.path().ends_with(":setIamPolicy")),
        "planning is read-only"
    );

    let retry = RetryConfig {
        attempts: 1,
        ..Default::default()
    };
    execute(
        &d,
        &run,
        &prov,
        &retry,
        &Progress::silent(),
        &mut items,
        Teardown {
            delete_service: del_svc,
            delete_sa: del_sa,
            delete_images: false,
            timeout: Duration::from_secs(5),
        },
    )
    .await
    .unwrap();

    let reqs = s.received_requests().await.unwrap();
    let order: Vec<String> = reqs
        .iter()
        .filter(|r| {
            matches!(r.method.as_str(), "DELETE" | "PATCH")
                || r.url.path().ends_with(":setIamPolicy")
        })
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert_eq!(
        order,
        [
            format!("DELETE /v2/{SVC}"),
            "POST /v3/projects/billing-data-1234:setIamPolicy".to_string(),
            "PATCH /bigquery/v2/projects/billing-data-1234/datasets/billingdata".to_string(),
            format!("DELETE /v1/projects/my-gcp-project/serviceAccounts/{SA}"),
        ],
        "service first, then grants, then the account"
    );
    let del = reqs
        .iter()
        .find(|r| r.method.as_str() == "DELETE" && r.url.path() == format!("/v2/{SVC}"))
        .unwrap();
    assert!(
        del.url
            .query_pairs()
            .any(|(k, v)| k == "etag" && v == "\"svc-etag-7\""),
        "the delete is conditioned on the etag just read: {}",
        del.url
    );
    let set = reqs
        .iter()
        .find(|r| r.url.path().ends_with(":setIamPolicy"))
        .unwrap();
    let b: Value = serde_json::from_slice(&set.body).unwrap();
    assert_eq!(
        b["policy"]["bindings"][0]["members"],
        json!(["user:keep@example.com"]),
        "other members kept"
    );
    let patch = reqs.iter().find(|r| r.method.as_str() == "PATCH").unwrap();
    let b: Value = serde_json::from_slice(&patch.body).unwrap();
    assert_eq!(
        b["access"],
        json!([{"role": "OWNER", "userByEmail": "owner@example.com"}])
    );
    assert!(items.iter().filter(|i| i.outcome.is_some()).count() >= 4);
}

#[tokio::test]
async fn keeps_accounts_without_marker_and_refuses_foreign_services() {
    let dir = tempfile::tempdir().unwrap();
    let d = deployment(dir.path());

    // Account created by hand: kept with its grants.
    let s = server(
        json!({"managed-by": "runway", "runway-app": "gcptree", "runway-stage": "prod"}),
        "created by the platform team",
    )
    .await;
    let (session, run) = clients(&s).await;
    let prov = Provisioner::with_options(&d, &session, &run, &endpoints(&s.uri()), true)
        .await
        .unwrap();
    let (items, del_svc, del_sa) = plan(&d, &run, &prov, false).await.unwrap();
    assert!(del_svc && !del_sa);
    let sa = items
        .iter()
        .find(|i| i.resource == format!("service account {SA}"))
        .unwrap();
    assert_eq!(sa.action, Action::Keep);
    assert!(sa.reason.contains("existed before"), "{}", sa.reason);
    assert!(
        items
            .iter()
            .filter(|i| i.resource.starts_with("grant "))
            .all(|i| i.action == Action::Keep)
    );

    // Service owned by another app: refused.
    let s = server(
        json!({"managed-by": "runway", "runway-app": "other", "runway-stage": "prod"}),
        "x",
    )
    .await;
    let (session, run) = clients(&s).await;
    let prov = Provisioner::with_options(&d, &session, &run, &endpoints(&s.uri()), true)
        .await
        .unwrap();
    let err = plan(&d, &run, &prov, false).await.unwrap_err();
    assert_eq!(err.kind, runway::error::ErrorKind::Conflict);
}

#[tokio::test]
async fn never_deletes_a_service_that_changed_owner_after_planning() {
    let dir = tempfile::tempdir().unwrap();
    let d = deployment(dir.path());
    // Planned against our service, but by execution time the name belongs to
    // another app (deleted and recreated in between).
    let s = server(
        json!({"managed-by": "runway", "runway-app": "other-app", "runway-stage": "prod"}),
        &sa_marker("gcptree", Some("prod"), "runtime"),
    )
    .await;
    let (session, run) = clients(&s).await;
    let prov = Provisioner::with_options(&d, &session, &run, &endpoints(&s.uri()), true)
        .await
        .unwrap();
    let retry = RetryConfig {
        attempts: 3,
        delay: Duration::from_millis(1),
        ..Default::default()
    };
    let err = execute(
        &d,
        &run,
        &prov,
        &retry,
        &Progress::silent(),
        &mut [],
        Teardown {
            delete_service: true,
            delete_sa: false,
            delete_images: false,
            timeout: Duration::from_secs(5),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, runway::error::ErrorKind::Conflict);
    let reqs = s.received_requests().await.unwrap();
    assert!(
        reqs.iter().all(|r| r.method.as_str() != "DELETE"),
        "nothing deleted"
    );
    assert_eq!(
        reqs.iter().filter(|r| r.method.as_str() == "GET").count(),
        1,
        "a refusal is permanent: not retried"
    );
}
