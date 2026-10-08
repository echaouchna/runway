//! `runway explain --agent`: the reference as Markdown for LLMs and coding
//! agents. No colors, no browser: every rule, default and example, fenced
//! YAML, and what the current runway.yaml sets and gets wrong.

use super::catalog::{Entry, catalog};
use super::file::FileInfo;
use std::fmt::Write;

/// The whole reference, or `key` and the keys below it.
pub fn render(key: Option<&Entry>, file: Option<&FileInfo>) -> String {
    let mut out = String::new();
    header(&mut out, key);
    if let Some(f) = file {
        file_section(&mut out, f, key);
    }
    match key {
        Some(e) if e.is_topic() => entry(&mut out, e, file, "##"),
        Some(e) => {
            out.push_str("## Keys\n\n");
            for x in catalog().iter().filter(|x| within(x, e)) {
                entry(&mut out, x, file, "###");
            }
        }
        None => {
            out.push_str("## Guide\n\n");
            for t in catalog().iter().filter(|e| e.is_topic()) {
                entry(&mut out, t, None, "###");
            }
            out.push_str("## Keys\n\n");
            for e in catalog().iter().filter(|e| !e.is_topic()) {
                entry(&mut out, e, file, "###");
            }
        }
    }
    out.trim_end().to_string() + "\n"
}

/// `x` is `e` or a key below it.
fn within(x: &Entry, e: &Entry) -> bool {
    !x.is_topic() && (x.path == e.path || x.path.starts_with(&format!("{}.", e.path)))
}

fn header(out: &mut String, key: Option<&Entry>) {
    let scope = match key {
        Some(e) if e.is_topic() => format!(": {}", e.name()),
        Some(e) => format!(": `{}`", e.path),
        None => String::new(),
    };
    let _ = write!(
        out,
        "# runway.yaml reference{scope}\n\n\
runway {} deploys to Google Cloud Run from `runway.yaml` (schema `version: 1`).\n\n\
How to read this:\n\
- A key is a dotted path; `<name>` and `<NAME>` are names you choose (`services.<name>` is `services.web`).\n\
- The keys of `service` are also valid in `defaults`, `services.<name>`, `stages.<name>.service`, `stages.<name>.defaults` and `stages.<name>.services.<name>`; jobs take the subset listed under `jobs.<name>`.\n\
- Layers, lowest first: `defaults`, `stages.S.defaults`, the service or job, `stages.S.<its block>`. Scalars are replaced; maps (`env`, `secrets`, `tags`, `volumes`, `sidecars`) merge by key and `null` removes an inherited key.\n\
- Unknown keys are errors. After an edit, run `runway validate` (offline, no credentials) and `runway plan --stage S --offline`.\n\
- `runway explain --agent KEY` prints one key and the keys below it; `runway -o json explain [KEY]` gives the same as JSON.\n\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn file_section(out: &mut String, f: &FileInfo, key: Option<&Entry>) {
    let _ = writeln!(
        out,
        "## The current runway.yaml\n\nFile: `{}`",
        f.path.display()
    );
    if let Some(err) = &f.error {
        let _ = writeln!(
            out,
            "\nIt cannot be read:\n\n```text\n{}\n```\n",
            err.trim_end()
        );
        return;
    }
    if let Some(app) = &f.app {
        let _ = writeln!(out, "App: `{app}`");
    }
    out.push('\n');
    for s in &f.stages {
        let state = if s.errors.is_empty() {
            "valid"
        } else {
            "INVALID"
        };
        let workloads: Vec<String> = s
            .workloads
            .iter()
            .map(|w| format!("{} `{}`", w.kind, w.id))
            .collect();
        let _ = write!(out, "- Stage `{}`: {state}", s.name);
        if !workloads.is_empty() {
            let _ = write!(out, "; deploys {}", workloads.join(", "));
        }
        out.push('\n');
        for (sev, list) in [("error", &s.errors), ("warning", &s.warnings)] {
            for i in list {
                let _ = writeln!(out, "  - {sev} at `{}`: {}", i.path, i.message);
            }
        }
    }
    // Keys set, with their values: the keys without keys below them (a
    // block's content shows through its keys).
    let set: Vec<(&Entry, Vec<super::file::Place>)> = catalog()
        .iter()
        .filter(|e| !e.is_topic() && key.is_none_or(|k| k.is_topic() || within(e, k)))
        .filter(|e| {
            !catalog()
                .iter()
                .any(|c| c.parent() == Some(e.path.as_str()))
        })
        .map(|e| (e, f.places(e)))
        .filter(|(_, p)| !p.is_empty())
        .collect();
    if !set.is_empty() {
        out.push_str("\nKeys this file sets (`key`: value, where):\n\n");
        for (e, places) in set {
            for p in places {
                let _ = writeln!(out, "- `{}`: `{}` (at `{}`)", e.path, p.value, p.path);
            }
        }
    }
    out.push('\n');
}

fn entry(out: &mut String, e: &Entry, file: Option<&FileInfo>, level: &str) {
    match e.is_topic() {
        true => {
            let _ = writeln!(out, "{level} {} (`{}`)\n", e.name(), e.path);
        }
        false => {
            let _ = writeln!(out, "{level} `{}`\n", e.path);
            let mut meta = Vec::new();
            if let Some(k) = &e.kind {
                meta.push(format!("type: {k}"));
            }
            if let Some(d) = &e.default {
                meta.push(format!("default: {d}"));
            }
            if !meta.is_empty() {
                let _ = writeln!(out, "{}\n", meta.join(" · "));
            }
        }
    }
    for p in &e.text {
        let _ = writeln!(out, "{p}\n");
    }
    if !e.rules.is_empty() {
        out.push_str("Rules:\n");
        for r in &e.rules {
            let _ = writeln!(out, "- {r}");
        }
        out.push('\n');
    }
    if !e.example.is_empty() {
        let _ = writeln!(out, "Example:\n\n```yaml\n{}\n```\n", e.example.join("\n"));
    }
    if let Some(f) = file
        && !e.is_topic()
    {
        let values = f.values(e);
        if !values.is_empty() {
            out.push_str("Resolved in the current file:\n");
            for v in values {
                let who = v.workload.map(|w| format!(" `{w}`")).unwrap_or_default();
                let _ = writeln!(out, "- stage `{}`{who}: `{}`", v.stage, v.value);
            }
            out.push('\n');
        }
        for (stage, sev, i) in f.issues(e) {
            let _ = writeln!(
                out,
                "Problem ({sev}, stage `{stage}`) at `{}`: {}\n",
                i.path, i.message
            );
        }
    }
    let mut refs = Vec::new();
    if !e.see.is_empty() {
        refs.push(format!(
            "related: {}",
            e.see
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(url) = e.docs_url() {
        refs.push(format!("docs: {url}"));
    }
    if !refs.is_empty() {
        let _ = writeln!(out, "{}\n", refs.join(" · "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain::catalog::find;

    #[test]
    fn the_whole_reference_is_plain_markdown() {
        let md = render(None, None);
        assert!(md.starts_with("# runway.yaml reference\n"));
        assert!(!md.contains('\u{1b}'), "no terminal colors");
        assert!(
            md.contains("## Guide\n")
                && md.contains("### Stages, defaults and precedence (`:layers`)")
        );
        assert!(md.contains("### `service.vpc.egress`\n\ntype: private-ranges-only | all-traffic · default: private-ranges-only\n"));
        assert!(md.contains("```yaml\nmemory: 1Gi\n```"));
        for e in catalog().iter().filter(|e| !e.is_topic()) {
            assert!(
                md.contains(&format!("### `{}`\n", e.path)),
                "{} missing",
                e.path
            );
        }
        assert_eq!(md.matches("```").count() % 2, 0, "fences are balanced");
    }

    #[test]
    fn a_key_brings_the_keys_below_it_and_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            "version: 1\napp: shop\nprovider: {project: my-gcp-project, region: europe-west1}\nservice:\n  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop:1\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\n  vpc: {network: default, subnet: default, egress: everything}\nstages: {prod: {}}\n",
        )
        .unwrap();
        let f = FileInfo::load(&p);
        let md = render(find("services.web.vpc"), f.as_ref());
        assert!(
            md.starts_with("# runway.yaml reference: `service.vpc`\n"),
            "{md}"
        );
        assert!(md.contains("### `service.vpc.subnet`"));
        assert!(!md.contains("### `service.memory`"), "only the subtree");
        assert!(md.contains("- Stage `prod`: INVALID"), "{md}");
        assert!(md.contains("  - error at `service.vpc.egress`:"), "{md}");
        assert!(
            md.contains("- `service.vpc.network`: `default` (at `service.vpc.network`)"),
            "{md}"
        );
        assert!(
            !md.contains("- `service.image`: "),
            "keys outside the subtree are not listed"
        );
        assert!(
            md.contains("Problem (error, stage `prod`) at `service.vpc.egress`"),
            "{md}"
        );
    }
}
