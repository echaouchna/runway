//! The text summary at the end of `deploy`: every service and job with
//! what changed in it, the traffic, and every provisioning step with what
//! it did. Nothing is summarized away; `-o json` has the same data.

use crate::deploy::{AccessChange, ServiceChange};
use crate::gcp::run::TrafficLine;
use crate::plan::FieldChange;
use crate::provision::{StepOutcome, StepResult};
use crate::style::Painter;

const LABEL: usize = 10;

/// `Label     value`, labels aligned.
pub fn field(c: &Painter, label: &str, value: &str) -> String {
    format!("  {} {value}\n", c.dim(&format!("{label:LABEL$}")))
}

pub fn heading(c: &Painter, title: &str) -> String {
    format!("\n{}\n", c.bold(title))
}

pub fn service_change(ch: ServiceChange) -> &'static str {
    match ch {
        ServiceChange::Created => "created",
        ServiceChange::Updated => "updated",
        ServiceChange::Unchanged => "unchanged",
    }
}

pub fn access(c: &Painter, iap: bool, public: bool, change: &AccessChange) -> String {
    let what = match (iap, public) {
        (true, _) => "Identity-Aware Proxy (signed-in members only)",
        (false, true) => "public (allUsers can invoke)",
        (false, false) => "private (requires an identity token with roles/run.invoker)",
    };
    match change {
        AccessChange::MadePublic => format!("{what} {}", c.green("(made public by this deploy)")),
        AccessChange::MadePrivate => {
            format!("{what} {}", c.yellow("(made private by this deploy)"))
        }
        AccessChange::Unchanged => what.to_string(),
    }
}

/// Every traffic entry: percentage, revision, tag and its URL.
pub fn traffic(c: &Painter, lines: &[TrafficLine], indent: &str) -> String {
    let mut s = String::new();
    for t in lines {
        let tag = match t.tag.is_empty() {
            true => String::new(),
            false => format!(
                "  tag {}{}",
                c.bold(&t.tag),
                match t.uri.is_empty() {
                    true => String::new(),
                    false => format!(" {}", c.cyan(&t.uri)),
                }
            ),
        };
        s.push_str(&format!("{indent}{:>3}%  {}{tag}\n", t.percent, t.revision));
    }
    s
}

/// One line per changed field, colored by kind.
pub fn changes(c: &Painter, changes: &[FieldChange], indent: &str) -> String {
    if changes.is_empty() {
        return format!("{indent}{}\n", c.dim("no configuration change"));
    }
    let mut s = String::new();
    for ch in changes {
        s.push_str(&match (&ch.before, &ch.after) {
            (Some(b), Some(a)) => format!(
                "{indent}{} {} -> {}\n",
                c.yellow(&format!("~ {}:", ch.field)),
                c.red(b),
                c.green(a)
            ),
            (None, Some(a)) => format!("{indent}{}\n", c.green(&format!("+ {}: {a}", ch.field))),
            (Some(b), None) => format!("{indent}{}\n", c.red(&format!("- {}: {b}", ch.field))),
            (None, None) => String::new(),
        });
    }
    s
}

/// `Steps (12: 3 changed, 9 already done)`, then each step and its details:
/// the changed ones first.
pub fn steps(c: &Painter, steps: &[StepResult]) -> String {
    if steps.is_empty() {
        return String::new();
    }
    let changed = steps
        .iter()
        .filter(|s| s.outcome == StepOutcome::Changed)
        .count();
    let mut s = heading(
        c,
        &format!(
            "Provisioning ({} step(s): {changed} changed, {} already done)",
            steps.len(),
            steps.len() - changed
        ),
    );
    let ordered = steps
        .iter()
        .filter(|s| s.outcome == StepOutcome::Changed)
        .chain(steps.iter().filter(|s| s.outcome == StepOutcome::Unchanged));
    for st in ordered {
        let (first, rest) = match st.detail.split_once('\n') {
            Some((a, b)) => (a, Some(b)),
            None => (st.detail.as_str(), None),
        };
        let line = match first.is_empty() {
            true => st.step.clone(),
            false => format!("{}: {first}", st.step),
        };
        s.push_str(&match st.outcome {
            StepOutcome::Changed => format!("  {} {line}\n", c.green("✓")),
            StepOutcome::Unchanged => format!("  {}\n", c.dim(&format!("= {line}"))),
        });
        for l in rest.unwrap_or("").lines().filter(|l| !l.trim().is_empty()) {
            let colored = match l.trim_start().chars().next() {
                Some('+') => c.green(l),
                Some('-') => c.red(l),
                Some('~') => c.yellow(l),
                _ => c.dim(l),
            };
            s.push_str(&format!("      {colored}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Painter = Painter { enabled: false };

    #[test]
    fn steps_show_every_step_and_every_detail() {
        let st = |step: &str, outcome, detail: &str| StepResult {
            step: step.into(),
            outcome,
            detail: detail.into(),
        };
        let text = steps(
            &PLAIN,
            &[
                st(
                    "create bucket cache",
                    StepOutcome::Unchanged,
                    "already exists",
                ),
                st(
                    "custom domains",
                    StepOutcome::Changed,
                    "+ URL map shop-prod-lb\n- DNS A old.example.com",
                ),
                st("grant roles/run.invoker", StepOutcome::Changed, "granted"),
            ],
        );
        assert_eq!(
            text,
            "\nProvisioning (3 step(s): 2 changed, 1 already done)\n  ✓ custom domains: + URL map shop-prod-lb\n      - DNS A old.example.com\n  ✓ grant roles/run.invoker: granted\n  = create bucket cache: already exists\n"
        );
    }

    #[test]
    fn changes_and_traffic_are_listed_in_full() {
        let ch = |f: &str, b: Option<&str>, a: Option<&str>| FieldChange {
            field: f.into(),
            before: b.map(String::from),
            after: a.map(String::from),
        };
        let text = changes(
            &PLAIN,
            &[
                ch("memory", Some("512Mi"), Some("1Gi")),
                ch("env.NEW", None, Some("1")),
                ch("env.OLD", Some("x"), None),
            ],
            "  ",
        );
        assert_eq!(
            text,
            "  ~ memory: 512Mi -> 1Gi\n  + env.NEW: 1\n  - env.OLD: x\n"
        );
        assert_eq!(changes(&PLAIN, &[], "  "), "  no configuration change\n");
        let t = traffic(
            &PLAIN,
            &[
                TrafficLine {
                    revision: "shop-prod-00002-abc".into(),
                    percent: 90,
                    tag: String::new(),
                    uri: String::new(),
                },
                TrafficLine {
                    revision: "shop-prod-00003-def".into(),
                    percent: 10,
                    tag: "canary".into(),
                    uri: "https://canary---shop.example.com".into(),
                },
            ],
            "  ",
        );
        assert_eq!(
            t,
            "   90%  shop-prod-00002-abc\n   10%  shop-prod-00003-def  tag canary https://canary---shop.example.com\n"
        );
    }
}
