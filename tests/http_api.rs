//! Wire-level tests: real SDK clients talk to a local mock HTTP server, so the
//! REST paths, query parameters and JSON bodies runway produces are verified
//! exactly as Google Cloud would receive them.

use async_trait::async_trait;
use google_cloud_auth::credentials::anonymous;
use runway::build::cloudbuild::{BuildInputs, Builder, SourceUploader};
use runway::build::package::{self, CompressedArchive};
use runway::config::{BuildConfig, SecretRef};
use runway::deploy::{Reconciler, ServiceChange, Target};
use runway::gcp::registry::{DigestResolver, ResolveError};
use runway::image_ref::ImageRef;
use runway::naming;
use runway::output::Progress;
use runway::plan::ServiceSpec;
use runway::poll::PollConfig;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HASH: &str = "5ce484b4bc34d08c7375ef268d8301111099d83296b98acae6b1c75fab70500c";
const DIGEST: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

struct NoImage;
#[async_trait]
impl DigestResolver for NoImage {
    async fn resolve(&self, _i: &ImageRef) -> Result<Option<String>, ResolveError> {
        Ok(None)
    }
}

struct Uploaded;
#[async_trait]
impl SourceUploader for Uploaded {
    async fn upload(
        &self,
        _b: &str,
        _o: &str,
        _a: &CompressedArchive,
    ) -> runway::error::Result<i64> {
        Ok(1700000000000001)
    }
}

fn body(req: &wiremock::Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

#[tokio::test]
async fn cloud_build_uses_regional_endpoints_and_expected_body() {
    let server = MockServer::start().await;
    let tagged = "europe-west1-docker.pkg.dev/p/apps/hello:src-5ce484b4bc34d08c";
    Mock::given(method("GET"))
        .and(path("/v1/projects/p/locations/europe-west1/builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/p/locations/europe-west1/builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "projects/p/locations/europe-west1/operations/build-op",
            "metadata": {
                "@type": "type.googleapis.com/google.devtools.cloudbuild.v1.BuildOperationMetadata",
                "build": {"id": "b-42", "status": "QUEUED"}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/projects/p/locations/europe-west1/builds/b-42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "b-42",
            "projectId": "p",
            "status": "SUCCESS",
            "logUrl": "https://console.cloud.google.com/cloud-build/builds;region=europe-west1/b-42",
            "results": {"images": [{"name": tagged, "digest": DIGEST}]}
        })))
        .mount(&server)
        .await;

    let client = google_cloud_build_v1::client::CloudBuild::builder()
        .with_endpoint(server.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    let cfg = BuildConfig {
        context_dir: ".".into(),
        strategy: runway::config::BuildStrategy::Dockerfile {
            path: "Dockerfile".into(),
        },
        artifact_location: "europe-west1".into(),
        artifact_project: "p".into(),
        artifact_repository: "apps".into(),
        artifact_package: None,
        source_bucket: "src-bucket".into(),
        build_service_account: "builds@p.iam.gserviceaccount.com".into(),
        excluded: vec![],
        create_resources: false,
        rebuild_always: false,
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Dockerfile"),
        "FROM scratch\nCOPY main.py /\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("main.py"), "print('hello')\n").unwrap();
    let source = package::scan(
        dir.path(),
        &runway::config::BuildStrategy::Dockerfile {
            path: "Dockerfile".into(),
        },
        &[],
    )
    .unwrap();
    assert_eq!(source.sha256, HASH);
    let progress = Progress::silent();
    let out = Builder {
        cloudbuild: &client,
        uploader: &Uploaded,
        resolver: &NoImage,
        logging: None,
        progress: &progress,
        poll: PollConfig::fast(),
    }
    .build(&BuildInputs {
        project: "p",
        region: "europe-west1",
        app: "hello",
        stage: "dev",
        config: &cfg,
        source: &source,
        image_checked: false,
        timeout: Duration::from_secs(900),
        force: false,
    })
    .await
    .unwrap();
    assert_eq!(out.digest, DIGEST);
    assert_eq!(out.build_id.as_deref(), Some("b-42"));

    let reqs = server.received_requests().await.unwrap();
    let create = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let b = body(create);
    assert_eq!(b["source"]["storageSource"]["bucket"], "src-bucket");
    assert_eq!(
        b["source"]["storageSource"]["object"],
        format!("runway/hello/source-{HASH}.tar.gz")
    );
    assert_eq!(
        b["source"]["storageSource"]["generation"],
        "1700000000000001"
    );
    assert_eq!(b["steps"][0]["name"], "gcr.io/cloud-builders/docker");
    assert_eq!(b["images"][0], tagged);
    assert_eq!(
        b["serviceAccount"],
        "projects/p/serviceAccounts/builds@p.iam.gserviceaccount.com"
    );
    // Enums are sent as their proto numbers (valid in the JSON mapping).
    use google_cloud_build_v1::model::build_options::LoggingMode;
    assert_eq!(
        b["options"]["logging"],
        json!(LoggingMode::CloudLoggingOnly.value().unwrap())
    );
    assert_eq!(b["timeout"], "900s");
    assert!(
        b["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("runway-src-5ce484b4bc34d08c"))
    );
}

fn spec() -> ServiceSpec {
    ServiceSpec {
        image: format!("europe-west1-docker.pkg.dev/p/apps/hello@{DIGEST}"),
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
        billing: runway::config::BILLING_REQUEST.into(),
        startup_cpu_boost: false,
        execution_environment: None,
        sandbox: false,
        command: Vec::new(),
        args: Vec::new(),
        vpc: None,
        cloud_sql: Vec::new(),
        custom_audiences: Vec::new(),
        annotations: BTreeMap::new(),
        revision_annotations: BTreeMap::new(),
        provenance: Default::default(),
        traffic: Default::default(),
    }
}

#[tokio::test]
async fn cloud_run_update_sends_mask_etag_and_template() {
    let server = MockServer::start().await;
    let name = "projects/p/locations/europe-west1/services/hello-dev";
    let svc_path = format!("/v2/{name}");
    let ready = json!({
        "name": name,
        "uri": "https://hello-dev-xyz-ew.a.run.app",
        "generation": "3",
        "observedGeneration": "3",
        "etag": "\"etag-3\"",
        "labels": {"managed-by": "runway", "runway-app": "hello", "runway-stage": "dev", "team": "a"},
        "latestReadyRevision": format!("{name}/revisions/hello-dev-00003-abc"),
        "latestCreatedRevision": format!("{name}/revisions/hello-dev-00003-abc"),
        "terminalCondition": {"type": "Ready", "state": "CONDITION_SUCCEEDED"},
        "template": {"containers": [{"image": "old@sha256:1"}]}
    });
    Mock::given(method("GET"))
        .and(path(svc_path.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(ready.clone()))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(svc_path.clone()))
        .and(query_param(
            "updateMask",
            "labels,annotations,client,clientVersion,customAudiences,ingress,invokerIamDisabled,iapEnabled,launchStage,template,traffic",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "projects/p/locations/europe-west1/operations/op-1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/v2/projects/p/locations/europe-west1/operations/op-1",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "projects/p/locations/europe-west1/operations/op-1",
            "done": true
        })))
        .mount(&server)
        .await;

    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    let progress = Progress::silent();
    let rec = Reconciler {
        run: &run,
        revisions: None,
        progress: &progress,
        poll: PollConfig::fast(),
        timeout: Duration::from_secs(5),
    };
    let existing = rec.get(name).await.unwrap();
    let applied = rec
        .apply(
            &Target {
                parent: "projects/p/locations/europe-west1",
                service_id: "hello-dev",
                name,
                app: "hello",
                stage: "dev",
                adopt: false,
                force: false,
            },
            &spec(),
            existing,
        )
        .await
        .unwrap();
    assert_eq!(applied.change, ServiceChange::Updated);
    // The mock keeps returning generation 3, so the fallback target is 4;
    // relax it to exercise the wait loop against the mock.
    let applied = runway::deploy::Applied {
        target_generation: 3,
        ..applied
    };
    let svc = rec.wait_ready(name, &applied).await.unwrap();
    assert_eq!(svc.uri, "https://hello-dev-xyz-ew.a.run.app");

    let reqs = server.received_requests().await.unwrap();
    let patch = reqs.iter().find(|r| r.method.as_str() == "PATCH").unwrap();
    let b = body(patch);
    assert_eq!(b["name"], name);
    assert_eq!(b["etag"], "\"etag-3\"");
    assert_eq!(b["labels"]["team"], "a", "foreign labels preserved");
    assert_eq!(b["labels"]["managed-by"], "runway");
    use google_cloud_run_v2::model::{IngressTraffic, TrafficTargetAllocationType};
    assert_eq!(b["ingress"], json!(IngressTraffic::All.value().unwrap()));
    assert_eq!(
        b["traffic"][0]["type"],
        json!(TrafficTargetAllocationType::Latest.value().unwrap())
    );
    assert_eq!(b["traffic"][0]["percent"], 100);
    let t = &b["template"];
    assert_eq!(t["serviceAccount"], "rt@p.iam.gserviceaccount.com");
    assert_eq!(t["timeout"], "60s");
    assert_eq!(t["maxInstanceRequestConcurrency"], 80);
    assert_eq!(t["scaling"]["maxInstanceCount"], 2);
    let c = &t["containers"][0];
    assert_eq!(
        c["image"],
        format!("europe-west1-docker.pkg.dev/p/apps/hello@{DIGEST}")
    );
    assert_eq!(c["ports"][0]["containerPort"], 8080);
    assert_eq!(c["resources"]["limits"]["memory"], "512Mi");
    // Explicit: with `resources` set, an absent cpuIdle means instance-based billing.
    assert_eq!(c["resources"]["cpuIdle"], true);
    let env = c["env"].as_array().unwrap();
    assert!(env.contains(&json!({"name": "LOG_LEVEL", "value": "info"})));
    assert!(env.contains(&json!({
        "name": "DATABASE_URL",
        "valueSource": {"secretKeyRef": {"secret": "database-url", "version": "1"}}
    })));
}

#[tokio::test]
async fn cloud_run_create_uses_service_id_and_parent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/v2/projects/p/locations/europe-west1/services/hello-dev",
        ))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/projects/p/locations/europe-west1/services"))
        .and(query_param("serviceId", "hello-dev"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "operations/op-c"})))
        .expect(1)
        .mount(&server)
        .await;
    let run = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(anonymous::Builder::new().build())
        .build()
        .await
        .unwrap();
    let progress = Progress::silent();
    let rec = Reconciler {
        run: &run,
        revisions: None,
        progress: &progress,
        poll: PollConfig::fast(),
        timeout: Duration::from_secs(5),
    };
    let name = "projects/p/locations/europe-west1/services/hello-dev";
    assert!(rec.get(name).await.unwrap().is_none());
    let mut spec = spec();
    spec.billing = runway::config::BILLING_INSTANCE.into();
    spec.startup_cpu_boost = true;
    spec.execution_environment = Some("gen2".into());
    spec.vpc = Some(runway::config::VpcConfig {
        network: "projects/host-project/global/networks/shared".into(),
        subnet: "projects/host-project/regions/europe-west1/subnetworks/run".into(),
        egress: "all-traffic".into(),
        network_tags: vec!["run-egress".into()],
    });
    spec.cloud_sql = vec!["p:europe-west1:db".into()];
    spec.custom_audiences = vec!["https://api.example.com".into()];
    spec.sandbox = true;
    let applied = rec
        .apply(
            &Target {
                parent: "projects/p/locations/europe-west1",
                service_id: "hello-dev",
                name,
                app: "hello",
                stage: "dev",
                adopt: false,
                force: false,
            },
            &spec,
            None,
        )
        .await
        .unwrap();
    assert_eq!(applied.change, ServiceChange::Created);
    let reqs = server.received_requests().await.unwrap();
    let post = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let b = body(post);
    assert!(
        b.get("name").is_none() || b["name"] == "",
        "create sends no resource name"
    );
    assert_eq!(b["labels"]["runway-stage"], "dev");
    use google_cloud_run_v2::model::{ExecutionEnvironment, vpc_access::VpcEgress};
    assert_eq!(b["customAudiences"], json!(["https://api.example.com"]));
    let t = &b["template"];
    assert_eq!(
        t["executionEnvironment"],
        json!(ExecutionEnvironment::Gen2.value().unwrap())
    );
    assert_eq!(
        t["vpcAccess"]["egress"],
        json!(VpcEgress::AllTraffic.value().unwrap())
    );
    assert_eq!(
        t["vpcAccess"]["networkInterfaces"],
        json!([{
            "network": "projects/host-project/global/networks/shared",
            "subnetwork": "projects/host-project/regions/europe-west1/subnetworks/run",
            "tags": ["run-egress"]
        }])
    );
    assert!(t["volumes"].as_array().unwrap().contains(
        &json!({"name": "cloudsql", "cloudSqlInstance": {"instances": ["p:europe-west1:db"]}})
    ));
    let c = &t["containers"][0];
    assert!(
        c["volumeMounts"]
            .as_array()
            .unwrap()
            .contains(&json!({"name": "cloudsql", "mountPath": "/cloudsql"}))
    );
    // Instance-based: cpuIdle false is the proto default, so it is omitted.
    assert!(c["resources"].get("cpuIdle").is_none_or(|v| v == false));
    assert_eq!(c["resources"]["startupCpuBoost"], true);
    // Not in the SDK yet: sent through its unknown-field passthrough.
    assert_eq!(c["sandboxLauncher"], true);
    assert_eq!(
        b["launchStage"],
        json!(google_cloud_api::model::LaunchStage::Beta.value().unwrap())
    );
}

/// A revision as Cloud Run reports it, created from `spec`'s template.
fn revision_json(name: &str, spec: &ServiceSpec) -> Value {
    let t = runway::gcp::run::desired_service(spec, name, None)
        .template
        .unwrap();
    serde_json::to_value(
        google_cloud_run_v2::model::Revision::new()
            .set_name(name)
            .set_containers(t.containers)
            .set_volumes(t.volumes)
            .set_service_account(t.service_account)
            .set_max_instance_request_concurrency(t.max_instance_request_concurrency)
            .set_or_clear_timeout(t.timeout)
            .set_or_clear_scaling(t.scaling)
            .set_annotations(t.annotations),
    )
    .unwrap()
}

fn marked(spec: &ServiceSpec, tag: &str) -> ServiceSpec {
    use runway::traffic::Mode;
    let mut s = spec
        .clone()
        .with_traffic_mode(Mode::Preview { tag: tag.into() });
    s.revision_annotations
        .insert("runway.dev/preview".into(), tag.into());
    s
}

#[tokio::test]
async fn a_revision_already_serving_the_configuration_is_kept() {
    use google_cloud_run_v2::model::{
        Condition, TrafficTarget, TrafficTargetAllocationType, condition,
    };
    use runway::gcp::run;
    let server = MockServer::start().await;
    let name = "projects/p/locations/europe-west1/services/hello-dev";
    let rev = |n: &str| format!("{name}/revisions/hello-dev-0000{n}");
    let prod = spec();
    let feat_a = marked(&prod, "feat-a");
    let mut feat_b = marked(&prod, "feat-b");
    feat_b.image = "europe-west1-docker.pkg.dev/p/apps/hello@sha256:bbbb".into();

    // Production on -00001, feat-a on -00002; the latest revision (the
    // service template) is feat-b's, with other code.
    let target = |t: TrafficTargetAllocationType, r: &str, pct: i32, tag: &str| {
        TrafficTarget::new()
            .set_type(t)
            .set_revision(r)
            .set_percent(pct)
            .set_tag(tag)
    };
    let live = run::desired_service(&feat_b, name, None)
        .set_name(name)
        .set_etag("\"etag-5\"")
        .set_generation(5)
        .set_observed_generation(5)
        .set_uri("https://hello-dev-xyz-ew.a.run.app")
        .set_latest_ready_revision(rev("3"))
        .set_latest_created_revision(rev("3"))
        .set_terminal_condition(
            Condition::new()
                .set_type("Ready")
                .set_state(condition::State::ConditionSucceeded),
        )
        .set_traffic([
            target(TrafficTargetAllocationType::Revision, &rev("1"), 100, ""),
            target(
                TrafficTargetAllocationType::Revision,
                &rev("2"),
                0,
                "feat-a",
            ),
            target(TrafficTargetAllocationType::Latest, "", 0, "feat-b"),
        ]);
    Mock::given(method("GET"))
        .and(path(format!("/v2/{name}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::to_value(&live).unwrap()),
        )
        .mount(&server)
        .await;
    for (n, s) in [("1", &prod), ("2", &feat_a)] {
        Mock::given(method("GET"))
            .and(path(format!("/v2/{}", rev(n))))
            .respond_with(ResponseTemplate::new(200).set_body_json(revision_json(&rev(n), s)))
            .mount(&server)
            .await;
    }
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let creds = anonymous::Builder::new().build();
    let services = google_cloud_run_v2::client::Services::builder()
        .with_endpoint(server.uri())
        .with_credentials(creds.clone())
        .build()
        .await
        .unwrap();
    let revisions = google_cloud_run_v2::client::Revisions::builder()
        .with_endpoint(server.uri())
        .with_credentials(creds)
        .build()
        .await
        .unwrap();
    let progress = Progress::silent();
    let rec = Reconciler {
        run: &services,
        revisions: Some(&revisions),
        progress: &progress,
        poll: PollConfig::fast(),
        timeout: Duration::from_secs(5),
    };
    let t = Target {
        parent: "projects/p/locations/europe-west1",
        service_id: "hello-dev",
        name,
        app: "hello",
        stage: "dev",
        adopt: false,
        force: false,
    };
    let live = rec.get(name).await.unwrap().unwrap();

    // The main deploy (production code) and feat-a again (same code): the
    // revisions behind those URLs are kept, nothing is written.
    for (desired, expected) in [(&prod, "hello-dev-00001"), (&feat_a, "hello-dev-00002")] {
        let mut s = run::spec_for_live(desired, Some(&live));
        let (target, revision) = rec
            .matching_revision(&live, &s)
            .await
            .unwrap_or_else(|| panic!("{expected} runs this configuration"));
        assert_eq!(revision, expected);
        s.traffic.serve = Some(target);
        let applied = rec.apply(&t, &s, Some(live.clone())).await.unwrap();
        assert_eq!(applied.change, ServiceChange::Unchanged, "{expected}");
    }

    // feat-a with new code: its revision does not match.
    let mut changed = feat_a.clone();
    changed.env.insert("NEW".into(), "1".into());
    let s = run::spec_for_live(&changed, Some(&live));
    assert_eq!(rec.matching_revision(&live, &s).await, None);
}
