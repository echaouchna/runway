//! Traffic split: full rollouts, previews (a tagged URL, no traffic) and
//! canaries (a percentage of traffic).
//!
//! There is no state file: the split is always derived from the live
//! service's traffic, its latest ready revision and the requested mode. Entries
//! that follow the *latest* revision are pinned to the revision currently
//! serving before a preview or canary is added, so that a later preview never
//! receives production traffic.

use serde::Serialize;
use std::collections::BTreeMap;

/// Tag carried by the canary revision (`https://canary---SERVICE...`).
pub const CANARY_TAG: &str = "canary";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// The latest ready revision (moves with every new revision).
    Latest,
    /// A revision, by short name.
    Revision(String),
}

impl Target {
    pub fn display(&self) -> &str {
        match self {
            Target::Latest => "latest",
            Target::Revision(r) => r,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub target: Target,
    pub percent: u32,
    /// Empty when untagged.
    pub tag: String,
}

impl Entry {
    pub fn new(target: Target, percent: u32, tag: impl Into<String>) -> Self {
        Self {
            target,
            percent,
            tag: tag.into(),
        }
    }
}

/// What a deploy does with traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Mode {
    /// All traffic to the new revision; existing tags are kept.
    #[default]
    Full,
    /// No traffic; the new revision is reachable through its tag URL.
    Preview { tag: String },
    /// `percent` of the traffic to the new revision, tagged `canary`.
    Canary { percent: u32 },
}

/// Live traffic of a service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Current {
    pub entries: Vec<Entry>,
    /// Short name of the latest ready revision (empty if none).
    pub latest_ready: String,
}

impl Current {
    /// Entries following `latest` are pinned to the revision serving now.
    fn pinned(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .map(|e| match &e.target {
                Target::Latest if !self.latest_ready.is_empty() => Entry {
                    target: Target::Revision(self.latest_ready.clone()),
                    ..e.clone()
                },
                _ => e.clone(),
            })
            .collect()
    }

    /// Tagged entries (at 0%) except the given tags.
    fn tags_except(pinned: &[Entry], skip: &[&str]) -> Vec<Entry> {
        pinned
            .iter()
            .filter(|e| !e.tag.is_empty() && !skip.contains(&e.tag.as_str()))
            .map(|e| Entry::new(e.target.clone(), 0, e.tag.clone()))
            .collect()
    }

    /// Target a tag (or `latest`, or a revision name) refers to.
    pub fn resolve(&self, name: &str, service_id: &str) -> Option<Target> {
        if name == "latest" {
            return Some(Target::Latest);
        }
        if let Some(e) = self.pinned().into_iter().find(|e| e.tag == name) {
            return Some(e.target);
        }
        let full = if name.starts_with(&format!("{service_id}-")) {
            name.to_string()
        } else {
            format!("{service_id}-{name}")
        };
        Some(Target::Revision(full))
    }

    /// The tag's URL target, if the tag exists.
    pub fn has_tag(&self, tag: &str) -> bool {
        self.entries.iter().any(|e| e.tag == tag)
    }
}

/// Traffic after deploying a new revision in `mode`. `current` is `None` when
/// the service does not exist yet (the new revision then gets everything).
pub fn plan(current: Option<&Current>, mode: &Mode) -> Vec<Entry> {
    plan_with(current, mode, None)
}

/// [`plan`] with what the mode's URL should serve: `None` for a new revision
/// (entries following `latest` are pinned first, since it moves), or the
/// target already behind that URL when its revision runs the desired
/// configuration (no revision is created, so nothing needs pinning).
pub fn plan_with(current: Option<&Current>, mode: &Mode, serve: Option<Target>) -> Vec<Entry> {
    let Some(cur) = current else {
        return vec![Entry::new(
            Target::Latest,
            100,
            match mode {
                Mode::Preview { tag } => tag.clone(),
                _ => String::new(),
            },
        )];
    };
    let (pinned, serve) = match serve {
        None => (cur.pinned(), Target::Latest),
        Some(target) => (cur.entries.clone(), target),
    };
    let out = match mode {
        Mode::Full => {
            let mut v = vec![Entry::new(serve, 100, "")];
            v.extend(Current::tags_except(&pinned, &[CANARY_TAG]));
            v
        }
        Mode::Preview { tag } => {
            let mut v: Vec<Entry> = pinned
                .iter()
                .filter(|e| e.percent > 0)
                .map(|e| Entry {
                    tag: if &e.tag == tag {
                        String::new()
                    } else {
                        e.tag.clone()
                    },
                    ..e.clone()
                })
                .collect();
            v.extend(Current::tags_except(
                &pinned
                    .iter()
                    .filter(|e| e.percent == 0)
                    .cloned()
                    .collect::<Vec<_>>(),
                &[tag],
            ));
            v.push(Entry::new(serve, 0, tag.clone()));
            v
        }
        Mode::Canary { percent } => {
            // The previous canary (tagged) is replaced: its share goes back
            // to the rest before the new canary takes its percentage.
            let served: Vec<Entry> = pinned
                .iter()
                .filter(|e| e.percent > 0 && e.tag != CANARY_TAG)
                .cloned()
                .collect();
            if served.is_empty() {
                // Nothing else serves: the canary is the whole service.
                let mut v = vec![Entry::new(serve, 100, CANARY_TAG)];
                v.extend(Current::tags_except(&pinned, &[CANARY_TAG]));
                return normalize(v);
            }
            let mut v = scale(&served, 100 - percent);
            v.extend(Current::tags_except(
                &pinned
                    .iter()
                    .filter(|e| e.percent == 0)
                    .cloned()
                    .collect::<Vec<_>>(),
                &[CANARY_TAG],
            ));
            v.push(Entry::new(serve, *percent, CANARY_TAG));
            v
        }
    };
    normalize(out)
}

/// 100% to the canary revision (tags kept, the canary tag dropped).
pub fn promote(cur: &Current) -> Option<Vec<Entry>> {
    let pinned = cur.pinned();
    let canary = pinned.iter().find(|e| e.tag == CANARY_TAG)?.target.clone();
    let target = if canary == Target::Revision(cur.latest_ready.clone()) {
        Target::Latest
    } else {
        canary
    };
    let mut v = vec![Entry::new(target, 100, "")];
    v.extend(Current::tags_except(&pinned, &[CANARY_TAG]));
    Some(normalize(v))
}

/// An explicit split (targets and percentages summing to 100); tags are kept.
pub fn split(cur: &Current, split: &[(Target, u32)]) -> Result<Vec<Entry>, String> {
    let total: u32 = split.iter().map(|(_, p)| p).sum();
    if total != 100 {
        return Err(format!("percentages add up to {total}, not 100"));
    }
    let pinned = cur.pinned();
    let mut v: Vec<Entry> = split
        .iter()
        .map(|(t, p)| Entry::new(t.clone(), *p, ""))
        .collect();
    v.extend(Current::tags_except(&pinned, &[]));
    Ok(normalize(v))
}

/// Removes a tag (and its URL). Traffic it carried stays, untagged.
pub fn remove_tag(cur: &Current, tag: &str) -> Vec<Entry> {
    normalize(
        cur.entries
            .iter()
            .map(|e| Entry {
                tag: if e.tag == tag {
                    String::new()
                } else {
                    e.tag.clone()
                },
                ..e.clone()
            })
            .collect(),
    )
}

/// Scales percentages to `to` (largest remainders get the leftover points).
fn scale(entries: &[Entry], to: u32) -> Vec<Entry> {
    let total: u32 = entries.iter().map(|e| e.percent).sum();
    if total == 0 {
        return entries.to_vec();
    }
    let mut out: Vec<(Entry, u32)> = entries
        .iter()
        .map(|e| {
            let exact = e.percent * to;
            (
                Entry {
                    percent: exact / total,
                    ..e.clone()
                },
                exact % total,
            )
        })
        .collect();
    let mut left = to - out.iter().map(|(e, _)| e.percent).sum::<u32>();
    let mut order: Vec<usize> = (0..out.len()).collect();
    order.sort_by(|a, b| out[*b].1.cmp(&out[*a].1).then(a.cmp(b)));
    for i in order {
        if left == 0 {
            break;
        }
        out[i].0.percent += 1;
        left -= 1;
    }
    out.into_iter().map(|(e, _)| e).collect()
}

/// Merges untagged entries with the same target, drops empty untagged ones
/// and orders the result (serving first).
pub fn normalize(entries: Vec<Entry>) -> Vec<Entry> {
    let mut untagged: BTreeMap<Target, u32> = BTreeMap::new();
    let mut tagged: Vec<Entry> = Vec::new();
    for e in entries {
        if e.tag.is_empty() {
            *untagged.entry(e.target).or_default() += e.percent;
        } else if !tagged.iter().any(|t| t.tag == e.tag) {
            tagged.push(e);
        }
    }
    let mut out: Vec<Entry> = untagged
        .into_iter()
        .filter(|(_, p)| *p > 0)
        .map(|(t, p)| Entry::new(t, p, ""))
        .collect();
    out.extend(tagged);
    out.sort_by(|a, b| {
        b.percent
            .cmp(&a.percent)
            .then_with(|| a.tag.cmp(&b.tag))
            .then_with(|| a.target.cmp(&b.target))
    });
    out
}

/// Flat, comparable representation (desired and observed alike):
/// `traffic` = the split, `traffic.tags.NAME` = what each tag points at.
pub fn flat(entries: &[Entry]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut serving: Vec<(String, u32)> = entries
        .iter()
        .filter(|e| e.percent > 0)
        .map(|e| (e.target.display().to_string(), e.percent))
        .collect();
    serving.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    m.insert(
        "traffic".into(),
        if serving.is_empty() {
            "100% latest".into()
        } else {
            serving
                .iter()
                .map(|(t, p)| format!("{p}% {t}"))
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    for e in entries.iter().filter(|e| !e.tag.is_empty()) {
        m.insert(
            format!("traffic.tags.{}", e.tag),
            e.target.display().to_string(),
        );
    }
    m
}

/// Longest tag such that `TAG---SERVICE-PROJECTNUMBER` (or the older
/// `TAG---SERVICE-HASH-REGION`) stays a valid 63-character DNS label.
pub fn max_tag_len(service_id: &str) -> usize {
    46usize.saturating_sub(service_id.len())
}

/// A valid revision tag from a branch name (`feature/Login_v2` ->
/// `feature-login-v2`), shortened with a hash suffix when too long.
pub fn preview_tag(raw: &str, service_id: &str) -> Result<String, String> {
    let max = max_tag_len(service_id);
    if max < 8 {
        return Err(format!(
            "service name `{service_id}` is too long for tagged URLs (tag + service must fit in 46 characters)"
        ));
    }
    let mut t = String::new();
    for c in raw.trim().to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            t.push(c);
        } else if !t.ends_with('-') {
            t.push('-');
        }
    }
    let mut t = t.trim_matches('-').to_string();
    if t.is_empty() {
        return Err(format!("`{raw}` does not contain any letter or digit"));
    }
    if !t.starts_with(|c: char| c.is_ascii_lowercase()) {
        t = format!("b-{t}");
    }
    if t == CANARY_TAG || t == "latest" {
        return Err(format!(
            "`{t}` is reserved (used for canary releases / the latest revision)"
        ));
    }
    if t.len() > max {
        use sha2::Digest;
        let h = crate::build::package::hex(&sha2::Sha256::digest(raw.as_bytes()));
        t.truncate(max - 5);
        t = format!("{}-{}", t.trim_end_matches('-'), &h[..4]);
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(r: &str) -> Target {
        Target::Revision(r.into())
    }

    fn cur(entries: Vec<Entry>, ready: &str) -> Current {
        Current {
            entries,
            latest_ready: ready.into(),
        }
    }

    #[test]
    fn serving_the_existing_target_leaves_traffic_unchanged() {
        // Production on s-1, feat-a's revision s-2, feat-b latest (s-3).
        let live = cur(
            vec![
                Entry::new(rev("s-1"), 100, ""),
                Entry::new(rev("s-2"), 0, "feat-a"),
                Entry::new(Target::Latest, 0, "feat-b"),
            ],
            "s-3",
        );
        // feat-a again, its revision already runs this configuration.
        let feat_a = Mode::Preview {
            tag: "feat-a".into(),
        };
        assert_eq!(
            plan_with(Some(&live), &feat_a, Some(rev("s-2"))),
            live.entries
        );
        // The main deploy, s-1 already serves it: nothing moves.
        assert_eq!(
            plan_with(Some(&live), &Mode::Full, Some(rev("s-1"))),
            live.entries
        );
        // Without a matching revision, feat-a moves to a new one.
        assert_eq!(
            plan(Some(&live), &feat_a),
            [
                Entry::new(rev("s-1"), 100, ""),
                Entry::new(Target::Latest, 0, "feat-a"),
                Entry::new(rev("s-3"), 0, "feat-b"),
            ]
        );
        // A canary kept at the same percentage; a new percentage only
        // rebalances traffic between the existing revisions.
        let canary = cur(
            vec![
                Entry::new(rev("s-1"), 90, ""),
                Entry::new(Target::Latest, 10, CANARY_TAG),
            ],
            "s-4",
        );
        let at = |percent| Mode::Canary { percent };
        assert_eq!(
            plan_with(Some(&canary), &at(10), Some(Target::Latest)),
            canary.entries
        );
        assert_eq!(
            plan_with(Some(&canary), &at(25), Some(Target::Latest)),
            [
                Entry::new(rev("s-1"), 75, ""),
                Entry::new(Target::Latest, 25, CANARY_TAG),
            ]
        );
    }

    #[test]
    fn new_service_gets_everything() {
        assert_eq!(
            plan(None, &Mode::Full),
            [Entry::new(Target::Latest, 100, "")]
        );
        assert_eq!(
            plan(None, &Mode::Preview { tag: "b".into() }),
            [Entry::new(Target::Latest, 100, "b")]
        );
    }

    #[test]
    fn full_rollout_keeps_tags_and_is_stable() {
        let c = cur(vec![Entry::new(Target::Latest, 100, "")], "s-1");
        let p = plan(Some(&c), &Mode::Full);
        assert_eq!(p, [Entry::new(Target::Latest, 100, "")]);
        assert_eq!(flat(&p)["traffic"], "100% latest");

        // After previews: production is pinned, tags point at revisions.
        let c = cur(
            vec![
                Entry::new(rev("s-1"), 100, ""),
                Entry::new(rev("s-2"), 0, "feat-a"),
                Entry::new(Target::Latest, 0, "feat-b"),
            ],
            "s-3",
        );
        let p = plan(Some(&c), &Mode::Full);
        assert_eq!(
            p,
            [
                Entry::new(Target::Latest, 100, ""),
                Entry::new(rev("s-2"), 0, "feat-a"),
                Entry::new(rev("s-3"), 0, "feat-b"),
            ]
        );
        // Re-planning from the result changes nothing.
        let again = plan(Some(&cur(p.clone(), "s-4")), &Mode::Full);
        assert_eq!(flat(&again), flat(&p));
    }

    #[test]
    fn preview_pins_production_and_tags_the_new_revision() {
        let c = cur(vec![Entry::new(Target::Latest, 100, "")], "s-1");
        let p = plan(
            Some(&c),
            &Mode::Preview {
                tag: "feat-a".into(),
            },
        );
        assert_eq!(
            p,
            [
                Entry::new(rev("s-1"), 100, ""),
                Entry::new(Target::Latest, 0, "feat-a"),
            ]
        );
        // A second branch pins the first one's revision.
        let p2 = plan(
            Some(&cur(p.clone(), "s-2")),
            &Mode::Preview {
                tag: "feat-b".into(),
            },
        );
        assert_eq!(
            p2,
            [
                Entry::new(rev("s-1"), 100, ""),
                Entry::new(rev("s-2"), 0, "feat-a"),
                Entry::new(Target::Latest, 0, "feat-b"),
            ]
        );
        // Redeploying the same branch without changes is stable.
        let again = plan(
            Some(&cur(p2.clone(), "s-3")),
            &Mode::Preview {
                tag: "feat-b".into(),
            },
        );
        assert_eq!(again, p2);
    }

    #[test]
    fn canary_takes_a_share_of_what_serves() {
        let c = cur(vec![Entry::new(Target::Latest, 100, "")], "s-1");
        let p = plan(Some(&c), &Mode::Canary { percent: 10 });
        assert_eq!(
            p,
            [
                Entry::new(rev("s-1"), 90, ""),
                Entry::new(Target::Latest, 10, CANARY_TAG),
            ]
        );
        // Unchanged redeploy (latest ready is the canary): stable.
        let again = plan(Some(&cur(p.clone(), "s-2")), &Mode::Canary { percent: 10 });
        assert_eq!(flat(&again), flat(&p));
        // Raising the share.
        let p25 = plan(Some(&cur(p.clone(), "s-2")), &Mode::Canary { percent: 25 });
        assert_eq!(flat(&p25)["traffic"], "75% s-1, 25% latest");
        // A new canary replaces the previous one.
        let c3 = cur(
            vec![
                Entry::new(rev("s-1"), 90, ""),
                Entry::new(rev("s-2"), 10, CANARY_TAG),
            ],
            "s-2",
        );
        let p3 = plan(Some(&c3), &Mode::Canary { percent: 10 });
        assert_eq!(flat(&p3)["traffic"], "90% s-1, 10% latest");
        // Promote: everything to the canary.
        let pr = promote(&cur(p.clone(), "s-2")).unwrap();
        assert_eq!(pr, [Entry::new(Target::Latest, 100, "")]);
        assert!(promote(&cur(vec![Entry::new(Target::Latest, 100, "")], "s-1")).is_none());
    }

    #[test]
    fn scaling_keeps_the_total() {
        let s = scale(
            &[
                Entry::new(rev("a"), 33, ""),
                Entry::new(rev("b"), 33, ""),
                Entry::new(rev("c"), 34, ""),
            ],
            90,
        );
        assert_eq!(s.iter().map(|e| e.percent).sum::<u32>(), 90);
    }

    #[test]
    fn split_and_remove_tag() {
        let c = cur(
            vec![
                Entry::new(Target::Latest, 100, ""),
                Entry::new(rev("s-2"), 0, "feat-a"),
            ],
            "s-3",
        );
        let s = split(&c, &[(rev("s-1"), 50), (Target::Latest, 50)]).unwrap();
        assert_eq!(flat(&s)["traffic"], "50% latest, 50% s-1");
        assert_eq!(flat(&s)["traffic.tags.feat-a"], "s-2");
        assert!(split(&c, &[(rev("s-1"), 50)]).is_err());
        let r = remove_tag(&c, "feat-a");
        assert_eq!(r, [Entry::new(Target::Latest, 100, "")]);
        assert_eq!(c.resolve("feat-a", "s"), Some(rev("s-2")));
        assert_eq!(c.resolve("00007-abc", "s"), Some(rev("s-00007-abc")));
    }

    #[test]
    fn preview_tags_from_branch_names() {
        assert_eq!(
            preview_tag("feature/Login_v2", "gcptree-prod").unwrap(),
            "feature-login-v2"
        );
        assert_eq!(preview_tag("123-fix", "svc").unwrap(), "b-123-fix");
        assert!(preview_tag("canary", "svc").is_err());
        assert!(preview_tag("///", "svc").is_err());
        let long = preview_tag(
            "feature/a-very-long-branch-name-that-goes-on-and-on",
            "gcptree-prod",
        )
        .unwrap();
        assert!(long.len() <= max_tag_len("gcptree-prod"), "{long}");
        assert!(!long.ends_with('-'));
    }
}
