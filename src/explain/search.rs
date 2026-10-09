//! The search of `runway explain`: ranks keys and topics for what you type,
//! words or what you want to do ("keep an instance warm"), understanding
//! word forms, typos and the other words a key goes by (`also:` in
//! catalog.txt). Offline and deterministic.

use super::catalog::{Entry, catalog, find, is_placeholder};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::OnceLock;

/// What the tips of the search show, each verified by a test: the query
/// and the key it finds first.
pub const EXAMPLES: &[(&str, &str)] = &[
    ("keep an instance warm", "service.min_instances"),
    ("run every night", "schedules"),
    ("database password", "service.secrets"),
    ("custom domain", "service.domains"),
    ("ram", "service.memory"),
    ("memroy", "service.memory"),
];

/// At most this many results: past them, a better query helps more.
const LIMIT: usize = 40;

// How much a word of the query counts, by where it is found.
const ALSO_PHRASE: u32 = 110;
const NAME: u32 = 100;
const ALSO_WORD: u32 = 60;
const PATH: u32 = 50;
const GROUP: u32 = 40;
const SUMMARY: u32 = 30;
const TEXT: u32 = 15;
const EXAMPLE: u32 = 5;

/// Words that say nothing about a key ("how do I …").
const STOP: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "by", "can", "could", "do", "does", "for",
    "from", "get", "have", "how", "i", "if", "in", "into", "is", "it", "its", "let", "me", "my",
    "need", "of", "on", "or", "our", "should", "so", "some", "that", "the", "their", "them",
    "there", "this", "to", "use", "want", "way", "we", "what", "when", "where", "which", "who",
    "why", "will", "with", "would", "you", "your",
];

/// A key or topic found, and why when its name does not say it.
#[derive(Debug, Clone)]
pub struct Hit {
    pub entry: &'static Entry,
    /// `for "keep warm"`, `"memroy" ≈ memory`, `in the explanation`.
    pub why: Option<String>,
    covered: usize,
    score: u32,
    order: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Src {
    Name,
    Also(&'static str),
    Group,
    Text,
}

#[derive(Debug)]
struct Word {
    stem: String,
    raw: String,
    weight: u32,
    src: Src,
}

#[derive(Debug)]
struct Doc {
    entry: &'static Entry,
    order: usize,
    depth: usize,
    words: Vec<Word>,
    /// Each `also:` term with the stems of its meaningful words.
    phrases: Vec<(&'static str, Vec<String>)>,
}

#[derive(Debug, Clone)]
enum Reason {
    Also(&'static str),
    Typo(String, String),
    Name,
    Group(&'static str),
    Text,
}

impl Reason {
    fn rank(&self) -> u8 {
        match self {
            Reason::Also(_) => 0,
            Reason::Typo(..) => 1,
            Reason::Name => 2,
            Reason::Group(_) => 3,
            Reason::Text => 4,
        }
    }
}

fn index() -> &'static [Doc] {
    static INDEX: OnceLock<Vec<Doc>> = OnceLock::new();
    INDEX.get_or_init(|| catalog().iter().enumerate().map(doc).collect())
}

fn doc((order, e): (usize, &'static Entry)) -> Doc {
    let mut words: HashMap<String, Word> = HashMap::new();
    let mut add = |text: &str, weight: u32, src: Src| {
        for raw in words_of(text) {
            let stem = stem(&raw);
            if words.get(&stem).is_none_or(|w| w.weight < weight) {
                words.insert(
                    stem.clone(),
                    Word {
                        stem,
                        raw,
                        weight,
                        src,
                    },
                );
            }
        }
    };
    let all = e.segments();
    let segs: Vec<&str> = all.iter().copied().filter(|s| !is_placeholder(s)).collect();
    if e.is_topic() {
        add(e.name(), NAME, Src::Name);
        add(&e.path, PATH, Src::Name);
    } else {
        // `secrets.<key>` is one secret, not the secrets: only its parent's
        // path, so that `secrets` comes first.
        let named = all.last().is_some_and(|s| !is_placeholder(s));
        for (i, seg) in segs.iter().enumerate() {
            let last = named && i + 1 == segs.len();
            add(seg, if last { NAME } else { PATH }, Src::Name);
        }
    }
    let mut phrases = Vec::new();
    for term in &e.also {
        let words: Vec<String> = words_of(term)
            .into_iter()
            .filter(|w| !STOP.contains(&w.as_str()))
            .collect();
        for w in &words {
            add(w, ALSO_WORD, Src::Also(term.as_str()));
        }
        phrases.push((term.as_str(), words.iter().map(|w| stem(w)).collect()));
    }
    if let Some(g) = &e.group {
        add(g, GROUP, Src::Group);
    }
    add(e.summary(), SUMMARY, Src::Text);
    for t in e.text.iter().skip(1).chain(&e.rules) {
        add(t, TEXT, Src::Text);
    }
    for t in [&e.kind, &e.default].into_iter().flatten() {
        add(t, TEXT, Src::Text);
    }
    for t in &e.example {
        add(t, EXAMPLE, Src::Text);
    }
    Doc {
        entry: e,
        order,
        depth: segs.len(),
        words: words.into_values().collect(),
        phrases,
    }
}

/// Keys and topics for `query`, best first.
pub fn search(query: &str) -> Vec<Hit> {
    search_among(query, |_| true)
}

/// Like `search`, among the entries `eligible` keeps: they are chosen
/// before ranking, so that the best of them are kept, not only those
/// among the best of all.
pub fn search_among(query: &str, eligible: impl Fn(&Entry) -> bool) -> Vec<Hit> {
    let all = words_of(query);
    let mut tokens: Vec<String> = all
        .iter()
        .filter(|w| !STOP.contains(&w.as_str()))
        .cloned()
        .collect();
    if tokens.is_empty() {
        tokens = all;
    }
    let mut seen = std::collections::HashSet::new();
    tokens.retain(|t| seen.insert(stem(t)));
    if tokens.is_empty() {
        return Vec::new();
    }
    let stems: Vec<String> = tokens.iter().map(|t| stem(t)).collect();
    // A key as written in a file (`stages.prod.services.web.memory`).
    let exact = query
        .contains('.')
        .then(|| find(query))
        .flatten()
        .filter(|e| !e.is_topic());
    let mut hits: Vec<Hit> = index()
        .iter()
        .filter(|d| eligible(d.entry))
        .filter_map(|d| score(d, &tokens, &stems, exact))
        .collect();
    hits.sort_by_key(|h| {
        (
            Reverse(h.covered),
            Reverse(h.score),
            index()[h.order].depth,
            h.order,
        )
    });
    // Keep what is about as relevant as the best: the rest is noise.
    let is_exact = |h: &Hit| exact.is_some_and(|e| std::ptr::eq(e, h.entry));
    let best_covered = hits.iter().map(|h| h.covered).max().unwrap_or(0);
    let best_score = hits
        .iter()
        .filter(|h| !is_exact(h))
        .map(|h| h.score)
        .max()
        .unwrap_or(0);
    hits.retain(|h| is_exact(h) || (h.covered + 1 >= best_covered && h.score * 6 >= best_score));
    hits.truncate(LIMIT);
    hits
}

fn score(d: &Doc, tokens: &[String], stems: &[String], exact: Option<&Entry>) -> Option<Hit> {
    let mut best: Vec<Option<(u32, Reason)>> = vec![None; stems.len()];
    let mut offer = |i: usize, weight: u32, reason: Reason| {
        if best[i].as_ref().is_none_or(|(w, _)| *w < weight) {
            best[i] = Some((weight, reason));
        }
    };
    // An `also:` term whose every word is in the query: its strongest sign.
    for (term, words) in &d.phrases {
        if !words.is_empty() && words.iter().all(|w| stems.contains(w)) {
            for (i, s) in stems.iter().enumerate() {
                if words.contains(s) {
                    offer(i, ALSO_PHRASE, Reason::Also(term));
                }
            }
        }
    }
    for (i, (q, typed)) in stems.iter().zip(tokens).enumerate() {
        for w in &d.words {
            let reason = || match w.src {
                Src::Name => Reason::Name,
                Src::Also(term) => Reason::Also(term),
                Src::Group => Reason::Group(d.entry.group.as_deref().unwrap_or("")),
                Src::Text => Reason::Text,
            };
            if w.stem == *q {
                offer(i, w.weight, reason());
            } else if typed.len() >= 3 && w.raw.starts_with(typed) {
                // A word being typed.
                offer(i, w.weight * 7 / 10, reason());
            } else if matches!(w.src, Src::Name | Src::Also(_)) && is_typo(typed, &w.raw) {
                offer(i, w.weight / 2, Reason::Typo(typed.clone(), w.raw.clone()));
            }
        }
    }
    let is_exact = exact.is_some_and(|e| std::ptr::eq(e, d.entry));
    let covered = best.iter().filter(|b| b.is_some()).count();
    if covered == 0 && !is_exact {
        return None;
    }
    let mut score: u32 = best.iter().flatten().map(|(w, _)| *w).sum();
    let why = match is_exact {
        true => {
            score += 10_000;
            None
        }
        false => best
            .iter()
            .flatten()
            .map(|(_, r)| r)
            .min_by_key(|r| r.rank())
            .and_then(|r| match r {
                Reason::Also(t) => Some(format!("for \"{t}\"")),
                Reason::Typo(typed, word) => Some(format!("\"{typed}\" ≈ {word}")),
                Reason::Name => None,
                Reason::Group(g) => Some(format!("in {g}")),
                Reason::Text => Some("in the explanation".into()),
            }),
    };
    Some(Hit {
        entry: d.entry,
        why,
        covered: if is_exact { stems.len() } else { covered },
        score,
        order: d.order,
    })
}

/// Lowercase words: `max_instances` is `max` and `instances`.
fn words_of(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2 || w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// A rough English stem, so that `instances`, `instance`, `scheduled` and
/// `scheduling` meet: plurals, then `-ing`/`-ed`, then a final `e`.
fn stem(w: &str) -> String {
    let mut s = w.to_string();
    let len = |s: &str| s.chars().count();
    if s.ends_with("ies") && len(&s) > 4 {
        s.truncate(s.len() - 3);
        s.push('y');
    } else if ["ses", "xes", "zes", "ches", "shes"]
        .iter()
        .any(|x| s.ends_with(x))
        && len(&s) > 4
    {
        s.truncate(s.len() - 2);
    } else if s.ends_with('s')
        && len(&s) > 3
        && !["ss", "us", "is", "as"].iter().any(|x| s.ends_with(x))
    {
        s.pop();
    }
    for suffix in ["ing", "ed"] {
        if s.ends_with(suffix) && len(&s) > suffix.len() + 3 {
            s.truncate(s.len() - suffix.len());
            let b = s.as_bytes();
            if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] && !b"lsz".contains(&b[b.len() - 1])
            {
                s.pop();
            }
            break;
        }
    }
    if s.ends_with('e') && len(&s) > 3 {
        s.pop();
    }
    s
}

/// `typed` is `word` with a typo or two: one for short words, two for long.
fn is_typo(typed: &str, word: &str) -> bool {
    let n = typed.chars().count();
    let budget = match n {
        0..=3 => return false,
        4..=7 => 1,
        _ => 2,
    };
    let m = word.chars().count();
    m >= 4 && n.abs_diff(m) <= budget && edits(typed, word) <= budget
}

/// Edits between two words, a swap of neighbours counting as one.
fn edits(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(q: &str) -> Option<&'static str> {
        search(q).first().map(|h| h.entry.path.as_str())
    }

    fn paths(q: &str) -> Vec<&'static str> {
        search(q).iter().map(|h| h.entry.path.as_str()).collect()
    }

    #[test]
    fn the_examples_shown_find_what_they_promise() {
        for (q, want) in EXAMPLES {
            assert_eq!(first(q), Some(*want), "{q}: {:?}", paths(q));
        }
    }

    #[test]
    fn what_you_want_to_do_finds_the_key() {
        for (q, want) in [
            ("how do I avoid cold starts", "service.min_instances"),
            ("limit autoscaling", "service.max_instances"),
            ("connect to postgres", "service.cloud_sql"),
            ("env vars", "service.env"),
            ("cron", "schedules"),
            ("api keys", "service.secrets"),
            ("only people of my company sign in", "service.iap"),
            ("allow unauthenticated", "service.public"),
            ("static outbound ip", "service.vpc.egress"),
            ("delete old files", "buckets.<key>.delete_after_days"),
            ("override per stage", ":layers"),
            ("tracing", "service.otel_collector"),
            ("database migration", "jobs"),
        ] {
            assert_eq!(first(q), Some(want), "{q}: {:?}", paths(q));
        }
    }

    #[test]
    fn names_word_forms_and_keys_as_written() {
        assert_eq!(first("memory"), Some("service.memory"));
        assert_eq!(first("max_instances"), Some("service.max_instances"));
        assert_eq!(first("instances maximum"), Some("service.max_instances"));
        assert_eq!(first("scheduling"), Some("schedules"));
        assert_eq!(first("egress"), Some("service.vpc.egress"));
        assert_eq!(first("precedence"), Some(":layers"));
        assert_eq!(
            first("stages.prod.services.web.memory"),
            Some("service.memory")
        );
        assert_eq!(first("mem"), Some("service.memory"), "a word being typed");
        assert_eq!(first("timout"), Some("service.timeout_seconds"));
    }

    #[test]
    fn results_say_why_when_the_name_does_not() {
        let hit = |q: &str| search(q).into_iter().next().unwrap();
        assert_eq!(hit("memory").why, None);
        assert_eq!(hit("ram").why.as_deref(), Some("for \"ram\""));
        assert_eq!(
            hit("keep an instance warm").why.as_deref(),
            Some("for \"keep warm\"")
        );
        assert_eq!(hit("memroy").why.as_deref(), Some("\"memroy\" ≈ memory"));
    }

    #[test]
    fn noise_is_left_out() {
        assert!(search("").is_empty());
        assert!(search("zzqx").is_empty());
        let ram = paths("ram");
        assert!(ram.len() < 5, "{ram:?}");
        assert!(search("service").len() <= LIMIT);
    }

    #[test]
    fn only_eligible_entries_compete_for_the_results() {
        let wanted = |e: &Entry| e.path == "service.cloud_sql";
        assert!(
            !search("service").iter().any(|h| wanted(h.entry)),
            "outranked among all keys: the case this guards"
        );
        let found: Vec<&str> = search_among("service", |e| wanted(e) || e.path == "service")
            .iter()
            .map(|h| h.entry.path.as_str())
            .collect();
        assert_eq!(found, ["service", "service.cloud_sql"]);
        assert!(search_among("service", |_| false).is_empty());
    }

    #[test]
    fn stems_meet() {
        for (a, b) in [
            ("instances", "instance"),
            ("scheduled", "scheduling"),
            ("schedules", "schedule"),
            ("running", "run"),
            ("aliases", "alias"),
            ("caching", "cache"),
            ("policies", "policy"),
        ] {
            assert_eq!(stem(a), stem(b), "{a} {b}");
        }
        assert_eq!(stem("string"), "string");
        assert_eq!(stem("access"), "access");
        assert_eq!(stem("status"), "status");
        assert_eq!(edits("memroy", "memory"), 1);
        assert!(is_typo("memroy", "memory"));
        assert!(!is_typo("ram", "rum"), "short words are not guessed");
        assert!(!is_typo("call", "all"));
    }
}
