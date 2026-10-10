//! `runway deploy`: provision identity and grants, build if necessary, deploy,
//! wait for readiness, bind tags, configure IAP and access.
//!
//! Every step is idempotent (it re-reads the live state first) and is retried
//! according to the `retry` configuration / `--retries`.

use crate::build::cloudbuild::{BuildInputs, Builder, BuiltImage};
use crate::build_client;
use crate::cli::{Context, DeployArgs};
use crate::commands::{load, registry_client};
use crate::config::{Artifact, Deployment, ReleaseFlag};
use crate::deploy::{AccessChange, Applied, Reconciler, ServiceChange, Target, check_ownership};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::run::{self, Readiness};
use crate::naming;
use crate::output::{OutputFormat, Progress, print_json};
use crate::plan::{ImagePlan, ServiceSpec, diff};
use crate::poll::PollConfig;
use crate::provision::{
    ANNOTATION_GRANTS, Provisioner, StepOutcome, StepResult, api_step, encode_grants, grant_record,
    managed_grants, merge_removals, post_steps, pre_steps, recorded_grants, revoke_steps,
};
use crate::retry::{RetryConfig, with_retry};
use futures::TryFutureExt;
use google_cloud_build_v1::client::CloudBuild;
use google_cloud_logging_v2::client::LoggingServiceV2;
use google_cloud_run_v2::client::{Revisions, Services};
use google_cloud_run_v2::model::Service;
use google_cloud_storage::client::Storage;
use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Serialize)]
pub struct DeployResult {
    pub app: String,
    pub stage: String,
    pub project: String,
    pub region: String,
    pub service: String,
    pub url: Option<String>,
    pub revision: String,
    pub image: String,
    pub change: ServiceChange,
    /// Every field the rollout changed (all of them for a new service).
    pub changes: Vec<crate::plan::FieldChange>,
    pub public: bool,
    pub access_change: AccessChange,
    pub iap: bool,
    /// Provisioning steps (identity, grants, tags, IAP) and what each did.
    pub steps: Vec<StepResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuiltImage>,
    /// Release tag applied from the changelog (`--tag` / `--tag-rc`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<crate::build::release::ReleaseTag>,
    /// How traffic was handled (full, preview or canary).
    pub traffic_mode: crate::traffic::Mode,
    /// URL of the preview or canary revision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_url: Option<String>,
    pub traffic: Vec<run::TrafficLine>,
    pub duration_seconds: u64,
}

/// Retry policy from the configuration, overridden by `--retries`/`--retry-delay`.
pub fn effective_retry(d: &Deployment, args: &DeployArgs) -> RetryConfig {
    let mut r = d.retry;
    if let Some(n) = args.retries {
        r.attempts = n.saturating_add(1);
    }
    if let Some(delay) = args.retry_delay {
        r.delay = delay;
        r.max_delay = r.max_delay.max(delay);
    }
    r
}

/// A provisioning step as it completes: what it is, then what it did
/// (several changes on lines of their own). Nothing is left out.
pub(crate) fn report_step(p: &Progress, r: &StepResult) {
    let (first, rest) = match r.detail.split_once('\n') {
        Some((a, b)) => (a, Some(b)),
        None => (r.detail.as_str(), None),
    };
    let line = match first.is_empty() {
        true => r.step.clone(),
        false => format!("{}: {first}", r.step),
    };
    match r.outcome {
        StepOutcome::Changed => p.success(line),
        StepOutcome::Unchanged => p.info(format!("= {line}")),
    }
    if let Some(rest) = rest {
        p.details(rest);
    }
}

/// Wraps a failure that happens after the service is live.
pub(crate) fn after_rollout(e: Error, d: &Deployment, url: Option<&str>) -> Error {
    let mut out = Error::new(
        e.kind,
        format!(
            "service {} is deployed and ready{}, but a later step failed: {}",
            d.service_id,
            url.map(|u| format!(" at {u}")).unwrap_or_default(),
            e.message
        ),
    )
    .hint("fix the cause and re-run `runway deploy`: completed steps are detected and skipped");
    out.hints.extend(e.hints);
    out
}

pub async fn run(ctx: &Context, args: DeployArgs) -> Result<()> {
    let started = Instant::now();
    let overrides = crate::commands::plan::overrides(args.image.clone(), &args.stage.only);
    let flag = match (args.tag, args.tag_rc) {
        (true, _) => Some(ReleaseFlag::Tag),
        (_, true) => Some(ReleaseFlag::TagRc),
        _ => None,
    };
    let stage = deploy_stage(
        &crate::config::load(&ctx.config)?,
        args.stage.stage.as_deref(),
        flag,
    )?;
    if let Some(f) = flag {
        ctx.progress
            .info(format!("--{}: stage {stage}", f.as_str()));
    }
    let cfg = crate::config::load(&ctx.config)?;
    if args.tag && args.force_build && crate::commands::release::promotes(&cfg) {
        return Err(Error::config(
            "--tag deploys the latest release candidate's image (a stage is mapped to tag-rc): --force-build would release an untested build",
        ));
    }
    let resolved = load(ctx, &stage, &overrides)?;
    if args.force_build
        && let Some(from) = &resolved.first().promote
    {
        return Err(Error::config(format!(
            "stage {stage} promotes stage {from}'s images (`stages.{stage}.promote.from`): --force-build would deploy an untested build"
        )));
    }
    let selected = resolved.select(&args.stage.only)?;
    if crate::commands::plan::is_single(&resolved) {
        return run_single(ctx, args, &resolved, started, &cfg).await;
    }
    crate::commands::deploy_stack::run(ctx, &args, &resolved, &selected, started, &cfg).await
}

/// The stage to deploy: `--stage`, or with `--tag`/`--tag-rc` the stage
/// runway.yaml maps to the flag. Once a stage is mapped to a flag, the flag
/// deploys no other stage; without a mapping, `--stage` is required (as
/// before release stages existed).
pub fn deploy_stage(
    cfg: &crate::config::LoadedConfig,
    stage: Option<&str>,
    flag: Option<ReleaseFlag>,
) -> Result<String> {
    let mapped = flag.map(|f| cfg.release_stages(f)).unwrap_or_default();
    let option = flag.map_or("", |f| f.as_str());
    match (stage, mapped.as_slice()) {
        (Some(s), []) => Ok(s.to_string()),
        (None, []) => Err(Error::config("--stage is required").hint(match flag {
            Some(f) => format!(
                "or map a stage to --{} in runway.yaml: `stages.<name>.release.flag: {}`",
                f.as_str(),
                f.as_str()
            ),
            None => "pass --stage NAME (or set RUNWAY_STAGE)".into(),
        })),
        (None, [one]) => Ok(one.clone()),
        (None, many) => Err(Error::config(format!(
            "several stages are mapped to --{option} ({}): choose one with --stage",
            many.join(", ")
        ))),
        (Some(s), many) if many.iter().any(|m| m == s) => Ok(s.to_string()),
        (Some(s), many) => Err(Error::config(format!(
            "--{option} deploys stage {} only (stages.<name>.release.flag), not `{s}`",
            many.join(", ")
        ))
        .hint(format!(
            "deploy `{s}` without --{option}, or map it with `stages.{s}.release.flag: {option}`"
        ))),
    }
}

/// A stage with one service (and no jobs or schedules), deployed as before
/// named services and jobs existed.
async fn run_single(
    ctx: &Context,
    args: DeployArgs,
    resolved: &crate::config::Resolved,
    started: Instant,
    cfg: &crate::config::LoadedConfig,
) -> Result<()> {
    let d = resolved.first();
    let p = &ctx.progress;
    let name = d.service_name();
    let retry = effective_retry(d, &args);

    // Release tag: read the changelog before doing anything else.
    let release_request = crate::commands::deploy_stack::release_request(ctx, &args, &[d])?;

    let mode = crate::commands::traffic_mode(args.preview.as_deref(), args.traffic, &d.service_id)?;
    p.step(format!(
        "Deploying {} (stage {}) to {}/{}{}",
        d.service_id,
        d.stage,
        d.project,
        d.region,
        match &mode {
            crate::traffic::Mode::Full => String::new(),
            crate::traffic::Mode::Preview { tag } => format!(", preview `{tag}` without traffic"),
            crate::traffic::Mode::Canary { percent } => format!(", canary at {percent}%"),
        }
    ));
    let session = crate::commands::connect(ctx, d).await?;
    // Registry work runway does itself (resolve, copy, tag): maybe another account.
    let push = crate::commands::push_session(ctx, d, &session).await?;
    // The deployment keeps its build settings (its repositories are
    // provisioned); a promoted image only replaces the image it deploys.
    let build_d = d.clone();
    let run_client = build_client!(Services, session)?;
    let revisions = build_client!(Revisions, session)?;
    let jobs = build_client!(google_cloud_run_v2::client::Jobs, session)?;
    let ar = build_client!(
        google_cloud_artifactregistry_v1::client::ArtifactRegistry,
        push
    )?;
    let reconciler = Reconciler {
        run: &run_client,
        revisions: Some(&revisions),
        progress: p,
        poll: PollConfig::default(),
        timeout: args.timeout,
    };

    // 1. APIs first: nothing else can be read or created without them. The
    //    registry login and the runtime version lookup do not need them.
    let provisioner = Provisioner::new(d, &session, &run_client).await?;
    let mut steps = Vec::new();
    let enable_apis = async {
        let Some(step) = api_step(d) else {
            return Ok(None);
        };
        p.step("Enabling required APIs");
        let r = with_retry(&retry, p, &step.describe(d), |_| provisioner.apply(&step)).await?;
        report_step(p, &r);
        Ok::<_, Error>(Some(r))
    };
    let login = async {
        let registry = with_retry(&retry, p, "authenticate", |_| registry_client(&push)).await?;
        let mut notes = Vec::new();
        let resolved = crate::commands::resolve_runtime_versions(d, &registry, &mut notes).await;
        Ok::<_, Error>((registry, resolved, notes))
    };
    let (api, login) = tokio::join!(enable_apis, login);
    steps.extend(api?);
    let (registry, resolved_versions, version_notes) = login?;
    let d = &resolved_versions;
    for n in version_notes {
        p.info(n);
    }

    // The stage's lease: other deploys, canaries, traffic changes and
    // teardowns of the stage wait for this run (previews share it).
    let mut lease = crate::commands::take_lease(
        resolved,
        &run_client,
        None,
        crate::commands::lease_mode(&mode),
        &command_line(&d.stage, &mode),
        &args.lock,
        p,
    )
    .await?;
    let outcome = async {
    // 2. Inspect: read the live service while resolving the image (a build:
    //    hashing the source; a promotion: what the source stage serves), all
    //    read-only. Ownership is checked before any change.
    let ours = crate::commands::this_source(ctx);
    let readers = crate::commands::release::Readers {
        ar: &ar,
        services: &run_client,
        revisions: &revisions,
        jobs: &jobs,
    };
    let req = crate::commands::release::Request {
        version: release_request.as_ref().map(|(k, v, _)| (*k, v.as_str())),
        ours: ours.as_ref(),
    };
    let (mut existing, resolution) = with_retry(&retry, p, "inspect current state", |_| async {
        let mut notes = Vec::new();
        let (existing, resolution) = tokio::join!(
            reconciler.get(&name),
            crate::commands::release::resolve(cfg, &build_d, Some(&readers), Some(&registry), &req, &mut notes)
        );
        Ok((existing?, resolution?))
    })
    .await?;
    let decision = &resolution.decision;
    let promoted = resolution.promoted.is_some();
    if let Some(pr) = &resolution.promoted {
        p.info(format!("deploying {}: {} (no build)", pr.image, pr.why));
    }
    if let Some(svc) = &existing {
        check_ownership(svc, &d.app, &d.stage, args.adopt)?;
    }
    // A first deploy whose service another run created since: take the lease
    // on it (or wait) before changing anything.
    if lease
        .confirm(&d.app, &d.stage, args.lock.wait(), p)
        .await?
    {
        // It was created (and maybe deployed) while this run waited: what was
        // read before is stale (its recorded commit, grants, domains).
        p.info(format!(
            "{} was created by another run meanwhile: reading it again",
            d.service_id
        ));
        existing = with_retry(&retry, p, "inspect current state", |_| reconciler.get(&name))
            .await?;
        if let Some(svc) = &existing {
            check_ownership(svc, &d.app, &d.stage, args.adopt)?;
        }
    }
    let held = lease.held();
    provisioner.hold(held.clone());
    // A deploy of an older commit than the one serving is refused (previews
    // do not change what serves). A promotion's commit is its image's.
    let source = crate::commands::deploy_commit(ctx, &resolution.commit);
    let records_source = !matches!(mode, crate::traffic::Mode::Preview { .. });
    if records_source && resolution.commit != crate::commands::release::Commit::Unknown {
        let live = existing
            .as_ref()
            .filter(|svc| run::ownership(svc, &d.app, &d.stage) == run::Ownership::Owned)
            .and_then(|svc| {
                crate::source::Source::decode(
                    svc.annotations
                        .get(crate::source::ANNOTATION_SOURCE)
                        .map(String::as_str),
                )
            });
        if let Some(l) = &live {
            p.info(format!("deployed: {l}"));
        }
        crate::source::check(
            &crate::commands::config_dir(ctx),
            source.as_ref(),
            live.as_ref(),
            &d.stage,
            args.allow_older,
        )?;
    }
    // Grants recorded on the service by earlier deploys (see `managed_grants`).
    let recorded = existing
        .as_ref()
        .map(|svc| recorded_grants(&svc.annotations))
        .unwrap_or_default();
    // Custom domains set up by an earlier deploy (runway.yaml has none now,
    // or the stage would not be deployed here): removed by a full deploy.
    let domains_previous = existing
        .as_ref()
        .filter(|svc| run::ownership(svc, &d.app, &d.stage) == run::Ownership::Owned)
        .and_then(|svc| crate::commands::plan::domains_record(&svc.annotations));

    // 3. Image: a build, or an existing image (checked before any change).
    let rebuild = match (&d.artifact, &decision.image) {
        _ if promoted => None,
        (Artifact::Build(cfg), img)
            if args.force_build || cfg.rebuild_always || !img.is_exact() =>
        {
            Some(cfg)
        }
        _ => None,
    };
    let existing_image = match (&d.artifact, &decision.image) {
        _ if rebuild.is_some() => None,
        (Artifact::Image { origin, .. }, ImagePlan::Unresolved { reference, reason }) => {
            if reason.contains("no such tag") {
                return Err(Error::prerequisite(format!(
                    "image {reference} was not found in its registry"
                ))
                .hint(format!("push the image first, or fix {origin}")));
            }
            p.warn(format!(
                "deploying tag {reference} without a pinned digest ({reason}); Cloud Run resolves it when the revision is created"
            ));
            Some(reference.clone())
        }
        (_, ImagePlan::Pinned { reference, .. }) => {
            if decision.build.is_some() {
                p.info(format!("image for this source already exists: {reference}"));
            } else {
                p.info(format!("image: {reference}"));
            }
            Some(reference.clone())
        }
        _ => return Err(Error::internal("inconsistent image plan")),
    };

    // 4. Buckets, repository, service accounts, then grants, in waves of
    //    independent steps. A build only waits for its own prerequisites: the
    //    runtime identity, its grants and secrets are provisioned while it runs.
    let pre = pre_steps(d);
    let (before_build, alongside): (Vec<_>, Vec<_>) = if rebuild.is_some() {
        pre.into_iter().partition(|s| s.build_prerequisite(d))
    } else {
        (pre, Vec::new())
    };
    if !before_build.is_empty() || !alongside.is_empty() {
        p.step("Provisioning buckets, identities and grants");
    }
    let report = |r: &StepResult| report_step(p, r);
    let record_now = || grant_record(&recorded, &managed_grants(d), &provisioner.granted(), true);
    match provisioner
        .apply_all(&before_build, &retry, p, &report)
        .await
    {
        Ok(done) => steps.extend(done),
        Err(e) => {
            let _ = save_grants(&reconciler, &name, existing.as_ref(), &record_now(), p).await;
            return Err(e);
        }
    }
    let provision = async {
        let done = provisioner
            .apply_all(&alongside, &retry, p, &report)
            .await?;
        // Every secret exists and has a value: pin the newest versions.
        let (pinned, notes) = with_retry(&retry, p, "resolve secret versions", |_| async {
            let mut notes = Vec::new();
            let pinned =
                crate::commands::resolve_secret_versions(d, &provisioner, &mut notes, true).await?;
            Ok((pinned, notes))
        })
        .await?;
        Ok::<_, Error>((done, pinned, notes))
    }
    .map_err(|e| match rebuild {
        Some(_) => e.hint(
            "a build still running continues in Cloud Build; the next deploy reuses its image",
        ),
        None => e,
    });
    let build = async {
        let Some(cfg) = rebuild else {
            return Ok(None);
        };
        let source = decision
            .source
            .as_ref()
            .ok_or_else(|| Error::internal("scanned source missing"))?;
        let cloudbuild = build_client!(CloudBuild, session)?;
        let storage = Storage::builder()
            .with_credentials(session.credentials.clone())
            .with_retry_policy(crate::gcp::retry_policy())
            .build()
            .await
            .map_err(|e| Error::internal(format!("cannot create Storage client: {e}")))?;
        let logging = build_client!(LoggingServiceV2, session)?;
        p.step(format!(
            "Building {} from {} ({} files, source {})",
            d.app,
            cfg.context_dir.display(),
            source.files,
            naming::short_hash(&source.sha256)
        ));
        if args.force_build {
            p.info("rebuilding: --force-build");
        } else if cfg.rebuild_always {
            p.info("rebuilding: `rebuild: always`");
        } else {
            let deployed = |key: &str| {
                existing
                    .as_ref()
                    .and_then(|svc| svc.annotations.get(key))
                    .map(String::as_str)
            };
            p.info(crate::build::inputs::rebuild_reason(
                deployed(naming::ANNOTATION_SOURCE_HASH),
                deployed(naming::ANNOTATION_BASE_IMAGES),
                &source.sha256,
                &source.bases,
            ));
        }
        let builder = Builder {
            cloudbuild: &cloudbuild,
            uploader: &storage,
            resolver: &registry,
            logging: Some(&logging),
            progress: p,
            poll: PollConfig::default(),
        };
        let package = d.image_package();
        let inputs = BuildInputs {
            project: &d.project,
            region: &d.region,
            app: &package,
            stage: &d.stage,
            config: cfg,
            source,
            image_checked: decision.build.as_ref().and_then(|b| b.will_build) == Some(true),
            timeout: args.build_timeout,
            force: args.force_build || cfg.rebuild_always,
        };
        // A retry re-checks for the image / an in-flight build first, so it
        // never builds the same source twice.
        Ok(Some(
            with_retry(&retry, p, "build image", |_| builder.build(&inputs)).await?,
        ))
    };
    // The first failure stops the deploy; the other branch is not awaited.
    let joined = tokio::try_join!(build, provision);
    // What runway granted so far is recorded now, whatever comes next: a grant
    // made before a failure (an empty secret, a failed build, a rollout
    // refused) would otherwise look pre-existing and never be revoked.
    let saved = save_grants(&reconciler, &name, existing.as_ref(), &record_now(), p).await;
    let (built, (done, pinned_secrets, secret_notes)): (Option<BuiltImage>, _) = joined?;
    // A record written changed the service's etag: the rollout starts from it.
    let existing = match saved? {
        true => reconciler.get(&name).await?,
        false => existing,
    };
    steps.extend(done);
    for n in secret_notes {
        p.warn(n);
    }
    let d = &pinned_secrets;
    let mut image = match (&built, existing_image) {
        (Some(out), _) => out.pinned.clone(),
        (None, Some(reference)) => reference,
        (None, None) => return Err(Error::internal("no image to deploy")),
    };

    // 5. Service.
    // Release tag on the image, once its digest is known.
    let mut annotations = decision.annotations.clone();
    // The rollout carries this run's lease: it creates the service with it
    // (a first deploy), and an update never overwrites another run's.
    if let Some((k, v)) = lease.annotation() {
        annotations.insert(k, v);
    }
    // The grants runway granted (never those it found in place), and those
    // removed from runway.yaml, which stay recorded until they are revoked.
    let desired_grants = managed_grants(d);
    let revokes = revoke_steps(&recorded, d);
    let record = grant_record(&recorded, &desired_grants, &provisioner.granted(), true);
    if !record.is_empty() || !recorded.is_empty() {
        annotations.insert(ANNOTATION_GRANTS.to_string(), encode_grants(&record));
    }
    if let Some(prev) = &domains_previous {
        annotations.insert(
            crate::domains::ANNOTATION_DOMAINS.to_string(),
            prev.encode(),
        );
    }
    // Copied where the stage runs it from (a promotion, a release target),
    // then tagged with the release; that copy is deployed.
    let mut release = None;
    if resolution.destination.is_some() {
        let registry = registry_client(&push).await?;
        let what = match &release_request {
            Some(_) => "publish the release",
            None => "copy the promoted image",
        };
        let (deployed, tag) = with_retry(&retry, p, what, |_| {
            crate::commands::release::publish(
                &ar,
                &registry,
                &resolution,
                &image,
                release_request
                    .as_ref()
                    .map(|(k, v, path)| (*k, v.as_str(), path.as_path())),
            )
        })
        .await?;
        image = deployed;
        if let Some(r) = tag {
            if r.created {
                p.success(format!("tagged {}", r.image));
            } else {
                p.info(format!("= {} already tags this image", r.image));
            }
            annotations.insert(naming::ANNOTATION_RELEASE.to_string(), r.tag.clone());
            release = Some(r);
        }
    }
    let mut base_spec =
        ServiceSpec::from_deployment(d, &image, annotations).with_traffic_mode(mode.clone());
    // The revision says where its image comes from (read with the image when
    // another stage promotes it).
    base_spec.provenance = resolution.provenance(release.as_ref().map(|r| r.tag.as_str()));

    // First deploy with `bootstrap`: a placeholder service that organization
    // policies accept, so the service's tags exist (and are effective) before
    // the real configuration (for example `ingress: all`) is applied.
    let mut existing = existing;
    let mut done_tags: Vec<String> = Vec::new();
    let bootstrapped = if existing.is_none()
        && let Some(bs) = &d.service.bootstrap
    {
        let placeholder = bootstrap_spec(d, bs, &base_spec);
        p.step(match &bs.image {
            None => format!(
                "First deploy: creating {} with ingress {} until its tags are effective",
                d.service_id, bs.ingress
            ),
            Some(img) => format!(
                "First deploy: creating {} from placeholder {img} with ingress {} until its tags are effective",
                d.service_id, bs.ingress
            ),
        });
        let what = if bs.image.is_some() {
            "create the placeholder service"
        } else {
            "create the service"
        };
        with_retry(&retry, p, what, |attempt| {
            let (held, reconciler, name, placeholder) = (&held, &reconciler, &name, &placeholder);
            async move {
                held.check()?;
                roll_out(
                    reconciler,
                    d,
                    name,
                    placeholder,
                    if attempt == 1 { Some(None) } else { None },
                    false,
                    p,
                )
                .await
            }
        })
        .await?;
        existing = reconciler.get(&name).await?;
        Some(bs)
    } else {
        None
    };
    // Whenever the service exists, its tags are bound and awaited before the
    // rollout: an organization policy may need them to accept the new
    // configuration (this also resumes an interrupted first deploy).
    if existing.is_some() {
        let tag_steps: Vec<_> = post_steps(d)
            .into_iter()
            .filter(|s| matches!(s, crate::provision::Step::Tag { .. }))
            .collect();
        if !tag_steps.is_empty() && bootstrapped.is_none() {
            p.step(format!(
                "Binding the tags of {} before the rollout",
                d.service_id
            ));
        }
        for r in provisioner
            .apply_all(&tag_steps, &retry, p, &report)
            .await?
        {
            done_tags.push(r.step.clone());
            steps.push(r);
        }
    }
    p.step(match (bootstrapped, &existing) {
        (Some(bs), _) if bs.image.is_some() => {
            format!("Applying the real configuration to {}", d.service_id)
        }
        (Some(_), _) => format!(
            "Switching {} to ingress {}",
            d.service_id, d.service.ingress
        ),
        (None, None) => format!("Creating Cloud Run service {}", d.service_id),
        (None, Some(_)) => format!("Updating Cloud Run service {}", d.service_id),
    });
    // A tag bound in this run (one an organization policy condition reads)
    // can take a while to reach policy evaluation: in that case organization
    // policy refusals are retried instead of failing fast.
    let tag_just_bound = steps.iter().any(|r| {
        (r.step.starts_with("project tag") || r.step.starts_with("tag "))
            && r.outcome == StepOutcome::Changed
    });
    let (reconciler_ref, name_ref, spec_ref) = (&reconciler, &name, &base_spec);
    let adopt = args.adopt;
    let held_ref = &held;
    let (applied, svc, changes) = with_retry(&retry, p, "deploy service", |attempt| {
        let known = if attempt == 1 { Some(existing.clone()) } else { None };
        async move {
            held_ref.check()?;
            roll_out(reconciler_ref, d, name_ref, spec_ref, known, adopt, p)
                .await
                .map_err(|mut e| {
                    if tag_just_bound && e.permanent && e.message.contains("constraints/") {
                        e.permanent = false;
                        e.hints.insert(
                            0,
                            "a tag was bound moments ago; organization policies can take a few minutes to see it".into(),
                        );
                    }
                    e
                })
        }
    })
    .await?;
    let url = (!svc.uri.is_empty()).then(|| svc.uri.clone());

    // 6. Tags and IAP (before public access, so that a tag allowing public
    //    access is bound before `allUsers` is granted).
    // Then access removed from runway.yaml is revoked, but only by a main
    // deploy after which one revision serves all traffic: a preview, a canary
    // or a revision still serving part of the traffic may need it.
    let revoke_now = mode == crate::traffic::Mode::Full && run::one_revision_serves_all(&svc);
    if !revokes.is_empty() && !revoke_now {
        p.info(format!(
            "{} grant(s) removed from runway.yaml are kept until a main deploy serves all traffic",
            revokes.len()
        ));
    }
    // Plus access and tags runway.yaml does not list, on what runway owns.
    let removals = if revoke_now {
        let unlisted = provisioner.unlisted(&recorded).await;
        for what in &unlisted.unchecked {
            p.warn(format!("could not check for {what}; nothing removed there"));
        }
        let mut removals = merge_removals(revokes.clone(), unlisted.steps);
        // Schedules left after the last one was removed from runway.yaml.
        // Without permission to list them (a stage that never had any),
        // nothing is said.
        match provisioner.orphan_schedules().await {
            Ok(left) => removals.extend(left),
            Err(e) if e.kind == ErrorKind::Prerequisite => {
                tracing::debug!(error = %e.message, "schedules not checked");
            }
            Err(e) => p.warn(format!(
                "could not check for schedules runway created ({}); none deleted",
                e.message
            )),
        }
        removals
    } else {
        Vec::new()
    };
    let remaining: Vec<_> = post_steps(d)
        .into_iter()
        .filter(|s| !done_tags.contains(&s.describe(d)))
        .chain(removals)
        .collect();
    match provisioner.apply_all(&remaining, &retry, p, &report).await {
        Ok(done) => steps.extend(done),
        Err(e) => {
            let live = reconciler.get(&name).await.ok().flatten();
            let record = grant_record(&recorded, &desired_grants, &provisioner.granted(), true);
            let _ = save_grants(&reconciler, &name, live.as_ref(), &record, p).await;
            return Err(after_rollout(e, d, url.as_deref()));
        }
    }
    // IAP members granted after the rollout, and revocations done.
    let final_record = grant_record(
        &recorded,
        &desired_grants,
        &provisioner.granted(),
        !revoke_now,
    );
    if final_record != record {
        let value = encode_grants(&final_record);
        with_retry(&retry, p, "record grants", |_| {
            reconciler.set_annotation(&name, ANNOTATION_GRANTS, &value)
        })
        .await
        .map_err(|e| after_rollout(e, d, url.as_deref()))?;
    }
    if let Some(previous) = domains_previous.filter(|_| mode == crate::traffic::Mode::Full) {
        p.step("Removing custom domains runway.yaml no longer has");
        let step = crate::provision::Step::Domains {
            want: Box::new(crate::domains::desired_of(d, &[])),
            previous,
            removals: true,
        };
        let holder = crate::commands::deploy_stack::Holder {
            d,
            adopting: args.adopt,
        };
        let done = crate::commands::deploy_stack::apply_domains(
            &provisioner,
            &reconciler,
            &holder,
            &step,
            &retry,
            p,
            &report,
        )
        .await
        .map_err(|e| after_rollout(e, d, url.as_deref()))?;
        steps.extend(done);
    }

    // 7. Public access.
    let access_change = with_retry(&retry, p, "invoker access", |_| async {
        held.check()?;
        reconciler.ensure_access(&name, d.service.public).await
    })
    .await
    .map_err(|e| after_rollout(e, d, url.as_deref()))?;

    // What serves now comes from this commit: a later deploy of an older one
    // is refused. A promoted image of unknown commit removes the record
    // rather than claim this checkout's.
    let record = match (&resolution.commit, &source) {
        (crate::commands::release::Commit::Unknown, _) => Some(None),
        (_, Some(src)) => Some(Some(src.encode())),
        (_, None) => None,
    };
    if records_source && let Some(value) = record {
        with_retry(&retry, p, "record the deployed commit", |_| async {
            held.check()?;
            reconciler
                .put_annotation(&name, crate::source::ANNOTATION_SOURCE, value.as_deref())
                .await
        })
        .await
        .map_err(|e| after_rollout(e, d, url.as_deref()))?;
    }

    let result = DeployResult {
        app: d.app.clone(),
        stage: d.stage.clone(),
        project: d.project.clone(),
        region: d.region.clone(),
        service: d.service_id.clone(),
        url: url.clone(),
        // The revision behind the URL this deploy targets (with an existing
        // revision kept serving, not necessarily the latest one).
        revision: run::mode_target(&run::current_traffic(&svc), &mode)
            .map(|(_, r)| r)
            .unwrap_or_else(|| run::short_revision(&svc.latest_ready_revision).to_string()),
        image,
        change: applied.change,
        changes,
        public: d.service.public,
        access_change,
        iap: d.service.iap.enabled,
        steps,
        build: built,
        release,
        revision_url: match &mode {
            crate::traffic::Mode::Full => None,
            crate::traffic::Mode::Preview { tag } => tagged_url(&svc, tag),
            crate::traffic::Mode::Canary { .. } => tagged_url(&svc, crate::traffic::CANARY_TAG),
        },
        traffic_mode: mode.clone(),
        traffic: run::status(&svc, &d.app, &d.stage).traffic,
        duration_seconds: started.elapsed().as_secs(),
    };

    match ctx.output {
        OutputFormat::Json => print_json(&result),
        OutputFormat::Text => print_text(p, &result),
    }
    if url.is_none() {
        return Err(Error::new(
            ErrorKind::Deploy,
            "the service is ready but reported no URL",
        ));
    }
    Ok(())
    }
    .await;
    lease.release(p).await;
    outcome
}

/// `runway deploy --stage S [--preview T | --traffic N]`, for leases.
pub(crate) fn command_line(stage: &str, mode: &crate::traffic::Mode) -> String {
    format!(
        "runway deploy --stage {stage}{}",
        match mode {
            crate::traffic::Mode::Full => String::new(),
            crate::traffic::Mode::Preview { tag } => format!(" --preview {tag}"),
            crate::traffic::Mode::Canary { percent } => format!(" --traffic {percent}"),
        }
    )
}

/// [`Reconciler::save_grant_record`], warning when it fails: before a failure
/// is reported, saving is best effort (the failure is what matters).
pub(crate) async fn save_grants(
    reconciler: &Reconciler<'_>,
    name: &str,
    live: Option<&Service>,
    record: &[crate::provision::ManagedGrant],
    p: &Progress,
) -> Result<bool> {
    reconciler
        .save_grant_record(name, live, record)
        .await
        .inspect_err(|e| {
            p.warn(format!(
                "cannot record the grants runway added ({}); removing them from runway.yaml will not revoke them",
                e.message
            ))
        })
}

/// One idempotent roll-out attempt: re-reads the service (except on the first
/// attempt), checks ownership, applies changes and waits for readiness.
pub(crate) async fn roll_out(
    reconciler: &Reconciler<'_>,
    d: &Deployment,
    name: &str,
    base_spec: &ServiceSpec,
    known: Option<Option<Service>>,
    adopt: bool,
    p: &Progress,
) -> Result<(Applied, Service, Vec<crate::plan::FieldChange>)> {
    let existing = match known {
        Some(e) => e,
        None => reconciler.get(name).await?,
    };
    if let Some(svc) = &existing {
        check_ownership(svc, &d.app, &d.stage, adopt)?;
    }
    let mut spec = run::spec_for_live(base_spec, existing.as_ref());
    // A preview or canary always runs in a revision of its own (marked by a
    // template annotation), even when its code and configuration equal what
    // already serves: otherwise its URL would point at the production revision.
    if let Some((key, value)) = run::revision_marker(&spec.traffic.mode) {
        spec.revision_annotations.insert(key.to_string(), value);
    }
    let mut force = existing
        .as_ref()
        .is_some_and(|s| run::needs_own_revision(s, &spec.traffic.mode));
    // The revision behind the URL this deploy changes (the main URL, the
    // preview's tag or the canary) may already run this configuration: it
    // then keeps serving, and no revision is created nor traffic moved.
    if let Some(svc) = &existing {
        let new_revision = force
            || diff(&run::observed_flat(svc), &spec.flatten())
                .iter()
                .any(|c| !run::is_service_level(&c.field));
        if new_revision
            && let Some((target, revision)) = reconciler.matching_revision(svc, &spec).await
        {
            p.info(format!(
                "{revision} already runs this configuration: no new revision"
            ));
            spec.traffic.serve = Some(target);
            spec = spec.with_current_traffic(Some(&run::current_traffic(svc)));
            force = false;
        }
    }
    // An owned service whose spec already matches but whose latest revision
    // failed gets a fresh revision (for example after a grant propagated).
    if !force
        && spec.traffic.serve.is_none()
        && let Some(svc) = &existing
        && diff(&run::observed_flat(svc), &spec.flatten()).is_empty()
        && matches!(run::readiness(svc), Readiness::Failed { .. })
    {
        p.info("configuration unchanged but the latest revision is failing; rolling out a new revision");
        spec.revision_annotations.insert(
            "runway.dev/redeployed-at".into(),
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        force = true;
    }
    // Every field this rollout changes (all of them for a new service).
    let changes: Vec<crate::plan::FieldChange> = match &existing {
        Some(svc) => run::pending_changes(svc, &spec),
        None => spec
            .flatten()
            .into_iter()
            .map(|(field, v)| crate::plan::FieldChange {
                field,
                before: None,
                after: Some(v),
            })
            .collect(),
    };
    p.details(&change_lines(&changes));
    let parent = d.parent();
    let target = Target {
        parent: &parent,
        service_id: &d.service_id,
        name,
        app: &d.app,
        stage: &d.stage,
        adopt,
        force,
    };
    let applied = reconciler.apply(&target, &spec, existing.clone()).await?;
    let svc = match existing {
        Some(s)
            if applied.change == ServiceChange::Unchanged
                && run::readiness(&s) == Readiness::Ready =>
        {
            p.info("no configuration changes");
            s
        }
        _ => {
            p.step("Waiting for the revision to become ready");
            reconciler.wait_ready(name, &applied).await?
        }
    };
    Ok((applied, svc, changes))
}

/// `+ field: value`, `~ field: before -> after`, `- field: before`, one per
/// line.
pub(crate) fn change_lines(changes: &[crate::plan::FieldChange]) -> String {
    changes
        .iter()
        .filter_map(|c| match (&c.before, &c.after) {
            (Some(b), Some(a)) => Some(format!("~ {}: {b} -> {a}", c.field)),
            (None, Some(a)) => Some(format!("+ {}: {a}", c.field)),
            (Some(b), None) => Some(format!("- {}: {b}", c.field)),
            (None, None) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a first `bootstrap` run creates: by default the real service with the
/// bootstrap ingress (the later switch changes only the service's ingress, no
/// new revision); with `bootstrap.image`, a minimal placeholder.
pub fn bootstrap_spec(
    d: &Deployment,
    bs: &crate::config::BootstrapConfig,
    real: &ServiceSpec,
) -> ServiceSpec {
    let Some(image) = &bs.image else {
        let mut s = real.clone();
        s.ingress = bs.ingress.clone();
        return s;
    };
    ServiceSpec {
        image: image.clone(),
        port: 8080,
        cpu: "1".into(),
        memory: "512Mi".into(),
        timeout_seconds: 300,
        concurrency: 80,
        min_instances: 0,
        max_instances: 1,
        service_account: d.service.service_account.clone(),
        ingress: bs.ingress.clone(),
        health_check: None,
        otel_collector: None,
        sidecars: Default::default(),
        env: Default::default(),
        secrets: Default::default(),
        labels: d.labels(),
        volumes: Default::default(),
        iap_enabled: false,
        billing: crate::config::BILLING_REQUEST.into(),
        startup_cpu_boost: false,
        execution_environment: None,
        sandbox: false,
        command: Vec::new(),
        args: Vec::new(),
        vpc: None,
        cloud_sql: Vec::new(),
        custom_audiences: Vec::new(),
        // The creation lease of a first deploy goes with the placeholder.
        annotations: std::iter::once(("runway.dev/bootstrap".to_string(), image.clone()))
            .chain(
                real.annotations
                    .get(crate::lease::ANNOTATION_LEASE)
                    .map(|v| (crate::lease::ANNOTATION_LEASE.to_string(), v.clone())),
            )
            .collect(),
        revision_annotations: Default::default(),
        // A placeholder runs nothing of the app's.
        provenance: Default::default(),
        traffic: Default::default(),
    }
}

/// URL of a tagged revision, once Cloud Run reports it.
pub(crate) fn tagged_url(svc: &google_cloud_run_v2::model::Service, tag: &str) -> Option<String> {
    svc.traffic_statuses
        .iter()
        .find(|t| t.tag == tag && !t.uri.is_empty())
        .map(|t| t.uri.clone())
}

fn print_text(p: &Progress, r: &DeployResult) {
    use crate::commands::summary as sm;
    let c = crate::style::out();
    p.success(format!(
        "{} {} in {}",
        r.service,
        sm::service_change(r.change),
        crate::lease::short_duration(std::time::Duration::from_secs(r.duration_seconds))
    ));
    let mut s = sm::heading(&c, &format!("Service {}", r.service));
    s.push_str(&sm::field(&c, "Change", sm::service_change(r.change)));
    s.push_str(&sm::field(
        &c,
        "URL",
        &c.bold_cyan(r.url.as_deref().unwrap_or("(none)")),
    ));
    s.push_str(&sm::field(&c, "Revision", &r.revision));
    match (&r.traffic_mode, &r.revision_url) {
        (crate::traffic::Mode::Preview { tag }, url) => s.push_str(&sm::field(
            &c,
            "Preview",
            &format!(
                "{} (tag {tag}, no traffic)",
                c.bold_cyan(url.as_deref().unwrap_or("(URL not reported yet)"))
            ),
        )),
        (crate::traffic::Mode::Canary { percent }, url) => s.push_str(&sm::field(
            &c,
            "Canary",
            &format!(
                "{} ({percent}% of the traffic; `runway traffic --promote` finishes it)",
                c.bold_cyan(url.as_deref().unwrap_or("(URL not reported yet)"))
            ),
        )),
        _ => {}
    }
    s.push_str(&sm::field(&c, "Image", &r.image));
    if let Some(rel) = &r.release {
        s.push_str(&sm::field(&c, "Release", &c.bold_green(&rel.tag)));
    }
    s.push_str(&sm::field(
        &c,
        "Access",
        &sm::access(&c, r.iap, r.public, &r.access_change),
    ));
    s.push_str(&sm::field(&c, "Traffic", ""));
    s.push_str(&sm::traffic(&c, &r.traffic, "    "));
    s.push_str(&sm::heading(
        &c,
        &format!("Changes to {} ({})", r.service, r.changes.len()),
    ));
    s.push_str(&sm::changes(&c, &r.changes, "  "));
    s.push_str(&sm::steps(&c, &r.steps));
    if !r.public
        && !r.iap
        && let Some(u) = &r.url
    {
        s.push_str(&format!(
            "\n{} curl -H \"Authorization: Bearer $(gcloud auth print-identity-token)\" {u}\n",
            c.bold("Try it:")
        ));
    }
    print!("{s}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_config(stages: &str) -> (tempfile::TempDir, crate::config::LoadedConfig) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            format!(
                "version: 1\napp: shop\nprovider: {{project: my-gcp-project, region: europe-west1}}\nservice:\n  image: nginx:1\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\nstages:\n{stages}"
            ),
        )
        .unwrap();
        let cfg = crate::config::load(&p).unwrap();
        (dir, cfg)
    }

    #[test]
    fn release_flags_pick_and_guard_their_stage() {
        use ReleaseFlag::{Tag, TagRc};
        let (_d, cfg) = release_config(
            "  dev: {}\n  staging: {release: {flag: tag-rc}}\n  prod: {release: {flag: tag}}\n",
        );
        assert_eq!(deploy_stage(&cfg, None, Some(Tag)).unwrap(), "prod");
        assert_eq!(deploy_stage(&cfg, None, Some(TagRc)).unwrap(), "staging");
        assert_eq!(deploy_stage(&cfg, Some("prod"), Some(Tag)).unwrap(), "prod");
        let e = deploy_stage(&cfg, Some("dev"), Some(Tag)).unwrap_err();
        assert!(
            e.message.contains("--tag deploys stage prod only"),
            "{}",
            e.message
        );
        assert!(
            deploy_stage(&cfg, None, None).is_err(),
            "no flag: --stage is required"
        );
        assert_eq!(deploy_stage(&cfg, Some("dev"), None).unwrap(), "dev");

        let (_d, cfg) =
            release_config("  eu: {release: {flag: tag}}\n  us: {release: {flag: tag}}\n");
        assert!(
            deploy_stage(&cfg, None, Some(Tag))
                .unwrap_err()
                .message
                .contains("several stages")
        );
        assert_eq!(deploy_stage(&cfg, Some("us"), Some(Tag)).unwrap(), "us");

        // A file without release stages: as before.
        let (_d, cfg) = release_config("  dev: {}\n  prod: {}\n");
        assert_eq!(deploy_stage(&cfg, Some("dev"), Some(Tag)).unwrap(), "dev");
        assert!(deploy_stage(&cfg, None, Some(Tag)).is_err());
    }
    use clap::Parser;

    #[test]
    fn bootstrap_placeholder_is_minimal_and_owned() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            "version: 1\napp: a1\nprovider: {project: my-gcp-project, region: europe-west1}\nservice: {image: nginx@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, service_account: rt@my-gcp-project.iam.gserviceaccount.com, port: 3000, ingress: all, bootstrap: {}, env: {A: b}}\nstages: {dev: {}}\n",
        )
        .unwrap();
        let d = crate::config::load_and_resolve(&p, "dev", &Default::default())
            .unwrap()
            .1
            .deployments[0]
            .clone();
        let real = ServiceSpec::from_deployment(&d, "nginx@sha256:aaaa", Default::default());

        // Default: the real app, with only the ingress restricted.
        let s = bootstrap_spec(&d, d.service.bootstrap.as_ref().unwrap(), &real);
        let mut expected = real.clone();
        expected.ingress = "internal".into();
        assert_eq!(s, expected, "everything else is the real configuration");
        let changes = crate::plan::diff(&s.flatten(), &real.flatten());
        assert_eq!(
            changes.len(),
            1,
            "the later switch only changes ingress: {changes:?}"
        );
        assert_eq!(changes[0].field, "ingress");

        // Opt-in placeholder image.
        let bs = crate::config::BootstrapConfig {
            image: Some(crate::config::HELLO_IMAGE.into()),
            ingress: "internal".into(),
        };
        let s = bootstrap_spec(&d, &bs, &real);
        assert_eq!(s.image, crate::config::HELLO_IMAGE);
        assert_eq!(
            (s.ingress.as_str(), s.port, s.max_instances),
            ("internal", 8080, 1)
        );
        assert!(s.env.is_empty());
        assert_eq!(
            s.service_account,
            "rt@my-gcp-project.iam.gserviceaccount.com"
        );
        assert_eq!(
            s.labels["runway-app"], "a1",
            "owned: the real deploy updates it"
        );
    }

    #[test]
    fn cli_retry_flags_override_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            "version: 1\napp: a1\nprovider: {project: my-gcp-project, region: europe-west1}\nservice: {image: nginx@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, service_account: rt@my-gcp-project.iam.gserviceaccount.com}\nretry: {attempts: 4, delay: 2s, max_delay: 30s}\nstages: {dev: {}}\n",
        )
        .unwrap();
        let d = crate::config::load_and_resolve(&p, "dev", &Default::default())
            .unwrap()
            .1
            .deployments[0]
            .clone();
        let parse = |args: &[&str]| match crate::cli::Cli::parse_from(args).command {
            crate::cli::Command::Deploy(a) => a,
            _ => unreachable!(),
        };
        let a = parse(&["runway", "deploy", "--stage", "dev"]);
        assert_eq!(effective_retry(&d, &a).attempts, 4);
        let a = parse(&["runway", "deploy", "--stage", "dev", "--retries", "0"]);
        assert_eq!(
            effective_retry(&d, &a).attempts,
            1,
            "--retries 0 disables retries"
        );
        let a = parse(&[
            "runway",
            "deploy",
            "--stage",
            "dev",
            "--retries",
            "7",
            "--retry-delay",
            "1m",
        ]);
        let r = effective_retry(&d, &a);
        assert_eq!(r.attempts, 8);
        assert_eq!(r.delay, std::time::Duration::from_secs(60));
        assert!(r.max_delay >= r.delay);
    }
}
