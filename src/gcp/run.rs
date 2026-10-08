//! Cloud Run v2 request construction and response interpretation.

use crate::config::SecretRef;
use crate::naming;
use crate::plan::{ServiceSpec, normalize_cpu, normalize_memory, secret_display};
use google_cloud_api::model::LaunchStage;
use google_cloud_run_v2::model::{
    CloudSqlInstance, Condition, Container, ContainerPort, EnvVar, EnvVarSource,
    ExecutionEnvironment, GCSVolumeSource, HTTPGetAction, IngressTraffic, Probe,
    ResourceRequirements, RevisionScaling, RevisionTemplate, SecretKeySelector, SecretVolumeSource,
    Service, TCPSocketAction, TrafficTarget, TrafficTargetAllocationType, VersionToPath, Volume,
    VolumeMount, VpcAccess, condition, env_var, probe, volume, vpc_access,
};
use serde::Serialize;
use std::collections::BTreeMap;

/// Fields runway owns on the Service resource. Everything else (for example
/// `description`, `binary_authorization`) is left untouched.
pub const UPDATE_MASK: &[&str] = &[
    "labels",
    "annotations",
    "client",
    "client_version",
    "custom_audiences",
    "ingress",
    "invoker_iam_disabled",
    "iap_enabled",
    "launch_stage",
    "template",
    "traffic",
];

pub const CLIENT_NAME: &str = "runway";

/// Label/annotation namespaces the Cloud Run v2 API rejects in requests.
const RESERVED_PREFIXES: &[&str] = &[
    "run.googleapis.com/",
    "cloud.googleapis.com/",
    "serving.knative.dev/",
    "autoscaling.knative.dev/",
];

pub(crate) fn is_reserved_key(k: &str) -> bool {
    RESERVED_PREFIXES.iter().any(|p| k.starts_with(p))
}

/// Builds the Service message for create (`existing = None`) or update.
///
/// Labels and annotations set by others are preserved; runway's own labels
/// are enforced and stale `runway.dev/*` annotations are dropped.
/// The app container (image, command, limits, env, mounts) and its volumes:
/// what services and jobs have in common.
pub(crate) fn app_container_and_volumes(spec: &ServiceSpec) -> (Container, Vec<Volume>) {
    let mut env: Vec<EnvVar> = spec
        .env
        .iter()
        .map(|(k, v)| EnvVar::new().set_name(k).set_value(v))
        .collect();
    env.extend(
        spec.secrets
            .iter()
            .filter(|(_, s)| s.path.is_none())
            .map(|(k, s)| {
                EnvVar::new().set_name(k).set_value_source(
                    EnvVarSource::new().set_secret_key_ref(
                        SecretKeySelector::new()
                            .set_secret(&s.secret)
                            .set_version(&s.version),
                    ),
                )
            }),
    );

    let container =
        Container::new()
            .set_image(&spec.image)
            .set_command(spec.command.clone())
            .set_args(spec.args.clone())
            .set_resources(ResourceRequirements::new().set_limits([
                ("cpu".to_string(), spec.cpu.clone()),
                ("memory".to_string(), spec.memory.clone()),
            ]))
            .set_env(env)
            .set_volume_mounts(
                spec.volumes
                    .iter()
                    .map(|(name, v)| {
                        VolumeMount::new()
                            .set_name(name)
                            .set_mount_path(&v.mount_path)
                    })
                    .chain(secret_files(spec).map(|(vol, dir, _, _)| {
                        VolumeMount::new().set_name(vol).set_mount_path(dir)
                    }))
                    .chain((!spec.cloud_sql.is_empty()).then(|| {
                        VolumeMount::new()
                            .set_name(crate::config::CLOUD_SQL_VOLUME)
                            .set_mount_path(crate::config::CLOUD_SQL_MOUNT)
                    })),
            );
    let volumes: Vec<Volume> = spec
        .volumes
        .iter()
        .map(|(name, v)| {
            Volume::new().set_name(name).set_gcs(
                GCSVolumeSource::new()
                    .set_bucket(&v.bucket)
                    .set_read_only(v.read_only)
                    .set_mount_options(v.mount_options.clone()),
            )
        })
        .chain(secret_files(spec).map(|(vol, _, file, s)| {
            Volume::new().set_name(vol).set_secret(
                SecretVolumeSource::new()
                    .set_secret(&s.secret)
                    .set_items([VersionToPath::new().set_path(file).set_version(&s.version)]),
            )
        }))
        .chain((!spec.cloud_sql.is_empty()).then(|| {
            Volume::new()
                .set_name(crate::config::CLOUD_SQL_VOLUME)
                .set_cloud_sql_instance(
                    CloudSqlInstance::new().set_instances(spec.cloud_sql.clone()),
                )
        }))
        .collect();
    (container, volumes)
}

pub fn desired_service(spec: &ServiceSpec, name: &str, existing: Option<&Service>) -> Service {
    let mut labels: BTreeMap<String, String> = existing
        .map(|s| {
            s.labels
                .iter()
                .filter(|(k, _)| !is_reserved_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    labels.extend(spec.labels.clone());

    let mut annotations: BTreeMap<String, String> = existing
        .map(|s| {
            s.annotations
                .iter()
                .filter(|(k, _)| !k.starts_with("runway.dev/") && !is_reserved_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    annotations.extend(spec.annotations.clone());

    let (container, volumes) = app_container_and_volumes(spec);
    let container = container.set_ports([ContainerPort::new()
        .set_name("http1")
        .set_container_port(spec.port as i32)]);
    let container = match &spec.health_check {
        None => container,
        Some(hc) => {
            let to_probe = |p: &crate::config::ProbeSettings| {
                Probe::new()
                    .set_http_get(HTTPGetAction::new().set_path(&hc.path))
                    .set_initial_delay_seconds(p.initial_delay_seconds as i32)
                    .set_period_seconds(p.period_seconds as i32)
                    .set_timeout_seconds(p.timeout_seconds as i32)
                    .set_failure_threshold(p.failure_threshold as i32)
            };
            let c = container.set_startup_probe(to_probe(&hc.startup));
            match &hc.liveness {
                Some(l) => c.set_liveness_probe(to_probe(l)),
                None => c,
            }
        }
    };

    // Same template as the live one, or a revision behind the mode's URL that
    // already runs this configuration: send the template back untouched, so
    // that changes to traffic or ingress alone never create a revision.
    let reuse = existing.filter(|e| {
        spec.traffic.serve.is_some()
            || (spec.revision_annotations.is_empty()
                && crate::plan::diff(&observed_flat(e), &spec.flatten())
                    .iter()
                    .all(|c| is_service_level(&c.field)))
    });
    let template = match reuse.and_then(|e| e.template.clone()) {
        Some(t) => t,
        None => build_template(spec, volumes, container),
    };

    let mut svc = Service::new()
        .set_labels(labels)
        .set_annotations(annotations)
        .set_client(CLIENT_NAME)
        .set_client_version(env!("CARGO_PKG_VERSION"))
        .set_ingress(match spec.ingress.as_str() {
            "internal" => IngressTraffic::InternalOnly,
            "internal-and-cloud-load-balancing" => IngressTraffic::InternalLoadBalancer,
            _ => IngressTraffic::All,
        })
        .set_invoker_iam_disabled(false)
        .set_iap_enabled(spec.iap_enabled)
        .set_custom_audiences(spec.custom_audiences.clone())
        .set_launch_stage(launch_stage(spec, existing))
        .set_template(template)
        .set_traffic(traffic_targets(&spec.traffic.entries));
    if let Some(e) = existing {
        svc = svc.set_name(name).set_etag(&e.etag);
    }
    svc
}

/// Sandboxes are a preview feature: the service must opt in to BETA.
/// Otherwise the live value is kept (it may allow features set outside runway).
fn launch_stage(spec: &ServiceSpec, existing: Option<&Service>) -> LaunchStage {
    match (spec.sandbox, existing) {
        (true, _) => LaunchStage::Beta,
        (false, Some(e)) => e.launch_stage.clone(),
        (false, None) => LaunchStage::default(),
    }
}

/// `Container.sandboxLauncher` is in the Cloud Run v2 API but not yet in
/// google-cloud-run-v2 (1.15). The SDK keeps fields it doesn't know and
/// sends them back, so a JSON round trip sets it.
pub(crate) fn with_sandbox_launcher(c: Container) -> Container {
    let mut v = serde_json::to_value(&c).expect("a container serializes");
    v["sandboxLauncher"] = true.into();
    serde_json::from_value(v).expect("a container deserializes")
}

fn sandbox_launcher(c: &Container) -> bool {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.get("sandboxLauncher")?.as_bool())
        .unwrap_or(false)
}

fn build_template(
    spec: &ServiceSpec,
    volumes: Vec<Volume>,
    container: Container,
) -> RevisionTemplate {
    let template = RevisionTemplate::new()
        .set_labels(spec.labels.clone())
        .set_annotations(spec.revision_annotations.clone())
        .set_scaling(
            RevisionScaling::new()
                .set_min_instance_count(spec.min_instances as i32)
                .set_max_instance_count(spec.max_instances as i32),
        )
        .set_timeout(
            google_cloud_wkt::Duration::new(spec.timeout_seconds as i64, 0)
                .expect("timeout within range"),
        )
        .set_service_account(&spec.service_account)
        .set_max_instance_request_concurrency(spec.concurrency as i32)
        .set_volumes(volumes)
        .set_containers(containers(spec, container))
        .set_execution_environment(execution_environment(spec));
    template.set_or_clear_vpc_access(vpc_access(spec))
}

pub(crate) fn execution_environment(spec: &ServiceSpec) -> ExecutionEnvironment {
    match spec.execution_environment.as_deref() {
        Some("gen1") => ExecutionEnvironment::Gen1,
        Some("gen2") => ExecutionEnvironment::Gen2,
        _ => ExecutionEnvironment::Unspecified,
    }
}

pub(crate) fn vpc_access(spec: &ServiceSpec) -> Option<VpcAccess> {
    spec.vpc.as_ref().map(|v| {
        VpcAccess::new()
            .set_egress(match v.egress.as_str() {
                "all-traffic" => vpc_access::VpcEgress::AllTraffic,
                _ => vpc_access::VpcEgress::PrivateRangesOnly,
            })
            .set_network_interfaces([vpc_access::NetworkInterface::new()
                .set_network(&v.network)
                .set_subnetwork(&v.subnet)
                .set_tags(v.network_tags.clone())])
    })
}

/// Env var carrying the collector configuration (read with `--config=env:`).
pub const OTEL_CONFIG_ENV: &str = "RUNWAY_OTELCOL_CONFIG";
const OTEL_HEALTH_PORT: i32 = 13133;
/// Name of the ingress (application) container when sidecars are present.
pub const APP_CONTAINER: &str = "app";

/// The ingress container plus sidecars. With the collector, the app container
/// is named and starts after the collector's health check passes.
fn containers(spec: &ServiceSpec, app: Container) -> Vec<Container> {
    let mut sidecars = Vec::new();
    let mut before_app: Vec<String> = Vec::new();
    if let Some(o) = &spec.otel_collector {
        sidecars.push(otel_container(o));
        before_app.push(crate::config::OTEL_COLLECTOR_NAME.to_string());
    }
    for (name, sc) in &spec.sidecars {
        sidecars.push(sidecar_container(name, sc));
        if sc.start_before_app {
            before_app.push(name.clone());
        }
    }
    let mut out = if sidecars.is_empty() {
        vec![app]
    } else {
        let mut out = vec![app.set_name(APP_CONTAINER).set_depends_on(before_app)];
        out.extend(sidecars);
        out
    };
    // Explicit on every container: with `resources` set, an absent `cpu_idle`
    // means CPU always allocated (instance-based billing).
    let cpu_idle = spec.billing != crate::config::BILLING_INSTANCE;
    for c in &mut out {
        let r = c.resources.take().unwrap_or_default();
        c.resources = Some(
            r.set_cpu_idle(cpu_idle)
                .set_startup_cpu_boost(spec.startup_cpu_boost),
        );
    }
    if spec.sandbox {
        out[0] = with_sandbox_launcher(std::mem::take(&mut out[0]));
    }
    out
}

fn sidecar_container(name: &str, sc: &crate::config::SidecarConfig) -> Container {
    let mut env: Vec<EnvVar> = sc
        .env
        .iter()
        .map(|(k, v)| EnvVar::new().set_name(k).set_value(v))
        .collect();
    env.extend(sc.secrets.iter().map(|(k, s)| {
        EnvVar::new().set_name(k).set_value_source(
            EnvVarSource::new().set_secret_key_ref(
                SecretKeySelector::new()
                    .set_secret(&s.secret)
                    .set_version(&s.version),
            ),
        )
    }));
    let c = Container::new()
        .set_name(name)
        .set_image(&sc.image)
        .set_command(sc.command.clone())
        .set_args(sc.args.clone())
        .set_env(env)
        .set_resources(ResourceRequirements::new().set_limits([
            ("cpu".to_string(), sc.cpu.clone()),
            ("memory".to_string(), sc.memory.clone()),
        ]))
        .set_volume_mounts(
            sc.volumes
                .iter()
                .map(|(v, p)| VolumeMount::new().set_name(v).set_mount_path(p)),
        );
    match &sc.health_check {
        None => c,
        Some(h) => {
            let probe = Probe::new()
                .set_period_seconds(5)
                .set_timeout_seconds(3)
                .set_failure_threshold(24);
            c.set_startup_probe(match &h.path {
                Some(p) => {
                    probe.set_http_get(HTTPGetAction::new().set_path(p).set_port(h.port as i32))
                }
                None => probe.set_tcp_socket(TCPSocketAction::new().set_port(h.port as i32)),
            })
        }
    }
}

fn otel_container(o: &crate::config::OtelCollector) -> Container {
    let name = crate::config::OTEL_COLLECTOR_NAME;
    Container::new()
        .set_name(name)
        .set_image(&o.image)
        .set_args([format!("--config=env:{OTEL_CONFIG_ENV}")])
        .set_env([EnvVar::new().set_name(OTEL_CONFIG_ENV).set_value(&o.config)])
        .set_resources(ResourceRequirements::new().set_limits([
            ("cpu".to_string(), o.cpu.clone()),
            ("memory".to_string(), o.memory.clone()),
        ]))
        .set_startup_probe(
            Probe::new()
                .set_http_get(
                    HTTPGetAction::new()
                        .set_path("/")
                        .set_port(OTEL_HEALTH_PORT),
                )
                .set_period_seconds(2)
                .set_timeout_seconds(1)
                .set_failure_threshold(30),
        )
}

/// Secrets mounted as files: (volume name, directory, file name, reference).
fn secret_files(spec: &ServiceSpec) -> impl Iterator<Item = (String, &str, &str, &SecretRef)> {
    spec.secrets.iter().filter_map(|(k, s)| {
        let path = s.path.as_deref()?;
        let dir = crate::config::secret_dir(path);
        let file = &path[dir.len() + 1..];
        Some((secret_volume_name(k), dir, file, s))
    })
}

/// `secret-<key>` as a valid volume name.
fn secret_volume_name(key: &str) -> String {
    let mut n: String = format!("secret-{key}")
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    n.truncate(63);
    n.trim_end_matches('-').to_string()
}

/// A live generic sidecar, in the same shape as the configuration.
fn observed_sidecar(c: &Container, app_deps: &[String]) -> crate::plan::SidecarView {
    let limits = c
        .resources
        .as_ref()
        .map(|r| r.limits.clone())
        .unwrap_or_default();
    let env = c
        .env
        .iter()
        .map(|e| {
            let v = match &e.values {
                Some(env_var::Values::Value(v)) => v.clone(),
                Some(env_var::Values::ValueSource(src)) => match &src.secret_key_ref {
                    Some(sel) => format!(
                        "secret {}",
                        secret_display(&SecretRef {
                            secret: sel.secret.clone(),
                            version: if sel.version.is_empty() {
                                "latest".into()
                            } else {
                                sel.version.clone()
                            },
                            ..Default::default()
                        })
                    ),
                    None => String::new(),
                },
                _ => String::new(),
            };
            (e.name.clone(), v)
        })
        .collect();
    let probe = c.startup_probe.as_ref().and_then(|p| match &p.probe_type {
        Some(probe::ProbeType::HttpGet(h)) => Some(crate::plan::sidecar_probe(
            Some(if h.path.is_empty() { "/" } else { &h.path }),
            h.port.max(0) as u16,
        )),
        Some(probe::ProbeType::TcpSocket(t)) => {
            Some(crate::plan::sidecar_probe(None, t.port.max(0) as u16))
        }
        _ => None,
    });
    crate::plan::SidecarView {
        image: c.image.clone(),
        cpu: limits.get("cpu").cloned().unwrap_or_else(|| "1".into()),
        memory: limits
            .get("memory")
            .cloned()
            .unwrap_or_else(|| "512Mi".into()),
        command: c.command.clone(),
        args: c.args.clone(),
        env,
        probe,
        mounts: c
            .volume_mounts
            .iter()
            .map(|m| (m.name.clone(), m.mount_path.clone()))
            .collect(),
        before_app: app_deps.contains(&c.name),
    }
}

/// The desired state against the live service: the traffic split for the
/// requested mode, and runway's annotations. All `runway.dev/*` annotations
/// describe the deployed image (reference, source hash, base images,
/// release), so the live ones the spec does not set are kept while the image
/// stays the same (a plain redeploy or a configuration change keeps the
/// release), and dropped when it changes. Plan and deploy both use this, so
/// the plan shows exactly what the request does.
pub fn spec_for_live(spec: &ServiceSpec, svc: Option<&Service>) -> ServiceSpec {
    let mut s = spec.with_current_traffic(svc.map(current_traffic).as_ref());
    let Some(svc) = svc else { return s };
    let live_image = svc
        .template
        .as_ref()
        .and_then(|t| {
            t.containers
                .iter()
                .find(|c| !c.ports.is_empty())
                .or(t.containers.first())
        })
        .map(|c| c.image.as_str());
    let same_image = if spec.image.is_empty() {
        // Planning before a build: the content-addressed image reference.
        let r = naming::ANNOTATION_IMAGE_REF;
        spec.annotations.contains_key(r) && spec.annotations.get(r) == svc.annotations.get(r)
    } else {
        live_image == Some(spec.image.as_str())
    };
    if same_image {
        for (k, v) in svc
            .annotations
            .iter()
            .filter(|(k, _)| k.starts_with("runway.dev/"))
        {
            s.annotations.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    // The grant record does not describe the image: a spec that does not set
    // it (plan) keeps it.
    for key in [
        crate::provision::ANNOTATION_GRANTS,
        crate::domains::ANNOTATION_DOMAINS,
    ] {
        if let Some(v) = svc.annotations.get(key) {
            s.annotations
                .entry(key.to_string())
                .or_insert_with(|| v.clone());
        }
    }
    s
}

/// True when every runway annotation of the spec (image reference, source
/// hash, base images, release) is already on the service. Annotations on the
/// service that the spec does not set are left alone, so a plain redeploy
/// keeps an earlier release annotation.
pub fn annotations_current(svc: &Service, spec: &ServiceSpec) -> bool {
    spec.annotations
        .iter()
        .all(|(k, v)| svc.annotations.get(k) == Some(v))
}

/// What a deploy changes on the live service. When a revision behind the
/// mode's URL already runs this configuration (`traffic.serve`), revision
/// fields are left alone: only service-level fields can change.
pub fn pending_changes(svc: &Service, spec: &ServiceSpec) -> Vec<crate::plan::FieldChange> {
    let mut changes = crate::plan::diff(&observed_flat(svc), &spec.flatten());
    if spec.traffic.serve.is_some() {
        changes.retain(|c| is_service_level(&c.field));
    }
    changes
}

/// Template annotation giving previews and canaries a revision of their own,
/// so that their URL never points at the revision serving production.
pub fn revision_marker(mode: &crate::traffic::Mode) -> Option<(&'static str, String)> {
    match mode {
        crate::traffic::Mode::Full => None,
        crate::traffic::Mode::Preview { tag } => Some(("runway.dev/preview", tag.clone())),
        crate::traffic::Mode::Canary { .. } => Some(("runway.dev/canary", "true".into())),
    }
}

/// True when a preview or canary needs a new revision because the latest one
/// is not already its own.
pub fn needs_own_revision(svc: &Service, mode: &crate::traffic::Mode) -> bool {
    revision_marker(mode).is_some_and(|(key, value)| {
        let live = svc.template.as_ref().and_then(|t| t.annotations.get(key));
        live != Some(&value)
    })
}

/// The traffic target behind the URL a deploy in `mode` changes, with its
/// revision: the main URL (one target serving 100%), the preview's tag or the
/// canary tag. `None` when there is no such single revision.
pub fn mode_target(
    cur: &crate::traffic::Current,
    mode: &crate::traffic::Mode,
) -> Option<(crate::traffic::Target, String)> {
    use crate::traffic::{CANARY_TAG, Mode, Target};
    let entry = match mode {
        Mode::Full => match cur
            .entries
            .iter()
            .filter(|e| e.percent > 0)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [e] if e.percent == 100 => Some(*e),
            _ => None,
        },
        Mode::Preview { tag } => cur.entries.iter().find(|e| &e.tag == tag),
        Mode::Canary { .. } => cur.entries.iter().find(|e| e.tag == CANARY_TAG),
    }?;
    let revision = match &entry.target {
        Target::Latest => cur.latest_ready.clone(),
        Target::Revision(r) => r.clone(),
    };
    (!revision.is_empty()).then(|| (entry.target.clone(), revision))
}

/// True when one revision serves all of the service's traffic. Grants removed
/// from the configuration are revoked only then, after a main deploy: no
/// other revision serving traffic can still need them.
pub fn one_revision_serves_all(svc: &Service) -> bool {
    mode_target(&current_traffic(svc), &crate::traffic::Mode::Full).is_some()
}

/// True when `rev` runs exactly the revision part of `spec` (image,
/// resources, scaling, env, secrets, probes, volumes, sidecars, identity) and
/// carries the marker of a preview or canary spec. Service-level fields are
/// compared elsewhere.
pub fn revision_matches(
    svc: &Service,
    rev: &google_cloud_run_v2::model::Revision,
    spec: &ServiceSpec,
) -> bool {
    if spec.image.is_empty() {
        return false;
    }
    let marked = spec
        .revision_annotations
        .iter()
        .all(|(k, v)| rev.annotations.get(k) == Some(v));
    let template = RevisionTemplate::new()
        .set_containers(rev.containers.clone())
        .set_volumes(rev.volumes.clone())
        .set_or_clear_vpc_access(rev.vpc_access.clone())
        // A revision reports the environment it runs on; unconfigured, Cloud
        // Run chose it, and any choice matches.
        .set_execution_environment(match spec.execution_environment {
            Some(_) => rev.execution_environment.clone(),
            None => ExecutionEnvironment::Unspecified,
        })
        .set_service_account(&rev.service_account)
        .set_max_instance_request_concurrency(rev.max_instance_request_concurrency)
        .set_or_clear_timeout(rev.timeout)
        .set_or_clear_scaling(rev.scaling.clone());
    let revision_fields = |m: BTreeMap<String, String>| -> BTreeMap<String, String> {
        m.into_iter()
            .filter(|(k, _)| !is_service_level(k))
            .collect()
    };
    marked
        && revision_fields(observed_flat(&svc.clone().set_template(template)))
            == revision_fields(spec.flatten())
}

/// Fields that live on the Service, not on the revision template.
pub fn is_service_level(field: &str) -> bool {
    matches!(
        field,
        "ingress" | "iap" | "invoker_iam_disabled" | "custom_audiences"
    ) || field == "traffic"
        || field.starts_with("traffic.")
        || field.starts_with("annotations.")
}

fn traffic_targets(entries: &[crate::traffic::Entry]) -> Vec<TrafficTarget> {
    entries
        .iter()
        .map(|e| {
            let t = TrafficTarget::new()
                .set_percent(e.percent as i32)
                .set_tag(&e.tag);
            match &e.target {
                crate::traffic::Target::Latest => t.set_type(TrafficTargetAllocationType::Latest),
                crate::traffic::Target::Revision(r) => t
                    .set_type(TrafficTargetAllocationType::Revision)
                    .set_revision(r),
            }
        })
        .collect()
}

/// The service's traffic as configured (not the status), with its latest
/// ready revision.
pub fn current_traffic(svc: &Service) -> crate::traffic::Current {
    use crate::traffic::{Entry, Target};
    let entries = if svc.traffic.is_empty() {
        vec![Entry::new(Target::Latest, 100, "")]
    } else {
        svc.traffic
            .iter()
            .map(|t| {
                Entry::new(
                    match t.r#type {
                        TrafficTargetAllocationType::Revision if !t.revision.is_empty() => {
                            Target::Revision(short_revision(&t.revision).to_string())
                        }
                        _ => Target::Latest,
                    },
                    t.percent.max(0) as u32,
                    t.tag.clone(),
                )
            })
            .collect()
    };
    crate::traffic::Current {
        entries: crate::traffic::normalize(entries),
        latest_ready: short_revision(&svc.latest_ready_revision).to_string(),
    }
}

/// Traffic-only update: the service as read, with `entries` as its traffic.
pub fn with_traffic(svc: &Service, entries: &[crate::traffic::Entry]) -> Service {
    svc.clone().set_traffic(traffic_targets(entries))
}

pub fn update_mask() -> google_cloud_wkt::FieldMask {
    google_cloud_wkt::FieldMask::default().set_paths(UPDATE_MASK.iter().map(|s| s.to_string()))
}

fn ingress_str(i: &IngressTraffic) -> String {
    match i {
        IngressTraffic::All | IngressTraffic::Unspecified => "all".into(),
        IngressTraffic::InternalOnly => "internal".into(),
        IngressTraffic::InternalLoadBalancer => "internal-and-cloud-load-balancing".into(),
        IngressTraffic::None => "none".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Extracts the normalized flat map of everything runway manages from a live service.
pub fn observed_flat(svc: &Service) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let template = svc.template.clone().unwrap_or_default();
    m.insert("containers".into(), template.containers.len().to_string());
    m.insert("ingress".into(), ingress_str(&svc.ingress));
    m.extend(crate::traffic::flat(&current_traffic(svc).entries));
    if svc.invoker_iam_disabled {
        m.insert("invoker_iam_disabled".into(), "true".into());
    }
    if svc.iap_enabled {
        m.insert("iap".into(), "enabled".into());
    }
    if !svc.custom_audiences.is_empty() {
        m.insert(
            "custom_audiences".into(),
            crate::plan::list_display(&svc.custom_audiences),
        );
    }
    match template.execution_environment {
        ExecutionEnvironment::Gen1 => {
            m.insert("execution_environment".into(), "gen1".into());
        }
        ExecutionEnvironment::Gen2 => {
            m.insert("execution_environment".into(), "gen2".into());
        }
        _ => {}
    }
    if let Some(va) = &template.vpc_access {
        let ni = va.network_interfaces.first().cloned().unwrap_or_default();
        if !ni.network.is_empty() || !ni.subnetwork.is_empty() || !va.connector.is_empty() {
            // Names alone (set outside runway) are in the service's project.
            let mut parts = svc.name.split('/').skip(1).step_by(2);
            let (project, region) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let full = |name: &str, f: &dyn Fn(&str) -> String| match name.is_empty()
                || project.is_empty()
            {
                true => name.to_string(),
                false => f(name),
            };
            let value = if !va.connector.is_empty() {
                format!("connector {}", va.connector)
            } else {
                crate::plan::vpc_display(&crate::config::VpcConfig {
                    network: full(&ni.network, &|n| {
                        crate::config::validate::full_network(n, project)
                    }),
                    subnet: full(&ni.subnetwork, &|n| {
                        crate::config::validate::full_subnet(n, project, region)
                    }),
                    egress: match va.egress {
                        vpc_access::VpcEgress::AllTraffic => "all-traffic".into(),
                        _ => "private-ranges-only".into(),
                    },
                    network_tags: ni.tags.clone(),
                })
            };
            m.insert("vpc".into(), value);
        }
    }
    for (k, v) in &svc.labels {
        m.insert(format!("labels.{k}"), v.clone());
    }
    for (k, v) in svc
        .annotations
        .iter()
        .filter(|(k, _)| k.starts_with("runway.dev/"))
    {
        m.insert(format!("annotations.{k}"), v.clone());
    }
    m.insert(
        "service_account".into(),
        if template.service_account.is_empty() {
            "(project default compute service account)".into()
        } else {
            template.service_account.clone()
        },
    );
    let timeout = template
        .timeout
        .as_ref()
        .map(|d| d.seconds())
        .unwrap_or(300);
    m.insert("timeout_seconds".into(), timeout.to_string());
    let conc = match template.max_instance_request_concurrency {
        0 => 80,
        c => c,
    };
    m.insert("concurrency".into(), conc.to_string());
    let (min, max) = template
        .scaling
        .as_ref()
        .map(|s| (s.min_instance_count, s.max_instance_count))
        .unwrap_or((0, 0));
    m.insert("min_instances".into(), min.to_string());
    m.insert(
        "max_instances".into(),
        if max == 0 {
            "100".into()
        } else {
            max.to_string()
        },
    );
    // Sidecars (containers without ports) are compared by name.
    let app_deps: Vec<String> = template
        .containers
        .iter()
        .find(|c| !c.ports.is_empty())
        .map(|c| c.depends_on.clone())
        .unwrap_or_default();
    for c in template
        .containers
        .iter()
        .filter(|c| c.ports.is_empty() && template.containers.len() > 1)
    {
        if c.name != crate::config::OTEL_COLLECTOR_NAME {
            m.insert(
                format!("sidecars.{}", c.name),
                crate::plan::sidecar_fingerprint(&observed_sidecar(c, &app_deps)),
            );
            continue;
        }
        let limits = c
            .resources
            .as_ref()
            .map(|r| r.limits.clone())
            .unwrap_or_default();
        let config = c
            .env
            .iter()
            .find(|e| e.name == OTEL_CONFIG_ENV)
            .and_then(|e| match &e.values {
                Some(env_var::Values::Value(v)) => Some(v.clone()),
                _ => None,
            })
            .unwrap_or_default();
        m.insert(
            format!("sidecars.{}", c.name),
            crate::plan::sidecar_display(
                &c.image,
                limits.get("cpu").map(String::as_str).unwrap_or("1"),
                limits.get("memory").map(String::as_str).unwrap_or("512Mi"),
                &config,
            ),
        );
    }
    let ingress = template
        .containers
        .iter()
        .find(|c| !c.ports.is_empty())
        .or(template.containers.first());
    if let Some(c) = ingress {
        for vol in &template.volumes {
            if let Some(volume::VolumeType::CloudSqlInstance(sql)) = &vol.volume_type {
                if !sql.instances.is_empty() {
                    m.insert(
                        "cloud_sql".into(),
                        crate::plan::list_display(&sql.instances),
                    );
                }
                continue;
            }
            // Secret files are compared by file path.
            if let Some(volume::VolumeType::Secret(sv)) = &vol.volume_type {
                let dir = c
                    .volume_mounts
                    .iter()
                    .find(|vm| vm.name == vol.name)
                    .map(|vm| vm.mount_path.clone())
                    .unwrap_or_else(|| "(not mounted)".into());
                for item in &sv.items {
                    m.insert(
                        format!("secret_files.{dir}/{}", item.path),
                        secret_display(&SecretRef {
                            secret: sv.secret.clone(),
                            version: if item.version.is_empty() {
                                "latest".into()
                            } else {
                                item.version.clone()
                            },
                            ..Default::default()
                        }),
                    );
                }
                continue;
            }
            let path = c
                .volume_mounts
                .iter()
                .find(|vm| vm.name == vol.name)
                .map(|vm| vm.mount_path.clone())
                .unwrap_or_else(|| "(not mounted)".into());
            let value = match &vol.volume_type {
                Some(volume::VolumeType::Gcs(g)) => {
                    crate::plan::volume_display(&crate::config::VolumeConfig {
                        bucket: g.bucket.clone(),
                        mount_path: path,
                        read_only: g.read_only,
                        mount_options: g.mount_options.clone(),
                    })
                }
                other => format!("{other:?} -> {path}"),
            };
            m.insert(format!("volumes.{}", vol.name), value);
        }
        m.insert("image".into(), c.image.clone());
        // Only HTTP probes are managed; Cloud Run's default TCP startup probe is ignored.
        let http_probe = |p: &Option<Probe>| -> Option<String> {
            let p = p.as_ref()?;
            match &p.probe_type {
                Some(probe::ProbeType::HttpGet(h)) => Some(crate::plan::probe_display(
                    if h.path.is_empty() { "/" } else { &h.path },
                    &crate::config::ProbeSettings {
                        initial_delay_seconds: p.initial_delay_seconds.max(0) as u32,
                        period_seconds: p.period_seconds.max(0) as u32,
                        timeout_seconds: p.timeout_seconds.max(0) as u32,
                        failure_threshold: p.failure_threshold.max(0) as u32,
                    },
                )),
                _ => None,
            }
        };
        if let Some(v) = http_probe(&c.startup_probe) {
            m.insert("health_check.startup".into(), v);
        }
        if let Some(v) = http_probe(&c.liveness_probe) {
            m.insert("health_check.liveness".into(), v);
        }
        let port = c.ports.first().map(|p| p.container_port).unwrap_or(8080);
        m.insert("port".into(), port.to_string());
        let limits = c
            .resources
            .as_ref()
            .map(|r| r.limits.clone())
            .unwrap_or_default();
        // Without `resources`, CPU is only allocated during requests.
        let instance_based = c.resources.as_ref().is_some_and(|r| !r.cpu_idle);
        m.insert(
            "billing".into(),
            if instance_based {
                crate::config::BILLING_INSTANCE
            } else {
                crate::config::BILLING_REQUEST
            }
            .into(),
        );
        if c.resources.as_ref().is_some_and(|r| r.startup_cpu_boost) {
            m.insert("startup_cpu_boost".into(), "enabled".into());
        }
        if sandbox_launcher(c) {
            m.insert("sandbox".into(), "enabled".into());
        }
        if !c.command.is_empty() {
            m.insert("command".into(), crate::plan::args_display(&c.command));
        }
        if !c.args.is_empty() {
            m.insert("args".into(), crate::plan::args_display(&c.args));
        }
        m.insert(
            "cpu".into(),
            normalize_cpu(limits.get("cpu").map(String::as_str).unwrap_or("1")),
        );
        m.insert(
            "memory".into(),
            normalize_memory(limits.get("memory").map(String::as_str).unwrap_or("512Mi")),
        );
        for e in &c.env {
            match &e.values {
                Some(env_var::Values::Value(v)) => {
                    m.insert(format!("env.{}", e.name), v.clone());
                }
                Some(env_var::Values::ValueSource(src)) => {
                    if let Some(sel) = &src.secret_key_ref {
                        m.insert(
                            format!("secrets.{}", e.name),
                            secret_display(&SecretRef {
                                secret: sel.secret.clone(),
                                version: if sel.version.is_empty() {
                                    "latest".into()
                                } else {
                                    sel.version.clone()
                                },
                                ..Default::default()
                            }),
                        );
                    }
                }
                _ => {
                    m.insert(format!("env.{}", e.name), String::new());
                }
            }
        }
    }
    m
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ownership {
    /// Labeled for this app and stage.
    Owned,
    /// No runway labels at all (created by another tool or by hand).
    Unmanaged,
    /// Managed by runway, but for a different app or stage.
    OtherOwner { app: String, stage: String },
}

pub fn ownership(svc: &Service, app: &str, stage: &str) -> Ownership {
    ownership_of(&svc.labels, app, stage)
}

/// [`ownership`] of any resource from its labels (services, jobs).
pub fn ownership_of(
    labels: &std::collections::HashMap<String, String>,
    app: &str,
    stage: &str,
) -> Ownership {
    let managed = labels.get(naming::LABEL_MANAGED_BY).map(String::as_str)
        == Some(naming::LABEL_MANAGED_BY_VALUE);
    let a = labels.get(naming::LABEL_APP).cloned().unwrap_or_default();
    let s = labels.get(naming::LABEL_STAGE).cloned().unwrap_or_default();
    if !managed && a.is_empty() && s.is_empty() {
        Ownership::Unmanaged
    } else if managed && a == app && s == stage {
        Ownership::Owned
    } else {
        Ownership::OtherOwner { app: a, stage: s }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Readiness {
    Ready,
    Reconciling { message: String },
    Failed { message: String },
}

fn condition_message(c: &Condition) -> String {
    let mut msg = c.message.clone();
    if msg.is_empty() {
        msg = format!("{:?}", c.reasons).trim().to_string();
    }
    msg
}

/// Interprets a service's reconciliation status.
pub fn readiness(svc: &Service) -> Readiness {
    let term = svc.terminal_condition.clone().unwrap_or_default();
    if svc.reconciling {
        return Readiness::Reconciling {
            message: if term.message.is_empty() {
                "reconciling".into()
            } else {
                term.message.clone()
            },
        };
    }
    match term.state {
        condition::State::ConditionSucceeded
            if svc.observed_generation == svc.generation
                && svc.latest_ready_revision == svc.latest_created_revision =>
        {
            Readiness::Ready
        }
        condition::State::ConditionFailed => Readiness::Failed {
            message: failure_details(svc),
        },
        condition::State::ConditionSucceeded => Readiness::Failed {
            message: format!(
                "latest revision {} is not serving (serving: {})",
                short_revision(&svc.latest_created_revision),
                short_revision(&svc.latest_ready_revision)
            ),
        },
        _ => Readiness::Reconciling {
            message: condition_message(&term),
        },
    }
}

/// Collects failure messages from the terminal condition and failed sub-conditions.
pub fn failure_details(svc: &Service) -> String {
    let mut parts = Vec::new();
    if let Some(t) = &svc.terminal_condition
        && !t.message.is_empty()
    {
        parts.push(t.message.clone());
    }
    for c in &svc.conditions {
        if c.state == condition::State::ConditionFailed
            && !c.message.is_empty()
            && !parts.contains(&c.message)
        {
            parts.push(format!("{}: {}", c.r#type, c.message));
        }
    }
    if parts.is_empty() {
        "the service did not become ready (no details reported)".into()
    } else {
        parts.join("; ")
    }
}

pub fn short_revision(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Serializable summary used by `info` and `deploy` output.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub url: Option<String>,
    pub urls: Vec<String>,
    pub readiness: Readiness,
    pub latest_ready_revision: String,
    pub latest_created_revision: String,
    pub generation: i64,
    pub observed_generation: i64,
    pub image: Option<String>,
    pub image_ref: Option<String>,
    /// Release tag recorded by `deploy --tag`/`--tag-rc`.
    pub release: Option<String>,
    pub traffic: Vec<TrafficLine>,
    pub ownership: Ownership,
    pub update_time: Option<String>,
    pub last_modifier: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrafficLine {
    pub revision: String,
    pub percent: i32,
    pub tag: String,
    pub uri: String,
}

/// The configured traffic entries (Cloud Run's status merges entries that
/// resolve to the same revision), with the URL of each tag.
fn traffic_lines(svc: &Service) -> Vec<TrafficLine> {
    let ready = short_revision(&svc.latest_ready_revision);
    let uri = |tag: &str| {
        svc.traffic_statuses
            .iter()
            .find(|t| !tag.is_empty() && t.tag == tag)
            .map(|t| t.uri.clone())
            .unwrap_or_default()
    };
    current_traffic(svc)
        .entries
        .iter()
        .map(|e| TrafficLine {
            revision: match &e.target {
                crate::traffic::Target::Latest => format!("{ready} (latest)"),
                crate::traffic::Target::Revision(r) => r.clone(),
            },
            percent: e.percent as i32,
            tag: e.tag.clone(),
            uri: uri(&e.tag),
        })
        .collect()
}

pub fn status(svc: &Service, app: &str, stage: &str) -> ServiceStatus {
    let image = svc
        .template
        .as_ref()
        .and_then(|t| t.containers.first())
        .map(|c| c.image.clone());
    ServiceStatus {
        name: svc.name.clone(),
        url: (!svc.uri.is_empty()).then(|| svc.uri.clone()),
        urls: svc.urls.clone(),
        readiness: readiness(svc),
        latest_ready_revision: short_revision(&svc.latest_ready_revision).to_string(),
        latest_created_revision: short_revision(&svc.latest_created_revision).to_string(),
        generation: svc.generation,
        observed_generation: svc.observed_generation,
        image,
        image_ref: svc.annotations.get(naming::ANNOTATION_IMAGE_REF).cloned(),
        release: svc.annotations.get(naming::ANNOTATION_RELEASE).cloned(),
        traffic: traffic_lines(svc),
        ownership: ownership(svc, app, stage),
        update_time: svc.update_time.map(String::from),
        last_modifier: svc.last_modifier.clone(),
        labels: svc
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::SecretRef;
    use google_cloud_run_v2::model::Condition;

    pub(crate) fn spec() -> ServiceSpec {
        ServiceSpec {
            image: "europe-west1-docker.pkg.dev/p/apps/hello@sha256:abc".into(),
            port: 9090,
            cpu: "2".into(),
            memory: "1Gi".into(),
            timeout_seconds: 60,
            concurrency: 40,
            min_instances: 1,
            max_instances: 3,
            service_account: "rt@p.iam.gserviceaccount.com".into(),
            ingress: "all".into(),
            health_check: None,
            otel_collector: None,
            sidecars: Default::default(),
            env: BTreeMap::from([("LOG_LEVEL".into(), "info".into())]),
            secrets: BTreeMap::from([(
                "DATABASE_URL".into(),
                SecretRef {
                    secret: "database-url".into(),
                    version: "3".into(),
                    ..Default::default()
                },
            )]),
            labels: naming::ownership_labels("hello", "dev"),
            volumes: BTreeMap::from([(
                "cache".to_string(),
                crate::config::VolumeConfig {
                    bucket: "my-cache".into(),
                    mount_path: "/mnt/cache".into(),
                    read_only: false,
                    mount_options: vec!["implicit-dirs".into()],
                },
            )]),
            iap_enabled: true,
            billing: crate::config::BILLING_REQUEST.into(),
            startup_cpu_boost: false,
            execution_environment: None,
            sandbox: false,
            command: Vec::new(),
            args: Vec::new(),
            vpc: None,
            cloud_sql: Vec::new(),
            custom_audiences: Vec::new(),
            annotations: BTreeMap::from([(
                naming::ANNOTATION_IMAGE_REF.to_string(),
                "europe-west1-docker.pkg.dev/p/apps/hello:src-1".to_string(),
            )]),
            revision_annotations: BTreeMap::new(),
            traffic: Default::default(),
        }
    }

    #[test]
    fn builds_complete_service_request() {
        let svc = desired_service(&spec(), "projects/p/locations/r/services/hello-dev", None);
        assert!(
            svc.name.is_empty(),
            "create requests use service_id, not name"
        );
        assert_eq!(svc.labels["managed-by"], "runway");
        assert_eq!(svc.labels["runway-stage"], "dev");
        assert_eq!(svc.ingress, IngressTraffic::All);
        assert!(!svc.invoker_iam_disabled);
        assert_eq!(svc.client, "runway");
        assert_eq!(svc.traffic.len(), 1);
        assert_eq!(svc.traffic[0].r#type, TrafficTargetAllocationType::Latest);
        assert_eq!(svc.traffic[0].percent, 100);

        let t = svc.template.as_ref().unwrap();
        assert_eq!(t.service_account, "rt@p.iam.gserviceaccount.com");
        assert_eq!(t.max_instance_request_concurrency, 40);
        assert_eq!(t.timeout.as_ref().unwrap().seconds(), 60);
        let sc = t.scaling.as_ref().unwrap();
        assert_eq!((sc.min_instance_count, sc.max_instance_count), (1, 3));
        assert_eq!(t.labels["runway-app"], "hello");

        let c = &t.containers[0];
        assert_eq!(
            c.image,
            "europe-west1-docker.pkg.dev/p/apps/hello@sha256:abc"
        );
        assert_eq!(c.ports[0].container_port, 9090);
        assert_eq!(c.ports[0].name, "http1");
        let limits = &c.resources.as_ref().unwrap().limits;
        assert_eq!(limits["cpu"], "2");
        assert_eq!(limits["memory"], "1Gi");
        assert_eq!(c.env.len(), 2);
        assert!(svc.iap_enabled);
        assert_eq!(c.volume_mounts[0].name, "cache");
        assert_eq!(c.volume_mounts[0].mount_path, "/mnt/cache");
        match &t.volumes[0].volume_type {
            Some(volume::VolumeType::Gcs(g)) => {
                assert_eq!(g.bucket, "my-cache");
                assert!(!g.read_only);
                assert_eq!(g.mount_options, ["implicit-dirs"]);
            }
            other => panic!("expected a GCS volume, got {other:?}"),
        }
        assert!(matches!(&c.env[0].values, Some(env_var::Values::Value(v)) if v == "info"));
        match &c.env[1].values {
            Some(env_var::Values::ValueSource(src)) => {
                let sel = src.secret_key_ref.as_ref().unwrap();
                assert_eq!(
                    (sel.secret.as_str(), sel.version.as_str()),
                    ("database-url", "3")
                );
            }
            other => panic!("expected secret ref, got {other:?}"),
        }
    }

    #[test]
    fn update_preserves_foreign_labels_and_annotations() {
        let existing = Service::new()
            .set_name("projects/p/locations/r/services/hello-dev")
            .set_etag("etag-1")
            .set_labels([
                ("team", "payments"),
                ("managed-by", "runway"),
                ("cloud.googleapis.com/location", "r"),
            ])
            .set_annotations([("example.com/owner", "alice"), ("runway.dev/stale", "x")]);
        let svc = desired_service(&spec(), &existing.name, Some(&existing));
        assert_eq!(svc.name, existing.name);
        assert_eq!(svc.etag, "etag-1");
        assert_eq!(svc.labels["team"], "payments");
        assert!(
            !svc.labels.contains_key("cloud.googleapis.com/location"),
            "reserved keys are not sent back"
        );
        assert_eq!(svc.annotations["example.com/owner"], "alice");
        assert!(!svc.annotations.contains_key("runway.dev/stale"));
        assert!(svc.annotations.contains_key(naming::ANNOTATION_IMAGE_REF));
    }

    #[test]
    fn health_check_becomes_http_probes_and_round_trips() {
        let mut s = spec();
        s.health_check = Some(crate::config::HealthCheck {
            path: "/healthz".into(),
            startup: crate::config::ProbeSettings::STARTUP,
            liveness: Some(crate::config::ProbeSettings::LIVENESS),
        });
        let svc = desired_service(&s, "n", None);
        let c = &svc.template.as_ref().unwrap().containers[0];
        let sp = c.startup_probe.as_ref().unwrap();
        assert_eq!(
            (sp.period_seconds, sp.timeout_seconds, sp.failure_threshold),
            (10, 3, 12)
        );
        assert!(
            matches!(&sp.probe_type, Some(probe::ProbeType::HttpGet(h)) if h.path == "/healthz")
        );
        assert_eq!(c.liveness_probe.as_ref().unwrap().period_seconds, 30);
        assert!(crate::plan::diff(&observed_flat(&svc), &s.flatten()).is_empty());

        // Cloud Run's default TCP startup probe on a live service is not a difference
        // for a configuration without health_check.
        let mut plain = spec();
        plain.health_check = None;
        let mut live = desired_service(&plain, "n", None);
        live.template.as_mut().unwrap().containers[0].startup_probe = Some(
            Probe::new()
                .set_tcp_socket(google_cloud_run_v2::model::TCPSocketAction::new().set_port(8080))
                .set_period_seconds(240)
                .set_failure_threshold(1),
        );
        assert!(crate::plan::diff(&observed_flat(&live), &plain.flatten()).is_empty());
    }

    #[test]
    fn otel_collector_sidecar_starts_first_and_round_trips() {
        let mut s = spec();
        s.otel_collector = Some(crate::config::OtelCollector {
            image: format!("{}:0.160.0", crate::config::OTEL_COLLECTOR_IMAGE),
            pinned: true,
            cpu: "1".into(),
            memory: "512Mi".into(),
            config: crate::config::OTEL_COLLECTOR_DEFAULT_CONFIG.into(),
        });
        let svc = desired_service(&s, "n", None);
        let cs = &svc.template.as_ref().unwrap().containers;
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].name, APP_CONTAINER);
        assert_eq!(
            cs[0].depends_on,
            ["otel-collector"],
            "app starts after the collector is healthy"
        );
        assert!(!cs[0].ports.is_empty(), "only the app receives traffic");
        let side = &cs[1];
        assert_eq!(side.name, "otel-collector");
        assert!(side.ports.is_empty());
        assert_eq!(side.args, ["--config=env:RUNWAY_OTELCOL_CONFIG"]);
        assert!(
            matches!(&side.env[0].values, Some(env_var::Values::Value(v)) if v.contains("googlemanagedprometheus"))
        );
        assert!(matches!(&side.startup_probe.as_ref().unwrap().probe_type,
            Some(probe::ProbeType::HttpGet(h)) if h.port == 13133));
        let obs = observed_flat(&svc);
        assert_eq!(obs["containers"], "2");
        assert!(
            crate::plan::diff(&obs, &s.flatten()).is_empty(),
            "{:?}",
            crate::plan::diff(&obs, &s.flatten())
        );

        // Removing the setting removes the sidecar.
        let mut without = s.clone();
        without.otel_collector = None;
        let changes = crate::plan::diff(&obs, &without.flatten());
        assert!(
            changes
                .iter()
                .any(|c| c.field == "sidecars.otel-collector" && c.after.is_none()),
            "{changes:?}"
        );
    }

    /// A revision as Cloud Run reports it, created from `t`.
    fn revision_of(t: &RevisionTemplate) -> google_cloud_run_v2::model::Revision {
        google_cloud_run_v2::model::Revision::new()
            .set_containers(t.containers.clone())
            .set_volumes(t.volumes.clone())
            .set_or_clear_vpc_access(t.vpc_access.clone())
            .set_execution_environment(t.execution_environment.clone())
            .set_service_account(&t.service_account)
            .set_max_instance_request_concurrency(t.max_instance_request_concurrency)
            .set_or_clear_timeout(t.timeout)
            .set_or_clear_scaling(t.scaling.clone())
            .set_annotations(t.annotations.clone())
    }

    #[test]
    fn revocations_wait_until_one_revision_serves_all_traffic() {
        use crate::traffic::{Entry, Target};
        let with = |entries: Vec<Entry>| {
            let mut svc = desired_service(&spec(), "n", None);
            svc.latest_ready_revision = "projects/p/locations/r/services/n/revisions/n-2".into();
            with_traffic(&svc, &entries)
        };
        assert!(one_revision_serves_all(&with(vec![
            Entry::new(Target::Latest, 100, ""),
            Entry::new(Target::Revision("n-1".into()), 0, "feat-a"),
        ])));
        assert!(
            !one_revision_serves_all(&with(vec![
                Entry::new(Target::Revision("n-1".into()), 90, ""),
                Entry::new(Target::Latest, 10, "canary"),
            ])),
            "the stable revision may still need a removed grant"
        );
    }

    #[test]
    fn a_revision_already_running_the_configuration_is_kept() {
        use crate::traffic::{Current, Entry, Mode, Target};
        let s = spec();
        let live = desired_service(&s, "n", None);
        let rev = revision_of(live.template.as_ref().unwrap());
        assert!(revision_matches(&live, &rev, &s), "same configuration");

        let mut env = s.clone();
        env.env.insert("NEW".into(), "1".into());
        assert!(!revision_matches(&live, &rev, &env), "env differs");
        let mut image = s.clone();
        image.image = "other@sha256:9".into();
        assert!(!revision_matches(&live, &rev, &image), "image differs");
        let mut pending = s.clone();
        pending.image = String::new();
        assert!(
            !revision_matches(&live, &rev, &pending),
            "image not built yet"
        );

        // A preview only reuses a revision carrying its own marker.
        let mode = Mode::Preview {
            tag: "feat-a".into(),
        };
        let mut preview = s.clone().with_traffic_mode(mode.clone());
        let (key, value) = revision_marker(&mode).unwrap();
        preview
            .revision_annotations
            .insert(key.into(), value.clone());
        assert!(
            !revision_matches(&live, &rev, &preview),
            "production revision"
        );
        let marked = rev.clone().set_annotations([(key, value)]);
        assert!(revision_matches(&live, &marked, &preview));

        // Which revision is behind the URL a mode changes.
        let traffic = Current {
            entries: vec![
                Entry::new(Target::Revision("n-1".into()), 100, ""),
                Entry::new(Target::Latest, 0, "feat-a"),
            ],
            latest_ready: "n-2".into(),
        };
        assert_eq!(
            mode_target(&traffic, &Mode::Full),
            Some((Target::Revision("n-1".into()), "n-1".into()))
        );
        assert_eq!(
            mode_target(&traffic, &mode),
            Some((Target::Latest, "n-2".into()))
        );
        assert_eq!(mode_target(&traffic, &Mode::Canary { percent: 10 }), None);
        let split = Current {
            entries: vec![
                Entry::new(Target::Revision("n-1".into()), 90, ""),
                Entry::new(Target::Latest, 10, "canary"),
            ],
            latest_ready: "n-2".into(),
        };
        assert_eq!(
            mode_target(&split, &Mode::Full),
            None,
            "two revisions serve"
        );

        // Kept revision: revision fields are left alone, service fields still apply.
        let mut kept = env.clone();
        kept.ingress = "internal".into();
        kept.traffic.serve = Some(Target::Latest);
        let fields: Vec<String> = pending_changes(&live, &kept)
            .into_iter()
            .map(|c| c.field)
            .collect();
        assert_eq!(fields, ["ingress"]);
        assert_eq!(
            desired_service(&kept, "n", Some(&live)).template,
            live.template,
            "template sent back as is: no new revision"
        );
    }

    #[test]
    fn preview_keeps_the_template_and_tags_the_latest_revision() {
        use crate::traffic::{Mode, Target};
        let s = spec();
        let mut live = desired_service(&s, "n", None);
        live.latest_ready_revision =
            "projects/p/locations/r/services/n/revisions/n-00001-abc".into();
        live.template.as_mut().unwrap().revision = String::new();

        // Same template, preview mode: only traffic changes, template sent back as is.
        let preview = s
            .clone()
            .with_traffic_mode(Mode::Preview {
                tag: "feat-a".into(),
            })
            .with_current_traffic(Some(&current_traffic(&live)));
        let req = desired_service(&preview, "n", Some(&live));
        assert_eq!(
            req.template, live.template,
            "no new revision for a traffic-only change"
        );
        let cur = current_traffic(&req);
        assert_eq!(cur.entries.len(), 2);
        assert_eq!(
            cur.entries[0].target,
            Target::Revision("n-00001-abc".into())
        );
        assert_eq!(cur.entries[0].percent, 100);
        assert_eq!(
            (cur.entries[1].target.clone(), cur.entries[1].tag.as_str()),
            (Target::Latest, "feat-a")
        );
        let changes = crate::plan::diff(&observed_flat(&live), &preview.flatten());
        assert!(
            changes.iter().any(|c| c.field == "traffic.tags.feat-a"),
            "{changes:?}"
        );
        assert!(changes.iter().any(|c| c.field == "traffic"), "{changes:?}");

        // A template change builds a new template.
        let mut changed = preview.clone();
        changed.env.insert("NEW".into(), "1".into());
        let req = desired_service(&changed, "n", Some(&live));
        assert_ne!(req.template, live.template);

        // Ingress alone is service-level: template reused.
        let mut ingress = s.clone();
        ingress.ingress = "internal".into();
        let req = desired_service(&ingress, "n", Some(&live));
        assert_eq!(req.template, live.template);
    }

    #[test]
    fn secret_files_are_secret_volumes_and_round_trip() {
        let mut s = spec();
        s.secrets.insert(
            "tls".into(),
            SecretRef {
                secret: "tls-cert".into(),
                version: "latest".into(),
                path: Some("/secrets/tls/cert.pem".into()),
                ..Default::default()
            },
        );
        s.secrets.insert(
            "API_KEY".into(),
            SecretRef {
                secret: "api-key".into(),
                version: "4".into(),
                pin_latest: true,
                ..Default::default()
            },
        );
        let svc = desired_service(&s, "n", None);
        let t = svc.template.as_ref().unwrap();
        let c = &t.containers[0];
        assert!(c.env.iter().any(|e| e.name == "API_KEY"));
        assert!(
            !c.env.iter().any(|e| e.name == "tls"),
            "files are not env vars"
        );
        let vol = t.volumes.iter().find(|v| v.name == "secret-tls").unwrap();
        match &vol.volume_type {
            Some(volume::VolumeType::Secret(sv)) => {
                assert_eq!(sv.secret, "tls-cert");
                assert_eq!(sv.items[0].path, "cert.pem");
                assert_eq!(sv.items[0].version, "latest");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            c.volume_mounts
                .iter()
                .any(|m| m.name == "secret-tls" && m.mount_path == "/secrets/tls")
        );
        let obs = observed_flat(&svc);
        assert_eq!(obs["secret_files./secrets/tls/cert.pem"], "tls-cert@latest");
        assert_eq!(obs["secrets.API_KEY"], "api-key@4");
        assert!(
            crate::plan::diff(&obs, &s.flatten()).is_empty(),
            "{:?}",
            crate::plan::diff(&obs, &s.flatten())
        );
        // A new value (resolved version) is a template change: new revision.
        let mut newer = s.clone();
        newer.secrets.get_mut("API_KEY").unwrap().version = "5".into();
        let ch = crate::plan::diff(&obs, &newer.flatten());
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].field, "secrets.API_KEY");
    }

    #[test]
    fn identical_preview_gets_its_own_revision_and_lines_stay_separate() {
        use crate::traffic::Mode;
        let s = spec();
        let mut live = desired_service(&s, "n", None);
        live.latest_ready_revision =
            "projects/p/locations/r/services/n/revisions/n-00001-abc".into();
        let mut preview = s
            .clone()
            .with_traffic_mode(Mode::Preview {
                tag: "feat-a".into(),
            })
            .with_current_traffic(Some(&current_traffic(&live)));
        preview
            .revision_annotations
            .insert("runway.dev/preview".into(), "feat-a".into());
        let req = desired_service(&preview, "n", Some(&live));
        let t = req.template.as_ref().unwrap();
        assert_ne!(
            req.template, live.template,
            "a new template: Cloud Run creates a revision"
        );
        assert_eq!(t.annotations["runway.dev/preview"], "feat-a");

        // After the rollout: production pinned at 100%, the preview at 0%.
        let mut after = req.clone();
        after.latest_ready_revision =
            "projects/p/locations/r/services/n/revisions/n-00002-def".into();
        let lines = status(&after, "a", "s").traffic;
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            (
                lines[0].percent,
                lines[0].revision.as_str(),
                lines[0].tag.as_str()
            ),
            (100, "n-00001-abc", "")
        );
        assert_eq!(
            (
                lines[1].percent,
                lines[1].revision.as_str(),
                lines[1].tag.as_str()
            ),
            (0, "n-00002-def (latest)", "feat-a")
        );
    }

    #[test]
    fn generic_sidecars_build_and_round_trip() {
        use crate::config::{SidecarCheck, SidecarConfig};
        let mut s = spec();
        s.volumes.insert(
            "cache".into(),
            crate::config::VolumeConfig {
                bucket: "b".into(),
                mount_path: "/mnt/cache".into(),
                read_only: false,
                mount_options: vec![],
            },
        );
        let proxy = SidecarConfig {
            image: "envoyproxy/envoy:v1.31.0".into(),
            cpu: "0.5".into(),
            memory: "256Mi".into(),
            command: vec!["envoy".into()],
            args: vec!["-c".into(), "/etc/envoy.yaml".into()],
            env: [("LOG_LEVEL".to_string(), "debug".to_string())].into(),
            secrets: [(
                "TOKEN".to_string(),
                SecretRef {
                    secret: "proxy-token".into(),
                    version: "3".into(),
                    ..Default::default()
                },
            )]
            .into(),
            health_check: Some(SidecarCheck {
                port: 9901,
                path: Some("/ready".into()),
            }),
            start_before_app: true,
            volumes: [("cache".to_string(), "/cache".to_string())].into(),
        };
        let sql = SidecarConfig {
            image: "gcr.io/cloud-sql-connectors/cloud-sql-proxy:2.14.0".into(),
            cpu: "1".into(),
            memory: "512Mi".into(),
            command: vec![],
            args: vec![],
            env: Default::default(),
            secrets: Default::default(),
            health_check: Some(SidecarCheck {
                port: 5432,
                path: None,
            }),
            start_before_app: false,
            volumes: Default::default(),
        };
        s.sidecars.insert("proxy".into(), proxy);
        s.sidecars.insert("sql".into(), sql);

        let svc = desired_service(&s, "n", None);
        let cs = &svc.template.as_ref().unwrap().containers;
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].name, APP_CONTAINER);
        assert_eq!(
            cs[0].depends_on,
            ["proxy"],
            "only sidecars that start before the app"
        );
        let p = cs.iter().find(|c| c.name == "proxy").unwrap();
        assert!(p.ports.is_empty());
        assert_eq!(p.command, ["envoy"]);
        assert!(p.env.iter().any(
            |e| e.name == "TOKEN" && matches!(&e.values, Some(env_var::Values::ValueSource(_)))
        ));
        assert_eq!(p.volume_mounts[0].mount_path, "/cache");
        assert!(matches!(&p.startup_probe.as_ref().unwrap().probe_type,
            Some(probe::ProbeType::HttpGet(h)) if h.port == 9901 && h.path == "/ready"));
        let q = cs.iter().find(|c| c.name == "sql").unwrap();
        assert!(matches!(&q.startup_probe.as_ref().unwrap().probe_type,
            Some(probe::ProbeType::TcpSocket(t)) if t.port == 5432));

        let obs = observed_flat(&svc);
        assert_eq!(obs["containers"], "3");
        let d = crate::plan::diff(&obs, &s.flatten());
        assert!(d.is_empty(), "{d:?}");

        // Any change to a sidecar shows up on its own line.
        let mut changed = s.clone();
        changed.sidecars.get_mut("sql").unwrap().start_before_app = true;
        let d = crate::plan::diff(&obs, &changed.flatten());
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].field, "sidecars.sql");
        let mut removed = s.clone();
        removed.sidecars.remove("sql");
        let d = crate::plan::diff(&obs, &removed.flatten());
        assert!(
            d.iter()
                .any(|c| c.field == "sidecars.sql" && c.after.is_none()),
            "{d:?}"
        );
    }

    #[test]
    fn plan_and_request_agree_on_kept_annotations() {
        let mut s = spec();
        s.annotations
            .insert("runway.dev/image-ref".into(), "img:1".into());
        let mut live = desired_service(&s, "n", None);
        live.annotations
            .insert("runway.dev/release".into(), "v1.2.0".into());
        // Same image, a configuration change: kept in both plan and request.
        let mut changed = s.clone();
        changed.env.insert("NEW".into(), "1".into());
        let for_live = spec_for_live(&changed, Some(&live));
        let changes = crate::plan::diff(&observed_flat(&live), &for_live.flatten());
        assert!(
            !changes.iter().any(|c| c.field.starts_with("annotations.")),
            "{changes:?}"
        );
        let req = desired_service(&for_live, "n", Some(&live));
        assert_eq!(req.annotations["runway.dev/release"], "v1.2.0");
        // A different image: dropped, and the plan says so.
        let mut new_image = s.clone();
        new_image.image = "img@sha256:2".into();
        let for_live = spec_for_live(&new_image, Some(&live));
        let changes = crate::plan::diff(&observed_flat(&live), &for_live.flatten());
        assert!(
            changes
                .iter()
                .any(|c| c.field == "annotations.runway.dev/release" && c.after.is_none()),
            "{changes:?}"
        );
        let req = desired_service(&for_live, "n", Some(&live));
        assert!(!req.annotations.contains_key("runway.dev/release"));
    }

    #[test]
    fn ingress_maps_to_the_api_enum() {
        for (v, want) in [
            ("all", IngressTraffic::All),
            ("internal", IngressTraffic::InternalOnly),
            (
                "internal-and-cloud-load-balancing",
                IngressTraffic::InternalLoadBalancer,
            ),
        ] {
            let mut s = spec();
            s.ingress = v.into();
            let svc = desired_service(&s, "n", None);
            assert_eq!(svc.ingress, want);
            assert_eq!(
                observed_flat(&svc)["ingress"],
                v,
                "observed state round-trips"
            );
        }
    }

    #[test]
    fn update_mask_covers_managed_fields_only() {
        let m = update_mask();
        assert!(m.paths.contains(&"template".to_string()));
        assert!(m.paths.contains(&"traffic".to_string()));
        assert!(!m.paths.contains(&"description".to_string()));
    }

    #[test]
    fn observed_state_round_trips_without_spurious_diff() {
        let s = spec();
        let mut svc = desired_service(&s, "n", None);
        // The API reports equivalent values in different spellings.
        if let Some(t) = svc.template.as_mut() {
            t.containers[0].resources = Some(
                ResourceRequirements::new()
                    .set_limits([("cpu", "2000m"), ("memory", "1024Mi")])
                    .set_cpu_idle(true),
            );
        }
        let observed = observed_flat(&svc);
        let changes = crate::plan::diff(&observed, &s.flatten());
        assert!(changes.is_empty(), "{changes:?}");
    }

    fn networked_spec() -> ServiceSpec {
        let mut s = spec();
        s.billing = crate::config::BILLING_INSTANCE.into();
        s.startup_cpu_boost = true;
        s.execution_environment = Some("gen2".into());
        // Full names, as configuration resolution produces them.
        s.vpc = Some(crate::config::VpcConfig {
            network: "projects/p/global/networks/default".into(),
            subnet: "projects/p/regions/europe-west1/subnetworks/run".into(),
            egress: "all-traffic".into(),
            network_tags: vec!["b".into(), "a".into()],
        });
        s.cloud_sql = vec!["p:europe-west1:db".into()];
        s.custom_audiences = vec!["https://api.example.com".into()];
        s.sidecars.insert(
            "proxy".into(),
            crate::config::SidecarConfig {
                image: "nginx".into(),
                cpu: "1".into(),
                memory: "512Mi".into(),
                command: vec![],
                args: vec![],
                env: Default::default(),
                secrets: Default::default(),
                health_check: None,
                start_before_app: false,
                volumes: Default::default(),
            },
        );
        s
    }

    #[test]
    fn networking_billing_and_cloud_sql_reach_the_request() {
        let s = networked_spec();
        let svc = desired_service(&s, "n", None);
        assert_eq!(svc.custom_audiences, ["https://api.example.com"]);
        let t = svc.template.clone().unwrap();
        assert_eq!(t.execution_environment, ExecutionEnvironment::Gen2);
        let va = t.vpc_access.clone().unwrap();
        assert_eq!(va.egress, vpc_access::VpcEgress::AllTraffic);
        assert_eq!(
            va.network_interfaces[0].network,
            "projects/p/global/networks/default"
        );
        assert_eq!(
            va.network_interfaces[0].subnetwork,
            "projects/p/regions/europe-west1/subnetworks/run"
        );
        // Every container: billing is per revision, set on each of them.
        for c in &t.containers {
            let r = c.resources.as_ref().unwrap();
            assert!(!r.cpu_idle, "instance-based: {}", c.name);
            assert!(r.startup_cpu_boost, "{}", c.name);
        }
        let sql = t
            .volumes
            .iter()
            .find(|v| v.name == crate::config::CLOUD_SQL_VOLUME)
            .unwrap();
        assert!(matches!(&sql.volume_type,
            Some(volume::VolumeType::CloudSqlInstance(c)) if c.instances == ["p:europe-west1:db"]));
        let app = t.containers.iter().find(|c| !c.ports.is_empty()).unwrap();
        assert!(
            app.volume_mounts
                .iter()
                .any(|m| m.name == "cloudsql" && m.mount_path == "/cloudsql")
        );

        // What runway sent reads back as the same configuration.
        let changes = crate::plan::diff(&observed_flat(&svc), &s.flatten());
        assert!(changes.is_empty(), "{changes:?}");

        // Request-based billing is sent explicitly (cpu_idle), not left out.
        let plain = desired_service(&spec(), "n", None);
        assert!(
            plain.template.unwrap().containers[0]
                .resources
                .as_ref()
                .unwrap()
                .cpu_idle
        );
    }

    #[test]
    fn services_without_cpu_idle_show_instance_based_billing() {
        // Deployed by runway before billing was managed: resources without
        // cpu_idle, which Cloud Run reads as CPU always allocated.
        let s = spec();
        let mut svc = desired_service(&s, "n", None);
        if let Some(t) = svc.template.as_mut() {
            t.containers[0].resources =
                Some(ResourceRequirements::new().set_limits([("cpu", "2"), ("memory", "1Gi")]));
        }
        let changes = crate::plan::diff(&observed_flat(&svc), &s.flatten());
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].field, "billing");
        assert_eq!(changes[0].before.as_deref(), Some("instance-based"));
        assert_eq!(changes[0].after.as_deref(), Some("request-based"));
    }

    #[test]
    fn audiences_alone_never_create_a_revision() {
        let s = networked_spec();
        let live = desired_service(&s, "n", None);
        let mut changed = s.clone();
        changed.custom_audiences = vec!["https://other.example.com".into()];
        let changes = crate::plan::diff(&observed_flat(&live), &changed.flatten());
        assert_eq!(changes.len(), 1);
        assert!(is_service_level(&changes[0].field));
        let req = desired_service(&changed, "n", Some(&live));
        assert_eq!(req.template, live.template, "template sent back untouched");
        assert_eq!(req.custom_audiences, ["https://other.example.com"]);
    }

    #[test]
    fn moving_to_another_shared_vpc_host_is_a_change() {
        let mut a = networked_spec();
        let mut b = networked_spec();
        let vpc = |host: &str| crate::config::VpcConfig {
            network: format!("projects/{host}/global/networks/shared"),
            subnet: format!("projects/{host}/regions/europe-west1/subnetworks/run"),
            egress: "private-ranges-only".into(),
            network_tags: vec![],
        };
        a.vpc = Some(vpc("host-a"));
        b.vpc = Some(vpc("host-b"));
        let live = desired_service(&a, "projects/p/locations/europe-west1/services/n", None);
        let changes = crate::plan::diff(&observed_flat(&live), &b.flatten());
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].field, "vpc");
    }

    #[test]
    fn a_revision_on_the_environment_cloud_run_chose_is_kept() {
        let s = spec();
        assert_eq!(s.execution_environment, None, "Cloud Run chooses");
        let live = desired_service(&s, "n", None);
        // A revision reports the environment it runs on.
        let rev = revision_of(live.template.as_ref().unwrap())
            .set_execution_environment(ExecutionEnvironment::Gen2);
        assert!(revision_matches(&live, &rev, &s));

        let mut gen1 = s.clone();
        gen1.execution_environment = Some("gen1".into());
        assert!(
            !revision_matches(&live, &rev, &gen1),
            "a configured environment counts"
        );
    }

    #[test]
    fn a_short_name_set_outside_runway_is_in_the_services_project() {
        let s = networked_spec();
        let mut svc = desired_service(&s, "n", None);
        svc.name = "projects/p/locations/europe-west1/services/n".into();
        if let Some(va) = svc.template.as_mut().and_then(|t| t.vpc_access.as_mut()) {
            va.network_interfaces[0].network = "default".into();
            va.network_interfaces[0].subnetwork = "run".into();
        }
        let changes = crate::plan::diff(&observed_flat(&svc), &s.flatten());
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn sandboxes_are_enabled_on_the_app_container_only() {
        let mut s = networked_spec();
        s.sandbox = true;
        let svc = desired_service(&s, "n", None);
        assert_eq!(svc.launch_stage, LaunchStage::Beta, "a preview feature");
        let t = svc.template.clone().unwrap();
        let json = |c: &Container| serde_json::to_value(c).unwrap();
        assert_eq!(json(&t.containers[0])["sandboxLauncher"], true);
        assert_eq!(t.containers[0].name, APP_CONTAINER);
        assert!(
            t.containers[1..]
                .iter()
                .all(|c| json(c).get("sandboxLauncher").is_none())
        );

        let changes = crate::plan::diff(&observed_flat(&svc), &s.flatten());
        assert!(changes.is_empty(), "{changes:?}");
        let rev = revision_of(&t);
        assert!(
            revision_matches(&svc, &rev, &s),
            "the field survives a revision read"
        );

        let mut off = s.clone();
        off.sandbox = false;
        let changes = crate::plan::diff(&observed_flat(&svc), &off.flatten());
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].field, "sandbox");
        assert!(!revision_matches(&svc, &rev, &off));
        // Turned off, the live launch stage is kept.
        assert_eq!(
            desired_service(&off, "n", Some(&svc)).launch_stage,
            LaunchStage::Beta
        );
        assert_eq!(
            desired_service(&off, "n", None).launch_stage,
            LaunchStage::default()
        );
    }

    #[test]
    fn a_sandbox_launcher_reported_by_cloud_run_is_read() {
        let svc: Service = serde_json::from_value(serde_json::json!({
            "launchStage": "BETA",
            "template": {"containers": [{
                "image": "europe-west1-docker.pkg.dev/p/apps/hello@sha256:abc",
                "sandboxLauncher": true
            }]}
        }))
        .unwrap();
        assert_eq!(observed_flat(&svc)["sandbox"], "enabled");
        assert_eq!(svc.launch_stage, LaunchStage::Beta);
    }

    #[test]
    fn ownership_detection() {
        let owned = Service::new().set_labels(naming::ownership_labels("hello", "dev"));
        assert_eq!(ownership(&owned, "hello", "dev"), Ownership::Owned);
        assert_eq!(
            ownership(&owned, "hello", "prod"),
            Ownership::OtherOwner {
                app: "hello".into(),
                stage: "dev".into()
            }
        );
        assert_eq!(
            ownership(&Service::new(), "hello", "dev"),
            Ownership::Unmanaged
        );
        let foreign = Service::new().set_labels([("team", "x")]);
        assert_eq!(ownership(&foreign, "hello", "dev"), Ownership::Unmanaged);
    }

    fn cond(state: condition::State, msg: &str) -> Condition {
        Condition::new()
            .set_type("Ready")
            .set_state(state)
            .set_message(msg)
    }

    #[test]
    fn readiness_states() {
        let ready = Service::new()
            .set_generation(2)
            .set_observed_generation(2)
            .set_latest_created_revision("r-2")
            .set_latest_ready_revision("r-2")
            .set_terminal_condition(cond(condition::State::ConditionSucceeded, ""));
        assert_eq!(readiness(&ready), Readiness::Ready);

        let reconciling = ready.clone().set_reconciling(true);
        assert!(matches!(
            readiness(&reconciling),
            Readiness::Reconciling { .. }
        ));

        let failed = ready
            .clone()
            .set_latest_ready_revision("r-1")
            .set_terminal_condition(cond(
                condition::State::ConditionFailed,
                "Revision 'r-2' is not ready and cannot serve traffic. The user-provided container failed to start and listen on the port defined provided by the PORT=8080 environment variable.",
            ));
        match readiness(&failed) {
            Readiness::Failed { message } => {
                assert!(message.contains("failed to start and listen"))
            }
            other => panic!("{other:?}"),
        }

        let stale = ready.clone().set_latest_ready_revision("r-1");
        assert!(matches!(readiness(&stale), Readiness::Failed { .. }));
    }
}
