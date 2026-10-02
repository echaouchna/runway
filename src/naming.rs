//! Deterministic resource names derived from `app`, `stage` and provider settings.
//!
//! These functions are the single source of truth for every name runway
//! creates or looks up, so repeated deployments converge on the same resources.

use std::collections::BTreeMap;

/// Label marking resources managed by runway.
pub const LABEL_MANAGED_BY: &str = "managed-by";
pub const LABEL_MANAGED_BY_VALUE: &str = "runway";
pub const LABEL_APP: &str = "runway-app";
pub const LABEL_STAGE: &str = "runway-stage";

/// Annotation recording the human-readable image reference (tag) that was resolved.
pub const ANNOTATION_IMAGE_REF: &str = "runway.dev/image-ref";
/// Annotation recording the source archive hash for source deployments.
pub const ANNOTATION_SOURCE_HASH: &str = "runway.dev/source-sha256";
/// Annotation recording the release tag (from the changelog) of the deployed image.
pub const ANNOTATION_RELEASE: &str = "runway.dev/release";
/// Annotation recording the base images (with digests) of the deployed image.
pub const ANNOTATION_BASE_IMAGES: &str = "runway.dev/base-images";

/// Maximum length of a Cloud Run service ID.
pub const MAX_SERVICE_NAME_LEN: usize = 49;

/// Cloud Run service ID: `{app}-{stage}`.
pub fn service_id(app: &str, stage: &str) -> String {
    format!("{app}-{stage}")
}

pub fn location_parent(project: &str, region: &str) -> String {
    format!("projects/{project}/locations/{region}")
}

pub fn service_name(project: &str, region: &str, app: &str, stage: &str) -> String {
    format!(
        "{}/services/{}",
        location_parent(project, region),
        service_id(app, stage)
    )
}

/// Ownership labels applied to the service and its revisions.
pub fn ownership_labels(app: &str, stage: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            LABEL_MANAGED_BY.to_string(),
            LABEL_MANAGED_BY_VALUE.to_string(),
        ),
        (LABEL_APP.to_string(), app.to_string()),
        (LABEL_STAGE.to_string(), stage.to_string()),
    ])
}

/// Short form of a source hash used in tags.
pub fn short_hash(source_sha256: &str) -> &str {
    &source_sha256[..source_sha256.len().min(16)]
}

/// Image repository (without tag) for source builds:
/// `{location}-docker.pkg.dev/{project}/{repository}/{app}`.
pub fn build_image_name(location: &str, project: &str, repository: &str, app: &str) -> String {
    format!("{location}-docker.pkg.dev/{project}/{repository}/{app}")
}

/// Content-addressed image tag for a source archive.
pub fn build_image_tag(source_sha256: &str) -> String {
    format!("src-{}", short_hash(source_sha256))
}

/// Object name of the uploaded source archive.
pub fn source_object(app: &str, source_sha256: &str) -> String {
    format!("runway/{app}/source-{source_sha256}.tar.gz")
}

/// Cloud Build tags used to find builds again (idempotency, diagnostics).
pub fn build_tags(app: &str, stage: &str, source_sha256: &str) -> Vec<String> {
    vec![
        "runway".to_string(),
        format!("runway-app-{app}"),
        format!("runway-stage-{stage}"),
        source_build_tag(source_sha256),
    ]
}

pub fn source_build_tag(source_sha256: &str) -> String {
    format!("runway-src-{}", short_hash(source_sha256))
}

/// IAM resource name of a service account. The project is spelled out when
/// the email reveals it: with the `projects/-` wildcard IAM cannot tell which
/// project to check permissions on for a missing account, and answers
/// PERMISSION_DENIED ("or it may not exist") instead of NOT_FOUND.
pub fn service_account_resource(email: &str) -> String {
    let project = email
        .split_once('@')
        .and_then(|(_, domain)| domain.strip_suffix(".iam.gserviceaccount.com"))
        .filter(|p| !p.is_empty() && !p.contains('.'))
        .unwrap_or("-");
    format!("projects/{project}/serviceAccounts/{email}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_account_resource_names_the_project() {
        assert_eq!(
            service_account_resource("run@my-proj.iam.gserviceaccount.com"),
            "projects/my-proj/serviceAccounts/run@my-proj.iam.gserviceaccount.com"
        );
        // Default accounts: the project is not in the domain.
        assert_eq!(
            service_account_resource("123-compute@developer.gserviceaccount.com"),
            "projects/-/serviceAccounts/123-compute@developer.gserviceaccount.com"
        );
        assert_eq!(
            service_account_resource("p@appspot.gserviceaccount.com"),
            "projects/-/serviceAccounts/p@appspot.gserviceaccount.com"
        );
    }

    #[test]
    fn names_are_deterministic() {
        assert_eq!(service_id("hello-api", "dev"), "hello-api-dev");
        assert_eq!(
            service_name("p1", "europe-west1", "hello-api", "prod"),
            "projects/p1/locations/europe-west1/services/hello-api-prod"
        );
        let h = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(build_image_tag(h), "src-0123456789abcdef");
        assert_eq!(
            build_image_name("europe-west1", "p1", "apps", "hello-api"),
            "europe-west1-docker.pkg.dev/p1/apps/hello-api"
        );
        assert_eq!(
            source_object("hello-api", h),
            format!("runway/hello-api/source-{h}.tar.gz")
        );
        assert!(build_tags("a", "dev", h).contains(&"runway-src-0123456789abcdef".to_string()));
    }
}
