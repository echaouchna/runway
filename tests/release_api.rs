//! Wire-level tests of releases: copying an image between Artifact Registry
//! repositories (registry protocol), promotions (what a stage serves, with
//! its provenance) and publication.

use runway::build::release::ReleaseKind;
use runway::commands::release;
use runway::config;
use runway::gcp::registry::RegistryClient;
use runway::image_ref::ImageRef;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

fn sha(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// An image manifest with a config and one layer; returns (bytes, digest).
fn manifest(config: &str, layer: &str) -> (Vec<u8>, String) {
    let body = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config, "size": 2},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer, "size": 5}]
    }))
    .unwrap();
    let d = sha(&body);
    (body, d)
}

async fn serve_manifest(s: &MockServer, repo: &str, body: &[u8], digest: &str, media: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", media)
                .set_body_bytes(body.to_vec()),
        )
        .mount(s)
        .await;
}

fn client(s: &MockServer) -> RegistryClient {
    RegistryClient::new(reqwest::Client::new(), Some("t".into())).with_base_url(s.uri())
}

#[tokio::test]
async fn an_image_is_copied_by_mounting_its_layers_and_keeps_its_digest() {
    let s = MockServer::start().await;
    let (src_repo, dst_repo) = (
        "my-gcp-project/builds/shop",
        "my-prod-project/releases/shop",
    );
    let (cfg_d, layer_d) = (sha(b"{}"), sha(b"layer"));
    let (body, digest) = manifest(&cfg_d, &layer_d);
    serve_manifest(&s, src_repo, &body, &digest, MANIFEST).await;
    // The config is already there; the layer is mounted from the source.
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{dst_repo}/blobs/{cfg_d}")))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/{dst_repo}/blobs/{layer_d}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v2/{dst_repo}/blobs/uploads/")))
        .and(query_param("mount", layer_d.as_str()))
        .and(query_param("from", src_repo))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{dst_repo}/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    let src = ImageRef::parse(&format!("europe-west1-docker.pkg.dev/{src_repo}@{digest}")).unwrap();
    let dst = ImageRef::parse(&format!("europe-west1-docker.pkg.dev/{dst_repo}")).unwrap();
    client(&s).copy(&src, &dst).await.unwrap();

    let put = s
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.method.as_str() == "PUT")
        .unwrap();
    assert_eq!(put.body, body, "the same bytes: the same digest");
    assert_eq!(put.headers["content-type"], MANIFEST);
    assert_eq!(put.headers["authorization"], "Bearer t");
}

#[tokio::test]
async fn across_locations_layers_are_streamed_and_indexes_copy_every_platform() {
    let s = MockServer::start().await;
    let (src_repo, dst_repo) = (
        "my-gcp-project/builds/shop",
        "my-prod-project/releases/shop",
    );
    let (cfg_d, layer_d) = (sha(b"{}"), sha(b"layer"));
    let (child, child_d) = manifest(&cfg_d, &layer_d);
    let index = serde_json::to_vec(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{"mediaType": MANIFEST, "digest": child_d, "size": child.len(),
                       "platform": {"architecture": "arm64", "os": "linux"}}]
    }))
    .unwrap();
    let index_d = sha(&index);
    serve_manifest(
        &s,
        src_repo,
        &index,
        &index_d,
        "application/vnd.oci.image.index.v1+json",
    )
    .await;
    serve_manifest(&s, src_repo, &child, &child_d, MANIFEST).await;
    for (d, bytes) in [(&cfg_d, b"{}".as_slice()), (&layer_d, b"layer".as_slice())] {
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/{dst_repo}/blobs/{d}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/{src_repo}/blobs/{d}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .mount(&s)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(format!("/v2/{dst_repo}/blobs/uploads/")))
        .respond_with(
            ResponseTemplate::new(202)
                .insert_header("location", format!("/v2/{dst_repo}/blobs/uploads/u1")),
        )
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{dst_repo}/blobs/uploads/u1")))
        .respond_with(ResponseTemplate::new(201))
        .expect(2)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{dst_repo}/manifests/{child_d}")))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{dst_repo}/manifests/{index_d}")))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    // Another registry host: no mount, the layers go through runway.
    let src = ImageRef::parse(&format!("us-docker.pkg.dev/{src_repo}@{index_d}")).unwrap();
    let dst = ImageRef::parse(&format!("europe-west1-docker.pkg.dev/{dst_repo}")).unwrap();
    client(&s).copy(&src, &dst).await.unwrap();

    let reqs = s.received_requests().await.unwrap();
    assert!(
        !reqs
            .iter()
            .any(|r| r.url.query().unwrap_or("").contains("mount=")),
        "no mount across hosts"
    );
    let uploads: Vec<(String, Vec<u8>)> = reqs
        .iter()
        .filter(|r| r.url.path().ends_with("/uploads/u1"))
        .map(|r| (r.url.query().unwrap_or("").to_string(), r.body.clone()))
        .collect();
    assert!(
        uploads.contains(&(format!("digest={layer_d}"), b"layer".to_vec())),
        "{uploads:?}"
    );
    // The platform manifest is pushed before the index that lists it.
    let pos = |d: &str| {
        reqs.iter()
            .position(|r| r.method.as_str() == "PUT" && r.url.path().ends_with(d))
            .unwrap()
    };
    assert!(pos(&child_d) < pos(&index_d));
}

fn release_config(stages: &str) -> (tempfile::TempDir, config::LoadedConfig) {
    release_config_with("", stages)
}

/// `top` goes before `stages:` (a global `release` block).
fn release_config_with(top: &str, stages: &str) -> (tempfile::TempDir, config::LoadedConfig) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(dir.path().join("CHANGELOG.md"), "## [1.2.0]\n").unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        format!(
            r#"
version: 1
app: shop
provider:
  project: my-gcp-project
  region: europe-west1
  artifact_repository: builds
  source_bucket: my-gcp-build-sources
  build_service_account: builds@my-gcp-project.iam.gserviceaccount.com
service:
  source: .
  service_account: rt@my-gcp-project.iam.gserviceaccount.com
{top}stages:
{stages}"#
        ),
    )
    .unwrap();
    let cfg = config::load(&p).unwrap();
    (dir, cfg)
}

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

fn image(repo: &str, d: &str) -> String {
    format!("europe-west1-docker.pkg.dev/{repo}@{d}")
}

/// `project/repo/package`'s `(tag, digest)` pairs.
async fn serve_tags(s: &MockServer, package: &str, tags: &[(&str, &str)]) {
    let mut parts = package.splitn(3, '/');
    let (project, repo, pkg) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let parent = format!(
        "projects/{project}/locations/europe-west1/repositories/{repo}/packages/{}",
        pkg.replace('/', "%2F")
    );
    let tags: Vec<Value> = tags
        .iter()
        .map(|(t, d)| json!({"name": format!("{parent}/tags/{t}"), "version": format!("{parent}/versions/{d}")}))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/v1/{parent}/tags")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tags": tags})))
        .mount(s)
        .await;
}

/// One revision of a service: its image and what it carries.
struct Rev<'a> {
    name: &'a str,
    image: String,
    commit: Option<&'a str>,
    release: Option<&'a str>,
}

/// Service `id` of `stage` (owned by runway), with these revisions, the
/// last one being the latest ready, serving this traffic (`(revision,
/// percent)`), settled.
async fn serve_service(
    s: &MockServer,
    id: &str,
    stage: &str,
    revs: &[Rev<'_>],
    traffic: &[(&str, i32)],
) {
    serve_service_with(s, id, stage, revs, traffic, traffic, false).await
}

/// [`serve_service`] with the traffic asked for (`traffic`) apart from the
/// traffic Cloud Run reports serving (`trafficStatuses`), and `reconciling`.
async fn serve_service_with(
    s: &MockServer,
    id: &str,
    stage: &str,
    revs: &[Rev<'_>],
    intended: &[(&str, i32)],
    observed: &[(&str, i32)],
    reconciling: bool,
) {
    let svc = format!("projects/my-gcp-project/locations/europe-west1/services/{id}");
    let latest = revs.last().unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/v2/{svc}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": svc,
            "labels": {"managed-by": "runway", "runway-app": "shop", "runway-stage": stage},
            // The template is the latest deploy's (a preview's, possibly).
            "template": {"containers": [{"image": latest.image, "ports": [{"containerPort": 8080}]}]},
            "latestReadyRevision": format!("{svc}/revisions/{}", latest.name),
            "reconciling": reconciling,
            "traffic": intended.iter().map(|(r, p)| json!({
                "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION",
                "revision": r,
                "percent": p
            })).collect::<Vec<_>>(),
            "trafficStatuses": observed.iter().map(|(r, p)| json!({
                "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION",
                "revision": r,
                "percent": p
            })).collect::<Vec<_>>()
        })))
        .mount(s)
        .await;
    for r in revs {
        let mut annotations = serde_json::Map::new();
        if let Some(c) = r.commit {
            annotations.insert(
                "runway.dev/source".into(),
                json!(json!({"commit": c, "time": "2026-10-01T10:00:00Z"}).to_string()),
            );
        }
        if let Some(t) = r.release {
            annotations.insert("runway.dev/release".into(), json!(t));
        }
        Mock::given(method("GET"))
            .and(path(format!("/v2/{svc}/revisions/{}", r.name)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": format!("{svc}/revisions/{}", r.name),
                "annotations": annotations,
                "containers": [{"image": r.image, "ports": [{"containerPort": 8080}]}]
            })))
            .mount(s)
            .await;
    }
}

async fn not_found(s: &MockServer, id: &str) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/projects/my-gcp-project/locations/europe-west1/services/{id}"
        )))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": {"code": 404, "message": "not found", "status": "NOT_FOUND"}
        })))
        .mount(s)
        .await;
}

struct Clients {
    ar: google_cloud_artifactregistry_v1::client::ArtifactRegistry,
    services: google_cloud_run_v2::client::Services,
    revisions: google_cloud_run_v2::client::Revisions,
    jobs: google_cloud_run_v2::client::Jobs,
}

impl Clients {
    async fn new(s: &MockServer) -> Self {
        let session = runway::gcp::Session::from_static_token("t").unwrap();
        let c = || session.credentials.clone();
        Self {
            ar: google_cloud_artifactregistry_v1::client::ArtifactRegistry::builder()
                .with_endpoint(s.uri())
                .with_credentials(c())
                .build()
                .await
                .unwrap(),
            services: google_cloud_run_v2::client::Services::builder()
                .with_endpoint(s.uri())
                .with_credentials(c())
                .build()
                .await
                .unwrap(),
            revisions: google_cloud_run_v2::client::Revisions::builder()
                .with_endpoint(s.uri())
                .with_credentials(c())
                .build()
                .await
                .unwrap(),
            jobs: google_cloud_run_v2::client::Jobs::builder()
                .with_endpoint(s.uri())
                .with_credentials(c())
                .build()
                .await
                .unwrap(),
        }
    }
    fn readers(&self) -> release::Readers<'_> {
        release::Readers {
            ar: &self.ar,
            services: &self.services,
            revisions: &self.revisions,
            jobs: &self.jobs,
        }
    }
}

fn source(commit: &str) -> runway::source::Source {
    runway::source::Source {
        commit: commit.into(),
        time: "2026-10-01T10:00:00Z".parse().unwrap(),
        dirty: false,
    }
}

async fn resolve(
    c: &Clients,
    cfg: &config::LoadedConfig,
    stage: &str,
    version: Option<(ReleaseKind, &str)>,
    ours: Option<&runway::source::Source>,
) -> runway::error::Result<release::Resolution> {
    let r = config::resolve(cfg, stage, &Default::default()).unwrap();
    release::resolve(
        cfg,
        r.first(),
        Some(&c.readers()),
        None,
        &release::Request { version, ours },
        &mut Vec::new(),
    )
    .await
}

const PROMOTING: &str = "  dev: {}\n  uat:\n    promote: {from: dev}\n    provider: {artifact_repository: uat-builds}\n  prod:\n    promote: {from: uat}\n    release: {repository: {project: my-prod-project, repository: releases}}\n";

#[tokio::test]
async fn a_promoted_stage_runs_a_copy_of_what_its_source_serves() {
    let s = MockServer::start().await;
    let (body, d) = manifest(&sha(b"{}"), &sha(b"layer"));
    let dev_image = image("my-gcp-project/builds/shop", &d);
    serve_service(
        &s,
        "shop-dev",
        "dev",
        &[Rev {
            name: "shop-dev-00001",
            image: dev_image.clone(),
            commit: Some("abc123"),
            release: None,
        }],
        &[("shop-dev-00001", 100)],
    )
    .await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;

    let res = resolve(&c, &cfg, "uat", None, Some(&source("fff999")))
        .await
        .unwrap();
    let p = res.promoted.as_ref().expect("uat promotes");
    assert_eq!(p.image, dev_image);
    assert_eq!(p.why, "what stage dev serves");
    // The commit is the image's, not the checkout's (fff999).
    assert_eq!(res.commit, release::Commit::Recorded(source("abc123")));
    assert_eq!(res.provenance(None).source, Some(source("abc123")));
    // plan and deploy agree on the reference deployed: uat's own copy.
    let copy = image("my-gcp-project/uat-builds/shop", &d);
    assert_eq!(res.deployed(&dev_image), copy);
    assert!(
        matches!(&res.planned_image(), runway::plan::ImagePlan::Pinned { reference, .. } if *reference == copy)
    );
    assert_eq!(
        res.ops,
        [release::Op::Copy {
            from: dev_image.clone(),
            to: "europe-west1-docker.pkg.dev/my-gcp-project/uat-builds/shop".into()
        }]
    );

    // publish carries it out: the copy, no tag.
    serve_manifest(&s, "my-gcp-project/builds/shop", &body, &d, MANIFEST).await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "/v2/my-gcp-project/uat-builds/shop/manifests/{d}"
        )))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    let (deployed, tag) = release::publish(&c.ar, &client(&s), &res, &dev_image, None)
        .await
        .unwrap();
    assert_eq!(deployed, copy);
    assert!(tag.is_none());

    // dev builds: nothing promoted.
    let dev = resolve(&c, &cfg, "dev", None, Some(&source("abc123")))
        .await
        .unwrap();
    assert!(dev.promoted.is_none());
    assert_eq!(
        dev.commit,
        release::Commit::Checkout(Some(source("abc123")))
    );
}

/// Review finding 2: a zero-traffic preview is not what a stage serves.
#[tokio::test]
async fn the_serving_revision_is_promoted_not_a_preview() {
    let s = MockServer::start().await;
    let (stable, preview) = (digest('a'), digest('b'));
    serve_service(
        &s,
        "shop-dev",
        "dev",
        &[
            Rev {
                name: "shop-dev-00001",
                image: image("my-gcp-project/builds/shop", &stable),
                commit: Some("abc"),
                release: None,
            },
            Rev {
                name: "shop-dev-00002",
                image: image("my-gcp-project/builds/shop", &preview),
                commit: Some("def"),
                release: None,
            },
        ],
        &[("shop-dev-00001", 100), ("shop-dev-00002", 0)],
    )
    .await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;
    let res = resolve(&c, &cfg, "uat", None, None).await.unwrap();
    assert!(
        res.promoted.unwrap().image.ends_with(&stable),
        "the stable revision"
    );
}

#[tokio::test]
async fn a_split_source_is_not_guessed() {
    let s = MockServer::start().await;
    serve_service(
        &s,
        "shop-dev",
        "dev",
        &[
            Rev {
                name: "shop-dev-00001",
                image: image("my-gcp-project/builds/shop", &digest('a')),
                commit: None,
                release: None,
            },
            Rev {
                name: "shop-dev-00002",
                image: image("my-gcp-project/builds/shop", &digest('b')),
                commit: None,
                release: None,
            },
        ],
        &[("shop-dev-00001", 90), ("shop-dev-00002", 10)],
    )
    .await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;
    let e = resolve(&c, &cfg, "uat", None, None).await.err().unwrap();
    assert!(
        e.message
            .contains("splits shop-dev's traffic (shop-dev-00001 90%, shop-dev-00002 10%)"),
        "{}",
        e.message
    );
    assert!(
        e.hints
            .iter()
            .any(|h| h.contains("runway traffic --stage dev --promote")),
        "{:?}",
        e.hints
    );
}

/// Review finding 1: with a version, the candidate the source *serves*, not
/// the latest one of a repository shared with other stages.
#[tokio::test]
async fn a_tagged_promotion_takes_what_the_source_serves() {
    let s = MockServer::start().await;
    let (rc1, rc2) = (digest('1'), digest('2'));
    // uat serves RC1; dev has since published RC2 in the same repository.
    serve_service(
        &s,
        "shop-uat",
        "uat",
        &[Rev {
            name: "shop-uat-00001",
            image: image("my-gcp-project/uat-builds/shop", &rc1),
            commit: Some("abc"),
            release: Some("1.2.0-RC1"),
        }],
        &[("shop-uat-00001", 100)],
    )
    .await;
    serve_tags(
        &s,
        "my-gcp-project/uat-builds/shop",
        &[("1.2.0-RC1", &rc1), ("1.2.0-RC2", &rc2)],
    )
    .await;
    serve_tags(&s, "my-prod-project/releases/shop", &[]).await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;

    let res = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        Some(&source("zzz")),
    )
    .await
    .unwrap();
    let p = res.promoted.as_ref().unwrap();
    assert!(p.image.ends_with(&rc1), "what uat serves: {}", p.image);
    assert_eq!(p.why, "1.2.0-RC1, which stage uat serves");
    assert_eq!(
        res.destination.as_ref().unwrap().image(),
        "europe-west1-docker.pkg.dev/my-prod-project/releases/shop"
    );
    assert!(
        res.ops
            .iter()
            .any(|o| matches!(o, release::Op::Tag { tag, .. } if tag == "1.2.0"))
    );
    // A release is not "carried": it is the tag this deploy publishes.
    assert_eq!(
        res.provenance(Some("1.2.0")).release.as_deref(),
        Some("1.2.0")
    );
}

#[tokio::test]
async fn without_a_candidate_only_the_same_commit_is_released() {
    let s = MockServer::start().await;
    let live = digest('c');
    serve_service(
        &s,
        "shop-uat",
        "uat",
        &[Rev {
            name: "shop-uat-00003",
            image: image("my-gcp-project/uat-builds/shop", &live),
            commit: Some("abc123"),
            release: None,
        }],
        &[("shop-uat-00003", 100)],
    )
    .await;
    serve_tags(&s, "my-gcp-project/uat-builds/shop", &[]).await;
    serve_tags(&s, "my-prod-project/releases/shop", &[]).await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;
    let v = Some((ReleaseKind::Release, "1.2.0"));

    let res = resolve(&c, &cfg, "prod", v, Some(&source("abc123")))
        .await
        .unwrap();
    assert!(
        res.promoted
            .as_ref()
            .unwrap()
            .why
            .contains("built from this commit")
    );
    assert_eq!(res.commit, release::Commit::Recorded(source("abc123")));

    let e = resolve(&c, &cfg, "prod", v, Some(&source("fff999")))
        .await
        .err()
        .unwrap();
    assert!(
        e.message.contains("no image to release as 1.2.0")
            && e.message.contains("commit abc123")
            && e.message.contains("fff999"),
        "{}",
        e.message
    );
    assert!(
        e.hints
            .iter()
            .any(|h| h.contains("deploy stage uat with --tag-rc")),
        "{:?}",
        e.hints
    );
}

/// Review finding 3: an image whose commit is unknown is not given the
/// checkout's.
#[tokio::test]
async fn an_unknown_image_commit_stays_unknown() {
    let s = MockServer::start().await;
    let rc = digest('d');
    serve_service(
        &s,
        "shop-uat",
        "uat",
        &[Rev {
            name: "shop-uat-00001",
            image: image("my-gcp-project/uat-builds/shop", &rc),
            commit: None,
            release: Some("1.2.0-RC4"),
        }],
        &[("shop-uat-00001", 100)],
    )
    .await;
    serve_tags(&s, "my-prod-project/releases/shop", &[]).await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;
    let res = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        Some(&source("newer")),
    )
    .await
    .unwrap();
    assert_eq!(
        res.commit,
        release::Commit::Unknown,
        "not the checkout's commit"
    );
    assert_eq!(res.provenance(Some("1.2.0")).source, None);
}

/// Re-running a release keeps the version this stage serves already.
#[tokio::test]
async fn a_release_rerun_keeps_what_the_stage_serves() {
    let s = MockServer::start().await;
    let (published, newer) = (digest('1'), digest('2'));
    serve_tags(
        &s,
        "my-prod-project/releases/shop",
        &[("1.2.0", &published)],
    )
    .await;
    serve_service(
        &s,
        "shop-prod",
        "prod",
        &[Rev {
            name: "shop-prod-00004",
            image: image("my-prod-project/releases/shop", &published),
            commit: Some("a"),
            release: Some("1.2.0"),
        }],
        &[("shop-prod-00004", 100)],
    )
    .await;
    // uat moved on to RC2 since.
    serve_service(
        &s,
        "shop-uat",
        "uat",
        &[Rev {
            name: "shop-uat-00002",
            image: image("my-gcp-project/uat-builds/shop", &newer),
            commit: Some("b"),
            release: Some("1.2.0-RC2"),
        }],
        &[("shop-uat-00002", 100)],
    )
    .await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;
    let res = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        None,
    )
    .await
    .unwrap();
    let p = res.promoted.unwrap();
    assert_eq!(p.image, image("my-prod-project/releases/shop", &published));
    assert_eq!(p.why, "release 1.2.0, which stage prod already serves");
    assert_eq!(res.commit, release::Commit::Recorded(source("a")));
}

/// Review finding: a version another stage published in a shared release
/// repository is not this stage's evidence.
#[tokio::test]
async fn a_version_published_by_another_stage_is_not_trusted() {
    let s = MockServer::start().await;
    let (dev_build, uat_rc) = (digest('7'), digest('8'));
    // dev published 1.2.0 in the shared repository; prod never deployed.
    serve_tags(
        &s,
        "my-prod-project/releases/shop",
        &[("1.2.0", &dev_build)],
    )
    .await;
    not_found(&s, "shop-prod").await;
    serve_service(
        &s,
        "shop-uat",
        "uat",
        &[Rev {
            name: "shop-uat-00001",
            image: image("my-gcp-project/uat-builds/shop", &uat_rc),
            commit: Some("u"),
            release: Some("1.2.0-RC1"),
        }],
        &[("shop-uat-00001", 100)],
    )
    .await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;
    let e = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(
        e.message.contains("already published")
            && e.message.contains("stage prod does not serve it")
            && e.message.contains("1.2.0-RC1, which stage uat serves"),
        "{}",
        e.message
    );
    assert!(e.permanent);

    // The same image as uat's candidate: released (tagging keeps the tag).
    let s = MockServer::start().await;
    serve_tags(&s, "my-prod-project/releases/shop", &[("1.2.0", &uat_rc)]).await;
    not_found(&s, "shop-prod").await;
    serve_service(
        &s,
        "shop-uat",
        "uat",
        &[Rev {
            name: "shop-uat-00001",
            image: image("my-gcp-project/uat-builds/shop", &uat_rc),
            commit: Some("u"),
            release: Some("1.2.0-RC1"),
        }],
        &[("shop-uat-00001", 100)],
    )
    .await;
    let c = Clients::new(&s).await;
    let res = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        res.promoted.unwrap().why,
        "1.2.0-RC1, which stage uat serves"
    );
}

/// Review finding: what Cloud Run reports serving, not the traffic it was
/// asked for; a rollout in progress is waited for.
#[tokio::test]
async fn observed_traffic_decides_and_a_rollout_is_waited_for() {
    let (old, pending) = (digest('a'), digest('b'));
    let revs = |s: &'static str| {
        [
            Rev {
                name: "shop-dev-00001",
                image: image("my-gcp-project/builds/shop", &old),
                commit: Some(s),
                release: None,
            },
            Rev {
                name: "shop-dev-00002",
                image: image("my-gcp-project/builds/shop", &pending),
                commit: Some(s),
                release: None,
            },
        ]
    };
    let (_dir, cfg) = release_config(PROMOTING);

    // Asked to move to 00002, still serving 00001: 00001 is what dev serves.
    let s = MockServer::start().await;
    serve_service_with(
        &s,
        "shop-dev",
        "dev",
        &revs("x"),
        &[("shop-dev-00002", 100)],
        &[("shop-dev-00001", 100)],
        false,
    )
    .await;
    let c = Clients::new(&s).await;
    let res = resolve(&c, &cfg, "uat", None, None).await.unwrap();
    assert!(
        res.promoted.unwrap().image.ends_with(&old),
        "the revision serving"
    );

    // Reconciling: not settled; the error is retried (waits), not permanent.
    let s = MockServer::start().await;
    serve_service_with(
        &s,
        "shop-dev",
        "dev",
        &revs("x"),
        &[("shop-dev-00002", 100)],
        &[("shop-dev-00001", 100)],
        true,
    )
    .await;
    let c = Clients::new(&s).await;
    let e = resolve(&c, &cfg, "uat", None, None).await.err().unwrap();
    assert!(e.message.contains("still rolling out"), "{}", e.message);
    assert!(runway::retry::is_retryable(&e), "{e:?}");
}

#[tokio::test]
async fn nothing_is_promoted_from_a_stage_that_is_not_deployed() {
    let s = MockServer::start().await;
    not_found(&s, "shop-dev").await;
    let (_dir, cfg) = release_config(PROMOTING);
    let c = Clients::new(&s).await;
    let e = resolve(&c, &cfg, "uat", None, None).await.err().unwrap();
    assert!(e.message.contains("is not deployed"), "{}", e.message);
    assert!(
        e.hints.iter().any(|h| h == "deploy stage dev first"),
        "{:?}",
        e.hints
    );
}

/// The alias of older files: `--tag` on a `tag` stage releases what the
/// `tag-rc` stage serves.
#[tokio::test]
async fn a_tag_stage_releases_what_the_candidate_stage_serves() {
    let s = MockServer::start().await;
    let (rc, other) = (digest('5'), digest('6'));
    serve_service(
        &s,
        "shop-staging",
        "staging",
        &[Rev {
            name: "shop-staging-00001",
            image: image("my-gcp-project/candidates/shop", &rc),
            commit: None,
            release: None,
        }],
        &[("shop-staging-00001", 100)],
    )
    .await;
    // Revisions deployed before they carried their release: the tags of the
    // served digest in its repository are the evidence (RC3 is not served).
    serve_tags(
        &s,
        "my-gcp-project/candidates/shop",
        &[("1.2.0-RC2", &rc), ("1.2.0-RC3", &other)],
    )
    .await;
    serve_tags(&s, "my-prod-project/releases/shop", &[]).await;
    let (_dir, cfg) = release_config(
        "  staging:\n    release: {flag: tag-rc, repository: {repository: candidates}}\n  prod:\n    release: {flag: tag, repository: {project: my-prod-project, repository: releases}}\n",
    );
    let c = Clients::new(&s).await;
    let res = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        None,
    )
    .await
    .unwrap();
    let p = res.promoted.unwrap();
    assert!(p.image.ends_with(&rc));
    assert_eq!(p.why, "1.2.0-RC2, which stage staging serves");

    // Without --tag, the tag stage builds, as before.
    let res = resolve(&c, &cfg, "prod", None, None).await.unwrap();
    assert!(res.promoted.is_none());
}

/// `--tag` publishes any image: a configured one needs a release repository.
#[tokio::test]
async fn a_configured_image_is_published_to_the_release_repository() {
    let s = MockServer::start().await;
    let c = Clients::new(&s).await;
    let d = digest('e');
    let (_dir, cfg) = release_config(&format!(
        "  prod:\n    service: {{image: \"europe-west1-docker.pkg.dev/my-gcp-project/vendor/tool@{d}\"}}\n"
    ));
    let e = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(
        e.hints.iter().any(|h| h.contains("release.repository")),
        "{:?}",
        e.hints
    );

    let (_dir, cfg) = release_config_with(
        "release:\n  repository: {repository: releases, package: tools/tool}\n",
        &format!(
            "  prod:\n    service: {{image: \"europe-west1-docker.pkg.dev/my-gcp-project/vendor/tool@{d}\"}}\n"
        ),
    );
    let res = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        res.ops,
        [
            release::Op::Copy {
                from: "europe-west1-docker.pkg.dev/my-gcp-project/vendor/tool".into(),
                to: "europe-west1-docker.pkg.dev/my-gcp-project/releases/tools/tool".into()
            },
            release::Op::Tag {
                image: "europe-west1-docker.pkg.dev/my-gcp-project/releases/tools/tool".into(),
                tag: "1.2.0".into()
            }
        ]
    );
    assert_eq!(
        res.provenance(None).source,
        None,
        "its commit is not this checkout's"
    );
}

#[tokio::test]
async fn a_release_is_published_under_the_package_the_file_names() {
    let s = MockServer::start().await;
    let (dir, cfg) = release_config_with(
        "release:\n  repository:\n    project: plat-artfcs-registry-prod-63a2\n    location: europe-west1\n    repository: docker-releases-plat\n    package: mr-terraform-agent/agent\n",
        "  prod: {}\n",
    );
    assert!(!release::promotes(&cfg), "no tag-rc stage: --tag builds");
    let (body, d) = manifest(&sha(b"{}"), &sha(b"layer"));
    let (src_repo, dst_repo) = (
        "my-gcp-project/builds/shop",
        "plat-artfcs-registry-prod-63a2/docker-releases-plat/mr-terraform-agent/agent",
    );
    serve_manifest(&s, src_repo, &body, &d, MANIFEST).await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{dst_repo}/manifests/{d}")))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    // The resource name escapes the nested path; the image name does not.
    let parent = "projects/plat-artfcs-registry-prod-63a2/locations/europe-west1/repositories/docker-releases-plat/packages/mr-terraform-agent%2Fagent";
    Mock::given(method("GET"))
        .and(path(format!("/v1/{parent}/tags")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/{parent}/tags")))
        .and(query_param("tagId", "1.2.0"))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b)
        })
        .expect(1)
        .mount(&s)
        .await;
    let c = Clients::new(&s).await;
    let res = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        None,
    )
    .await
    .unwrap();
    assert!(res.promoted.is_none(), "a build");
    let built = format!("europe-west1-docker.pkg.dev/{src_repo}@{d}");
    let (deployed, tag) = release::publish(
        &c.ar,
        &client(&s),
        &res,
        &built,
        Some((
            ReleaseKind::Release,
            "1.2.0",
            &dir.path().join("CHANGELOG.md"),
        )),
    )
    .await
    .unwrap();
    let image = format!("europe-west1-docker.pkg.dev/{dst_repo}");
    assert_eq!(deployed, format!("{image}@{d}"), "the copy is deployed");
    let tag = tag.unwrap();
    assert_eq!(tag.image, format!("{image}:1.2.0"));
    assert!(tag.created);
    let reqs = s.received_requests().await.unwrap();
    let post = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let b: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(b["version"], format!("{parent}/versions/{d}"));
}

/// Review finding: each promoted workload takes what its own counterpart
/// serves, even when it would share a build in the promoting stage.
#[tokio::test]
async fn each_workload_is_promoted_from_its_own_counterpart() {
    let s = MockServer::start().await;
    let (web, tool) = (digest('a'), digest('b'));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::create_dir(dir.path().join("tools")).unwrap();
    std::fs::write(dir.path().join("tools/Dockerfile"), "FROM scratch\n").unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        "version: 1\napp: shop\nprovider:\n  project: my-gcp-project\n  region: europe-west1\n  artifact_repository: builds\n  source_bucket: my-gcp-build-sources\n  build_service_account: builds@my-gcp-project.iam.gserviceaccount.com\ndefaults:\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\nservice:\n  source: .\njobs:\n  migrate:\n    source: .\nstages:\n  dev:\n    jobs:\n      migrate: {source: tools}\n  uat:\n    promote: {from: dev}\n",
    )
    .unwrap();
    let cfg = config::load(&p).unwrap();
    serve_service(
        &s,
        "shop-dev",
        "dev",
        &[Rev {
            name: "shop-dev-00001",
            image: image("my-gcp-project/builds/shop", &web),
            commit: None,
            release: None,
        }],
        &[("shop-dev-00001", 100)],
    )
    .await;
    let job = "projects/my-gcp-project/locations/europe-west1/jobs/shop-migrate-dev";
    Mock::given(method("GET"))
        .and(path(format!("/v2/{job}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": job,
            "labels": {"managed-by": "runway", "runway-app": "shop", "runway-stage": "dev"},
            "template": {"template": {"containers": [{"image": image("my-gcp-project/builds/shop-migrate", &tool)}]}}
        })))
        .mount(&s)
        .await;
    let c = Clients::new(&s).await;
    let uat = config::resolve(&cfg, "uat", &Default::default()).unwrap();
    let mut got = Vec::new();
    for d in &uat.deployments {
        let owner = uat.build_owner(d);
        let res = release::resolve(
            &cfg,
            owner,
            Some(&c.readers()),
            None,
            &release::Request::default(),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        got.push((d.what(), res.promoted.unwrap().image));
    }
    assert_eq!(
        got,
        [
            (
                "service".to_string(),
                image("my-gcp-project/builds/shop", &web)
            ),
            (
                "job migrate".to_string(),
                image("my-gcp-project/builds/shop-migrate", &tool)
            ),
        ]
    );
}

fn resolution_to(destination: &str, d: &str) -> release::Resolution {
    let t = release::Target::of_image(destination).unwrap();
    release::Resolution {
        decision: runway::commands::plan::ImageDecision {
            image: runway::plan::ImagePlan::Pinned {
                reference: image("my-gcp-project/builds/x", d),
                digest: d.into(),
                origin: "test".into(),
            },
            build: None,
            source: None,
            annotations: Default::default(),
        },
        promoted: None,
        destination: Some(t),
        commit: release::Commit::Unknown,
        image_commit: None,
        carried_release: None,
        ops: Vec::new(),
        notes: Vec::new(),
    }
}

#[test]
fn a_release_names_one_image_per_destination() {
    let dest = "europe-west1-docker.pkg.dev/my-prod-project/releases/shop";
    let (a, b) = (digest('a'), digest('b'));
    let (ra, rb) = (resolution_to(dest, &a), resolution_to(dest, &b));
    let (ia, ib) = (
        image("my-gcp-project/builds/x", &a),
        image("my-gcp-project/builds/x", &b),
    );
    let items = [
        ("service".to_string(), &ra, ia.as_str()),
        ("job migrate".to_string(), &rb, ib.as_str()),
    ];
    let e = release::check_destinations(&items, true).unwrap_err();
    assert!(
        e.message
            .contains("service and job migrate are released to the same image"),
        "{}",
        e.message
    );
    // Without a tag, both copies can live in the package; the same image twice is fine.
    release::check_destinations(&items, false).unwrap();
    let same = [
        ("service".to_string(), &ra, ia.as_str()),
        ("job migrate".to_string(), &ra, ia.as_str()),
    ];
    release::check_destinations(&same, true).unwrap();
}

/// Service `id` of `stage` with this revision history (`(revision, ready)`,
/// oldest first); the last Ready one serves all the traffic.
async fn serve_history(s: &MockServer, id: &str, stage: &str, revs: &[(Rev<'_>, bool)]) {
    let svc = format!("projects/my-gcp-project/locations/europe-west1/services/{id}");
    let serving = revs
        .iter()
        .rev()
        .find(|(_, ready)| *ready)
        .map(|(r, _)| r.name)
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/v2/{svc}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": svc,
            "labels": {"managed-by": "runway", "runway-app": "shop", "runway-stage": stage},
            "latestReadyRevision": format!("{svc}/revisions/{serving}"),
            "trafficStatuses": [{"type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": serving, "percent": 100}]
        })))
        .mount(s)
        .await;
    let list: Vec<Value> = revs
        .iter()
        .map(|(r, ready)| {
            let mut annotations = serde_json::Map::new();
            if let Some(c) = r.commit {
                annotations.insert(
                    "runway.dev/source".into(),
                    json!(json!({"commit": c, "time": "2026-10-01T10:00:00Z"}).to_string()),
                );
            }
            if let Some(t) = r.release {
                annotations.insert("runway.dev/release".into(), json!(t));
            }
            json!({
                "name": format!("{svc}/revisions/{}", r.name),
                "annotations": annotations,
                "containers": [{"image": r.image, "ports": [{"containerPort": 8080}]}],
                "conditions": [{
                    "type": "Ready",
                    "state": if *ready { "CONDITION_SUCCEEDED" } else { "CONDITION_FAILED" }
                }]
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/v2/{svc}/revisions")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"revisions": list})))
        .mount(s)
        .await;
}

const BY_COMMIT: &str = "  dev: {}\n  uat:\n    promote: {from: dev}\n    provider: {artifact_repository: uat-builds}\n  prod:\n    promote: {from: uat, commit: checkout}\n    release: {repository: {project: my-prod-project, repository: releases}}\n";

fn uat_rev(
    name: &'static str,
    d: &str,
    commit: &'static str,
    release: Option<&'static str>,
) -> Rev<'static> {
    Rev {
        name,
        image: image("my-gcp-project/uat-builds/shop", d),
        commit: Some(commit),
        release,
    }
}

/// On a git tag, prod takes the image uat ran built from that exact commit,
/// even after uat moved on.
#[tokio::test]
async fn the_image_of_the_tagged_commit_is_found_in_the_history() {
    let s = MockServer::start().await;
    let (a, b) = (digest('a'), digest('b'));
    serve_history(
        &s,
        "shop-uat",
        "uat",
        &[
            (uat_rev("shop-uat-00001", &a, "aaaa", None), true),
            (uat_rev("shop-uat-00002", &b, "bbbb", None), true),
        ],
    )
    .await;
    serve_tags(&s, "my-prod-project/releases/shop", &[]).await;
    let (_dir, cfg) = release_config(BY_COMMIT);
    let c = Clients::new(&s).await;
    for version in [None, Some((ReleaseKind::Release, "1.2.0"))] {
        let res = resolve(&c, &cfg, "prod", version, Some(&source("aaaa")))
            .await
            .unwrap();
        let p = res.promoted.unwrap();
        assert!(p.image.ends_with(&a), "{version:?}: {}", p.image);
        assert_eq!(
            p.why,
            "commit aaaa, which stage uat ran (revision shop-uat-00001)"
        );
        assert_eq!(res.commit, release::Commit::Recorded(source("aaaa")));
    }
}

/// Review finding: a matching release version never stands in for the commit.
#[tokio::test]
async fn a_matching_release_does_not_replace_the_commit() {
    let s = MockServer::start().await;
    let b = digest('b');
    serve_history(
        &s,
        "shop-uat",
        "uat",
        &[(
            uat_rev("shop-uat-00002", &b, "bbbb", Some("1.2.0-RC1")),
            true,
        )],
    )
    .await;
    serve_tags(&s, "my-prod-project/releases/shop", &[]).await;
    let (_dir, cfg) = release_config(BY_COMMIT);
    let c = Clients::new(&s).await;
    let e = resolve(
        &c,
        &cfg,
        "prod",
        Some((ReleaseKind::Release, "1.2.0")),
        Some(&source("aaaa")),
    )
    .await
    .err()
    .unwrap();
    assert!(
        e.message
            .contains("has not run an image built from commit aaaa"),
        "{}",
        e.message
    );
    assert!(
        e.hints
            .iter()
            .any(|h| h.contains("deploy commit aaaa to stage uat first")),
        "{:?}",
        e.hints
    );
    assert!(e.permanent);
}

#[tokio::test]
async fn only_ready_revisions_count_and_ambiguity_is_refused() {
    let (a1, a2, b) = (digest('1'), digest('2'), digest('b'));
    let (_dir, cfg) = release_config(BY_COMMIT);

    // A revision of the commit that never became Ready is not evidence.
    let s = MockServer::start().await;
    serve_history(
        &s,
        "shop-uat",
        "uat",
        &[
            (uat_rev("shop-uat-00001", &a1, "aaaa", None), false),
            (uat_rev("shop-uat-00002", &b, "bbbb", None), true),
        ],
    )
    .await;
    let c = Clients::new(&s).await;
    let e = resolve(&c, &cfg, "prod", None, Some(&source("aaaa")))
        .await
        .err()
        .unwrap();
    assert!(
        e.message.contains("none of its 1 Ready revision(s)"),
        "{}",
        e.message
    );

    // Two different images built from the commit: not guessed.
    let s = MockServer::start().await;
    serve_history(
        &s,
        "shop-uat",
        "uat",
        &[
            (uat_rev("shop-uat-00001", &a1, "aaaa", None), true),
            (uat_rev("shop-uat-00002", &a2, "aaaa", None), true),
        ],
    )
    .await;
    let c = Clients::new(&s).await;
    let e = resolve(&c, &cfg, "prod", None, Some(&source("aaaa")))
        .await
        .err()
        .unwrap();
    assert!(
        e.message
            .contains("ran several images built from commit aaaa"),
        "{}",
        e.message
    );

    // Without this checkout's commit, nothing is matched.
    let e = resolve(&c, &cfg, "prod", None, None).await.err().unwrap();
    assert!(
        e.message.contains("this checkout's commit is unknown"),
        "{}",
        e.message
    );
}
