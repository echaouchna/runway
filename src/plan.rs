//! Desired-state model, diffing and plan rendering.
//!
//! This module is independent of the Google SDK: the desired state is a
//! [`ServiceSpec`], and both desired and observed state are compared through a
//! flat, normalized `field -> value` map so that equivalent spellings (for
//! example `1000m` and `1`, or `1Gi` and `1024Mi`) do not show up as changes.

use crate::config::validate::{canonical_cpu, parse_cpu_millis, parse_memory_bytes};
use crate::config::{Deployment, SecretRef};
use serde::Serialize;
use std::collections::BTreeMap;

/// Desired configuration of the Cloud Run service (everything runway manages).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServiceSpec {
    pub image: String,
    pub port: u16,
    pub cpu: String,
    pub memory: String,
    pub timeout_seconds: u32,
    pub concurrency: u32,
    pub min_instances: u32,
    pub max_instances: u32,
    pub service_account: String,
    /// `all`, `internal` or `internal-and-cloud-load-balancing`.
    pub ingress: String,
    pub health_check: Option<crate::config::HealthCheck>,
    /// OpenTelemetry Collector sidecar.
    pub otel_collector: Option<crate::config::OtelCollector>,
    /// Other sidecars, by container name.
    pub sidecars: BTreeMap<String, crate::config::SidecarConfig>,
    pub env: BTreeMap<String, String>,
    pub secrets: BTreeMap<String, SecretRef>,
    pub labels: BTreeMap<String, String>,
    /// Cloud Storage volumes by name.
    pub volumes: BTreeMap<String, crate::config::VolumeConfig>,
    /// Identity-Aware Proxy on the service.
    pub iap_enabled: bool,
    /// `request-based` or `instance-based`.
    pub billing: String,
    pub startup_cpu_boost: bool,
    /// `gen1` or `gen2`; `None`: Cloud Run chooses.
    pub execution_environment: Option<String>,
    /// The app container may launch Cloud Run sandboxes.
    pub sandbox: bool,
    /// Entrypoint and arguments of the app container; empty: the image's.
    pub command: Vec<String>,
    pub args: Vec<String>,
    pub vpc: Option<crate::config::VpcConfig>,
    /// Cloud SQL connection names.
    pub cloud_sql: Vec<String>,
    /// Service-level: extra accepted ID token audiences.
    pub custom_audiences: Vec<String>,
    /// Service-level annotations (not diffed).
    pub annotations: BTreeMap<String, String>,
    /// Revision-template annotations (not diffed); used to force a new revision.
    pub revision_annotations: BTreeMap<String, String>,
    /// What happens to traffic, and the resulting split (see [`ServiceSpec::for_service`]).
    pub traffic: TrafficSpec,
}

/// Requested traffic mode and the split computed from the live service.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrafficSpec {
    pub mode: crate::traffic::Mode,
    pub entries: Vec<crate::traffic::Entry>,
    /// The target already behind the mode's URL, when its revision runs
    /// this configuration: it keeps serving, no revision is created.
    pub serve: Option<crate::traffic::Target>,
}

impl Default for TrafficSpec {
    fn default() -> Self {
        let mode = crate::traffic::Mode::Full;
        Self {
            entries: crate::traffic::plan(None, &mode),
            mode,
            serve: None,
        }
    }
}

impl ServiceSpec {
    pub fn from_deployment(
        d: &Deployment,
        image: &str,
        annotations: BTreeMap<String, String>,
    ) -> Self {
        let s = &d.service;
        Self {
            image: image.to_string(),
            port: s.port,
            cpu: s.cpu.clone(),
            memory: s.memory.clone(),
            timeout_seconds: s.timeout_seconds,
            concurrency: s.concurrency,
            min_instances: s.min_instances,
            max_instances: s.max_instances,
            service_account: s.service_account.clone(),
            ingress: s.ingress.clone(),
            health_check: s.health_check.clone(),
            otel_collector: s.otel_collector.clone(),
            sidecars: s.sidecars.clone(),
            env: s.env.clone(),
            secrets: s.secrets.clone(),
            labels: d.labels(),
            volumes: s.volumes.clone(),
            iap_enabled: s.iap.enabled,
            billing: s.billing.clone(),
            startup_cpu_boost: s.startup_cpu_boost,
            execution_environment: s.execution_environment.clone(),
            sandbox: s.sandbox,
            command: s.command.clone(),
            args: s.args.clone(),
            vpc: s.vpc.clone(),
            cloud_sql: s.cloud_sql.clone(),
            custom_audiences: s.custom_audiences.clone(),
            annotations,
            revision_annotations: BTreeMap::new(),
            traffic: TrafficSpec::default(),
        }
    }

    /// Sets the traffic mode; the split is recomputed against the live
    /// service by [`ServiceSpec::with_current_traffic`].
    pub fn with_traffic_mode(mut self, mode: crate::traffic::Mode) -> Self {
        self.traffic = TrafficSpec {
            entries: crate::traffic::plan(None, &mode),
            mode,
            serve: None,
        };
        self
    }

    /// The split for this mode given the live traffic (`None`: new service).
    pub fn with_current_traffic(&self, current: Option<&crate::traffic::Current>) -> Self {
        let mut s = self.clone();
        s.traffic.entries =
            crate::traffic::plan_with(current, &s.traffic.mode, s.traffic.serve.clone());
        s
    }

    /// Normalized flat representation used for diffs.
    pub fn flatten(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("image".into(), self.image.clone());
        m.insert("port".into(), self.port.to_string());
        m.insert("cpu".into(), normalize_cpu(&self.cpu));
        m.insert("memory".into(), normalize_memory(&self.memory));
        m.insert("timeout_seconds".into(), self.timeout_seconds.to_string());
        m.insert("concurrency".into(), self.concurrency.to_string());
        m.insert("min_instances".into(), self.min_instances.to_string());
        m.insert("max_instances".into(), self.max_instances.to_string());
        m.insert("service_account".into(), self.service_account.clone());
        m.insert("ingress".into(), self.ingress.clone());
        m.extend(crate::traffic::flat(&self.traffic.entries));
        m.insert(
            "containers".into(),
            (1 + usize::from(self.otel_collector.is_some()) + self.sidecars.len()).to_string(),
        );
        for (name, sc) in &self.sidecars {
            m.insert(
                format!("sidecars.{name}"),
                sidecar_fingerprint(&SidecarView::from_config(sc)),
            );
        }
        if let Some(o) = &self.otel_collector {
            m.insert(
                format!("sidecars.{}", crate::config::OTEL_COLLECTOR_NAME),
                sidecar_display(&o.image, &o.cpu, &o.memory, &o.config),
            );
        }
        for (k, v) in &self.env {
            m.insert(format!("env.{k}"), v.clone());
        }
        for (k, v) in &self.secrets {
            match &v.path {
                None => m.insert(format!("secrets.{k}"), secret_display(v)),
                Some(p) => m.insert(format!("secret_files.{p}"), secret_display(v)),
            };
        }
        for (k, v) in &self.labels {
            m.insert(format!("labels.{k}"), v.clone());
        }
        // runway's service annotations (image reference, source hash, base
        // images, release): service-level, they never create a revision.
        for (k, v) in &self.annotations {
            m.insert(format!("annotations.{k}"), v.clone());
        }
        for (k, v) in &self.volumes {
            m.insert(format!("volumes.{k}"), volume_display(v));
        }
        if self.iap_enabled {
            m.insert("iap".into(), "enabled".into());
        }
        m.insert("billing".into(), self.billing.clone());
        if self.startup_cpu_boost {
            m.insert("startup_cpu_boost".into(), "enabled".into());
        }
        if self.sandbox {
            m.insert("sandbox".into(), "enabled".into());
        }
        if !self.command.is_empty() {
            m.insert("command".into(), args_display(&self.command));
        }
        if !self.args.is_empty() {
            m.insert("args".into(), args_display(&self.args));
        }
        if let Some(e) = &self.execution_environment {
            m.insert("execution_environment".into(), e.clone());
        }
        if let Some(v) = &self.vpc {
            m.insert("vpc".into(), vpc_display(v));
        }
        if !self.cloud_sql.is_empty() {
            m.insert("cloud_sql".into(), list_display(&self.cloud_sql));
        }
        if !self.custom_audiences.is_empty() {
            m.insert(
                "custom_audiences".into(),
                list_display(&self.custom_audiences),
            );
        }
        if let Some(hc) = &self.health_check {
            m.insert(
                "health_check.startup".into(),
                probe_display(&hc.path, &hc.startup),
            );
            if let Some(l) = &hc.liveness {
                m.insert("health_check.liveness".into(), probe_display(&hc.path, l));
            }
        }
        m
    }
}

/// Command-line words in order, quoted when they contain spaces.
pub fn args_display(words: &[String]) -> String {
    words
        .iter()
        .map(|w| match w.contains(char::is_whitespace) || w.is_empty() {
            true => format!("{w:?}"),
            false => w.clone(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `network N, subnet S, egress E[, tags a b]` (desired and observed), with
/// full resource names: in a Shared VPC, the host project is part of what
/// is compared.
pub fn vpc_display(v: &crate::config::VpcConfig) -> String {
    let name = |p: &str| {
        p.strip_prefix("https://www.googleapis.com/compute/v1/")
            .unwrap_or(p)
            .to_string()
    };
    let mut out = format!(
        "network {}, subnet {}, egress {}",
        name(&v.network),
        name(&v.subnet),
        v.egress
    );
    if !v.network_tags.is_empty() {
        let mut tags = v.network_tags.clone();
        tags.sort();
        out.push_str(&format!(", tags {}", tags.join(" ")));
    }
    out
}

/// Sorted, comma-separated: the order of these lists carries no meaning.
pub fn list_display(items: &[String]) -> String {
    let mut v = items.to_vec();
    v.sort();
    v.join(", ")
}

/// `IMAGE, cpu C, memory M, config <hash>` for the collector sidecar (desired and observed).
pub fn sidecar_display(image: &str, cpu: &str, memory: &str, config: &str) -> String {
    use sha2::Digest;
    let h = crate::build::package::hex(&sha2::Sha256::digest(config.as_bytes()));
    format!(
        "{image}, cpu {}, memory {}, config {}",
        normalize_cpu(cpu),
        normalize_memory(memory),
        &h[..12]
    )
}

/// What identifies a generic sidecar, built the same way from the
/// configuration and from a live container.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidecarView {
    pub image: String,
    pub cpu: String,
    pub memory: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
    /// Plain values, and `secret NAME@VERSION` for Secret Manager references.
    pub env: BTreeMap<String, String>,
    /// `GET /path :port` or `TCP :port`.
    pub probe: Option<String>,
    /// Volume name -> mount path.
    pub mounts: BTreeMap<String, String>,
    pub before_app: bool,
}

impl SidecarView {
    pub fn from_config(sc: &crate::config::SidecarConfig) -> Self {
        let mut env = sc.env.clone();
        for (k, s) in &sc.secrets {
            env.insert(k.clone(), format!("secret {}", secret_display(s)));
        }
        Self {
            image: sc.image.clone(),
            cpu: sc.cpu.clone(),
            memory: sc.memory.clone(),
            command: sc.command.clone(),
            args: sc.args.clone(),
            env,
            probe: sc
                .health_check
                .as_ref()
                .map(|h| sidecar_probe(h.path.as_deref(), h.port)),
            mounts: sc.volumes.clone(),
            before_app: sc.start_before_app,
        }
    }
}

pub fn sidecar_probe(path: Option<&str>, port: u16) -> String {
    match path {
        Some(p) => format!("GET {p} :{port}"),
        None => format!("TCP :{port}"),
    }
}

/// `IMAGE, cpu C, memory M, config <hash>`: the hash covers the command,
/// arguments, environment, probe, mounts and start order.
pub fn sidecar_fingerprint(v: &SidecarView) -> String {
    use sha2::Digest;
    let rest = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{}",
        v.command, v.args, v.env, v.probe, v.mounts, v.before_app
    );
    let h = crate::build::package::hex(&sha2::Sha256::digest(rest.as_bytes()));
    format!(
        "{}, cpu {}, memory {}, config {}",
        v.image,
        normalize_cpu(&v.cpu),
        normalize_memory(&v.memory),
        &h[..12]
    )
}

/// `GET /healthz every 10s, timeout 3s, 12 failures, delay 0s` (desired and observed).
pub fn probe_display(path: &str, p: &crate::config::ProbeSettings) -> String {
    format!(
        "GET {path} every {}s, timeout {}s, {} failures, delay {}s",
        p.period_seconds, p.timeout_seconds, p.failure_threshold, p.initial_delay_seconds
    )
}

/// `gs://bucket -> /path (ro) [opt,opt]`, the same for desired and observed state.
pub fn volume_display(v: &crate::config::VolumeConfig) -> String {
    let mut s = format!(
        "gs://{} -> {} ({})",
        v.bucket,
        v.mount_path,
        if v.read_only { "ro" } else { "rw" }
    );
    if !v.mount_options.is_empty() {
        s.push_str(&format!(" [{}]", v.mount_options.join(",")));
    }
    s
}

pub fn secret_display(s: &SecretRef) -> String {
    if s.pin_latest && s.version == "latest" {
        // Not resolved yet (offline plan).
        return format!("{}@(newest version, resolved at deploy)", s.secret);
    }
    format!("{}@{}", s.secret, s.version)
}

pub fn normalize_cpu(s: &str) -> String {
    parse_cpu_millis(s)
        .map(canonical_cpu)
        .unwrap_or_else(|| s.to_string())
}

pub fn normalize_memory(s: &str) -> String {
    const MI: u64 = 1024 * 1024;
    match parse_memory_bytes(s) {
        Some(b) if b % (1024 * MI) == 0 => format!("{}Gi", b / (1024 * MI)),
        Some(b) if b % MI == 0 => format!("{}Mi", b / MI),
        Some(b) => b.to_string(),
        None => s.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldChange {
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// Field-by-field difference between observed and desired flat maps.
///
/// Labels present on the live service but not managed by runway are ignored
/// (runway preserves them). The `image` field can be excluded when the final
/// image is not yet known.
pub fn diff(
    observed: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> Vec<FieldChange> {
    let mut out = Vec::new();
    for (k, after) in desired {
        match observed.get(k) {
            Some(before) if before == after => {}
            before => out.push(FieldChange {
                field: k.clone(),
                before: before.cloned(),
                after: Some(after.clone()),
            }),
        }
    }
    for (k, before) in observed {
        // Labels set by others are kept. runway annotations that stay are in
        // the desired state (see `gcp::run::spec_for_live`); others go.
        if desired.contains_key(k) || k.starts_with("labels.") {
            continue;
        }
        out.push(FieldChange {
            field: k.clone(),
            before: Some(before.clone()),
            after: None,
        });
    }
    out
}

/// What will be deployed as the container image.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ImagePlan {
    /// Immutable digest is known now.
    Pinned {
        reference: String,
        digest: String,
        origin: String,
    },
    /// A tag that could not be resolved locally; Cloud Run resolves it when the revision is created.
    Unresolved { reference: String, reason: String },
    /// The image will be produced by Cloud Build; its digest is unknown until the build finishes.
    PendingBuild { target: String, reason: String },
}

impl ImagePlan {
    pub fn is_exact(&self) -> bool {
        matches!(self, ImagePlan::Pinned { .. })
    }
    pub fn deploy_reference(&self) -> Option<&str> {
        match self {
            ImagePlan::Pinned { reference, .. } | ImagePlan::Unresolved { reference, .. } => {
                Some(reference)
            }
            ImagePlan::PendingBuild { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildPlan {
    pub context: String,
    /// `Dockerfile \`path\`` or `buildpacks (builder)`.
    pub strategy: String,
    pub source_sha256: String,
    pub files: usize,
    /// Uncompressed size of the build context (the upload is gzip-compressed).
    pub tar_bytes: u64,
    pub upload_to: String,
    pub image: String,
    pub build_service_account: String,
    /// `true` = a build will run, `false` = image already exists, `null` = unknown (offline).
    pub will_build: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Create,
    Update,
    NoChange,
    /// The service exists but is not owned by this app/stage.
    Conflict,
    /// Remote state was not inspected.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessAction {
    GrantPublic,
    RevokePublic,
    NoChange,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccessPlan {
    pub desired_public: bool,
    /// Identity-Aware Proxy authenticates users in front of the service.
    pub iap: bool,
    pub current_public: Option<bool>,
    pub action: AccessAction,
}

impl AccessPlan {
    pub fn new(desired_public: bool, current_public: Option<bool>) -> Self {
        let action = match current_public {
            None => AccessAction::Unknown,
            Some(c) if c == desired_public => AccessAction::NoChange,
            Some(_) if desired_public => AccessAction::GrantPublic,
            Some(_) => AccessAction::RevokePublic,
        };
        Self {
            desired_public,
            iap: false,
            current_public,
            action,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub app: String,
    pub stage: String,
    pub project: String,
    pub region: String,
    pub service: String,
    /// True only when every value, including the image digest, is known.
    pub exact: bool,
    pub remote_inspected: bool,
    pub action: ServiceAction,
    pub changes: Vec<FieldChange>,
    pub image: ImagePlan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildPlan>,
    pub access: AccessPlan,
    /// Provisioning steps (identity, grants, tags, IAP) and their live state.
    pub steps: Vec<crate::provision::StepCheck>,
    pub notes: Vec<String>,
}

impl Plan {
    pub fn has_changes(&self) -> bool {
        !matches!(self.action, ServiceAction::NoChange)
            || !matches!(self.access.action, AccessAction::NoChange)
            || self
                .build
                .as_ref()
                .is_some_and(|b| b.will_build != Some(false))
    }
}

/// Builds the plan's action and change list from observed state.
///
/// `observed` is `None` when the service does not exist. `image_known`
/// controls whether the image field participates in the diff.
pub fn compute_changes(
    observed: Option<&BTreeMap<String, String>>,
    desired: &ServiceSpec,
    image: &ImagePlan,
) -> (ServiceAction, Vec<FieldChange>) {
    let mut flat = desired.flatten();
    let pending = matches!(image, ImagePlan::PendingBuild { .. });
    if pending {
        flat.remove("image");
    }
    match observed {
        None => {
            let mut changes: Vec<FieldChange> = flat
                .into_iter()
                .map(|(field, v)| FieldChange {
                    field,
                    before: None,
                    after: Some(v),
                })
                .collect();
            if let ImagePlan::PendingBuild { target, .. } = image {
                changes.insert(
                    0,
                    FieldChange {
                        field: "image".into(),
                        before: None,
                        after: Some(format!("{target} (digest known after build)")),
                    },
                );
            }
            (ServiceAction::Create, changes)
        }
        Some(obs) => {
            let mut obs = obs.clone();
            if pending {
                obs.remove("image");
            }
            let mut changes = diff(&obs, &flat);
            if let ImagePlan::PendingBuild { target, .. } = image {
                changes.insert(
                    0,
                    FieldChange {
                        field: "image".into(),
                        before: observed.and_then(|o| o.get("image").cloned()),
                        after: Some(format!("{target} (digest known after build)")),
                    },
                );
            }
            if changes.is_empty() {
                (ServiceAction::NoChange, changes)
            } else {
                (ServiceAction::Update, changes)
            }
        }
    }
}

/// Colors a diff line by its marker: `+` green, `-` red, `~` yellow,
/// `=` dimmed, `?` and `!` yellow. Details in parentheses are dimmed.
fn diff_line(
    c: &crate::style::Painter,
    indent: &str,
    mark: &str,
    text: &str,
    detail: Option<&str>,
) -> String {
    let body = format!("{mark} {text}");
    let body = match mark {
        "+" => c.green(&body),
        "-" => c.red(&body),
        "~" => c.yellow(&body),
        "=" => c.dim(&body),
        _ => c.bold_yellow(&body),
    };
    match detail {
        Some(d) if !d.is_empty() => format!("{indent}{body} {}\n", c.dim(&format!("({d})"))),
        _ => format!("{indent}{body}\n"),
    }
}

/// The plan of a stage with several services, jobs or schedules. A stage
/// with one service is planned as a [`Plan`] (same output as before).
#[derive(Debug, Clone, Serialize)]
pub struct StackPlan {
    pub app: String,
    pub stage: String,
    pub project: String,
    pub region: String,
    /// True only when every value of every part is known.
    pub exact: bool,
    pub remote_inspected: bool,
    pub services: Vec<Plan>,
    pub jobs: Vec<JobPlan>,
    /// Stage-wide steps: APIs, shared resources and grants, schedules, and
    /// removals.
    pub steps: Vec<crate::provision::StepCheck>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobPlan {
    pub job: String,
    pub name: String,
    pub action: ServiceAction,
    pub changes: Vec<FieldChange>,
    pub image: ImagePlan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildPlan>,
    pub notes: Vec<String>,
}

/// Changes of a job, with a pending image shown as in [`compute_changes`].
pub fn job_changes(
    observed: Option<&BTreeMap<String, String>>,
    desired: BTreeMap<String, String>,
    image: &ImagePlan,
) -> (ServiceAction, Vec<FieldChange>) {
    let mut flat = desired;
    let mut obs = observed.cloned();
    if let ImagePlan::PendingBuild { .. } = image {
        flat.remove("image");
        if let Some(o) = obs.as_mut() {
            o.remove("image");
        }
    }
    let mut changes = diff(&obs.clone().unwrap_or_default(), &flat);
    if let ImagePlan::PendingBuild { target, .. } = image {
        changes.insert(
            0,
            FieldChange {
                field: "image".into(),
                before: obs
                    .as_ref()
                    .and_then(|_| observed.and_then(|o| o.get("image").cloned())),
                after: Some(format!("{target} (digest known after build)")),
            },
        );
    }
    let action = match (observed, changes.is_empty()) {
        (None, _) => ServiceAction::Create,
        (Some(_), true) => ServiceAction::NoChange,
        (Some(_), false) => ServiceAction::Update,
    };
    (action, changes)
}

fn step_lines(c: &crate::style::Painter, steps: &[crate::provision::StepCheck]) -> String {
    let mut s = String::new();
    for st in steps {
        let mark = match st.state {
            crate::provision::StepState::InSync => "=",
            crate::provision::StepState::Pending => "+",
            crate::provision::StepState::Unknown => "?",
            crate::provision::StepState::PendingRemoval => "-",
        };
        s.push_str(&diff_line(c, "  ", mark, &st.step, Some(&st.detail)));
    }
    s
}

/// Human-readable plan of a stage: each service as [`render_text`], then
/// jobs, stage-wide steps and one verdict.
pub fn render_stack_text(p: &StackPlan) -> String {
    let c = crate::style::out();
    let header = |t: &str| format!("\n{}\n", c.bold(t));
    let mut s = format!(
        "{} {} {} in {}/{}\n",
        c.bold("Plan for"),
        c.bold_cyan(&p.app),
        c.dim(&format!("(stage {})", p.stage)),
        p.project,
        p.region
    );
    for svc in &p.services {
        let text = render_text(svc);
        let body = text.split("\nThis plan is").next().unwrap_or(&text);
        s.push('\n');
        s.push_str(body.trim_end());
        s.push('\n');
    }
    for j in &p.jobs {
        s.push_str(&format!("\n{} {}\n", c.bold("Job"), c.bold_cyan(&j.job)));
        if let Some(b) = &j.build {
            let what = match b.will_build {
                Some(true) => format!("+ build {}", b.image),
                Some(false) => format!("= image {} already exists; build skipped", b.image),
                None => format!("? build {} (unless already built)", b.image),
            };
            s.push_str(&format!("  {what}\n"));
        }
        let verb = match j.action {
            ServiceAction::Create => "create",
            ServiceAction::Update => "update",
            ServiceAction::NoChange => "no change",
            ServiceAction::Conflict => "refused (not managed by runway for this app and stage)",
            ServiceAction::Unknown => "unknown (not inspected)",
        };
        s.push_str(&format!("  {}\n", c.bold(verb)));
        for ch in &j.changes {
            let (mark, text) = match (&ch.before, &ch.after) {
                (Some(b), Some(a)) => ("~", format!("{}: {b} -> {a}", ch.field)),
                (None, Some(a)) => ("+", format!("{}: {a}", ch.field)),
                (Some(b), None) => ("-", format!("{}: {b}", ch.field)),
                (None, None) => ("~", ch.field.clone()),
            };
            s.push_str(&diff_line(&c, "    ", mark, &text, None));
        }
        for n in &j.notes {
            s.push_str(&format!("  - {n}\n"));
        }
    }
    if !p.steps.is_empty() {
        s.push_str(&header("Stage provisioning:"));
        s.push_str(&step_lines(&c, &p.steps));
    }
    if !p.notes.is_empty() {
        s.push_str(&header("Notes:"));
        for n in &p.notes {
            s.push_str(&format!("  - {n}\n"));
        }
    }
    s.push_str(&format!(
        "\nThis plan is {}.\n",
        if p.exact {
            c.bold_green("exact: it shows the configuration deploy will apply")
        } else {
            c.bold_yellow(
                "NOT an exact preview: some values are only known during deploy (see above)",
            )
        }
    ));
    s
}

/// Human-readable plan. Colors (diff markers, headers, verdict) only appear
/// when stdout is a terminal; see [`crate::style`].
pub fn render_text(p: &Plan) -> String {
    let c = crate::style::out();
    let header = |t: &str| format!("\n{}\n", c.bold(t));
    let mut s = String::new();
    s.push_str(&format!(
        "{} {} {} in {}/{}\n",
        c.bold("Plan for"),
        c.bold_cyan(&p.service),
        c.dim(&format!("(stage {})", p.stage)),
        p.project,
        p.region
    ));
    if !p.remote_inspected {
        s.push_str(&format!(
            "  {}\n",
            c.yellow("Remote state was NOT inspected (offline); changes are relative to an empty project.")
        ));
    }
    if let Some(b) = &p.build {
        s.push_str(&header("Build:"));
        s.push_str(&format!(
            "  context     {} ({} files, {} uncompressed), {}\n",
            b.context,
            b.files,
            human_bytes(b.tar_bytes),
            b.strategy
        ));
        s.push_str(&format!("  source hash {}\n", c.dim(&b.source_sha256)));
        match b.will_build {
            Some(true) => {
                s.push_str(&diff_line(
                    &c,
                    "  ",
                    "+",
                    &format!("upload    {}", b.upload_to),
                    None,
                ));
                s.push_str(&diff_line(
                    &c,
                    "  ",
                    "+",
                    &format!("build     {}", b.image),
                    Some(&format!("Cloud Build as {}", b.build_service_account)),
                ));
            }
            Some(false) => s.push_str(&diff_line(
                &c,
                "  ",
                "=",
                &format!("image {} already exists; build skipped", b.image),
                None,
            )),
            None => {
                s.push_str(&diff_line(
                    &c,
                    "  ",
                    "?",
                    &format!("upload    {}", b.upload_to),
                    Some("unless already built"),
                ));
                s.push_str(&diff_line(
                    &c,
                    "  ",
                    "?",
                    &format!("build     {}", b.image),
                    None,
                ));
            }
        }
    }
    s.push_str(&header("Image:"));
    match &p.image {
        ImagePlan::Pinned { reference, origin, .. } => s.push_str(&format!(
            "  {reference}\n  {}\n",
            c.green(&format!("(immutable digest, {origin})"))
        )),
        ImagePlan::Unresolved { reference, reason } => s.push_str(&format!(
            "  {reference}\n  {}\n",
            c.yellow(&format!(
                "(digest NOT resolved: {reason}; Cloud Run will resolve the tag when the revision is created)"
            ))
        )),
        ImagePlan::PendingBuild { target, reason } => s.push_str(&format!(
            "  {target}\n  {}\n",
            c.yellow(&format!("(digest NOT known yet: {reason})"))
        )),
    }
    s.push_str(&header("Service:"));
    let (mark, verb) = match p.action {
        ServiceAction::Create => ("+", "create"),
        ServiceAction::Update => ("~", "update"),
        ServiceAction::NoChange => ("=", "no changes to"),
        ServiceAction::Conflict => ("!", "refuse to modify"),
        ServiceAction::Unknown => ("?", "create or update"),
    };
    s.push_str(&diff_line(
        &c,
        "  ",
        mark,
        &format!("{verb} {}", p.service),
        None,
    ));
    for ch in &p.changes {
        match (&ch.before, &ch.after) {
            (None, Some(a)) => s.push_str(&diff_line(
                &c,
                "      ",
                "+",
                &format!("{}: {a}", ch.field),
                None,
            )),
            (Some(b), None) => s.push_str(&diff_line(
                &c,
                "      ",
                "-",
                &format!("{}: {b}", ch.field),
                None,
            )),
            (Some(b), Some(a)) => s.push_str(&format!(
                "      {} {}\n",
                c.yellow(&format!("~ {}:", ch.field)),
                format_args!("{} -> {}", c.red(b), c.green(a))
            )),
            (None, None) => {}
        }
    }
    s.push_str(&header("Access:"));
    let want = match (p.access.iap, p.access.desired_public) {
        (true, false) => "Identity-Aware Proxy (IAP members only; no allUsers invoker)",
        (true, true) => "Identity-Aware Proxy in front of an allUsers invoker binding",
        (false, true) => "public (allUsers may invoke)",
        (false, false) => "private (IAM-authenticated callers only)",
    };
    s.push_str(&match p.access.action {
        AccessAction::GrantPublic => diff_line(
            &c,
            "  ",
            "+",
            &format!("grant roles/run.invoker to allUsers -> {want}"),
            None,
        ),
        AccessAction::RevokePublic => diff_line(
            &c,
            "  ",
            "-",
            &format!("remove allUsers from roles/run.invoker -> {want}"),
            None,
        ),
        AccessAction::NoChange => diff_line(&c, "  ", "=", want, None),
        AccessAction::Unknown => {
            diff_line(&c, "  ", "?", want, Some("current policy not inspected"))
        }
    });
    if !p.steps.is_empty() {
        s.push_str(&header("Provisioning:"));
        for st in &p.steps {
            let mark = match st.state {
                crate::provision::StepState::InSync => "=",
                crate::provision::StepState::Pending => "+",
                crate::provision::StepState::Unknown => "?",
                crate::provision::StepState::PendingRemoval => "-",
            };
            s.push_str(&diff_line(&c, "  ", mark, &st.step, Some(&st.detail)));
        }
    }
    if !p.notes.is_empty() {
        s.push_str(&header("Notes:"));
        for n in &p.notes {
            s.push_str(&format!("  - {n}\n"));
        }
    }
    s.push_str(&format!(
        "\nThis plan is {}.\n",
        if p.exact {
            c.bold_green("exact: it shows the configuration deploy will apply")
        } else {
            c.bold_yellow(
                "NOT an exact preview: some values are only known during deploy (see above)",
            )
        }
    ));
    s
}

pub fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f < K {
        format!("{b} B")
    } else if f < K * K {
        format!("{:.1} KiB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MiB", f / K / K)
    } else {
        format!("{:.1} GiB", f / K / K / K)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Artifact, ServiceConfig};
    use crate::image_ref::ImageRef;

    fn deployment() -> Deployment {
        Deployment {
            app: "hello".into(),
            stage: "dev".into(),
            project: "my-gcp-project".into(),
            region: "europe-west1".into(),
            service_id: "hello-dev".into(),
            key: None,
            kind: crate::config::WorkloadKind::Service,
            artifact: Artifact::Image {
                reference: "nginx:1".into(),
                parsed: ImageRef::parse("nginx:1").unwrap(),
            },
            service: ServiceConfig {
                port: 8080,
                cpu: "1".into(),
                memory: "512Mi".into(),
                timeout_seconds: 60,
                concurrency: 80,
                min_instances: 0,
                max_instances: 2,
                public: false,
                service_account: "rt@my-gcp-project.iam.gserviceaccount.com".into(),
                ingress: "all".into(),
                health_check: None,
                bootstrap: None,
                otel_collector: None,
                sidecars: Default::default(),
                env: BTreeMap::from([("LOG_LEVEL".into(), "info".into())]),
                secrets: BTreeMap::from([(
                    "DB".into(),
                    SecretRef {
                        secret: "db".into(),
                        version: "1".into(),
                        ..Default::default()
                    },
                )]),
                tags: BTreeMap::new(),
                volumes: BTreeMap::new(),
                iap: Default::default(),
                identity: Default::default(),
                billing: crate::config::BILLING_REQUEST.into(),
                startup_cpu_boost: false,
                execution_environment: None,
                sandbox: false,
                command: Vec::new(),
                args: Vec::new(),
                vpc: None,
                cloud_sql: Vec::new(),
                custom_audiences: Vec::new(),
            },
            retry: Default::default(),
            apis: Default::default(),
            impersonate: None,
            scheduler_region: "europe-west1".into(),
            release: Default::default(),
            project_tags: Default::default(),
            buckets: Default::default(),
            secrets: Default::default(),
        }
    }

    fn pinned(r: &str) -> ImagePlan {
        ImagePlan::Pinned {
            reference: r.into(),
            digest: "sha256:x".into(),
            origin: "test".into(),
        }
    }

    #[test]
    fn create_when_service_missing() {
        let spec = ServiceSpec::from_deployment(&deployment(), "img@sha256:1", BTreeMap::new());
        let (action, changes) = compute_changes(None, &spec, &pinned("img@sha256:1"));
        assert_eq!(action, ServiceAction::Create);
        assert!(
            changes
                .iter()
                .any(|c| c.field == "env.LOG_LEVEL" && c.after.as_deref() == Some("info"))
        );
        assert!(
            changes
                .iter()
                .any(|c| c.field == "secrets.DB" && c.after.as_deref() == Some("db@1"))
        );
    }

    #[test]
    fn no_change_when_equivalent_values() {
        let spec = ServiceSpec::from_deployment(&deployment(), "img@sha256:1", BTreeMap::new());
        let mut observed = spec.flatten();
        // Equivalent spellings from the API must not count as changes.
        observed.insert("cpu".into(), normalize_cpu("1000m"));
        observed.insert("memory".into(), normalize_memory("0512Mi"));
        // Labels added by someone else are preserved, not reported.
        observed.insert("labels.team".into(), "payments".into());
        let (action, changes) = compute_changes(Some(&observed), &spec, &pinned("img@sha256:1"));
        assert_eq!(action, ServiceAction::NoChange, "{changes:?}");
    }

    #[test]
    fn update_reports_field_level_changes() {
        let spec = ServiceSpec::from_deployment(&deployment(), "img@sha256:2", BTreeMap::new());
        let mut observed = spec.flatten();
        observed.insert("image".into(), "img@sha256:1".into());
        observed.insert("max_instances".into(), "5".into());
        observed.insert("env.OLD".into(), "x".into());
        observed.remove("secrets.DB");
        let (action, changes) = compute_changes(Some(&observed), &spec, &pinned("img@sha256:2"));
        assert_eq!(action, ServiceAction::Update);
        let fields: Vec<_> = changes.iter().map(|c| c.field.as_str()).collect();
        assert_eq!(fields, ["image", "max_instances", "secrets.DB", "env.OLD"]);
        let removed = changes.iter().find(|c| c.field == "env.OLD").unwrap();
        assert_eq!(removed.after, None);
    }

    #[test]
    fn annotation_only_changes_are_planned() {
        let with = |k: &str, v: &str| {
            ServiceSpec::from_deployment(
                &deployment(),
                "img@sha256:1",
                [(k.to_string(), v.to_string())].into(),
            )
        };
        // Same digest, reached through another tag: only the image reference differs.
        let live = with("runway.dev/image-ref", "img:1.0").flatten();
        let spec = with("runway.dev/image-ref", "img:stable");
        let (action, changes) = compute_changes(Some(&live), &spec, &pinned("img@sha256:1"));
        assert_eq!(action, ServiceAction::Update);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].field, "annotations.runway.dev/image-ref");
        // A live annotation the desired state does not keep is reported as removed
        // (which ones are kept is decided by `gcp::run::spec_for_live`).
        let mut live = with("runway.dev/image-ref", "img:1.0").flatten();
        live.insert("annotations.runway.dev/release".into(), "v1.0.0".into());
        let spec = with("runway.dev/image-ref", "img:1.0");
        let (action, changes) = compute_changes(Some(&live), &spec, &pinned("img@sha256:1"));
        assert_eq!(action, ServiceAction::Update);
        assert!(
            changes
                .iter()
                .any(|c| c.field == "annotations.runway.dev/release" && c.after.is_none())
        );
    }

    #[test]
    fn pending_build_is_always_an_image_change_and_not_exact() {
        let spec = ServiceSpec::from_deployment(&deployment(), "", BTreeMap::new());
        let observed =
            ServiceSpec::from_deployment(&deployment(), "old@sha256:1", BTreeMap::new()).flatten();
        let image = ImagePlan::PendingBuild {
            target: "repo/hello:src-abc".into(),
            reason: "not built yet".into(),
        };
        let (action, changes) = compute_changes(Some(&observed), &spec, &image);
        assert_eq!(action, ServiceAction::Update);
        assert_eq!(changes.len(), 1);
        assert!(
            changes[0]
                .after
                .as_ref()
                .unwrap()
                .contains("digest known after build")
        );
        assert!(!image.is_exact());
    }

    #[test]
    fn access_plan_actions() {
        assert_eq!(
            AccessPlan::new(true, Some(false)).action,
            AccessAction::GrantPublic
        );
        assert_eq!(
            AccessPlan::new(false, Some(true)).action,
            AccessAction::RevokePublic
        );
        assert_eq!(
            AccessPlan::new(false, Some(false)).action,
            AccessAction::NoChange
        );
        assert_eq!(AccessPlan::new(false, None).action, AccessAction::Unknown);
    }

    #[test]
    fn rendered_plan_flags_inexact_previews() {
        let spec = ServiceSpec::from_deployment(&deployment(), "", BTreeMap::new());
        let image = ImagePlan::PendingBuild {
            target: "repo/hello:src-abc".into(),
            reason: "the build has not run".into(),
        };
        let (action, changes) = compute_changes(None, &spec, &image);
        let plan = Plan {
            app: "hello".into(),
            stage: "dev".into(),
            project: "p".into(),
            region: "r".into(),
            service: "hello-dev".into(),
            exact: false,
            remote_inspected: false,
            action,
            changes,
            image,
            build: None,
            access: AccessPlan::new(false, None),
            steps: vec![],
            notes: vec![],
        };
        let text = render_text(&plan);
        assert!(text.contains("digest NOT known yet"));
        assert!(text.contains("NOT an exact preview"));
        assert!(text.contains("+ create hello-dev"));
        assert!(plan.has_changes());
    }
}
