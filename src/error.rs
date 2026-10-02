//! Error type with stable process exit codes and actionable hints.

use std::fmt;

/// Broad error categories. Each maps to a documented process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Unexpected failure (bug, I/O error, unclassified API error).
    Internal,
    /// `runway.yaml` is missing, malformed or invalid.
    Config,
    /// Credentials missing, permission denied, API disabled or missing prerequisite resource.
    Prerequisite,
    /// Cloud Build failed, timed out or was cancelled.
    Build,
    /// Cloud Run rejected the service or the new revision never became ready.
    Deploy,
    /// Runway refused to modify a resource it does not own.
    Conflict,
    /// A bounded wait exceeded its deadline.
    Timeout,
    /// The requested service does not exist (info/logs).
    NotFound,
    /// The user interrupted the command (Ctrl-C).
    Interrupted,
}

impl ErrorKind {
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::Internal => 1,
            ErrorKind::Config => 3,
            ErrorKind::Prerequisite => 4,
            ErrorKind::Build => 5,
            ErrorKind::Deploy => 6,
            ErrorKind::Conflict => 7,
            ErrorKind::Timeout => 8,
            ErrorKind::NotFound => 9,
            ErrorKind::Interrupted => 130,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Internal => "internal",
            ErrorKind::Config => "config",
            ErrorKind::Prerequisite => "prerequisite",
            ErrorKind::Build => "build",
            ErrorKind::Deploy => "deploy",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Timeout => "timeout",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Interrupted => "interrupted",
        }
    }
}

/// The error type used throughout runway.
#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub hints: Vec<String>,
    pub source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    /// The command already reported this failure on stdout/stderr; only set the exit code.
    pub reported: bool,
    /// Retrying cannot help (for example an organization policy violation).
    pub permanent: bool,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hints: Vec::new(),
            source: None,
            reported: false,
            permanent: false,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }
    pub fn prerequisite(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Prerequisite, message)
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into());
        self
    }

    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }

    /// Marks the error as not retryable.
    pub fn permanent(mut self) -> Self {
        self.permanent = true;
        self
    }

    /// Marks the error as already reported to the user.
    pub fn reported(mut self) -> Self {
        self.reported = true;
        self
    }

    /// Full message including the chain of sources, suitable for display.
    pub fn detailed_message(&self) -> String {
        let mut out = self.message.clone();
        let mut src: Option<&(dyn std::error::Error + 'static)> = self
            .source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static));
        while let Some(e) = src {
            let s = e.to_string();
            if !s.is_empty() && !out.contains(&s) {
                out.push_str(": ");
                out.push_str(&s);
            }
            src = e.source();
        }
        out
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::internal(format!("I/O error: {e}")).with_source(e)
    }
}

/// Extension trait to attach context to results.
pub trait Context<T> {
    fn context(self, kind: ErrorKind, message: impl Into<String>) -> Result<T>;
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context(self, kind: ErrorKind, message: impl Into<String>) -> Result<T> {
        self.map_err(|e| {
            let msg = format!("{}: {e}", message.into());
            Error::new(kind, msg).with_source(e)
        })
    }
}
