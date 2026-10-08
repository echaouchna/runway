//! Release images (`deploy --tag`, `--tag-rc`). A stage publishes them to its
//! release repository (`release.repository`, global or per stage), else its
//! build repository. Once a stage is mapped to `tag-rc`, `--tag` releases the
//! latest release candidate of the changelog version instead of building:
//! what was tested is what is released.

use crate::build::release::{self, Package, ReleaseKind, ReleaseTag};
use crate::config::{Artifact, Deployment, LoadedConfig, Overrides, ReleaseFlag};
use crate::error::{Error, Result};
use crate::gcp::registry::{RegistryClient, ResolveError};
use crate::image_ref::ImageRef;
use google_cloud_artifactregistry_v1::client::ArtifactRegistry;

/// Where a workload's released images are tagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub project: String,
    pub location: String,
    pub repository: String,
    pub package: String,
}

impl Target {
    pub fn package(&self) -> Package<'_> {
        Package {
            project: &self.project,
            location: &self.location,
            repository: &self.repository,
            package: &self.package,
        }
    }
    pub fn image(&self) -> String {
        self.package().image()
    }
    pub fn pinned(&self, digest: &str) -> String {
        format!("{}@{digest}", self.image())
    }
}

/// The stage's release repository, else the build repository; `None` for a
/// workload that deploys an existing image.
pub fn target(d: &Deployment) -> Option<Target> {
    let Artifact::Build(b) = &d.artifact else {
        return None;
    };
    let (project, location, repository) = match &d.release.repository {
        Some(r) => (r.project.clone(), r.location.clone(), r.repository.clone()),
        None => (
            d.project.clone(),
            b.artifact_location.clone(),
            b.artifact_repository.clone(),
        ),
    };
    Some(Target {
        project,
        location,
        repository,
        package: d.image_package(),
    })
}

/// `d` deploying `image` (pinned) instead of building.
pub fn with_image(d: &Deployment, image: &str) -> Result<Deployment> {
    let mut out = d.clone();
    out.artifact = Artifact::Image {
        reference: image.to_string(),
        parsed: ImageRef::parse(image).map_err(Error::internal)?,
    };
    Ok(out)
}

/// Whether `--tag` promotes a release candidate (some stage is mapped to
/// `tag-rc`) rather than building, as before release stages existed.
pub fn promotes(cfg: &LoadedConfig) -> bool {
    !cfg.release_stages(ReleaseFlag::TagRc).is_empty()
}

fn digest_of(image: &str) -> Result<String> {
    image
        .rsplit_once('@')
        .map(|(_, d)| d.to_string())
        .ok_or_else(|| Error::internal(format!("{image} has no digest")))
}

fn copy_error(e: ResolveError, src: &str, dst: &str) -> Error {
    let msg = format!("copying {src} to {dst}: {e}");
    match e {
        ResolveError::Unauthorized(_) => Error::prerequisite(msg).hint(
            "the deployer needs roles/artifactregistry.reader on the source repository and roles/artifactregistry.writer on the destination",
        ),
        ResolveError::Other(_) => Error::internal(msg),
    }
}

/// Copies `image` (pinned) into `target` unless it is already there.
async fn copy_into(registry: &RegistryClient, image: &str, target: &Target) -> Result<String> {
    let digest = digest_of(image)?;
    let (path, _) = image.rsplit_once('@').expect("pinned");
    if path != target.image() {
        let src = ImageRef::parse(image).map_err(Error::internal)?;
        let dst = ImageRef::parse(&target.image()).map_err(Error::internal)?;
        registry
            .copy(&src, &dst)
            .await
            .map_err(|e| copy_error(e, image, &target.image()))?;
    }
    Ok(target.pinned(&digest))
}

/// Publishes `image` (pinned, in the build repository or already in the
/// target) for `d`: copied into its release target when elsewhere, then
/// tagged. Returns the image to deploy, from the target.
pub async fn publish(
    ar: &ArtifactRegistry,
    registry: &RegistryClient,
    d: &Deployment,
    image: &str,
    kind: ReleaseKind,
    version: &str,
    changelog: &std::path::Path,
) -> Result<(String, ReleaseTag)> {
    let t = target(d).ok_or_else(|| Error::internal("releases are images runway builds"))?;
    let deployed = copy_into(registry, image, &t).await?;
    let tag = release::apply(
        ar,
        &t.package(),
        &digest_of(&deployed)?,
        kind,
        version,
        changelog,
    )
    .await?;
    Ok((deployed, tag))
}

/// `--tag` with release candidates: the image to release for `d`'s build,
/// found without changing anything (the copy into the stage's release
/// repository is made by [`publish`], once the repositories are provisioned).
///
/// A version already tagged in the stage's target is reused (a re-run).
/// Otherwise the latest candidate of the version, from `release.from`'s
/// repository, or else from the target and every `tag-rc` stage's: since
/// each repository numbers its candidates on its own, candidates found in
/// several repositories must be the same image.
pub async fn find_candidate(
    ar: &ArtifactRegistry,
    cfg: &LoadedConfig,
    d: &Deployment,
    version: &str,
) -> Result<String> {
    let t = target(d).ok_or_else(|| Error::internal("releases are images runway builds"))?;
    let here = release::list_tags(ar, &t.package()).await?;
    if let Some((_, digest)) = here.iter().find(|(tag, _)| tag == version) {
        return Ok(t.pinned(digest));
    }
    let rc_stages = match &d.release.from {
        Some(f) => vec![f.clone()],
        None => cfg.release_stages(ReleaseFlag::TagRc),
    };
    let mut sources: Vec<(Target, Vec<(String, String)>)> = Vec::new();
    if d.release.from.is_none() {
        sources.push((t.clone(), here));
    }
    for stage in &rc_stages {
        let r = crate::config::resolve(cfg, stage, &Overrides::default()).map_err(|diag| {
            diag.into_error(&format!("stage `{stage}` (mapped to tag-rc) is invalid"))
        })?;
        let same = r
            .deployments
            .iter()
            .find(|w| w.key == d.key && w.is_job() == d.is_job())
            .map(|w| r.build_owner(w));
        if let Some(src) = same.and_then(target)
            && !sources.iter().any(|(s, _)| *s == src)
        {
            let tags = release::list_tags(ar, &src.package()).await?;
            sources.push((src, tags));
        }
    }
    let found: Vec<(&Target, String, String)> = sources
        .iter()
        .filter_map(|(s, tags)| {
            release::latest_candidate(tags, version).map(|(_, tag, digest)| (s, tag, digest))
        })
        .collect();
    match found.as_slice() {
        [] => {
            let looked: Vec<String> = sources.iter().map(|(s, _)| s.image()).collect();
            Err(Error::prerequisite(format!(
                "no release candidate of {version} for {}: looked in {}",
                d.image_package(),
                looked.join(", ")
            ))
            .hint(format!(
                "deploy one first: runway deploy --tag-rc{}",
                match rc_stages.as_slice() {
                    [one] => format!(" (stage {one})"),
                    _ => String::new(),
                }
            )))
        }
        [first, rest @ ..] if rest.iter().all(|(_, _, dg)| *dg == first.2) => {
            // The same image everywhere: the copy already in the target, if any.
            let (source, tag, digest) = found.iter().find(|(s, _, _)| **s == t).unwrap_or(first);
            tracing::info!(%tag, from = %source.image(), "releasing a candidate");
            Ok(source.pinned(digest))
        }
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|(s, tag, dg)| {
                    format!(
                        "{tag} ({}) in {}",
                        crate::naming::short_hash(dg.trim_start_matches("sha256:")),
                        s.image()
                    )
                })
                .collect();
            Err(Error::config(format!(
                "release candidates of {version} for {} are different images in different repositories: {}",
                d.image_package(),
                list.join("; ")
            ))
            .hint(format!(
                "choose the stage to release from: `stages.{}.release.from: <tag-rc stage>`",
                d.stage
            )))
        }
    }
}
