//! Command implementations.

pub mod completions;
pub mod deploy;
pub mod deploy_stack;
pub mod describe;
pub mod doctor;
pub mod info;
pub mod init;
pub mod logs;
pub mod plan;
pub mod preview;
pub mod release;
pub mod run_job;
pub mod summary;
pub mod traffic;
pub mod undeploy;
pub mod unlock;
pub mod validate;

use crate::cli::Context;
use crate::config::{self, Issue, Overrides, Resolved};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::Session;
use crate::gcp::registry::{DigestResolver, RegistryClient, ResolveError};
use crate::image_ref::ImageRef;
use crate::output::Progress;
use crate::plan::ImagePlan;

pub(crate) fn print_warnings(progress: &Progress, warnings: &[Issue]) {
    for w in warnings {
        progress.warn(w.to_string());
    }
}

/// Loads and resolves the configuration for a stage, printing warnings.
/// The one service (or job, with `jobs`) a command is about: the only one,
/// or the one `--only` selects.
pub(crate) fn one<'a>(
    r: &'a Resolved,
    only: &[String],
    jobs: bool,
) -> Result<&'a crate::config::Deployment> {
    let picked: Vec<_> = r
        .select(only)?
        .into_iter()
        .filter(|d| jobs || !d.is_job())
        .collect();
    match picked.as_slice() {
        [d] => Ok(d),
        [] => Err(crate::error::Error::config(
            "no service selected (jobs have no URL or traffic)",
        )),
        many => Err(crate::error::Error::config(format!(
            "this stage has several {}: choose one with --only ({})",
            if jobs {
                "services and jobs"
            } else {
                "services"
            },
            many.iter().map(|d| d.name()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

pub(crate) fn load(ctx: &Context, stage: &str, overrides: &Overrides) -> Result<Resolved> {
    let (_, resolved) = config::load_and_resolve(&ctx.config, stage, overrides)?;
    print_warnings(&ctx.progress, &resolved.warnings);
    Ok(resolved)
}

/// Registry client authenticated with ADC for Google registries.
pub(crate) async fn registry_client(session: &Session) -> Result<RegistryClient> {
    let token = session.token().await?;
    Ok(RegistryClient::new(session.http.clone(), Some(token)))
}

/// Resolves an existing image reference to a digest for planning or deployment.
pub(crate) async fn resolve_existing_image(
    resolver: &dyn DigestResolver,
    image: &ImageRef,
    original: &str,
) -> ImagePlan {
    if let Some(d) = &image.digest {
        return ImagePlan::Pinned {
            reference: image.pinned(d),
            digest: d.clone(),
            origin: "digest given in configuration".into(),
        };
    }
    match resolver.resolve(image).await {
        Ok(Some(d)) => ImagePlan::Pinned {
            reference: image.pinned(&d),
            digest: d,
            origin: format!("resolved from {original}"),
        },
        Ok(None) => ImagePlan::Unresolved {
            reference: original.to_string(),
            reason: "the registry reports no such tag".into(),
        },
        Err(ResolveError::Unauthorized(m)) => ImagePlan::Unresolved {
            reference: original.to_string(),
            reason: format!("cannot read the registry ({m})"),
        },
        Err(ResolveError::Other(m)) => ImagePlan::Unresolved {
            reference: original.to_string(),
            reason: m,
        },
    }
}

/// Opens a Google Cloud session for a deployment: ADC, impersonating the
/// service account from the CLI flag/env or `provider.impersonate_service_account`.
pub(crate) async fn connect(ctx: &Context, d: &crate::config::Deployment) -> Result<Session> {
    let spec = ctx.impersonate.clone().or_else(|| d.impersonate.clone());
    let imp = match spec {
        Some(s) => Some(
            crate::gcp::Impersonation::parse(&s)
                .map_err(|e| Error::config(format!("--impersonate-service-account: {e}")))?,
        ),
        None => None,
    };
    if let Some(i) = &imp {
        ctx.progress.info(format!("impersonating {}", i.target));
    }
    let session = Session::connect(imp.as_ref())?;
    if imp.is_some() {
        session.verify().await?;
    }
    Ok(session)
}

/// The session for the registry work runway does itself (resolving,
/// copying and tagging images): `provider.push_service_account`,
/// impersonated from the caller's credentials, else `session` itself.
pub(crate) async fn push_session(
    ctx: &Context,
    d: &crate::config::Deployment,
    session: &Session,
) -> Result<Session> {
    let Some(sa) = &d.push_service_account else {
        return Ok(session.clone());
    };
    if session.impersonating.as_ref().map(|i| &i.target) == Some(sa) {
        return Ok(session.clone());
    }
    let imp = crate::gcp::Impersonation {
        target: sa.clone(),
        delegates: Vec::new(),
    };
    ctx.progress.info(format!("registry operations as {sa}"));
    let push = Session::connect(Some(&imp))?;
    push.verify().await.map_err(|e| {
        e.hint("or remove `provider.push_service_account` to use your own credentials")
    })?;
    Ok(push)
}

/// Pins secrets referenced without a version to their newest enabled
/// version, so that a new value rolls out (new revision) with the next deploy.
/// `strict`: a secret without any enabled version is an error (deploy);
/// otherwise a note (plan).
pub(crate) async fn resolve_secret_versions(
    d: &crate::config::Deployment,
    prov: &crate::provision::Provisioner<'_>,
    notes: &mut Vec<String>,
    strict: bool,
) -> Result<crate::config::Deployment, Error> {
    let mut out = d.clone();
    let svc = &mut out.service;
    let refs: Vec<_> = svc
        .secrets
        .iter_mut()
        .chain(
            svc.sidecars
                .values_mut()
                .flat_map(|sc| sc.secrets.iter_mut()),
        )
        .filter(|(_, s)| s.pin_latest && s.version == "latest")
        .collect();
    // One lookup per secret, all at once; results are applied in order.
    let names: Vec<String> = refs.iter().map(|(_, s)| s.full_name(&d.project)).collect();
    let versions =
        futures::future::join_all(names.iter().map(|n| prov.newest_enabled_version(n))).await;
    for (((key, s), full), version) in refs.into_iter().zip(names).zip(versions) {
        match version {
            Ok(Some(v)) => s.version = v,
            Ok(None) if strict => {
                return Err(Error::prerequisite(format!(
                    "secret {} (used by `{key}`) has no enabled version",
                    s.secret
                ))
                .hint(format!(
                    "printf '%s' 'VALUE' | gcloud secrets versions add {} --project {} --data-file=-",
                    full.rsplit('/').next().unwrap_or(&full),
                    full.split('/').nth(1).unwrap_or(&d.project)
                ))
                .permanent());
            }
            Ok(None) => notes.push(format!(
                "secret {} (used by `{key}`) has no enabled version yet; deploy stops until one is added",
                s.secret
            )),
            Err(e) => {
                notes.push(format!(
                    "`{key}`: cannot read the versions of {} ({}); `latest` is used and resolved when instances start",
                    s.secret, e.message
                ));
                s.pin_latest = false;
            }
        }
    }
    Ok(out)
}

/// Traffic mode from `--preview` / `--traffic`.
/// The service ID preview tags are sized for: the longest of the stage, so
/// that one tag fits every service (and does not depend on `--only`).
pub(crate) fn preview_basis(r: &Resolved) -> String {
    r.services()
        .map(|d| d.service_id.clone())
        .max_by_key(|id| id.len())
        .unwrap_or_else(|| crate::naming::service_id(&r.first().app, &r.first().stage))
}

/// Preview copy of a job: `{job_id}-{tag}`, within Cloud Run's limit.
pub(crate) fn preview_job_id(job_id: &str, tag: &str) -> Result<String> {
    let id = format!("{job_id}-{tag}");
    if id.len() > crate::naming::MAX_JOB_NAME_LEN {
        return Err(crate::error::Error::config(format!(
            "--preview: the preview job `{id}` is longer than {} characters; use a shorter preview name",
            crate::naming::MAX_JOB_NAME_LEN
        )));
    }
    Ok(id)
}

pub(crate) fn traffic_mode(
    preview: Option<&str>,
    percent: Option<u32>,
    service_id: &str,
) -> Result<crate::traffic::Mode, Error> {
    Ok(match (preview, percent) {
        (Some(name), _) => crate::traffic::Mode::Preview {
            tag: crate::traffic::preview_tag(name, service_id)
                .map_err(|e| Error::config(format!("--preview: {e}")))?,
        },
        (None, Some(p)) => crate::traffic::Mode::Canary { percent: p },
        (None, None) => crate::traffic::Mode::Full,
    })
}

/// Versions decided at runtime: an unpinned `otel_collector` uses the newest
/// released collector. Falls back to the built-in version on registry errors.
pub(crate) async fn resolve_runtime_versions(
    d: &crate::config::Deployment,
    resolver: &dyn DigestResolver,
    notes: &mut Vec<String>,
) -> crate::config::Deployment {
    let mut d = d.clone();
    if let Some(o) = d.service.otel_collector.as_mut()
        && !o.pinned
    {
        let base = ImageRef::parse(&o.image).expect("validated");
        match resolver.tags(&base).await {
            Ok(tags) => match crate::gcp::registry::latest_semver(&tags) {
                Some(v) => {
                    o.image = format!("{}:{v}", base.name());
                    notes.push(format!("otel_collector: latest released version is {v}"));
                }
                None => notes.push(format!(
                    "otel_collector: no released version found; using {}",
                    o.image
                )),
            },
            Err(e) => notes.push(format!(
                "otel_collector: cannot list collector versions ({e}); using {}",
                o.image
            )),
        }
    }
    d
}

pub(crate) fn not_deployed(service: &str, stage: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("service {service} does not exist yet"),
    )
    .hint(format!("deploy it with `runway deploy --stage {stage}`"))
}

/// The workload holding the stage's lease: the holder of the grants record.
pub(crate) fn lease_target(
    r: &Resolved,
    run: &google_cloud_run_v2::client::Services,
    jobs: Option<&google_cloud_run_v2::client::Jobs>,
) -> Option<crate::lease::Target> {
    let h = r.holder();
    match (h.is_job(), jobs) {
        (false, _) => Some(crate::lease::Target::Service {
            run: run.clone(),
            name: h.service_name(),
        }),
        (true, Some(jobs)) => Some(crate::lease::Target::Job {
            jobs: jobs.clone(),
            name: h.service_name(),
        }),
        (true, None) => None,
    }
}

/// Takes the stage's lease for `command` (see [`crate::lease`]).
pub(crate) async fn take_lease(
    r: &Resolved,
    run: &google_cloud_run_v2::client::Services,
    jobs: Option<&google_cloud_run_v2::client::Jobs>,
    mode: crate::lease::Mode,
    command: &str,
    lock: &crate::cli::LockArgs,
    p: &Progress,
) -> Result<crate::lease::Guard> {
    let Some(target) = lease_target(r, run, jobs) else {
        return Ok(crate::lease::Guard::none());
    };
    let h = r.holder();
    crate::lease::acquire(target, mode, &h.app, &h.stage, command, lock.wait(), p).await
}

/// Exclusive for what changes the code or traffic of the stage; shared for
/// a preview (it only adds its own tag).
pub(crate) fn lease_mode(mode: &crate::traffic::Mode) -> crate::lease::Mode {
    match mode {
        crate::traffic::Mode::Preview { .. } => crate::lease::Mode::Shared,
        _ => crate::lease::Mode::Exclusive,
    }
}

/// The folder of runway.yaml (`.` when it is in the current one).
pub(crate) fn config_dir(ctx: &Context) -> std::path::PathBuf {
    ctx.config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| ".".into())
}

/// The commit a deploy records and checks against the one serving: this
/// checkout's for a build or a configured image, the image's for a promotion
/// (`None` when unknown: nothing is checked, and the record is removed).
pub(crate) fn deploy_commit(
    ctx: &Context,
    commit: &crate::commands::release::Commit,
) -> Option<crate::source::Source> {
    use crate::commands::release::Commit;
    match commit {
        Commit::Checkout(s) => s.clone(),
        Commit::Recorded(s) => {
            ctx.progress
                .info(format!("source: {s} (the promoted image's)"));
            Some(s.clone())
        }
        Commit::Unknown => {
            ctx.progress.warn(
                "the commit of the promoted image is unknown: older deploys are not checked, and the stage's commit record is removed",
            );
            None
        }
    }
}

/// The commit this checkout deploys, said once.
pub(crate) fn this_source(ctx: &Context) -> Option<crate::source::Source> {
    let s = crate::source::current(&config_dir(ctx));
    match &s {
        Some(s) => ctx.progress.info(format!("source: {s}")),
        None => ctx.progress.info(
            "source: not a git checkout (set RUNWAY_SOURCE_COMMIT and RUNWAY_SOURCE_TIME to record it): older deploys are not detected",
        ),
    }
    s
}
