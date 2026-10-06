//! Reconciliation tests against an in-memory Cloud Run stub (`Services::from_stub`).

use super::*;
use crate::config::SecretRef;
use crate::naming;
use google_cloud_gax::error::rpc::Status;
use google_cloud_gax::options::RequestOptions;
use google_cloud_gax::response::Response;
use google_cloud_iam_v1::model::{Binding, GetIamPolicyRequest, Policy, SetIamPolicyRequest};
use google_cloud_longrunning::model::{GetOperationRequest, Operation};
use google_cloud_run_v2::model::{
    Condition, CreateServiceRequest, GetRevisionRequest, GetServiceRequest, Revision,
    UpdateServiceRequest, condition,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

type GaxError = google_cloud_gax::error::Error;
type R<T> = google_cloud_gax::Result<Response<T>>;

const PARENT: &str = "projects/p/locations/europe-west1";
const NAME: &str = "projects/p/locations/europe-west1/services/hello-dev";

#[derive(Debug, Default)]
struct State {
    service: Option<Service>,
    policy: Policy,
    creates: Vec<CreateServiceRequest>,
    updates: Vec<UpdateServiceRequest>,
    iam_sets: Vec<SetIamPolicyRequest>,
    /// Errors returned by create/update/setIamPolicy, in order.
    create_errors: VecDeque<GaxError>,
    update_errors: VecDeque<GaxError>,
    iam_errors: VecDeque<GaxError>,
    /// When true, the mutation is applied before the scripted error is returned (ambiguous outcome).
    apply_then_error: bool,
    polls_until_done: u32,
    polls: u32,
    fail_rollout: Option<String>,
    revision_failure: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct FakeRun(Arc<Mutex<State>>);

fn unavailable() -> GaxError {
    GaxError::service(
        Status::default()
            .set_code(Code::Unavailable)
            .set_message("backend unavailable"),
    )
}

fn status_err(code: Code, msg: &str) -> GaxError {
    GaxError::service(Status::default().set_code(code).set_message(msg))
}

fn operation(svc: &Service, n: usize) -> Operation {
    Operation::new()
        .set_name(format!("{PARENT}/operations/op-{n}"))
        .set_metadata(google_cloud_wkt::Any::from_msg(svc).unwrap())
}

impl FakeRun {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    fn mutate(st: &mut State, mut svc: Service, generation: i64) -> Service {
        svc.generation = generation;
        svc.etag = format!("etag-{generation}");
        svc.reconciling = true;
        svc.uri = "https://hello-dev-abc-ew.a.run.app".into();
        svc.latest_created_revision = format!("{NAME}/revisions/hello-dev-0000{generation}");
        if let Some(old) = &st.service {
            svc.latest_ready_revision = old.latest_ready_revision.clone();
            svc.observed_generation = old.observed_generation;
        }
        st.polls = 0;
        st.service = Some(svc.clone());
        svc
    }
}

impl FakeRun {
    /// Advances the simulated rollout by one poll; returns true once finished.
    fn advance(st: &mut State) -> bool {
        st.polls += 1;
        let done = st.polls >= st.polls_until_done;
        if done && let Some(mut svc) = st.service.clone() {
            if !svc.reconciling {
                return true;
            }
            svc.reconciling = false;
            match &st.fail_rollout {
                Some(msg) => {
                    svc.terminal_condition = Some(
                        Condition::new()
                            .set_type("Ready")
                            .set_state(condition::State::ConditionFailed)
                            .set_message(msg),
                    );
                }
                None => {
                    svc.terminal_condition = Some(
                        Condition::new()
                            .set_type("Ready")
                            .set_state(condition::State::ConditionSucceeded),
                    );
                    svc.latest_ready_revision = svc.latest_created_revision.clone();
                    svc.observed_generation = svc.generation;
                }
            }
            st.service = Some(svc);
        }
        done
    }
}

impl google_cloud_run_v2::stub::Services for FakeRun {
    async fn create_service(&self, req: CreateServiceRequest, _o: RequestOptions) -> R<Operation> {
        let mut st = self.state();
        st.creates.push(req.clone());
        let err = match st.create_errors.pop_front() {
            Some(e) if !st.apply_then_error => return Err(e),
            other => other,
        };
        if st.service.is_some() {
            return Err(status_err(Code::AlreadyExists, "exists"));
        }
        let mut svc = req.service.unwrap();
        svc.name = format!("{}/services/{}", req.parent, req.service_id);
        let svc = Self::mutate(&mut st, svc, 1);
        let n = st.creates.len();
        match err {
            Some(e) => Err(e),
            None => Ok(Response::from(operation(&svc, n))),
        }
    }

    async fn get_service(&self, req: GetServiceRequest, _o: RequestOptions) -> R<Service> {
        let mut st = self.state();
        if st.service.as_ref().is_some_and(|s| s.reconciling) {
            Self::advance(&mut st);
        }
        match &st.service {
            Some(s) if s.name == req.name => Ok(Response::from(s.clone())),
            _ => Err(status_err(Code::NotFound, "not found")),
        }
    }

    async fn update_service(&self, req: UpdateServiceRequest, _o: RequestOptions) -> R<Operation> {
        let mut st = self.state();
        st.updates.push(req.clone());
        let err = match st.update_errors.pop_front() {
            Some(e) if !st.apply_then_error => return Err(e),
            other => other,
        };
        let current = st.service.clone().expect("update requires a service");
        let incoming = req.service.unwrap();
        if !incoming.etag.is_empty() && incoming.etag != current.etag {
            return Err(status_err(Code::Aborted, "etag mismatch"));
        }
        let mut next = current.clone();
        let mask = req.update_mask.unwrap().paths;
        for p in &mask {
            match p.as_str() {
                "labels" => next.labels = incoming.labels.clone(),
                "annotations" => next.annotations = incoming.annotations.clone(),
                "template" => next.template = incoming.template.clone(),
                "traffic" => next.traffic = incoming.traffic.clone(),
                "ingress" => next.ingress = incoming.ingress.clone(),
                "invoker_iam_disabled" => next.invoker_iam_disabled = incoming.invoker_iam_disabled,
                "client" => next.client = incoming.client.clone(),
                "client_version" => next.client_version = incoming.client_version.clone(),
                "iap_enabled" => next.iap_enabled = incoming.iap_enabled,
                "custom_audiences" => next.custom_audiences = incoming.custom_audiences.clone(),
                "launch_stage" => next.launch_stage = incoming.launch_stage.clone(),
                other => panic!("unexpected mask path {other}"),
            }
        }
        let svc = Self::mutate(&mut st, next, current.generation + 1);
        let n = st.updates.len();
        match err {
            Some(e) => Err(e),
            None => Ok(Response::from(operation(&svc, n))),
        }
    }

    async fn get_operation(&self, req: GetOperationRequest, _o: RequestOptions) -> R<Operation> {
        let mut st = self.state();
        let done = Self::advance(&mut st);
        let mut op = Operation::new().set_name(req.name).set_done(done);
        if done && let Some(msg) = &st.fail_rollout {
            op = op.set_error(
                google_cloud_rpc::model::Status::new()
                    .set_code(9)
                    .set_message(msg),
            );
        }
        Ok(Response::from(op))
    }

    async fn get_iam_policy(&self, _req: GetIamPolicyRequest, _o: RequestOptions) -> R<Policy> {
        Ok(Response::from(self.state().policy.clone()))
    }

    async fn set_iam_policy(&self, req: SetIamPolicyRequest, _o: RequestOptions) -> R<Policy> {
        let mut st = self.state();
        st.iam_sets.push(req.clone());
        if let Some(e) = st.iam_errors.pop_front() {
            return Err(e);
        }
        let p = req.policy.unwrap();
        st.policy = p.clone();
        Ok(Response::from(p))
    }
}

impl google_cloud_run_v2::stub::Revisions for FakeRun {
    async fn get_revision(&self, req: GetRevisionRequest, _o: RequestOptions) -> R<Revision> {
        let st = self.state();
        let mut r = Revision::new()
            .set_name(req.name)
            .set_log_uri("https://console.cloud.google.com/logs/x");
        if let Some(msg) = &st.revision_failure {
            r = r.set_conditions([Condition::new()
                .set_type("ContainerHealthy")
                .set_state(condition::State::ConditionFailed)
                .set_message(msg)]);
        }
        Ok(Response::from(r))
    }
}

fn spec(image: &str) -> ServiceSpec {
    ServiceSpec {
        image: image.into(),
        port: 8080,
        cpu: "1".into(),
        memory: "512Mi".into(),
        timeout_seconds: 60,
        concurrency: 80,
        min_instances: 0,
        max_instances: 2,
        service_account: "rt@p.iam.gserviceaccount.com".into(),
        ingress: "all".into(),
        health_check: None,
        otel_collector: None,
        sidecars: Default::default(),
        env: BTreeMap::from([("LOG_LEVEL".into(), "info".into())]),
        secrets: BTreeMap::from([(
            "DATABASE_URL".into(),
            SecretRef {
                secret: "database-url".into(),
                version: "1".into(),
                ..Default::default()
            },
        )]),
        labels: naming::ownership_labels("hello", "dev"),
        volumes: BTreeMap::new(),
        iap_enabled: false,
        billing: crate::config::BILLING_REQUEST.into(),
        startup_cpu_boost: false,
        execution_environment: None,
        sandbox: false,
        vpc: None,
        cloud_sql: Vec::new(),
        custom_audiences: Vec::new(),
        annotations: BTreeMap::new(),
        revision_annotations: BTreeMap::new(),
        traffic: Default::default(),
    }
}

fn target(adopt: bool) -> Target<'static> {
    Target {
        parent: PARENT,
        service_id: "hello-dev",
        name: NAME,
        app: "hello",
        stage: "dev",
        adopt,
        force: false,
    }
}

struct Harness {
    fake: FakeRun,
    run: Services,
    revisions: Revisions,
    progress: Progress,
}

impl Harness {
    fn new() -> Self {
        let fake = FakeRun::default();
        fake.state().polls_until_done = 2;
        Self {
            run: Services::from_stub(fake.clone()),
            revisions: Revisions::from_stub(fake.clone()),
            fake,
            progress: Progress::silent(),
        }
    }

    fn reconciler(&self) -> Reconciler<'_> {
        Reconciler {
            run: &self.run,
            revisions: Some(&self.revisions),
            progress: &self.progress,
            poll: PollConfig::fast(),
            timeout: Duration::from_secs(5),
        }
    }

    async fn deploy(&self, s: &ServiceSpec, adopt: bool) -> Result<(Applied, Service)> {
        let r = self.reconciler();
        let existing = r.get(NAME).await?;
        let applied = r.apply(&target(adopt), s, existing).await?;
        let svc = r.wait_ready(NAME, &applied).await?;
        Ok((applied, svc))
    }
}

#[tokio::test]
async fn creates_service_and_waits_until_ready() {
    let h = Harness::new();
    let (applied, svc) = h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Created);
    assert_eq!(run::readiness(&svc), Readiness::Ready);
    assert_eq!(svc.uri, "https://hello-dev-abc-ew.a.run.app");

    let st = h.fake.state();
    assert_eq!(st.creates.len(), 1);
    let req = &st.creates[0];
    assert_eq!(req.parent, PARENT);
    assert_eq!(req.service_id, "hello-dev");
    let s = req.service.as_ref().unwrap();
    assert_eq!(s.labels["managed-by"], "runway");
    assert_eq!(
        s.template.as_ref().unwrap().containers[0].image,
        "img@sha256:1"
    );
    assert!(st.polls >= 2, "waited for the operation to finish");
}

#[tokio::test]
async fn repeated_deploys_converge_without_mutation() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    let (applied, _) = h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Unchanged);
    let st = h.fake.state();
    assert_eq!(st.creates.len(), 1);
    assert!(st.updates.is_empty(), "no update for an identical spec");
}

#[tokio::test]
async fn annotation_only_changes_update_the_service_without_a_revision() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    // Same image, now tagged as a release (`deploy --tag`).
    let mut tagged = spec("img@sha256:1");
    tagged
        .annotations
        .insert("runway.dev/release".into(), "v1.2.0".into());
    let (applied, svc) = h.deploy(&tagged, false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    assert_eq!(svc.annotations["runway.dev/release"], "v1.2.0");
    {
        let st = h.fake.state();
        let req = st.updates[0].service.as_ref().unwrap();
        let created = st.creates[0].service.as_ref().unwrap();
        assert_eq!(
            req.template, created.template,
            "template reused: no new revision"
        );
    }
    // A plain redeploy afterwards keeps the release annotation and changes nothing.
    let (applied, svc) = h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Unchanged);
    assert_eq!(svc.annotations["runway.dev/release"], "v1.2.0");
    // A configuration change on the same image keeps it too.
    let mut env_change = spec("img@sha256:1");
    env_change.env.insert("NEW".into(), "1".into());
    let (applied, svc) = h.deploy(&env_change, false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    assert_eq!(svc.annotations["runway.dev/release"], "v1.2.0");
    // A new image without --tag drops it: the release described the old image.
    let (applied, svc) = h.deploy(&spec("img@sha256:2"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    assert!(!svc.annotations.contains_key("runway.dev/release"));
}

#[tokio::test]
async fn updates_with_mask_and_etag() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    let (applied, svc) = h.deploy(&spec("img@sha256:2"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    assert_eq!(svc.generation, 2);
    let st = h.fake.state();
    let req = &st.updates[0];
    assert_eq!(req.service.as_ref().unwrap().etag, "etag-1");
    assert_eq!(req.service.as_ref().unwrap().name, NAME);
    assert_eq!(req.update_mask.as_ref().unwrap().paths, run::UPDATE_MASK);
}

#[tokio::test]
async fn refuses_unrelated_services_unless_adopting_unlabeled() {
    let h = Harness::new();
    h.fake.state().service = Some(
        Service::new()
            .set_name(NAME)
            .set_etag("e")
            .set_generation(4)
            .set_labels([("team", "x")]),
    );
    let err = h.deploy(&spec("img@sha256:1"), false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert!(err.hints.iter().any(|h| h.contains("--adopt")));
    assert!(h.fake.state().updates.is_empty());

    let (applied, svc) = h.deploy(&spec("img@sha256:1"), true).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    assert_eq!(svc.labels["team"], "x", "foreign labels preserved");
    assert_eq!(svc.labels["runway-app"], "hello");

    // Another app's service is never adopted.
    let h = Harness::new();
    h.fake.state().service = Some(
        Service::new()
            .set_name(NAME)
            .set_labels(naming::ownership_labels("other", "dev")),
    );
    let err = h.deploy(&spec("img@sha256:1"), true).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
    assert!(err.message.contains("app `other`"));
}

#[tokio::test]
async fn ambiguous_create_is_resolved_by_reading_back() {
    let h = Harness::new();
    {
        let mut st = h.fake.state();
        st.apply_then_error = true;
        st.create_errors.push_back(unavailable());
    }
    let (applied, svc) = h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Created);
    assert!(applied.operation.is_none());
    assert_eq!(run::readiness(&svc), Readiness::Ready);
    assert_eq!(h.fake.state().creates.len(), 1, "create was not repeated");
}

#[tokio::test]
async fn ambiguous_create_that_did_not_apply_is_retried() {
    let h = Harness::new();
    h.fake.state().create_errors.push_back(unavailable());
    let (applied, _) = h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Created);
    assert_eq!(h.fake.state().creates.len(), 2);
}

#[tokio::test]
async fn ambiguous_update_is_not_repeated_when_applied() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    {
        let mut st = h.fake.state();
        st.apply_then_error = true;
        st.update_errors.push_back(GaxError::timeout("deadline"));
    }
    let (applied, svc) = h.deploy(&spec("img@sha256:2"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    assert_eq!(svc.generation, 2);
    assert_eq!(h.fake.state().updates.len(), 1);
}

#[tokio::test]
async fn stale_etag_conflict_is_retried_after_fresh_read() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    h.fake
        .state()
        .update_errors
        .push_back(status_err(Code::Aborted, "concurrent modification"));
    let (applied, _) = h.deploy(&spec("img@sha256:2"), false).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    assert_eq!(h.fake.state().updates.len(), 2);
}

#[tokio::test]
async fn permission_errors_are_not_retried() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    h.fake.state().update_errors.push_back(status_err(
        Code::PermissionDenied,
        "Permission 'iam.serviceaccounts.actAs' denied on service account rt@p.iam.gserviceaccount.com",
    ));
    let err = h.deploy(&spec("img@sha256:2"), false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Prerequisite);
    assert!(
        err.hints
            .iter()
            .any(|h| h.contains("roles/iam.serviceAccountUser")),
        "{:?}",
        err.hints
    );
    assert_eq!(h.fake.state().updates.len(), 1);
}

#[tokio::test]
async fn unhealthy_revision_reports_diagnostics() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    {
        let mut st = h.fake.state();
        st.fail_rollout = Some("Revision 'hello-dev-00002' is not ready and cannot serve traffic. The user-provided container failed to start and listen on the port defined provided by the PORT=8080 environment variable.".into());
        st.revision_failure = Some("Default STARTUP TCP probe failed 1 time consecutively".into());
    }
    let err = h.deploy(&spec("img@sha256:2"), false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Deploy);
    assert!(
        err.message.contains("failed to start and listen"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("STARTUP TCP probe failed"),
        "{}",
        err.message
    );
    assert!(
        err.message
            .contains("previous ready revision hello-dev-00001"),
        "{}",
        err.message
    );
    assert!(err.hints.iter().any(|h| h.contains("$PORT")));
    assert!(err.hints.iter().any(|h| h.contains("runway logs")));
}

#[tokio::test]
async fn readiness_wait_is_bounded() {
    let h = Harness::new();
    h.fake.state().polls_until_done = u32::MAX;
    let mut r = h.reconciler();
    r.timeout = Duration::from_millis(30);
    let applied = r
        .apply(&target(false), &spec("img@sha256:1"), None)
        .await
        .unwrap();
    let err = r.wait_ready(NAME, &applied).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(err.hints.iter().any(|h| h.contains("runway info")));
}

#[tokio::test]
async fn access_changes_preserve_other_bindings_and_retry_conflicts() {
    let h = Harness::new();
    h.fake.state().policy = Policy::new().set_bindings([Binding::new()
        .set_role("roles/run.invoker")
        .set_members(["serviceAccount:caller@p.iam.gserviceaccount.com"])]);
    h.fake
        .state()
        .iam_errors
        .push_back(status_err(Code::Aborted, "etag"));
    let r = h.reconciler();
    assert_eq!(r.current_public(NAME).await.unwrap(), Some(false));
    assert_eq!(
        r.ensure_access(NAME, true).await.unwrap(),
        AccessChange::MadePublic
    );
    assert_eq!(r.current_public(NAME).await.unwrap(), Some(true));
    {
        let st = h.fake.state();
        assert_eq!(st.iam_sets.len(), 2, "conflict retried once");
        let members = &st.policy.bindings[0].members;
        assert!(members.contains(&"serviceAccount:caller@p.iam.gserviceaccount.com".to_string()));
        assert!(members.contains(&"allUsers".to_string()));
    }
    assert_eq!(
        r.ensure_access(NAME, true).await.unwrap(),
        AccessChange::Unchanged
    );
    assert_eq!(
        r.ensure_access(NAME, false).await.unwrap(),
        AccessChange::MadePrivate
    );
    let st = h.fake.state();
    assert_eq!(
        st.policy.bindings[0].members,
        ["serviceAccount:caller@p.iam.gserviceaccount.com"]
    );
}

#[tokio::test]
async fn forced_apply_creates_new_revision_for_failed_unchanged_service() {
    let h = Harness::new();
    h.deploy(&spec("img@sha256:1"), false).await.unwrap();
    let mut s = spec("img@sha256:1");
    s.revision_annotations
        .insert("runway.dev/redeploy-at".into(), "now".into());
    let r = h.reconciler();
    let existing = r.get(NAME).await.unwrap();
    let mut t = target(false);
    t.force = true;
    let applied = r.apply(&t, &s, existing).await.unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    let st = h.fake.state();
    let tmpl = st.updates[0]
        .service
        .as_ref()
        .unwrap()
        .template
        .as_ref()
        .unwrap();
    assert_eq!(tmpl.annotations["runway.dev/redeploy-at"], "now");
}
