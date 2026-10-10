//! Release tags read from a changelog: `deploy --tag` tags the image with the
//! latest changelog version; `deploy --tag-rc` with `<version>-RC<n>`, where
//! `n` follows the highest existing release-candidate tag for that version.
//!
//! Idempotent: re-running with the same image reuses its tag. A release tag
//! already pointing to another image is never moved.

use crate::error::{Error, ErrorKind, Result};
use crate::gcp::{api_error, status_code};
use google_cloud_artifactregistry_v1::client::ArtifactRegistry;
use google_cloud_artifactregistry_v1::model::Tag;
use google_cloud_gax::error::rpc::Code;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const CHANGELOG_NAMES: &[&str] = &[
    "CHANGELOG.md",
    "CHANGELOG",
    "Changelog.md",
    "changelog.md",
    "CHANGES.md",
    "HISTORY.md",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseKind {
    Release,
    Candidate,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseTag {
    pub tag: String,
    /// `<repository path>/<package>:<tag>`.
    pub image: String,
    pub version: String,
    pub changelog: String,
    /// False when the image already carried this tag.
    pub created: bool,
}

/// First changelog found in `dirs` (in order).
pub fn find_changelog(dirs: &[&Path]) -> Option<PathBuf> {
    dirs.iter()
        .flat_map(|d| CHANGELOG_NAMES.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

fn is_version(tok: &str) -> bool {
    let core = tok
        .strip_prefix('v')
        .or_else(|| tok.strip_prefix('V'))
        .unwrap_or(tok);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// The latest version: the first heading containing `vX.Y.Z` or `X.Y.Z`
/// (Keep a Changelog `## [1.2.3] - date`, `## v1.2.3`, `# 1.2.3 (date)`…),
/// skipping "Unreleased" sections. Returned as written.
pub fn latest_version(changelog: &str) -> Option<String> {
    changelog
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .filter(|l| !l.to_ascii_lowercase().contains("unreleased"))
        .find_map(|l| {
            l.split(|c: char| c.is_whitespace() || matches!(c, '[' | ']' | '(' | ')' | ',' | ':'))
                .find(|t| is_version(t))
                .map(|t| t.to_string())
        })
}

/// RC number for `version` given existing tags: highest `<version>-RC<n>` + 1.
pub fn next_rc(version: &str, existing: &[String]) -> u32 {
    let prefix = format!("{version}-rc");
    existing
        .iter()
        .filter_map(|t| {
            t.to_ascii_lowercase()
                .strip_prefix(&prefix)?
                .parse::<u32>()
                .ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

pub fn rc_number(version: &str, tag: &str) -> Option<u32> {
    tag.to_ascii_lowercase()
        .strip_prefix(&format!("{version}-rc"))?
        .parse()
        .ok()
}

/// Where the app's image lives in Artifact Registry.
pub struct Package<'a> {
    pub project: &'a str,
    pub location: &'a str,
    pub repository: &'a str,
    pub package: &'a str,
}

impl Package<'_> {
    /// The package's resource name. Artifact Registry escapes the slashes
    /// of a nested image path (`team/agent` is `team%2Fagent`): unescaped,
    /// they would end the package ID.
    pub fn parent(&self) -> String {
        format!(
            "projects/{}/locations/{}/repositories/{}/packages/{}",
            self.project,
            self.location,
            self.repository,
            self.package.replace('/', "%2F")
        )
    }
    /// `LOCATION-docker.pkg.dev/PROJECT/REPOSITORY/PACKAGE` (no tag).
    pub fn image(&self) -> String {
        format!(
            "{}-docker.pkg.dev/{}/{}/{}",
            self.location, self.project, self.repository, self.package
        )
    }
}

/// The latest release candidate of `version` among `(tag, digest)` pairs.
pub fn latest_candidate(tags: &[(String, String)], version: &str) -> Option<(u32, String, String)> {
    tags.iter()
        .filter_map(|(t, d)| rc_number(version, t).map(|n| (n, t.clone(), d.clone())))
        .max_by_key(|(n, _, _)| *n)
}

/// `(tag, digest)` pairs of the package (none when it does not exist yet).
pub async fn list_tags(ar: &ArtifactRegistry, pkg: &Package<'_>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let resp = ar
            .list_tags()
            .set_parent(pkg.parent())
            .set_page_size(1000)
            .set_page_token(token.clone())
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) if status_code(&e) == Some(Code::NotFound) => return Ok(out),
            Err(e) => return Err(api_error(e, "listing image tags")),
        };
        for t in resp.tags {
            let tag = t.name.rsplit('/').next().unwrap_or(&t.name).to_string();
            let digest = t
                .version
                .rsplit('/')
                .next()
                .unwrap_or(&t.version)
                .to_string();
            out.push((tag, digest));
        }
        if resp.next_page_token.is_empty() {
            return Ok(out);
        }
        token = resp.next_page_token;
    }
}

async fn create_tag(
    ar: &ArtifactRegistry,
    pkg: &Package<'_>,
    tag: &str,
    digest: &str,
) -> Result<bool> {
    let res = ar
        .create_tag()
        .set_parent(pkg.parent())
        .set_tag_id(tag)
        .set_tag(Tag::new().set_version(format!("{}/versions/{digest}", pkg.parent())))
        .send()
        .await;
    match res {
        Ok(_) => Ok(true),
        Err(e) if status_code(&e) == Some(Code::AlreadyExists) || e.http_status_code() == Some(409) => Ok(false),
        Err(e) => Err(api_error(e, &format!("tagging the image {tag}"))
            .hint("the deployer needs artifactregistry.tags.create (roles/artifactregistry.writer on the repository)")),
    }
}

/// Tags `digest` with the changelog version (or the next release candidate).
pub async fn apply(
    ar: &ArtifactRegistry,
    pkg: &Package<'_>,
    digest: &str,
    kind: ReleaseKind,
    version: &str,
    changelog: &Path,
) -> Result<ReleaseTag> {
    for _attempt in 0..3 {
        let tags = list_tags(ar, pkg).await?;
        let tag = match kind {
            ReleaseKind::Release => {
                if let Some((_, d)) = tags.iter().find(|(t, _)| t == version) {
                    if d == digest {
                        return Ok(result(pkg, version, version, changelog, false));
                    }
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("release {version} is already published for another image ({d})"),
                    )
                    .permanent()
                    .hint("bump the version in the changelog, or use --tag-rc for a release candidate"));
                }
                version.to_string()
            }
            ReleaseKind::Candidate => {
                // Reuse an RC tag this image already carries for this version.
                if let Some((t, _)) = tags
                    .iter()
                    .filter(|(t, d)| d == digest && rc_number(version, t).is_some())
                    .max_by_key(|(t, _)| rc_number(version, t))
                {
                    return Ok(result(pkg, t, version, changelog, false));
                }
                let names: Vec<String> = tags.iter().map(|(t, _)| t.clone()).collect();
                format!("{version}-RC{}", next_rc(version, &names))
            }
        };
        if create_tag(ar, pkg, &tag, digest).await? {
            return Ok(result(pkg, &tag, version, changelog, true));
        }
        // Lost a race with a concurrent deploy: re-read and decide again.
    }
    Err(Error::new(
        ErrorKind::Conflict,
        "could not allocate a release tag after 3 attempts",
    ))
}

fn result(
    pkg: &Package<'_>,
    tag: &str,
    version: &str,
    changelog: &Path,
    created: bool,
) -> ReleaseTag {
    ReleaseTag {
        tag: tag.to_string(),
        image: format!("{}:{tag}", pkg.image()),
        version: version.to_string(),
        changelog: changelog.display().to_string(),
        created,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_candidate_of_a_version() {
        let t = |tag: &str, d: &str| (tag.to_string(), d.to_string());
        let tags = [
            t("1.2.0-RC1", "sha256:a"),
            t("1.2.0-RC3", "sha256:c"),
            t("1.2.0-RC2", "sha256:b"),
            t("1.1.0-RC9", "sha256:x"),
            t("1.2.0", "sha256:z"),
        ];
        assert_eq!(
            latest_candidate(&tags, "1.2.0"),
            Some((3, "1.2.0-RC3".into(), "sha256:c".into()))
        );
        assert_eq!(latest_candidate(&tags, "2.0.0"), None);
    }

    #[test]
    fn a_nested_package_is_escaped_in_its_resource_name_only() {
        let p = Package {
            project: "my-gcp-project",
            location: "europe-west1",
            repository: "releases",
            package: "team/agent",
        };
        assert_eq!(
            p.parent(),
            "projects/my-gcp-project/locations/europe-west1/repositories/releases/packages/team%2Fagent"
        );
        assert_eq!(
            p.image(),
            "europe-west1-docker.pkg.dev/my-gcp-project/releases/team/agent"
        );
    }

    #[test]
    fn reads_the_latest_version() {
        let keep_a_changelog = "# Changelog\n\n## [Unreleased]\n- wip\n\n## [1.4.2] - 2026-09-30\n### Fixed\n- x\n\n## [1.4.1] - 2026-09-01\n";
        assert_eq!(latest_version(keep_a_changelog).as_deref(), Some("1.4.2"));
        assert_eq!(
            latest_version("# Changes\n## v2.0.0 (2026-10-01)\n## v1.9.9\n").as_deref(),
            Some("v2.0.0")
        );
        assert_eq!(
            latest_version("## Version 3.1.0: big release\n").as_deref(),
            Some("3.1.0")
        );
        assert_eq!(
            latest_version("no headings 1.2.3 here\n"),
            None,
            "only headings count"
        );
        assert_eq!(
            latest_version("## [1.2] - date\n## 1.2.3.4\n"),
            None,
            "X.Y.Z only"
        );
    }

    #[test]
    fn release_candidates_increment() {
        let tags: Vec<String> = [
            "1.4.2-RC1",
            "1.4.2-RC3",
            "1.4.1-RC9",
            "1.4.2",
            "src-abc",
            "1.4.2-rc2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(next_rc("1.4.2", &tags), 4);
        assert_eq!(next_rc("1.5.0", &tags), 1);
        assert_eq!(
            next_rc("v1.4.2", &tags),
            1,
            "the version is matched as written"
        );
    }

    #[test]
    fn finds_changelog_in_order() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert!(find_changelog(&[a.path(), b.path()]).is_none());
        std::fs::write(b.path().join("CHANGES.md"), "## 1.0.0").unwrap();
        assert_eq!(
            find_changelog(&[a.path(), b.path()]).unwrap(),
            b.path().join("CHANGES.md")
        );
        std::fs::write(a.path().join("CHANGELOG.md"), "## 2.0.0").unwrap();
        assert_eq!(
            find_changelog(&[a.path(), b.path()]).unwrap(),
            a.path().join("CHANGELOG.md")
        );
    }
}
