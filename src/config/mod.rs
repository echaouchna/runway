//! Configuration loading, stage resolution and validation.
//!
//! Precedence (highest wins): CLI overrides (`--image`) → `stages.<stage>.*` →
//! top-level `provider` / `service` → built-in defaults. Maps (`env`,
//! `secrets`) are merged key by key; a `null` value in a stage removes an
//! inherited key. The build mode (`image` versus `source`/`dockerfile`) is
//! decided by the highest-precedence layer that sets any of those fields.

pub mod interp;
pub mod schema;
pub mod validate;

use crate::error::{Error, Result};
use crate::image_ref::ImageRef;
use crate::naming;
use schema::{RawConfig, RawProvider, RawService, RawStage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_CONFIG_FILE: &str = "runway.yaml";
pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_CPU: &str = "1";
pub const DEFAULT_MEMORY: &str = "512Mi";
pub const DEFAULT_TIMEOUT_SECONDS: u32 = 300;
pub const DEFAULT_CONCURRENCY: u32 = 80;
pub const DEFAULT_MIN_INSTANCES: u32 = 0;
pub const DEFAULT_MAX_INSTANCES: u32 = 10;
pub const DEFAULT_DOCKERFILE: &str = "Dockerfile";
pub const DEFAULT_BUILDER: &str = "gcr.io/buildpacks/builder:latest";
pub const DEFAULT_ARTIFACT_REPOSITORY: &str = "runway";

/// A parsed (but not yet resolved) configuration file.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub raw: RawConfig,
    pub path: PathBuf,
    /// Directory containing the configuration; relative paths resolve against it.
    pub dir: PathBuf,
}

impl LoadedConfig {
    pub fn stage_names(&self) -> Vec<String> {
        self.raw.stages.keys().cloned().collect()
    }
}

/// Reads and strictly parses a configuration file.
pub fn load(path: &Path) -> Result<LoadedConfig> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            let default = path.file_name().and_then(|n| n.to_str()) == Some("runway.yaml");
            let what = if default {
                format!(
                    "no runway.yaml or runway.yml found in `{}`",
                    path.parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new("."))
                        .display()
                )
            } else {
                format!("configuration file `{}` not found", path.display())
            };
            Error::config(what)
                .hint("run `runway init` to create one, or pass `--config <file>` (any name)")
        } else {
            Error::config(format!("cannot read `{}`: {e}", path.display()))
        }
    })?;
    let raw = parse(&text)
        .map_err(|msg| Error::config(format!("{} is not valid:\n{msg}", path.display())))?;
    let dir = path
        .parent()
        .map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        })
        .unwrap_or(Path::new("."))
        .to_path_buf();
    Ok(LoadedConfig {
        raw,
        path: path.to_path_buf(),
        dir,
    })
}

/// Parses YAML text into the raw schema. Errors include location and snippet.
pub fn parse(text: &str) -> std::result::Result<RawConfig, String> {
    serde_saphyr::from_str::<RawConfig>(text).map_err(|e| {
        e.to_string()
            .replace("invalid i64", "expected an integer")
            .replace(", set DuplicateKeyPolicy in Options if acceptable", "")
    })
}

/// A validation finding with the YAML path it refers to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Diagnostics {
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

impl Diagnostics {
    fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.errors.push(Issue {
            path: path.into(),
            message: message.into(),
        });
    }
    fn warn(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Issue {
            path: path.into(),
            message: message.into(),
        });
    }

    pub fn into_error(self, context: &str) -> Error {
        let mut msg = format!("{context}:");
        for e in &self.errors {
            msg.push_str(&format!("\n  - {e}"));
        }
        Error::config(msg)
    }
}

/// CLI-level overrides with the highest precedence.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub image: Option<String>,
}

/// Fully resolved and validated settings for one stage.
#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub app: String,
    pub stage: String,
    pub project: String,
    pub region: String,
    pub service_id: String,
    pub artifact: Artifact,
    pub service: ServiceConfig,
    pub retry: crate::retry::RetryConfig,
    pub apis: ApisConfig,
    /// Service account impersonated for API calls (config default; the CLI flag wins).
    pub impersonate: Option<String>,
    /// Tags bound to the deployment project: namespaced key -> value short name.
    pub project_tags: BTreeMap<String, String>,
    /// Buckets runway creates and keeps configured, by key.
    pub buckets: BTreeMap<String, BucketConfig>,
    /// Secrets runway creates, by key.
    pub secrets: BTreeMap<String, ManagedSecret>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ApisConfig {
    /// Enable missing APIs during deploy.
    pub enable: bool,
    /// APIs declared in addition to those runway infers.
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BucketConfig {
    pub key: String,
    pub name: String,
    pub location: String,
    pub storage_class: Option<String>,
    pub versioning: Option<bool>,
    pub delete_after_days: Option<u32>,
    pub labels: BTreeMap<String, String>,
}

impl Deployment {
    pub fn service_name(&self) -> String {
        naming::service_name(&self.project, &self.region, &self.app, &self.stage)
    }
    pub fn parent(&self) -> String {
        naming::location_parent(&self.project, &self.region)
    }
    pub fn labels(&self) -> BTreeMap<String, String> {
        naming::ownership_labels(&self.app, &self.stage)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Artifact {
    /// Deploy an existing image.
    Image {
        reference: String,
        #[serde(skip)]
        parsed: ImageRef,
    },
    /// Build from local source with Cloud Build.
    Build(BuildConfig),
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildConfig {
    /// Absolute (or config-relative) build context directory.
    pub context_dir: PathBuf,
    /// How the image is built.
    pub strategy: BuildStrategy,
    /// Rebuild on every deploy (`rebuild: always`).
    pub rebuild_always: bool,
    pub artifact_location: String,
    pub artifact_repository: String,
    pub source_bucket: String,
    pub build_service_account: String,
    /// Create the repository, source bucket and build service account (+ roles) if missing.
    pub create_resources: bool,
    /// Context-relative paths excluded from the archive (the config file, if inside the context).
    pub excluded: Vec<String>,
}

/// How a source build produces its image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BuildStrategy {
    /// `docker build -f <path>` (path relative to the context, `/` separators).
    Dockerfile { path: String },
    /// Cloud Native Buildpacks (`pack build --builder <builder>`).
    Buildpacks { builder: String },
}

impl BuildStrategy {
    pub fn dockerfile(&self) -> Option<&str> {
        match self {
            BuildStrategy::Dockerfile { path } => Some(path),
            BuildStrategy::Buildpacks { .. } => None,
        }
    }
}

impl std::fmt::Display for BuildStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildStrategy::Dockerfile { path } => write!(f, "Dockerfile `{path}`"),
            BuildStrategy::Buildpacks { builder } => write!(f, "buildpacks ({builder})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct SecretRef {
    pub secret: String,
    /// Version number, or `latest`.
    pub version: String,
    /// `version` is `latest` until runway resolves it to the newest enabled
    /// version at plan/deploy time.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pin_latest: bool,
    /// Mounted as a file at this path (instead of an environment variable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ServiceConfig {
    /// Every secret reference: the application's and the sidecars'.
    pub fn all_secrets(&self) -> impl Iterator<Item = &SecretRef> {
        self.secrets
            .values()
            .chain(self.sidecars.values().flat_map(|s| s.secrets.values()))
    }
}

impl SecretRef {
    /// `projects/P/secrets/S` (IDs are in the deployment project).
    pub fn full_name(&self, project: &str) -> String {
        if self.secret.starts_with("projects/") {
            self.secret.clone()
        } else {
            format!("projects/{project}/secrets/{}", self.secret)
        }
    }
}

/// A secret runway creates (values are added by people).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagedSecret {
    pub key: String,
    /// Secret ID in the deployment project.
    pub name: String,
    pub adders: Vec<String>,
    /// Empty: automatic replication.
    pub locations: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceConfig {
    pub port: u16,
    pub cpu: String,
    pub memory: String,
    pub timeout_seconds: u32,
    pub concurrency: u32,
    pub min_instances: u32,
    pub max_instances: u32,
    pub public: bool,
    /// `all`, `internal` or `internal-and-cloud-load-balancing`.
    pub ingress: String,
    pub health_check: Option<HealthCheck>,
    pub bootstrap: Option<BootstrapConfig>,
    pub otel_collector: Option<OtelCollector>,
    /// Extra containers, by name.
    pub sidecars: BTreeMap<String, SidecarConfig>,
    pub service_account: String,
    pub env: BTreeMap<String, String>,
    pub secrets: BTreeMap<String, SecretRef>,
    /// Resource Manager tags: namespaced key -> value short name.
    pub tags: BTreeMap<String, String>,
    pub volumes: BTreeMap<String, VolumeConfig>,
    pub iap: IapConfig,
    pub identity: IdentityConfig,
    /// `request-based` or `instance-based`.
    pub billing: String,
    pub startup_cpu_boost: bool,
    /// `gen1` or `gen2`; `None`: Cloud Run chooses.
    pub execution_environment: Option<String>,
    pub vpc: Option<VpcConfig>,
    /// Cloud SQL connection names (`PROJECT:REGION:INSTANCE`).
    pub cloud_sql: Vec<String>,
    pub custom_audiences: Vec<String>,
}

/// Direct VPC egress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VpcConfig {
    pub network: String,
    pub subnet: String,
    /// `private-ranges-only` or `all-traffic`.
    pub egress: String,
    pub network_tags: Vec<String>,
}

pub const CLOUD_SQL_CLIENT_ROLE: &str = "roles/cloudsql.client";
pub const BILLING_REQUEST: &str = "request-based";
pub const BILLING_INSTANCE: &str = "instance-based";
/// Where Cloud SQL sockets appear in the container (`/cloudsql/CONNECTION_NAME`).
pub const CLOUD_SQL_MOUNT: &str = "/cloudsql";
/// Volume name of the Cloud SQL connections.
pub const CLOUD_SQL_VOLUME: &str = "cloudsql";

/// A placeholder image that listens on $PORT, for `bootstrap.image`.
pub const HELLO_IMAGE: &str = "us-docker.pkg.dev/cloudrun/container/hello";
/// Latest Google-built collector release known to this runway version.
pub const OTEL_COLLECTOR_VERSION: &str = "0.160.0";
pub const OTEL_COLLECTOR_IMAGE: &str = "us-docker.pkg.dev/cloud-ops-agents-artifacts/google-cloud-opentelemetry-collector/otelcol-google";
pub const OTEL_COLLECTOR_NAME: &str = "otel-collector";
/// Roles the runtime service account needs for the default collector config.
pub const OTEL_COLLECTOR_ROLES: &[&str] = &[
    "roles/cloudtrace.agent",
    "roles/monitoring.metricWriter",
    "roles/logging.logWriter",
];
/// Default collector configuration: OTLP on localhost, Google Cloud exporters.
/// Validated with `otelcol-google validate` (0.160.0).
pub const OTEL_COLLECTOR_DEFAULT_CONFIG: &str = r#"receivers:
  otlp:
    protocols:
      grpc:
        endpoint: localhost:4317
      http:
        endpoint: localhost:4318
processors:
  memory_limiter:
    check_interval: 1s
    limit_percentage: 65
    spike_limit_percentage: 20
  resourcedetection:
    detectors: [env, gcp]
    timeout: 2s
    override: false
  batch:
    send_batch_size: 200
    timeout: 5s
exporters:
  googlecloud:
    log:
      default_log_name: opentelemetry-collector
  googlemanagedprometheus: {}
extensions:
  health_check:
    endpoint: 0.0.0.0:13133
service:
  extensions: [health_check]
  pipelines:
    traces:
      receivers: [otlp]
      processors: [memory_limiter, resourcedetection, batch]
      exporters: [googlecloud]
    metrics:
      receivers: [otlp]
      processors: [memory_limiter, resourcedetection, batch]
      exporters: [googlemanagedprometheus]
    logs:
      receivers: [otlp]
      processors: [memory_limiter, resourcedetection, batch]
      exporters: [googlecloud]
"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootstrapConfig {
    /// Placeholder image; `None` deploys the real app with the bootstrap ingress.
    pub image: Option<String>,
    pub ingress: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SidecarConfig {
    pub image: String,
    pub cpu: String,
    pub memory: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Environment variables from Secret Manager.
    pub secrets: BTreeMap<String, SecretRef>,
    pub health_check: Option<SidecarCheck>,
    pub start_before_app: bool,
    /// Service volume name -> mount path.
    pub volumes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SidecarCheck {
    pub port: u16,
    /// HTTP path; `None` for a TCP check.
    pub path: Option<String>,
}

/// Cloud Run runs at most 10 containers per instance.
pub const MAX_CONTAINERS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OtelCollector {
    pub image: String,
    /// False when neither `version` nor `image` is set: plan/deploy then use
    /// the newest released version (looked up in the registry).
    pub pinned: bool,
    pub cpu: String,
    pub memory: String,
    pub config: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthCheck {
    pub path: String,
    pub startup: ProbeSettings,
    /// `None` when liveness probing is disabled.
    pub liveness: Option<ProbeSettings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProbeSettings {
    pub initial_delay_seconds: u32,
    pub period_seconds: u32,
    pub timeout_seconds: u32,
    pub failure_threshold: u32,
}

impl ProbeSettings {
    /// Startup defaults: every 10 s, 3 s timeout, 12 failures (about 2 minutes to start).
    pub const STARTUP: Self = Self {
        initial_delay_seconds: 0,
        period_seconds: 10,
        timeout_seconds: 3,
        failure_threshold: 12,
    };
    /// Liveness defaults: every 30 s, 3 s timeout, restart after 3 failures.
    pub const LIVENESS: Self = Self {
        initial_delay_seconds: 0,
        period_seconds: 30,
        timeout_seconds: 3,
        failure_threshold: 3,
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeConfig {
    pub bucket: String,
    pub mount_path: String,
    pub read_only: bool,
    pub mount_options: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IapConfig {
    pub enabled: bool,
    /// Granted `roles/iap.httpsResourceAccessor` (added, never removed).
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IdentityConfig {
    /// Create the runtime service account if missing.
    pub create: bool,
    pub display_name: Option<String>,
    /// Roles granted to the runtime service account (added, never removed).
    pub roles: Vec<RoleBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleBinding {
    pub role: String,
    pub target: RoleTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoleTarget {
    Project {
        project: String,
    },
    Bucket {
        bucket: String,
    },
    Dataset {
        project: String,
        dataset: String,
    },
    /// Full name `projects/P/secrets/S`.
    Secret {
        name: String,
    },
    /// Artifact Registry repository (used for the build service account).
    Repository {
        project: String,
        location: String,
        repository: String,
    },
}

impl std::fmt::Display for RoleTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoleTarget::Project { project } => write!(f, "project {project}"),
            RoleTarget::Bucket { bucket } => write!(f, "bucket gs://{bucket}"),
            RoleTarget::Dataset { project, dataset } => write!(f, "dataset {project}.{dataset}"),
            RoleTarget::Secret { name } => write!(f, "secret {name}"),
            RoleTarget::Repository {
                location,
                repository,
                ..
            } => {
                write!(f, "repository {location}/{repository}")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub deployment: Deployment,
    pub warnings: Vec<Issue>,
}

/// Picks the highest-precedence value and remembers where it came from.
fn pick<T: Clone>(
    stage: &str,
    stage_val: &Option<T>,
    base_val: &Option<T>,
    section: &str,
    field: &str,
) -> Option<(T, String)> {
    if let Some(v) = stage_val {
        return Some((v.clone(), format!("stages.{stage}.{section}.{field}")));
    }
    base_val
        .as_ref()
        .map(|v| (v.clone(), format!("{section}.{field}")))
}

fn int_in_range(
    diags: &mut Diagnostics,
    value: Option<(i64, String)>,
    default: u32,
    min: i64,
    max: i64,
) -> u32 {
    match value {
        None => default,
        Some((v, path)) => {
            if v < min || v > max {
                diags.error(path, format!("must be between {min} and {max} (got {v})"));
                default
            } else {
                v as u32
            }
        }
    }
}

/// Resolves one stage: merges layers, applies defaults and validates.
pub fn resolve(
    cfg: &LoadedConfig,
    stage: &str,
    overrides: &Overrides,
) -> std::result::Result<Resolved, Diagnostics> {
    let raw = &cfg.raw;
    let mut d = Diagnostics::default();

    if raw.version != 1 {
        d.error(
            "version",
            format!(
                "unsupported schema version {} (this runway supports version 1)",
                raw.version
            ),
        );
    }
    if let Err(e) = validate::name_component("app", &raw.app, 40) {
        d.error("app", e);
    }
    if raw.stages.is_empty() {
        d.error(
            "stages",
            "define at least one stage, for example `stages: { dev: {} }`",
        );
    }
    for name in raw.stages.keys() {
        if let Err(e) = validate::name_component("stage name", name, 20) {
            d.error(format!("stages.{name}"), e);
        }
    }

    let empty = RawStage::default();
    let st: &RawStage = match raw.stages.get(stage) {
        Some(Some(s)) => s,
        Some(None) => &empty,
        None => {
            let known: Vec<&str> = raw.stages.keys().map(String::as_str).collect();
            d.error(
                "stages",
                format!(
                    "stage `{stage}` is not defined{}",
                    if known.is_empty() {
                        String::new()
                    } else {
                        format!(" (defined stages: {})", known.join(", "))
                    }
                ),
            );
            &empty
        }
    };

    let service_id = naming::service_id(&raw.app, stage);
    if service_id.len() > naming::MAX_SERVICE_NAME_LEN {
        d.error(
            "app",
            format!(
                "service name `{service_id}` (app-stage) exceeds {} characters; shorten `app` or the stage name",
                naming::MAX_SERVICE_NAME_LEN
            ),
        );
    }

    // ---- provider ----
    let p = |f: fn(&RawProvider) -> &Option<String>, field: &str| {
        pick(stage, f(&st.provider), f(&raw.provider), "provider", field)
    };
    let project = p(|x| &x.project, "project");
    let region = p(|x| &x.region, "region");
    let artifact_repository = p(|x| &x.artifact_repository, "artifact_repository");
    let artifact_location = p(|x| &x.artifact_location, "artifact_location");
    let source_bucket = p(|x| &x.source_bucket, "source_bucket");
    let build_sa = p(|x| &x.build_service_account, "build_service_account");

    let project = required(&mut d, project, "provider.project", validate::project_id);
    let region = required(&mut d, region, "provider.region", validate::region);

    // ---- variables and buckets (interpolation) ----
    let mut ix = interp::Interp::new(&project, &region, &raw.app, stage);
    let mut user_vars = raw.vars.clone();
    user_vars.extend(st.vars.clone());
    for (k, v) in &user_vars {
        let path = if st.vars.contains_key(k) {
            format!("stages.{stage}.vars.{k}")
        } else {
            format!("vars.{k}")
        };
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || k.is_empty() {
            d.error(
                path.clone(),
                "variable names may contain only letters, digits and `_`",
            );
        }
        // Variables may use the built-ins, not other variables.
        match interp::Interp::new(&project, &region, &raw.app, stage).apply(v) {
            Ok(val) => ix.set(format!("vars.{k}"), val),
            Err(e) => d.error(path, e),
        }
    }
    let buckets = resolve_buckets(&mut d, raw, &ix, &region);
    for b in buckets.values() {
        ix.set(format!("buckets.{}", b.key), b.name.clone());
    }
    let managed_secrets = resolve_secrets(&mut d, raw, &ix, &region);
    for s in managed_secrets.values() {
        ix.set(format!("secrets.{}", s.key), s.name.clone());
    }
    let interp_opt =
        |d: &mut Diagnostics, v: Option<(String, String)>| -> Option<(String, String)> {
            v.map(|(val, path)| match ix.apply(&val) {
                Ok(x) => (x, path),
                Err(e) => {
                    d.error(path.clone(), e);
                    (val, path)
                }
            })
        };
    let ixs = |d: &mut Diagnostics, v: &str, path: &str| -> String {
        match ix.apply(v) {
            Ok(x) => x,
            Err(e) => {
                d.error(path, e);
                v.to_string()
            }
        }
    };
    let source_bucket = interp_opt(&mut d, source_bucket);
    let build_sa = interp_opt(&mut d, build_sa);

    let enable_apis = pick(
        stage,
        &st.provider.enable_apis,
        &raw.provider.enable_apis,
        "provider",
        "enable_apis",
    )
    .map(|(v, _)| v)
    .unwrap_or(false);
    let mut extra_apis = Vec::new();
    if let Some((list, path)) = pick(
        stage,
        &st.provider.apis,
        &raw.provider.apis,
        "provider",
        "apis",
    ) {
        for (i, a) in list.into_iter().enumerate() {
            if let Err(e) = validate::api_name(&a) {
                d.error(format!("{path}[{i}]"), e);
            }
            if !extra_apis.contains(&a) {
                extra_apis.push(a);
            }
        }
    }
    let create_build = pick(
        stage,
        &st.provider.create_build_resources,
        &raw.provider.create_build_resources,
        "provider",
        "create_build_resources",
    )
    .map(|(v, _)| v)
    .unwrap_or(false);

    let impersonate = interp_opt(
        &mut d,
        pick(
            stage,
            &st.provider.impersonate_service_account,
            &raw.provider.impersonate_service_account,
            "provider",
            "impersonate_service_account",
        ),
    );
    if let Some((spec, path)) = &impersonate
        && let Err(e) = crate::gcp::Impersonation::parse(spec)
    {
        d.error(path.clone(), e);
    }
    let impersonate = impersonate.map(|(v, _)| v);
    let mut project_tags = BTreeMap::new();
    if let Some((tags, path)) = pick(
        stage,
        &st.provider.tags,
        &raw.provider.tags,
        "provider",
        "tags",
    ) {
        for (k, v) in tags {
            if let Err(e) = validate::tag_key(&k) {
                d.error(format!("{path}.{k}"), e);
            }
            if let Err(e) = validate::tag_value(&v) {
                d.error(format!("{path}.{k}"), e);
            }
            project_tags.insert(k, v);
        }
    }

    let check_opt = |d: &mut Diagnostics,
                     v: &Option<(String, String)>,
                     f: fn(&str) -> std::result::Result<(), String>| {
        if let Some((val, path)) = v
            && let Err(e) = f(val)
        {
            d.error(path.clone(), e);
        }
    };
    check_opt(&mut d, &artifact_repository, validate::repository_id);
    check_opt(&mut d, &artifact_location, validate::region);
    check_opt(&mut d, &source_bucket, validate::bucket_name);
    check_opt(&mut d, &build_sa, validate::service_account_email);

    // ---- service ----
    let base: &RawService = &raw.service;
    let sv: &RawService = &st.service;
    macro_rules! s {
        ($f:ident) => {
            pick(stage, &sv.$f, &base.$f, "service", stringify!($f))
        };
    }

    let artifact = resolve_artifact(
        &mut d,
        cfg,
        stage,
        overrides,
        base,
        sv,
        BuildProvider {
            region: region.clone(),
            artifact_repository,
            artifact_location,
            source_bucket,
            build_sa,
            create: create_build,
            project: project.clone(),
        },
    );
    if create_build && matches!(artifact, Some(Artifact::Image { .. })) {
        d.warn(
            "provider.create_build_resources",
            "ignored: this stage deploys an existing image",
        );
    }
    if let Some(Artifact::Build(b)) = &artifact
        && b.create_resources
        && validate::service_account_project(&b.build_service_account).is_none()
    {
        d.error(
            "provider.build_service_account",
            "create_build_resources can only create user-managed service accounts (NAME@PROJECT.iam.gserviceaccount.com)",
        );
    }
    if let Some(Artifact::Build(b)) = &artifact
        && b.create_resources
        && let Some(dup) = buckets.values().find(|x| x.name == b.source_bucket)
    {
        d.error(
            format!("buckets.{}", dup.key),
            "the build source bucket is created by `create_build_resources`; do not declare it again",
        );
    }

    let port = int_in_range(&mut d, s!(port), DEFAULT_PORT as u32, 1, 65535) as u16;

    let (cpu_millis, cpu) = match s!(cpu) {
        None => (1000, DEFAULT_CPU.to_string()),
        Some((v, path)) => match validate::cpu(&v.to_string_value()) {
            Ok((m, c)) => (m, c),
            Err(e) => {
                d.error(path, e);
                (1000, DEFAULT_CPU.to_string())
            }
        },
    };
    let memory = match s!(memory) {
        None => DEFAULT_MEMORY.to_string(),
        Some((v, path)) => {
            if let Err(e) = validate::memory(&v, Some(cpu_millis)) {
                d.error(path, e);
            }
            v
        }
    };
    let timeout_seconds = int_in_range(
        &mut d,
        s!(timeout_seconds),
        DEFAULT_TIMEOUT_SECONDS,
        1,
        3600,
    );
    let concurrency = int_in_range(&mut d, s!(concurrency), DEFAULT_CONCURRENCY, 1, 1000);
    let min_v = s!(min_instances);
    let max_v = s!(max_instances);
    let min_path = min_v.as_ref().map(|(_, p)| p.clone());
    let min_instances = int_in_range(&mut d, min_v, DEFAULT_MIN_INSTANCES, 0, 1000);
    let max_instances = int_in_range(&mut d, max_v, DEFAULT_MAX_INSTANCES, 1, 1000);
    if min_instances > max_instances {
        d.error(
            min_path
                .clone()
                .unwrap_or_else(|| "service.min_instances".into()),
            format!(
                "min_instances ({min_instances}) must not exceed max_instances ({max_instances})"
            ),
        );
    }
    if min_instances > 0 {
        d.warn(
            min_path
                .clone()
                .unwrap_or_else(|| "service.min_instances".into()),
            format!("{min_instances} instance(s) will be kept warm and billed while idle"),
        );
    }
    let public = s!(public).map(|(v, _)| v).unwrap_or(false);
    let health_check =
        s!(health_check).map(|(raw_hc, path)| resolve_health_check(&mut d, raw_hc, &path));
    let ingress = match s!(ingress) {
        None => "all".to_string(),
        Some((v, path)) => {
            if !["all", "internal", "internal-and-cloud-load-balancing"].contains(&v.as_str()) {
                d.error(
                    path,
                    format!(
                        "`{v}` must be `all`, `internal` or `internal-and-cloud-load-balancing`"
                    ),
                );
            } else if v != "all" && public {
                d.warn(
                    path,
                    "`public: true` with restricted ingress: only internal or load-balancer traffic reaches the service",
                );
            }
            v
        }
    };

    let service_account = match s!(service_account) {
        None => {
            d.error(
                "service.service_account",
                "a runtime service account is required (runway does not deploy with the broad default compute service account)",
            );
            String::new()
        }
        Some((v, path)) => {
            let v = ixs(&mut d, &v, &path);
            if let Err(e) = validate::service_account_email(&v) {
                d.error(path, e);
            }
            v
        }
    };

    // ---- bootstrap ----
    let bootstrap = s!(bootstrap).map(|(b, path)| {
        let image = b.image;
        if let Some(img) = &image
            && let Err(e) = ImageRef::parse(img)
        {
            d.error(format!("{path}.image"), e);
        }
        let ingress = b.ingress.unwrap_or_else(|| "internal".to_string());
        if !["all", "internal", "internal-and-cloud-load-balancing"].contains(&ingress.as_str()) {
            d.error(
                format!("{path}.ingress"),
                "must be `all`, `internal` or `internal-and-cloud-load-balancing`",
            );
        }
        BootstrapConfig { image, ingress }
    });

    // ---- OpenTelemetry Collector sidecar ----
    let otel_collector = match s!(otel_collector) {
        Some((o, path)) if o.enabled.unwrap_or(true) => {
            let image = match (&o.image, &o.version) {
                (Some(img), _) => img.clone(),
                (None, v) => format!(
                    "{OTEL_COLLECTOR_IMAGE}:{}",
                    v.as_deref()
                        .filter(|v| *v != "latest")
                        .unwrap_or(OTEL_COLLECTOR_VERSION)
                ),
            };
            if let Err(e) = ImageRef::parse(&image) {
                d.error(format!("{path}.image"), e);
            }
            let (ocpu_m, ocpu) = match &o.cpu {
                None => (1000, "1".to_string()),
                Some(v) => match validate::cpu(&v.to_string_value()) {
                    Ok(x) => x,
                    Err(e) => {
                        d.error(format!("{path}.cpu"), e);
                        (1000, "1".to_string())
                    }
                },
            };
            let omem = o.memory.clone().unwrap_or_else(|| "512Mi".to_string());
            if let Err(e) = validate::memory(&omem, Some(ocpu_m)) {
                d.error(format!("{path}.memory"), e);
            }
            let config = o
                .config
                .clone()
                .unwrap_or_else(|| OTEL_COLLECTOR_DEFAULT_CONFIG.to_string());
            if config.trim().is_empty() {
                d.error(format!("{path}.config"), "must not be empty");
            }
            let pinned = o.image.is_some() || o.version.as_deref().is_some_and(|v| v != "latest");
            Some(OtelCollector {
                image,
                pinned,
                cpu: ocpu,
                memory: omem,
                config,
            })
        }
        _ => None,
    };

    // ---- env / secrets ----
    let env = merge_map(&mut d, stage, &base.env, &sv.env, "env");
    let mut env_out = BTreeMap::new();
    for (k, (v, path)) in env {
        if let Err(e) = validate::env_name(&k) {
            d.error(path.clone(), e);
        }
        let value = ixs(&mut d, &v.to_string_value(), &path);
        if value.len() > 32768 {
            d.error(path.clone(), "value exceeds 32768 bytes");
        }
        if validate::looks_sensitive(&k) {
            d.warn(
                path,
                "this looks like a credential; plain env values are visible in the Cloud Run configuration. Prefer `secrets`",
            );
        }
        env_out.insert(k, value);
    }
    // The collector listens on localhost: point the app at it unless told otherwise.
    if otel_collector.is_some() {
        match env_out.get("OTEL_EXPORTER_OTLP_ENDPOINT") {
            None => {
                env_out.insert(
                    "OTEL_EXPORTER_OTLP_ENDPOINT".into(),
                    "http://localhost:4318".into(),
                );
            }
            Some(v) if !v.contains("localhost") && !v.contains("127.0.0.1") => d.warn(
                "service.env.OTEL_EXPORTER_OTLP_ENDPOINT",
                format!("`{v}` bypasses the otel_collector sidecar (it listens on http://localhost:4318)"),
            ),
            Some(_) => {}
        }
    }
    let secrets = merge_map(&mut d, stage, &base.secrets, &sv.secrets, "secrets");
    let mut secrets_out = BTreeMap::new();
    let mut secret_dirs = std::collections::BTreeSet::new();
    for (k, (v, path)) in secrets {
        if k.is_empty()
            || !k
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            d.error(
                path.clone(),
                "secret keys may contain only letters, digits, `_` and `-`",
            );
        }
        if env_out.contains_key(&k) {
            d.error(
                path.clone(),
                format!("`{k}` is defined both in env and secrets"),
            );
        }
        let secret_name = ixs(&mut d, &v.secret, &format!("{path}.secret"));
        if let Err(e) = validate::secret_name(&secret_name) {
            d.error(format!("{path}.secret"), e);
        }
        let file = v
            .path
            .as_ref()
            .map(|p| ixs(&mut d, p, &format!("{path}.path")));
        if let Some(f) = &file {
            match validate::mount_path(f) {
                Err(e) => d.error(format!("{path}.path"), e),
                Ok(()) => {
                    let dir = secret_dir(f);
                    if dir == "/" || validate::mount_path(dir).is_err() {
                        d.error(
                            format!("{path}.path"),
                            format!("`{f}`: the secret file needs its own directory (for example /secrets/NAME/value)"),
                        );
                    } else if !secret_dirs.insert(dir.to_string()) {
                        d.error(
                            format!("{path}.path"),
                            format!("directory `{dir}` already holds another secret file; Cloud Run mounts one secret per directory"),
                        );
                    }
                }
            }
        }
        let (version, pin_latest) = match &v.version {
            None if file.is_some() => ("latest".to_string(), false),
            None => ("latest".to_string(), true),
            Some(s) => (s.to_string_value(), false),
        };
        if let Err(e) = validate::secret_version(&version) {
            d.error(format!("{path}.version"), e);
        } else if version == "latest" && !pin_latest && file.is_none() {
            d.warn(
                format!("{path}.version"),
                "`latest` is resolved when each instance starts, so instances may run different values; omit `version` to pin the newest version at deploy time",
            );
        }
        if file.is_none()
            && let Err(e) = validate::env_name(&k)
        {
            d.error(path.clone(), e);
        }
        secrets_out.insert(
            k,
            SecretRef {
                secret: secret_name,
                version,
                pin_latest,
                path: file,
            },
        );
    }

    // ---- tags ----
    let mut tags_out = BTreeMap::new();
    for (k, (v, path)) in merge_map(&mut d, stage, &base.tags, &sv.tags, "tags") {
        if let Err(e) = validate::tag_key(&k) {
            d.error(path.clone(), e);
        }
        if let Err(e) = validate::tag_value(&v) {
            d.error(path, e);
        }
        tags_out.insert(k, v);
    }

    // ---- volumes ----
    let mut volumes_out = BTreeMap::new();
    let mut mount_paths = std::collections::BTreeSet::new();
    for (name, (v, path)) in merge_map(&mut d, stage, &base.volumes, &sv.volumes, "volumes") {
        if let Err(e) = validate::volume_name(&name) {
            d.error(path.clone(), e);
        }
        let vbucket = ixs(&mut d, &v.bucket, &format!("{path}.bucket"));
        if let Err(e) = validate::bucket_name(&vbucket) {
            d.error(format!("{path}.bucket"), e);
        }
        if let Err(e) = validate::mount_path(&v.mount_path) {
            d.error(format!("{path}.mount_path"), e);
        } else if secrets_out
            .values()
            .filter_map(|s: &SecretRef| s.path.as_deref())
            .any(|f| secret_dir(f) == v.mount_path)
        {
            d.error(
                format!("{path}.mount_path"),
                format!("`{}` is also a secret file directory", v.mount_path),
            );
        } else if !mount_paths.insert(v.mount_path.clone()) {
            d.error(
                format!("{path}.mount_path"),
                format!("`{}` is mounted twice", v.mount_path),
            );
        }
        let opts = v.mount_options.clone().unwrap_or_default();
        for o in &opts {
            if o.is_empty() || o.chars().any(char::is_whitespace) {
                d.error(
                    format!("{path}.mount_options"),
                    format!("invalid mount option `{o}`"),
                );
            }
        }
        volumes_out.insert(
            name,
            VolumeConfig {
                bucket: vbucket,
                mount_path: v.mount_path,
                read_only: v.read_only.unwrap_or(false),
                mount_options: opts,
            },
        );
    }

    // ---- billing, execution environment, VPC, Cloud SQL, audiences ----
    let memory_bytes = validate::parse_memory_bytes(&memory).unwrap_or(512 * 1024 * 1024);
    let small_memory = memory_bytes < 512 * 1024 * 1024;
    let fractional_cpu = cpu_millis < 1000;
    if fractional_cpu && concurrency != 1 {
        d.error(
            "service.concurrency",
            format!(
                "with less than 1 CPU, Cloud Run requires `concurrency: 1` (got {concurrency})"
            ),
        );
    }
    let billing = match s!(billing) {
        None => BILLING_REQUEST.to_string(),
        Some((v, path)) => {
            if v == BILLING_INSTANCE {
                if small_memory {
                    d.error(
                        path.clone(),
                        "instance-based billing needs at least 512Mi of memory",
                    );
                }
                if fractional_cpu {
                    d.error(path.clone(), "instance-based billing needs at least 1 CPU");
                }
            } else if v != BILLING_REQUEST {
                d.error(
                    path.clone(),
                    format!("`{v}` must be `{BILLING_REQUEST}` or `{BILLING_INSTANCE}`"),
                );
            }
            v
        }
    };
    let startup_cpu_boost = s!(startup_cpu_boost).map(|(v, _)| v).unwrap_or(false);
    let cloud_sql_v = s!(cloud_sql);
    let execution_environment = match s!(execution_environment) {
        None => {
            // Cloud Storage volumes run on gen2, which Cloud Run picks for them.
            if !volumes_out.is_empty() && fractional_cpu {
                d.error(
                    "service.cpu",
                    "Cloud Storage volumes need the gen2 execution environment, which needs at least 1 CPU",
                );
            } else if !volumes_out.is_empty() && small_memory {
                d.error(
                    "service.memory",
                    "Cloud Storage volumes need the gen2 execution environment, which needs at least 512Mi of memory",
                );
            }
            None
        }
        Some((v, path)) => {
            match v.as_str() {
                "gen2" => {
                    if small_memory {
                        d.error(path.clone(), "gen2 needs at least 512Mi of memory");
                    }
                    if fractional_cpu {
                        d.error(path.clone(), "less than 1 CPU needs gen1");
                    }
                }
                "gen1" => {
                    if !volumes_out.is_empty() {
                        d.error(path.clone(), "Cloud Storage volumes need gen2");
                    }
                    if cloud_sql_v.as_ref().is_some_and(|(l, _)| !l.is_empty()) {
                        d.warn(
                            path.clone(),
                            "on gen1, Cloud SQL connections only work with instances that use the per-instance CA (GOOGLE_MANAGED_INTERNAL_CA)",
                        );
                    }
                }
                _ => d.error(path.clone(), format!("`{v}` must be `gen1` or `gen2`")),
            }
            Some(v)
        }
    };
    let vpc = s!(vpc).map(|(raw, path)| {
        let network = ixs(&mut d, &raw.network, &format!("{path}.network"));
        if let Err(e) = validate::vpc_network(&network) {
            d.error(format!("{path}.network"), e);
        }
        let subnet = ixs(&mut d, &raw.subnet, &format!("{path}.subnet"));
        if let Err(e) = validate::vpc_subnet(&subnet, &region) {
            d.error(format!("{path}.subnet"), e);
        }
        let egress = raw.egress.unwrap_or_else(|| "private-ranges-only".into());
        if !["private-ranges-only", "all-traffic"].contains(&egress.as_str()) {
            d.error(
                format!("{path}.egress"),
                format!("`{egress}` must be `private-ranges-only` or `all-traffic`"),
            );
        }
        let mut network_tags: Vec<String> = Vec::new();
        for t in raw.network_tags.unwrap_or_default() {
            if let Err(e) = validate::network_tag(&t) {
                d.error(format!("{path}.network_tags"), e);
            } else if network_tags.contains(&t) {
                d.warn(
                    format!("{path}.network_tags"),
                    format!("`{t}` is listed twice"),
                );
            } else {
                network_tags.push(t);
            }
        }
        // Full names: a name alone is in the deployment project.
        VpcConfig {
            network: validate::full_network(&network, &project),
            subnet: validate::full_subnet(&subnet, &project, &region),
            egress,
            network_tags,
        }
    });
    let mut cloud_sql: Vec<String> = Vec::new();
    let mut cloud_sql_projects: Vec<String> = Vec::new();
    if let Some((list, path)) = cloud_sql_v {
        for raw_name in list {
            let n = ixs(&mut d, &raw_name, &path);
            // An instance name alone: this project and region (like gcloud).
            let n = if n.contains(':') {
                n
            } else {
                format!("{project}:{region}:{n}")
            };
            match validate::cloud_sql_instance(&n) {
                Err(e) => d.error(path.clone(), e),
                Ok(_) if cloud_sql.contains(&n) => {
                    d.warn(path.clone(), format!("`{n}` is listed twice"));
                }
                Ok(p) => {
                    if !cloud_sql_projects.iter().any(|x| x == p) {
                        cloud_sql_projects.push(p.to_string());
                    }
                    cloud_sql.push(n);
                }
            }
        }
    }
    if !cloud_sql.is_empty() {
        if let Some(name) = volumes_out.keys().find(|k| *k == CLOUD_SQL_VOLUME) {
            d.error(
                format!("service.volumes.{name}"),
                format!("`{CLOUD_SQL_VOLUME}` is reserved for the Cloud SQL connections"),
            );
        }
        let at_cloudsql = |p: &str| p == CLOUD_SQL_MOUNT || p.starts_with("/cloudsql/");
        if let Some((name, _)) = volumes_out.iter().find(|(_, v)| at_cloudsql(&v.mount_path)) {
            d.error(
                format!("service.volumes.{name}.mount_path"),
                format!("`{CLOUD_SQL_MOUNT}` is where the Cloud SQL connections are mounted"),
            );
        }
        if let Some((key, _)) = secrets_out
            .iter()
            .find(|(_, s)| s.path.as_deref().is_some_and(at_cloudsql))
        {
            d.error(
                format!("service.secrets.{key}.path"),
                format!("`{CLOUD_SQL_MOUNT}` is where the Cloud SQL connections are mounted"),
            );
        }
    }
    let mut custom_audiences: Vec<String> = Vec::new();
    if let Some((list, path)) = s!(custom_audiences) {
        for a in list {
            let a = ixs(&mut d, &a, &path);
            if a.is_empty() || a.chars().any(char::is_whitespace) {
                d.error(
                    path.clone(),
                    format!("audience `{a}` must be non-empty, without spaces"),
                );
            } else if custom_audiences.contains(&a) {
                d.warn(path.clone(), format!("`{a}` is listed twice"));
            } else {
                custom_audiences.push(a);
            }
        }
        if serde_json::to_string(&custom_audiences).map_or(0, |j| j.len()) > 32_768 {
            d.error(
                path,
                "the audiences exceed Cloud Run's limit (32,768 characters as JSON)",
            );
        }
    }

    // ---- sidecars ----
    let app_port = port;
    let mut sidecars_out = BTreeMap::new();
    for (name, (sc, path)) in merge_map(&mut d, stage, &base.sidecars, &sv.sidecars, "sidecars") {
        if let Err(e) = validate::container_name(&name) {
            d.error(path.clone(), e);
        } else if name == crate::gcp::run::APP_CONTAINER
            || (name == OTEL_COLLECTOR_NAME && otel_collector.is_some())
        {
            d.error(path.clone(), format!("`{name}` is reserved"));
        }
        let image = ixs(&mut d, &sc.image, &format!("{path}.image"));
        if let Err(e) = ImageRef::parse(&image) {
            d.error(format!("{path}.image"), e);
        }
        let (cpu_m, sc_cpu) = match &sc.cpu {
            None => (1000, "1".to_string()),
            Some(v) => validate::cpu(&v.to_string_value()).unwrap_or_else(|e| {
                d.error(format!("{path}.cpu"), e);
                (1000, "1".to_string())
            }),
        };
        let sc_mem = sc.memory.clone().unwrap_or_else(|| "512Mi".to_string());
        if let Err(e) = validate::memory(&sc_mem, Some(cpu_m)) {
            d.error(format!("{path}.memory"), e);
        }
        let list = |d: &mut Diagnostics, v: &Option<Vec<String>>, field: &str| -> Vec<String> {
            v.iter()
                .flatten()
                .enumerate()
                .map(|(i, a)| ixs(d, a, &format!("{path}.{field}[{i}]")))
                .collect()
        };
        let command = list(&mut d, &sc.command, "command");
        let args = list(&mut d, &sc.args, "args");
        let mut sc_env = BTreeMap::new();
        for (k, v) in sc.env.iter().flatten() {
            let p = format!("{path}.env.{k}");
            if let Err(e) = validate::env_name(k) {
                d.error(p.clone(), e);
            }
            sc_env.insert(k.clone(), ixs(&mut d, &v.to_string_value(), &p));
        }
        let mut sc_secrets = BTreeMap::new();
        for (k, r) in sc.secrets.iter().flatten() {
            let p = format!("{path}.secrets.{k}");
            if let Err(e) = validate::env_name(k) {
                d.error(p.clone(), e);
            }
            if sc_env.contains_key(k) {
                d.error(
                    p.clone(),
                    format!("`{k}` is defined both in env and secrets"),
                );
            }
            if r.path.is_some() {
                d.error(
                    format!("{p}.path"),
                    "sidecar secrets are environment variables only",
                );
            }
            let secret = ixs(&mut d, &r.secret, &format!("{p}.secret"));
            if let Err(e) = validate::secret_name(&secret) {
                d.error(format!("{p}.secret"), e);
            }
            let (version, pin_latest) = match &r.version {
                None => ("latest".to_string(), true),
                Some(v) => (v.to_string_value(), false),
            };
            if let Err(e) = validate::secret_version(&version) {
                d.error(format!("{p}.version"), e);
            }
            sc_secrets.insert(
                k.clone(),
                SecretRef {
                    secret,
                    version,
                    pin_latest,
                    path: None,
                },
            );
        }
        let health_check = sc.health_check.as_ref().and_then(|h| {
            let p = format!("{path}.health_check");
            let port = match u16::try_from(h.port) {
                Ok(p) if p > 0 => p,
                _ => {
                    d.error(format!("{p}.port"), "must be between 1 and 65535");
                    return None;
                }
            };
            if port == app_port {
                d.error(
                    format!("{p}.port"),
                    format!("{port} is the application's port; containers share the network"),
                );
            }
            if let Some(hp) = &h.path
                && !hp.starts_with('/')
            {
                d.error(format!("{p}.path"), "must start with `/`");
            }
            Some(SidecarCheck {
                port,
                path: h.path.clone(),
            })
        });
        let mut mounts = BTreeMap::new();
        for (vol, mp) in sc.volumes.iter().flatten() {
            let p = format!("{path}.volumes.{vol}");
            if !volumes_out.contains_key(vol) {
                d.error(p.clone(), format!("no volume `{vol}` in service.volumes"));
            }
            if let Err(e) = validate::mount_path(mp) {
                d.error(p, e);
            }
            mounts.insert(vol.clone(), mp.clone());
        }
        sidecars_out.insert(
            name,
            SidecarConfig {
                image,
                cpu: sc_cpu,
                memory: sc_mem,
                command,
                args,
                env: sc_env,
                secrets: sc_secrets,
                health_check,
                start_before_app: sc.start_before_app.unwrap_or(true),
                volumes: mounts,
            },
        );
    }
    let containers = 1 + usize::from(otel_collector.is_some()) + sidecars_out.len();
    if containers > MAX_CONTAINERS {
        d.error(
            "service.sidecars",
            format!(
                "{containers} containers; Cloud Run allows at most {MAX_CONTAINERS} per instance"
            ),
        );
    }

    // ---- IAP ----
    let iap = match s!(iap) {
        None => IapConfig::default(),
        Some((raw_iap, path)) => {
            let mut members = Vec::new();
            for (i, m) in raw_iap.members.unwrap_or_default().iter().enumerate() {
                let mpath = format!("{path}.members[{i}]");
                let m = ixs(&mut d, m, &mpath);
                if let Err(e) = validate::iam_member(&m) {
                    d.error(mpath, e);
                }
                members.push(m);
            }
            let enabled = raw_iap.enabled.unwrap_or(true);
            if !enabled && !members.is_empty() {
                d.warn(path.clone(), "members are ignored while IAP is disabled");
            }
            if enabled && members.is_empty() {
                d.warn(
                    path.clone(),
                    "IAP is enabled but no members are granted access; nobody will get in",
                );
            }
            if enabled && public {
                d.warn(
                    path,
                    "`public: true` with IAP: IAP still authenticates every request; `allUsers` invoker access is unnecessary",
                );
            }
            IapConfig {
                enabled,
                members: if enabled { members } else { vec![] },
            }
        }
    };

    // ---- identity ----
    let identity = match s!(identity) {
        None => IdentityConfig::default(),
        Some((raw_id, path)) => {
            let create = raw_id.create.unwrap_or(false);
            if create
                && validate::service_account_project(&service_account).is_none()
                && !service_account.is_empty()
            {
                d.error(
                    format!("{path}.create"),
                    "only user-managed service accounts (NAME@PROJECT.iam.gserviceaccount.com) can be created",
                );
            }
            if create
                && let Some(local) = service_account.split('@').next()
                && !(6..=30).contains(&local.len())
            {
                d.error(
                    "service.service_account",
                    "service account names must be 6-30 characters to be created",
                );
            }
            let mut roles = Vec::new();
            for (i, mut rb) in raw_id.roles.unwrap_or_default().into_iter().enumerate() {
                let rp = format!("{path}.roles[{i}]");
                for (field, val) in [
                    ("project", &mut rb.project),
                    ("bucket", &mut rb.bucket),
                    ("dataset", &mut rb.dataset),
                    ("secret", &mut rb.secret),
                ] {
                    if let Some(v) = val.as_mut() {
                        *v = ixs(&mut d, v, &format!("{rp}.{field}"));
                    }
                }
                if let Err(e) = validate::role_name(&rb.role) {
                    d.error(format!("{rp}.role"), e);
                }
                let set = [
                    rb.project.is_some(),
                    rb.bucket.is_some(),
                    rb.dataset.is_some(),
                    rb.secret.is_some(),
                ]
                .iter()
                .filter(|x| **x)
                .count();
                if set != 1 {
                    d.error(
                        rp.clone(),
                        "set exactly one of `project`, `bucket`, `dataset` or `secret`",
                    );
                    continue;
                }
                let target = if let Some(p) = rb.project {
                    if let Err(e) = validate::project_id(&p) {
                        d.error(format!("{rp}.project"), e);
                    }
                    RoleTarget::Project { project: p }
                } else if let Some(b) = rb.bucket {
                    if let Err(e) = validate::bucket_name(&b) {
                        d.error(format!("{rp}.bucket"), e);
                    }
                    RoleTarget::Bucket { bucket: b }
                } else if let Some(ds) = rb.dataset {
                    match validate::dataset_ref(&ds) {
                        Ok((project, dataset)) => RoleTarget::Dataset { project, dataset },
                        Err(e) => {
                            d.error(format!("{rp}.dataset"), e);
                            continue;
                        }
                    }
                } else {
                    let sname = rb.secret.unwrap_or_default();
                    if let Err(e) = validate::secret_name(&sname) {
                        d.error(format!("{rp}.secret"), e);
                    }
                    let name = if sname.starts_with("projects/") {
                        sname
                    } else {
                        format!("projects/{project}/secrets/{sname}")
                    };
                    RoleTarget::Secret { name }
                };
                let binding = RoleBinding {
                    role: rb.role,
                    target,
                };
                if roles.contains(&binding) {
                    d.warn(rp, "duplicate role binding");
                } else {
                    roles.push(binding);
                }
            }
            IdentityConfig {
                create,
                display_name: raw_id
                    .display_name
                    .map(|n| ixs(&mut d, &n, &format!("{path}.display_name"))),
                roles,
            }
        }
    };

    // The collector's exporters need these roles on the deployment project.
    let mut identity = identity;
    // Cloud SQL connections need the client role in each instance's project.
    for p in &cloud_sql_projects {
        let b = RoleBinding {
            role: CLOUD_SQL_CLIENT_ROLE.to_string(),
            target: RoleTarget::Project { project: p.clone() },
        };
        if !identity.roles.contains(&b) {
            identity.roles.push(b);
        }
    }
    if otel_collector.is_some() {
        for role in OTEL_COLLECTOR_ROLES {
            let b = RoleBinding {
                role: role.to_string(),
                target: RoleTarget::Project {
                    project: project.clone(),
                },
            };
            if !identity.roles.contains(&b) {
                identity.roles.push(b);
            }
        }
    }

    // ---- retry ----
    let retry = resolve_retry(&mut d, raw.retry.as_ref());

    // An interpolation error makes further checks of the same field noise.
    let interp_failed: std::collections::BTreeSet<String> = d
        .errors
        .iter()
        .filter(|e| {
            e.message.starts_with("unknown variable") || e.message.starts_with("unterminated")
        })
        .map(|e| e.path.clone())
        .collect();
    d.errors.retain(|e| {
        !interp_failed.contains(&e.path)
            || e.message.starts_with("unknown variable")
            || e.message.starts_with("unterminated")
    });

    if !d.errors.is_empty() {
        d.errors.sort();
        d.errors.dedup();
        return Err(d);
    }

    Ok(Resolved {
        deployment: Deployment {
            app: raw.app.clone(),
            stage: stage.to_string(),
            project,
            region,
            service_id,
            artifact: artifact.expect("artifact resolved when no errors"),
            service: ServiceConfig {
                port,
                cpu,
                memory,
                timeout_seconds,
                concurrency,
                min_instances,
                max_instances,
                public,
                ingress,
                health_check,
                bootstrap,
                otel_collector,
                sidecars: sidecars_out,
                service_account,
                env: env_out,
                secrets: secrets_out,
                tags: tags_out,
                volumes: volumes_out,
                iap,
                identity,
                billing,
                startup_cpu_boost,
                execution_environment,
                vpc,
                cloud_sql,
                custom_audiences,
            },
            retry,
            apis: ApisConfig {
                enable: enable_apis,
                extra: extra_apis,
            },
            impersonate,
            project_tags,
            buckets,
            secrets: managed_secrets,
        },
        warnings: d.warnings,
    })
}

fn resolve_probe(
    d: &mut Diagnostics,
    raw: Option<&schema::RawProbe>,
    defaults: ProbeSettings,
    path: &str,
    max_delay: i64,
) -> ProbeSettings {
    let r = raw.cloned().unwrap_or_default();
    let mut out = defaults;
    let mut field = |v: Option<i64>, name: &str, min: i64, max: i64, slot: &mut u32| {
        if let Some(x) = v {
            if (min..=max).contains(&x) {
                *slot = x as u32;
            } else {
                d.error(
                    format!("{path}.{name}"),
                    format!("must be between {min} and {max} (got {x})"),
                );
            }
        }
    };
    field(
        r.initial_delay_seconds,
        "initial_delay_seconds",
        0,
        max_delay,
        &mut out.initial_delay_seconds,
    );
    field(
        r.period_seconds,
        "period_seconds",
        1,
        max_delay,
        &mut out.period_seconds,
    );
    field(
        r.timeout_seconds,
        "timeout_seconds",
        1,
        max_delay,
        &mut out.timeout_seconds,
    );
    field(
        r.failure_threshold,
        "failure_threshold",
        1,
        100,
        &mut out.failure_threshold,
    );
    if out.timeout_seconds > out.period_seconds {
        d.error(
            format!("{path}.timeout_seconds"),
            format!(
                "must not exceed period_seconds ({}s); the effective value is {}s",
                out.period_seconds, out.timeout_seconds
            ),
        );
    }
    out
}

fn resolve_health_check(
    d: &mut Diagnostics,
    raw: schema::RawHealthCheck,
    path: &str,
) -> HealthCheck {
    if !raw.path.starts_with('/') || raw.path.chars().any(char::is_whitespace) {
        d.error(
            format!("{path}.path"),
            format!(
                "`{}` must be an absolute HTTP path such as `/healthz`",
                raw.path
            ),
        );
    }
    // Cloud Run limits: startup values up to 240 s, liveness up to 3600 s.
    let startup = resolve_probe(
        d,
        raw.startup.as_ref(),
        ProbeSettings::STARTUP,
        &format!("{path}.startup"),
        240,
    );
    let liveness = match raw.liveness {
        None | Some(schema::RawLiveness::Enabled(true)) => Some(ProbeSettings::LIVENESS),
        Some(schema::RawLiveness::Enabled(false)) => None,
        Some(schema::RawLiveness::Settings(p)) => Some(resolve_probe(
            d,
            Some(&p),
            ProbeSettings::LIVENESS,
            &format!("{path}.liveness"),
            3600,
        )),
    };
    HealthCheck {
        path: raw.path,
        startup,
        liveness,
    }
}

/// Directory a secret file is mounted in (Cloud Run mounts one secret per directory).
pub fn secret_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((dir, _)) => dir,
    }
}

fn resolve_secrets(
    d: &mut Diagnostics,
    raw: &RawConfig,
    ix: &interp::Interp,
    region: &str,
) -> BTreeMap<String, ManagedSecret> {
    let mut out = BTreeMap::new();
    let mut names = std::collections::BTreeSet::new();
    for (key, s) in &raw.secrets {
        let path = format!("secrets.{key}");
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            d.error(
                path.clone(),
                "secret keys may contain only letters, digits, `_` and `-`",
            );
        }
        let name = match ix.apply(s.name.as_deref().unwrap_or(key)) {
            Ok(n) => n,
            Err(e) => {
                d.error(format!("{path}.name"), e);
                continue;
            }
        };
        if name.starts_with("projects/") {
            d.error(
                format!("{path}.name"),
                "runway creates secrets in the deployment project: use a secret ID, not a full name",
            );
        } else if let Err(e) = validate::secret_name(&name) {
            d.error(format!("{path}.name"), e);
        } else if !names.insert(name.clone()) {
            d.error(
                format!("{path}.name"),
                format!("secret `{name}` is declared twice"),
            );
        }
        let mut adders = Vec::new();
        for (i, m) in s.adders.clone().unwrap_or_default().iter().enumerate() {
            let mp = format!("{path}.adders[{i}]");
            match ix.apply(m) {
                Ok(m) => {
                    if let Err(e) = validate::iam_member(&m) {
                        d.error(mp, e);
                    }
                    adders.push(m);
                }
                Err(e) => d.error(mp, e),
            }
        }
        if adders.is_empty() {
            d.warn(
                format!("{path}.adders"),
                "nobody is granted roles/secretmanager.secretVersionAdder; only project owners/admins can add the value",
            );
        }
        let locations = s
            .locations
            .clone()
            .unwrap_or_else(|| vec![region.to_string()]);
        for (i, l) in locations.iter().enumerate() {
            if let Err(e) = validate::region(l) {
                d.error(format!("{path}.locations[{i}]"), e);
            }
        }
        let labels = s.labels.clone().unwrap_or_default();
        for (k, v) in &labels {
            if let Err(e) = validate::label(k, v) {
                d.error(format!("{path}.labels.{k}"), e);
            }
        }
        out.insert(
            key.clone(),
            ManagedSecret {
                key: key.clone(),
                name,
                adders,
                locations,
                labels,
            },
        );
    }
    out
}

fn resolve_buckets(
    d: &mut Diagnostics,
    raw: &RawConfig,
    ix: &interp::Interp,
    region: &str,
) -> BTreeMap<String, BucketConfig> {
    let mut out = BTreeMap::new();
    let mut names = std::collections::BTreeSet::new();
    for (key, b) in &raw.buckets {
        let path = format!("buckets.{key}");
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            d.error(
                path.clone(),
                "bucket keys may contain only letters, digits, `_` and `-`",
            );
        }
        let name = match ix.apply(&b.name) {
            Ok(n) => n,
            Err(e) => {
                d.error(format!("{path}.name"), e);
                continue;
            }
        };
        if let Err(e) = validate::bucket_name(&name) {
            d.error(format!("{path}.name"), e);
        } else if !names.insert(name.clone()) {
            d.error(
                format!("{path}.name"),
                format!("bucket `{name}` is declared twice"),
            );
        }
        let location = b.location.clone().unwrap_or_else(|| region.to_string());
        if let Err(e) = validate::bucket_location(&location) {
            d.error(format!("{path}.location"), e);
        }
        if let Some(sc) = &b.storage_class
            && !["STANDARD", "NEARLINE", "COLDLINE", "ARCHIVE"].contains(&sc.as_str())
        {
            d.error(
                format!("{path}.storage_class"),
                "must be STANDARD, NEARLINE, COLDLINE or ARCHIVE",
            );
        }
        let delete_after_days = match b.delete_after_days {
            None => None,
            Some(n) if (1..=36500).contains(&n) => Some(n as u32),
            Some(n) => {
                d.error(
                    format!("{path}.delete_after_days"),
                    format!("must be between 1 and 36500 (got {n})"),
                );
                None
            }
        };
        let labels = b.labels.clone().unwrap_or_default();
        for (k, v) in &labels {
            if let Err(e) = validate::label(k, v) {
                d.error(format!("{path}.labels.{k}"), e);
            }
        }
        out.insert(
            key.clone(),
            BucketConfig {
                key: key.clone(),
                name,
                location,
                storage_class: b.storage_class.clone(),
                versioning: b.versioning,
                delete_after_days,
                labels,
            },
        );
    }
    out
}

fn resolve_retry(d: &mut Diagnostics, raw: Option<&schema::RawRetry>) -> crate::retry::RetryConfig {
    let mut cfg = crate::retry::RetryConfig::default();
    let Some(r) = raw else { return cfg };
    if let Some(a) = r.attempts {
        if (1..=20).contains(&a) {
            cfg.attempts = a as u32;
        } else {
            d.error(
                "retry.attempts",
                format!("must be between 1 and 20 (got {a})"),
            );
        }
    }
    let dur =
        |d: &mut Diagnostics, v: &Option<String>, path: &str| -> Option<std::time::Duration> {
            let s = v.as_ref()?;
            match humantime::parse_duration(s) {
                Ok(x) if x <= std::time::Duration::from_secs(3600) => Some(x),
                Ok(_) => {
                    d.error(path, "must be at most 1h");
                    None
                }
                Err(e) => {
                    d.error(
                        path,
                        format!("invalid duration `{s}`: {e} (examples: 5s, 1m)"),
                    );
                    None
                }
            }
        };
    if let Some(x) = dur(d, &r.delay, "retry.delay") {
        cfg.delay = x;
    }
    if let Some(x) = dur(d, &r.max_delay, "retry.max_delay") {
        cfg.max_delay = x;
    }
    if cfg.max_delay < cfg.delay {
        d.error("retry.max_delay", "must not be shorter than retry.delay");
    }
    cfg
}

fn required(
    d: &mut Diagnostics,
    v: Option<(String, String)>,
    path: &str,
    f: fn(&str) -> std::result::Result<(), String>,
) -> String {
    match v {
        None => {
            d.error(path, "is required");
            String::new()
        }
        Some((val, p)) => {
            if let Err(e) = f(&val) {
                d.error(p, e);
            }
            val
        }
    }
}

struct BuildProvider {
    create: bool,
    project: String,
    region: String,
    artifact_repository: Option<(String, String)>,
    artifact_location: Option<(String, String)>,
    source_bucket: Option<(String, String)>,
    build_sa: Option<(String, String)>,
}

fn resolve_artifact(
    d: &mut Diagnostics,
    cfg: &LoadedConfig,
    stage: &str,
    overrides: &Overrides,
    base: &RawService,
    sv: &RawService,
    bp: BuildProvider,
) -> Option<Artifact> {
    struct Layer<'a> {
        prefix: String,
        image: Option<&'a String>,
        source: Option<&'a String>,
        dockerfile: Option<&'a String>,
        builder: Option<&'a String>,
        rebuild: Option<&'a String>,
    }
    let layers = [
        Layer {
            prefix: "--image".into(),
            image: overrides.image.as_ref(),
            source: None,
            dockerfile: None,
            builder: None,
            rebuild: None,
        },
        Layer {
            prefix: format!("stages.{stage}.service"),
            image: sv.image.as_ref(),
            source: sv.source.as_ref(),
            dockerfile: sv.dockerfile.as_ref(),
            builder: sv.builder.as_ref(),
            rebuild: sv.rebuild.as_ref(),
        },
        Layer {
            prefix: "service".into(),
            image: base.image.as_ref(),
            source: base.source.as_ref(),
            dockerfile: base.dockerfile.as_ref(),
            builder: base.builder.as_ref(),
            rebuild: base.rebuild.as_ref(),
        },
    ];
    // Same-layer conflicts are always errors.
    for l in &layers {
        if l.image.is_some()
            && (l.source.is_some() || l.dockerfile.is_some() || l.builder.is_some())
        {
            d.error(
                format!("{}.image", l.prefix),
                "`image` cannot be combined with `source`/`dockerfile`/`builder` in the same block; use one deployment mode",
            );
            return None;
        }
    }
    let Some(top) = layers.iter().position(|l| {
        l.image.is_some() || l.source.is_some() || l.dockerfile.is_some() || l.builder.is_some()
    }) else {
        d.error(
            "service",
            "set either `service.image` (deploy an existing image) or `service.source` (build from source with a Dockerfile or buildpacks)",
        );
        return None;
    };

    if let Some(image) = layers[top].image {
        let path = if top == 0 {
            "--image".to_string()
        } else {
            format!("{}.image", layers[top].prefix)
        };
        return match ImageRef::parse(image) {
            Ok(parsed) => {
                if parsed.is_mutable() {
                    d.warn(
                        path,
                        "mutable tag; runway resolves it to an immutable digest at deploy time when the registry allows",
                    );
                }
                Some(Artifact::Image {
                    reference: image.clone(),
                    parsed,
                })
            }
            Err(e) => {
                d.error(path, e);
                None
            }
        };
    }

    // Build mode: source/dockerfile merge across the remaining layers.
    let lower = &layers[top..];
    let source = lower.iter().find_map(|l| {
        l.source
            .map(|s| (s.clone(), format!("{}.source", l.prefix)))
    });
    let dockerfile = lower.iter().find_map(|l| {
        l.dockerfile
            .map(|s| (s.clone(), format!("{}.dockerfile", l.prefix)))
    });
    let Some((source, source_path)) = source else {
        d.error(
            "service.source",
            "is required when building from source (for example `source: .`)",
        );
        return None;
    };
    let builder = lower.iter().find_map(|l| {
        l.builder
            .map(|s| (s.clone(), format!("{}.builder", l.prefix)))
    });

    let context_dir = normalize_path(&cfg.dir.join(&source));
    let mut ok = true;
    if !context_dir.is_dir() {
        d.error(
            &source_path,
            format!(
                "build context `{}` is not a directory",
                context_dir.display()
            ),
        );
        ok = false;
    }
    // Strategy: an explicit `dockerfile` or `builder` wins; otherwise a
    // `Dockerfile` in the context is used, and buildpacks when there is none.
    let strategy = match (dockerfile, builder) {
        (Some((_, dpath)), Some(_)) => {
            d.error(dpath, "`dockerfile` and `builder` are mutually exclusive");
            return None;
        }
        (None, Some((b, bpath))) => {
            if let Err(e) = ImageRef::parse(&b) {
                d.error(bpath, e);
                ok = false;
            }
            BuildStrategy::Buildpacks { builder: b }
        }
        (explicit, None) => {
            let auto = explicit.is_none();
            let (dockerfile, dockerfile_path) = explicit
                .unwrap_or_else(|| (DEFAULT_DOCKERFILE.to_string(), "service.dockerfile".into()));
            let df = Path::new(&dockerfile);
            if df.is_absolute() || df.components().any(|c| matches!(c, Component::ParentDir)) {
                d.error(
                    &dockerfile_path,
                    "must be a relative path inside the build context (it is uploaded with the source)",
                );
                ok = false;
                BuildStrategy::Dockerfile { path: dockerfile }
            } else if ok && !context_dir.join(df).is_file() {
                if auto {
                    BuildStrategy::Buildpacks {
                        builder: DEFAULT_BUILDER.to_string(),
                    }
                } else {
                    d.error(
                        &dockerfile_path,
                        format!(
                            "`{}` not found in build context `{}`",
                            dockerfile,
                            context_dir.display()
                        ),
                    );
                    ok = false;
                    BuildStrategy::Dockerfile { path: dockerfile }
                }
            } else {
                BuildStrategy::Dockerfile {
                    path: df
                        .components()
                        .filter_map(|c| match c {
                            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("/"),
                }
            }
        }
    };

    let rebuild_always = match lower.iter().find_map(|l| {
        l.rebuild
            .map(|r| (r.clone(), format!("{}.rebuild", l.prefix)))
    }) {
        None => false,
        Some((r, path)) => match r.as_str() {
            "on-change" => false,
            "always" => true,
            _ => {
                d.error(path, format!("`{r}` must be `on-change` or `always`"));
                false
            }
        },
    };
    // With `create_build_resources`, runway picks the names itself.
    let defaults = |field: &str| -> Option<String> {
        if !bp.create {
            return None;
        }
        Some(match field {
            "artifact_repository" => DEFAULT_ARTIFACT_REPOSITORY.to_string(),
            "source_bucket" => format!("{}-runway-sources", bp.project),
            _ => format!("runway-build@{}.iam.gserviceaccount.com", bp.project),
        })
    };
    let need = |d: &mut Diagnostics, v: &Option<(String, String)>, field: &str| -> Option<String> {
        match v {
            Some((val, _)) => Some(val.clone()),
            None if bp.create => defaults(field),
            None => {
                d.error(
                    format!("provider.{field}"),
                    "is required for source builds (or set `provider.create_build_resources: true` to let runway create it with a default name, or deploy an existing image with `service.image`)",
                );
                None
            }
        }
    };
    let repo = need(d, &bp.artifact_repository, "artifact_repository");
    let bucket = need(d, &bp.source_bucket, "source_bucket");
    let build_sa = need(d, &bp.build_sa, "build_service_account");
    let location = bp
        .artifact_location
        .map(|(v, _)| v)
        .unwrap_or(bp.region.clone());
    match (repo, bucket, build_sa, ok) {
        (Some(artifact_repository), Some(source_bucket), Some(build_service_account), true) => {
            let excluded = config_relative_to(&cfg.path, &context_dir)
                .into_iter()
                .collect();
            Some(Artifact::Build(BuildConfig {
                excluded,
                context_dir,
                strategy,
                rebuild_always,
                artifact_location: location,
                artifact_repository,
                source_bucket,
                build_service_account,
                create_resources: bp.create,
            }))
        }
        _ => None,
    }
}

/// The config file's path relative to the build context, if it lives inside it.
fn config_relative_to(config: &Path, context: &Path) -> Option<String> {
    let config = std::fs::canonicalize(config).ok()?;
    let context = std::fs::canonicalize(context).ok()?;
    let rel = config.strip_prefix(&context).ok()?;
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Removes `.` components (`./.` -> `.`, `a/./b` -> `a/b`).
fn normalize_path(p: &Path) -> PathBuf {
    let out: PathBuf = p
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Merges `env`/`secrets` maps key by key. A `null` in the stage removes the key.
fn merge_map<V: Clone>(
    d: &mut Diagnostics,
    stage: &str,
    base: &Option<BTreeMap<String, Option<V>>>,
    over: &Option<BTreeMap<String, Option<V>>>,
    field: &str,
) -> BTreeMap<String, (V, String)> {
    let mut out = BTreeMap::new();
    if let Some(b) = base {
        for (k, v) in b {
            match v {
                Some(v) => {
                    out.insert(k.clone(), (v.clone(), format!("service.{field}.{k}")));
                }
                None => d.error(
                    format!("service.{field}.{k}"),
                    "value must not be null (null is only meaningful in stage overrides, to remove an inherited key)",
                ),
            }
        }
    }
    if let Some(o) = over {
        for (k, v) in o {
            match v {
                Some(v) => {
                    out.insert(
                        k.clone(),
                        (v.clone(), format!("stages.{stage}.service.{field}.{k}")),
                    );
                }
                None => {
                    out.remove(k);
                }
            }
        }
    }
    out
}

/// Validates every defined stage. Returns per-stage results.
pub fn resolve_all(
    cfg: &LoadedConfig,
    overrides: &Overrides,
) -> Vec<(String, std::result::Result<Resolved, Diagnostics>)> {
    let mut stages: BTreeSet<String> = cfg.raw.stages.keys().cloned().collect();
    if stages.is_empty() {
        // Still report top-level problems.
        stages.insert("default".into());
    }
    stages
        .into_iter()
        .map(|s| {
            let r = resolve(cfg, &s, overrides);
            (s, r)
        })
        .collect()
}

/// Loads and resolves in one step, turning diagnostics into an error.
pub fn load_and_resolve(
    path: &Path,
    stage: &str,
    overrides: &Overrides,
) -> Result<(LoadedConfig, Resolved)> {
    let cfg = load(path)?;
    let resolved = resolve(&cfg, stage, overrides).map_err(|d| {
        d.into_error(&format!(
            "{} is invalid for stage `{stage}`",
            cfg.path.display()
        ))
    })?;
    Ok((cfg, resolved))
}

#[cfg(test)]
mod tests;
