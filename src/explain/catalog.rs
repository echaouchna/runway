//! What `runway explain` knows about each key of runway.yaml, parsed from
//! `catalog.txt` (see the format at the top of that file).

use serde::Serialize;
use std::sync::OnceLock;

const TEXT: &str = include_str!("catalog.txt");
const DOCS: &str = "https://runway.echaouchna.dev/docs/";

/// A key of runway.yaml (`service.memory`, `<name>` for names you choose),
/// or a guide topic (`:start`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Entry {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip)]
    pub group: Option<String>,
    #[serde(skip)]
    pub docs: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub see: Vec<String>,
    /// JSON pointer of the resolved value in a deployment; `-`: none.
    #[serde(skip)]
    pub resolved: Option<String>,
    pub text: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub example: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
}

impl Entry {
    pub fn is_topic(&self) -> bool {
        self.path.starts_with(':')
    }

    /// The first paragraph.
    pub fn summary(&self) -> &str {
        self.text.first().map_or("", String::as_str)
    }

    /// The last segment of the path, or the topic's title.
    pub fn name(&self) -> &str {
        match &self.title {
            Some(t) => t,
            None => self.path.rsplit('.').next().unwrap_or(&self.path),
        }
    }

    pub fn segments(&self) -> Vec<&str> {
        self.path.split('.').collect()
    }

    /// The parent key (`service` for `service.memory`).
    pub fn parent(&self) -> Option<&str> {
        self.path.rsplit_once('.').map(|(p, _)| p)
    }

    pub fn docs_url(&self) -> Option<String> {
        let d = self.docs.as_deref()?;
        let (page, anchor) = d.split_once('#').unwrap_or((d, ""));
        let page = page.trim_end_matches(".md");
        let page = if page == "index" { "" } else { page };
        Some(match anchor {
            "" => format!("{DOCS}{page}/"),
            a => format!("{DOCS}{page}/#{a}"),
        })
    }
}

/// Every entry, in file order (parents before their keys).
pub fn catalog() -> &'static [Entry] {
    static CATALOG: OnceLock<Vec<Entry>> = OnceLock::new();
    CATALOG.get_or_init(|| parse(TEXT))
}

#[derive(PartialEq)]
enum Part {
    Meta,
    Text,
    Example,
    Rules,
}

pub fn parse(text: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut part = Part::Meta;
    let mut paragraph = String::new();
    fn flush(e: Option<&mut Entry>, p: &mut String) {
        if let Some(e) = e
            && !p.trim().is_empty()
        {
            e.text.push(p.trim().to_string());
        }
        p.clear();
    }
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("@ ") {
            flush(out.last_mut(), &mut paragraph);
            out.push(Entry {
                path: path.trim().to_string(),
                ..Default::default()
            });
            part = Part::Meta;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Some(e) = out.last_mut() else { continue };
        if part == Part::Meta
            && let Some((k, v)) = line.split_once(": ")
        {
            let v = v.trim().to_string();
            let meta = match k {
                "title" => Some(&mut e.title),
                "type" => Some(&mut e.kind),
                "default" => Some(&mut e.default),
                "group" => Some(&mut e.group),
                "docs" => Some(&mut e.docs),
                "resolved" => Some(&mut e.resolved),
                _ => None,
            };
            if let Some(field) = meta {
                *field = Some(v);
                continue;
            }
            if k == "see" {
                e.see = v.split(", ").map(String::from).collect();
                continue;
            }
        }
        match line {
            "Example:" => {
                flush(Some(e), &mut paragraph);
                part = Part::Example;
                continue;
            }
            "Rules:" => {
                flush(Some(e), &mut paragraph);
                part = Part::Rules;
                continue;
            }
            _ => {}
        }
        match part {
            Part::Example if line.starts_with("  ") || line.is_empty() => e
                .example
                .push(line.strip_prefix("  ").unwrap_or("").to_string()),
            Part::Rules if line.starts_with("- ") => e.rules.push(line[2..].trim().to_string()),
            Part::Rules if line.starts_with("  ") && !e.rules.is_empty() => {
                let last = e.rules.last_mut().expect("not empty");
                last.push(' ');
                last.push_str(line.trim());
            }
            _ if line.trim().is_empty() => flush(Some(e), &mut paragraph),
            _ => {
                part = Part::Text;
                if !paragraph.is_empty() {
                    paragraph.push(' ');
                }
                paragraph.push_str(line.trim());
            }
        }
    }
    flush(out.last_mut(), &mut paragraph);
    for e in &mut out {
        while e.example.last().is_some_and(|l| l.is_empty()) {
            e.example.pop();
        }
    }
    out
}

/// A pattern segment that matches any name.
pub fn is_placeholder(seg: &str) -> bool {
    seg.starts_with('<') && seg.ends_with('>')
}

fn matches(pattern: &str, segs: &[&str]) -> bool {
    let p: Vec<&str> = pattern.split('.').collect();
    p.len() == segs.len() && p.iter().zip(segs).all(|(p, s)| is_placeholder(p) || p == s)
}

fn exact(segs: &[&str]) -> Option<&'static Entry> {
    // A literal key beats a placeholder (`stages.<name>.service` over
    // `stages.<name>.<…>`): the fewest placeholders wins.
    catalog()
        .iter()
        .filter(|e| !e.is_topic() && matches(&e.path, segs))
        .min_by_key(|e| e.segments().iter().filter(|s| is_placeholder(s)).count())
}

/// Keys of a stage block, which override the top-level ones.
const STAGE_BLOCKS: &[&str] = &[
    "vars",
    "provider",
    "service",
    "defaults",
    "services",
    "jobs",
    "schedules",
    "scheduler",
    "release",
    "domains",
];

/// The entry for a key as written anywhere in runway.yaml or in a
/// validation message: `services.web.memory`, `stages.prod.service.vpc`,
/// `service.secrets.DB.version`, `service.identity.roles[0].role`, a topic
/// (`:start`) or its title.
pub fn find(input: &str) -> Option<&'static Entry> {
    let input = input.trim().trim_matches('`');
    if input.starts_with(':') || input.contains(' ') || !input.contains(['.', '_']) {
        let lower = input.to_ascii_lowercase();
        if let Some(t) = catalog().iter().find(|e| {
            e.is_topic()
                && (e.path == input
                    || e.title.as_deref().map(str::to_ascii_lowercase) == Some(lower.clone()))
        }) {
            return Some(t);
        }
    }
    // List indexes say nothing about the key.
    let cleaned: String = {
        let mut s = String::new();
        let mut depth = 0;
        for c in input.chars() {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ if depth == 0 => s.push(c),
                _ => {}
            }
        }
        s
    };
    let mut segs: Vec<String> = cleaned
        .split('.')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    for _ in 0..4 {
        let refs: Vec<&str> = segs.iter().map(String::as_str).collect();
        if let Some(e) = exact(&refs) {
            return Some(e);
        }
        segs = match refs.as_slice() {
            ["stages", _, block, rest @ ..] if STAGE_BLOCKS.contains(block) && !rest.is_empty() => {
                refs[2..].iter().map(|s| s.to_string()).collect()
            }
            ["defaults", rest @ ..] if !rest.is_empty() => std::iter::once("service")
                .chain(rest.iter().copied())
                .map(String::from)
                .collect(),
            ["services", _, rest @ ..] if !rest.is_empty() => std::iter::once("service")
                .chain(rest.iter().copied())
                .map(String::from)
                .collect(),
            ["jobs", _, rest @ ..] if !rest.is_empty() => std::iter::once("service")
                .chain(rest.iter().copied())
                .map(String::from)
                .collect(),
            [head @ .., "liveness", last] if head.first() == Some(&"service") => head
                .iter()
                .copied()
                .chain(["startup", last])
                .map(String::from)
                .collect(),
            ["service", "sidecars", _, "secrets", name, rest @ ..] => ["service", "secrets", name]
                .into_iter()
                .chain(rest.iter().copied())
                .map(String::from)
                .collect(),
            _ => return None,
        };
    }
    None
}

/// Keys close to `input`, for "did you mean".
pub fn suggest(input: &str) -> Vec<&'static str> {
    let last = input.rsplit('.').next().unwrap_or(input);
    let mut scored: Vec<(usize, &'static str)> = catalog()
        .iter()
        .filter(|e| !e.is_topic())
        .map(|e| {
            let d = distance(input, &e.path).min(distance(last, e.name()) + 1);
            (d, e.path.as_str())
        })
        .filter(|(d, _)| *d <= 2.max(last.len() / 3))
        .collect();
    scored.sort();
    scored.dedup_by_key(|(_, p)| *p);
    scored.into_iter().take(3).map(|(_, p)| p).collect()
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != *cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_reads_meta_text_examples_and_rules() {
        let e = &parse(
            "# comment\n@ service.memory\ntype: string\ndefault: 512Mi\nsee: service.cpu, :layers\nMemory per\ninstance.\n\nMore.\nExample:\n  memory: 1Gi\n\n  # two\nRules:\n- one\n  continued\n- two\n",
        )[0];
        assert_eq!(e.kind.as_deref(), Some("string"));
        assert_eq!(e.default.as_deref(), Some("512Mi"));
        assert_eq!(e.see, ["service.cpu", ":layers"]);
        assert_eq!(e.text, ["Memory per instance.", "More."]);
        assert_eq!(e.example, ["memory: 1Gi", "", "# two"]);
        assert_eq!(e.rules, ["one continued", "two"]);
        assert_eq!(e.name(), "memory");
        assert_eq!(e.parent(), Some("service"));
    }

    #[test]
    fn keys_are_found_however_they_are_written() {
        let path = |k: &str| find(k).map(|e| e.path.as_str());
        assert_eq!(path("service.memory"), Some("service.memory"));
        assert_eq!(path("services.web.memory"), Some("service.memory"));
        assert_eq!(path("stages.prod.service.memory"), Some("service.memory"));
        assert_eq!(
            path("stages.prod.services.web.vpc.subnet"),
            Some("service.vpc.subnet")
        );
        assert_eq!(path("defaults.env"), Some("service.env"));
        assert_eq!(path("jobs.migrate.tasks"), Some("jobs.<name>.tasks"));
        assert_eq!(path("jobs.migrate.memory"), Some("service.memory"));
        assert_eq!(
            path("stages.prod.jobs.migrate.tasks"),
            Some("jobs.<name>.tasks")
        );
        assert_eq!(
            path("service.secrets.DATABASE_URL.version"),
            Some("service.secrets.<NAME>.version")
        );
        assert_eq!(
            path("service.identity.roles[1].dataset"),
            Some("service.identity.roles.dataset")
        );
        assert_eq!(
            path("service.health_check.liveness.period_seconds"),
            Some("service.health_check.startup.period_seconds")
        );
        assert_eq!(path("stages.prod.release.flag"), Some("release.flag"));
        assert_eq!(path("stages.prod.service"), Some("stages.<name>.service"));
        assert_eq!(path("stages.prod"), Some("stages.<name>"));
        assert_eq!(path("schedules.nightly.job"), Some("schedules.<name>.job"));
        assert_eq!(path(":layers"), Some(":layers"));
        assert_eq!(path("variables"), Some(":variables"));
        assert_eq!(path("service.memroy"), None);
        assert!(suggest("service.memroy").contains(&"service.memory"));
        assert!(suggest("max_instance").contains(&"service.max_instances"));
    }

    #[test]
    fn docs_links_point_at_the_site() {
        let e = find("service.vpc").unwrap();
        assert_eq!(
            e.docs_url().as_deref(),
            Some(
                "https://runway.echaouchna.dev/docs/configuration/#cpu-sandboxes-networking-and-cloud-sql"
            )
        );
    }
}
