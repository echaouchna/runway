//! `runway plan`: compute intended changes without mutating anything.
//!
//! With credentials, the plan reads the live service, its IAM policy and the
//! registry (read-only). With `--offline`, nothing is contacted and the plan
//! only describes the desired state.

use crate::build::package::{self, SourceManifest};
use crate::build_client;
use crate::cli::{Context, PlanArgs};
use crate::commands::{load, registry_client, resolve_existing_image};
use crate::config::{Artifact, Deployment, Overrides};
use crate::deploy::Reconciler;
use crate::error::Result;
use crate::gcp::registry::DigestResolver;
use crate::gcp::run::{self, Ownership};
use crate::image_ref::ImageRef;
use crate::naming;
use crate::output::{OutputFormat, Progress, print_json};
use crate::plan::{
    AccessPlan, BuildPlan, ImagePlan, Plan, ServiceAction, ServiceSpec, compute_changes,
    render_text,
};
use crate::poll::PollConfig;
use crate::provision::{Provisioner, StepCheck, StepState, all_steps};
use google_cloud_run_v2::client::Services;
use std::collections::BTreeMap;
use std::time::Duration;

/// Read-only access to Google Cloud used while planning.
pub struct Remote<'a> {
    pub run: &'a Services,
    pub resolver: &'a dyn DigestResolver,
    /// Checks provisioning steps; `None` skips them (reported as unknown).
    pub provisioner: Option<&'a Provisioner<'a>>,
}

/// Desired image information derived from the configuration.
pub struct ImageDecision {
    pub image: ImagePlan,
    pub build: Option<BuildPlan>,
    /// Scanned (hashed, not compressed) build context for source builds.
    pub source: Option<SourceManifest>,
    pub annotations: BTreeMap<String, String>,
}

pub fn build_target(
    d: &Deployment,
    b: &crate::config::BuildConfig,
    sha256: &str,
) -> (String, String) {
    let name = naming::build_image_name(
        &b.artifact_location,
        &d.project,
        &b.artifact_repository,
        &d.app,
    );
    let tagged = format!("{name}:{}", naming::build_image_tag(sha256));
    (name, tagged)
}

pub async fn decide_image(
    d: &Deployment,
    resolver: Option<&dyn DigestResolver>,
    notes: &mut Vec<String>,
) -> Result<ImageDecision> {
    let mut annotations = BTreeMap::new();
    match &d.artifact {
        Artifact::Image { reference, parsed } => {
            annotations.insert(naming::ANNOTATION_IMAGE_REF.to_string(), reference.clone());
            let image = match resolver {
                Some(r) => resolve_existing_image(r, parsed, reference).await,
                None => match &parsed.digest {
                    Some(dg) => ImagePlan::Pinned {
                        reference: parsed.pinned(dg),
                        digest: dg.clone(),
                        origin: "digest given in configuration".into(),
                    },
                    None => ImagePlan::Unresolved {
                        reference: reference.clone(),
                        reason: "offline: the registry was not contacted".into(),
                    },
                },
            };
            Ok(ImageDecision {
                image,
                build: None,
                source: None,
                annotations,
            })
        }
        Artifact::Build(b) => {
            let ctx_dir = b.context_dir.clone();
            let strategy = b.strategy.clone();
            let excluded = b.excluded.clone();
            // Hash only: compression happens later, and only if an upload is needed.
            let ctx_dir2 = ctx_dir.clone();
            let strategy2 = strategy.clone();
            let mut archive =
                tokio::task::spawn_blocking(move || package::scan(&ctx_dir, &strategy, &excluded))
                    .await
                    .map_err(|e| {
                        crate::error::Error::internal(format!("packaging task failed: {e}"))
                    })??;
            // Base images are part of the content address: an upstream update
            // under the same tag produces a new image tag, hence a rebuild.
            let refs = crate::build::inputs::base_references(&strategy2, &ctx_dir2);
            let (bases, base_notes) = crate::build::inputs::resolve(&refs, resolver).await;
            if resolver.is_some() {
                notes.extend(base_notes);
            } else if !refs.is_empty() {
                notes.push(
                    "offline: base images were not resolved, so the image tag shown is provisional"
                        .into(),
                );
            }
            archive.inputs_sha256 = crate::build::inputs::fingerprint(&archive.sha256, &bases);
            if !bases.is_empty() {
                annotations.insert(
                    naming::ANNOTATION_BASE_IMAGES.to_string(),
                    crate::build::inputs::annotation(&bases),
                );
            }
            archive.bases = bases;
            let (name, tagged) = build_target(d, b, &archive.inputs_sha256);
            annotations.insert(naming::ANNOTATION_IMAGE_REF.to_string(), tagged.clone());
            annotations.insert(
                naming::ANNOTATION_SOURCE_HASH.to_string(),
                archive.sha256.clone(),
            );
            let pending = |reason: &str| ImagePlan::PendingBuild {
                target: tagged.clone(),
                reason: reason.to_string(),
            };
            let (image, will_build) = match resolver {
                None => (
                    pending("offline: it is unknown whether this source was already built"),
                    None,
                ),
                Some(_) if b.rebuild_always => (
                    pending(
                        "`rebuild: always` builds on every deploy; the digest is only known after the build",
                    ),
                    Some(true),
                ),
                Some(r) => {
                    let tref = ImageRef::parse(&tagged).map_err(|e| {
                        crate::error::Error::internal(format!("invalid generated image: {e}"))
                    })?;
                    match r.resolve(&tref).await {
                        Ok(Some(dg)) => (
                            ImagePlan::Pinned {
                                reference: format!("{name}@{dg}"),
                                digest: dg,
                                origin: "an image for this exact source was already built".into(),
                            },
                            Some(false),
                        ),
                        Ok(None) => (
                            pending(
                                "Cloud Build produces it during deploy; the digest is only known after the build",
                            ),
                            Some(true),
                        ),
                        Err(e) => {
                            notes.push(format!(
                                "could not check Artifact Registry for {tagged}: {e}"
                            ));
                            (pending("the registry could not be checked"), None)
                        }
                    }
                }
            };
            let build = BuildPlan {
                context: b.context_dir.display().to_string(),
                strategy: b.strategy.to_string(),
                source_sha256: archive.sha256.clone(),
                files: archive.files,
                tar_bytes: archive.tar_bytes,
                upload_to: format!(
                    "gs://{}/{}",
                    b.source_bucket,
                    naming::source_object(&d.app, &archive.sha256)
                ),
                image: tagged.clone(),
                build_service_account: b.build_service_account.clone(),
                will_build,
            };
            notes.push(format!(
                "build context packaged using {} ({} files)",
                archive.ignore_source.describe(),
                archive.files
            ));
            Ok(ImageDecision {
                image,
                build: Some(build),
                source: Some(archive),
                annotations,
            })
        }
    }
}

/// Computes a plan; `remote = None` means offline.
pub async fn compute(
    d: &Deployment,
    remote: Option<Remote<'_>>,
    mode: &crate::traffic::Mode,
    progress: &Progress,
) -> Result<(Plan, ImageDecision)> {
    let mut notes = Vec::new();
    let name = d.service_name();
    // Packaging, registry lookup, service read and IAM read are independent:
    // run them concurrently so a plan costs about one round trip.
    let (decision, live) = match &remote {
        None => (decide_image(d, None, &mut notes).await?, None),
        Some(r) => {
            let rec = Reconciler {
                run: r.run,
                revisions: None,
                progress,
                poll: PollConfig::default(),
                timeout: Duration::from_secs(30),
            };
            let mut img_notes = Vec::new();
            let (decision, svc, public) = tokio::join!(
                decide_image(d, Some(r.resolver), &mut img_notes),
                rec.get(&name),
                rec.current_public(&name),
            );
            notes.extend(img_notes);
            // With `enable_apis`, a disabled Cloud Run API is a pending step,
            // not a planning failure: the service cannot exist yet.
            let api_pending = |e: &crate::error::Error| {
                d.apis.enable
                    && e.kind == crate::error::ErrorKind::Prerequisite
                    && e.message.contains("not enabled")
            };
            let (svc, public) = match (svc, public) {
                (Err(e), _) | (_, Err(e)) if api_pending(&e) => {
                    notes.push(
                        "the Cloud Run API is not enabled yet; deploy enables it first".into(),
                    );
                    (None, Some(false))
                }
                (svc, public) => (svc?, public?),
            };
            (decision?, Some((svc, public)))
        }
    };
    let spec = ServiceSpec::from_deployment(
        d,
        decision.image.deploy_reference().unwrap_or(""),
        decision.annotations.clone(),
    )
    .with_traffic_mode(mode.clone());
    let spec = match &live {
        Some((Some(svc), _)) => run::spec_for_live(&spec, Some(svc)),
        _ => spec,
    };
    if !d.service.secrets.is_empty() {
        notes.push(format!(
            "{} secret reference(s); runway reads version numbers, never values",
            d.service.secrets.len()
        ));
    }
    if let ImagePlan::Unresolved { reason, .. } = &decision.image
        && reason.contains("no such tag")
    {
        notes.push(
            "the configured image was not found; deploy will fail unless it is pushed first".into(),
        );
    }

    let live_exists = live.as_ref().map(|(svc, _)| svc.is_some());
    let (action, changes, access) = match live {
        None => {
            let (_, changes) = compute_changes(None, &spec, &decision.image);
            (
                ServiceAction::Unknown,
                changes,
                AccessPlan::new(d.service.public, None),
            )
        }
        Some((None, _)) => {
            if let Some(bs) = &d.service.bootstrap {
                notes.push(match &bs.image {
                    None => format!(
                        "first deploy: the service is created with ingress {}, its tags are bound and awaited, then ingress is switched to {}",
                        bs.ingress, d.service.ingress
                    ),
                    Some(img) => format!(
                        "first deploy: a placeholder ({img}, ingress {}) is created, the service tags are bound and awaited, then this configuration is applied",
                        bs.ingress
                    ),
                });
            }
            let (a, c) = compute_changes(None, &spec, &decision.image);
            (a, c, AccessPlan::new(d.service.public, Some(false)))
        }
        Some((Some(svc), public)) => {
            if public.is_none() {
                notes.push("cannot read the service IAM policy (run.services.getIamPolicy)".into());
            }
            let access = AccessPlan::new(d.service.public, public);
            match run::ownership(&svc, &d.app, &d.stage) {
                Ownership::Owned => {
                    let (a, c) =
                        compute_changes(Some(&run::observed_flat(&svc)), &spec, &decision.image);
                    // Release metadata: explain rebuilds caused by base image updates.
                    if let (Some(deployed), Some(src)) = (
                        svc.annotations.get(naming::ANNOTATION_BASE_IMAGES),
                        decision.source.as_ref(),
                    ) {
                        for (r, old, new) in
                            crate::build::inputs::changed_since(deployed, &src.bases)
                        {
                            notes.push(format!(
                                "base image {r} changed since the deployed release ({} -> {}); the image will be rebuilt",
                                crate::naming::short_hash(old.trim_start_matches("sha256:")),
                                crate::naming::short_hash(new.trim_start_matches("sha256:"))
                            ));
                        }
                    }
                    if let run::Readiness::Failed { message } = run::readiness(&svc) {
                        notes.push(format!(
                            "the latest revision is failing ({message}); deploy will roll out a new revision"
                        ));
                    }
                    (a, c, access)
                }
                Ownership::Unmanaged => {
                    notes.push(format!(
                        "{} exists but is not managed by runway; deploy refuses unless `--adopt` is given",
                        d.service_id
                    ));
                    let (_, c) =
                        compute_changes(Some(&run::observed_flat(&svc)), &spec, &decision.image);
                    (ServiceAction::Conflict, c, access)
                }
                Ownership::OtherOwner { app, stage } => {
                    notes.push(format!(
                        "{} is managed by runway for app `{app}` stage `{stage}`; deploy will refuse",
                        d.service_id
                    ));
                    (ServiceAction::Conflict, vec![], access)
                }
            }
        }
    };
    // Provisioning steps: all checks are read-only and run concurrently.
    let all_steps = all_steps(d);
    let service_exists = matches!(live_exists, Some(true));
    let steps: Vec<StepCheck> = match remote.as_ref().and_then(|r| r.provisioner) {
        Some(prov) => {
            futures::future::join_all(all_steps.iter().map(|s| prov.check(s, service_exists))).await
        }
        None => all_steps
            .iter()
            .map(|s| StepCheck {
                step: s.describe(d),
                state: StepState::Unknown,
                detail: if remote.is_some() {
                    "not checked".into()
                } else {
                    "offline".into()
                },
            })
            .collect(),
    };
    let mut access = access;
    access.iap = d.service.iap.enabled;
    let exact = remote.is_some()
        && steps.iter().all(|s| s.state != StepState::Unknown)
        && decision.image.is_exact()
        && access.current_public.is_some()
        && action != ServiceAction::Conflict;
    Ok((
        Plan {
            app: d.app.clone(),
            stage: d.stage.clone(),
            project: d.project.clone(),
            region: d.region.clone(),
            service: d.service_id.clone(),
            exact,
            remote_inspected: remote.is_some(),
            action,
            changes,
            image: decision.image.clone(),
            build: decision.build.clone(),
            access,
            steps,
            notes,
        },
        decision,
    ))
}

pub async fn run(ctx: &Context, args: PlanArgs) -> Result<()> {
    let overrides = Overrides { image: args.image };
    let resolved = load(ctx, &args.stage.stage, &overrides)?;
    let d = &resolved.deployment;
    let mode = crate::commands::traffic_mode(args.preview.as_deref(), args.traffic, &d.service_id)?;
    let (mut plan, _) = if args.offline {
        let (mut plan, dec) = compute(d, None, &mode, &ctx.progress).await?;
        if d.service.otel_collector.as_ref().is_some_and(|o| !o.pinned) {
            plan.notes.push(
                "offline: the newest otel_collector version was not looked up; deploy uses the latest release".into(),
            );
        }
        (plan, dec)
    } else {
        let session = crate::commands::connect(ctx, d)
            .await
            .map_err(|e| e.hint("use `runway plan --offline` to plan without credentials"))?;
        let run = build_client!(Services, session)?;
        let registry = registry_client(&session).await?;
        let mut version_notes = Vec::new();
        let d = &crate::commands::resolve_runtime_versions(d, &registry, &mut version_notes).await;
        let provisioner = Provisioner::new(d, &session, &run).await?;
        let d =
            &crate::commands::resolve_secret_versions(d, &provisioner, &mut version_notes, false)
                .await?;
        let (mut plan, dec) = compute(
            d,
            Some(Remote {
                run: &run,
                resolver: &registry,
                provisioner: Some(&provisioner),
            }),
            &mode,
            &ctx.progress,
        )
        .await?;
        plan.notes.extend(version_notes);
        (plan, dec)
    };
    plan.notes.dedup();
    match ctx.output {
        OutputFormat::Json => print_json(&plan),
        OutputFormat::Text => print!("{}", render_text(&plan)),
    }
    Ok(())
}
