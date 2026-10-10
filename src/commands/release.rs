//! Where a workload's image comes from, where it goes, and what it carries.
//!
//! The image source is the workload's: a build, a configured image, or a
//! promotion (`stages.<name>.promote.from`, and as an alias for older files,
//! `--tag` on a stage when stages are mapped to `tag-rc`). A promotion takes
//! what the source stage *serves* (its one revision with all the traffic),
//! with that revision's provenance, read together. `--tag`/`--tag-rc` only
//! publish: copy into the stage's release target and tag.
//!
//! [`resolve`] decides everything without changing anything (`plan` stops
//! there); [`publish`] performs the copy and the tag (`deploy`).

use crate::build::release::{self, Package, ReleaseKind, ReleaseTag};
use crate::commands::plan::{ImageDecision, decide_image};
use crate::config::{Artifact, Deployment, LoadedConfig, Overrides, ReleaseFlag};
use crate::error::{Error, Result};
use crate::gcp::registry::{DigestResolver, RegistryClient, ResolveError};
use crate::image_ref::ImageRef;
use crate::plan::{ImagePlan, Provenance};
use crate::source::Source;
use google_cloud_artifactregistry_v1::client::ArtifactRegistry;

/// An Artifact Registry package images are pushed, copied or tagged in.
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
    /// The package of an Artifact Registry image (`LOCATION-docker.pkg.dev/
    /// PROJECT/REPOSITORY/PACKAGE[@digest]`); `None` for another registry.
    pub fn of_image(image: &str) -> Option<Self> {
        let path = image.split('@').next()?;
        let (host, rest) = path.split_once('/')?;
        let location = host.strip_suffix("-docker.pkg.dev")?;
        let mut parts = rest.splitn(3, '/');
        let (project, repository, package) = (parts.next()?, parts.next()?, parts.next()?);
        Some(Self {
            project: project.into(),
            location: location.into(),
            repository: repository.into(),
            package: package.into(),
        })
    }
}

/// The stage's own repository for `d` (where its builds or promoted copies
/// go); `None` for a workload that deploys a configured image.
pub fn stage_target(d: &Deployment) -> Option<Target> {
    let (location, project, repository) = match &d.artifact {
        Artifact::Build(b) => (
            &b.artifact_location,
            &b.artifact_project,
            &b.artifact_repository,
        ),
        Artifact::Promote(p) => (
            &p.artifact_location,
            &p.artifact_project,
            &p.artifact_repository,
        ),
        Artifact::Image { .. } => return None,
    };
    Some(Target {
        project: project.clone(),
        location: location.clone(),
        repository: repository.clone(),
        package: d.build_package(),
    })
}

/// Where `--tag`/`--tag-rc` publish `d`'s image: the stage's release
/// repository (with its `package`, else the build's), else its own
/// repository. A configured image needs a release repository.
pub fn target(d: &Deployment) -> Option<Target> {
    match &d.release.repository {
        Some(r) => Some(Target {
            project: r.project.clone(),
            location: r.location.clone(),
            repository: r.repository.clone(),
            package: r.package.clone().unwrap_or_else(|| d.build_package()),
        }),
        None => stage_target(d),
    }
}

/// Where `d`'s builds are pushed (its stage's build repository).
pub fn build_target(d: &Deployment, b: &crate::config::BuildConfig) -> Target {
    Target {
        project: b.artifact_project.clone(),
        location: b.artifact_location.clone(),
        repository: b.artifact_repository.clone(),
        package: d.build_package(),
    }
}

/// `d` deploying `image` (pinned) instead of what its configuration says.
pub fn with_image(d: &Deployment, image: &str, origin: &str) -> Result<Deployment> {
    let mut out = d.clone();
    out.artifact = Artifact::Image {
        reference: image.to_string(),
        parsed: ImageRef::parse(image).map_err(Error::internal)?,
        origin: origin.to_string(),
    };
    Ok(out)
}

/// Whether some stage is mapped to `tag-rc` (then `--tag` on a `tag` stage
/// releases a candidate instead of building, as an alias of `promote.from`).
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
            "the account copying (provider.push_service_account, else yours) needs roles/artifactregistry.reader on the source repository and roles/artifactregistry.writer on the destination",
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

// ---------- what a stage serves ----------

/// What promotion reads: tags (with the push session), and the services,
/// revisions and jobs of the source stage (with the API session).
pub struct Readers<'a> {
    pub ar: &'a ArtifactRegistry,
    pub services: &'a google_cloud_run_v2::client::Services,
    pub revisions: &'a google_cloud_run_v2::client::Revisions,
    pub jobs: &'a google_cloud_run_v2::client::Jobs,
}

/// The image a workload serves, with its provenance, from one immutable
/// read (the revision with all the traffic, or the job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serving {
    /// Pinned.
    pub image: String,
    /// The revision (`None` for a job).
    pub revision: Option<String>,
    pub provenance: Provenance,
}

fn read_error(w: &Deployment) -> impl Fn(crate::gcp::GaxError) -> Error + '_ {
    move |e| crate::gcp::api_error(e, &format!("reading {} of stage {}", w.service_id, w.stage))
}

fn not_deployed(w: &Deployment) -> Error {
    Error::prerequisite(format!(
        "{} of stage {} ({}) is not deployed: there is nothing to promote",
        w.what(),
        w.stage,
        w.service_id
    ))
    .hint(format!("deploy stage {} first", w.stage))
    .permanent()
}

/// What `w` (a workload of another stage) serves, if runway deployed it.
pub async fn serving(r: &Readers<'_>, w: &Deployment) -> Result<Serving> {
    let not_owned = || {
        Error::prerequisite(format!(
            "{} of stage {} is not managed by runway: its image is not promoted",
            w.service_id, w.stage
        ))
        .permanent()
    };
    let (image, revision, provenance) = if w.is_job() {
        let job = match r.jobs.get_job().set_name(w.service_name()).send().await {
            Ok(j) => j,
            Err(e) if crate::gcp::is_not_found(&e) => return Err(not_deployed(w)),
            Err(e) => return Err(read_error(w)(e)),
        };
        if crate::gcp::jobs::check_job_ownership(&job, &w.app, &w.stage).is_err() {
            return Err(not_owned());
        }
        let t = job.template.clone().unwrap_or_default();
        let image = t
            .template
            .as_ref()
            .and_then(|t| t.containers.first())
            .map(|c| c.image.clone());
        (image, None, Provenance::from_annotations(&t.annotations))
    } else {
        let svc = match r
            .services
            .get_service()
            .set_name(w.service_name())
            .send()
            .await
        {
            Ok(s) => s,
            Err(e) if crate::gcp::is_not_found(&e) => return Err(not_deployed(w)),
            Err(e) => return Err(read_error(w)(e)),
        };
        if crate::gcp::run::ownership(&svc, &w.app, &w.stage) != crate::gcp::run::Ownership::Owned {
            return Err(not_owned());
        }
        let revision = serving_revision(&svc, w)?;
        let name = format!("{}/revisions/{revision}", w.service_name());
        let rev = r
            .revisions
            .get_revision()
            .set_name(&name)
            .send()
            .await
            .map_err(read_error(w))?;
        let image = rev
            .containers
            .iter()
            .find(|c| !c.ports.is_empty())
            .or(rev.containers.first())
            .map(|c| c.image.clone());
        (
            image,
            Some(revision),
            Provenance::from_annotations(&rev.annotations),
        )
    };
    match image {
        Some(i) if i.contains("@sha256:") => Ok(Serving {
            image: i,
            revision,
            provenance,
        }),
        Some(i) => Err(Error::prerequisite(format!(
            "{} of stage {} runs {i}, which is not pinned to a digest: it cannot be promoted as is",
            w.service_id, w.stage
        ))
        .hint(format!(
            "deploy stage {} again with runway: it pins images",
            w.stage
        ))),
        None => Err(not_deployed(w)),
    }
}

/// The one revision behind all of `svc`'s traffic, as Cloud Run reports it
/// serving (`traffic_statuses`, not the traffic it was asked for). A rollout
/// in progress is waited for (the error is retried); a split (a canary, a
/// partial rollback) is not guessed: it must be settled first.
fn serving_revision(svc: &google_cloud_run_v2::model::Service, w: &Deployment) -> Result<String> {
    let unsettled = |why: &str| {
        Error::prerequisite(format!(
            "{} of stage {} {why}: what it serves is not settled",
            w.service_id, w.stage
        ))
        .hint(format!(
            "wait for stage {}'s rollout to finish (`runway info --stage {}`), then deploy again",
            w.stage, w.stage
        ))
    };
    if svc.reconciling {
        return Err(unsettled("is still rolling out"));
    }
    let latest_ready = crate::gcp::run::short_revision(&svc.latest_ready_revision).to_string();
    let mut serving: Vec<(String, i32)> = Vec::new();
    for t in svc.traffic_statuses.iter().filter(|t| t.percent > 0) {
        // A status of the "latest" target names no revision: the latest ready.
        let rev = match t.revision.is_empty() {
            true => latest_ready.clone(),
            false => crate::gcp::run::short_revision(&t.revision).to_string(),
        };
        match serving.iter_mut().find(|(r, _)| *r == rev) {
            Some((_, p)) => *p += t.percent,
            None => serving.push((rev, t.percent)),
        }
    }
    match serving.as_slice() {
        [(rev, _)] if !rev.is_empty() => Ok(rev.clone()),
        [] if svc.latest_ready_revision.is_empty() => Err(not_deployed(w)),
        [] | [_] => Err(unsettled("reports no serving revision yet")),
        split => Err(Error::prerequisite(format!(
            "stage {} splits {}'s traffic ({}): which image to promote is not guessed",
            w.stage,
            w.service_id,
            split
                .iter()
                .map(|(r, p)| format!("{r} {p}%"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .hint(format!(
            "settle it in stage {} first: `runway traffic --stage {} --promote` sends all traffic to the canary, `runway traffic --stage {} --set REVISION=100` keeps one revision",
            w.stage, w.stage, w.stage
        ))
        .permanent()),
    }
}

/// `tag` names `version` or one of its candidates.
fn is_release_of(tag: &str, version: &str) -> bool {
    tag == version || release::rc_number(version, tag).is_some()
}

// ---------- resolution ----------

/// What a deploy records as the commit it deploys (the older-deploy guard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commit {
    /// Built from (or configured in) this checkout; `None` outside a git
    /// checkout (nothing recorded, as before).
    Checkout(Option<Source>),
    /// A promoted image, deployed from this commit in its source stage.
    Recorded(Source),
    /// A promoted image whose commit is not known: the stage's record is
    /// removed rather than replaced by this checkout's commit.
    Unknown,
}

impl Commit {
    pub fn source(&self) -> Option<&Source> {
        match self {
            Commit::Checkout(s) => s.as_ref(),
            Commit::Recorded(s) => Some(s),
            Commit::Unknown => None,
        }
    }
}

/// A change a deploy makes to images.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Build { image: String },
    Copy { from: String, to: String },
    Tag { image: String, tag: String },
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Build { image } => write!(f, "build {image}"),
            Op::Copy { from, to } => write!(f, "copy {from} to {to}"),
            Op::Tag { image, tag } => write!(f, "tag {image}:{tag}"),
        }
    }
}

/// An image another stage serves, chosen for this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promoted {
    /// Pinned, where the source stage serves it from.
    pub image: String,
    pub from: String,
    pub why: String,
    pub provenance: Provenance,
}

/// What a deploy asks for besides the configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    /// `--tag` (release) or `--tag-rc` (candidate) and the changelog version.
    pub version: Option<(ReleaseKind, &'a str)>,
    /// This checkout's commit.
    pub ours: Option<&'a Source>,
}

/// Everything about a workload's image, decided before anything changes:
/// `plan` shows it, `deploy` carries it out.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The image to deploy before it is put in place: a build (maybe
    /// pending), a configured image, or a promoted one (pinned at its source).
    pub decision: ImageDecision,
    pub promoted: Option<Promoted>,
    /// Where the deployed image lives when it is not where it is now (a
    /// promoted copy, a release target); `None`: deployed where it is.
    pub destination: Option<Target>,
    pub commit: Commit,
    /// The commit the image itself comes from (stamped on the revision): this
    /// checkout's for a build, the source revision's for a promotion, none
    /// for a configured image.
    pub image_commit: Option<Source>,
    /// The release carried by a promoted image (stamped on the revision).
    pub carried_release: Option<String>,
    pub ops: Vec<Op>,
    /// What deciding found worth saying (base images, digests).
    pub notes: Vec<String>,
}

impl Resolution {
    /// The reference deployed for `image` (pinned): its copy at the
    /// destination, else itself.
    pub fn deployed(&self, image: &str) -> String {
        match (&self.destination, image.rsplit_once('@')) {
            (Some(t), Some((_, digest))) => t.pinned(digest),
            _ => image.to_string(),
        }
    }

    /// What `plan` shows: the deployed reference when the digest is known.
    pub fn planned_image(&self) -> ImagePlan {
        match &self.decision.image {
            ImagePlan::Pinned {
                reference,
                digest,
                origin,
            } => ImagePlan::Pinned {
                reference: self.deployed(reference),
                digest: digest.clone(),
                origin: origin.clone(),
            },
            other => other.clone(),
        }
    }

    /// The provenance stamped on the revision; `release` is the tag this
    /// deploy publishes, if any.
    pub fn provenance(&self, release: Option<&str>) -> Provenance {
        Provenance {
            source: self.image_commit.clone(),
            release: release
                .map(String::from)
                .or_else(|| self.carried_release.clone()),
        }
    }
}

/// The stages `d`'s image is promoted from for this request: its own
/// `promote.from`, else (alias) with `--tag` on a build when stages are
/// mapped to `tag-rc`: `release.from`, or every `tag-rc` stage.
fn sources(cfg: &LoadedConfig, d: &Deployment, req: &Request<'_>) -> Vec<String> {
    match &d.artifact {
        Artifact::Promote(p) => vec![p.from.clone()],
        Artifact::Build(_)
            if matches!(req.version, Some((ReleaseKind::Release, _))) && promotes(cfg) =>
        {
            match &d.release.from {
                Some(f) => vec![f.clone()],
                None => cfg
                    .release_stages(ReleaseFlag::TagRc)
                    .into_iter()
                    .filter(|s| *s != d.stage)
                    .collect(),
            }
        }
        _ => Vec::new(),
    }
}

/// Why a source's serving image qualifies (or not) for `version`.
async fn evidence(
    r: &Readers<'_>,
    s: &Serving,
    from: &str,
    version: &str,
    ours: Option<&Source>,
) -> Result<std::result::Result<String, String>> {
    // The release the serving revision carries, else the tags of its digest
    // in the repository it is served from (deployments before revisions
    // carried it).
    let tag = match &s.provenance.release {
        Some(t) => Some(t.clone()),
        None => match Target::of_image(&s.image) {
            Some(t) => {
                let digest = digest_of(&s.image)?;
                release::list_tags(r.ar, &t.package())
                    .await?
                    .into_iter()
                    .filter(|(tag, d)| *d == digest && is_release_of(tag, version))
                    .map(|(tag, _)| tag)
                    .max_by_key(|tag| release::rc_number(version, tag).unwrap_or(u32::MAX))
            }
            None => None,
        },
    };
    if let Some(t) = tag.filter(|t| is_release_of(t, version)) {
        return Ok(Ok(format!("{t}, which stage {from} serves")));
    }
    let theirs = s.provenance.source.as_ref();
    Ok(match (ours, theirs) {
        (Some(o), Some(t)) if o.commit == t.commit && !t.dirty => Ok(format!(
            "what stage {from} serves, built from this commit ({})",
            t.short()
        )),
        (Some(o), Some(t)) => Err(format!(
            "stage {from} serves {} (commit {}{}, not this one, {}), not a candidate of {version}",
            s.image,
            t.short(),
            if t.dirty {
                " with uncommitted changes"
            } else {
                ""
            },
            o.short()
        )),
        (None, _) => Err(format!(
            "stage {from} serves {}, not a candidate of {version}, and this checkout's commit is unknown (set RUNWAY_SOURCE_COMMIT and RUNWAY_SOURCE_TIME)",
            s.image
        )),
        (_, None) => Err(format!(
            "stage {from} serves {}, not a candidate of {version}, and its commit is unknown",
            s.image
        )),
    })
}

/// The image `w` (a workload of another stage) ran built from `ours`, the
/// commit being deployed: from its Ready revisions (newest first), or a
/// job's current image. Nothing else: no match, or several images for the
/// commit, stops the deploy.
async fn of_commit(r: &Readers<'_>, w: &Deployment, ours: Option<&Source>) -> Result<Promoted> {
    let ours = ours.ok_or_else(|| {
        Error::config(format!(
            "stage {} promotes the image of the commit being deployed (`promote.commit: checkout`), but this checkout's commit is unknown",
            w.stage
        ))
        .hint("deploy from a git checkout, or set RUNWAY_SOURCE_COMMIT and RUNWAY_SOURCE_TIME")
        .permanent()
    })?;
    let built_from_ours = |p: &Provenance| {
        p.source
            .as_ref()
            .is_some_and(|s| s.commit == ours.commit && !s.dirty)
    };
    let missing = |what: String| {
        Error::prerequisite(format!(
            "stage {} has not run an image built from commit {} for {}: {what}",
            w.stage,
            ours.short(),
            w.what()
        ))
        .hint(format!(
            "deploy commit {} to stage {} first: an image is promoted only once it ran there",
            ours.short(),
            w.stage
        ))
        .permanent()
    };
    if w.is_job() {
        // A job keeps no history: its current image only.
        let s = serving(r, w).await?;
        if !built_from_ours(&s.provenance) {
            return Err(missing(format!(
                "its job runs {}{} (jobs keep no history)",
                s.image,
                s.provenance
                    .source
                    .as_ref()
                    .map(|c| format!(", built from {}", c.short()))
                    .unwrap_or_default()
            )));
        }
        return Ok(Promoted {
            why: format!(
                "commit {}, which stage {}'s job runs",
                ours.short(),
                w.stage
            ),
            image: s.image,
            from: w.stage.clone(),
            provenance: s.provenance,
        });
    }
    let svc = match r
        .services
        .get_service()
        .set_name(w.service_name())
        .send()
        .await
    {
        Ok(s) => s,
        Err(e) if crate::gcp::is_not_found(&e) => return Err(not_deployed(w)),
        Err(e) => return Err(read_error(w)(e)),
    };
    if crate::gcp::run::ownership(&svc, &w.app, &w.stage) != crate::gcp::run::Ownership::Owned {
        return Err(Error::prerequisite(format!(
            "{} of stage {} is not managed by runway: its image is not promoted",
            w.service_id, w.stage
        ))
        .permanent());
    }
    let mut revisions = Vec::new();
    let mut token = String::new();
    loop {
        let page = r
            .revisions
            .list_revisions()
            .set_parent(w.service_name())
            .set_page_token(token.clone())
            .send()
            .await
            .map_err(read_error(w))?;
        revisions.extend(page.revisions);
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    let ready = |rev: &google_cloud_run_v2::model::Revision| {
        rev.conditions.iter().any(|c| {
            c.r#type == "Ready"
                && c.state == google_cloud_run_v2::model::condition::State::ConditionSucceeded
        })
    };
    // (revision, image, provenance), newest first.
    let mut matches: Vec<(String, String, Provenance)> = revisions
        .iter()
        .filter(|rev| ready(rev))
        .filter_map(|rev| {
            let p = Provenance::from_annotations(&rev.annotations);
            let image = rev
                .containers
                .iter()
                .find(|c| !c.ports.is_empty())
                .or(rev.containers.first())?
                .image
                .clone();
            (built_from_ours(&p) && image.contains("@sha256:")).then(|| {
                (
                    crate::gcp::run::short_revision(&rev.name).to_string(),
                    image,
                    p,
                )
            })
        })
        .collect();
    matches.sort_by(|a, b| b.0.cmp(&a.0));
    let digests: std::collections::BTreeSet<String> = matches
        .iter()
        .filter_map(|(_, i, _)| digest_of(i).ok())
        .collect();
    match (matches.first(), digests.len()) {
        (None, _) => Err(missing(format!(
            "none of its {} Ready revision(s) was",
            revisions.iter().filter(|r| ready(r)).count()
        ))),
        (Some((rev, image, p)), 1) => Ok(Promoted {
            why: format!(
                "commit {}, which stage {} ran (revision {rev})",
                ours.short(),
                w.stage
            ),
            image: image.clone(),
            from: w.stage.clone(),
            provenance: p.clone(),
        }),
        _ => Err(Error::prerequisite(format!(
            "stage {} ran several images built from commit {} for {}: {}",
            w.stage,
            ours.short(),
            w.what(),
            matches
                .iter()
                .map(|(rev, i, _)| format!("{rev} ({i})"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .hint("which one to promote is not guessed: delete the revisions that should not be promoted, or deploy the commit again")
        .permanent()),
    }
}

/// The image another stage serves, chosen for the build owner `d`; `None`
/// when `d` does not promote for this request.
pub async fn select(
    r: &Readers<'_>,
    cfg: &LoadedConfig,
    d: &Deployment,
    req: &Request<'_>,
) -> Result<Option<Promoted>> {
    let from = sources(cfg, d, req);
    if from.is_empty() {
        return Ok(None);
    }
    let match_commit = matches!(&d.artifact, Artifact::Promote(p) if p.match_commit);
    // The version already in the release target (a repository other stages
    // may share): kept only when this stage serves it already (a re-run).
    // Anything else must come from the source stage.
    let published = match (req.version, target(d)) {
        (Some((ReleaseKind::Release, version)), Some(t)) => release::list_tags(r.ar, &t.package())
            .await?
            .into_iter()
            .find(|(tag, _)| tag == version)
            .map(|(_, digest)| (t, version, digest)),
        _ => None,
    };
    if let Some((t, version, digest)) = &published
        && !match_commit
        && let Ok(s) = serving(r, d).await
        && digest_of(&s.image).ok().as_ref() == Some(digest)
    {
        return Ok(Some(Promoted {
            image: t.pinned(digest),
            from: d.stage.clone(),
            why: format!("release {version}, which stage {} already serves", d.stage),
            provenance: s.provenance,
        }));
    }
    let mut found: Vec<Promoted> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for stage in &from {
        let src = crate::config::resolve(cfg, stage, &Overrides::default()).map_err(|diag| {
            diag.into_error(&format!("stage `{stage}` (promoted from) is invalid"))
        })?;
        let w = src
            .deployments
            .iter()
            .find(|w| w.key == d.key && w.is_job() == d.is_job())
            .ok_or_else(|| {
                Error::config(format!(
                    "stage `{stage}` has no {}: there is nothing to promote",
                    d.what()
                ))
                .hint(format!(
                    "remove `stages.{}.promote` to build it in this stage",
                    d.stage
                ))
            })?;
        if match_commit {
            // The commit decides alone: no release tag, no fallback.
            found.push(of_commit(r, w, req.ours).await?);
            continue;
        }
        let s = serving(r, w).await?;
        let why = match req.version {
            None => Ok(format!("what stage {stage} serves")),
            Some((_, version)) => evidence(r, &s, stage, version, req.ours).await?,
        };
        match why {
            Ok(why) => found.push(Promoted {
                image: s.image,
                from: stage.clone(),
                why,
                provenance: s.provenance,
            }),
            Err(reason) => refused.push(reason),
        }
    }
    match found.as_slice() {
        [] => {
            let version = req.version.map(|(_, v)| v).unwrap_or_default();
            Err(Error::prerequisite(format!(
                "no image to release as {version} for {}: {}",
                d.what(),
                refused.join("; ")
            ))
            .hint(match from.as_slice() {
                [one] => {
                    format!("deploy stage {one} with --tag-rc first, or deploy this commit there")
                }
                _ => "deploy a tag-rc stage with --tag-rc first".to_string(),
            }))
        }
        [first, rest @ ..]
            if rest
                .iter()
                .all(|p| digest_of(&p.image).ok() == digest_of(&first.image).ok()) =>
        {
            // The version tagged on another image already (by another stage
            // sharing the release repository): refused now, before anything
            // changes, rather than when tagging.
            if let Some((t, version, digest)) = &published
                && digest_of(&first.image)? != *digest
            {
                return Err(Error::new(
                    crate::error::ErrorKind::Conflict,
                    format!(
                        "release {version} is already published in {} for another image ({digest}), and stage {} does not serve it; {} is {}",
                        t.image(),
                        d.stage,
                        first.image,
                        first.why
                    ),
                )
                .permanent()
                .hint("bump the version in the changelog, or give this stage its own release repository (`stages.<name>.release.repository`)"));
            }
            Ok(Some(first.clone()))
        }
        many => Err(Error::config(format!(
            "the candidates for {} differ between stages: {}",
            d.what(),
            many.iter()
                .map(|p| format!("{} ({})", p.image, p.why))
                .collect::<Vec<_>>()
                .join("; ")
        ))
        .hint(format!(
            "choose the stage to release from: `stages.{}.promote.from: <stage>`",
            d.stage
        ))),
    }
}

/// Decides `d`'s image (`d` is a build owner), without changing anything.
/// `readers = None`: offline (a promotion is then looked up when deploying).
pub async fn resolve(
    cfg: &LoadedConfig,
    d: &Deployment,
    readers: Option<&Readers<'_>>,
    resolver: Option<&dyn DigestResolver>,
    req: &Request<'_>,
    notes: &mut Vec<String>,
) -> Result<Resolution> {
    let publish_to = match req.version {
        Some(_) => Some(target(d).ok_or_else(|| {
            Error::config(format!(
                "--tag/--tag-rc publish {}'s image to a release repository: this stage deploys a configured image and has none",
                d.what()
            ))
            .hint("set `release.repository` (or `stages.<name>.release.repository`)")
        })?),
        None => None,
    };
    let tag_op = |image: &str| {
        req.version.map(|(kind, v)| Op::Tag {
            image: image.to_string(),
            tag: match kind {
                ReleaseKind::Release => v.to_string(),
                ReleaseKind::Candidate => format!("{v}-RC<n>"),
            },
        })
    };
    let from = sources(cfg, d, req);
    if !from.is_empty() {
        let destination = publish_to.or_else(|| stage_target(d));
        let Some(r) = readers else {
            let what = format!("(the image stage {} serves)", from.join(" or "));
            let mut ops = Vec::new();
            if let Some(t) = &destination {
                ops.push(Op::Copy {
                    from: what.clone(),
                    to: t.image(),
                });
                ops.extend(tag_op(&t.image()));
            }
            return Ok(Resolution {
                decision: ImageDecision {
                    image: ImagePlan::Unresolved {
                        reference: what,
                        reason: "offline: looked up when deploying".into(),
                    },
                    build: None,
                    source: None,
                    annotations: Default::default(),
                },
                promoted: None,
                destination,
                commit: Commit::Unknown,
                image_commit: None,
                carried_release: None,
                ops,
                notes: Vec::new(),
            });
        };
        let p = select(r, cfg, d, req)
            .await?
            .ok_or_else(|| Error::internal("a promotion without a source"))?;
        let o = with_image(d, &p.image, &format!("the image of stage {}", p.from))?;
        let decision = decide_image(&o, resolver, notes).await?;
        let mut ops = Vec::new();
        if let Some(t) = &destination {
            let (path, _) = p.image.rsplit_once('@').expect("pinned");
            if path != t.image() {
                ops.push(Op::Copy {
                    from: p.image.clone(),
                    to: t.image(),
                });
            }
            ops.extend(tag_op(&t.image()));
        }
        return Ok(Resolution {
            decision,
            commit: match &p.provenance.source {
                Some(s) => Commit::Recorded(s.clone()),
                None => Commit::Unknown,
            },
            image_commit: p.provenance.source.clone(),
            carried_release: match req.version {
                None => p.provenance.release.clone(),
                Some(_) => None,
            },
            promoted: Some(p),
            destination,
            ops,
            notes: Vec::new(),
        });
    }
    let decision = decide_image(d, resolver, notes).await?;
    let mut ops = Vec::new();
    if let ImagePlan::PendingBuild { target: built, .. } = &decision.image {
        ops.push(Op::Build {
            image: built.clone(),
        });
    }
    if let Some(t) = &publish_to {
        let here = decision
            .image
            .deploy_reference()
            .map(|r| r.split('@').next().unwrap_or(r).to_string())
            .or_else(|| d.build_image());
        if here.as_deref() != Some(t.image().as_str()) {
            ops.push(Op::Copy {
                from: here.unwrap_or_else(|| "the built image".into()),
                to: t.image(),
            });
        }
        ops.extend(tag_op(&t.image()));
    }
    let built = matches!(d.artifact, Artifact::Build(_));
    Ok(Resolution {
        commit: Commit::Checkout(req.ours.cloned()),
        image_commit: req.ours.filter(|_| built).cloned(),
        decision,
        promoted: None,
        destination: publish_to,
        carried_release: None,
        ops,
        notes: Vec::new(),
    })
}

/// Images going to the same destination (`(what, resolution, image)`): one
/// copy each, and with a release tag, the same image (a version names one
/// image in a package). Checked before anything is copied or tagged.
pub fn check_destinations(items: &[(String, &Resolution, &str)], tagging: bool) -> Result<()> {
    if !tagging {
        return Ok(());
    }
    for (i, (what, r, image)) in items.iter().enumerate() {
        let Some(t) = &r.destination else { continue };
        for (other, o, other_image) in &items[..i] {
            if o.destination.as_ref() == Some(t) && digest_of(image)? != digest_of(other_image)? {
                return Err(Error::config(format!(
                    "{other} and {what} are released to the same image {} but are different images ({other_image}, {image})",
                    t.image()
                ))
                .hint("give them their own packages (remove `artifact_package` / `release.repository.package`), or make their source stage serve the same image"));
            }
        }
    }
    Ok(())
}

/// Carries out a resolution's copy and tag for `image` (pinned: the build,
/// the configured or the promoted image). Returns the image to deploy.
pub async fn publish(
    ar: &ArtifactRegistry,
    registry: &RegistryClient,
    res: &Resolution,
    image: &str,
    release: Option<(ReleaseKind, &str, &std::path::Path)>,
) -> Result<(String, Option<ReleaseTag>)> {
    let Some(t) = &res.destination else {
        return Ok((image.to_string(), None));
    };
    let deployed = copy_into(registry, image, t).await?;
    let tag = match release {
        Some((kind, version, changelog)) => Some(
            release::apply(
                ar,
                &t.package(),
                &digest_of(&deployed)?,
                kind,
                version,
                changelog,
            )
            .await?,
        ),
        None => None,
    };
    Ok((deployed, tag))
}
