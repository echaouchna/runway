//! Terminal colors, used sparingly: status markers, diff lines, section
//! headers and key values. Disabled for non-terminals, `NO_COLOR`,
//! `TERM=dumb` and JSON output; `--color always|never` overrides detection.

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

/// Decides once, at startup, whether stdout and stderr get colors.
pub fn init(mode: ColorMode, json_output: bool) {
    let env_ok = std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
    let (out, err) = match mode {
        ColorMode::Always => (true, true),
        ColorMode::Never => (false, false),
        ColorMode::Auto => (
            env_ok && std::io::stdout().is_terminal(),
            env_ok && std::io::stderr().is_terminal(),
        ),
    };
    STDOUT.store(out && !json_output, Ordering::Relaxed);
    STDERR.store(err, Ordering::Relaxed);
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
}
