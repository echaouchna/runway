//! Deployment reconciliation against the live Cloud Run service.
//!
//! The live service is the source of truth. Every mutation is preceded by a
//! read (ownership + etag) and every ambiguous outcome (timeout, transport
//! error, 5xx) is resolved by re-reading the service before retrying.

use crate::error::{Error, ErrorKind, Result};
use crate::gcp::run::{self, Ownership, Readiness};
use crate::gcp::{api_error, iam, is_ambiguous, is_concurrency_conflict, is_not_found};
use crate::output::Progress;
use crate::plan::{ServiceSpec, diff};
use crate::poll::{PollConfig, Poller, Tick};
use google_cloud_gax::error::rpc::Code;
use google_cloud_iam_v1::model::GetPolicyOptions;
use google_cloud_longrunning::model::operation;
use google_cloud_run_v2::client::{Revisions, Services};
use google_cloud_run_v2::model::Service;
use serde::Serialize;
use std::time::Duration;

const MAX_MUTATION_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceChange {
    Created,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessChange {
    MadePublic,
    MadePrivate,
    Unchanged,
}

/// Result of applying the desired spec (before readiness).
#[derive(Debug, Clone)]
pub struct Applied {
    pub change: ServiceChange,
    pub operation: Option<String>,
    /// Minimum generation that must be observed.
    pub target_generation: i64,
}

/// Identifies the service being reconciled.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub parent: &'a str,
    pub service_id: &'a str,
    pub name: &'a str,
    pub app: &'a str,
    pub stage: &'a str,
    /// Allow taking over an unlabeled service.
    pub adopt: bool,
    /// Apply even when no managed field differs (creates a new revision).
    pub force: bool,
}

pub struct Reconciler<'a> {
    pub run: &'a Services,
    pub revisions: Option<&'a Revisions>,
    pub progress: &'a Progress,
    pub poll: PollConfig,
    pub timeout: Duration,
}

/// Fails unless the service is owned by this app/stage (or `adopt` allows taking over an unlabeled one).
pub fn check_ownership(svc: &Service, app: &str, stage: &str, adopt: bool) -> Result<()> {
    match run::ownership(svc, app, stage) {
        Ownership::Owned => Ok(()),
        Ownership::Unmanaged if adopt => Ok(()),
        Ownership::Unmanaged => Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "Cloud Run service {} already exists and is not managed by runway",
                run::short_revision(&svc.name)
            ),
        )
        .hint("choose a different `app` or stage name, delete the existing service, or re-run with `--adopt` to take it over (its revision template will be replaced)")),
        Ownership::OtherOwner { app: a, stage: s } => Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "Cloud Run service {} is managed by runway for app `{a}` stage `{s}`, not app `{app}` stage `{stage}`",
                run::short_revision(&svc.name)
            ),
        )
        .hint("refusing to overwrite another application's service; rename `app` or the stage")),
    }
}

fn target_generation_from_op(
    op: &google_cloud_longrunning::model::Operation,
    fallback: i64,
) -> i64 {
    op.metadata
        .as_ref()
        .and_then(|m| m.to_msg::<Service>().ok())
        .map(|s| s.generation)
        .filter(|g| *g > 0)
        .unwrap_or(fallback)
}

impl Reconciler<'_> {
    pub async fn get(&self, name: &str) -> Result<Option<Service>> {
        match self.run.get_service().set_name(name).send().await {
            Ok(s) => Ok(Some(s)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(api_error(e, &format!("reading Cloud Run service {name}"))),
        }
    }

    /// The target behind the URL `spec`'s mode changes, when its revision
    /// already runs `spec` (see [`run::revision_matches`]): deploying then
    /// needs no new revision. A revision that cannot be read counts as
    /// different, so the deploy creates a revision as it would otherwise.
    pub async fn matching_revision(
        &self,
        svc: &Service,
        spec: &ServiceSpec,
    ) -> Option<(crate::traffic::Target, String)> {
        let revisions = self.revisions?;
        let (target, short) = run::mode_target(&run::current_traffic(svc), &spec.traffic.mode)?;
        let rev = revisions
            .get_revision()
            .set_name(format!("{}/revisions/{short}", svc.name))
            .send()
            .await
            .ok()?;
        run::revision_matches(svc, &rev, spec).then_some((target, short))
    }

    /// Writes the grant record (`runway.dev/grants`) when it differs from
    /// `live`'s; true when it wrote. Nothing without a service, nor an empty
    /// record where there was none.
    pub async fn save_grant_record(
        &self,
        name: &str,
        live: Option<&Service>,
        record: &[crate::provision::ManagedGrant],
    ) -> Result<bool> {
        let Some(svc) = live else {
            return Ok(false);
        };
        let key = crate::provision::ANNOTATION_GRANTS;
        let value = crate::provision::encode_grants(record);
        let current = svc.annotations.get(key);
        if current == Some(&value) || (current.is_none() && record.is_empty()) {
            return Ok(false);
        }
        self.set_annotation(name, key, &value).await.map(|()| true)
    }

    /// Sets one service annotation: an update of `annotations` only, so no
    /// revision is created and traffic is untouched.
    pub async fn set_annotation(&self, name: &str, key: &str, value: &str) -> Result<()> {
        for attempt in 1..=MAX_MUTATION_ATTEMPTS {
            let Some(mut svc) = self.get(name).await? else {
                return Ok(());
            };
            if svc.annotations.get(key).map(String::as_str) == Some(value) {
                return Ok(());
            }
            svc.annotations.insert(key.into(), value.into());
            match self
                .run
                .update_service()
                .set_service(svc)
                .set_update_mask(google_cloud_wkt::FieldMask::default().set_paths(["annotations"]))
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e)
                    if (is_concurrency_conflict(&e) || is_ambiguous(&e))
                        && attempt < MAX_MUTATION_ATTEMPTS =>
                {
                    continue;
                }
                Err(e) => return Err(api_error(e, &format!("updating annotations of {name}"))),
            }
        }
        unreachable!("loop returns")
    }

    /// Creates or updates the service so that it matches `spec`.
    pub async fn apply(
        &self,
        t: &Target<'_>,
        spec: &ServiceSpec,
        mut existing: Option<Service>,
    ) -> Result<Applied> {
        let Target {
            parent,
            service_id,
            name,
            app,
            stage,
            adopt,
            force,
        } = *t;
        let base = spec;
        for attempt in 1..=MAX_MUTATION_ATTEMPTS {
            let last = attempt == MAX_MUTATION_ATTEMPTS;
            // The traffic split depends on the service as just read.
            let spec = &run::spec_for_live(base, existing.as_ref());
            let desired_flat = spec.flatten();
            match existing.take() {
                None => {
                    let req = run::desired_service(spec, name, None);
                    match self
                        .run
                        .create_service()
                        .set_parent(parent)
                        .set_service_id(service_id)
                        .set_service(req)
                        .send()
                        .await
                    {
                        Ok(op) => {
                            return Ok(Applied {
                                change: ServiceChange::Created,
                                target_generation: target_generation_from_op(&op, 1),
                                operation: Some(op.name),
                            });
                        }
                        Err(e)
                            if e.status().is_some_and(|s| s.code == Code::AlreadyExists)
                                || is_ambiguous(&e) =>
                        {
                            let ambiguous = is_ambiguous(&e);
                            self.progress.warn(format!(
                                "create returned `{e}`; re-reading the service before retrying"
                            ));
                            match self.get(name).await? {
                                Some(svc) => {
                                    check_ownership(&svc, app, stage, false)?;
                                    if ambiguous
                                        && diff(&run::observed_flat(&svc), &desired_flat).is_empty()
                                    {
                                        // Our create went through.
                                        return Ok(Applied {
                                            change: ServiceChange::Created,
                                            operation: None,
                                            target_generation: svc.generation.max(1),
                                        });
                                    }
                                    existing = Some(svc);
                                }
                                None if last || !ambiguous => {
                                    return Err(api_error(
                                        e,
                                        &format!("creating Cloud Run service {service_id}"),
                                    ));
                                }
                                None => {}
                            }
                        }
                        Err(e) => {
                            return Err(with_deploy_hints(
                                api_error(e, &format!("creating Cloud Run service {service_id}")),
                                spec,
                            ));
                        }
                    }
                }
                Some(current) => {
                    check_ownership(&current, app, stage, adopt)?;
                    if !force
                        && run::ownership(&current, app, stage) == Ownership::Owned
                        && run::pending_changes(&current, spec).is_empty()
                        && run::annotations_current(&current, spec)
                    {
                        return Ok(Applied {
                            change: ServiceChange::Unchanged,
                            operation: None,
                            target_generation: current.generation,
                        });
                    }
                    let req = run::desired_service(spec, name, Some(&current));
                    match self
                        .run
                        .update_service()
                        .set_service(req)
                        .set_update_mask(run::update_mask())
                        .send()
                        .await
                    {
                        Ok(op) => {
                            return Ok(Applied {
                                change: ServiceChange::Updated,
                                target_generation: target_generation_from_op(
                                    &op,
                                    current.generation + 1,
                                ),
                                operation: Some(op.name),
                            });
                        }
                        Err(e) if (is_concurrency_conflict(&e) || is_ambiguous(&e)) && !last => {
                            self.progress.warn(format!(
                                "update returned `{e}`; re-reading the service before retrying"
                            ));
                            let fresh = self.get(name).await?.ok_or_else(|| {
                                Error::new(
                                    ErrorKind::Deploy,
                                    format!("service {service_id} disappeared during the update"),
                                )
                            })?;
                            if is_ambiguous(&e)
                                && fresh.generation > current.generation
                                && diff(&run::observed_flat(&fresh), &desired_flat).is_empty()
                            {
                                return Ok(Applied {
                                    change: ServiceChange::Updated,
                                    operation: None,
                                    target_generation: fresh.generation,
                                });
                            }
                            existing = Some(fresh);
                        }
                        Err(e) => {
                            return Err(with_deploy_hints(
                                api_error(e, &format!("updating Cloud Run service {service_id}")),
                                spec,
                            ));
                        }
                    }
                }
            }
        }
        Err(Error::new(
            ErrorKind::Deploy,
            format!(
                "could not apply changes to {service_id} after {MAX_MUTATION_ATTEMPTS} attempts"
            ),
        ))
    }

    /// Waits for the operation (if known) and for the service to finish reconciling.
    pub async fn wait_ready(&self, name: &str, applied: &Applied) -> Result<Service> {
        let mut poller = Poller::new(self.poll, self.timeout);
        let mut op_done = applied.operation.is_none();
        let mut op_error: Option<String> = None;
        let mut last_msg = String::new();
        let mut reread_now = false;
        loop {
            // Operation and service are read concurrently: one round trip per poll.
            let op_name = applied.operation.as_deref().filter(|_| !op_done);
            let op_read = async {
                match op_name {
                    Some(n) => Some(self.run.get_operation().set_name(n).send().await),
                    None => None,
                }
            };
            let svc_read = self.run.get_service().set_name(name).send();
            let (op_res, svc_res) = tokio::join!(op_read, svc_read);
            let was_done = op_done;
            match op_res {
                Some(Ok(op)) if op.done => {
                    op_done = true;
                    if let Some(operation::Result::Error(st)) = &op.result {
                        op_error = Some(st.message.clone());
                    }
                }
                Some(Ok(_)) | None => {}
                // Fall back to the service status.
                Some(Err(e)) if is_not_found(&e) => op_done = true,
                Some(Err(e)) if is_ambiguous(&e) => {}
                Some(Err(e)) => return Err(api_error(e, "reading the Cloud Run operation")),
            }
            let svc = match svc_res {
                Ok(s) => Some(s),
                Err(e) if is_ambiguous(&e) => None,
                Err(e) => return Err(api_error(e, &format!("reading Cloud Run service {name}"))),
            };
            // The service may have been read just before the operation finished:
            // re-read immediately instead of waiting a full interval.
            if op_done && !was_done && !reread_now {
                reread_now = true;
                if svc.as_ref().is_none_or(|s| s.reconciling) {
                    continue;
                }
            }
            if let Some(svc) = svc {
                let r = run::readiness(&svc);
                if let Readiness::Reconciling { message } = &r
                    && message != &last_msg
                {
                    self.progress.info(format!("waiting: {message}"));
                    last_msg = message.clone();
                }
                let settled = op_done
                    && !svc.reconciling
                    && (svc.generation >= applied.target_generation)
                    && !matches!(r, Readiness::Reconciling { .. });
                if settled {
                    return match r {
                        Readiness::Ready if op_error.is_none() => Ok(svc),
                        Readiness::Ready => {
                            Err(self.failure(&svc, op_error.unwrap_or_default()).await)
                        }
                        Readiness::Failed { message } => {
                            let msg = match op_error {
                                Some(o) if !message.contains(&o) => format!("{o}; {message}"),
                                _ => message,
                            };
                            Err(self.failure(&svc, msg).await)
                        }
                        Readiness::Reconciling { .. } => unreachable!(),
                    };
                }
            }
            match poller.wait().await {
                Tick::Continue => {}
                Tick::TimedOut => {
                    return Err(Error::new(
                        ErrorKind::Timeout,
                        format!(
                            "timed out after {}s waiting for {} to become ready",
                            self.timeout.as_secs(),
                            run::short_revision(name)
                        ),
                    )
                    .hint("the rollout continues in Google Cloud; check progress with `runway info`, or raise `--timeout`"));
                }
                Tick::Cancelled => {
                    return Err(Error::new(
                        ErrorKind::Interrupted,
                        "interrupted while waiting for the rollout",
                    )
                    .hint("Cloud Run continues the rollout in the background; run `runway info` to see the outcome"));
                }
            }
        }
    }

    async fn failure(&self, svc: &Service, message: String) -> Error {
        let mut msg = format!(
            "revision {} of {} is not healthy: {message}",
            run::short_revision(&svc.latest_created_revision),
            run::short_revision(&svc.name)
        );
        let mut log_uri = None;
        if let Some(rev) = self.revisions
            && !svc.latest_created_revision.is_empty()
            && let Ok(r) = rev
                .get_revision()
                .set_name(&svc.latest_created_revision)
                .send()
                .await
        {
            for c in &r.conditions {
                if c.state == google_cloud_run_v2::model::condition::State::ConditionFailed
                    && !c.message.is_empty()
                    && !msg.contains(&c.message)
                {
                    msg.push_str(&format!("\n  {}: {}", c.r#type, c.message));
                }
            }
            if !r.log_uri.is_empty() {
                log_uri = Some(r.log_uri.clone());
            }
        }
        if !svc.latest_ready_revision.is_empty() {
            msg.push_str(&format!(
                "\n  traffic remains on the previous ready revision {}",
                run::short_revision(&svc.latest_ready_revision)
            ));
        }
        let mut e = Error::new(ErrorKind::Deploy, msg.clone());
        let lower = msg.to_ascii_lowercase();
        if lower.contains("listen on the port") || lower.contains("failed to start") {
            e = e.hint("make sure the container listens on 0.0.0.0:$PORT (runway sets PORT from `service.port`) and starts within the startup timeout");
        }
        if lower.contains("secret") {
            e = e.hint("grant the runtime service account roles/secretmanager.secretAccessor on the secret, and check the secret version exists and is enabled");
        }
        if lower.contains("image") && (lower.contains("not found") || lower.contains("denied")) {
            e = e.hint("the Cloud Run service agent must be able to pull the image (roles/artifactregistry.reader on the repository for cross-project images)");
        }
        e = e.hint("inspect application logs with `runway logs --stage <stage> --since 15m`");
        if let Some(u) = log_uri {
            e = e.hint(format!("revision logs: {u}"));
        }
        e
    }

    /// Reads whether the service is currently public. `None` if the policy is not readable.
    pub async fn current_public(&self, name: &str) -> Result<Option<bool>> {
        match self
            .run
            .get_iam_policy()
            .set_resource(name)
            .set_options(GetPolicyOptions::new().set_requested_policy_version(3))
            .send()
            .await
        {
            Ok(p) => Ok(Some(iam::is_public(&p))),
            Err(e) if is_not_found(&e) => Ok(Some(false)),
            Err(e) if e.status().is_some_and(|s| s.code == Code::PermissionDenied) => Ok(None),
            Err(e) => Err(api_error(e, "reading the service IAM policy")),
        }
    }

    /// Ensures `allUsers` holds (or does not hold) `roles/run.invoker`, preserving other bindings.
    pub async fn ensure_access(&self, name: &str, public: bool) -> Result<AccessChange> {
        for attempt in 1..=MAX_MUTATION_ATTEMPTS {
            let mut policy = self
                .run
                .get_iam_policy()
                .set_resource(name)
                .set_options(GetPolicyOptions::new().set_requested_policy_version(3))
                .send()
                .await
                .map_err(|e| {
                    api_error(e, "reading the service IAM policy")
                        .hint("the deploying principal needs run.services.getIamPolicy")
                })?;
            if !iam::set_public(&mut policy, public) {
                return Ok(AccessChange::Unchanged);
            }
            match self
                .run
                .set_iam_policy()
                .set_resource(name)
                .set_policy(policy)
                .send()
                .await
            {
                Ok(_) => {
                    return Ok(if public {
                        AccessChange::MadePublic
                    } else {
                        AccessChange::MadePrivate
                    });
                }
                Err(e)
                    if (is_concurrency_conflict(&e) || is_ambiguous(&e))
                        && attempt < MAX_MUTATION_ATTEMPTS =>
                {
                    self.progress
                        .warn(format!("IAM update returned `{e}`; re-reading the policy"));
                }
                Err(e) => {
                    let msg = e.status().map(|s| s.message.clone()).unwrap_or_default();
                    let mut err = api_error(e, "updating the service IAM policy")
                        .hint("the deploying principal needs run.services.setIamPolicy");
                    if public && msg.contains("permitted customer") {
                        err = err.hint("an organization policy (Domain Restricted Sharing) blocks `allUsers`; keep the service private or ask an administrator for an exception");
                    }
                    return Err(err);
                }
            }
        }
        unreachable!("loop returns on the last attempt")
    }
}

fn with_deploy_hints(mut e: Error, spec: &ServiceSpec) -> Error {
    let m = e.message.to_ascii_lowercase();
    if m.contains("actas")
        || m.contains("iam.serviceaccounts.actas")
        || m.contains("service account")
    {
        e = e.hint(format!(
            "the deploying principal needs roles/iam.serviceAccountUser on {}",
            spec.service_account
        ));
    }
    if m.contains("secret") {
        e = e.hint(
            "check that each referenced secret and version exists (runway doctor verifies this)",
        );
    }
    e
}

#[cfg(test)]
mod tests;
