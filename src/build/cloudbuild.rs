//! Source upload and Cloud Build orchestration.
//!
//! Flow: (1) reuse an existing image with the same content-addressed tag;
//! (2) attach to an in-flight build of the same source; otherwise (3) upload
//! the archive, (4) submit a build, (5) poll it to completion and (6) return
//! the pushed image digest from the build results.

use crate::build::package::{self, CompressedArchive, SourceManifest};
use crate::config::{BuildConfig, BuildStrategy};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::registry::{DigestResolver, ResolveError};
use crate::gcp::{api_error, is_ambiguous, logging};
use crate::image_ref::ImageRef;
use crate::naming;
use crate::output::Progress;
use crate::poll::{PollConfig, Poller, Tick};
use async_trait::async_trait;
use google_cloud_build_v1::client::CloudBuild;
use google_cloud_build_v1::model::{
    Build, BuildOperationMetadata, BuildOptions, BuildStep, Source, StorageSource, build,
    build_options::LoggingMode,
};
use google_cloud_logging_v2::client::LoggingServiceV2;
use serde::Serialize;
use std::time::Duration;

pub const DOCKER_BUILDER: &str = "gcr.io/cloud-builders/docker";
/// Image running the Cloud Native Buildpacks CLI (as used by `gcloud builds submit --pack`).
pub const PACK_BUILDER: &str = "gcr.io/k8s-skaffold/pack";

/// The single build step. Both variants build into the worker's Docker
/// daemon; Cloud Build then pushes `images` and reports their digests.
pub fn build_step(strategy: &BuildStrategy, tagged: &str) -> BuildStep {
    match strategy {
        BuildStrategy::Dockerfile { path } => BuildStep::new().set_name(DOCKER_BUILDER).set_args([
            "build".to_string(),
            "-t".to_string(),
            tagged.to_string(),
            "-f".to_string(),
            path.clone(),
            ".".to_string(),
        ]),
        BuildStrategy::Buildpacks { builder } => BuildStep::new()
            .set_name(PACK_BUILDER)
            .set_entrypoint("pack")
            .set_args([
                "build".to_string(),
                tagged.to_string(),
                "--builder".to_string(),
                builder.clone(),
                "--network".to_string(),
                "cloudbuild".to_string(),
                "--path".to_string(),
                ".".to_string(),
            ]),
    }
}

/// Uploads build sources. Implemented for the Cloud Storage client and by fakes in tests.
#[async_trait]
pub trait SourceUploader: Send + Sync {
    /// Uploads the archive (streamed from its temporary file) and returns the object generation.
    async fn upload(&self, bucket: &str, object: &str, archive: &CompressedArchive) -> Result<i64>;
}

#[async_trait]
impl SourceUploader for google_cloud_storage::client::Storage {
    async fn upload(&self, bucket: &str, object: &str, archive: &CompressedArchive) -> Result<i64> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            // Streamed from disk: memory use does not grow with the archive size.
            let payload = tokio::fs::File::from_std(archive.reopen()?);
            // The object name is content-addressed, so re-uploading is safe and
            // the upload can be treated as idempotent (retried by the client).
            let res = self
                .write_object(format!("projects/_/buckets/{bucket}"), object, payload)
                .set_content_type("application/gzip")
                .with_idempotency(true)
                .send_unbuffered()
                .await;
            match res {
                Ok(obj) => return Ok(obj.generation),
                Err(e) if attempt < 3 && is_ambiguous(&e) => {
                    tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                }
                Err(e) => {
                    return Err(api_error(
                        e,
                        &format!("uploading source to gs://{bucket}/{object}"),
                    )
                    .hint(format!(
                        "the deploying principal needs storage.objects.create on bucket {bucket}"
                    )));
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BuiltImage {
    /// Tagged reference (`...:src-<hash>`).
    pub tagged: String,
    /// Immutable reference (`...@sha256:...`).
    pub pinned: String,
    pub digest: String,
    pub build_id: Option<String>,
    pub log_url: Option<String>,
    /// True when an existing image was reused and no build ran.
    pub reused: bool,
}

pub struct BuildInputs<'a> {
    pub project: &'a str,
    pub region: &'a str,
    pub app: &'a str,
    pub stage: &'a str,
    pub config: &'a BuildConfig,
    pub source: &'a SourceManifest,
    /// The caller already looked for an existing image and found none
    /// (skips a redundant registry request).
    pub image_checked: bool,
    pub timeout: Duration,
    pub force: bool,
}

/// Runs builds in the configured region. Requests set only regional resource
/// names (`projects/*/locations/*/...`) so the SDK selects the regional REST
/// bindings rather than the global `projects/{project_id}/builds` ones.
pub struct Builder<'a> {
    pub cloudbuild: &'a CloudBuild,
    pub uploader: &'a dyn SourceUploader,
    pub resolver: &'a dyn DigestResolver,
    pub logging: Option<&'a LoggingServiceV2>,
    pub progress: &'a Progress,
    pub poll: PollConfig,
}

pub fn image_target(inp: &BuildInputs<'_>) -> (String, String) {
    let name = naming::build_image_name(
        &inp.config.artifact_location,
        inp.project,
        &inp.config.artifact_repository,
        inp.app,
    );
    let tagged = format!(
        "{name}:{}",
        naming::build_image_tag(&inp.source.inputs_sha256)
    );
    (name, tagged)
}

/// The Cloud Build request for a source archive.
pub fn build_request(inp: &BuildInputs<'_>, generation: i64) -> Build {
    let (_, tagged) = image_target(inp);
    let object = naming::source_object(inp.app, &inp.source.sha256);
    Build::new()
        .set_source(
            Source::new().set_storage_source(
                StorageSource::new()
                    .set_bucket(&inp.config.source_bucket)
                    .set_object(object)
                    .set_generation(generation),
            ),
        )
        .set_steps([build_step(&inp.config.strategy, &tagged)])
        .set_images([tagged])
        .set_service_account(format!(
            "projects/{}/serviceAccounts/{}",
            inp.project, inp.config.build_service_account
        ))
        .set_options(BuildOptions::new().set_logging(LoggingMode::CloudLoggingOnly))
        .set_timeout(
            google_cloud_wkt::Duration::new(inp.timeout.as_secs() as i64, 0)
                .expect("build timeout within range"),
        )
        .set_tags(naming::build_tags(
            inp.app,
            inp.stage,
            &inp.source.inputs_sha256,
        ))
}

fn is_terminal(s: &build::Status) -> bool {
    !matches!(
        s,
        build::Status::Pending
            | build::Status::Queued
            | build::Status::Working
            | build::Status::Unknown
    )
}

fn status_name(s: &build::Status) -> String {
    s.name().unwrap_or("UNKNOWN").to_string()
}

impl Builder<'_> {
    fn parent(inp: &BuildInputs<'_>) -> String {
        naming::location_parent(inp.project, inp.region)
    }

    fn build_name(inp: &BuildInputs<'_>, id: &str) -> String {
        format!("{}/builds/{id}", Self::parent(inp))
    }

    /// Finds a build of the same inputs (by tag) that publishes this exact
    /// image, optionally only in-flight ones. Another app with identical
    /// sources has the same tag but a different image path.
    async fn find_build(
        &self,
        inp: &BuildInputs<'_>,
        in_flight_only: bool,
    ) -> Result<Option<Build>> {
        let tag = naming::source_build_tag(&inp.source.inputs_sha256);
        let (_, tagged) = image_target(inp);
        let resp = self
            .cloudbuild
            .list_builds()
            .set_parent(Self::parent(inp))
            .set_filter(format!("tags=\"{tag}\""))
            .set_page_size(20)
            .send()
            .await
            .map_err(|e| api_error(e, "listing Cloud Builds"))?;
        Ok(resp
            .builds
            .into_iter()
            .filter(|b| b.tags.contains(&tag) && b.images.contains(&tagged))
            .find(|b| {
                if in_flight_only {
                    !is_terminal(&b.status)
                } else {
                    !matches!(
                        b.status,
                        build::Status::Failure
                            | build::Status::InternalError
                            | build::Status::Timeout
                            | build::Status::Cancelled
                            | build::Status::Expired
                    )
                }
            }))
    }

    async fn submit(&self, inp: &BuildInputs<'_>, generation: i64) -> Result<String> {
        let request = build_request(inp, generation);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let res = self
                .cloudbuild
                .create_build()
                .set_parent(Self::parent(inp))
                .set_build(request.clone())
                .send()
                .await;
            match res {
                Ok(op) => {
                    let id = op
                        .metadata
                        .as_ref()
                        .and_then(|m| m.to_msg::<BuildOperationMetadata>().ok())
                        .and_then(|m| m.build)
                        .map(|b| b.id)
                        .filter(|id| !id.is_empty());
                    if let Some(id) = id {
                        return Ok(id);
                    }
                    // Fall back to looking the build up by tag.
                    if let Some(b) = self.find_build(inp, false).await? {
                        return Ok(b.id);
                    }
                    return Err(Error::internal(
                        "Cloud Build accepted the build but did not return its ID",
                    ));
                }
                Err(e) if is_ambiguous(&e) => {
                    // The build may have been created: look before resubmitting.
                    self.progress.warn(format!(
                        "build submission outcome unknown ({e}); checking for an existing build"
                    ));
                    if let Some(b) = self.find_build(inp, false).await? {
                        return Ok(b.id);
                    }
                    if attempt >= 2 {
                        return Err(api_error(e, "submitting the Cloud Build"));
                    }
                }
                Err(e) => {
                    return Err(api_error(e, "submitting the Cloud Build").hint(format!(
                        "the deploying principal needs cloudbuild.builds.create and iam.serviceAccounts.actAs on {}",
                        inp.config.build_service_account
                    )));
                }
            }
        }
    }

    /// Builds (or reuses) the image for the archive.
    pub async fn build(&self, inp: &BuildInputs<'_>) -> Result<BuiltImage> {
        let (name, tagged) = image_target(inp);
        let tagged_ref = ImageRef::parse(&tagged)
            .map_err(|e| Error::internal(format!("generated invalid image reference: {e}")))?;

        if !inp.force && !inp.image_checked {
            match self.resolver.resolve(&tagged_ref).await {
                Ok(Some(digest)) => {
                    self.progress
                        .info(format!("image {tagged} already exists; skipping build"));
                    return Ok(BuiltImage {
                        pinned: format!("{name}@{digest}"),
                        tagged,
                        digest,
                        build_id: None,
                        log_url: None,
                        reused: true,
                    });
                }
                Ok(None) => {}
                Err(ResolveError::Unauthorized(m)) => self.progress.warn(format!(
                    "cannot check whether {tagged} already exists ({m}); building"
                )),
                Err(ResolveError::Other(m)) => self.progress.warn(format!(
                    "cannot check for an existing image ({m}); building"
                )),
            }
        }

        // Compress (CPU, all cores) while looking for an in-flight build of the
        // same source (network); the archive is simply dropped if one exists.
        let source = inp.source.clone();
        let compress = tokio::task::spawn_blocking(move || package::compress(&source));
        let (in_flight, archive) = tokio::join!(self.find_build(inp, true), compress);
        let build_id = match in_flight? {
            Some(b) if !inp.force => {
                self.progress.info(format!(
                    "attaching to in-flight build {} of the same source",
                    b.id
                ));
                b.id
            }
            _ => {
                let archive = archive
                    .map_err(|e| Error::internal(format!("compression task failed: {e}")))??;
                let object = naming::source_object(inp.app, &inp.source.sha256);
                self.progress.step(format!(
                    "Uploading source ({} files, {}) to gs://{}/{}",
                    inp.source.files,
                    crate::plan::human_bytes(archive.len),
                    inp.config.source_bucket,
                    object
                ));
                let generation = self
                    .uploader
                    .upload(&inp.config.source_bucket, &object, &archive)
                    .await?;
                drop(archive);
                self.progress
                    .step(format!("Submitting Cloud Build for {tagged}"));
                self.submit(inp, generation).await?
            }
        };
        self.wait(inp, &build_id, &tagged, &name).await
    }

    async fn wait(
        &self,
        inp: &BuildInputs<'_>,
        id: &str,
        tagged: &str,
        name: &str,
    ) -> Result<BuiltImage> {
        let build_name = Self::build_name(inp, id);
        let mut poller = Poller::new(self.poll, inp.timeout + Duration::from_secs(300));
        let mut last_status = String::new();
        let mut announced_logs = false;
        loop {
            let b = match self
                .cloudbuild
                .get_build()
                .set_name(&build_name)
                .send()
                .await
            {
                Ok(b) => b,
                Err(e) if is_ambiguous(&e) => {
                    self.progress
                        .warn(format!("transient error polling build: {e}"));
                    match poller.wait().await {
                        Tick::Continue => continue,
                        Tick::TimedOut => return Err(self.timeout_error(id, None)),
                        Tick::Cancelled => return Err(self.cancel(inp, id).await),
                    }
                }
                Err(e) => return Err(api_error(e, &format!("reading Cloud Build {id}"))),
            };
            if !announced_logs && !b.log_url.is_empty() {
                self.progress
                    .info(format!("build {id} logs: {}", b.log_url));
                announced_logs = true;
            }
            let status = status_name(&b.status);
            if status != last_status {
                self.progress.info(format!("build status: {status}"));
                last_status = status;
            }
            if is_terminal(&b.status) {
                return self.finish(b, tagged, name).await;
            }
            match poller.wait().await {
                Tick::Continue => {}
                Tick::TimedOut => return Err(self.timeout_error(id, Some(&b.log_url))),
                Tick::Cancelled => return Err(self.cancel(inp, id).await),
            }
        }
    }

    fn timeout_error(&self, id: &str, log_url: Option<&str>) -> Error {
        let mut e = Error::new(
            ErrorKind::Timeout,
            format!("timed out waiting for Cloud Build {id}"),
        )
        .hint("the build may still be running; re-run deploy to attach to it once it finishes");
        if let Some(u) = log_url.filter(|u| !u.is_empty()) {
            e = e.hint(format!("build logs: {u}"));
        }
        e
    }

    async fn cancel(&self, inp: &BuildInputs<'_>, id: &str) -> Error {
        self.progress
            .warn(format!("interrupted; cancelling Cloud Build {id}"));
        let res = self
            .cloudbuild
            .cancel_build()
            .set_name(Self::build_name(inp, id))
            .send()
            .await;
        let mut e = Error::new(
            ErrorKind::Interrupted,
            format!("interrupted during Cloud Build {id}"),
        );
        if let Err(err) = res {
            e = e.hint(format!(
                "could not cancel the build ({err}); cancel it in the console"
            ));
        }
        e
    }

    async fn finish(&self, b: Build, tagged: &str, name: &str) -> Result<BuiltImage> {
        if b.status == build::Status::Success {
            let digest = b
                .results
                .as_ref()
                .and_then(|r| r.images.iter().find(|i| i.name == tagged))
                .map(|i| i.digest.clone())
                .filter(|d| d.starts_with("sha256:"))
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Build,
                        format!(
                            "build {} succeeded but reported no digest for {tagged}",
                            b.id
                        ),
                    )
                })?;
            self.progress.success(format!("built {name}@{digest}"));
            return Ok(BuiltImage {
                tagged: tagged.to_string(),
                pinned: format!("{name}@{digest}"),
                digest,
                build_id: Some(b.id.clone()),
                log_url: (!b.log_url.is_empty()).then(|| b.log_url.clone()),
                reused: false,
            });
        }
        let status = status_name(&b.status);
        let mut msg = format!("Cloud Build {} finished with status {status}", b.id);
        if !b.status_detail.is_empty() {
            msg.push_str(&format!(": {}", b.status_detail));
        }
        if let Some(fi) = &b.failure_info
            && !fi.detail.is_empty()
            && !msg.contains(&fi.detail)
        {
            msg.push_str(&format!(" ({})", fi.detail));
        }
        if let Some(step) = b.steps.iter().find(|s| s.status == build::Status::Failure) {
            msg.push_str(&format!(
                "\n  failed step: {} {}",
                step.name,
                step.args.join(" ")
            ));
        }
        if let Some(lg) = self.logging {
            match logging::fetch(lg, &b.project_id, &logging::build_filter(&b.id), 30).await {
                Ok(lines) if !lines.is_empty() => {
                    msg.push_str("\n  last build log lines:");
                    for l in lines {
                        msg.push_str(&format!("\n    {}", l.message));
                    }
                }
                _ => {}
            }
        }
        // FAILURE means the build itself failed (not retryable); infrastructure
        // outcomes are retryable.
        // Failures caused by permissions that may still be propagating (fresh
        // build service account or grants) are retryable, like infrastructure errors.
        use google_cloud_build_v1::model::build::failure_info::FailureType;
        let propagation = b.failure_info.as_ref().is_some_and(|f| {
            matches!(
                f.r#type,
                FailureType::FetchSourceFailed
                    | FailureType::PushFailed
                    | FailureType::PushNotAuthorized
                    | FailureType::LoggingFailure
            )
        });
        let kind = match b.status {
            build::Status::Timeout => ErrorKind::Timeout,
            build::Status::InternalError | build::Status::Expired => ErrorKind::Internal,
            _ if propagation => ErrorKind::Internal,
            _ => ErrorKind::Build,
        };
        let mut e = Error::new(kind, msg);
        if !b.log_url.is_empty() {
            e = e.hint(format!("full build logs: {}", b.log_url));
        }
        if b.status_detail.contains("permission") || b.status_detail.contains("denied") {
            e = e.hint(format!(
                "check the build service account's roles ({}/permissions/)",
                crate::DOCS_URL
            ));
        }
        Err(e)
    }
}

#[cfg(test)]
mod tests;
