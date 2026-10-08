//! A lease on the stage's holder workload (the one that records grants: the
//! main service, else the first service, else the first job), so that runs
//! that would interfere wait for each other. No lock file and no other
//! resource: the lease is the annotation [`ANNOTATION_LEASE`], written with
//! the resource's etag, so two runs can never both take it. A write counts
//! only once its operation completed and the resource carries it.
//!
//! Deploys, canaries, traffic changes and teardowns take it exclusively;
//! previews share it (they only add their own tag). A lease expires
//! [`TTL`] after its last renewal. A run that loses its lease (expired
//! without renewal, taken over, removed by `runway unlock`) stops before its
//! next change: see [`Held::check`].

use crate::error::{Error, ErrorKind, Result};
use crate::gcp::{api_error, is_ambiguous, is_concurrency_conflict, is_not_found};
use crate::output::Progress;
use chrono::{DateTime, Utc};
use google_cloud_lro::Poller;
use google_cloud_run_v2::client::{Jobs, Services};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ANNOTATION_LEASE: &str = "runway.dev/lease";
/// How long a lease lasts without renewal.
pub const TTL: Duration = Duration::from_secs(120);
/// How often a held lease is renewed.
pub const RENEW_EVERY: Duration = Duration::from_secs(30);
/// A run stops changing things this long before its lease would expire
/// without a renewal (clock differences, a renewal in flight).
const MARGIN: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Several runs at once (previews).
    Shared,
    /// One run, alone (deploys, canaries, traffic changes, teardowns).
    Exclusive,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Shared => "shared",
            Mode::Exclusive => "exclusive",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub id: String,
    pub mode: Mode,
    /// Who: CI job, user and host, command.
    pub by: String,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

impl Holder {
    fn live(&self, now: DateTime<Utc>) -> bool {
        self.until > now
    }

    /// `exclusive lease held by gitlab job 4711 … for 3m12s (expires in 1m40s …)`.
    pub fn describe(&self, now: DateTime<Utc>) -> String {
        let ago = (now - self.since).to_std().unwrap_or_default();
        let left = (self.until - now).to_std().unwrap_or_default();
        format!(
            "{} lease held by {} for {} (expires in {} unless renewed)",
            self.mode,
            self.by,
            short_duration(ago),
            short_duration(left)
        )
    }
}

/// The value of [`ANNOTATION_LEASE`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    #[serde(default)]
    pub holders: Vec<Holder>,
}

impl Lease {
    pub fn decode(v: Option<&str>) -> Lease {
        v.and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default()
    }

    pub fn encode(&self) -> Option<String> {
        (!self.holders.is_empty()).then(|| serde_json::to_string(self).expect("serializes"))
    }

    /// Holders whose lease has not expired.
    pub fn live(&self, now: DateTime<Utc>) -> Vec<&Holder> {
        self.holders.iter().filter(|h| h.live(now)).collect()
    }

    /// Whether `mode` can be taken now by `id` (its own entry aside).
    pub fn admits(&self, id: &str, mode: Mode, now: DateTime<Utc>) -> bool {
        self.blockers(id, mode, now).is_empty()
    }

    /// The live holders that keep `mode` out.
    pub fn blockers(&self, id: &str, mode: Mode, now: DateTime<Utc>) -> Vec<&Holder> {
        self.live(now)
            .into_iter()
            .filter(|h| h.id != id && (mode == Mode::Exclusive || h.mode == Mode::Exclusive))
            .collect()
    }

    /// This lease with `h` added (or renewed) and expired holders dropped.
    pub fn with(&self, h: Holder, now: DateTime<Utc>) -> Lease {
        let mut holders: Vec<Holder> = self
            .holders
            .iter()
            .filter(|x| x.live(now) && x.id != h.id)
            .cloned()
            .collect();
        holders.push(h);
        Lease { holders }
    }

    /// This lease without `id` (and without expired holders).
    pub fn without(&self, id: &str, now: DateTime<Utc>) -> Lease {
        Lease {
            holders: self
                .holders
                .iter()
                .filter(|x| x.live(now) && x.id != id)
                .cloned()
                .collect(),
        }
    }

    /// `id`'s holder, if it still holds the lease.
    pub fn holder(&self, id: &str, now: DateTime<Utc>) -> Option<&Holder> {
        self.live(now).into_iter().find(|h| h.id == id)
    }
}

/// `3m12s`, `45s`, `1h02m`.
pub fn short_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Who is running: the CI job when there is one, else user@host.
pub fn whoami(command: &str) -> String {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let who = if let Some(job) = env("CI_JOB_ID") {
        let mut s = format!("gitlab job {job}");
        if let (Some(r), Some(sha)) = (env("CI_COMMIT_REF_NAME"), env("CI_COMMIT_SHORT_SHA")) {
            s.push_str(&format!(" ({r} @ {sha})"));
        }
        if let Some(url) = env("CI_JOB_URL") {
            s.push_str(&format!(" {url}"));
        }
        s
    } else if let Some(run) = env("GITHUB_RUN_ID") {
        let mut s = format!("github run {run}");
        if let (Some(r), Some(sha)) = (env("GITHUB_REF_NAME"), env("GITHUB_SHA")) {
            s.push_str(&format!(" ({r} @ {})", &sha[..sha.len().min(8)]));
        }
        if let (Some(server), Some(repo)) = (env("GITHUB_SERVER_URL"), env("GITHUB_REPOSITORY")) {
            s.push_str(&format!(" {server}/{repo}/actions/runs/{run}"));
        }
        s
    } else {
        let user = env("USER")
            .or_else(|| env("USERNAME"))
            .unwrap_or_else(|| "someone".into());
        let host = env("HOSTNAME")
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|h| h.trim().to_string())
                    .filter(|h| !h.is_empty())
            })
            .unwrap_or_else(|| "unknown host".into());
        format!("{user}@{host}")
    };
    format!("{who}: {command}")
}

fn new_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!(
        "{}-{}-{:?}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        std::thread::current().id()
    );
    Sha256::digest(seed.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Not there, or Cloud Run not enabled yet (a first deploy): no holder.
fn absent(e: &crate::gcp::GaxError) -> bool {
    is_not_found(e) || crate::gcp::is_service_disabled(e)
}

/// The workload holding the lease.
#[derive(Clone)]
pub enum Target {
    Service { run: Services, name: String },
    Job { jobs: Jobs, name: String },
}

/// What a lease change sees of the holder.
pub struct Seen<'a> {
    pub lease: &'a Lease,
    pub labels: &'a HashMap<String, String>,
}

impl Target {
    pub fn name(&self) -> &str {
        match self {
            Target::Service { name, .. } | Target::Job { name, .. } => name,
        }
    }

    fn short(&self) -> &str {
        self.name().rsplit('/').next().unwrap_or(self.name())
    }

    /// Reads the lease, lets `change` decide, and writes its answer with the
    /// etag of what was read. The write counts once its operation completed
    /// and the resource carries the new lease; a concurrent write (or a lost
    /// answer) makes it start over. `Ok(None)`: the holder does not exist.
    pub async fn update<T>(
        &self,
        mut change: impl FnMut(Seen<'_>) -> Result<(Option<Lease>, T)>,
    ) -> Result<Option<T>> {
        for _ in 0..8 {
            // Read, decide, and prepare the write.
            let (wanted, out, write) = match self {
                Target::Service { run, name } => {
                    let svc = match run.get_service().set_name(name).send().await {
                        Ok(s) => s,
                        Err(e) if absent(&e) => return Ok(None),
                        Err(e) => return Err(api_error(e, &format!("reading {}", self.short()))),
                    };
                    let current =
                        Lease::decode(svc.annotations.get(ANNOTATION_LEASE).map(String::as_str));
                    let (next, out) = change(Seen {
                        lease: &current,
                        labels: &svc.labels,
                    })?;
                    let Some(next) = next.filter(|n| *n != current) else {
                        return Ok(Some(out));
                    };
                    let wanted = next.encode();
                    let mut svc = svc;
                    match &wanted {
                        Some(v) => svc.annotations.insert(ANNOTATION_LEASE.into(), v.clone()),
                        None => svc.annotations.remove(ANNOTATION_LEASE),
                    };
                    let res = run
                        .update_service()
                        .set_service(svc)
                        .set_update_mask(
                            google_cloud_wkt::FieldMask::default().set_paths(["annotations"]),
                        )
                        .poller()
                        .until_done()
                        .await
                        .map(|s| s.annotations);
                    (wanted, out, res)
                }
                Target::Job { jobs, name } => {
                    let job = match jobs.get_job().set_name(name).send().await {
                        Ok(j) => j,
                        Err(e) if absent(&e) => return Ok(None),
                        Err(e) => return Err(api_error(e, &format!("reading {}", self.short()))),
                    };
                    let current =
                        Lease::decode(job.annotations.get(ANNOTATION_LEASE).map(String::as_str));
                    let (next, out) = change(Seen {
                        lease: &current,
                        labels: &job.labels,
                    })?;
                    let Some(next) = next.filter(|n| *n != current) else {
                        return Ok(Some(out));
                    };
                    let wanted = next.encode();
                    let mut job = job;
                    match &wanted {
                        Some(v) => job.annotations.insert(ANNOTATION_LEASE.into(), v.clone()),
                        None => job.annotations.remove(ANNOTATION_LEASE),
                    };
                    let res = jobs
                        .update_job()
                        .set_job(job)
                        .poller()
                        .until_done()
                        .await
                        .map(|j| j.annotations);
                    (wanted, out, res)
                }
            };
            match write {
                // Done, and the resource says what was written.
                Ok(annotations) if annotations.get(ANNOTATION_LEASE) == wanted.as_ref() => {
                    return Ok(Some(out));
                }
                // Done, but something else is there: read again.
                Ok(_) => continue,
                // Someone wrote in between, or the answer was lost: the next
                // read says where things stand.
                Err(e) if is_concurrency_conflict(&e) || is_ambiguous(&e) => continue,
                Err(e) => {
                    return Err(
                        api_error(e, &format!("updating the lease on {}", self.short()))
                            .hint("nothing was changed by this run; re-run it"),
                    );
                }
            }
        }
        Err(Error::new(
            ErrorKind::Conflict,
            format!("the lease on {} keeps changing; retry", self.short()),
        ))
    }

    /// The live holders, for `plan` and `unlock`.
    pub async fn holders(&self) -> Result<Vec<Holder>> {
        let now = Utc::now();
        Ok(self
            .update(|seen| {
                Ok((
                    None,
                    seen.lease
                        .live(now)
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                ))
            })
            .await?
            .unwrap_or_default())
    }

    /// Removes the whole lease (`runway unlock`) from a holder runway owns
    /// for `app`/`stage`; returns who held it.
    pub async fn clear(&self, app: &str, stage: &str) -> Result<Vec<Holder>> {
        let now = Utc::now();
        let short = self.short().to_string();
        Ok(self
            .update(|seen| {
                if crate::gcp::run::ownership_of(seen.labels, app, stage)
                    != crate::gcp::run::Ownership::Owned
                {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("{short} is not managed by runway for app {app} and stage {stage}: its annotations are not runway's"),
                    )
                    .permanent());
                }
                let had: Vec<Holder> = seen.lease.live(now).into_iter().cloned().collect();
                Ok((Some(Lease::default()), had))
            })
            .await?
            .unwrap_or_default())
    }
}

/// How long to wait for the lease.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    /// `None`: fail at once when the lease is held.
    pub timeout: Option<Duration>,
    /// Pause between attempts (doubles, up to 10 seconds).
    pub poll: Duration,
    /// How often the held lease is renewed ([`RENEW_EVERY`]).
    pub renew: Duration,
}

impl Default for Wait {
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(30 * 60)),
            poll: Duration::from_secs(2),
            renew: RENEW_EVERY,
        }
    }
}

/// What a run knows about its lease, shared with its renewal and with what
/// it changes (see [`Held::check`]).
#[derive(Debug)]
struct State {
    target: String,
    /// Not locked (no holder runway owns): nothing to lose.
    active: bool,
    /// The holder did not exist: the lease goes into its creation.
    pending: AtomicBool,
    lost: AtomicBool,
    /// Expiry of the last lease this run wrote (or put into a creation).
    until: Mutex<DateTime<Utc>>,
}

/// A handle to check, before a change, that this run still holds its lease.
#[derive(Debug, Clone)]
pub struct Held(Arc<State>);

impl Held {
    /// No lease (for code paths and tests that do not lock).
    pub fn none() -> Held {
        Held(Arc::new(State {
            target: String::new(),
            active: false,
            pending: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            until: Mutex::new(Utc::now()),
        }))
    }

    /// Fails when the lease was lost (taken over, removed, or about to
    /// expire without a renewal): the run must change nothing more.
    pub fn check(&self) -> Result<()> {
        let s = &self.0;
        if !s.active {
            return Ok(());
        }
        let lost = |why: &str| {
            Error::new(
                ErrorKind::Conflict,
                format!(
                    "this run lost its lease on {} ({why}); it stops before changing anything else",
                    s.target
                ),
            )
            .permanent()
            .hint("another run may hold the stage now: check with `runway unlock --stage <stage>`, then re-run")
        };
        if s.lost.load(Ordering::SeqCst) {
            return Err(lost("taken over or removed"));
        }
        // A pending lease is not on any resource yet: nothing can take it.
        if s.pending.load(Ordering::SeqCst) {
            return Ok(());
        }
        let until = *s.until.lock().expect("not poisoned");
        let margin = chrono::Duration::from_std(MARGIN).expect("small");
        if Utc::now() + margin >= until {
            return Err(lost("it could not be renewed in time"));
        }
        Ok(())
    }
}

/// A held lease. [`Guard::release`] gives it back; a guard that is dropped
/// stops renewing, and its lease expires after [`TTL`].
pub struct Guard {
    target: Option<Target>,
    id: String,
    mode: Mode,
    by: String,
    since: DateTime<Utc>,
    state: Arc<State>,
    renew: Option<tokio::task::JoinHandle<()>>,
    renew_every: Duration,
}

impl Guard {
    /// No lease (a holder runway does not own, or nothing to lock).
    pub fn none() -> Guard {
        Guard {
            target: None,
            id: new_id(),
            mode: Mode::Shared,
            by: String::new(),
            since: Utc::now(),
            state: Held::none().0,
            renew: None,
            renew_every: RENEW_EVERY,
        }
    }

    /// The handle to check before each change.
    pub fn held(&self) -> Held {
        Held(self.state.clone())
    }

    /// The annotation the holder's rollout carries: this run's lease, valid
    /// from now. It creates the holder with it (a first deploy), and every
    /// update of the holder refuses to overwrite a lease another run holds
    /// (see [`created_by_other`]). `None` without a lease.
    pub fn annotation(&self) -> Option<(String, String)> {
        self.target.as_ref()?;
        let now = Utc::now();
        let lease = Lease::default().with(self.holder(now), now);
        if self.state.pending.load(Ordering::SeqCst) {
            *self.state.until.lock().expect("not poisoned") = now + ttl();
        }
        Some((ANNOTATION_LEASE.to_string(), lease.encode()?))
    }

    fn holder(&self, now: DateTime<Utc>) -> Holder {
        Holder {
            id: self.id.clone(),
            mode: self.mode,
            by: self.by.clone(),
            since: self.since,
            until: now + ttl(),
        }
    }

    /// When the holder did not exist at acquisition (a first deploy): takes
    /// the lease on it if another run created it since, waiting as
    /// [`acquire`] does. Call it after reading the holder, before changing
    /// anything. `true`: the holder appeared and this run now holds its
    /// lease: what was read before is stale, read it again.
    pub async fn confirm(
        &mut self,
        app: &str,
        stage: &str,
        wait: Wait,
        p: &Progress,
    ) -> Result<bool> {
        if !self.state.pending.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let Some(target) = self.target.clone() else {
            return Ok(false);
        };
        self.take(&target, app, stage, wait, p).await?;
        Ok(self.target.is_some() && !self.state.pending.load(Ordering::SeqCst))
    }

    /// The acquisition loop, on `target`.
    async fn take(
        &mut self,
        target: &Target,
        app: &str,
        stage: &str,
        wait: Wait,
        p: &Progress,
    ) -> Result<()> {
        let mode = self.mode;
        let started = std::time::Instant::now();
        let mut poll = wait.poll;
        let mut last_told: Option<(std::time::Instant, String)> = None;
        loop {
            let now = Utc::now();
            let me = self.holder(now);
            let id = self.id.clone();
            let outcome = target
                .update(|seen| {
                    if crate::gcp::run::ownership_of(seen.labels, app, stage)
                        != crate::gcp::run::Ownership::Owned
                    {
                        return Ok((None, Err(None)));
                    }
                    match seen.lease.admits(&id, mode, now) {
                        true => Ok((Some(seen.lease.with(me.clone(), now)), Ok(()))),
                        false => Ok((
                            None,
                            Err(Some(
                                seen.lease
                                    .blockers(&id, mode, now)
                                    .iter()
                                    .map(|h| h.describe(now))
                                    .collect::<Vec<_>>()
                                    .join("; "),
                            )),
                        )),
                    }
                })
                .await?;
            match outcome {
                None => {
                    if !self.state.pending.swap(true, Ordering::SeqCst) {
                        p.info(format!(
                            "{} does not exist yet: it is created with this run's lease ({mode})",
                            target.short()
                        ));
                    }
                    self.activate(target);
                    return Ok(());
                }
                Some(Err(None)) => {
                    p.warn(format!(
                        "{} is not managed by runway for this app and stage: no lease taken",
                        target.short()
                    ));
                    *self = Guard::none();
                    return Ok(());
                }
                Some(Ok(())) => {
                    *self.state.until.lock().expect("not poisoned") = me.until;
                    self.state.pending.store(false, Ordering::SeqCst);
                    self.state.lost.store(false, Ordering::SeqCst);
                    if started.elapsed() > Duration::from_secs(1) {
                        p.success(format!(
                            "took the {mode} lease on {} after waiting {}",
                            target.short(),
                            short_duration(started.elapsed())
                        ));
                    } else {
                        p.info(format!("took the {mode} lease on {}", target.short()));
                    }
                    self.activate(target);
                    return Ok(());
                }
                Some(Err(Some(blockers))) => {
                    let waited = started.elapsed();
                    let hint = format!(
                        "if that run is gone, its lease expires within {}; `runway unlock --stage {stage} --yes` removes it now",
                        short_duration(TTL)
                    );
                    let Some(timeout) = wait.timeout else {
                        return Err(Error::new(
                            ErrorKind::Conflict,
                            format!("stage {stage} is busy: {blockers}"),
                        )
                        .permanent()
                        .hint("re-run without `--no-wait` to wait for it")
                        .hint(hint));
                    };
                    if waited >= timeout {
                        return Err(Error::new(
                            ErrorKind::Conflict,
                            format!(
                                "stage {stage} was still busy after waiting {}: {blockers}",
                                short_duration(waited)
                            ),
                        )
                        .permanent()
                        .hint("raise `--wait-timeout`")
                        .hint(hint));
                    }
                    let tell = match &last_told {
                        None => true,
                        Some((at, what)) => {
                            *what != blockers || at.elapsed() >= Duration::from_secs(30)
                        }
                    };
                    if tell {
                        p.info(format!(
                            "waiting for the {mode} lease on {} ({} so far): {blockers}",
                            target.short(),
                            short_duration(waited)
                        ));
                        last_told = Some((std::time::Instant::now(), blockers));
                    }
                    tokio::time::sleep(poll.min(timeout.saturating_sub(waited))).await;
                    poll = (poll * 2).min(Duration::from_secs(10));
                }
            }
        }
    }

    /// Starts renewing (once) on `target`.
    fn activate(&mut self, target: &Target) {
        if self.target.is_none() {
            self.target = Some(target.clone());
            let pending = self.state.pending.load(Ordering::SeqCst);
            let until = *self.state.until.lock().expect("not poisoned");
            self.state = Arc::new(State {
                target: target.short().to_string(),
                active: true,
                pending: AtomicBool::new(pending),
                lost: AtomicBool::new(false),
                until: Mutex::new(until),
            });
        }
        // A renewal that stopped (it saw another run's holder while this one
        // was pending) starts again once the lease is taken.
        if self.renew.as_ref().is_none_or(|h| h.is_finished()) {
            self.renew = Some(spawn_renewal(target.clone(), self));
        }
    }

    /// Gives the lease back (best effort: otherwise it expires).
    pub async fn release(mut self, p: &Progress) {
        if let Some(h) = self.renew.take() {
            h.abort();
        }
        let Some(target) = self.target.take() else {
            return;
        };
        if self.state.lost.load(Ordering::SeqCst) {
            p.warn(format!(
                "the lease on {} was lost during this run (expired, taken over or removed by `runway unlock`)",
                target.short()
            ));
            return;
        }
        let id = self.id.clone();
        let now = Utc::now();
        if let Err(e) = target
            .update(|seen| Ok((Some(seen.lease.without(&id, now)), ())))
            .await
        {
            p.warn(format!(
                "could not release the lease on {} ({}); it expires within {}",
                target.short(),
                e.message,
                short_duration(TTL)
            ));
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(h) = self.renew.take() {
            h.abort();
        }
    }
}

fn ttl() -> chrono::Duration {
    chrono::Duration::from_std(TTL).expect("small")
}

/// Takes the lease on `target` in `mode`, waiting while runs that exclude
/// it hold it. A holder that does not exist yet (first deploy) cannot be
/// locked: the guard is pending, its lease goes into the holder's creation
/// ([`Guard::annotation`]) and [`Guard::confirm`] takes it if another run
/// creates the holder first. A holder runway does not own for `app`/`stage`
/// is not locked (the run refuses to change it anyway).
pub async fn acquire(
    target: Target,
    mode: Mode,
    app: &str,
    stage: &str,
    command: &str,
    wait: Wait,
    p: &Progress,
) -> Result<Guard> {
    let mut guard = Guard {
        target: None,
        id: new_id(),
        mode,
        by: whoami(command),
        since: Utc::now(),
        state: Held::none().0,
        renew: None,
        renew_every: wait.renew,
    };
    guard.take(&target, app, stage, wait, p).await?;
    Ok(guard)
}

/// Renews the lease every [`RENEW_EVERY`] until the guard is released. Its
/// entry gone (expired and taken over, or removed by `runway unlock`): the
/// lease is lost, never taken again.
fn spawn_renewal(target: Target, guard: &Guard) -> tokio::task::JoinHandle<()> {
    let (id, mode, by, since, state, every) = (
        guard.id.clone(),
        guard.mode,
        guard.by.clone(),
        guard.since,
        guard.state.clone(),
        guard.renew_every,
    );
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let now = Utc::now();
            let me = Holder {
                id: id.clone(),
                mode,
                by: by.clone(),
                since,
                until: now + ttl(),
            };
            let res = target
                .update(|seen| match seen.lease.holder(&id, now) {
                    Some(_) => Ok((Some(seen.lease.with(me.clone(), now)), true)),
                    None => Ok((None, false)),
                })
                .await;
            match res {
                // Renewed.
                Ok(Some(true)) => {
                    *state.until.lock().expect("not poisoned") = me.until;
                    state.pending.store(false, Ordering::SeqCst);
                }
                // The holder exists without this run's entry: lost (unless
                // still waiting for this run to create it).
                // The holder exists without this run's entry: lost. (A
                // holder this run creates carries its lease, so a pending run
                // seeing it without its entry means another run created it.)
                Ok(Some(false)) => {
                    state.lost.store(true, Ordering::SeqCst);
                    return;
                }
                // Not created yet.
                Ok(None) => {}
                // Not renewed this time: `Held::check` stops the run once
                // the lease could have expired.
                Err(_) => {}
            }
        }
    })
}

/// The error for an update of the holder that would overwrite a lease held
/// by another run: `ours` is this run's lease annotation (the rollout
/// carries it), `live` the holder's. `None` when this run may write.
pub fn created_by_other(ours: Option<&str>, live: Option<&str>, name: &str) -> Option<Error> {
    let ours = Lease::decode(ours);
    let mine = ours.holders.first()?;
    let now = Utc::now();
    let theirs = Lease::decode(live);
    let other = theirs
        .blockers(&mine.id, mine.mode, now)
        .into_iter()
        .next()?;
    Some(
        Error::new(
            ErrorKind::Conflict,
            format!(
                "another runway run holds {name}: {}; this run stops before changing it",
                other.describe(now)
            ),
        )
        .permanent()
        .hint("re-run: it waits for that run to finish"),
    )
}

/// Best effort, for `plan`: who holds the lease on `annotations`.
pub fn status(annotations: &HashMap<String, String>) -> Vec<String> {
    let now = Utc::now();
    Lease::decode(annotations.get(ANNOTATION_LEASE).map(String::as_str))
        .live(now)
        .iter()
        .map(|h| h.describe(now))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: &str, mode: Mode, until_s: i64) -> Holder {
        let now = Utc::now();
        Holder {
            id: id.into(),
            mode,
            by: format!("run {id}"),
            since: now - chrono::Duration::seconds(30),
            until: now + chrono::Duration::seconds(until_s),
        }
    }

    #[test]
    fn exclusive_excludes_everyone_and_shared_shares() {
        let now = Utc::now();
        let free = Lease::default();
        assert!(free.admits("a", Mode::Exclusive, now));
        let shared = Lease {
            holders: vec![h("p1", Mode::Shared, 60)],
        };
        assert!(
            shared.admits("p2", Mode::Shared, now),
            "previews run together"
        );
        assert!(
            !shared.admits("d", Mode::Exclusive, now),
            "a deploy waits for previews"
        );
        let excl = Lease {
            holders: vec![h("d", Mode::Exclusive, 60)],
        };
        assert!(!excl.admits("p", Mode::Shared, now));
        assert!(!excl.admits("d2", Mode::Exclusive, now));
        assert!(
            excl.admits("d", Mode::Exclusive, now),
            "its own entry is not in the way"
        );
        assert_eq!(excl.blockers("p", Mode::Shared, now).len(), 1);
    }

    #[test]
    fn expired_holders_do_not_count_and_are_dropped() {
        let now = Utc::now();
        let stale = Lease {
            holders: vec![h("old", Mode::Exclusive, -1)],
        };
        assert!(stale.admits("new", Mode::Exclusive, now));
        let next = stale.with(h("new", Mode::Exclusive, 120), now);
        assert_eq!(next.holders.len(), 1);
        assert_eq!(next.holders[0].id, "new");
        assert_eq!(
            next.without("new", now).encode(),
            None,
            "an empty lease is no annotation"
        );
    }

    #[test]
    fn the_annotation_round_trips_and_garbage_is_no_lease() {
        let l = Lease {
            holders: vec![h("a", Mode::Shared, 60)],
        };
        assert_eq!(Lease::decode(l.encode().as_deref()), l);
        assert_eq!(Lease::decode(Some("not json")), Lease::default());
    }

    #[test]
    fn a_write_over_another_run_s_lease_is_refused() {
        let now = Utc::now();
        let mine = Lease::default()
            .with(h("me", Mode::Exclusive, 60), now)
            .encode();
        let theirs = Lease::default()
            .with(h("them", Mode::Exclusive, 60), now)
            .encode();
        assert!(created_by_other(mine.as_deref(), theirs.as_deref(), "shop-prod").is_some());
        assert!(created_by_other(mine.as_deref(), mine.as_deref(), "shop-prod").is_none());
        let expired = Lease::default()
            .with(h("them", Mode::Exclusive, -5), now)
            .encode();
        assert!(created_by_other(mine.as_deref(), expired.as_deref(), "shop-prod").is_none());
        assert!(
            created_by_other(None, theirs.as_deref(), "shop-prod").is_none(),
            "no lease of ours"
        );
        let preview = Lease::default()
            .with(h("p", Mode::Shared, 60), now)
            .encode();
        let other_preview = Lease::default()
            .with(h("q", Mode::Shared, 60), now)
            .encode();
        assert!(
            created_by_other(preview.as_deref(), other_preview.as_deref(), "shop-prod").is_none(),
            "previews share"
        );
    }

    #[test]
    fn a_lost_or_unrenewed_lease_stops_the_run() {
        let held = Held(Arc::new(State {
            target: "shop-prod".into(),
            active: true,
            pending: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            until: Mutex::new(Utc::now() + chrono::Duration::seconds(100)),
        }));
        assert!(held.check().is_ok());
        *held.0.until.lock().unwrap() = Utc::now() + chrono::Duration::seconds(5);
        let e = held.check().unwrap_err();
        assert!(
            e.message.contains("could not be renewed in time"),
            "{}",
            e.message
        );
        held.0.pending.store(true, Ordering::SeqCst);
        assert!(held.check().is_ok(), "a pending lease is on nothing yet");
        held.0.lost.store(true, Ordering::SeqCst);
        let e = held.check().unwrap_err();
        assert!(e.message.contains("taken over or removed"), "{}", e.message);
        assert!(e.permanent);
        assert!(Held::none().check().is_ok());
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(short_duration(Duration::from_secs(45)), "45s");
        assert_eq!(short_duration(Duration::from_secs(192)), "3m12s");
        assert_eq!(short_duration(Duration::from_secs(3720)), "1h02m");
    }
}
