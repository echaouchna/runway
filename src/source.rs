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
