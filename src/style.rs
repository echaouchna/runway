//! Terminal colors, used sparingly: status markers, diff lines, section
//! headers and key values. `auto` colors terminals and CI job logs that
//! render ANSI; `NO_COLOR`, `TERM=dumb` and JSON output disable colors,
//! `CLICOLOR_FORCE`/`FORCE_COLOR` force them, and `--color always|never`
//! overrides detection.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static STDOUT: AtomicBool = AtomicBool::new(false);
static STDERR: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

/// CI systems whose job logs render ANSI colors although output is piped.
const CI_WITH_COLORS: &[&str] = &[
    "GITLAB_CI",
    "GITHUB_ACTIONS",
    "GITEA_ACTIONS",
    "FORGEJO_ACTIONS",
    "BUILDKITE",
    "CIRCLECI",
    "TF_BUILD",
];

/// Decides once, at startup, whether stdout and stderr get colors.
pub fn init(mode: ColorMode, json_output: bool) {
    let env = |name: &str| std::env::var(name).ok();
    let (out, err) = match mode {
        ColorMode::Always => (true, true),
        ColorMode::Never => (false, false),
        ColorMode::Auto => (
            auto(
                env,
                std::io::stdout().is_terminal(),
                is_regular_file("/dev/stdout"),
            ),
            auto(
                env,
                std::io::stderr().is_terminal(),
                is_regular_file("/dev/stderr"),
            ),
        ),
    };
    STDOUT.store(out && !json_output, Ordering::Relaxed);
    STDERR.store(err, Ordering::Relaxed);
}

/// `--color auto` for one stream. Output redirected to a file stays plain in
/// CI so saved plans and logs do not contain escape codes.
fn auto(env: impl Fn(&str) -> Option<String>, terminal: bool, regular_file: bool) -> bool {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    let truthy = |name: &str| {
        env(name).is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
    };
    if set("NO_COLOR") {
        return false;
    }
    if truthy("CLICOLOR_FORCE") || truthy("FORCE_COLOR") {
        return true;
    }
    if env("TERM").as_deref() == Some("dumb") {
        return false;
    }
    terminal || (!regular_file && CI_WITH_COLORS.iter().any(|name| truthy(name)))
}

/// Unix only: elsewhere the path does not exist and the stream counts as a pipe.
fn is_regular_file(stream: &str) -> bool {
    std::fs::metadata(stream).is_ok_and(|m| m.is_file())
}

/// Styling for one stream.
#[derive(Debug, Clone, Copy)]
pub struct Painter {
    pub enabled: bool,
}

/// Painter for results printed on stdout.
pub fn out() -> Painter {
    Painter {
        enabled: STDOUT.load(Ordering::Relaxed),
    }
}

/// Painter for progress and diagnostics printed on stderr.
pub fn err() -> Painter {
    Painter {
        enabled: STDERR.load(Ordering::Relaxed),
    }
}

impl Painter {
    pub fn paint(&self, code: &str, s: &str) -> String {
        if self.enabled && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
    pub fn bold_red(&self, s: &str) -> String {
        self.paint("1;31", s)
    }
    pub fn bold_green(&self, s: &str) -> String {
        self.paint("1;32", s)
    }
    pub fn bold_yellow(&self, s: &str) -> String {
        self.paint("1;33", s)
    }
    pub fn bold_cyan(&self, s: &str) -> String {
        self.paint("1;36", s)
    }

    /// Colors a log severity.
    pub fn severity(&self, s: &str) -> String {
        match s.trim() {
            "ERROR" | "CRITICAL" | "ALERT" | "EMERGENCY" => self.bold_red(s),
            "WARNING" => self.yellow(s),
            "NOTICE" => self.cyan(s),
            "DEBUG" | "DEFAULT" => self.dim(s),
            _ => s.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_painter_is_plain() {
        let p = Painter { enabled: false };
        assert_eq!(p.green("+ create"), "+ create");
        assert_eq!(p.severity("ERROR"), "ERROR");
    }

    #[test]
    fn enabled_painter_wraps_in_ansi() {
        let p = Painter { enabled: true };
        assert_eq!(p.red("- x"), "\x1b[31m- x\x1b[0m");
        assert_eq!(p.bold(""), "", "empty strings stay empty");
        assert!(p.severity("WARNING").starts_with("\x1b[33m"));
    }

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn auto_colors_terminals_only_outside_ci() {
        assert!(auto(env(&[]), true, false));
        assert!(!auto(env(&[]), false, false), "pipe");
        assert!(!auto(env(&[("CI", "true")]), false, false), "unknown CI");
        assert!(!auto(env(&[("TERM", "dumb")]), true, false));
    }

    #[test]
    fn auto_colors_ci_logs_but_not_redirected_files() {
        assert!(auto(env(&[("GITLAB_CI", "true")]), false, false));
        assert!(auto(env(&[("GITHUB_ACTIONS", "true")]), false, false));
        assert!(auto(env(&[("TF_BUILD", "True")]), false, false));
        assert!(!auto(env(&[("GITLAB_CI", "true")]), false, true), "file");
        assert!(!auto(env(&[("GITLAB_CI", "false")]), false, false));
        assert!(!auto(env(&[("GITLAB_CI", "")]), false, false));
    }

    #[test]
    fn no_color_wins_and_force_overrides_detection() {
        let ci = ("GITLAB_CI", "true");
        assert!(!auto(env(&[("NO_COLOR", "1")]), true, false));
        assert!(!auto(env(&[ci, ("NO_COLOR", "1")]), false, false));
        assert!(!auto(
            env(&[("NO_COLOR", "1"), ("FORCE_COLOR", "1")]),
            true,
            false
        ));
        assert!(
            auto(env(&[("NO_COLOR", "")]), true, false),
            "empty is unset"
        );
        assert!(auto(env(&[("FORCE_COLOR", "1")]), false, true));
        assert!(auto(
            env(&[("CLICOLOR_FORCE", "1"), ("TERM", "dumb")]),
            false,
            false
        ));
        assert!(!auto(env(&[("FORCE_COLOR", "0")]), false, false));
        assert!(!auto(env(&[("FORCE_COLOR", "false")]), false, false));
    }
}
