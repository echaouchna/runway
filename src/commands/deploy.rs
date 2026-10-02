//! `runway deploy`: provision identity and grants, build if necessary, deploy,
//! wait for readiness, bind tags, configure IAP and access.
//!
//! Every step is idempotent (it re-reads the live state first) and is retried
//! according to the `retry` configuration / `--retries`.

use crate::build::cloudbuild::{BuildInputs, Builder, BuiltImage};
use crate::build_client;
use crate::cli::{Context, DeployArgs};
use crate::commands::plan::decide_image;
use crate::commands::{load, registry_client};
use crate::config::{Artifact, Deployment, Overrides};
use crate::deploy::{AccessChange, Applied, Reconciler, ServiceChange, Target, check_ownership};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::run::{self, Readiness};
use crate::naming;
use crate::output::{OutputFormat, Progress, print_json};
use crate::plan::{ImagePlan, ServiceSpec, diff};
use crate::poll::PollConfig;
use crate::provision::{Provisioner, StepOutcome, StepResult, api_step, post_steps, pre_steps};
use crate::retry::{RetryConfig, with_retry};
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

fn report_step(p: &Progress, r: &StepResult) {
    match r.outcome {
        StepOutcome::Changed => p.success(format!("{}: {}", r.step, r.detail)),
        StepOutcome::Unchanged => p.info(format!("= {} (already done)", r.step)),
    }
}

/// Wraps a failure that happens after the service is live.
fn after_rollout(e: Error, d: &Deployment, url: Option<&str>) -> Error {
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
    let overrides = Overrides {
        image: args.image.clone(),
    };
    let resolved = load(ctx, &args.stage.stage, &overrides)?;
    let d = &resolved.deployment;
    let p = &ctx.progress;
    let name = d.service_name();
    let retry = effective_retry(d, &args);

    // Release tag: read the changelog before doing anything else.
    let release_request = if args.tag || args.tag_rc {
        let Artifact::Build(b) = &d.artifact else {
            return Err(Error::config(
                "--tag/--tag-rc tag images built by runway; this stage deploys an existing image",
            ));
        };
        let config_dir = ctx
            .config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let path = crate::build::release::find_changelog(&[&b.context_dir, config_dir])
            .ok_or_else(|| {
                Error::config(format!(
                    "--tag needs a changelog ({}) in {} or next to {}",
                    crate::build::release::CHANGELOG_NAMES.join(", "),
                    b.context_dir.display(),
                    ctx.config.display()
                ))
            })?;
        let text = std::fs::read_to_string(&path)?;
        let version = crate::build::release::latest_version(&text).ok_or_else(|| {
            Error::config(format!(
                "no `vX.Y.Z` or `X.Y.Z` version heading found in {}",
                path.display()
            ))
        })?;
        let kind = if args.tag_rc {
            crate::build::release::ReleaseKind::Candidate
        } else {
            crate::build::release::ReleaseKind::Release
        };
        p.info(format!("release version {version} from {}", path.display()));
        Some((kind, version, path))
    } else {
        None
    };

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
    let run_client = build_client!(Services, session)?;
    let revisions = build_client!(Revisions, session)?;
    let reconciler = Reconciler {
        run: &run_client,
        revisions: Some(&revisions),
        progress: p,
        poll: PollConfig::default(),
        timeout: args.timeout,
    };

    // 1. APIs first: nothing else can be read or created without them.
    let provisioner = Provisioner::new(d, &session, &run_client).await?;
    let mut steps = Vec::new();
    if let Some(step) = api_step(d) {
        p.step("Enabling required APIs");
        let r = with_retry(&retry, p, &step.describe(d), |_| provisioner.apply(&step)).await?;
        report_step(p, &r);
        steps.push(r);
    }

    // 2. Inspect: read the live service while hashing the source and resolving
    //    the image (all read-only). Ownership is checked before any change.
    let registry = with_retry(&retry, p, "authenticate", |_| registry_client(&session)).await?;
    let mut version_notes = Vec::new();
    let resolved_versions =
        crate::commands::resolve_runtime_versions(d, &registry, &mut version_notes).await;
    let d = &resolved_versions;
    for n in version_notes {
        p.info(n);
    }
    let (existing, decision) = with_retry(&retry, p, "inspect current state", |_| async {
        let mut notes = Vec::new();
        let (existing, decision) = tokio::join!(
            reconciler.get(&name),
            decide_image(d, Some(&registry), &mut notes)
        );
        Ok((existing?, decision?))
    })
    .await?;
    if let Some(svc) = &existing {
        check_ownership(svc, &d.app, &d.stage, args.adopt)?;
    }

    // 3. Buckets, repository, service accounts, then grants (dependency order).
    let pre = pre_steps(d);
    if !pre.is_empty() {
        p.step("Provisioning buckets, identities and grants");
    }
    for step in &pre {
        let r = with_retry(&retry, p, &step.describe(d), |_| provisioner.apply(step)).await?;
        report_step(p, &r);
        steps.push(r);
    }
    // Every secret exists and has a value: pin the newest versions.
    let (pinned_secrets, secret_notes) =
        with_retry(&retry, p, "resolve secret versions", |_| async {
            let mut notes = Vec::new();
            let pinned =
                crate::commands::resolve_secret_versions(d, &provisioner, &mut notes, true).await?;
            Ok((pinned, notes))
        })
        .await?;
    for n in secret_notes {
        p.warn(n);
    }
    let d = &pinned_secrets;

    // 4. Image.
    let mut built: Option<BuiltImage> = None;
    let image = match (&d.artifact, &decision.image) {
        (Artifact::Image { .. }, ImagePlan::Unresolved { reference, reason }) => {
            if reason.contains("no such tag") {
                return Err(Error::prerequisite(format!(
                    "image {reference} was not found in its registry"
                ))
                .hint("push the image first, or fix `service.image`"));
            }
            p.warn(format!(
                "deploying tag {reference} without a pinned digest ({reason}); Cloud Run resolves it when the revision is created"
            ));
            reference.clone()
        }
        (Artifact::Build(cfg), img)
            if args.force_build || cfg.rebuild_always || !img.is_exact() =>
        {
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
            let builder = Builder {
                cloudbuild: &cloudbuild,
                uploader: &storage,
                resolver: &registry,
                logging: Some(&logging),
                progress: p,
                poll: PollConfig::default(),
            };
            let inputs = BuildInputs {
                project: &d.project,
                region: &d.region,
                app: &d.app,
                stage: &d.stage,
                config: cfg,
                source,
                image_checked: decision.build.as_ref().and_then(|b| b.will_build) == Some(true),
                timeout: args.build_timeout,
                force: args.force_build || cfg.rebuild_always,
            };
            // A retry re-checks for the image / an in-flight build first, so it
            // never builds the same source twice.
            let out = with_retry(&retry, p, "build image", |_| builder.build(&inputs)).await?;
            let pinned = out.pinned.clone();
            built = Some(out);
            pinned
        }
        (_, ImagePlan::Pinned { reference, .. }) => {
            if decision.build.is_some() {
                p.info(format!("image for this source already exists: {reference}"));
            } else {
                p.info(format!("image: {reference}"));
            }
            reference.clone()
        }
        _ => return Err(Error::internal("inconsistent image plan")),
    };

    // 5. Service.
    // Release tag on the image, once its digest is known.
    let mut annotations = decision.annotations.clone();
    let mut release = None;
    if let (Some((kind, version, path)), Artifact::Build(b)) = (&release_request, &d.artifact) {
        let digest = image
            .rsplit_once('@')
            .map(|(_, dg)| dg.to_string())
            .ok_or_else(|| Error::internal("the built image has no digest"))?;
        let ar = build_client!(
            google_cloud_artifactregistry_v1::client::ArtifactRegistry,
            session
        )?;
        let pkg = crate::build::release::Package {
            project: &d.project,
            location: &b.artifact_location,
            repository: &b.artifact_repository,
            package: &d.app,
        };
        let r = with_retry(&retry, p, "tag the release", |_| {
            crate::build::release::apply(&ar, &pkg, &digest, *kind, version, path)
        })
        .await?;
        if r.created {
            p.success(format!("tagged {}", r.image));
        } else {
            p.info(format!("= {} already tags this image", r.image));
        }
        annotations.insert(naming::ANNOTATION_RELEASE.to_string(), r.tag.clone());
        release = Some(r);
    }
    let base_spec =
        ServiceSpec::from_deployment(d, &image, annotations).with_traffic_mode(mode.clone());

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
            roll_out(
                &reconciler,
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
        for step in &tag_steps {
            let r = with_retry(&retry, p, &step.describe(d), |_| provisioner.apply(step)).await?;
            report_step(p, &r);
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
    let (applied, svc) = with_retry(&retry, p, "deploy service", |attempt| {
        let known = if attempt == 1 { Some(existing.clone()) } else { None };
        async move {
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
    for step in post_steps(d)
        .iter()
        .filter(|s| !done_tags.contains(&s.describe(d)))
    {
        let r = with_retry(&retry, p, &step.describe(d), |_| provisioner.apply(step))
            .await
            .map_err(|e| after_rollout(e, d, url.as_deref()))?;
        report_step(p, &r);
        steps.push(r);
    }

    // 7. Public access.
    let access_change = with_retry(&retry, p, "invoker access", |_| {
        reconciler.ensure_access(&name, d.service.public)
    })
    .await
    .map_err(|e| after_rollout(e, d, url.as_deref()))?;

    let result = DeployResult {
        app: d.app.clone(),
        stage: d.stage.clone(),
        project: d.project.clone(),
        region: d.region.clone(),
        service: d.service_id.clone(),
        url: url.clone(),
        revision: run::short_revision(&svc.latest_ready_revision).to_string(),
        image,
        change: applied.change,
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

/// One idempotent roll-out attempt: re-reads the service (except on the first
/// attempt), checks ownership, applies changes and waits for readiness.
async fn roll_out(
    reconciler: &Reconciler<'_>,
    d: &Deployment,
    name: &str,
    base_spec: &ServiceSpec,
    known: Option<Option<Service>>,
    adopt: bool,
    p: &Progress,
) -> Result<(Applied, Service)> {
    let existing = match known {
        Some(e) => e,
        None => reconciler.get(name).await?,
    };
    if let Some(svc) = &existing {
        check_ownership(svc, &d.app, &d.stage, adopt)?;
    }
    let mut spec = run::spec_for_live(base_spec, existing.as_ref());
    // An owned service whose spec already matches but whose latest revision
    // failed gets a fresh revision (for example after a grant propagated).
    let mut force = false;
    // A preview or canary always runs in a revision of its own (marked by a
    // template annotation), even when its code and configuration equal what
    // already serves: otherwise its URL would point at the production revision.
    if let Some((key, value)) = own_revision_marker(&spec.traffic.mode) {
        let live = existing
            .as_ref()
            .and_then(|s| s.template.as_ref())
            .and_then(|t| t.annotations.get(key));
        if existing.is_some() && live.map(String::as_str) != Some(value.as_str()) {
            force = true;
        }
        spec.revision_annotations.insert(key.to_string(), value);
    }
    if !force
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
    if let Some(svc) = &existing {
        for c in diff(&run::observed_flat(svc), &spec.flatten()) {
            match (&c.before, &c.after) {
                (Some(b), Some(a)) => p.info(format!("~ {}: {b} -> {a}", c.field)),
                (None, Some(a)) => p.info(format!("+ {}: {a}", c.field)),
                (Some(b), None) => p.info(format!("- {}: {b}", c.field)),
                (None, None) => {}
            }
        }
    }
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
    Ok((applied, svc))
}

/// Template annotation giving previews and canaries their own revision.
fn own_revision_marker(mode: &crate::traffic::Mode) -> Option<(&'static str, String)> {
    match mode {
        crate::traffic::Mode::Full => None,
        crate::traffic::Mode::Preview { tag } => Some(("runway.dev/preview", tag.clone())),
        crate::traffic::Mode::Canary { .. } => Some(("runway.dev/canary", "true".into())),
    }
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
        annotations: [("runway.dev/bootstrap".to_string(), image.clone())].into(),
        revision_annotations: Default::default(),
        traffic: Default::default(),
    }
}

/// URL of a tagged revision, once Cloud Run reports it.
fn tagged_url(svc: &google_cloud_run_v2::model::Service, tag: &str) -> Option<String> {
    svc.traffic_statuses
        .iter()
        .find(|t| t.tag == tag && !t.uri.is_empty())
        .map(|t| t.uri.clone())
}

fn print_text(p: &Progress, r: &DeployResult) {
    let verb = match r.change {
        ServiceChange::Created => "created",
        ServiceChange::Updated => "updated",
        ServiceChange::Unchanged => "unchanged",
    };
    p.success(format!("{} {verb} in {}s", r.service, r.duration_seconds));
    let c = crate::style::out();
    println!("Service:  {}", c.bold(&r.service));
    println!(
        "URL:      {}",
        c.bold_cyan(r.url.as_deref().unwrap_or("(none)"))
    );
    println!("Revision: {}", r.revision);
    match (&r.traffic_mode, &r.revision_url) {
        (crate::traffic::Mode::Preview { tag }, url) => println!(
            "Preview:  {} (tag {tag}, no traffic)",
            c.bold_cyan(url.as_deref().unwrap_or("(URL not reported yet)"))
        ),
        (crate::traffic::Mode::Canary { percent }, url) => println!(
            "Canary:   {} ({percent}% of the traffic; `runway traffic --promote` to finish)",
            c.bold_cyan(url.as_deref().unwrap_or("(URL not reported yet)"))
        ),
        _ => {}
    }
    if r.traffic.len() > 1 {
        crate::commands::info::print_traffic(&r.traffic);
    }
    println!("Image:    {}", r.image);
    if let Some(rel) = &r.release {
        println!("Release:  {}", c.bold_green(&rel.tag));
    }
    let access = match (r.iap, r.public) {
        (true, _) => "Identity-Aware Proxy (signed-in members only)",
        (false, true) => "public (allUsers can invoke)",
        (false, false) => "private (requires an identity token with roles/run.invoker)",
    };
    println!("Access:   {access}");
    if !r.steps.is_empty() {
        let changed = r
            .steps
            .iter()
            .filter(|s| s.outcome == StepOutcome::Changed)
            .count();
        println!(
            "Steps:    {} provisioning step(s), {} changed, {} already done",
            r.steps.len(),
            c.green(&changed.to_string()),
            r.steps.len() - changed
        );
    }
    if !r.public
        && !r.iap
        && let Some(u) = &r.url
    {
        println!();
        println!(
            "Try it:   curl -H \"Authorization: Bearer $(gcloud auth print-identity-token)\" {u}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            .deployment;
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
            .deployment;
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
