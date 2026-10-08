//! Wire-level tests of lifecycle cases with several services, jobs and
//! schedules: partial deploys and teardowns, shared identities, and what
//! runway.yaml no longer lists. Real SDK clients against a mock server.

use runway::commands::deploy_stack::{Holder, save_holder};
use runway::commands::plan::holder_record;
use runway::commands::undeploy::{Action, execute_all, plan, plan_schedules, settle_accounts};
use runway::config::{self, Overrides, Resolved};
use runway::deploy::Reconciler;
use runway::gcp::Session;
use runway::gcp::jobs::JobReconciler;
use runway::output::Progress;
use runway::poll::PollConfig;
use runway::provision::{Endpoints, ManagedGrant, Provisioner, Step, sa_marker};
use runway::retry::RetryConfig;
use serde_json::{Value, json};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PARENT: &str = "projects/my-gcp-project/locations/europe-west1";
const SA: &str = "shop-runtime@my-gcp-project.iam.gserviceaccount.com";
const IMAGE: &str = "europe-west1-docker.pkg.dev/my-gcp-project/apps/shop@sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn resolved(body: &str) -> (tempfile::TempDir, Resolved) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        format!(
            r#"
version: 1
app: shop
provider: {{project: my-gcp-project, region: europe-west1}}
defaults:
  image: {IMAGE}
  service_account: {SA}
  identity: {{create: true}}
{body}
stages: {{ prod: {{}} }}
"#
        ),
    )
    .unwrap();
    let (_, r) = config::load_and_resolve(&p, "prod", &Overrides::default()).unwrap();
    (dir, r)
}

fn ok(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}

fn owned_labels() -> Value {
    json!({"managed-by": "runway", "runway-app": "shop", "runway-stage": "prod"})
}

struct Clients {
    session: Session,
    run: google_cloud_run_v2::client::Services,
    jobs: google_cloud_run_v2::client::Jobs,
}

async fn clients(s: &MockServer) -> Clients {
    let session = Session::from_static_token("t").unwrap();
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(s.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    let jobs = google_cloud_run_v2::client::Jobs::builder()
        .with_endpoint(s.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    Clients { session, run, jobs }
}

fn endpoints(uri: &str) -> Endpoints {
    Endpoints {
        run: Some(uri.into()),
        scheduler: Some(uri.into()),
        iam: Some(uri.into()),
        resource_manager: Some(uri.into()),
        tag_bindings: Some(uri.into()),
        iap: Some(uri.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_partial_undeploy_keeps_a_schedule_runway_did_not_create() {
    let s = MockServer::start().await;
    let name = format!("{PARENT}/jobs/shop-nightly-prod");
    Mock::given(method("GET"))
        .and(path(format!("/v1/{name}")))
        .respond_with(ok(json!({"name": name, "description": "made by hand"})))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ok(json!({})))
        .expect(0)
        .mount(&s)
        .await;
    let c = clients(&s).await;
    let (_dir, r) = resolved(
        "jobs: {migrate: {}}\nschedules: {nightly: {schedule: \"0 3 * * *\", job: migrate}}",
    );
    let prov = Provisioner::with_endpoints(r.first(), &c.session, &c.run, &endpoints(&s.uri()))
        .await
        .unwrap()
        .with_stack(&r);
    let job = r.jobs().next().unwrap();
    let mut items = Vec::new();
    let (names, invoker) = plan_schedules(&r, &prov, &[job], false, false, &mut items)
        .await
        .unwrap();
    assert!(names.is_empty() && invoker.is_none(), "{names:?}");
    assert!(
        items
            .iter()
            .any(|i| i.resource == "schedule shop-nightly-prod" && i.action == Action::Keep)
    );
    // Checked again right before a delete.
    let scheduler = prov.scheduler_client().unwrap();
    let e = runway::gcp::scheduler::delete_owned(&scheduler, &name, "shop", "prod")
        .await
        .unwrap_err();
    assert_eq!(e.kind, runway::error::ErrorKind::Conflict);
}

/// Two owned services running as `SA`, deletable, and nothing else using it.
async fn mount_two_services(s: &MockServer) -> String {
    for svc in ["shop-prod", "shop-web-prod"] {
        let n = format!("{PARENT}/services/{svc}");
        let mut labels = owned_labels();
        if svc == "shop-web-prod" {
            labels["runway-name"] = json!("web");
        }
        Mock::given(method("GET"))
            .and(path(format!("/v2/{n}")))
            .respond_with(ok(json!({"name": n, "labels": labels, "etag": "\"e\"",
                "template": {"serviceAccount": SA}})))
            .mount(s)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!("/v2/{n}")))
            .respond_with(ok(json!({"name": format!("{PARENT}/operations/del"), "done": true,
                "response": {"@type": "type.googleapis.com/google.cloud.run.v2.Service", "name": n}})))
            .expect(1)
            .mount(s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!("/v2/{PARENT}/services")))
        .respond_with(ok(json!({"services": [
            {"name": format!("{PARENT}/services/shop-prod"), "template": {"serviceAccount": SA}},
            {"name": format!("{PARENT}/services/shop-web-prod"), "template": {"serviceAccount": SA}}
        ]})))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{PARENT}/jobs")))
        .respond_with(ok(json!({})))
        .mount(s)
        .await;
    let sa_path = format!("/v1/projects/my-gcp-project/serviceAccounts/{SA}");
    Mock::given(method("GET"))
        .and(path(sa_path.clone()))
        .respond_with(ok(
            json!({"email": SA, "description": sa_marker("shop", Some("prod"), "runtime")}),
        ))
        .mount(s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(sa_path.clone()))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(s)
        .await;
    sa_path
}

/// Plans and runs the teardown of every workload of `r`; returns the plan.
async fn tear_down(
    r: &Resolved,
    c: &Clients,
    s: &MockServer,
) -> Vec<runway::commands::undeploy::Item> {
    let prov = Provisioner::with_endpoints(r.first(), &c.session, &c.run, &endpoints(&s.uri()))
        .await
        .unwrap()
        .with_stack(r);
    let removing: Vec<String> = r.deployments.iter().map(|d| d.service_name()).collect();
    let mut items: Vec<runway::commands::undeploy::Item> = Vec::new();
    let mut plans = Vec::new();
    for d in &r.deployments {
        let (its, del_svc, del_sa) = plan(d, &c.run, &c.jobs, &prov, false, &removing)
            .await
            .unwrap();
        assert!(del_svc);
        for i in its {
            if !items.iter().any(|x| x.resource == i.resource) {
                items.push(i);
            }
        }
        plans.push((d, del_svc, del_sa));
    }
    settle_accounts(&mut plans, &mut items);
    assert!(
        plans.iter().all(|(_, _, del_sa)| *del_sa),
        "only these two run as the account: it goes, for both"
    );
    let retry = RetryConfig {
        attempts: 1,
        ..Default::default()
    };
    let progress = Progress::silent();
    execute_all(
        &plans,
        &c.run,
        &c.jobs,
        &prov,
        &retry,
        &progress,
        &mut items,
        false,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    items
}

#[tokio::test]
async fn a_shared_account_is_deleted_after_every_service() {
    let s = MockServer::start().await;
    let sa_path = mount_two_services(&s).await;
    let c = clients(&s).await;
    let (_dir, r) = resolved("service: {}\nservices: {web: {}}");
    let _ = tear_down(&r, &c, &s).await;
    let order: Vec<String> = s
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|q| q.method.as_str() == "DELETE")
        .map(|q| q.url.path().to_string())
        .collect();
    assert_eq!(order.len(), 3, "{order:?}");
    assert_eq!(order[2], sa_path, "the account goes last: {order:?}");
}

/// Two services share the account with different roles; `main_create` is
/// the main service's `identity.create` (the other one creates it).
async fn shared_account_with_different_roles(main_create: bool) {
    let s = MockServer::start().await;
    let sa_path = mount_two_services(&s).await;
    let member = format!("serviceAccount:{SA}");
    Mock::given(path("/v3/projects/my-gcp-project:getIamPolicy"))
        .respond_with(ok(json!({"etag": "BwX1", "bindings": [
            {"role": "roles/viewer", "members": [member]},
            {"role": "roles/logging.viewer", "members": [member]}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/projects/my-gcp-project:setIamPolicy"))
        .respond_with(|r: &wiremock::Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ok(b["policy"].clone())
        })
        .mount(&s)
        .await;
    let c = clients(&s).await;
    // Each service gives the shared account a different role.
    let (_dir, r) = resolved(&format!(
        "service:\n  identity: {{create: {main_create}, roles: [{{role: roles/viewer, project: my-gcp-project}}]}}\n\
         services:\n  web:\n    identity: {{create: true, roles: [{{role: roles/logging.viewer, project: my-gcp-project}}]}}",
    ));
    let items = tear_down(&r, &c, &s).await;
    // The plan says what execution does: the account goes, both roles too.
    let action = |res: &str| items.iter().find(|i| i.resource == res).map(|i| i.action);
    assert_eq!(
        action(&format!("service account {SA}")),
        Some(Action::Delete)
    );
    for role in ["roles/viewer", "roles/logging.viewer"] {
        let res = format!("grant {role} on project my-gcp-project to {SA}");
        assert_eq!(action(&res), Some(Action::Revoke), "{res}");
    }
    let reqs = s.received_requests().await.unwrap();
    let mut revoked: Vec<String> = Vec::new();
    for q in reqs
        .iter()
        .filter(|q| q.url.path().ends_with(":setIamPolicy"))
    {
        let b: Value = serde_json::from_slice(&q.body).unwrap();
        for role in ["roles/viewer", "roles/logging.viewer"] {
            let still = b["policy"]["bindings"].as_array().unwrap().iter().any(|x| {
                x["role"] == role && x["members"].as_array().unwrap().contains(&json!(member))
            });
            if !still && !revoked.contains(&role.to_string()) {
                revoked.push(role.to_string());
            }
        }
    }
    revoked.sort();
    assert_eq!(
        revoked,
        ["roles/logging.viewer", "roles/viewer"],
        "both services' roles"
    );
    let last = reqs
        .iter()
        .rposition(|q| q.url.path().ends_with(":setIamPolicy"))
        .unwrap();
    let deleted = reqs
        .iter()
        .position(|q| q.method.as_str() == "DELETE" && q.url.path() == sa_path)
        .unwrap();
    assert!(last < deleted, "revoked, then the account deleted");
}

#[tokio::test]
async fn a_shared_account_loses_the_roles_of_every_service_before_deletion() {
    shared_account_with_different_roles(true).await;
}

#[tokio::test]
async fn a_shared_account_created_by_one_service_loses_the_roles_of_both() {
    shared_account_with_different_roles(false).await;
}

#[tokio::test]
async fn a_live_job_keeps_the_runtime_account_even_when_unconfigured() {
    let s = MockServer::start().await;
    let svc = format!("{PARENT}/services/shop-prod");
    Mock::given(method("GET"))
        .and(path(format!("/v2/{svc}")))
        .respond_with(ok(
            json!({"name": svc, "labels": owned_labels(), "etag": "\"e\""}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{PARENT}/services")))
        .respond_with(ok(
            json!({"services": [{"name": svc, "template": {"serviceAccount": SA}}]}),
        ))
        .mount(&s)
        .await;
    // Removed from runway.yaml, still deployed.
    Mock::given(method("GET"))
        .and(path(format!("/v2/{PARENT}/jobs")))
        .respond_with(ok(json!({"jobs": [{
            "name": format!("{PARENT}/jobs/shop-report-prod"),
            "template": {"template": {"serviceAccount": SA}}
        }]})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v1/projects/my-gcp-project/serviceAccounts/{SA}"
        )))
        .respond_with(ok(
            json!({"email": SA, "description": sa_marker("shop", Some("prod"), "runtime")}),
        ))
        .mount(&s)
        .await;
    let c = clients(&s).await;
    let (_dir, r) = resolved("service: {}");
    let d = r.first();
    let prov = Provisioner::with_endpoints(d, &c.session, &c.run, &endpoints(&s.uri()))
        .await
        .unwrap();
    let (items, del_svc, del_sa) = plan(d, &c.run, &c.jobs, &prov, false, &[d.service_name()])
        .await
        .unwrap();
    assert!(del_svc && !del_sa);
    let sa = items
        .iter()
        .find(|i| i.resource == format!("service account {SA}"))
        .unwrap();
    assert_eq!(sa.action, Action::Keep);
    assert!(sa.reason.contains("job shop-report-prod"), "{}", sa.reason);
}

#[tokio::test]
async fn an_unowned_holder_is_neither_read_nor_written() {
    let s = MockServer::start().await;
    let svc = format!("{PARENT}/services/shop-prod");
    let foreign = r#"[{"member":"user:someone@example.com","role":"roles/owner","target":{"Project":{"project":"my-gcp-project"}}}]"#;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{svc}")))
        .respond_with(ok(json!({"name": svc, "labels": {"team": "other"},
            "annotations": {"runway.dev/grants": foreign}})))
        .mount(&s)
        .await;
    Mock::given(method("PATCH"))
        .respond_with(ok(json!({})))
        .expect(0)
        .mount(&s)
        .await;
    let c = clients(&s).await;
    // `deploy --only migrate`: the main service (the holder) is not selected.
    let (_dir, r) = resolved("service: {}\njobs: {migrate: {}}");
    assert_eq!(
        holder_record(&r, &c.run, &c.jobs).await.unwrap(),
        Vec::<ManagedGrant>::new(),
        "foreign annotations are not runway's record"
    );
    let progress = Progress::silent();
    let rec = Reconciler {
        run: &c.run,
        revisions: None,
        progress: &progress,
        poll: PollConfig::fast(),
        timeout: Duration::from_secs(5),
    };
    let jrec = JobReconciler {
        jobs: &c.jobs,
        progress: &progress,
        poll: PollConfig::fast(),
        timeout: Duration::from_secs(5),
    };
    let grant = ManagedGrant {
        member: format!("serviceAccount:{SA}"),
        role: "roles/run.invoker".into(),
        target: None,
        runtime: false,
        service: None,
    };
    let holder = Holder {
        d: r.holder(),
        adopting: false,
    };
    let wrote = save_holder(&rec, &jrec, &holder, &[grant], false, &progress)
        .await
        .unwrap();
    assert!(!wrote);
}

#[tokio::test]
async fn leftover_schedules_are_found_when_none_is_configured() {
    let s = MockServer::start().await;
    let mine = format!("{PARENT}/jobs/shop-nightly-prod");
    Mock::given(method("GET"))
        .and(path(format!("/v1/{PARENT}/jobs")))
        .respond_with(ok(json!({"jobs": [
            {"name": mine, "description": format!("runway schedule nightly: {}", runway::gcp::scheduler::marker("shop", "prod"))},
            {"name": format!("{PARENT}/jobs/backup"), "description": "made by hand"}
        ]})))
        .mount(&s)
        .await;
    let c = clients(&s).await;
    // The last schedule was removed: a stage with one service, as before.
    let (_dir, r) = resolved("service: {}");
    let prov = Provisioner::with_endpoints(r.first(), &c.session, &c.run, &endpoints(&s.uri()))
        .await
        .unwrap();
    let left = prov.orphan_schedules().await.unwrap();
    assert_eq!(left, vec![Step::Unschedule { name: mine }]);
}

#[tokio::test]
async fn leftover_schedules_are_found_in_the_scheduler_region() {
    let s = MockServer::start().await;
    let other = "projects/my-gcp-project/locations/us-central1";
    let mine = format!("{other}/jobs/shop-nightly-prod");
    Mock::given(method("GET"))
        .and(path(format!("/v1/{other}/jobs")))
        .respond_with(ok(json!({"jobs": [
            {"name": mine, "description": format!("runway schedule nightly: {}", runway::gcp::scheduler::marker("shop", "prod"))}
        ]})))
        .mount(&s)
        .await;
    let c = clients(&s).await;
    // No schedule left, but Cloud Scheduler is in another region than Cloud Run.
    let (_dir, r) = resolved("service: {}\nscheduler: {region: us-central1}");
    assert!(r.scheduler.is_none());
    let prov = Provisioner::with_endpoints(r.first(), &c.session, &c.run, &endpoints(&s.uri()))
        .await
        .unwrap();
    let left = prov.orphan_schedules().await.unwrap();
    assert_eq!(left, vec![Step::Unschedule { name: mine }]);
}
