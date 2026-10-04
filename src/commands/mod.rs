//! Command implementations.

pub mod completions;
pub mod deploy;
pub mod describe;
pub mod doctor;
pub mod info;
pub mod init;
pub mod logs;
pub mod plan;
pub mod preview;
pub mod traffic;
pub mod undeploy;
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
