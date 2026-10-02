//! Configurable per-step retries.
//!
//! Every deployment step (create the service account, grant a role, build,
//! roll out, bind tags, set access, configure IAP) is retried on failure with
//! exponential backoff. Steps are idempotent: each attempt re-reads the
//! current state and only changes what is still missing, so a retry never
//! duplicates work.
//!
//! Errors that cannot be fixed by waiting are not retried: invalid
//! configuration, ownership conflicts, interruptions and failed Docker builds.
//! Everything else is: transient API errors, timeouts, unhealthy revisions and
//! permission or "does not exist" errors, which are commonly caused by IAM
//! propagation right after a service account is created or a role is granted.

use crate::error::{Error, ErrorKind, Result};
use crate::output::Progress;
use serde::Serialize;
use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RetryConfig {
    /// Total attempts per step (1 = no retry).
    pub attempts: u32,
    #[serde(with = "humantime_serde_compat")]
    pub delay: Duration,
    #[serde(with = "humantime_serde_compat")]
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: 3,
            delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(60),
        }
    }
}

mod humantime_serde_compat {
    pub fn serialize<S: serde::Serializer>(
        d: &std::time::Duration,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(&humantime::format_duration(*d).to_string())
    }
}

impl RetryConfig {
    /// Delay before attempt `n + 1` (n starts at 1): delay, 2x, 4x, … capped.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1)).min(1 << 16);
        self.delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// Whether an error is worth retrying.
pub fn is_retryable(e: &Error) -> bool {
    !e.permanent
        && !matches!(
            e.kind,
            ErrorKind::Config | ErrorKind::Conflict | ErrorKind::Interrupted | ErrorKind::Build
        )
}

/// Runs `op` until it succeeds, fails with a non-retryable error, or the
/// attempts are exhausted. Ctrl-C during a backoff aborts immediately.
pub async fn with_retry<T, F, Fut>(
    cfg: &RetryConfig,
    progress: &Progress,
    step: &str,
    mut op: F,
) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 1;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < cfg.attempts && is_retryable(&e) => {
                let wait = cfg.backoff(attempt);
                progress.warn(format!(
                    "{step}: attempt {attempt}/{} failed: {}; retrying in {}",
                    cfg.attempts,
                    e.message,
                    humantime::format_duration(wait)
                ));
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        return Err(Error::new(ErrorKind::Interrupted, format!("interrupted while retrying {step}")));
                    }
                    _ = tokio::time::sleep(wait) => {}
                }
                attempt += 1;
            }
            Err(mut e) => {
                if attempt > 1 {
                    e.message = format!("{step} failed after {attempt} attempt(s): {}", e.message);
                } else if !e.message.starts_with(step) {
                    e.message = format!("{step}: {}", e.message);
                }
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn fast(attempts: u32) -> RetryConfig {
        RetryConfig {
            attempts,
            delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let c = RetryConfig {
            attempts: 10,
            delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(30),
        };
        assert_eq!(c.backoff(1), Duration::from_secs(5));
        assert_eq!(c.backoff(2), Duration::from_secs(10));
        assert_eq!(c.backoff(3), Duration::from_secs(20));
        assert_eq!(c.backoff(4), Duration::from_secs(30));
        assert_eq!(c.backoff(40), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn retries_transient_failures_until_success() {
        let calls = AtomicU32::new(0);
        let out = with_retry(&fast(3), &Progress::silent(), "grant role", |_| async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(Error::prerequisite("Service account x does not exist"))
            } else {
                Ok(42)
            }
        })
        .await
        .unwrap();
        assert_eq!(out, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_configured_attempts() {
        let calls = AtomicU32::new(0);
        let err = with_retry(&fast(4), &Progress::silent(), "roll out", |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(Error::new(ErrorKind::Deploy, "revision not ready"))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(
            err.message.contains("roll out failed after 4 attempt(s)"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn does_not_retry_permanent_errors() {
        for kind in [ErrorKind::Config, ErrorKind::Conflict, ErrorKind::Build] {
            let calls = AtomicU32::new(0);
            let err = with_retry(&fast(5), &Progress::silent(), "step", |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Error::new(kind, "permanent"))
            })
            .await
            .unwrap_err();
            assert_eq!(calls.load(Ordering::SeqCst), 1, "{kind:?}");
            assert_eq!(err.kind, kind);
        }
    }

    #[tokio::test]
    async fn one_attempt_disables_retries() {
        let calls = AtomicU32::new(0);
        let _ = with_retry(&fast(1), &Progress::silent(), "s", |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(Error::internal("boom"))
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
