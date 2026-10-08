//! Raw, strictly-parsed `runway.yaml` schema (version 1).
//!
//! Every struct uses `deny_unknown_fields`, so typos are reported instead of
//! silently ignored. All service fields are optional here: the same type is used
//! for `service`, `defaults`, each `services.NAME` and their `stages.<name>.…`
//! overrides. Jobs use [`RawJob`].
//! Defaults and validation are applied in [`crate::config::resolve`].

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    /// Schema version. Only `1` is supported.
    pub version: i64,
    /// Application name; used to derive resource names.
    pub app: String,
    pub provider: RawProvider,
    /// The main service, named `<app>-<stage>`.
    pub service: Option<RawService>,
    /// Settings every service and job inherits (jobs only those they have).
    pub defaults: Option<RawService>,
    /// More services, by name: `<app>-<name>-<stage>`.
    #[serde(default)]
    pub services: BTreeMap<String, Option<RawService>>,
    /// Cloud Run jobs, by name: `<app>-<name>-<stage>`.
    #[serde(default)]
    pub jobs: BTreeMap<String, Option<RawJob>>,
    /// Cloud Scheduler jobs that run a job or call a service, by name.
    #[serde(default)]
    pub schedules: BTreeMap<String, Option<RawSchedule>>,
    /// The account Cloud Scheduler uses to call its targets.
    pub scheduler: Option<RawScheduler>,
    /// Release images (`deploy --tag`, `--tag-rc`): where they are published.
    pub release: Option<RawRelease>,
    /// How custom domains are served (a stage block replaces it).
    pub domains: Option<RawDomains>,
    #[serde(default)]
    pub stages: BTreeMap<String, Option<RawStage>>,
    /// Per-step retry policy for deployments.
    pub retry: Option<RawRetry>,
    /// User variables, usable as `${vars.NAME}` in most string fields.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Cloud Storage buckets runway creates and keeps configured, by key
    /// (referenced as `${buckets.KEY}`).
    #[serde(default)]
    pub buckets: BTreeMap<String, RawBucket>,
    /// Secret Manager secrets runway creates (empty: people add the values),
    /// by key (referenced as `${secrets.KEY}`). Never deleted by runway.
    #[serde(default)]
    pub secrets: BTreeMap<String, RawManagedSecret>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawManagedSecret {
    /// Secret ID; defaults to the key. Supports `${app}`, `${stage}`, etc.
    pub name: Option<String>,
    /// Principals allowed to add values (`roles/secretmanager.secretVersionAdder`),
    /// e.g. `group:devops@example.com`.
    pub adders: Option<Vec<String>>,
    /// Replica locations (user-managed replication). Default: `[provider.region]`.
    /// `[]` selects automatic replication.
    pub locations: Option<Vec<String>>,
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBucket {
    /// Globally unique bucket name (supports `${project}` etc.).
    pub name: String,
    /// Location; defaults to `provider.region`.
    pub location: Option<String>,
    /// Default storage class, e.g. `STANDARD`.
    pub storage_class: Option<String>,
    pub versioning: Option<bool>,
    /// Delete objects older than this many days (lifecycle rule).
    pub delete_after_days: Option<i64>,
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRetry {
    /// Total attempts per step (1 disables retries).
    pub attempts: Option<i64>,
    /// Initial delay between attempts (doubles each time), e.g. `5s`.
    pub delay: Option<String>,
    /// Upper bound for the delay, e.g. `60s`.
    pub max_delay: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProvider {
    pub project: Option<String>,
    pub region: Option<String>,
    pub artifact_repository: Option<String>,
    /// Location of the Artifact Registry repository. Defaults to `region`.
    pub artifact_location: Option<String>,
    pub source_bucket: Option<String>,
    pub build_service_account: Option<String>,
    /// Enable the APIs the configuration needs (plus `apis`) when missing.
    pub enable_apis: Option<bool>,
    /// Extra APIs to enable/check, e.g. `telemetry.googleapis.com`.
    pub apis: Option<Vec<String>>,
    /// Create the Artifact Registry repository, source bucket and build
    /// service account (with its roles) when missing.
    pub create_build_resources: Option<bool>,
    /// Service account to impersonate for every API call (`SA` or
    /// `DELEGATE,...,SA`); `--impersonate-service-account` overrides it.
    pub impersonate_service_account: Option<String>,
    /// Resource Manager tags bound to the deployment project
    /// (`ORG_OR_PROJECT/key: value`), for example the tag an organization
    /// policy requires before `ingress: all` is allowed. Bound before anything
    /// else is created; a stage map replaces the inherited one.
    pub tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStage {
    /// Stage values override top-level `vars`.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub provider: RawProvider,
    pub service: Option<RawService>,
    pub defaults: Option<RawService>,
    /// Field overrides of named services; `null` removes one from the stage.
    #[serde(default)]
    pub services: BTreeMap<String, Option<RawService>>,
    /// Field overrides of jobs; `null` removes one from the stage.
    #[serde(default)]
    pub jobs: BTreeMap<String, Option<RawJob>>,
    /// A schedule here replaces the inherited one; `null` removes it.
    #[serde(default)]
    pub schedules: BTreeMap<String, Option<RawSchedule>>,
    pub scheduler: Option<RawScheduler>,
    /// `flag` maps the stage to `deploy --tag` or `--tag-rc`; `repository`
    /// replaces the global one.
    pub release: Option<RawRelease>,
    pub domains: Option<RawDomains>,
}

/// How a stage serves its services' custom domains.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDomains {
    /// `load-balancer` (default), `existing-load-balancer` or `domain-mapping`.
    pub mode: Option<String>,
    /// Cloud DNS zone for the records (else they are printed).
    pub dns: Option<RawDnsZone>,
    /// The load balancer runway adds its routes to (`existing-load-balancer`).
    pub load_balancer: Option<RawExistingLoadBalancer>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDnsZone {
    /// Managed zone name (not its DNS name).
    pub zone: String,
    /// Default: `provider.project`.
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExistingLoadBalancer {
    /// URL map of the load balancer, in the deployment project.
    pub url_map: String,
    /// Certificate map the load balancer's HTTPS proxy uses: runway adds
    /// certificates for its hosts. Without it, certificates are not runway's.
    pub certificate_map: Option<String>,
    /// The load balancer's IP address, for DNS records.
    pub address: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRelease {
    /// In a stage: `tag` (deployed by `deploy --tag`) or `tag-rc`.
    pub flag: Option<String>,
    /// In a `tag` stage: the `tag-rc` stage whose candidates it releases.
    pub from: Option<String>,
    /// Where released images are published (copied, same digest).
    pub repository: Option<RawReleaseRepository>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawReleaseRepository {
    /// Default: `provider.project`.
    pub project: Option<String>,
    /// Default: the build repository's location (`provider.artifact_location`,
    /// else `provider.region`).
    pub location: Option<String>,
    pub repository: String,
}

/// A Cloud Run job: the runtime settings of a service that apply to jobs,
/// plus how its executions run.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawJob {
    pub source: Option<String>,
    pub dockerfile: Option<String>,
    pub builder: Option<String>,
    pub rebuild: Option<String>,
    pub image: Option<String>,
    /// Entrypoint and arguments of the container (default: the image's).
    pub command: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
    pub cpu: Option<Scalar>,
    pub memory: Option<String>,
    /// Timeout of each task, in seconds (default 600, up to 168 hours).
    pub timeout_seconds: Option<i64>,
    /// Tasks per execution (default 1).
    pub tasks: Option<i64>,
    /// Tasks running at once (default: as many as possible).
    pub parallelism: Option<i64>,
    /// Retries of a failed task (default 3).
    pub max_retries: Option<i64>,
    pub service_account: Option<String>,
    pub env: Option<BTreeMap<String, Option<Scalar>>>,
    pub secrets: Option<BTreeMap<String, Option<RawSecretRef>>>,
    pub volumes: Option<BTreeMap<String, Option<RawVolume>>>,
    pub identity: Option<RawIdentity>,
    pub execution_environment: Option<String>,
    pub sandbox: Option<bool>,
    pub vpc: Option<RawVpc>,
    pub cloud_sql: Option<Vec<String>>,
}

impl RawJob {
    /// The service fields a job has (the rest stay unset).
    pub fn as_service(&self) -> RawService {
        RawService {
            source: self.source.clone(),
            dockerfile: self.dockerfile.clone(),
            builder: self.builder.clone(),
            rebuild: self.rebuild.clone(),
            image: self.image.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            cpu: self.cpu.clone(),
            memory: self.memory.clone(),
            timeout_seconds: self.timeout_seconds,
            service_account: self.service_account.clone(),
            env: self.env.clone(),
            secrets: self.secrets.clone(),
            volumes: self.volumes.clone(),
            identity: self.identity.clone(),
            execution_environment: self.execution_environment.clone(),
            sandbox: self.sandbox,
            vpc: self.vpc.clone(),
            cloud_sql: self.cloud_sql.clone(),
            ..Default::default()
        }
    }
}

impl RawService {
    /// What a job inherits from `defaults`: the fields jobs have, except the
    /// timeout (a request timeout is not a task timeout).
    pub fn for_job(&self) -> RawService {
        RawJob {
            source: self.source.clone(),
            dockerfile: self.dockerfile.clone(),
            builder: self.builder.clone(),
            rebuild: self.rebuild.clone(),
            image: self.image.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            cpu: self.cpu.clone(),
            memory: self.memory.clone(),
            service_account: self.service_account.clone(),
            env: self.env.clone(),
            secrets: self.secrets.clone(),
            volumes: self.volumes.clone(),
            identity: self.identity.clone(),
            execution_environment: self.execution_environment.clone(),
            sandbox: self.sandbox,
            vpc: self.vpc.clone(),
            cloud_sql: self.cloud_sql.clone(),
            ..Default::default()
        }
        .as_service()
    }
}

/// A Cloud Scheduler job. Exactly one target: `job` or `service`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSchedule {
    /// Cron expression, e.g. `0 3 * * *`.
    pub schedule: String,
    /// IANA time zone (default `Etc/UTC`).
    pub time_zone: Option<String>,
    /// Runs this job (a key of `jobs`).
    pub job: Option<String>,
    /// Calls this service (a key of `services`, or the app name for the main
    /// service) with an ID token.
    pub service: Option<String>,
    /// Path on the service (default `/`).
    pub path: Option<String>,
    /// HTTP method for a service (default `POST`).
    pub method: Option<String>,
    /// Request body for a service.
    pub body: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    /// Retries of a failed attempt (default 0, up to 5).
    pub retries: Option<i64>,
    /// How long an attempt may take, in seconds (15-1800, default 180).
    pub attempt_deadline_seconds: Option<i64>,
    /// Created paused.
    pub paused: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScheduler {
    /// The account Cloud Scheduler calls targets with. Default:
    /// `<app>-<stage>-sched@PROJECT.iam.gserviceaccount.com`, created by runway.
    pub service_account: Option<String>,
    /// Create the account (default: true for the default account).
    pub create: Option<bool>,
    /// Cloud Scheduler region (default: `provider.region`).
    pub region: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawService {
    pub source: Option<String>,
    /// Entrypoint and arguments of the app container (default: the image's).
    pub command: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
    pub dockerfile: Option<String>,
    /// Buildpacks builder image. Without a Dockerfile, buildpacks are used
    /// automatically with `gcr.io/buildpacks/builder:latest`.
    pub builder: Option<String>,
    /// `on-change` (default: rebuild when the source or a base image changes)
    /// or `always` (rebuild on every deploy, like `--force-build`).
    pub rebuild: Option<String>,
    pub image: Option<String>,
    pub port: Option<i64>,
    pub cpu: Option<Scalar>,
    pub memory: Option<String>,
    pub timeout_seconds: Option<i64>,
    pub concurrency: Option<i64>,
    pub min_instances: Option<i64>,
    pub max_instances: Option<i64>,
    pub public: Option<bool>,
    /// `all` (default), `internal` or `internal-and-cloud-load-balancing`.
    pub ingress: Option<String>,
    /// HTTP startup (and liveness) probes. A stage block replaces the inherited one.
    pub health_check: Option<RawHealthCheck>,
    /// First deploy only: create the service with a restricted ingress (and
    /// optionally a placeholder image), bind `tags`, wait until they are
    /// effective, then apply the configured ingress.
    pub bootstrap: Option<RawBootstrap>,
    /// Google-built OpenTelemetry Collector as a sidecar.
    pub otel_collector: Option<RawOtelCollector>,
    /// Extra containers next to the application, by container name. In
    /// stage overrides a `null` value removes an inherited sidecar.
    pub sidecars: Option<BTreeMap<String, Option<RawSidecar>>>,
    pub service_account: Option<String>,
    /// Plain environment variables. In stage overrides a `null` value removes
    /// an inherited variable.
    pub env: Option<BTreeMap<String, Option<Scalar>>>,
    /// Secret Manager references. In stage overrides a `null` value removes an
    /// inherited reference.
    pub secrets: Option<BTreeMap<String, Option<RawSecretRef>>>,
    /// Resource Manager tags bound to the service: `ORG_OR_PROJECT/key: value`.
    /// In stage overrides a `null` value removes an inherited tag.
    pub tags: Option<BTreeMap<String, Option<String>>>,
    /// Cloud Storage volumes mounted into the container, by volume name.
    /// In stage overrides a `null` value removes an inherited volume.
    pub volumes: Option<BTreeMap<String, Option<RawVolume>>>,
    /// Identity-Aware Proxy. A stage block replaces the inherited one.
    pub iap: Option<RawIap>,
    /// Runtime service account management. A stage block replaces the inherited one.
    pub identity: Option<RawIdentity>,
    /// `request-based` (default: CPU only while handling requests) or
    /// `instance-based` (CPU always allocated, billed for the instance's
    /// whole lifetime).
    pub billing: Option<String>,
    /// Extra CPU while instances start (and for 10 seconds after).
    pub startup_cpu_boost: Option<bool>,
    /// `gen1` or `gen2`. Default: Cloud Run chooses from the features used.
    pub execution_environment: Option<String>,
    /// Cloud Run sandboxes (preview): the app can run untrusted code with the
    /// `sandbox` command. Implies gen2.
    pub sandbox: Option<bool>,
    /// Direct VPC egress. A stage block replaces the inherited one.
    pub vpc: Option<RawVpc>,
    /// Cloud SQL instances (`PROJECT:REGION:INSTANCE`, or an instance name in
    /// the deployment project and region), reachable under `/cloudsql`.
    pub cloud_sql: Option<Vec<String>>,
    /// Extra audiences accepted in ID tokens (besides the `run.app` URL).
    pub custom_audiences: Option<Vec<String>>,
    /// Custom domains: `shop.example.com`, `example.com/api` (a path, with a
    /// load balancer), or `shop.cloud.run` (a Cloud Run custom URL).
    pub domains: Option<Vec<String>>,
    /// Preview URLs on a domain: `*.preview.example.com` routes
    /// `<tag>.preview.example.com` to each preview (load balancer modes).
    pub preview_domain: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVpc {
    /// Network name, or `projects/HOST/global/networks/NAME` (Shared VPC).
    pub network: String,
    /// Subnet name, or `projects/HOST/regions/REGION/subnetworks/NAME`.
    pub subnet: String,
    /// `private-ranges-only` (default) or `all-traffic`.
    pub egress: Option<String>,
    /// Network tags on the revision, for firewall rules.
    pub network_tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBootstrap {
    /// Optional placeholder image (for example
    /// `us-docker.pkg.dev/cloudrun/container/hello`). Default: the real app.
    pub image: Option<String>,
    /// Ingress used until the tags are effective (default `internal`).
    pub ingress: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSidecar {
    pub image: String,
    /// Default 1.
    pub cpu: Option<Scalar>,
    /// Default 512Mi.
    pub memory: Option<String>,
    /// Replaces the image entrypoint.
    pub command: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, Scalar>>,
    /// Secret Manager references exposed as environment variables.
    pub secrets: Option<BTreeMap<String, RawSecretRef>>,
    /// Startup check: HTTP `GET path` on `port`, or a TCP connection to
    /// `port` when `path` is omitted.
    pub health_check: Option<RawSidecarCheck>,
    /// Start (and pass the health check) before the application container.
    /// Default true.
    pub start_before_app: Option<bool>,
    /// Service volumes mounted in this container: volume name -> mount path.
    pub volumes: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSidecarCheck {
    pub port: i64,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOtelCollector {
    /// Default true when the block is present.
    pub enabled: Option<bool>,
    /// Collector version (tag of the Google-built image).
    pub version: Option<String>,
    /// Full image reference (overrides `version`).
    pub image: Option<String>,
    pub cpu: Option<Scalar>,
    pub memory: Option<String>,
    /// Collector configuration (YAML). Default: OTLP on localhost, exported
    /// to Cloud Trace, Cloud Logging and Managed Service for Prometheus.
    pub config: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHealthCheck {
    /// HTTP path probed on the container port, e.g. `/healthz`.
    pub path: String,
    pub startup: Option<RawProbe>,
    /// `false` disables the liveness probe; a map tunes it.
    pub liveness: Option<RawLiveness>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawLiveness {
    Enabled(bool),
    Settings(RawProbe),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProbe {
    pub initial_delay_seconds: Option<i64>,
    pub period_seconds: Option<i64>,
    pub timeout_seconds: Option<i64>,
    pub failure_threshold: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVolume {
    pub bucket: String,
    pub mount_path: String,
    pub read_only: Option<bool>,
    pub mount_options: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIap {
    pub enabled: Option<bool>,
    /// Principals granted `roles/iap.httpsResourceAccessor`, e.g. `group:finops@example.com`.
    pub members: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIdentity {
    /// Create `service.service_account` if it does not exist.
    pub create: Option<bool>,
    pub display_name: Option<String>,
    /// Roles granted to the runtime service account.
    pub roles: Option<Vec<RawRoleBinding>>,
}

/// A role granted to the runtime service account on exactly one resource.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRoleBinding {
    pub role: String,
    pub project: Option<String>,
    pub bucket: Option<String>,
    /// BigQuery dataset, `PROJECT.DATASET` or `PROJECT:DATASET`.
    pub dataset: Option<String>,
    /// Secret ID (in the provider project) or `projects/P/secrets/S`.
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSecretRef {
    /// Secret ID (`database-url`) or full name (`projects/123/secrets/database-url`).
    pub secret: String,
    /// Version number or `latest`. Omitted: environment variables get the
    /// newest enabled version, pinned at deploy time (a new value rolls out
    /// with the next deploy); files use `latest` (read live).
    pub version: Option<Scalar>,
    /// Mount as a file at this absolute path instead of an environment
    /// variable. Each secret file needs its own directory.
    pub path: Option<String>,
}

/// A YAML scalar accepted where users commonly write numbers or booleans for
/// string-typed values (for example `cpu: 1` or `DEBUG: true`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

impl Scalar {
    pub fn to_string_value(&self) -> String {
        match self {
            Scalar::Bool(b) => b.to_string(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => f.to_string(),
            Scalar::String(s) => s.clone(),
        }
    }
}
