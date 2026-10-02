//! Bounded polling with exponential backoff and Ctrl-C cancellation.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct PollConfig {
    pub initial: Duration,
    pub max: Duration,
    pub factor: f64,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            // Short enough that completion is noticed within ~5 s, while staying
            // far below API read quotas (at most ~2 reads/s at the start).
            initial: Duration::from_secs(1),
            max: Duration::from_secs(5),
            factor: 1.5,
        }
    }
}

impl PollConfig {
    /// Near-zero delays for tests.
    pub fn fast() -> Self {
        Self {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(2),
            factor: 1.0,
        }
    }
}

/// Tracks a deadline and the next backoff interval.
pub struct Poller {
    cfg: PollConfig,
    next: Duration,
    deadline: Instant,
}

pub enum Tick {
    Continue,
    TimedOut,
    Cancelled,
}

impl Poller {
    pub fn new(cfg: PollConfig, timeout: Duration) -> Self {
        Self {
            cfg,
            next: cfg.initial,
            deadline: Instant::now() + timeout,
        }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Sleeps for the next interval (never past the deadline), honouring Ctrl-C.
    pub async fn wait(&mut self) -> Tick {
        let now = Instant::now();
        if now >= self.deadline {
            return Tick::TimedOut;
        }
        let d = self.next.min(self.deadline - now);
        self.next = self
            .next
            .mul_f64(self.cfg.factor)
            .min(self.cfg.max)
            .max(self.cfg.initial);
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Tick::Cancelled,
            _ = tokio::time::sleep(d) => Tick::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poller_times_out() {
        let mut p = Poller::new(PollConfig::fast(), Duration::from_millis(10));
        let mut ticks = 0;
        loop {
            match p.wait().await {
                Tick::Continue => ticks += 1,
                Tick::TimedOut => break,
                Tick::Cancelled => unreachable!(),
            }
            assert!(ticks < 10_000);
        }
        assert!(p.expired());
    }
}
