//! `runway deploy` for a stage with several services, jobs or schedules.
//! Stage-wide provisioning runs once (APIs, shared resources and grants),
//! builds that are the same run once, then each service and job is applied,
//! then schedules, then removals. A stage with one service goes through
//! [`super::deploy`] as before.

use crate::build::cloudbuild::{BuildInputs, Builder, BuiltImage};
use crate::build_client;
use crate::cli::{Context, DeployArgs};
use crate::commands::deploy::{
    after_rollout, bootstrap_spec, effective_retry, report_step, roll_out, save_grants, tagged_url,
};
use crate::commands::plan::{ImageDecision, decide_image, holder_record, job_for_mode, job_labels};
use crate::commands::registry_client;
use crate::config::{Artifact, BuildConfig, Deployment, Resolved, WorkloadKind};
use crate::deploy::{AccessChange, Reconciler, ServiceChange, check_ownership};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::jobs::{JobChange, JobReconciler, check_job_ownership};
use crate::gcp::run;
use crate::naming;
use crate::output::{OutputFormat, Progress, print_json};
use crate::plan::{ImagePlan, ServiceSpec};
use crate::poll::PollConfig;
use crate::provision::{
    ANNOTATION_GRANTS, ManagedGrant, Provisioner, Step, StepOutcome, StepResult, encode_grants,
    grant_record, merge_removals, schedule_steps, stack_api_step, stack_managed_grants,
    stack_pre_steps, stack_revoke_steps,
};
use crate::retry::{RetryConfig, with_retry};
use crate::traffic::Mode;
use google_cloud_build_v1::client::CloudBuild;
use google_cloud_logging_v2::client::LoggingServiceV2;
use google_cloud_run_v2::client::{Jobs, Revisions, Services};
use google_cloud_run_v2::model::{Job, Service};
use google_cloud_storage::client::Storage;
use serde::Serialize;
use std::time::Instant;

/// A service of the stage after deploy.
#[derive(Debug, Serialize)]
pub struct ServiceResult {
    pub name: String,
    pub service: String,
    pub url: Option<String>,
    pub revision: String,
    pub image: String,
    pub change: ServiceChange,
    pub public: bool,
    pub access_change: AccessChange,
    pub iap: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_url: Option<String>,
    pub traffic: Vec<run::TrafficLine>,
}

/// A job of the stage after deploy.
#[derive(Debug, Serialize)]
pub struct JobResult {
    pub name: String,
    pub job: String,
    pub image: String,
    pub change: JobChange,
}

#[derive(Debug, Serialize)]
pub struct StackResult {
    pub app: String,
    pub stage: String,
    pub project: String,
    pub region: String,
    pub services: Vec<ServiceResult>,
    pub jobs: Vec<JobResult>,
    /// Provisioning steps (stage-wide, per service, schedules, removals).
    pub steps: Vec<StepResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub builds: Vec<BuiltImage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub releases: Vec<crate::build::release::ReleaseTag>,
    pub traffic_mode: Mode,
    pub duration_seconds: u64,
}

// A handful per deploy: boxing would only add noise.
#[allow(clippy::large_enum_variant)]
enum Live {
    Service(Option<Service>),
    Job(Option<Job>),
}

fn is_holder(d: &Deployment, holder: &Deployment) -> bool {
    d.service_id == holder.service_id && d.is_job() == holder.is_job()
}

/// Where the record goes: the holder, which this deploy may write to only if
/// runway owns it (or adopts it, when it is deployed with `--adopt`).
pub struct Holder<'a> {
    pub d: &'a Deployment,
    pub adopting: bool,
}

/// Saves the grants record on the holder (best effort before a failure is
/// reported). Returns whether something was written. A holder runway does
/// not own (for example an unmanaged main service when `--only` selects a
/// job) is never written to: the grants are then not recorded.
pub async fn save_holder(
    reconciler: &Reconciler<'_>,
    jobs: &JobReconciler<'_>,
    holder: &Holder<'_>,
    record: &[ManagedGrant],
    had_record: bool,
    p: &Progress,
) -> Result<bool> {
    let (holder, adopting) = (holder.d, holder.adopting);
    if record.is_empty() && !had_record {
        return Ok(false);
    }
    let name = holder.service_name();
    let labels = match holder.is_job() {
        true => jobs.get(&name).await?.map(|j| j.labels),
        false => reconciler.get(&name).await?.map(|s| s.labels),
    };
    if let Some(l) = &labels
        && run::ownership_of(l, &holder.app, &holder.stage) != run::Ownership::Owned
        && !adopting
    {
        p.warn(format!(
            "grants not recorded: {} is not managed by runway for this app and stage; removing them from runway.yaml will not revoke them",
            holder.service_id
        ));
        return Ok(false);
    }
    if holder.is_job() {
        let value = encode_grants(record);
        return match jobs.set_annotation(&name, ANNOTATION_GRANTS, &value).await {
            Ok(()) => Ok(true),
            Err(e) => {
                p.warn(format!(
                    "cannot record the grants runway added ({}); removing them from runway.yaml will not revoke them",
                    e.message
                ));
                Err(e)
            }
        };
    }
    let live = reconciler.get(&name).await?;
    save_grants(reconciler, &name, live.as_ref(), record, p).await
}

pub async fn run(
    ctx: &Context,
    args: &DeployArgs,
    r: &Resolved,
    selected: &[&Deployment],
    started: Instant,
) -> Result<()> {
    let p = &ctx.progress;
    let first = r.first();
    let retry = effective_retry(first, args);
    let mode = crate::commands::traffic_mode(
        args.preview.as_deref(),
        args.traffic,
        &crate::commands::preview_basis(r),
    )?;
    if let Mode::Preview { tag } = &mode {
        for d in selected.iter().filter(|d| d.is_job()) {
            crate::commands::preview_job_id(&d.service_id, tag)?;
        }
    }
    let full = mode == Mode::Full;
    let everything = args.stage.only.is_empty();
    let release_request = release_request(ctx, args, selected)?;
    let services_n = selected.iter().filter(|d| !d.is_job()).count();
    let jobs_n = selected.len() - services_n;
    p.step(format!(
        "Deploying {} (stage {}) to {}/{}: {services_n} service(s), {jobs_n} job(s){}{}",
        first.app,
        first.stage,
        first.project,
        first.region,
        match (full, r.schedules.len()) {
            (true, n) if n > 0 => format!(", {n} schedule(s)"),
            _ => String::new(),
        },
        match &mode {
            Mode::Full => String::new(),
            Mode::Preview { tag } => format!(", preview `{tag}` without traffic"),
            Mode::Canary { percent } => format!(", canary at {percent}%"),
        }
    ));
    let session = crate::commands::connect(ctx, first).await?;
    let run_client = build_client!(Services, session)?;
    let revisions = build_client!(Revisions, session)?;
    let jobs_client = build_client!(Jobs, session)?;
    let reconciler = Reconciler {
        run: &run_client,
        revisions: Some(&revisions),
        progress: p,
        poll: PollConfig::default(),
        timeout: args.timeout,
    };
    let jrec = JobReconciler {
        jobs: &jobs_client,
        progress: p,
        poll: PollConfig::default(),
        timeout: args.timeout,
    };
    let provisioner = Provisioner::new(first, &session, &run_client)
        .await?
        .with_stack(r);
    let mut steps: Vec<StepResult> = Vec::new();
    let report = |x: &StepResult| report_step(p, x);

    // 1. APIs first; the registry login and runtime versions do not need them.
    let enable_apis = async {
        let Some(step) = stack_api_step(r) else {
            return Ok(None);
        };
        p.step("Enabling required APIs");
        let res = with_retry(&retry, p, &step.describe(first), |_| {
            provisioner.apply(&step)
        })
        .await?;
        report_step(p, &res);
        Ok::<_, Error>(Some(res))
    };
    let login = async {
        let registry = with_retry(&retry, p, "authenticate", |_| registry_client(&session)).await?;
        let mut notes = Vec::new();
        let mut ws = Vec::new();
        for d in selected {
            ws.push(crate::commands::resolve_runtime_versions(d, &registry, &mut notes).await);
        }
        Ok::<_, Error>((registry, ws, notes))
    };
    let (api, login) = tokio::join!(enable_apis, login);
    steps.extend(api?);
    let (registry, ws, notes) = login?;
    for n in notes {
        p.info(n);
    }

    // 2. Inspect: every workload, one image decision per distinct build, and
    //    the grants recorded on the holder. Ownership is checked first.
    let (mut lives, decisions, by_workload, recorded) =
        with_retry(&retry, p, "inspect current state", |_| async {
            let mut lives = Vec::new();
            for d in &ws {
                lives.push(match d.is_job() {
                    true => Live::Job(jrec.get(&job_for_mode(d, &mode)?.service_name()).await?),
                    false => Live::Service(reconciler.get(&d.service_name()).await?),
                });
            }
            // One decision per distinct build, made for its owner.
            let mut owners: Vec<&Deployment> = Vec::new();
            let mut decisions: Vec<ImageDecision> = Vec::new();
            let mut by_workload = Vec::new();
            for d in &ws {
                let owner = r.build_owner(d);
                let found = d.build_key().and_then(|_| {
                    owners.iter().position(|o| {
                        o.service_id == owner.service_id && o.is_job() == owner.is_job()
                    })
                });
                let i = match found {
                    Some(i) => i,
                    None => {
                        let mut notes = Vec::new();
                        decisions.push(decide_image(owner, Some(&registry), &mut notes).await?);
                        owners.push(owner);
                        decisions.len() - 1
                    }
                };
                by_workload.push(i);
            }
            let recorded = holder_record(r, &run_client, &jobs_client).await?;
            Ok((lives, decisions, by_workload, recorded))
        })
        .await?;
    for (d, live) in ws.iter().zip(&lives) {
        match live {
            Live::Service(Some(svc)) => check_ownership(svc, &d.app, &d.stage, args.adopt)?,
            Live::Job(Some(j)) => check_job_ownership(j, &d.app, &d.stage)?,
            _ => {}
        }
    }
    // The workload whose build produces each decision's image.
    let owners: Vec<&Deployment> = (0..decisions.len())
        .map(|i| {
            let w = by_workload
                .iter()
                .position(|x| *x == i)
                .expect("decision has a workload");
            r.build_owner(&ws[w])
        })
        .collect();

    // 3. Images: builds needed, or existing images (checked before any change).
    let rebuild: Vec<Option<&BuildConfig>> = decisions
        .iter()
        .zip(&owners)
        .map(|(dec, owner)| match (&owner.artifact, &dec.image) {
            (Artifact::Build(cfg), img)
                if args.force_build || cfg.rebuild_always || !img.is_exact() =>
            {
                Some(cfg)
            }
            _ => None,
        })
        .collect();
    let mut existing_images: Vec<Option<String>> = Vec::new();
    for (i, dec) in decisions.iter().enumerate() {
        existing_images.push(match (&owners[i].artifact, &dec.image) {
            _ if rebuild[i].is_some() => None,
            (Artifact::Image { .. }, ImagePlan::Unresolved { reference, reason }) => {
                if reason.contains("no such tag") {
                    return Err(Error::prerequisite(format!(
                        "image {reference} was not found in its registry"
                    ))
                    .hint(format!("push the image first, or fix the `image` of {}", owners[i].what())));
                }
                p.warn(format!(
                    "deploying tag {reference} without a pinned digest ({reason}); Cloud Run resolves it when the revision is created"
                ));
                Some(reference.clone())
            }
            (_, ImagePlan::Pinned { reference, .. }) => {
                p.info(format!("image for {}: {reference}", owners[i].what()));
                Some(reference.clone())
            }
            _ => return Err(Error::internal("inconsistent image plan")),
        });
    }

    // 4. Stage-wide provisioning; builds only wait for their prerequisites.
    let holder_d = r.holder();
    let holder = Holder {
        d: holder_d,
        adopting: args.adopt && selected.iter().any(|d| is_holder(d, holder_d)),
    };
    let desired_grants = stack_managed_grants(r);
    let record_now = || grant_record(&recorded, &desired_grants, &provisioner.granted(), true);
    let pre = stack_pre_steps(r);
    let (before_build, alongside): (Vec<_>, Vec<_>) = if rebuild.iter().any(Option::is_some) {
        pre.into_iter()
            .partition(|s| owners.iter().any(|d| s.build_prerequisite(d)))
    } else {
        (pre, Vec::new())
    };
    if !before_build.is_empty() || !alongside.is_empty() {
        p.step("Provisioning buckets, identities and grants");
    }
    match provisioner
        .apply_all(&before_build, &retry, p, &report)
        .await
    {
        Ok(done) => steps.extend(done),
        Err(e) => {
            let _ = save_holder(
                &reconciler,
                &jrec,
                &holder,
                &record_now(),
                !recorded.is_empty(),
                p,
            )
            .await;
            return Err(e);
        }
    }
    let provision = async {
        let done = provisioner
            .apply_all(&alongside, &retry, p, &report)
            .await?;
        let mut pinned = Vec::new();
        for d in &ws {
            let (x, notes) = with_retry(&retry, p, "resolve secret versions", |_| async {
                let mut notes = Vec::new();
                let x = crate::commands::resolve_secret_versions(d, &provisioner, &mut notes, true)
                    .await?;
                Ok((x, notes))
            })
            .await?;
            for n in notes {
                p.warn(n);
            }
            pinned.push(x);
        }
        Ok::<_, Error>((done, pinned))
    };
    let builds = futures::future::try_join_all(decisions.iter().enumerate().map(|(i, dec)| {
        let session = &session;
        let registry = &registry;
        let rebuild = rebuild[i];
        let owner = owners[i];
        let deployed = lives
            .iter()
            .zip(&ws)
            .find(|(_, d)| d.service_id == owner.service_id)
            .and_then(|(l, _)| match l {
                Live::Service(Some(s)) => Some(s.annotations.clone()),
                Live::Job(Some(j)) => Some(j.annotations.clone()),
                _ => None,
            })
            .unwrap_or_default();
        async move {
            let Some(cfg) = rebuild else {
                return Ok::<_, Error>(None);
            };
            let source = dec
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
                owner.image_package(),
                cfg.context_dir.display(),
                source.files,
                naming::short_hash(&source.sha256)
            ));
            if args.force_build {
                p.info("rebuilding: --force-build");
            } else if cfg.rebuild_always {
                p.info("rebuilding: `rebuild: always`");
            } else {
                p.info(crate::build::inputs::rebuild_reason(
                    deployed
                        .get(naming::ANNOTATION_SOURCE_HASH)
                        .map(String::as_str),
                    deployed
                        .get(naming::ANNOTATION_BASE_IMAGES)
                        .map(String::as_str),
                    &source.sha256,
                    &source.bases,
                ));
            }
            let builder = Builder {
                cloudbuild: &cloudbuild,
                uploader: &storage,
                resolver: registry,
                logging: Some(&logging),
                progress: p,
                poll: PollConfig::default(),
            };
            let package = owner.image_package();
            let inputs = BuildInputs {
                project: &owner.project,
                region: &owner.region,
                app: &package,
                stage: &owner.stage,
                config: cfg,
                source,
                image_checked: dec.build.as_ref().and_then(|b| b.will_build) == Some(true),
                timeout: args.build_timeout,
                force: args.force_build || cfg.rebuild_always,
            };
            Ok(Some(
                with_retry(&retry, p, "build image", |_| builder.build(&inputs)).await?,
            ))
        }
    }));
    let joined = tokio::try_join!(builds, provision);
    let saved = save_holder(
        &reconciler,
        &jrec,
        &holder,
        &record_now(),
        !recorded.is_empty(),
        p,
    )
    .await;
    let (built, (done, pinned)) = joined?;
    // A record written changed the holder's etag: its rollout starts from it.
    if saved? {
        for (d, live) in ws.iter().zip(lives.iter_mut()) {
            if is_holder(d, holder.d) {
                *live = match d.is_job() {
                    true => Live::Job(jrec.get(&job_for_mode(d, &mode)?.service_name()).await?),
                    false => Live::Service(reconciler.get(&d.service_name()).await?),
                };
            }
        }
    }
    steps.extend(done);
    let images: Vec<String> = (0..decisions.len())
        .map(|i| match (&built[i], &existing_images[i]) {
            (Some(out), _) => Ok(out.pinned.clone()),
            (None, Some(reference)) => Ok(reference.clone()),
            (None, None) => Err(Error::internal("no image to deploy")),
        })
        .collect::<Result<_>>()?;
    let releases = apply_releases(&session, &retry, p, &release_request, &owners, &images).await?;

    // 5. Services, then jobs.
    let record = grant_record(&recorded, &desired_grants, &provisioner.granted(), true);
    let mut service_results = Vec::new();
    let mut job_results = Vec::new();
    let mut live_services: Vec<(&Deployment, Service)> = Vec::new();
    for (i, d) in pinned.iter().enumerate() {
        let k = by_workload[i];
        let image = &images[k];
        let mut annotations = decisions[k].annotations.clone();
        if is_holder(d, holder.d) && (!record.is_empty() || !recorded.is_empty()) {
            annotations.insert(ANNOTATION_GRANTS.to_string(), encode_grants(&record));
        }
        if let Some(rel) = releases.iter().find(|(o, _)| *o == k).map(|(_, r)| r) {
            annotations.insert(naming::ANNOTATION_RELEASE.to_string(), rel.tag.clone());
        }
        match &d.kind {
            WorkloadKind::Service => {
                let existing = match &lives[i] {
                    Live::Service(s) => s.clone(),
                    Live::Job(_) => None,
                };
                let view = provisioner.for_service(d);
                let (result, svc, done) = deploy_service(
                    &reconciler,
                    &view,
                    d,
                    image,
                    annotations,
                    existing,
                    &mode,
                    &retry,
                    args.adopt,
                    p,
                )
                .await?;
                steps.extend(done);
                live_services.push((d, svc));
                service_results.push(result);
            }
            WorkloadKind::Job(settings) => {
                if let Mode::Canary { .. } = mode {
                    p.info(format!(
                        "job {}: unchanged by a canary (a full deploy updates it)",
                        d.name()
                    ));
                    continue;
                }
                let target = job_for_mode(d, &mode)?;
                let mut spec = ServiceSpec::from_deployment(&target, image, annotations);
                spec.labels = job_labels(d, &mode);
                let known = match &lives[i] {
                    Live::Job(j) => j.clone(),
                    Live::Service(_) => None,
                };
                p.step(format!(
                    "{} Cloud Run job {}",
                    if known.is_some() {
                        "Updating"
                    } else {
                        "Creating"
                    },
                    target.service_id
                ));
                if let Some(j) = &known {
                    for c in crate::gcp::jobs::changes(Some(j), &spec, settings) {
                        match (&c.before, &c.after) {
                            (Some(b), Some(a)) => p.info(format!("~ {}: {b} -> {a}", c.field)),
                            (None, Some(a)) => p.info(format!("+ {}: {a}", c.field)),
                            (Some(b), None) => p.info(format!("- {}: {b}", c.field)),
                            (None, None) => {}
                        }
                    }
                }
                let name = target.service_name();
                let change = with_retry(&retry, p, "deploy job", |attempt| {
                    let (jrec, spec, target, known, name) = (&jrec, &spec, &target, &known, &name);
                    async move {
                        // A retry re-reads the job: its etag changed if the
                        // failed attempt applied.
                        let existing = match attempt {
                            1 => known.clone(),
                            _ => jrec.get(name).await?,
                        };
                        jrec.apply(
                            &target.parent(),
                            &target.service_id,
                            spec,
                            settings,
                            existing,
                            &d.app,
                            &d.stage,
                        )
                        .await
                    }
                })
                .await?;
                job_results.push(JobResult {
                    name: d.name().to_string(),
                    job: target.service_id.clone(),
                    image: image.clone(),
                    change,
                });
            }
        }
    }

    // 6. Schedules (full deploys, for the targets deployed), then removals:
    //    only after a full deploy of everything, once every service serves
    //    one revision (a preview, canary or partial rollout may need access).
    let mut post: Vec<Step> = Vec::new();
    if full {
        post.extend(schedule_steps(r).into_iter().filter(|s| {
            match s {
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
            }
        }));
    }
    let revoke_now = full
        && everything
        && live_services
            .iter()
            .all(|(_, s)| run::one_revision_serves_all(s));
    let revokes = stack_revoke_steps(&recorded, r);
    if !revokes.is_empty() && !revoke_now {
        p.info(format!(
            "{} grant(s) removed from runway.yaml are kept until a full deploy of every service and job serves all traffic",
            revokes.len()
        ));
    }
    if revoke_now {
        let shared = provisioner.unlisted_shared(&recorded).await;
        let mut removals = merge_removals(revokes.clone(), shared.steps);
        let mut unchecked = shared.unchecked;
        for (d, _) in &live_services {
            let own = provisioner.for_service(d).unlisted_service().await;
            unchecked.extend(own.unchecked);
            removals = merge_removals(removals, own.steps);
        }
        for what in &unchecked {
            p.warn(format!("could not check for {what}; nothing removed there"));
        }
        match provisioner.orphan_schedules().await {
            Ok(left) => removals.extend(left),
            Err(e) => p.warn(format!(
                "could not check for schedules runway created ({}); none deleted",
                e.message
            )),
        }
        post.extend(removals);
    }
    let any_url = service_results.iter().find_map(|s| s.url.clone());
    if !post.is_empty() {
        match provisioner.apply_all(&post, &retry, p, &report).await {
            Ok(done) => steps.extend(done),
            Err(e) => {
                let record = grant_record(&recorded, &desired_grants, &provisioner.granted(), true);
                let _ = save_holder(&reconciler, &jrec, &holder, &record, true, p).await;
                return Err(after_rollout(e, first, any_url.as_deref()));
            }
        }
    }
    // 7. The final record: grants added after the rollout, revocations done.
    let final_record = grant_record(
        &recorded,
        &desired_grants,
        &provisioner.granted(),
        !revoke_now,
    );
    if final_record != record {
        with_retry(&retry, p, "record grants", |_| {
            save_holder(&reconciler, &jrec, &holder, &final_record, true, p)
        })
        .await
        .map_err(|e| after_rollout(e, first, any_url.as_deref()))?;
    }
    if everything && full {
        for o in crate::commands::undeploy::orphan_workloads(r, &run_client, &jobs_client).await? {
            p.warn(format!(
                "{} {} is no longer in runway.yaml; remove it with `runway undeploy --stage {} --orphans`",
                if o.is_job() { "job" } else { "service" },
                o.service_id,
                first.stage
            ));
        }
    }

    let result = StackResult {
        app: first.app.clone(),
        stage: first.stage.clone(),
        project: first.project.clone(),
        region: first.region.clone(),
        services: service_results,
        jobs: job_results,
        steps,
        builds: built.into_iter().flatten().collect(),
        releases: releases.into_iter().map(|(_, r)| r).collect(),
        traffic_mode: mode,
        duration_seconds: started.elapsed().as_secs(),
    };
    match ctx.output {
        OutputFormat::Json => print_json(&result),
        OutputFormat::Text => print_text(p, &result),
    }
    if result.services.iter().any(|s| s.url.is_none()) {
        return Err(Error::new(
            ErrorKind::Deploy,
            "a service is ready but reported no URL",
        ));
    }
    Ok(())
}

/// One service: bootstrap on first deploy, tags, rollout, its tags and IAP,
/// then public access. Returns its result, the live service and the steps.
#[allow(clippy::too_many_arguments)]
async fn deploy_service(
    reconciler: &Reconciler<'_>,
    view: &Provisioner<'_>,
    d: &Deployment,
    image: &str,
    annotations: std::collections::BTreeMap<String, String>,
    existing: Option<Service>,
    mode: &Mode,
    retry: &RetryConfig,
    adopt: bool,
    p: &Progress,
) -> Result<(ServiceResult, Service, Vec<StepResult>)> {
    let name = d.service_name();
    let report = |x: &StepResult| report_step(p, x);
    let mut steps = Vec::new();
    let base_spec =
        ServiceSpec::from_deployment(d, image, annotations).with_traffic_mode(mode.clone());
    let mut existing = existing;
    let bootstrapped = if existing.is_none()
        && let Some(bs) = &d.service.bootstrap
    {
        let placeholder = bootstrap_spec(d, bs, &base_spec);
        p.step(format!(
            "First deploy: creating {} with ingress {} until its tags are effective",
            d.service_id, bs.ingress
        ));
        with_retry(retry, p, "create the service", |attempt| {
            roll_out(
                reconciler,
                d,
                &name,
                &placeholder,
                if attempt == 1 { Some(None) } else { None },
                false,
                p,
            )
        })
        .await?;
        existing = reconciler.get(&name).await?;
        true
    } else {
        false
    };
    let post = crate::provision::post_steps(d);
    let tag_steps: Vec<Step> = post
        .iter()
        .filter(|s| matches!(s, Step::Tag { .. }))
        .cloned()
        .collect();
    let mut done_tags = Vec::new();
    if existing.is_some() && !tag_steps.is_empty() {
        if !bootstrapped {
            p.step(format!(
                "Binding the tags of {} before the rollout",
                d.service_id
            ));
        }
        for res in view.apply_all(&tag_steps, retry, p, &report).await? {
            done_tags.push(res.step.clone());
            steps.push(res);
        }
    }
    p.step(match (&existing, bootstrapped) {
        (_, true) => format!("Applying the configuration to {}", d.service_id),
        (None, _) => format!("Creating Cloud Run service {}", d.service_id),
        (Some(_), _) => format!("Updating Cloud Run service {}", d.service_id),
    });
    let tag_just_bound = steps.iter().any(|x| x.outcome == StepOutcome::Changed);
    let (applied, svc) = with_retry(retry, p, "deploy service", |attempt| {
        let known = if attempt == 1 { Some(existing.clone()) } else { None };
        let (name, base_spec) = (&name, &base_spec);
        async move {
            roll_out(reconciler, d, name, base_spec, known, adopt, p)
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
    let remaining: Vec<Step> = post
        .into_iter()
        .filter(|s| !done_tags.contains(&s.describe(d)))
        .collect();
    steps.extend(
        view.apply_all(&remaining, retry, p, &report)
            .await
            .map_err(|e| after_rollout(e, d, url.as_deref()))?,
    );
    let access_change = with_retry(retry, p, "invoker access", |_| {
        reconciler.ensure_access(&name, d.service.public)
    })
    .await
    .map_err(|e| after_rollout(e, d, url.as_deref()))?;
    let result = ServiceResult {
        name: d.name().to_string(),
        service: d.service_id.clone(),
        url,
        revision: run::mode_target(&run::current_traffic(&svc), mode)
            .map(|(_, r)| r)
            .unwrap_or_else(|| run::short_revision(&svc.latest_ready_revision).to_string()),
        image: image.to_string(),
        change: applied.change,
        public: d.service.public,
        access_change,
        iap: d.service.iap.enabled,
        revision_url: match mode {
            Mode::Full => None,
            Mode::Preview { tag } => tagged_url(&svc, tag),
            Mode::Canary { .. } => tagged_url(&svc, crate::traffic::CANARY_TAG),
        },
        traffic: run::status(&svc, &d.app, &d.stage).traffic,
    };
    Ok((result, svc, steps))
}

/// `--tag`/`--tag-rc`: the version from the changelog (next to runway.yaml
/// or in the first build context), checked before anything changes.
fn release_request(
    ctx: &Context,
    args: &DeployArgs,
    selected: &[&Deployment],
) -> Result<
    Option<(
        crate::build::release::ReleaseKind,
        String,
        std::path::PathBuf,
    )>,
> {
    if !(args.tag || args.tag_rc) {
        return Ok(None);
    }
    let Some(b) = selected.iter().find_map(|d| match &d.artifact {
        Artifact::Build(b) => Some(b),
        Artifact::Image { .. } => None,
    }) else {
        return Err(Error::config(
            "--tag/--tag-rc tag images built by runway; this selection deploys existing images only",
        ));
    };
    let config_dir = ctx
        .config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let path =
        crate::build::release::find_changelog(&[config_dir, &b.context_dir]).ok_or_else(|| {
            Error::config(format!(
                "--tag needs a changelog ({}) next to {} or in {}",
                crate::build::release::CHANGELOG_NAMES.join(", "),
                ctx.config.display(),
                b.context_dir.display()
            ))
        })?;
    let text = std::fs::read_to_string(&path)?;
    let version = crate::build::release::latest_version(&text).ok_or_else(|| {
        Error::config(format!(
            "no `vX.Y.Z` or `X.Y.Z` version heading found in {}",
            path.display()
        ))
    })?;
    let kind = match args.tag_rc {
        true => crate::build::release::ReleaseKind::Candidate,
        false => crate::build::release::ReleaseKind::Release,
    };
    ctx.progress
        .info(format!("release version {version} from {}", path.display()));
    Ok(Some((kind, version, path)))
}

/// Tags each image runway builds (one per distinct build) with the release.
async fn apply_releases(
    session: &crate::gcp::Session,
    retry: &RetryConfig,
    p: &Progress,
    request: &Option<(
        crate::build::release::ReleaseKind,
        String,
        std::path::PathBuf,
    )>,
    owners: &[&Deployment],
    images: &[String],
) -> Result<Vec<(usize, crate::build::release::ReleaseTag)>> {
    let Some((kind, version, path)) = request else {
        return Ok(Vec::new());
    };
    let ar = build_client!(
        google_cloud_artifactregistry_v1::client::ArtifactRegistry,
        session
    )?;
    let mut out = Vec::new();
    for (i, owner) in owners.iter().enumerate() {
        let Artifact::Build(b) = &owner.artifact else {
            continue;
        };
        let digest = images[i]
            .rsplit_once('@')
            .map(|(_, dg)| dg.to_string())
            .ok_or_else(|| Error::internal("the built image has no digest"))?;
        let package = owner.image_package();
        let pkg = crate::build::release::Package {
            project: &owner.project,
            location: &b.artifact_location,
            repository: &b.artifact_repository,
            package: &package,
        };
        let r = with_retry(retry, p, "tag the release", |_| {
            crate::build::release::apply(&ar, &pkg, &digest, *kind, version, path)
        })
        .await?;
        if r.created {
            p.success(format!("tagged {}", r.image));
        } else {
            p.info(format!("= {} already tags this image", r.image));
        }
        out.push((i, r));
    }
    Ok(out)
}

fn print_text(p: &Progress, r: &StackResult) {
    p.success(format!(
        "{} (stage {}) deployed in {}s",
        r.app, r.stage, r.duration_seconds
    ));
    let c = crate::style::out();
    let verb = |ch: ServiceChange| match ch {
        ServiceChange::Created => "created",
        ServiceChange::Updated => "updated",
        ServiceChange::Unchanged => "unchanged",
    };
    if !r.services.is_empty() {
        println!("Services:");
        for s in &r.services {
            println!(
                "  {} {} {} {}",
                c.bold(&s.service),
                c.bold_cyan(s.url.as_deref().unwrap_or("(no URL)")),
                s.revision,
                c.dim(&format!("({})", verb(s.change)))
            );
            if let Some(u) = &s.revision_url {
                println!(
                    "    {}: {}",
                    match r.traffic_mode {
                        Mode::Preview { .. } => "preview",
                        _ => "canary",
                    },
                    c.bold_cyan(u)
                );
            }
        }
    }
    if !r.jobs.is_empty() {
        println!("Jobs:");
        for j in &r.jobs {
            let v = match j.change {
                JobChange::Created => "created",
                JobChange::Updated => "updated",
                JobChange::Unchanged => "unchanged",
            };
            println!("  {} {}", c.bold(&j.job), c.dim(&format!("({v})")));
        }
    }
    for rel in &r.releases {
        println!("Release:  {}", c.bold_green(&rel.tag));
    }
    let changed = r
        .steps
        .iter()
        .filter(|s| s.outcome == StepOutcome::Changed)
        .count();
    if !r.steps.is_empty() {
        println!("Steps:    {} step(s), {changed} changed", r.steps.len());
    }
}
