//! Release tags from the changelog against a mocked Artifact Registry.

use runway::build::release::{Package, ReleaseKind, apply};
use runway::gcp::Session;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PKG: &str = "projects/p/locations/europe-west1/repositories/runway/packages/gcptree";

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

async fn registry(
    existing: &[(&str, char)],
) -> (
    MockServer,
    google_cloud_artifactregistry_v1::client::ArtifactRegistry,
) {
    let s = MockServer::start().await;
    let tags: Vec<Value> = existing
        .iter()
        .map(|(t, d)| json!({"name": format!("{PKG}/tags/{t}"), "version": format!("{PKG}/versions/{}", digest(*d))}))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/v1/{PKG}/tags")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tags": tags })))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/{PKG}/tags")))
        .respond_with(|r: &wiremock::Request| {
            let b: Value = serde_json::from_slice(&r.body).unwrap();
            ResponseTemplate::new(200).set_body_json(b)
        })
        .mount(&s)
        .await;
    let session = Session::from_static_token("t").unwrap();
    let ar = google_cloud_artifactregistry_v1::client::ArtifactRegistry::builder()
        .with_endpoint(s.uri())
        .with_credentials(session.credentials.clone())
        .build()
        .await
        .unwrap();
    (s, ar)
}

fn pkg() -> Package<'static> {
    Package {
        project: "p",
        location: "europe-west1",
        repository: "runway",
        package: "gcptree",
    }
}

async fn posted(s: &MockServer) -> Vec<(String, Value)> {
    s.received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| {
            let tag = r
                .url
                .query_pairs()
                .find(|(k, _)| k == "tagId")
                .map(|(_, v)| v.to_string())
                .unwrap();
            (tag, serde_json::from_slice(&r.body).unwrap())
        })
        .collect()
}

const EXISTING: &[(&str, char)] = &[
    ("src-1234", 'a'),
    ("1.4.2-RC1", 'b'),
    ("1.4.2-RC2", 'c'),
    ("1.4.1", 'e'),
];

#[tokio::test]
async fn next_release_candidate_is_created() {
    let (s, ar) = registry(EXISTING).await;
    let cl = std::path::Path::new("CHANGELOG.md");
    let r = apply(
        &ar,
        &pkg(),
        &digest('d'),
        ReleaseKind::Candidate,
        "1.4.2",
        cl,
    )
    .await
    .unwrap();
    assert_eq!(r.tag, "1.4.2-RC3");
    assert!(r.created);
    assert_eq!(
        r.image,
        "europe-west1-docker.pkg.dev/p/runway/gcptree:1.4.2-RC3"
    );
    let p = posted(&s).await;
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].0, "1.4.2-RC3");
    assert_eq!(p[0].1["version"], format!("{PKG}/versions/{}", digest('d')));
}

#[tokio::test]
async fn same_image_reuses_its_candidate_tag() {
    let (s, ar) = registry(EXISTING).await;
    let r = apply(
        &ar,
        &pkg(),
        &digest('c'),
        ReleaseKind::Candidate,
        "1.4.2",
        "CHANGELOG.md".as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(r.tag, "1.4.2-RC2");
    assert!(!r.created);
    assert!(
        posted(&s).await.is_empty(),
        "no new RC for an image that already has one"
    );
}

#[tokio::test]
async fn release_tag_is_created_once_and_never_moved() {
    let (s, ar) = registry(EXISTING).await;
    let r = apply(
        &ar,
        &pkg(),
        &digest('d'),
        ReleaseKind::Release,
        "1.4.2",
        "CHANGELOG.md".as_ref(),
    )
    .await
    .unwrap();
    assert_eq!((r.tag.as_str(), r.created), ("1.4.2", true));
    assert_eq!(posted(&s).await[0].0, "1.4.2");

    // 1.4.1 already tags image `e`: same image is a no-op, another image is refused.
    let (s, ar) = registry(EXISTING).await;
    let r = apply(
        &ar,
        &pkg(),
        &digest('e'),
        ReleaseKind::Release,
        "1.4.1",
        "CHANGELOG.md".as_ref(),
    )
    .await
    .unwrap();
    assert!(!r.created);
    let err = apply(
        &ar,
        &pkg(),
        &digest('f'),
        ReleaseKind::Release,
        "1.4.1",
        "CHANGELOG.md".as_ref(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, runway::error::ErrorKind::Conflict);
    assert!(err.permanent, "not retried");
    assert!(err.message.contains("already published"));
    assert!(posted(&s).await.is_empty());
}
