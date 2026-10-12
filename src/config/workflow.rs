//! Google Cloud Workflows: resolution of `workflows`, `workflow_defaults`
//! and their stage overrides, and the templates that carry references to
//! the stage's services and jobs.
//!
//! References use the same names in two places: `${services.api.url}` in an
//! `env` value of runway.yaml, and `${runway.services.api.url}` inside the
//! workflow file (any other `${…}` there is a Workflows expression and is
//! left alone). A service URL is only known once the service exists, so it
//! stays a placeholder until deploy reads it.

use super::schema::{RawConfig, RawStage, RawWorkflow};
use super::{Diagnostics, IdentityConfig, WorkloadInput, interp, resolve_roles, validate};
use crate::naming;
use serde::Serialize;
use std::collections::BTreeMap;

/// Workflows' limit on a workflow's source.
pub const MAX_SOURCE_BYTES: usize = 128 * 1024;
/// Workflows' limits on user-defined environment variables.
pub const MAX_ENV_VARS: usize = 20;
pub const MAX_ENV_DEFINITION_BYTES: usize = 4096;
/// What marks a runway reference inside a workflow file.
const INLINE_PREFIX: &str = "${runway.";

/// A workflow of the stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkflowConfig {
    pub key: String,
    /// Workflow ID: `{app}-{key}-{stage}`.
    pub id: String,
    pub region: String,
    /// The file, as written in runway.yaml (relative to it).
    pub source_file: String,
    pub source: Template,
    pub description: String,
    pub env: BTreeMap<String, Template>,
    pub call_log_level: CallLogLevel,
    pub service_account: String,
    pub identity: IdentityConfig,
    /// Granted `roles/workflows.invoker` on the project.
    pub invokers: Vec<String>,
    /// The services and jobs it references (its account may call them).
    pub targets: Vec<WorkflowTarget>,
}

impl WorkflowConfig {
    pub fn name(&self, project: &str) -> String {
        format!(
            "projects/{project}/locations/{}/workflows/{}",
            self.region, self.id
        )
    }
    /// Service IDs whose URL the workflow needs.
    pub fn service_urls(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for t in std::iter::once(&self.source).chain(self.env.values()) {
            for id in t.service_urls() {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }
    /// Whether it references this service or job ID.
    pub fn references(&self, resource_id: &str) -> bool {
        self.targets.iter().any(|t| t.resource_id() == resource_id)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallLogLevel {
    #[default]
    None,
    ErrorsOnly,
    AllCalls,
}

impl CallLogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            CallLogLevel::None => "none",
            CallLogLevel::ErrorsOnly => "errors-only",
            CallLogLevel::AllCalls => "all-calls",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(CallLogLevel::None),
            "errors-only" => Some(CallLogLevel::ErrorsOnly),
            "all-calls" => Some(CallLogLevel::AllCalls),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTarget {
    Service { name: String, service_id: String },
    Job { name: String, job_id: String },
}

impl WorkflowTarget {
    pub fn resource_id(&self) -> &str {
        match self {
            WorkflowTarget::Service { service_id, .. } => service_id,
            WorkflowTarget::Job { job_id, .. } => job_id,
        }
    }
}

/// Text with the URLs of services left to fill in at deploy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Template(pub Vec<TemplatePart>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TemplatePart {
    Text(String),
    /// The URL of this service (its ID).
    ServiceUrl(String),
}

impl Template {
    pub fn text(s: impl Into<String>) -> Self {
        Template(vec![TemplatePart::Text(s.into())])
    }
    fn push_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        match self.0.last_mut() {
            Some(TemplatePart::Text(t)) => t.push_str(s),
            _ => self.0.push(TemplatePart::Text(s.to_string())),
        }
    }
    pub fn service_urls(&self) -> impl Iterator<Item = &str> {
        self.0.iter().filter_map(|p| match p {
            TemplatePart::ServiceUrl(id) => Some(id.as_str()),
            TemplatePart::Text(_) => None,
        })
    }
    /// The text with every URL filled in; `Err(service ID)` for the first
    /// service `url` does not know.
    pub fn render(&self, url: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
        let mut out = String::new();
        for p in &self.0 {
            match p {
                TemplatePart::Text(t) => out.push_str(t),
                TemplatePart::ServiceUrl(id) => {
                    out.push_str(&url(id).ok_or_else(|| id.clone())?);
                }
            }
        }
        Ok(out)
    }
    /// For explanations: URLs shown as `<URL of ID>`.
    pub fn display(&self) -> String {
        self.render(&|id| Some(format!("<URL of {id}>")))
            .unwrap_or_default()
    }
    /// Length without the URLs (a lower bound of the rendered length).
    fn literal_len(&self) -> usize {
        self.0
            .iter()
            .map(|p| match p {
                TemplatePart::Text(t) => t.len(),
                TemplatePart::ServiceUrl(_) => 0,
            })
            .sum()
    }
}

/// What references resolve against.
pub(super) struct Refs<'a> {
    pub app: &'a str,
    pub stage: &'a str,
    pub project: &'a str,
    pub region: &'a str,
    pub ix: &'a interp::Interp,
    pub inputs: &'a [WorkloadInput],
}

impl Refs<'_> {
    fn workload(&self, name: &str, job: bool) -> Option<Option<&str>> {
        let key = match (job, name == self.app) {
            (false, true) => None,
            _ => Some(name),
        };
        self.inputs
            .iter()
            .find(|w| w.job.is_some() == job && w.key.as_deref() == key)
            .map(|w| w.key.as_deref())
    }

    /// One reference, and the service or job it names.
    fn resolve(&self, name: &str) -> Result<(TemplatePart, Option<WorkflowTarget>), String> {
        for (prefix, job) in [("services.", false), ("jobs.", true)] {
            let Some(rest) = name.strip_prefix(prefix) else {
                continue;
            };
            let what = if job { "job" } else { "service" };
            let fields = if job {
                "`id`, `name` or `url`"
            } else {
                "`url`, `id` or `name`"
            };
            let (w, field) = rest.rsplit_once('.').ok_or_else(|| {
                format!(
                    "`${{{name}}}`: add the field ({fields}), e.g. `${{{prefix}NAME.{}}}`",
                    if job { "name" } else { "url" }
                )
            })?;
            let key = self.workload(w, job).ok_or_else(|| {
                let names: Vec<String> = self
                    .inputs
                    .iter()
                    .filter(|x| x.job.is_some() == job)
                    .map(|x| x.key.clone().unwrap_or_else(|| self.app.to_string()))
                    .collect();
                format!(
                    "`${{{name}}}`: `{w}` is not a {what} of stage `{}` ({})",
                    self.stage,
                    if names.is_empty() {
                        format!("it has no {what}s")
                    } else {
                        format!("{what}s: {}", names.join(", "))
                    }
                )
            })?;
            let id = naming::workload_id(self.app, key, self.stage);
            let full = format!(
                "projects/{}/locations/{}/{}/{id}",
                self.project,
                self.region,
                if job { "jobs" } else { "services" }
            );
            let part = match (job, field) {
                (false, "url") => TemplatePart::ServiceUrl(id.clone()),
                (_, "id") => TemplatePart::Text(id.clone()),
                (_, "name") => TemplatePart::Text(full.clone()),
                (true, "url") => {
                    TemplatePart::Text(format!("https://run.googleapis.com/v2/{full}:run"))
                }
                _ => {
                    return Err(format!(
                        "`${{{name}}}`: a {what} has {fields}, not `{field}`"
                    ));
                }
            };
            let target = match job {
                false => WorkflowTarget::Service {
                    name: w.to_string(),
                    service_id: id,
                },
                true => WorkflowTarget::Job {
                    name: w.to_string(),
                    job_id: id,
                },
            };
            return Ok((part, Some(target)));
        }
        match self.ix.get(name) {
            Some(v) => Ok((TemplatePart::Text(v.to_string()), None)),
            None => {
                let mut known = self.ix.known();
                known.extend(
                    [
                        "${services.NAME.url}",
                        "${services.NAME.id}",
                        "${services.NAME.name}",
                    ]
                    .map(String::from),
                );
                known.extend(
                    ["${jobs.NAME.name}", "${jobs.NAME.id}", "${jobs.NAME.url}"].map(String::from),
                );
                Err(format!(
                    "unknown variable `${{{name}}}` (available: {})",
                    known.join(", ")
                ))
            }
        }
    }

    /// A runway.yaml value: every `${…}` is a reference, `$$` a literal `$`.
    pub fn value(&self, s: &str, targets: &mut Vec<WorkflowTarget>) -> Result<Template, String> {
        let mut out = Template::default();
        let mut rest = s;
        while let Some(i) = rest.find('$') {
            out.push_text(&rest[..i]);
            let after = &rest[i + 1..];
            if let Some(r) = after.strip_prefix('$') {
                out.push_text("$");
                rest = r;
            } else if let Some(r) = after.strip_prefix('{') {
                let end = r
                    .find('}')
                    .ok_or_else(|| format!("unterminated `${{` in `{s}`"))?;
                self.add(&mut out, r[..end].trim(), targets)?;
                rest = &r[end + 1..];
            } else {
                out.push_text("$");
                rest = after;
            }
        }
        out.push_text(rest);
        Ok(out)
    }

    /// A workflow file: only `${runway.…}` is runway's; errors give the line.
    pub fn source(&self, s: &str, targets: &mut Vec<WorkflowTarget>) -> Result<Template, String> {
        let mut out = Template::default();
        let mut rest = s;
        while let Some(i) = rest.find(INLINE_PREFIX) {
            out.push_text(&rest[..i]);
            let line = s[..s.len() - rest.len() + i].matches('\n').count() + 1;
            let r = &rest[i + INLINE_PREFIX.len()..];
            let end = r
                .find('}')
                .ok_or_else(|| format!("line {line}: unterminated `{INLINE_PREFIX}`"))?;
            self.add(&mut out, r[..end].trim(), targets)
                .map_err(|e| format!("line {line}: {}", e.replace("`${", "`${runway.")))?;
            rest = &r[end + 1..];
        }
        out.push_text(rest);
        Ok(out)
    }

    fn add(
        &self,
        out: &mut Template,
        name: &str,
        targets: &mut Vec<WorkflowTarget>,
    ) -> Result<(), String> {
        let (part, target) = self.resolve(name)?;
        match part {
            TemplatePart::Text(t) => out.push_text(&t),
            url => out.0.push(url),
        }
        if let Some(t) = target
            && !targets.contains(&t)
        {
            targets.push(t);
        }
        Ok(())
    }
}

/// The value of the highest layer that sets it, with its YAML path.
fn pick<T: Clone>(
    layers: &[(&RawWorkflow, String)],
    f: impl Fn(&RawWorkflow) -> &Option<T>,
    field: &str,
) -> Option<(T, String)> {
    layers
        .iter()
        .rev()
        .find_map(|(l, p)| f(l).as_ref().map(|v| (v.clone(), format!("{p}.{field}"))))
}

/// The workflows of a stage and the default Workflows region (where those
/// left after their removal from runway.yaml are looked for).
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_workflows(
    d: &mut Diagnostics,
    dir: &std::path::Path,
    raw: &RawConfig,
    st: &RawStage,
    stage: &str,
    project: &str,
    region: &str,
    ix: &interp::Interp,
    inputs: &[WorkloadInput],
) -> (Vec<WorkflowConfig>, String) {
    let refs = Refs {
        app: &raw.app,
        stage,
        project,
        region,
        ix,
        inputs,
    };
    let ixs = |d: &mut Diagnostics, v: &str, path: &str| -> String {
        ix.apply(v).unwrap_or_else(|e| {
            d.error(path, e);
            v.to_string()
        })
    };
    let empty = RawWorkflow::default();
    let mut defaults: Vec<(&RawWorkflow, String)> = Vec::new();
    if let Some(w) = &raw.workflow_defaults {
        defaults.push((w, "workflow_defaults".into()));
    }
    if let Some(w) = &st.workflow_defaults {
        defaults.push((w, format!("stages.{stage}.workflow_defaults")));
    }
    for (w, p) in &defaults {
        for (set, field) in [
            (w.source.is_some(), "source"),
            (w.description.is_some(), "description"),
        ] {
            if set {
                d.error(
                    format!("{p}.{field}"),
                    "set it on each workflow, not in `workflow_defaults`",
                );
            }
        }
    }
    let default_region = match pick(&defaults, |w| &w.region, "region") {
        Some((r, p)) => {
            if let Err(e) = validate::region(&r) {
                d.error(p, e);
            }
            r
        }
        None => region.to_string(),
    };

    let mut keys: Vec<&String> = raw.workflows.keys().collect();
    for (k, v) in &st.workflows {
        if v.is_some() && !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys.sort();
    let mut out = Vec::new();
    for key in keys {
        if matches!(st.workflows.get(key), Some(None)) {
            continue;
        }
        let mut layers = defaults.clone();
        let base = format!("workflows.{key}");
        if raw.workflows.contains_key(key) {
            layers.push((raw.workflows[key].as_ref().unwrap_or(&empty), base.clone()));
        }
        if let Some(Some(w)) = st.workflows.get(key) {
            layers.push((w, format!("stages.{stage}.workflows.{key}")));
        }
        if let Err(e) = validate::name_component("workflow name", key, 30) {
            d.error(base.clone(), e);
        }
        let id = format!("{}-{key}-{stage}", raw.app);
        if id.len() > 64 {
            d.error(
                base.clone(),
                format!("the workflow ID `{id}` is longer than 64 characters; use a shorter name"),
            );
        }
        let mut targets = Vec::new();

        // ---- source ----
        let (source_file, source) = match pick(&layers, |w| &w.source, "source") {
            None => {
                d.error(
                    format!("{base}.source"),
                    "the workflow definition is required (a YAML or JSON file, relative to runway.yaml)",
                );
                (String::new(), Template::default())
            }
            Some((file, path)) => {
                let file = ixs(d, &file, &path);
                let full = dir.join(&file);
                match std::fs::read_to_string(&full) {
                    Err(e) => {
                        d.error(path, format!("cannot read `{}`: {e}", full.display()));
                        (file, Template::default())
                    }
                    Ok(text) => {
                        if text.len() > MAX_SOURCE_BYTES {
                            d.error(
                                path.clone(),
                                format!(
                                    "`{file}` is {} KiB; Workflows accepts up to 128 KiB (split it into subworkflows)",
                                    text.len() / 1024
                                ),
                            );
                        }
                        if let Err(e) = serde_saphyr::from_str::<serde_json::Value>(&text) {
                            d.error(
                                path.clone(),
                                format!("`{file}` is not valid YAML or JSON: {e}"),
                            );
                        }
                        let t = refs.source(&text, &mut targets).unwrap_or_else(|e| {
                            d.error(path.clone(), format!("`{file}`, {e}"));
                            Template::default()
                        });
                        (file, t)
                    }
                }
            }
        };
        let description = pick(&layers, |w| &w.description, "description")
            .map(|(v, p)| ixs(d, &v, &p))
            .unwrap_or_default();

        // ---- env ----
        let mut env_raw: BTreeMap<String, (String, String)> = BTreeMap::new();
        for (i, (l, p)) in layers.iter().enumerate() {
            for (k, v) in l.env.iter().flatten() {
                let path = format!("{p}.env.{k}");
                match v {
                    Some(v) => {
                        env_raw.insert(k.clone(), (v.clone(), path));
                    }
                    None if i == 0 || !p.starts_with("stages.") => d.error(
                        path,
                        "value must not be null (null is only meaningful in stage overrides, to remove an inherited key)",
                    ),
                    None => {
                        env_raw.remove(k);
                    }
                }
            }
        }
        if env_raw.len() > MAX_ENV_VARS {
            d.error(
                format!("{base}.env"),
                format!(
                    "{} variables; Workflows allows at most {MAX_ENV_VARS}",
                    env_raw.len()
                ),
            );
        }
        let mut env = BTreeMap::new();
        for (k, (v, path)) in env_raw {
            let named = k
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !named {
                d.error(
                    path.clone(),
                    format!("`{k}` is not a valid variable name (letters, digits and `_`, not starting with a digit)"),
                );
            } else if k.starts_with("GOOGLE_") {
                d.error(
                    path.clone(),
                    "names starting with `GOOGLE_` are reserved by Workflows",
                );
            }
            match refs.value(&v, &mut targets) {
                Ok(t) => {
                    if k.len() + 1 + t.literal_len() > MAX_ENV_DEFINITION_BYTES {
                        d.error(
                            path,
                            "`NAME=value` is longer than 4 KiB, the Workflows limit",
                        );
                    }
                    env.insert(k, t);
                }
                Err(e) => d.error(path, e),
            }
        }

        // ---- logging ----
        let call_log_level = match pick(&layers, |w| &w.call_log_level, "call_log_level") {
            None => CallLogLevel::None,
            Some((v, p)) => CallLogLevel::parse(&v).unwrap_or_else(|| {
                d.error(
                    p,
                    format!("`{v}` must be `none`, `errors-only` or `all-calls`"),
                );
                CallLogLevel::None
            }),
        };

        // ---- identity ----
        let given = pick(&layers, |w| &w.service_account, "service_account");
        let service_account = match &given {
            Some((v, p)) => {
                let v = ixs(d, v, p);
                if let Err(e) = validate::service_account_email(&v) {
                    d.error(p.clone(), e);
                }
                v
            }
            None => format!(
                "{}@{project}.iam.gserviceaccount.com",
                naming::workflow_account_id(&raw.app, stage)
            ),
        };
        let raw_identity = pick(&layers, |w| &w.identity, "identity");
        let create = raw_identity
            .as_ref()
            .and_then(|(i, _)| i.create)
            .unwrap_or(given.is_none());
        if create && validate::service_account_project(&service_account).is_none() {
            d.error(
                format!("{base}.identity.create"),
                "only user-managed service accounts (NAME@PROJECT.iam.gserviceaccount.com) can be created",
            );
        }
        let identity = match raw_identity {
            None => IdentityConfig {
                create,
                ..Default::default()
            },
            Some((i, p)) => IdentityConfig {
                create,
                display_name: i
                    .display_name
                    .map(|n| ixs(d, &n, &format!("{p}.display_name"))),
                roles: resolve_roles(d, ix, project, i.roles.unwrap_or_default(), &p),
            },
        };

        // ---- invokers ----
        let mut invokers = Vec::new();
        if let Some((list, p)) = pick(&layers, |w| &w.invokers, "invokers") {
            for (i, m) in list.iter().enumerate() {
                let mp = format!("{p}[{i}]");
                let m = ixs(d, m, &mp);
                if let Err(e) = validate::iam_member(&m) {
                    d.error(mp, e);
                }
                if !invokers.contains(&m) {
                    invokers.push(m);
                }
            }
            if !invokers.is_empty() {
                d.warn(
                    p,
                    format!("Workflows has no per-workflow access: these members can run every workflow of project {project}"),
                );
            }
        }
        let wf_region = match pick(&layers, |w| &w.region, "region") {
            Some((r, p)) => {
                if let Err(e) = validate::region(&r) {
                    d.error(p, e);
                }
                r
            }
            None => default_region.clone(),
        };
        out.push(WorkflowConfig {
            key: key.clone(),
            id,
            region: wf_region,
            source_file,
            source,
            description,
            env,
            call_log_level,
            service_account,
            identity,
            invokers,
            targets,
        });
    }
    (out, default_region)
}
