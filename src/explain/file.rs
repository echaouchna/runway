//! What the current runway.yaml says about a key: where it is set (with the
//! value written there), its resolved value in each stage, and the
//! validation issues about it.

use super::catalog::{Entry, find, is_placeholder};
use crate::config::{self, Issue, Overrides};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Service keys a job has (and inherits from `defaults`).
const JOB_KEYS: &[&str] = &[
    "source",
    "dockerfile",
    "builder",
    "rebuild",
    "image",
    "command",
    "args",
    "cpu",
    "memory",
    "service_account",
    "identity",
    "env",
    "secrets",
    "volumes",
    "vpc",
    "cloud_sql",
    "execution_environment",
    "sandbox",
];

/// Top-level keys a stage can override (`stages.S.<key>`).
const STAGEABLE: &[&str] = &[
    "vars",
    "provider",
    "defaults",
    "services",
    "jobs",
    "schedules",
    "scheduler",
    "release",
    "domains",
];

#[derive(Debug, Clone, Serialize)]
pub struct Workload {
    pub kind: &'static str,
    pub name: String,
    pub id: String,
    #[serde(skip)]
    pub json: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stage {
    pub name: String,
    pub workloads: Vec<Workload>,
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    pub path: PathBuf,
    /// Why the file could not be read or parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub stages: Vec<Stage>,
    #[serde(skip)]
    raw: Value,
}

/// Where a key is set, with the value written there.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Place {
    pub path: String,
    pub value: String,
}

/// The resolved value of a key in a stage (for one workload).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageValue {
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    pub value: String,
}

impl FileInfo {
    /// Reads `path`; `None` when there is no file.
    pub fn load(path: &Path) -> Option<FileInfo> {
        if !path.is_file() {
            return None;
        }
        let mut info = FileInfo {
            path: path.to_path_buf(),
            error: None,
            app: None,
            stages: Vec::new(),
            raw: Value::Null,
        };
        match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_saphyr::from_str::<Value>(&t).map_err(|e| e.to_string()))
        {
            Ok(v) => info.raw = v,
            Err(e) => {
                info.error = Some(e);
                return Some(info);
            }
        }
        let cfg = match config::load(path) {
            Ok(c) => c,
            Err(e) => {
                info.error = Some(e.message);
                return Some(info);
            }
        };
        info.app = Some(cfg.raw.app.clone());
        for name in cfg.stage_names() {
            let mut stage = Stage {
                name: name.clone(),
                workloads: Vec::new(),
                errors: Vec::new(),
                warnings: Vec::new(),
            };
            match config::resolve(&cfg, &name, &Overrides::default()) {
                Ok(r) => {
                    stage.warnings = r.warnings.clone();
                    stage.workloads = r
                        .deployments
                        .iter()
                        .map(|d| Workload {
                            kind: if d.is_job() { "job" } else { "service" },
                            name: d.name().to_string(),
                            id: d.service_id.clone(),
                            json: serde_json::to_value(d).unwrap_or(Value::Null),
                        })
                        .collect();
                }
                Err(d) => {
                    stage.errors = d.errors;
                    stage.warnings = d.warnings;
                }
            }
            // One message per key, not one per workload that inherits it.
            for list in [&mut stage.errors, &mut stage.warnings] {
                let mut seen = std::collections::HashSet::new();
                list.retain(|i| seen.insert((i.path.clone(), i.message.clone())));
            }
            info.stages.push(stage);
        }
        Some(info)
    }

    /// Distinct errors, across stages.
    pub fn error_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for i in self.stages.iter().flat_map(|s| &s.errors) {
            seen.insert((&i.path, &i.message));
        }
        seen.len() + usize::from(self.error.is_some())
    }

    /// Where `e` is set in the file: its own place and every block that
    /// takes the same keys (`defaults`, `services.NAME`, stage overrides).
    pub fn places(&self, e: &Entry) -> Vec<Place> {
        if e.is_topic() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for pattern in patterns(&e.path) {
            let segs: Vec<&str> = pattern.split('.').collect();
            walk(&self.raw, &segs, String::new(), &mut out);
        }
        out.dedup();
        out
    }

    /// Whether `e`, or a key below it, is set.
    pub fn sets_below(&self, e: &Entry) -> bool {
        !self.places(e).is_empty()
    }

    /// The resolved value of `e` in each stage that resolves.
    pub fn values(&self, e: &Entry) -> Vec<StageValue> {
        let Some((pointer, per_workload)) = pointer(e) else {
            return Vec::new();
        };
        let job_key = e.path.starts_with("jobs.");
        let service_key = e.path.starts_with("service.");
        let first_key = e.path.split('.').nth(1).unwrap_or("");
        let mut out = Vec::new();
        for s in &self.stages {
            for w in &s.workloads {
                // Stage-wide keys (provider, domains…) resolve the same in
                // every workload, service or job.
                let applies = match (service_key, job_key) {
                    (false, false) => true,
                    (true, _) => w.kind == "service" || JOB_KEYS.contains(&first_key),
                    (_, true) => w.kind == "job",
                };
                if !applies {
                    continue;
                }
                if let Some(v) = w.json.pointer(&pointer) {
                    out.push(StageValue {
                        stage: s.name.clone(),
                        workload: per_workload.then(|| w.id.clone()),
                        value: show_in(&pointer, v, self.path.parent()),
                    });
                }
                if !per_workload {
                    break;
                }
            }
        }
        out
    }

    /// Validation issues about `e` (or, for a topic, none), per stage.
    pub fn issues(&self, e: &Entry) -> Vec<(String, &'static str, Issue)> {
        let mut out = Vec::new();
        for s in &self.stages {
            for (sev, list) in [("error", &s.errors), ("warning", &s.warnings)] {
                for i in list {
                    if find(&i.path).is_some_and(|f| f.path == e.path) {
                        out.push((s.name.clone(), sev, i.clone()));
                    }
                }
            }
        }
        out
    }

    /// Every issue whose key is `path` or below it.
    pub fn has_error_below(&self, path: &str) -> bool {
        self.stages.iter().flat_map(|s| &s.errors).any(|i| {
            find(&i.path).is_some_and(|f| f.path == path || f.path.starts_with(&format!("{path}.")))
        })
    }
}

/// Where the keys of `path` can be written.
fn patterns(path: &str) -> Vec<String> {
    let segs: Vec<&str> = path.split('.').collect();
    match segs.as_slice() {
        ["service"] => vec!["service".into(), "stages.*.service".into()],
        ["service", first, ..] => {
            let rest = &path["service.".len()..];
            let mut blocks = vec![
                "service",
                "defaults",
                "services.*",
                "stages.*.service",
                "stages.*.defaults",
                "stages.*.services.*",
            ];
            if JOB_KEYS.contains(first) {
                blocks.extend(["jobs.*", "stages.*.jobs.*"]);
            }
            blocks.iter().map(|b| format!("{b}.{rest}")).collect()
        }
        [first, ..] if STAGEABLE.contains(first) => {
            vec![path.to_string(), format!("stages.*.{path}")]
        }
        _ => vec![path.to_string()],
    }
}

fn walk(v: &Value, segs: &[&str], at: String, out: &mut Vec<Place>) {
    if let Value::Array(items) = v
        && !segs.is_empty()
    {
        for (i, item) in items.iter().enumerate() {
            walk(item, segs, format!("{at}[{i}]"), out);
        }
        return;
    }
    let Some((seg, rest)) = segs.split_first() else {
        out.push(Place {
            path: at,
            value: show("", v),
        });
        return;
    };
    let Value::Object(map) = v else { return };
    let join = |k: &str| match at.is_empty() {
        true => k.to_string(),
        false => format!("{at}.{k}"),
    };
    if *seg == "*" || is_placeholder(seg) {
        for (k, child) in map {
            walk(child, rest, join(k), out);
        }
    } else if let Some(child) = map.get(*seg) {
        walk(child, rest, join(seg), out);
    }
}

/// Where the resolved value of `e` is, and whether it differs per workload.
fn pointer(e: &Entry) -> Option<(String, bool)> {
    if e.is_topic() || e.resolved.as_deref() == Some("-") {
        return None;
    }
    let per_workload = e.path.starts_with("service.") || e.path.starts_with("jobs.");
    if let Some(p) = &e.resolved {
        return Some((p.clone(), per_workload));
    }
    let segs: Vec<&str> = e.path.split('.').collect();
    if segs.iter().any(|s| is_placeholder(s)) || segs.len() < 2 {
        return None;
    }
    Some((format!("/{}", segs.join("/")), per_workload))
}

/// A value on one line.
pub fn show(pointer: &str, v: &Value) -> String {
    show_in(pointer, v, None)
}

/// A value on one line; the build folder relative to `base` (runway.yaml's
/// folder). Nothing else is rewritten: other values are not paths.
fn show_in(pointer: &str, v: &Value, base: Option<&Path>) -> String {
    if pointer == "/artifact" {
        if v["kind"] == "image"
            && let Some(r) = v["reference"].as_str()
        {
            return format!("image {r}");
        }
        if v["kind"] == "promote"
            && let Some(from) = v["from"].as_str()
        {
            return format!("promoted from stage {from}");
        }
        if v["kind"] == "build" {
            let b = v;
            let full = b["context_dir"].as_str().unwrap_or(".");
            let dir = match base.and_then(|d| Path::new(full).strip_prefix(d).ok()) {
                Some(rel) if rel.as_os_str().is_empty() => ".".to_string(),
                Some(rel) => rel.display().to_string(),
                None => full.to_string(),
            };
            let how = match b["strategy"]["type"].as_str() {
                Some("dockerfile") => {
                    format!(
                        "Dockerfile {}",
                        b["strategy"]["path"].as_str().unwrap_or("")
                    )
                }
                _ => format!(
                    "buildpacks {}",
                    b["strategy"]["builder"].as_str().unwrap_or("")
                ),
            };
            return format!("build {dir} ({how})");
        }
    }
    let s = match v {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => format!(
            "[{}]",
            a.iter().map(|x| show("", x)).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(m) if m.is_empty() => "{}".into(),
        other => other.to_string(),
    };
    match s.chars().count() > 90 {
        true => format!("{}…", s.chars().take(89).collect::<String>()),
        false => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain::catalog::find;

    fn file(yaml: &str) -> (tempfile::TempDir, FileInfo) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(&p, yaml).unwrap();
        let info = FileInfo::load(&p).unwrap();
        (dir, info)
    }

    const YAML: &str = r#"
version: 1
app: shop
provider: {project: my-gcp-project, region: europe-west1}
defaults:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop@sha256:0000000000000000000000000000000000000000000000000000000000000000
  service_account: rt@my-gcp-project.iam.gserviceaccount.com
  memory: 1Gi
service: {}
services:
  web: {}
jobs:
  migrate: {tasks: 2}
stages:
  dev: {}
  prod:
    service: {memory: 2Gi}
    jobs: {migrate: {tasks: 4}}
"#;

    #[test]
    fn a_key_is_found_wherever_it_is_set() {
        let (_d, f) = file(YAML);
        let places = f.places(find("service.memory").unwrap());
        assert_eq!(
            places,
            [
                Place {
                    path: "defaults.memory".into(),
                    value: "1Gi".into()
                },
                Place {
                    path: "stages.prod.service.memory".into(),
                    value: "2Gi".into()
                },
            ]
        );
        assert_eq!(f.places(find("jobs.<name>.tasks").unwrap()).len(), 2);
        assert!(f.places(find("service.cpu").unwrap()).is_empty());
    }

    #[test]
    fn values_are_shown_per_stage_and_workload() {
        let (_d, f) = file(YAML);
        let v = f.values(find("service.memory").unwrap());
        let prod: Vec<(String, String)> = v
            .iter()
            .filter(|x| x.stage == "prod")
            .map(|x| (x.workload.clone().unwrap(), x.value.clone()))
            .collect();
        assert_eq!(
            prod,
            [
                ("shop-prod".to_string(), "2Gi".to_string()),
                ("shop-web-prod".to_string(), "1Gi".to_string()),
                ("shop-migrate-prod".to_string(), "1Gi".to_string()),
            ],
            "memory is a job key too"
        );
        let tasks = f.values(find("jobs.<name>.tasks").unwrap());
        assert_eq!(
            tasks.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(),
            ["2", "4"]
        );
        let region = f.values(find("provider.region").unwrap());
        assert_eq!(region.len(), 2, "stage-wide: one value per stage");
        let image = f.values(find("service.image").unwrap());
        assert!(
            image[0]
                .value
                .starts_with("image europe-west1-docker.pkg.dev/")
        );
    }

    #[test]
    fn issues_belong_to_their_key() {
        let (_d, f) = file(&YAML.replace("service: {memory: 2Gi}", "service: {memory: 99Ti}"));
        let issues = f.issues(find("service.memory").unwrap());
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].0, "prod");
        assert!(f.has_error_below("service"));
        assert!(!f.has_error_below("provider"));
    }

    #[test]
    fn only_the_build_folder_is_made_relative() {
        let base = Some(Path::new("prod"));
        let project = Value::String("my-prod-project".into());
        assert_eq!(show_in("/project", &project, base), "my-prod-project");
        let sa = Value::String("prod-runtime@my-prod-project.iam.gserviceaccount.com".into());
        assert_eq!(
            show_in("/service/service_account", &sa, base),
            sa.as_str().unwrap()
        );
        let build = serde_json::json!({
            "kind": "build",
            "context_dir": "prod/apps/web",
            "strategy": {"type": "dockerfile", "path": "Dockerfile"}
        });
        assert_eq!(
            show_in("/artifact", &build, base),
            "build apps/web (Dockerfile Dockerfile)"
        );
        let root = serde_json::json!({"kind": "build", "context_dir": "prod", "strategy": {"type": "buildpacks", "builder": "b"}});
        assert_eq!(show_in("/artifact", &root, base), "build . (buildpacks b)");
    }

    #[test]
    fn stage_wide_keys_resolve_in_a_stage_of_jobs() {
        let (_d, f) = file(
            "version: 1\napp: shop\nprovider: {project: my-gcp-project, region: europe-west1}\ndefaults:\n  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop:1\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\njobs:\n  migrate: {}\nstages: {prod: {}}\n",
        );
        assert!(f.stages[0].errors.is_empty(), "{:?}", f.stages[0].errors);
        let v = f.values(find("provider.project").unwrap());
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(
            (v[0].stage.as_str(), v[0].value.as_str()),
            ("prod", "my-gcp-project")
        );
        assert!(
            f.values(find("service.port").unwrap()).is_empty(),
            "jobs have no port"
        );
        assert_eq!(
            f.values(find("service.memory").unwrap()).len(),
            1,
            "jobs have memory"
        );
    }

    #[test]
    fn a_broken_file_is_reported_not_fatal() {
        let (_d, f) = file("version: 1\napp: [");
        assert!(f.error.is_some());
        assert!(f.places(find("app").unwrap()).is_empty());
    }
}
