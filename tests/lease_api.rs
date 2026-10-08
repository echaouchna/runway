//! Wire-level tests of the stage lease: a fake Cloud Run service that keeps
//! its state and applies a write only with the current etag (as Cloud Run
//! does), so the compare-and-swap is really exercised.

use google_cloud_auth::credentials::anonymous;
use runway::error::ErrorKind;
use runway::lease::{self, Guard, Lease, Mode, Target, Wait};
use runway::output::Progress;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const NAME: &str = "projects/my-gcp-project/locations/europe-west1/services/shop-prod";

#[derive(Clone)]
struct Fake {
    state: Arc<Mutex<Option<Value>>>,
    writes: Arc<AtomicUsize>,
    /// Writes to answer with a conflict, as if someone wrote first.
    conflicts: Arc<AtomicUsize>,
    /// Writes whose operation completes with an error (nothing applied).
    failures: Arc<AtomicUsize>,
}

impl Fake {
    fn new(service: Option<Value>) -> Fake {
        Fake {
            state: Arc::new(Mutex::new(service)),
            writes: Default::default(),
            conflicts: Default::default(),
            failures: Default::default(),
        }
    }

    fn lease(&self) -> Lease {
        let s = self.state.lock().unwrap();
        Lease::decode(
            s.as_ref()
                .and_then(|v| v["annotations"][lease::ANNOTATION_LEASE].as_str()),
        )
    }
}

impl Respond for Fake {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let mut state = self.state.lock().unwrap();
        let Some(svc) = state.as_mut() else {
            return ResponseTemplate::new(404).set_body_json(
                json!({"error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}}),
            );
        };
        match req.method.as_str() {
            "GET" => ResponseTemplate::new(200).set_body_json(svc.clone()),
            "PATCH" => {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let stale = body["etag"] != svc["etag"];
                if stale || self.conflicts.load(Ordering::SeqCst) > 0 {
                    if !stale {
                        self.conflicts.fetch_sub(1, Ordering::SeqCst);
                        // The other writer's change bumps the etag.
                        svc["etag"] = json!(format!("{}x", svc["etag"].as_str().unwrap()));
                    }
                    return ResponseTemplate::new(409).set_body_json(
                        json!({"error": {"code": 409, "message": "etag mismatch", "status": "ABORTED"}}),
                    );
                }
                if self.failures.load(Ordering::SeqCst) > 0 {
                    self.failures.fetch_sub(1, Ordering::SeqCst);
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "name": "projects/p/locations/l/operations/op", "done": true,
                        "error": {"code": 9, "message": "the revision template is invalid"}
                    }));
                }
                self.writes.fetch_add(1, Ordering::SeqCst);
                svc["annotations"] = body["annotations"].clone();
                let n: u64 = svc["etag"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches('e')
                    .parse()
                    .unwrap_or(0);
                svc["etag"] = json!(format!("e{}", n + 1));
                let mut done = svc.clone();
                done["@type"] = json!("type.googleapis.com/google.cloud.run.v2.Service");
                ResponseTemplate::new(200).set_body_json(json!({
                    "name": "projects/p/locations/l/operations/op", "done": true, "response": done
                }))
            }
            _ => ResponseTemplate::new(405),
        }
    }
}

fn service(labels: Value, annotations: Value) -> Value {
    json!({"name": NAME, "etag": "e1", "labels": labels, "annotations": annotations})
}

fn owned() -> Value {
    json!({"managed-by": "runway", "runway-app": "shop", "runway-stage": "prod"})
}

async fn target(fake: &Fake) -> (MockServer, Target) {
    let s = MockServer::start().await;
    Mock::given(path(format!("/v2/{NAME}")))
        .respond_with(fake.clone())
        .mount(&s)
        .await;
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(s.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    (
        s,
        Target::Service {
            run,
            name: NAME.into(),
        },
    )
}

fn no_wait() -> Wait {
    Wait {
        timeout: None,
        poll: Duration::from_millis(20),
        ..Default::default()
    }
}

async fn take(t: &Target, mode: Mode, wait: Wait) -> runway::error::Result<Guard> {
    lease::acquire(
        t.clone(),
        mode,
        "shop",
        "prod",
        "runway deploy --stage prod",
        wait,
        &Progress::silent(),
    )
    .await
}

#[tokio::test]
async fn an_exclusive_lease_keeps_others_out_until_released() {
    let fake = Fake::new(Some(service(owned(), json!({"team": "a"}))));
    let (_s, t) = target(&fake).await;
    let first = take(&t, Mode::Exclusive, no_wait()).await.unwrap();
    let held = fake.lease();
    assert_eq!(held.holders.len(), 1);
    assert_eq!(held.holders[0].mode, Mode::Exclusive);
    assert!(
        held.holders[0].by.ends_with("runway deploy --stage prod"),
        "{}",
        held.holders[0].by
    );
    assert_eq!(
        fake.state.lock().unwrap().as_ref().unwrap()["annotations"]["team"],
        "a",
        "other annotations are kept"
    );

    for mode in [Mode::Exclusive, Mode::Shared] {
        let e = take(&t, mode, no_wait()).await.err().expect("refused");
        assert_eq!(e.kind, ErrorKind::Conflict);
        assert!(
            e.message
                .contains("stage prod is busy: exclusive lease held by"),
            "{}",
            e.message
        );
        assert!(
            e.hints
                .iter()
                .any(|h| h.contains("runway unlock --stage prod --yes"))
        );
    }

    first.release(&Progress::silent()).await;
    assert!(fake.lease().holders.is_empty());
    assert!(
        fake.state.lock().unwrap().as_ref().unwrap()["annotations"]
            .get(lease::ANNOTATION_LEASE)
            .is_none(),
        "an empty lease is no annotation"
    );
    take(&t, Mode::Exclusive, no_wait()).await.unwrap();
}

#[tokio::test]
async fn previews_share_the_stage_and_a_deploy_waits_for_them() {
    let fake = Fake::new(Some(service(owned(), json!({}))));
    let (_s, t) = target(&fake).await;
    let a = take(&t, Mode::Shared, no_wait()).await.unwrap();
    let b = take(&t, Mode::Shared, no_wait()).await.unwrap();
    assert_eq!(fake.lease().holders.len(), 2, "two previews at once");

    let waited = std::time::Instant::now();
    let e = take(
        &t,
        Mode::Exclusive,
        Wait {
            timeout: Some(Duration::from_millis(300)),
            poll: Duration::from_millis(20),
            ..Default::default()
        },
    )
    .await
    .err()
    .expect("timed out");
    assert!(waited.elapsed() >= Duration::from_millis(300), "it waited");
    assert!(
        e.message.contains("was still busy after waiting"),
        "{}",
        e.message
    );
    assert!(e.message.contains("shared lease held by"), "{}", e.message);

    // The deploy gets the stage as soon as the previews are done.
    let waiting = tokio::spawn({
        let t = t.clone();
        async move {
            take(
                &t,
                Mode::Exclusive,
                Wait {
                    timeout: Some(Duration::from_secs(5)),
                    poll: Duration::from_millis(20),
                    ..Default::default()
                },
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.release(&Progress::silent()).await;
    b.release(&Progress::silent()).await;
    let deploy = waiting.await.unwrap().unwrap();
    assert_eq!(fake.lease().holders.len(), 1);
    assert_eq!(fake.lease().holders[0].mode, Mode::Exclusive);
    deploy.release(&Progress::silent()).await;
}

#[tokio::test]
async fn a_concurrent_write_makes_it_read_again() {
    let fake = Fake::new(Some(service(owned(), json!({}))));
    fake.conflicts.store(2, Ordering::SeqCst);
    let (_s, t) = target(&fake).await;
    let g = take(&t, Mode::Exclusive, no_wait()).await.unwrap();
    assert_eq!(
        fake.writes.load(Ordering::SeqCst),
        1,
        "one write went through, after two conflicts"
    );
    assert_eq!(fake.lease().holders.len(), 1);
    g.release(&Progress::silent()).await;
}

#[tokio::test]
async fn an_expired_lease_is_taken_over() {
    let now = chrono::Utc::now();
    let stale = json!({"holders": [{
        "id": "gone", "mode": "exclusive", "by": "a killed runner",
        "since": (now - chrono::Duration::minutes(10)).to_rfc3339(),
        "until": (now - chrono::Duration::seconds(5)).to_rfc3339()
    }]});
    let fake = Fake::new(Some(service(
        owned(),
        json!({(lease::ANNOTATION_LEASE): stale.to_string()}),
    )));
    let (_s, t) = target(&fake).await;
    let g = take(&t, Mode::Exclusive, no_wait()).await.unwrap();
    let l = fake.lease();
    assert_eq!(l.holders.len(), 1);
    assert_ne!(l.holders[0].id, "gone", "the expired holder is dropped");
    g.release(&Progress::silent()).await;
}

#[tokio::test]
async fn a_service_runway_does_not_own_is_not_locked() {
    let fake = Fake::new(Some(service(json!({"team": "other"}), json!({}))));
    let (_s, t) = target(&fake).await;
    let g = take(&t, Mode::Exclusive, no_wait()).await.unwrap();
    assert_eq!(
        fake.writes.load(Ordering::SeqCst),
        0,
        "nothing written on it"
    );
    assert!(g.annotation().is_none());
    g.release(&Progress::silent()).await;
    assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_first_deploy_creates_the_holder_with_its_lease() {
    let fake = Fake::new(None);
    let (_s, t) = target(&fake).await;
    let g = take(&t, Mode::Exclusive, no_wait()).await.unwrap();
    let (key, value) = g.annotation().expect("pending creation");
    assert_eq!(key, lease::ANNOTATION_LEASE);
    let mine = Lease::decode(Some(&value));
    assert_eq!(mine.holders.len(), 1);
    assert!(
        mine.holders[0].until > chrono::Utc::now(),
        "valid from creation"
    );

    // Another run created the service first, with its own lease: this one stops.
    let theirs = Lease::default()
        .with(
            lease::Holder {
                id: "other".into(),
                mode: Mode::Exclusive,
                by: "another runner".into(),
                since: chrono::Utc::now(),
                until: chrono::Utc::now() + chrono::Duration::minutes(2),
            },
            chrono::Utc::now(),
        )
        .encode();
    let e = lease::created_by_other(Some(&value), theirs.as_deref(), "shop-prod").expect("stops");
    assert_eq!(e.kind, ErrorKind::Conflict);
    assert!(
        e.message.contains("another runway run holds shop-prod"),
        "{}",
        e.message
    );
    g.release(&Progress::silent()).await;
}

#[tokio::test]
async fn unlock_shows_and_removes_the_lease() {
    let fake = Fake::new(Some(service(owned(), json!({}))));
    let (_s, t) = target(&fake).await;
    let g = take(&t, Mode::Exclusive, no_wait()).await.unwrap();
    let shown = t.holders().await.unwrap();
    assert_eq!(shown.len(), 1);
    let removed = t.clear("shop", "prod").await.unwrap();
    assert_eq!(removed, shown);
    assert!(t.holders().await.unwrap().is_empty());
    // The run whose lease was removed releases without harm.
    drop(g);
    take(&t, Mode::Exclusive, no_wait()).await.unwrap();
}

fn other_lease(by: &str) -> String {
    let now = chrono::Utc::now();
    Lease::default()
        .with(
            lease::Holder {
                id: "other".into(),
                mode: Mode::Exclusive,
                by: by.into(),
                since: now,
                until: now + chrono::Duration::minutes(2),
            },
            now,
        )
        .encode()
        .unwrap()
}

#[tokio::test]
async fn a_failed_lease_write_is_not_a_lease() {
    let fake = Fake::new(Some(service(owned(), json!({}))));
    fake.failures.store(1, Ordering::SeqCst);
    let (_s, t) = target(&fake).await;
    let e = take(&t, Mode::Exclusive, no_wait())
        .await
        .err()
        .expect("failed");
    assert!(
        e.message.contains("updating the lease on shop-prod"),
        "{}",
        e.message
    );
    assert!(
        e.message.contains("the revision template is invalid"),
        "{}",
        e.message
    );
    assert!(fake.lease().holders.is_empty(), "nothing stored");
    assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_first_deploy_confirms_its_lease_when_the_holder_appears() {
    let fake = Fake::new(None);
    let (_s, t) = target(&fake).await;
    let mut g = take(&t, Mode::Exclusive, no_wait()).await.unwrap();
    assert!(
        g.annotation().is_some(),
        "pending: the creation carries the lease"
    );
    // Another run created the service meanwhile, and holds it.
    *fake.state.lock().unwrap() = Some(service(
        owned(),
        json!({(lease::ANNOTATION_LEASE): other_lease("another runner")}),
    ));
    let e = g
        .confirm("shop", "prod", no_wait(), &Progress::silent())
        .await
        .expect_err("busy");
    assert!(e.message.contains("stage prod is busy"), "{}", e.message);
    assert_eq!(
        fake.writes.load(Ordering::SeqCst),
        0,
        "their lease is untouched"
    );

    // Once it is free, confirming takes the lease on it.
    *fake.state.lock().unwrap() = Some(service(owned(), json!({})));
    assert!(
        g.confirm("shop", "prod", no_wait(), &Progress::silent())
            .await
            .unwrap(),
        "the holder appeared: what was read before is stale"
    );
    assert_eq!(fake.lease().holders.len(), 1);
    assert!(g.held().check().is_ok());
    g.release(&Progress::silent()).await;
}

#[tokio::test]
async fn a_removed_lease_is_lost_and_never_taken_again() {
    let fake = Fake::new(Some(service(owned(), json!({}))));
    let (_s, t) = target(&fake).await;
    let g = take(
        &t,
        Mode::Exclusive,
        Wait {
            renew: Duration::from_millis(50),
            ..no_wait()
        },
    )
    .await
    .unwrap();
    assert!(g.held().check().is_ok());
    t.clear("shop", "prod").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        fake.lease().holders.is_empty(),
        "the renewal does not undo `unlock`"
    );
    let e = g.held().check().expect_err("lost");
    assert!(e.message.contains("lost its lease"), "{}", e.message);
    let writes = fake.writes.load(Ordering::SeqCst);
    g.release(&Progress::silent()).await;
    assert_eq!(
        fake.writes.load(Ordering::SeqCst),
        writes,
        "releasing a lost lease writes nothing"
    );
}

#[tokio::test]
async fn unlock_refuses_a_holder_runway_does_not_own() {
    let labels = json!({"managed-by": "runway", "runway-app": "other", "runway-stage": "prod"});
    let fake = Fake::new(Some(service(
        labels,
        json!({(lease::ANNOTATION_LEASE): other_lease("another app")}),
    )));
    let (_s, t) = target(&fake).await;
    let e = t.clear("shop", "prod").await.expect_err("refused");
    assert!(
        e.message.contains("not managed by runway for app shop"),
        "{}",
        e.message
    );
    assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fake.lease().holders.len(), 1);
}

#[tokio::test]
async fn a_confirmed_lease_keeps_being_renewed() {
    let fake = Fake::new(None);
    let (_s, t) = target(&fake).await;
    let fast = Wait {
        renew: Duration::from_millis(50),
        ..no_wait()
    };
    let mut g = take(&t, Mode::Exclusive, fast).await.unwrap();
    // Another run creates the holder: the renewal sees no entry of this run
    // and stops.
    *fake.state.lock().unwrap() = Some(service(
        owned(),
        json!({(lease::ANNOTATION_LEASE): other_lease("another runner")}),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    // That run is done: this one takes the lease on the holder.
    *fake.state.lock().unwrap() = Some(service(owned(), json!({})));
    assert!(
        g.confirm("shop", "prod", fast, &Progress::silent())
            .await
            .unwrap()
    );
    let first = fake.lease().holders[0].until;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later = fake.lease().holders[0].until;
    assert!(
        later > first,
        "renewed after confirmation: {first} then {later}"
    );
    assert!(g.held().check().is_ok());
    g.release(&Progress::silent()).await;
}
