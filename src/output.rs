//! Output helpers. Results go to stdout (text or JSON); progress and
//! diagnostics go to stderr so that `--output json` stays machine-readable.

use crate::error::Error;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

/// Progress reporter writing to stderr.
#[derive(Debug, Clone)]
pub struct Progress {
    enabled: bool,
    color: bool,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            color: crate::style::err().enabled,
        }
    }

    /// A reporter that prints nothing (tests).
    pub fn silent() -> Self {
        Self {
            enabled: false,
            color: false,
        }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn step(&self, msg: impl AsRef<str>) {
        if self.enabled {
            eprintln!("{} {}", self.paint("1;36", "==>"), msg.as_ref());
        }
    }

    pub fn info(&self, msg: impl AsRef<str>) {
        if self.enabled {
            eprintln!("    {}", msg.as_ref());
        }
    }

    pub fn warn(&self, msg: impl AsRef<str>) {
        if self.enabled {
            eprintln!("{} {}", self.paint("1;33", "warning:"), msg.as_ref());
        }
    }

    pub fn success(&self, msg: impl AsRef<str>) {
        if self.enabled {
            eprintln!("{} {}", self.paint("1;32", "✓"), msg.as_ref());
        }
    }
}

pub fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: cannot serialize output: {e}"),
    }
}

#[derive(Serialize)]
struct JsonError<'a> {
    error: JsonErrorBody<'a>,
}

#[derive(Serialize)]
struct JsonErrorBody<'a> {
    kind: &'a str,
    message: String,
    hints: &'a [String],
    exit_code: i32,
}

pub fn print_error(err: &Error, format: OutputFormat) {
    match format {
        OutputFormat::Json => print_json(&JsonError {
            error: JsonErrorBody {
                kind: err.kind.as_str(),
                message: err.detailed_message(),
                hints: &err.hints,
                exit_code: err.exit_code(),
            },
        }),
        OutputFormat::Text => {
            let color = crate::style::err().enabled;
            let label = if color {
                "\x1b[1;31merror:\x1b[0m"
            } else {
                "error:"
            };
            eprintln!("{label} {}", err.detailed_message());
            for h in &err.hints {
                eprintln!("  {} {h}", crate::style::err().cyan("hint:"));
            }
        }
    }
}
