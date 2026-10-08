//! `runway explain`: what every key of runway.yaml does, with examples,
//! rules, and what the current file sets. A full-screen browser in a
//! terminal; plain text for one key, in pipes and in CI; JSON for scripts.

pub mod agent;
pub mod catalog;
pub mod file;
pub mod tui;

use crate::cli::{Context, ExplainArgs};
use crate::error::{Error, Result};
use crate::output::{OutputFormat, print_json};
use crate::style::Painter;
use catalog::{Entry, catalog, find, suggest};
use file::FileInfo;
use serde::Serialize;
use std::io::IsTerminal;

#[derive(Serialize)]
struct Explained<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    set_in_file: Vec<file::Place>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    per_stage: Vec<file::StageValue>,
}

fn explained<'a>(e: &'a Entry, f: Option<&FileInfo>) -> Explained<'a> {
    Explained {
        entry: e,
        docs: e.docs_url(),
        set_in_file: f.map(|f| f.places(e)).unwrap_or_default(),
        per_stage: f.map(|f| f.values(e)).unwrap_or_default(),
    }
}

pub fn run(ctx: &Context, args: ExplainArgs) -> Result<()> {
    let file = FileInfo::load(&ctx.config);
    let entry = match &args.key {
        None => None,
        Some(k) => Some(find(k).ok_or_else(|| {
            let close = suggest(k);
            let mut e = Error::config(format!("`{k}` is not a key of runway.yaml"));
            if !close.is_empty() {
                e = e.hint(format!("did you mean {}?", close.join(", ")));
            }
            e.hint("`runway explain` lists every key")
        })?),
    };
    if ctx.output == OutputFormat::Json {
        match entry {
            Some(e) => print_json(&explained(e, file.as_ref())),
            None => print_json(
                &catalog()
                    .iter()
                    .map(|e| explained(e, file.as_ref()))
                    .collect::<Vec<_>>(),
            ),
        }
        return Ok(());
    }
    if args.agent {
        print!("{}", agent::render(entry, file.as_ref()));
        return Ok(());
    }
    let interactive =
        std::io::stdout().is_terminal() && std::io::stdin().is_terminal() && !args.plain;
    let p = crate::style::out();
    match entry {
        Some(e) => print!("{}", render(e, file.as_ref(), &p)),
        None if interactive => tui::run(file, p.enabled)?,
        None => print!("{}", overview(&p)),
    }
    Ok(())
}

/// One key, as plain (optionally colored) text.
pub fn render(e: &Entry, f: Option<&FileInfo>, p: &Painter) -> String {
    let mut out = String::new();
    let mut head = match e.is_topic() {
        true => p.bold_cyan(e.name()),
        false => p.bold_cyan(&e.path),
    };
    if let Some(k) = &e.kind {
        head.push_str(&format!("  {}", p.yellow(k)));
    }
    if let Some(d) = &e.default {
        head.push_str(&format!("  {} {d}", p.dim("default")));
    }
    out.push_str(&head);
    out.push_str("\n\n");
    for (i, para) in e.text.iter().enumerate() {
        let text = wrap(para, 78, "");
        out.push_str(&match i {
            0 => p.bold(&text),
            _ => text,
        });
        out.push_str("\n\n");
    }
    if !e.example.is_empty() {
        out.push_str(&p.bold("Example"));
        out.push('\n');
        for l in &e.example {
            out.push_str(&format!("  {l}\n"));
        }
        out.push('\n');
    }
    if !e.rules.is_empty() {
        out.push_str(&p.bold("Rules"));
        out.push('\n');
        for r in &e.rules {
            out.push_str(&format!("  • {}\n", wrap(r, 74, "    ")));
        }
        out.push('\n');
    }
    if let Some(f) = f
        && !e.is_topic()
    {
        out.push_str(&p.bold("In your file"));
        out.push('\n');
        let places = f.places(e);
        if places.is_empty() {
            out.push_str(&format!("  {}\n", p.dim("not set")));
        }
        for pl in places {
            out.push_str(&format!("  {}: {}\n", pl.path, p.green(&pl.value)));
        }
        let values = f.values(e);
        if !values.is_empty() {
            out.push_str(&format!("{}\n", p.bold("Per stage (resolved)")));
            let sw = values.iter().map(|v| v.stage.len()).max().unwrap_or(0);
            let ww = values
                .iter()
                .map(|v| v.workload.as_deref().map_or(0, str::len))
                .max()
                .unwrap_or(0);
            for v in values {
                let who = v.workload.map(|w| format!("{w:ww$}  ")).unwrap_or_default();
                out.push_str(&format!("  {:sw$}  {who}{}\n", v.stage, p.green(&v.value)));
            }
        }
        for (stage, sev, i) in f.issues(e) {
            out.push_str(&format!(
                "  {} {stage} · {}: {}\n",
                p.severity(sev),
                i.path,
                i.message
            ));
        }
        out.push('\n');
    }
    if !e.see.is_empty() {
        out.push_str(&format!("{}  {}\n", p.bold("See also"), e.see.join(", ")));
    }
    if let Some(url) = e.docs_url() {
        out.push_str(&format!("{}  {url}\n", p.bold("Docs")));
    }
    out
}

/// Every key with the first sentence of what it does.
pub fn overview(p: &Painter) -> String {
    let mut out = format!(
        "{}\n\n",
        p.bold("runway.yaml keys (`runway explain KEY` for one, in a terminal `runway explain` browses them)")
    );
    for e in catalog() {
        let depth = match e.is_topic() {
            true => 0,
            false => e.path.matches('.').count(),
        };
        let name = match e.is_topic() {
            true => format!("{} ({})", e.name(), e.path),
            false => e.path.clone(),
        };
        let summary = e.summary();
        let first = summary
            .find(". ")
            .map_or(summary, |i| &summary[..=i])
            .replace('`', "");
        out.push_str(&format!(
            "{}{}  {}\n",
            "  ".repeat(depth),
            if depth == 0 {
                p.bold_cyan(&name)
            } else {
                p.cyan(&name)
            },
            p.dim(&first)
        ));
    }
    out
}

fn wrap(s: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    let mut line = 0;
    for word in s.split_whitespace() {
        if line > 0 && line + 1 + word.chars().count() > width {
            out.push('\n');
            out.push_str(indent);
            line = indent.len();
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += word.chars().count();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::*;

    const PLAIN: Painter = Painter { enabled: false };

    /// The fields of a schema struct, from serde's "unknown field" message.
    fn fields<T: serde::de::DeserializeOwned>() -> Vec<String> {
        let err = match serde_saphyr::from_str::<T>("__not_a_field__: 1") {
            Ok(_) => panic!("accepted an unknown field"),
            Err(e) => e.to_string(),
        };
        let list = err
            .split("expected one of ")
            .nth(1)
            .or_else(|| err.split("expected ").nth(1))
            .unwrap_or_else(|| panic!("no field list in: {err}"));
        list.lines()
            .next()
            .unwrap()
            .split(", ")
            .map(|f| f.trim_matches('`').trim().to_string())
            .collect()
    }

    fn missing<T: serde::de::DeserializeOwned>(prefix: &str, out: &mut Vec<String>) {
        for f in fields::<T>() {
            let path = match prefix {
                "" => f.clone(),
                p => format!("{p}.{f}"),
            };
            if find(&path).is_none() {
                out.push(path);
            }
        }
    }

    #[test]
    fn every_schema_key_is_explained() {
        let mut m = Vec::new();
        missing::<RawConfig>("", &mut m);
        missing::<RawProvider>("provider", &mut m);
        missing::<RawService>("service", &mut m);
        missing::<RawJob>("jobs.x", &mut m);
        missing::<RawSchedule>("schedules.x", &mut m);
        missing::<RawScheduler>("scheduler", &mut m);
        missing::<RawRelease>("release", &mut m);
        missing::<RawReleaseRepository>("release.repository", &mut m);
        missing::<RawDomains>("domains", &mut m);
        missing::<RawDnsZone>("domains.dns", &mut m);
        missing::<RawExistingLoadBalancer>("domains.load_balancer", &mut m);
        missing::<RawRetry>("retry", &mut m);
        missing::<RawBucket>("buckets.x", &mut m);
        missing::<RawManagedSecret>("secrets.x", &mut m);
        missing::<RawStage>("stages.x", &mut m);
        missing::<RawVpc>("service.vpc", &mut m);
        missing::<RawBootstrap>("service.bootstrap", &mut m);
        missing::<RawSidecar>("service.sidecars.x", &mut m);
        missing::<RawSidecarCheck>("service.sidecars.x.health_check", &mut m);
        missing::<RawOtelCollector>("service.otel_collector", &mut m);
        missing::<RawHealthCheck>("service.health_check", &mut m);
        missing::<RawProbe>("service.health_check.startup", &mut m);
        missing::<RawVolume>("service.volumes.x", &mut m);
        missing::<RawIap>("service.iap", &mut m);
        missing::<RawIdentity>("service.identity", &mut m);
        missing::<RawRoleBinding>("service.identity.roles", &mut m);
        missing::<RawSecretRef>("service.secrets.X", &mut m);
        assert!(m.is_empty(), "keys runway explain does not know: {m:?}");
    }

    #[test]
    fn every_key_has_a_parent_a_summary_and_a_docs_page() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        for e in catalog() {
            assert!(!e.summary().is_empty(), "{} has no text", e.path);
            if let Some(p) = e.parent().filter(|_| !e.is_topic()) {
                assert!(
                    catalog().iter().any(|x| x.path == p),
                    "{} has no parent entry {p}",
                    e.path
                );
            }
            let docs = e
                .docs
                .as_deref()
                .unwrap_or_else(|| panic!("{} has no docs", e.path));
            let (page, anchor) = docs.split_once('#').unwrap_or((docs, ""));
            let text = std::fs::read_to_string(root.join(page))
                .unwrap_or_else(|_| panic!("{}: no page docs/{page}", e.path));
            if !anchor.is_empty() {
                let slugs: Vec<String> = text
                    .lines()
                    .filter(|l| l.starts_with('#'))
                    .map(|l| {
                        l.trim_start_matches('#')
                            .trim()
                            .to_lowercase()
                            .chars()
                            .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-')
                            .collect::<String>()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join("-")
                    })
                    .collect();
                assert!(
                    slugs.iter().any(|s| s == anchor),
                    "{}: no #{anchor} in docs/{page}",
                    e.path
                );
            }
            for s in &e.see {
                assert!(find(s).is_some(), "{} sees unknown {s}", e.path);
            }
        }
    }

    #[test]
    fn examples_are_valid_yaml() {
        for e in catalog().iter().filter(|e| !e.example.is_empty()) {
            let text = e.example.join("\n");
            assert!(
                serde_saphyr::from_str::<serde_json::Value>(&text).is_ok(),
                "{}: example is not YAML:\n{text}",
                e.path
            );
        }
    }

    #[test]
    fn the_start_example_is_a_valid_runway_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(&p, find(":start").unwrap().example.join("\n")).unwrap();
        for stage in ["dev", "prod"] {
            crate::config::load_and_resolve(&p, stage, &Default::default())
                .unwrap_or_else(|e| panic!("{stage}: {}", e.message));
        }
    }

    #[test]
    fn plain_text_shows_the_key_and_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            "version: 1\napp: shop\nprovider: {project: my-gcp-project, region: europe-west1}\nservice:\n  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop:1\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\n  memory: 1Gi\nstages: {prod: {}}\n",
        )
        .unwrap();
        let f = FileInfo::load(&p);
        let text = render(find("services.web.memory").unwrap(), f.as_ref(), &PLAIN);
        assert!(
            text.starts_with("service.memory  string  default 512Mi"),
            "{text}"
        );
        assert!(text.contains("  service.memory: 1Gi\n"), "{text}");
        assert!(text.contains("  prod  shop-prod  1Gi\n"), "{text}");
        assert!(text.contains("Docs  https://runway.echaouchna.dev/docs/configuration/"));
        let all = overview(&PLAIN);
        assert!(all.contains("\n  service.vpc  Direct VPC egress"), "{all}");
    }
}
