//! Wire-level tests of Cloud Run jobs, Cloud Scheduler jobs and the
//! scheduler's invoker grant: real SDK clients against a mock server.

use google_cloud_auth::credentials::anonymous;
use runway::config::{self, JobSettings, Overrides, ScheduleConfig, ScheduleTarget};
use runway::gcp::Session;
use runway::gcp::jobs::{JobChange, JobReconciler};
use runway::output::Progress;
use runway::plan::ServiceSpec;
use runway::poll::PollConfig;
use runway::provision::{Endpoints, Provisioner, StepOutcome, schedule_grant_steps};
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const PARENT: &str = "projects/my-gcp-project/locations/europe-west1";

fn resolved() -> (tempfile::TempDir, config::Resolved) {
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
jobs:
  migrate:
    command: [python, manage.py, migrate]
    tasks: 3
    max_retries: 1
schedules:
  nightly: {schedule: "0 3 * * *", time_zone: Europe/Paris, job: migrate, paused: true}
stages: { prod: {} }
"#,
    )
    .unwrap();
    let (_, r) = config::load_and_resolve(&p, "prod", &Overrides::default()).unwrap();
    (dir, r)
}

#[tokio::test]
async fn a_job_is_created_with_its_task_settings_and_awaited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v2/{PARENT}/jobs")))
        .and(query_param("jobId", "shop-migrate-prod"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"name": format!("{PARENT}/operations/op-j")})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{PARENT}/operations/op-j")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": format!("{PARENT}/operations/op-j"), "done": true
        })))
        .mount(&server)
        .await;
    let jobs = google_cloud_run_v2::client::Jobs::builder()
        .with_endpoint(server.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    let (_dir, r) = resolved();
    let d = r.jobs().next().unwrap();
    let config::WorkloadKind::Job(settings) = &d.kind else {
        panic!("a job")
    };
    assert_eq!(
        *settings,
        JobSettings {
            tasks: 3,
            parallelism: 0,
            max_retries: 1
        }
    );
    let image = "europe-west1-docker.pkg.dev/my-gcp-project/apps/shop@sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let spec = ServiceSpec::from_deployment(d, image, Default::default());
    let progress = Progress::silent();
    let rec = JobReconciler {
        jobs: &jobs,
        progress: &progress,
        poll: PollConfig::fast(),
        timeout: Duration::from_secs(5),
    };
    let change = rec
        .apply(PARENT, &d.service_id, &spec, settings, None, "shop", "prod")
        .await
        .unwrap();
    assert_eq!(change, JobChange::Created);

    let reqs = server.received_requests().await.unwrap();
    let create = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let b: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(b["labels"]["runway-name"], "migrate");
    assert_eq!(b["labels"]["runway-app"], "shop");
    let t = &b["template"];
    assert_eq!(t["taskCount"], 3);
    let task = &t["template"];
    assert_eq!(task["maxRetries"], 1);
    assert_eq!(task["timeout"], "600s");
    assert_eq!(
        task["serviceAccount"],
        "shop-runtime@my-gcp-project.iam.gserviceaccount.com"
    );
    let c = &task["containers"][0];
    assert_eq!(c["image"], image);
    assert_eq!(c["command"], json!(["python", "manage.py", "migrate"]));
    assert!(c.get("ports").is_none(), "a job serves no requests");
}

#[tokio::test]
async fn a_paused_schedule_runs_a_job_with_an_oauth_token() {
    let server = MockServer::start().await;
    let name = format!("{PARENT}/jobs/shop-nightly-prod");
    Mock::given(method("POST"))
        .and(path(format!("/v1/{PARENT}/jobs")))
        .respond_with(|r: &Request| {
            let mut b: Value = serde_json::from_slice(&r.body).unwrap();
            b["state"] = json!("ENABLED");
            ResponseTemplate::new(200).set_body_json(b)
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/{name}:pause")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": name, "state": "PAUSED"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = google_cloud_scheduler_v1::client::CloudScheduler::builder()
        .with_endpoint(server.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    let (_dir, r) = resolved();
    let s: &ScheduleConfig = &r.schedules[0];
    assert!(matches!(s.target, ScheduleTarget::Job { .. }));
    let invoker = &r.scheduler.as_ref().unwrap().service_account;
    let uri =
        runway::gcp::scheduler::target_uri(s, "my-gcp-project", "europe-west1", None).unwrap();
    let desired = runway::gcp::scheduler::desired(s, &name, "shop", "prod", invoker, &uri, None);
    let job = runway::gcp::scheduler::apply(&client, PARENT, desired, false, true)
        .await
        .unwrap();
    assert_eq!(
        job.state,
        google_cloud_scheduler_v1::model::job::State::Paused
    );

    let reqs = server.received_requests().await.unwrap();
    let b: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(b["schedule"], "0 3 * * *");
    assert_eq!(b["timeZone"], "Europe/Paris");
    assert_eq!(b["httpTarget"]["uri"], uri);
    assert_eq!(
        b["httpTarget"]["oauthToken"]["serviceAccountEmail"],
        invoker.as_str()
    );
    assert_eq!(
        b["httpTarget"]["oauthToken"]["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert!(
        b["description"]
            .as_str()
            .unwrap()
            .contains("managed-by=runway app=shop stage=prod")
    );
}

#[tokio::test]
async fn the_invoker_is_granted_run_invoker_on_the_job() {
    let server = MockServer::start().await;
    let job = format!("{PARENT}/jobs/shop-migrate-prod");
    Mock::given(method("GET"))
        .and(path(format!("/v2/{job}:getIamPolicy")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"etag": "BwX1"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v2/{job}:setIamPolicy")))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b["policy"].clone())
        })
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
    let (_dir, r) = resolved();
    let ep = Endpoints {
        run: Some(server.uri()),
        scheduler: Some(server.uri()),
        iam: Some(server.uri()),
        resource_manager: Some(server.uri()),
        tag_bindings: Some(server.uri()),
        iap: Some(server.uri()),
        ..Default::default()
    };
    let prov = Provisioner::with_endpoints(r.first(), &session, &run, &ep)
        .await
        .unwrap()
        .with_stack(&r);
    let steps = schedule_grant_steps(&r);
    assert_eq!(steps.len(), 1);
    let res = prov.apply(&steps[0]).await.unwrap();
    assert_eq!(res.outcome, StepOutcome::Changed);
    // Recorded: removing the schedule revokes it.
    assert!(prov.granted().iter().any(|g| g.role == "roles/run.invoker"));

    let reqs = server.received_requests().await.unwrap();
    let set = reqs
        .iter()
        .find(|r| r.url.path().ends_with(":setIamPolicy"))
        .unwrap();
    let b: Value = serde_json::from_slice(&set.body).unwrap();
    assert_eq!(b["policy"]["bindings"][0]["role"], "roles/run.invoker");
    assert_eq!(
        b["policy"]["bindings"][0]["members"],
        json!(["serviceAccount:shop-prod-sched@my-gcp-project.iam.gserviceaccount.com"])
    );
}
