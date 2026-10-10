//! Wire-level tests of releases: copying an image between Artifact Registry
//! repositories (registry protocol) and promoting a release candidate.

use runway::config::{self, ReleaseFlag};
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

fn tags_path(project: &str, repo: &str) -> String {
    format!("/v1/projects/{project}/locations/europe-west1/repositories/{repo}/packages/shop/tags")
}

/// `repo`'s package lists these `(tag, digest)` pairs.
async fn serve_tags(s: &MockServer, project: &str, repo: &str, tags: &[(&str, &str)]) {
    let parent =
        format!("projects/{project}/locations/europe-west1/repositories/{repo}/packages/shop");
    let tags: Vec<Value> = tags
        .iter()
        .map(|(t, d)| json!({"name": format!("{parent}/tags/{t}"), "version": format!("{parent}/versions/{d}")}))
        .collect();
    Mock::given(method("GET"))
        .and(path(tags_path(project, repo)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tags": tags})))
        .mount(s)
        .await;
}

async fn artifact_registry(
    s: &MockServer,
) -> google_cloud_artifactregistry_v1::client::ArtifactRegistry {
    let session = runway::gcp::Session::from_static_token("t").unwrap();
    google_cloud_artifactregistry_v1::client::ArtifactRegistry::builder()
        .with_endpoint(s.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_release_finds_the_candidate_first_and_copies_it_when_publishing() {
    let s = MockServer::start().await;
    let (dir, cfg) = release_config(
        "  staging:\n    release: {flag: tag-rc, repository: {repository: candidates}}\n  prod:\n    release: {flag: tag, repository: {project: my-prod-project, repository: releases}}\n",
    );
    assert_eq!(cfg.release_stages(ReleaseFlag::Tag), ["prod"]);
    let prod = config::resolve(&cfg, "prod", &Default::default()).unwrap();
    let (body, digest) = manifest(&sha(b"{}"), &sha(b"layer"));
    serve_tags(&s, "my-prod-project", "releases", &[]).await;
    let older = format!("sha256:{}", "1".repeat(64));
    serve_tags(
        &s,
        "my-gcp-project",
        "candidates",
        &[("1.2.0-RC1", &older), ("1.2.0-RC2", &digest)],
    )
    .await;
    let ar = artifact_registry(&s).await;

    // 1. Found before anything is provisioned: no copy yet.
    let source = runway::commands::release::find_candidate(&ar, &cfg, prod.first(), "1.2.0")
        .await
        .unwrap();
    let (src_repo, dst_repo) = (
        "my-gcp-project/candidates/shop",
        "my-prod-project/releases/shop",
    );
    assert_eq!(
        source,
        format!("europe-west1-docker.pkg.dev/{src_repo}@{digest}"),
        "RC2"
    );
    let reqs = s.received_requests().await.unwrap();
    assert!(
        !reqs.iter().any(|r| r.url.path().starts_with("/v2/")),
        "nothing copied"
    );

    // 2. Published once the repositories exist: copied, tagged, deployed from prod.
    serve_manifest(&s, src_repo, &body, &digest, MANIFEST).await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{dst_repo}/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(tags_path("my-prod-project", "releases")))
        .and(query_param("tagId", "1.2.0"))
        .respond_with(|r: &Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b)
        })
        .expect(1)
        .mount(&s)
        .await;
    let (deployed, tag) = runway::commands::release::publish(
        &ar,
        &client(&s),
        prod.first(),
        &source,
        runway::build::release::ReleaseKind::Release,
        "1.2.0",
        &dir.path().join("CHANGELOG.md"),
    )
    .await
    .unwrap();
    assert_eq!(
        deployed,
        format!("europe-west1-docker.pkg.dev/{dst_repo}@{digest}")
    );
    assert_eq!((tag.tag.as_str(), tag.created), ("1.2.0", true));

    // No candidate of another version: a clear error, nothing built.
    let e = runway::commands::release::find_candidate(&ar, &cfg, prod.first(), "2.0.0")
        .await
        .unwrap_err();
    assert!(
        e.message.contains("no release candidate of 2.0.0"),
        "{}",
        e.message
    );
    assert!(
        e.hints
            .iter()
            .any(|h| h.contains("--tag-rc (stage staging)")),
        "{:?}",
        e.hints
    );
}

#[tokio::test]
async fn a_release_is_published_under_the_package_the_file_names() {
    let s = MockServer::start().await;
    let (dir, cfg) = release_config_with(
        "release:\n  repository:\n    project: plat-artfcs-registry-prod-63a2\n    location: europe-west1\n    repository: docker-releases-plat\n    package: mr-terraform-agent/agent\n",
        "  prod: {}\n",
    );
    assert!(
        !runway::commands::release::promotes(&cfg),
        "no tag-rc stage: --tag builds"
    );
    let prod = config::resolve(&cfg, "prod", &Default::default()).unwrap();
    let (body, digest) = manifest(&sha(b"{}"), &sha(b"layer"));
    let (src_repo, dst_repo) = (
        "my-gcp-project/builds/shop",
        "plat-artfcs-registry-prod-63a2/docker-releases-plat/mr-terraform-agent/agent",
    );
    serve_manifest(&s, src_repo, &body, &digest, MANIFEST).await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("/v2/{dst_repo}/manifests/{digest}")))
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
    let ar = artifact_registry(&s).await;
    let built = format!("europe-west1-docker.pkg.dev/{src_repo}@{digest}");
    let (deployed, tag) = runway::commands::release::publish(
        &ar,
        &client(&s),
        prod.first(),
        &built,
        runway::build::release::ReleaseKind::Release,
        "1.2.0",
        &dir.path().join("CHANGELOG.md"),
    )
    .await
    .unwrap();
    let image = format!("europe-west1-docker.pkg.dev/{dst_repo}");
    assert_eq!(
        deployed,
        format!("{image}@{digest}"),
        "the copy is deployed"
    );
    assert_eq!(tag.image, format!("{image}:1.2.0"));
    assert!(tag.created);
    let reqs = s.received_requests().await.unwrap();
    let post = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let b: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(b["version"], format!("{parent}/versions/{digest}"));
}

#[tokio::test]
async fn candidates_from_several_repositories_must_agree_or_be_chosen() {
    let s = MockServer::start().await;
    let stages = "  eu:\n    release: {flag: tag-rc, repository: {repository: rc-eu}}\n  us:\n    release: {flag: tag-rc, repository: {repository: rc-us}}\n  prod:\n    release: {flag: tag, repository: {repository: releases}}\n";
    let (a, b) = (
        format!("sha256:{}", "a".repeat(64)),
        format!("sha256:{}", "b".repeat(64)),
    );
    // eu's RC5 is older than us's RC1: the numbers do not compare.
    serve_tags(&s, "my-gcp-project", "releases", &[]).await;
    serve_tags(&s, "my-gcp-project", "rc-eu", &[("1.2.0-RC5", &a)]).await;
    serve_tags(&s, "my-gcp-project", "rc-us", &[("1.2.0-RC1", &b)]).await;
    let ar = artifact_registry(&s).await;

    let (_dir, cfg) = release_config(stages);
    let prod = config::resolve(&cfg, "prod", &Default::default()).unwrap();
    let e = runway::commands::release::find_candidate(&ar, &cfg, prod.first(), "1.2.0")
        .await
        .unwrap_err();
    assert!(e.message.contains("different images"), "{}", e.message);
    assert!(e.message.contains("1.2.0-RC5") && e.message.contains("1.2.0-RC1"));
    assert!(
        e.hints
            .iter()
            .any(|h| h.contains("stages.prod.release.from")),
        "{:?}",
        e.hints
    );

    // An explicit source decides.
    let (_dir, cfg) = release_config(&stages.replace("{flag: tag,", "{flag: tag, from: us,"));
    let prod = config::resolve(&cfg, "prod", &Default::default()).unwrap();
    let found = runway::commands::release::find_candidate(&ar, &cfg, prod.first(), "1.2.0")
        .await
        .unwrap();
    assert_eq!(
        found,
        format!("europe-west1-docker.pkg.dev/my-gcp-project/rc-us/shop@{b}")
    );
}

#[tokio::test]
async fn the_same_candidate_in_several_repositories_is_accepted() {
    let s = MockServer::start().await;
    let d = format!("sha256:{}", "c".repeat(64));
    serve_tags(&s, "my-gcp-project", "releases", &[("1.2.0-RC2", &d)]).await;
    serve_tags(&s, "my-gcp-project", "rc-eu", &[("1.2.0-RC7", &d)]).await;
    let ar = artifact_registry(&s).await;
    let (_dir, cfg) = release_config(
        "  eu:\n    release: {flag: tag-rc, repository: {repository: rc-eu}}\n  prod:\n    release: {flag: tag, repository: {repository: releases}}\n",
    );
    let prod = config::resolve(&cfg, "prod", &Default::default()).unwrap();
    let found = runway::commands::release::find_candidate(&ar, &cfg, prod.first(), "1.2.0")
        .await
        .unwrap();
    assert_eq!(
        found,
        format!("europe-west1-docker.pkg.dev/my-gcp-project/releases/shop@{d}"),
        "the copy already in prod's repository"
    );
}
