//! `runway plan`: compute intended changes without mutating anything.
//!
//! With credentials, the plan reads the live service, its IAM policy and the
//! registry (read-only). With `--offline`, nothing is contacted and the plan
//! only describes the desired state.

use crate::build::package::{self, SourceManifest};
use crate::build_client;
use crate::cli::{Context, PlanArgs};
use crate::commands::{load, registry_client, resolve_existing_image};
use crate::config::{Artifact, Deployment, Overrides, Resolved, WorkloadKind};
use crate::deploy::Reconciler;
use crate::error::Result;
use crate::gcp::registry::DigestResolver;
use crate::gcp::run::{self, Ownership};
use crate::image_ref::ImageRef;
use crate::naming;
use crate::output::{OutputFormat, Progress, print_json};
use crate::plan::{
    AccessPlan, BuildPlan, ImagePlan, JobPlan, Plan, ServiceAction, ServiceSpec, StackPlan,
    compute_changes, render_stack_text, render_text,
};
use crate::poll::PollConfig;
use crate::provision::{
    ANNOTATION_GRANTS, Provisioner, Step, StepCheck, StepState, all_steps, managed_grants,
    merge_removals, post_steps, recorded_grants, revoke_steps,
};
use google_cloud_run_v2::client::{Jobs, Revisions, Services};
use std::collections::BTreeMap;
use std::time::Duration;

/// Read-only access to Google Cloud used while planning.
pub struct Remote<'a> {
    pub run: &'a Services,
    /// Reads the revision behind the URL a deploy changes; `None` assumes a
    /// new revision is needed whenever the revision fields differ.
    pub revisions: Option<&'a Revisions>,
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
        &d.image_package(),
    );
    let tagged = format!("{name}:{}", naming::build_image_tag(sha256));
    (name, tagged)
}

/// A removal found by [`Provisioner::unlisted`]: pending by construction.
fn unlisted_check(step: &Step, d: &Deployment) -> StepCheck {
    let detail = match step {
        Step::Revoke(g) => format!("not in runway.yaml: revoke from {}", g.member),
        _ => "not in service.tags: unbind".into(),
    };
    StepCheck {
        step: step.describe(d),
        state: StepState::PendingRemoval,
        detail,
    }
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
                    naming::source_object(&d.image_package(), &archive.sha256)
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

/// Changes to an owned service, as deploy makes them. A preview or canary
/// runs in a revision of its own, which a field diff cannot show. When the
/// revision behind the URL the mode changes already runs the configuration,
/// it keeps serving: only service-level changes remain.
async fn revision_aware_changes(
    svc: &google_cloud_run_v2::model::Service,
    spec: &ServiceSpec,
    image: &ImagePlan,
    remote: Option<&Remote<'_>>,
    progress: &Progress,
    notes: &mut Vec<String>,
) -> (ServiceAction, Vec<crate::plan::FieldChange>) {
    let mode = &spec.traffic.mode;
    let mut spec = spec.clone();
    if let Some((key, value)) = run::revision_marker(mode) {
        spec.revision_annotations.insert(key.to_string(), value);
    }
    let own = run::needs_own_revision(svc, mode);
    let observed = run::observed_flat(svc);
    let new_revision = own
        || crate::plan::diff(&observed, &spec.flatten())
            .iter()
            .any(|c| !run::is_service_level(&c.field));
    if new_revision && image.is_exact() {
        let matching = match remote {
            Some(r) => {
                Reconciler {
                    run: r.run,
                    revisions: r.revisions,
                    progress,
                    poll: PollConfig::default(),
                    timeout: Duration::from_secs(30),
                }
                .matching_revision(svc, &spec)
                .await
            }
            None => None,
        };
        if let Some((target, revision)) = matching {
            notes.push(format!(
                "{revision} already runs this configuration: deploy keeps it serving and creates no revision"
            ));
            spec.traffic.serve = Some(target);
            spec = spec.with_current_traffic(Some(&run::current_traffic(svc)));
            let changes = run::pending_changes(svc, &spec);
            let action = if changes.is_empty() {
                ServiceAction::NoChange
            } else {
                ServiceAction::Update
            };
            return (action, changes);
        }
    }
    let (mut action, mut changes) = compute_changes(Some(&observed), &spec, image);
    if own && !changes.iter().any(|c| !run::is_service_level(&c.field)) {
        changes.push(crate::plan::FieldChange {
            field: "revision".into(),
            before: None,
            after: Some(match mode {
                crate::traffic::Mode::Preview { tag } => {
                    format!("new, of its own for preview `{tag}` (same configuration)")
                }
                _ => "new, of its own for the canary (same configuration)".into(),
            }),
        });
        action = ServiceAction::Update;
    }
    (action, changes)
}

/// Computes a plan; `remote = None` means offline. With `stack`, the service
/// is one of several: only its own steps (tags, IAP) and removals are
/// checked here, stage-wide ones by [`stack_plan`].
pub async fn compute(
    d: &Deployment,
    remote: Option<Remote<'_>>,
    mode: &crate::traffic::Mode,
    progress: &Progress,
    stack: Option<&Resolved>,
) -> Result<(Plan, ImageDecision)> {
    let mut notes = Vec::new();
    let name = d.service_name();
    let all_steps = match stack {
        None => all_steps(d),
        Some(_) => post_steps(d),
    };
    // A build shared with other workloads is made for its owner.
    let image_of = stack.map_or(d, |r| r.build_owner(d));
    let provisioner = remote.as_ref().and_then(|r| r.provisioner);
    // Packaging, registry lookup, service read, IAM read and the checks that
    // do not need the service are independent: they run concurrently, so a
    // plan costs about one round trip.
    let (decision, live, early_checks) = match &remote {
        None => (
            decide_image(image_of, None, &mut notes).await?,
            None,
            Vec::new(),
        ),
        Some(r) => {
            let rec = Reconciler {
                run: r.run,
                revisions: r.revisions,
                progress,
                poll: PollConfig::default(),
                timeout: Duration::from_secs(30),
            };
            let early = async {
                match provisioner {
                    Some(prov) => {
                        futures::future::join_all(
                            all_steps
                                .iter()
                                .filter(|s| !s.needs_service())
                                .map(|s| prov.check(s, false)),
                        )
                        .await
                    }
                    None => Vec::new(),
                }
            };
            let mut img_notes = Vec::new();
            let (decision, svc, public, early) = tokio::join!(
                decide_image(image_of, Some(r.resolver), &mut img_notes),
                rec.get(&name),
                rec.current_public(&name),
                early,
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
            (decision?, Some((svc, public)), early)
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
    // Grants recorded by earlier deploys: those removed from runway.yaml are
    // revoked (after the rollout).
    let live_svc = live.as_ref().and_then(|(svc, _)| svc.as_ref());
    let recorded = live_svc
        .map(|svc| recorded_grants(&svc.annotations))
        .unwrap_or_default();
    let domains_previous = live_svc.and_then(|svc| domains_record(&svc.annotations));
    // Only a main deploy revokes (once one revision serves all traffic): a
    // preview or canary leaves them for the next main deploy.
    let mut revokes = match stack {
        None => revoke_steps(&recorded, d),
        Some(_) => Vec::new(),
    };
    if !revokes.is_empty() && *mode != crate::traffic::Mode::Full {
        notes.push(format!(
            "{} grant(s) removed from runway.yaml are revoked by the next main deploy, not by a preview or canary",
            revokes.len()
        ));
        revokes.clear();
    }
    if stack.is_none()
        && live_svc.is_some_and(|svc| !svc.annotations.contains_key(ANNOTATION_GRANTS))
        && !managed_grants(d).is_empty()
    {
        notes.push(format!(
            "runway records the grants it adds on the service ({ANNOTATION_GRANTS}), so that removing one from runway.yaml revokes it; grants already in place are never recorded nor revoked"
        ));
    }
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
                    let (a, c) = revision_aware_changes(
                        &svc,
                        &spec,
                        &decision.image,
                        remote.as_ref(),
                        progress,
                        &mut notes,
                    )
                    .await;
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
    // Service-scoped checks (tags, IAP) once it is known whether the service
    // exists; then every check in step order.
    let service_exists = matches!(live_exists, Some(true));
    let steps: Vec<StepCheck> = match provisioner {
        Some(prov) => {
            // Access and tags runway.yaml does not list are removed by a main
            // deploy, on a service runway owns.
            let authoritative = service_exists
                && *mode == crate::traffic::Mode::Full
                && action != ServiceAction::Conflict;
            let (late, revoked, unlisted) = tokio::join!(
                futures::future::join_all(
                    all_steps
                        .iter()
                        .filter(|s| s.needs_service())
                        .map(|s| prov.check(s, service_exists)),
                ),
                futures::future::join_all(revokes.iter().map(|s| prov.check(s, service_exists))),
                async {
                    match (authoritative, stack) {
                        (true, None) => Some(prov.unlisted(&recorded).await),
                        (true, Some(_)) => Some(prov.unlisted_service().await),
                        (false, _) => None,
                    }
                },
            );
            let unlisted: Vec<StepCheck> = match unlisted {
                None => Vec::new(),
                Some(found) => {
                    let mut checks: Vec<StepCheck> = merge_removals(revokes.clone(), found.steps)
                        .into_iter()
                        .skip(revokes.len())
                        .map(|s| unlisted_check(&s, d))
                        .collect();
                    for what in found.unchecked {
                        notes.push(format!(
                            "could not check for access runway.yaml does not list ({what})"
                        ));
                        checks.push(StepCheck {
                            step: format!(
                                "remove {} runway.yaml does not list",
                                what.split(':').next().unwrap_or("access")
                            ),
                            state: StepState::Unknown,
                            detail: "not checked".into(),
                        });
                    }
                    checks
                }
            };
            // A stage without schedules may have some left from runway.yaml:
            // a full deploy deletes them. Without permission to list them
            // (a stage that never had any), nothing is said.
            let mut unlisted = unlisted;
            if stack.is_none() && authoritative {
                match prov.orphan_schedules().await {
                    Ok(left) => {
                        for s in left {
                            unlisted.push(prov.check(&s, true).await);
                        }
                    }
                    Err(e) if e.kind == crate::error::ErrorKind::Prerequisite => {}
                    Err(e) => notes.push(format!(
                        "could not check for schedules runway created ({})",
                        e.message
                    )),
                }
                // Custom domains runway.yaml no longer has.
                if let Some(previous) = domains_previous {
                    let step = Step::Domains {
                        want: Box::new(crate::domains::desired_of(d, &[])),
                        previous,
                        removals: true,
                    };
                    unlisted.push(prov.check(&step, true).await);
                }
                notes.extend(prov.take_notes());
            }
            let (mut early, mut late) = (early_checks.into_iter(), late.into_iter());
            all_steps
                .iter()
                .filter_map(|s| {
                    if s.needs_service() {
                        late.next()
                    } else {
                        early.next()
                    }
                })
                .chain(revoked)
                .chain(unlisted)
                .collect()
        }
        None => all_steps
            .iter()
            .chain(revokes.iter())
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

/// `--image` applies to the one workload `--only` names.
pub(crate) fn overrides(image: Option<String>, only: &[String]) -> Overrides {
    Overrides {
        image,
        target: match only {
            [one] => Some(one.clone()),
            _ => None,
        },
    }
}

/// A stage planned as before named services and jobs existed: one service.
pub(crate) fn is_single(r: &Resolved) -> bool {
    r.deployments.len() == 1
        && !r.first().is_job()
        && r.schedules.is_empty()
        && !crate::domains::configured(r)
}

pub async fn run(ctx: &Context, args: PlanArgs) -> Result<()> {
    let resolved = load(
        ctx,
        &args.stage.stage,
        &overrides(args.image.clone(), &args.stage.only),
    )?;
    resolved.select(&args.stage.only)?;
    let mode = crate::commands::traffic_mode(
        args.preview.as_deref(),
        args.traffic,
        &crate::commands::preview_basis(&resolved),
    )?;
    if !is_single(&resolved) {
        let plan = stack_plan(ctx, &resolved, &args.stage.only, &mode, args.offline).await?;
        match ctx.output {
            OutputFormat::Json => print_json(&plan),
            OutputFormat::Text => print!("{}", render_stack_text(&plan)),
        }
        return Ok(());
    }
    let d = resolved.first();
    let (mut plan, _) = if args.offline {
        let (mut plan, dec) = compute(d, None, &mode, &ctx.progress, None).await?;
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
        let revisions = build_client!(Revisions, session)?;
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
                revisions: Some(&revisions),
                resolver: &registry,
                provisioner: Some(&provisioner),
            }),
            &mode,
            &ctx.progress,
            None,
        )
        .await?;
        plan.notes.extend(version_notes);
        // Who holds the stage, and the commit that serves.
        if let Ok(svc) = run.get_service().set_name(d.service_name()).send().await
            && run::ownership(&svc, &d.app, &d.stage) == Ownership::Owned
        {
            for l in crate::lease::status(&svc.annotations) {
                plan.notes
                    .push(format!("the stage is busy: {l}; deploy waits for it"));
            }
            let live = crate::source::Source::decode(
                svc.annotations
                    .get(crate::source::ANNOTATION_SOURCE)
                    .map(String::as_str),
            );
            plan.notes.extend(source_note(ctx, &mode, live.as_ref()));
        } else {
            plan.notes.extend(source_note(ctx, &mode, None));
        }
        (plan, dec)
    };
    plan.notes.dedup();
    match ctx.output {
        OutputFormat::Json => print_json(&plan),
        OutputFormat::Text => print!("{}", render_text(&plan)),
    }
    Ok(())
}

/// The commit a deploy in `mode` would come from, against the one that
/// serves (previews do not change what serves).
fn source_note(
    ctx: &Context,
    mode: &crate::traffic::Mode,
    live: Option<&crate::source::Source>,
) -> Option<String> {
    if matches!(mode, crate::traffic::Mode::Preview { .. }) {
        return None;
    }
    let dir = crate::commands::config_dir(ctx);
    crate::source::plan_note(&dir, crate::source::current(&dir).as_ref(), live)
}

/// The job a deploy in `mode` changes: the job itself, or with `--preview`
/// its preview copy (`{job}-{tag}`, labeled with the tag).
pub(crate) fn job_for_mode(d: &Deployment, mode: &crate::traffic::Mode) -> Result<Deployment> {
    let mut d = d.clone();
    if let crate::traffic::Mode::Preview { tag } = mode {
        d.service_id = crate::commands::preview_job_id(&d.service_id, tag)?;
    }
    Ok(d)
}

/// Labels of a job deployed in `mode` (a preview copy carries its tag).
pub(crate) fn job_labels(d: &Deployment, mode: &crate::traffic::Mode) -> BTreeMap<String, String> {
    let mut l = d.labels();
    if let crate::traffic::Mode::Preview { tag } = mode {
        l.insert(naming::LABEL_PREVIEW.into(), tag.clone());
    }
    l
}

/// What deploy changes on a job.
async fn job_plan(
    d: &Deployment,
    owner: &Deployment,
    remote: Option<(&Jobs, &dyn DigestResolver)>,
    mode: &crate::traffic::Mode,
    progress: &Progress,
) -> Result<JobPlan> {
    let WorkloadKind::Job(settings) = &d.kind else {
        return Err(crate::error::Error::internal("not a job"));
    };
    let mut notes = Vec::new();
    let target = job_for_mode(d, mode)?;
    let decision = decide_image(owner, remote.map(|r| r.1), &mut notes).await?;
    let mut spec = ServiceSpec::from_deployment(
        &target,
        decision.image.deploy_reference().unwrap_or(""),
        decision.annotations.clone(),
    );
    spec.labels = job_labels(d, mode);
    let desired = crate::gcp::jobs::desired_flat(&spec, settings);
    if let crate::traffic::Mode::Canary { .. } = mode {
        notes.push(
            "a canary changes services only: jobs and schedules change with a full deploy".into(),
        );
        return Ok(JobPlan {
            job: target.service_id.clone(),
            name: d.name().to_string(),
            action: ServiceAction::NoChange,
            changes: Vec::new(),
            image: decision.image,
            build: decision.build,
            notes,
        });
    }
    let (action, changes) = match remote {
        None => {
            let (_, c) = crate::plan::job_changes(None, desired, &decision.image);
            (ServiceAction::Unknown, c)
        }
        Some((jobs, _)) => {
            let rec = crate::gcp::jobs::JobReconciler {
                jobs,
                progress,
                poll: PollConfig::default(),
                timeout: Duration::from_secs(30),
            };
            match rec.get(&target.service_name()).await? {
                None => crate::plan::job_changes(None, desired, &decision.image),
                Some(job) => match crate::gcp::jobs::check_job_ownership(&job, &d.app, &d.stage) {
                    Err(e) => {
                        notes.push(format!("{}; deploy will refuse", e.message));
                        (ServiceAction::Conflict, Vec::new())
                    }
                    Ok(()) => crate::plan::job_changes(
                        Some(&crate::gcp::jobs::observed_flat(&job)),
                        desired,
                        &decision.image,
                    ),
                },
            }
        }
    };
    Ok(JobPlan {
        job: target.service_id.clone(),
        name: d.name().to_string(),
        action,
        changes,
        image: decision.image,
        build: decision.build,
        notes,
    })
}

/// Grants recorded on the stage's holder (see [`Resolved::holder`]).
pub async fn holder_record(
    r: &Resolved,
    run: &Services,
    jobs: &Jobs,
) -> Result<Vec<crate::provision::ManagedGrant>> {
    Ok(holder_records(r, run, jobs).await?.grants)
}

/// Grants and the custom domains record (see
/// [`crate::domains::ANNOTATION_DOMAINS`]) on the stage's holder.
pub async fn holder_records(r: &Resolved, run: &Services, jobs: &Jobs) -> Result<HolderRecords> {
    let h = r.holder();
    let live = match h.is_job() {
        false => match run.get_service().set_name(h.service_name()).send().await {
            Ok(svc) => Some((svc.labels, svc.annotations)),
            Err(e) if crate::gcp::is_not_found(&e) => None,
            Err(e) => return Err(crate::gcp::api_error(e, "reading the service")),
        },
        true => match jobs.get_job().set_name(h.service_name()).send().await {
            Ok(j) => Some((j.labels, j.annotations)),
            Err(e) if crate::gcp::is_not_found(&e) => None,
            Err(e) => return Err(crate::gcp::api_error(e, "reading the job")),
        },
    };
    let exists = live.is_some();
    let (labels, annotations) = live.unwrap_or_default();
    // Annotations on a resource runway does not own are not its record.
    if run::ownership_of(&labels, &h.app, &h.stage) != Ownership::Owned {
        return Ok(HolderRecords {
            exists,
            ..Default::default()
        });
    }
    Ok(HolderRecords {
        exists,
        owned: exists,
        grants: recorded_grants(&annotations),
        domains: domains_record(&annotations),
        source: crate::source::Source::decode(
            annotations
                .get(crate::source::ANNOTATION_SOURCE)
                .map(String::as_str),
        ),
        lease: crate::lease::status(&annotations),
    })
}

/// What runway records on the stage's holder (see [`Resolved::holder`]),
/// when runway owns it.
#[derive(Debug, Clone, Default)]
pub struct HolderRecords {
    /// The holder exists (owned or not).
    pub exists: bool,
    /// It exists and runway owns it for this app and stage.
    pub owned: bool,
    pub grants: Vec<crate::provision::ManagedGrant>,
    pub domains: Option<crate::domains::Recorded>,
    /// The commit that serves.
    pub source: Option<crate::source::Source>,
    /// Who holds the stage's lease (descriptions).
    pub lease: Vec<String>,
}

pub fn domains_record(
    annotations: &std::collections::HashMap<String, String>,
) -> Option<crate::domains::Recorded> {
    annotations
        .get(crate::domains::ANNOTATION_DOMAINS)
        .map(|v| crate::domains::Recorded::decode(v))
}

/// Stage-wide steps (once) and removals, then each selected service and
/// job.
async fn stack_plan(
    ctx: &Context,
    r: &Resolved,
    only: &[String],
    mode: &crate::traffic::Mode,
    offline: bool,
) -> Result<StackPlan> {
    let selected = r.select(only)?;
    let first = r.first();
    let progress = &ctx.progress;
    let mut notes = Vec::new();
    let mut services = Vec::new();
    let mut job_plans = Vec::new();
    let full = *mode == crate::traffic::Mode::Full;
    let stage_steps: Vec<Step> = crate::provision::stack_api_step(r)
        .into_iter()
        .chain(crate::provision::stack_pre_steps(r))
        .collect();
    // Schedules change with a full deploy, for the targets it deploys.
    let schedule_steps: Vec<Step> = match full {
        false => Vec::new(),
        true => crate::provision::schedule_steps(r)
            .into_iter()
            .filter(|s| match s {
                Step::Schedule(sc) => selected
                    .iter()
                    .any(|d| d.service_id == sc.target.resource_id()),
                Step::Grant { binding, .. } => selected.iter().any(|d| {
                    binding
                        .target
                        .to_string()
                        .ends_with(&format!(" {}", d.service_id))
                }),
                _ => true,
            })
            .collect(),
    };
    if !full && !r.schedules.is_empty() {
        notes.push("schedules change with a full deploy, not a preview or canary".into());
    }
    if offline {
        for d in &selected {
            match d.is_job() {
                true => job_plans.push(job_plan(d, r.build_owner(d), None, mode, progress).await?),
                false => services.push(compute(d, None, mode, progress, Some(r)).await?.0),
            }
        }
        let domains = crate::provision::domains_step(r, None, false).filter(|_| full);
        let steps = stage_steps
            .iter()
            .chain(schedule_steps.iter())
            .chain(domains.iter())
            .map(|s| StepCheck {
                step: s.describe(first),
                state: StepState::Unknown,
                detail: "offline".into(),
            })
            .collect();
        return Ok(stack_result(r, false, services, job_plans, steps, notes));
    }
    let session = crate::commands::connect(ctx, first)
        .await
        .map_err(|e| e.hint("use `runway plan --offline` to plan without credentials"))?;
    let run = build_client!(Services, session)?;
    let revisions = build_client!(Revisions, session)?;
    let jobs = build_client!(Jobs, session)?;
    let registry = registry_client(&session).await?;
    let prov = Provisioner::new(first, &session, &run).await?.with_stack(r);
    let mut ready: Vec<Deployment> = Vec::new();
    for d in &selected {
        let d = crate::commands::resolve_runtime_versions(d, &registry, &mut notes).await;
        ready.push(crate::commands::resolve_secret_versions(&d, &prov, &mut notes, false).await?);
    }
    for d in &ready {
        if d.is_job() {
            job_plans.push(
                job_plan(
                    d,
                    r.build_owner(d),
                    Some((&jobs, &registry)),
                    mode,
                    progress,
                )
                .await?,
            );
        } else {
            let view = prov.for_service(d);
            let (plan, _) = compute(
                d,
                Some(Remote {
                    run: &run,
                    revisions: Some(&revisions),
                    resolver: &registry,
                    provisioner: Some(&view),
                }),
                mode,
                progress,
                Some(r),
            )
            .await?;
            services.push(plan);
        }
    }
    let mut steps: Vec<StepCheck> =
        futures::future::join_all(stage_steps.iter().map(|s| prov.check(s, false))).await;
    steps.extend(
        futures::future::join_all(schedule_steps.iter().map(|s| prov.check(s, true))).await,
    );
    // Removals: only a full deploy of every service and job makes them.
    let records = holder_records(r, &run, &jobs).await?;
    let (recorded, domains_previous) = (records.grants.clone(), records.domains.clone());
    for l in &records.lease {
        notes.push(format!("the stage is busy: {l}; deploy waits for it"));
    }
    notes.extend(source_note(ctx, mode, records.source.as_ref()));
    // Custom domains change with a full deploy (removals: of everything).
    match crate::provision::domains_step(r, domains_previous, only.is_empty()) {
        Some(s) if full => steps.push(prov.check(&s, true).await),
        Some(_) => notes.push("custom domains change with a full deploy".into()),
        None => {}
    }
    let revokes = crate::provision::stack_revoke_steps(&recorded, r);
    if full && only.is_empty() {
        steps.extend(futures::future::join_all(revokes.iter().map(|s| prov.check(s, true))).await);
        let shared = prov.unlisted_shared(&recorded).await;
        for s in merge_removals(revokes.clone(), shared.steps)
            .into_iter()
            .skip(revokes.len())
        {
            steps.push(unlisted_check(&s, first));
        }
        for what in shared.unchecked {
            notes.push(format!(
                "could not check for access runway.yaml does not list ({what})"
            ));
        }
        match prov.orphan_schedules().await {
            Ok(left) => {
                for s in left {
                    steps.push(prov.check(&s, true).await);
                }
            }
            Err(e) => notes.push(format!(
                "could not check for schedules runway created ({})",
                e.message
            )),
        }
        for o in crate::commands::undeploy::orphan_workloads(r, &run, &jobs).await? {
            notes.push(format!(
                "{} {} is no longer in runway.yaml; remove it with `runway undeploy --stage {} --orphans`",
                if o.is_job() { "job" } else { "service" },
                o.service_id,
                first.stage
            ));
        }
    } else if !revokes.is_empty() {
        notes.push(format!(
            "{} grant(s) removed from runway.yaml are revoked by a full deploy of every service and job",
            revokes.len()
        ));
    }
    notes.extend(prov.take_notes());
    Ok(stack_result(r, true, services, job_plans, steps, notes))
}

fn stack_result(
    r: &Resolved,
    remote: bool,
    services: Vec<Plan>,
    jobs: Vec<JobPlan>,
    steps: Vec<StepCheck>,
    mut notes: Vec<String>,
) -> StackPlan {
    let d = r.first();
    notes.dedup();
    let exact = remote
        && services.iter().all(|p| p.exact)
        && jobs
            .iter()
            .all(|j| j.image.is_exact() && j.action != ServiceAction::Conflict)
        && steps.iter().all(|s| s.state != StepState::Unknown);
    StackPlan {
        app: d.app.clone(),
        stage: d.stage.clone(),
        project: d.project.clone(),
        region: d.region.clone(),
        exact,
        remote_inspected: remote,
        services,
        jobs,
        steps,
        notes,
    }
}
