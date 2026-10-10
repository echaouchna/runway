//! The commit a deploy comes from, recorded on the stage's holder
//! ([`ANNOTATION_SOURCE`]), so that a deploy of an older commit than the one
//! already serving is refused (two pipelines finishing out of order, an old
//! branch deployed by mistake). `--allow-older` rolls back on purpose.

use crate::error::{Error, ErrorKind, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

pub const ANNOTATION_SOURCE: &str = "runway.dev/source";

/// A commit: its id and its committer date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub commit: String,
    pub time: DateTime<Utc>,
    /// The checkout had uncommitted changes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
}

impl Source {
    pub fn short(&self) -> &str {
        &self.commit[..self.commit.len().min(12)]
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("serializes")
    }

    pub fn decode(v: Option<&str>) -> Option<Source> {
        serde_json::from_str(v?).ok()
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "commit {} of {}{}",
            self.short(),
            self.time.format("%Y-%m-%d %H:%M:%S UTC"),
            if self.dirty {
                ", with uncommitted changes"
            } else {
                ""
            }
        )
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit of the checkout holding `dir`. `RUNWAY_SOURCE_COMMIT` and
/// `RUNWAY_SOURCE_TIME` (RFC 3339 or Unix seconds) replace git, for builds
/// without a checkout. `None`: not a git checkout (no guard).
pub fn current(dir: &Path) -> Option<Source> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let (Some(commit), Some(time)) = (env("RUNWAY_SOURCE_COMMIT"), env("RUNWAY_SOURCE_TIME")) {
        let time = DateTime::parse_from_rfc3339(time.trim())
            .map(|t| t.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                time.trim()
                    .parse::<i64>()
                    .ok()
                    .and_then(|s| DateTime::from_timestamp(s, 0))
            })?;
        return Some(Source {
            commit: commit.trim().to_string(),
            time,
            dirty: false,
        });
    }
    let line = git(dir, &["log", "-1", "--format=%H %ct", "HEAD"])?;
    let (commit, secs) = line.split_once(' ')?;
    Some(Source {
        commit: commit.to_string(),
        time: DateTime::from_timestamp(secs.parse().ok()?, 0)?,
        dirty: git(dir, &["status", "--porcelain", "--untracked-files=no"])
            .is_some_and(|s| !s.is_empty()),
    })
}

/// The git tags on `commit` (HEAD when unknown): the CI's tag when the
/// pipeline runs for one (GitLab `CI_COMMIT_TAG`, GitHub `GITHUB_REF_NAME`
/// with `GITHUB_REF_TYPE=tag`), else `git tag --points-at`.
pub fn tags(dir: &Path, commit: Option<&str>) -> Vec<String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(t) = env("CI_COMMIT_TAG") {
        return vec![t];
    }
    if env("GITHUB_REF_TYPE").as_deref() == Some("tag")
        && let Some(t) = env("GITHUB_REF_NAME")
    {
        return vec![t];
    }
    git(dir, &["tag", "--points-at", commit.unwrap_or("HEAD")])
        .map(|out| out.lines().map(String::from).collect())
        .unwrap_or_default()
}

/// `X.Y.Z` of a version written `X.Y.Z` or `vX.Y.Z`, with any `-rc.1` or
/// `+build` suffix dropped; `None` for anything else.
fn version_core(s: &str) -> Option<&str> {
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let core = s.split(['-', '+']).next()?;
    let parts: Vec<&str> = core.split('.').collect();
    (parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())))
    .then_some(core)
}

/// On a commit tagged with a version, the changelog's `version` must be it:
/// a release named after the changelog must not contradict the git tag.
/// Tags that are not versions are ignored.
pub fn check_tag_version(version: &str, tags: &[String]) -> Result<()> {
    let versions: Vec<&String> = tags.iter().filter(|t| version_core(t).is_some()).collect();
    if versions.is_empty()
        || versions
            .iter()
            .any(|t| version_core(t) == version_core(version))
    {
        return Ok(());
    }
    Err(Error::config(format!(
        "the commit is tagged {}, but the changelog's version is {version}",
        versions
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .hint("make the changelog's latest version match the tag (or tag the commit with the changelog's version)")
    .permanent())
}

/// How `ours` relates to what is deployed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Order {
    Same,
    Newer,
    /// Older: the reason (ancestry or dates).
    Older(String),
    /// Neither (another branch): the dates decide, and they do not.
    Unordered,
}

/// Compares `ours` with `live`: by ancestry when the checkout knows both
/// commits, else by commit date.
pub fn order(dir: &Path, ours: &Source, live: &Source) -> Order {
    if ours.commit == live.commit {
        return Order::Same;
    }
    let known = |c: &str| git(dir, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_some();
    let ancestor = |a: &str, b: &str| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["merge-base", "--is-ancestor", a, b])
            .status()
            .ok()
            .map(|s| s.success())
    };
    if known(&ours.commit) && known(&live.commit) {
        if ancestor(&ours.commit, &live.commit) == Some(true) {
            return Order::Older(format!(
                "{} is an ancestor of the deployed commit",
                ours.short()
            ));
        }
        if ancestor(&live.commit, &ours.commit) == Some(true) {
            return Order::Newer;
        }
    }
    match ours.time.cmp(&live.time) {
        std::cmp::Ordering::Less => Order::Older("its commit date is earlier".into()),
        std::cmp::Ordering::Greater => Order::Newer,
        std::cmp::Ordering::Equal => Order::Unordered,
    }
}

/// Refuses a deploy of an older commit than the one deployed (unless
/// `allow_older`).
pub fn check(
    dir: &Path,
    ours: Option<&Source>,
    live: Option<&Source>,
    stage: &str,
    allow_older: bool,
) -> Result<()> {
    let (Some(ours), Some(live)) = (ours, live) else {
        return Ok(());
    };
    match order(dir, ours, live) {
        Order::Older(why) if !allow_older => Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "refusing to deploy {ours} to stage {stage}: it is older than the deployed {live} ({why})"
            ),
        )
        .permanent()
        .hint("deploy the newer commit (for example after pulling), or pass `--allow-older` to roll back on purpose")),
        _ => Ok(()),
    }
}

/// What `plan` says about the commit a deploy would come from.
pub fn plan_note(dir: &Path, ours: Option<&Source>, live: Option<&Source>) -> Option<String> {
    Some(match (ours, live) {
        (None, None) => return None,
        (None, Some(l)) => format!(
            "this is not a git checkout: what it deploys is not compared with the deployed {l}"
        ),
        (Some(o), None) => format!("deploys {o} (no commit recorded on the stage yet)"),
        (Some(o), Some(l)) => match order(dir, o, l) {
            Order::Same => format!("deploys {o}, the commit that already serves"),
            Order::Newer => format!("deploys {o}, newer than the deployed {l}"),
            Order::Older(why) => format!(
                "deploy would be REFUSED: this checkout ({o}) is older than the deployed {l} ({why}); `--allow-older` rolls back on purpose"
            ),
            Order::Unordered => {
                format!("deploys {o}; the deployed {l} has the same commit date (not compared)")
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_tag_must_match_the_changelog() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(check_tag_version("1.2.0", &t(&["v1.2.0"])).is_ok());
        assert!(check_tag_version("v1.2.0", &t(&["1.2.0", "latest"])).is_ok());
        assert!(
            check_tag_version("1.2.0", &t(&["v1.2.0-rc.1"])).is_ok(),
            "a candidate of it"
        );
        assert!(
            check_tag_version("1.2.0", &t(&["latest", "prod"])).is_ok(),
            "not versions"
        );
        assert!(check_tag_version("1.2.0", &[]).is_ok(), "not on a tag");
        let e = check_tag_version("1.2.0", &t(&["v1.3.0"])).unwrap_err();
        assert!(
            e.message.contains("tagged v1.3.0") && e.message.contains("version is 1.2.0"),
            "{}",
            e.message
        );
        assert!(e.permanent);
    }

    fn s(commit: &str, secs: i64) -> Source {
        Source {
            commit: commit.into(),
            time: DateTime::from_timestamp(secs, 0).unwrap(),
            dirty: false,
        }
    }

    #[test]
    fn dates_decide_when_git_does_not_know_the_commits() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (s("aaaa", 1_000), s("bbbb", 2_000));
        assert!(matches!(order(dir.path(), &old, &new), Order::Older(_)));
        assert_eq!(order(dir.path(), &new, &old), Order::Newer);
        assert_eq!(order(dir.path(), &old, &old), Order::Same);
        let e = check(dir.path(), Some(&old), Some(&new), "prod", false).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Conflict);
        assert!(
            e.message.contains("older than the deployed commit bbbb"),
            "{}",
            e.message
        );
        assert!(
            check(dir.path(), Some(&old), Some(&new), "prod", true).is_ok(),
            "--allow-older"
        );
        assert!(check(dir.path(), Some(&new), Some(&old), "prod", false).is_ok());
        assert!(
            check(dir.path(), None, Some(&new), "prod", false).is_ok(),
            "no git: no guard"
        );
        assert!(
            check(dir.path(), Some(&old), None, "prod", false).is_ok(),
            "nothing recorded"
        );
    }

    #[test]
    fn ancestry_beats_dates() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let run = |args: &[&str], date: &str| {
            Command::new("git")
                .arg("-C")
                .arg(d)
                .args(args)
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q"], "2026-01-01T00:00:00Z") {
            return; // no git here
        }
        std::fs::write(d.join("a"), "1").unwrap();
        run(&["add", "a"], "2026-01-02T00:00:00Z");
        run(&["commit", "-q", "-m", "one"], "2026-01-02T00:00:00Z");
        let first = current(d).unwrap();
        // A child commit dated earlier (a rebase, a wrong clock): still newer.
        std::fs::write(d.join("a"), "2").unwrap();
        run(&["commit", "-q", "-am", "two"], "2025-12-01T00:00:00Z");
        let second = current(d).unwrap();
        assert!(second.time < first.time);
        assert_eq!(order(d, &second, &first), Order::Newer);
        assert!(matches!(order(d, &first, &second), Order::Older(why) if why.contains("ancestor")));
        std::fs::write(d.join("a"), "3").unwrap();
        assert!(current(d).unwrap().dirty);
        assert!(Source::decode(Some(&second.encode())) == Some(second));
    }
}
