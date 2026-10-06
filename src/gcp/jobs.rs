//! Cloud Run jobs: the request runway sends, what it reads back, and how it
//! is applied (create or update, then wait for the operation). The container
//! and its volumes are built like a service's app container.

use crate::config::JobSettings;
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::run::{self, Ownership};
use crate::gcp::{api_error, is_not_found};
use crate::output::Progress;
use crate::plan::{FieldChange, ServiceSpec, diff};
use crate::poll::{PollConfig, Poller, Tick};
use google_cloud_api::model::LaunchStage;
use google_cloud_longrunning::model::operation;
use google_cloud_run_v2::client::Jobs;
use google_cloud_run_v2::model::{
    ExecutionTemplate, Job, RevisionTemplate, Service, TaskTemplate, task_template,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// Keys of [`ServiceSpec::flatten`] that apply to a job (the others are
/// about serving requests).
fn job_key(k: &str) -> bool {
    matches!(
        k,
        "image"
            | "cpu"
            | "memory"
            | "service_account"
            | "sandbox"
            | "execution_environment"
            | "vpc"
            | "cloud_sql"
            | "command"
            | "args"
            | "timeout_seconds"
    ) || [
        "env.",
        "secrets.",
        "secret_files.",
        "labels.",
        "annotations.",
        "volumes.",
    ]
    .iter()
    .any(|p| k.starts_with(p))
}

/// The configuration of a job, as compared with the live one.
pub fn desired_flat(spec: &ServiceSpec, j: &JobSettings) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = spec
        .flatten()
        .into_iter()
        .filter(|(k, _)| job_key(k))
        .collect();
    m.insert("tasks".into(), j.tasks.to_string());
    m.insert("parallelism".into(), j.parallelism.to_string());
    m.insert("max_retries".into(), j.max_retries.to_string());
    m
}

/// The live configuration of a job, with the same keys as [`desired_flat`].
pub fn observed_flat(job: &Job) -> BTreeMap<String, String> {
    let t = job.template.clone().unwrap_or_default();
    let task = t.template.clone().unwrap_or_default();
    // Read through the service view, so both are compared the same way.
    let as_service = Service::new()
        .set_name(job.name.replacen("/jobs/", "/services/", 1))
        .set_labels(job.labels.clone())
        .set_annotations(job.annotations.clone())
        .set_template(
            RevisionTemplate::new()
                .set_containers(task.containers.clone())
                .set_volumes(task.volumes.clone())
                .set_service_account(&task.service_account)
                .set_execution_environment(task.execution_environment.clone())
                .set_or_clear_vpc_access(task.vpc_access.clone())
                .set_or_clear_timeout(task.timeout),
        );
    let mut m: BTreeMap<String, String> = run::observed_flat(&as_service)
        .into_iter()
        .filter(|(k, _)| job_key(k))
        .collect();
    m.insert("tasks".into(), t.task_count.max(1).to_string());
    m.insert("parallelism".into(), t.parallelism.to_string());
    let retries = match &task.retries {
        Some(task_template::Retries::MaxRetries(n)) => *n,
        _ => 3,
    };
    m.insert("max_retries".into(), retries.to_string());
    m
}

/// The Job message for create (`existing = None`) or update. Labels and
/// annotations set by others are kept, as on services.
pub fn desired_job(spec: &ServiceSpec, j: &JobSettings, existing: Option<&Job>) -> Job {
    let mut labels: BTreeMap<String, String> = existing
        .map(|e| {
            e.labels
                .iter()
                .filter(|(k, _)| !run::is_reserved_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    labels.extend(spec.labels.clone());
    let mut annotations: BTreeMap<String, String> = existing
        .map(|e| {
            e.annotations
                .iter()
                .filter(|(k, _)| !k.starts_with("runway.dev/") && !run::is_reserved_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    annotations.extend(spec.annotations.clone());
    let (container, volumes) = run::app_container_and_volumes(spec);
    let container = match spec.sandbox {
        true => run::with_sandbox_launcher(container),
        false => container,
    };
    let task = TaskTemplate::new()
        .set_containers([container])
        .set_volumes(volumes)
        .set_service_account(&spec.service_account)
        .set_timeout(
            google_cloud_wkt::Duration::new(spec.timeout_seconds as i64, 0)
                .expect("timeout within range"),
        )
        .set_max_retries(j.max_retries as i32)
        .set_execution_environment(run::execution_environment(spec))
        .set_or_clear_vpc_access(run::vpc_access(spec));
    let launch_stage = match (spec.sandbox, existing) {
        (true, _) => LaunchStage::Beta,
        (false, Some(e)) => e.launch_stage.clone(),
        (false, None) => LaunchStage::default(),
    };
    let mut job = Job::new()
        .set_labels(labels)
        .set_annotations(annotations)
        .set_client(run::CLIENT_NAME)
        .set_client_version(env!("CARGO_PKG_VERSION"))
        .set_launch_stage(launch_stage)
        .set_template(
            ExecutionTemplate::new()
                .set_labels(spec.labels.clone())
                .set_task_count(j.tasks as i32)
                .set_parallelism(j.parallelism as i32)
                .set_template(task),
        );
    if let Some(e) = existing {
        job = job.set_name(&e.name).set_etag(&e.etag);
    }
    job
}

/// What applying a job did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobChange {
    Created,
    Updated,
    Unchanged,
}

/// Changes from the live job (none for a job to create).
pub fn changes(live: Option<&Job>, spec: &ServiceSpec, j: &JobSettings) -> Vec<FieldChange> {
    let desired = desired_flat(spec, j);
    match live {
        Some(job) => diff(&observed_flat(job), &desired),
        None => diff(&BTreeMap::new(), &desired),
    }
}

pub struct JobReconciler<'a> {
    pub jobs: &'a Jobs,
    pub progress: &'a Progress,
    pub poll: PollConfig,
    pub timeout: Duration,
}

impl JobReconciler<'_> {
    pub async fn get(&self, name: &str) -> Result<Option<Job>> {
        match self.jobs.get_job().set_name(name).send().await {
            Ok(j) => Ok(Some(j)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(api_error(e, &format!("reading Cloud Run job {name}"))),
        }
    }

    /// Creates or updates the job when its configuration differs, then waits
    /// for the operation. Ownership is checked first.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply(
        &self,
        parent: &str,
        job_id: &str,
        spec: &ServiceSpec,
        j: &JobSettings,
        existing: Option<Job>,
        app: &str,
        stage: &str,
    ) -> Result<JobChange> {
        if let Some(job) = &existing {
            check_job_ownership(job, app, stage)?;
            if changes(Some(job), spec, j).is_empty() {
                return Ok(JobChange::Unchanged);
            }
        }
        let req = desired_job(spec, j, existing.as_ref());
        let (op, change) = match existing {
            None => (
                self.jobs
                    .create_job()
                    .set_parent(parent)
                    .set_job_id(job_id)
                    .set_job(req)
                    .send()
                    .await
                    .map_err(|e| api_error(e, &format!("creating Cloud Run job {job_id}")))?,
                JobChange::Created,
            ),
            Some(_) => (
                self.jobs
                    .update_job()
                    .set_job(req)
                    .send()
                    .await
                    .map_err(|e| api_error(e, &format!("updating Cloud Run job {job_id}")))?,
                JobChange::Updated,
            ),
        };
        self.wait(&op.name, job_id).await?;
        Ok(change)
    }

    async fn wait(&self, operation: &str, job_id: &str) -> Result<()> {
        let mut poller = Poller::new(self.poll, self.timeout);
        loop {
            let op = self
                .jobs
                .get_operation()
                .set_name(operation)
                .send()
                .await
                .map_err(|e| api_error(e, "reading the Cloud Run operation"))?;
            if op.done {
                return match &op.result {
                    Some(operation::Result::Error(st)) => Err(Error::new(
                        ErrorKind::Deploy,
                        format!("Cloud Run job {job_id} was not applied: {}", st.message),
                    )
                    .permanent()),
                    _ => Ok(()),
                };
            }
            match poller.wait().await {
                Tick::Continue => {}
                Tick::TimedOut => {
                    return Err(Error::new(
                        ErrorKind::Timeout,
                        format!("Cloud Run job {job_id} is still being applied"),
                    )
                    .hint("re-run `runway deploy`: an applied job is detected and kept"));
                }
                Tick::Cancelled => {
                    return Err(Error::new(
                        ErrorKind::Interrupted,
                        "interrupted while waiting for the job to be applied",
                    ));
                }
            }
        }
    }

    /// Sets one annotation on the job (a read, then an update with its etag).
    pub async fn set_annotation(&self, name: &str, key: &str, value: &str) -> Result<()> {
        let Some(mut job) = self.get(name).await? else {
            return Ok(());
        };
        if job.annotations.get(key).map(String::as_str) == Some(value) {
            return Ok(());
        }
        job.annotations.insert(key.into(), value.into());
        let op = self
            .jobs
            .update_job()
            .set_job(job)
            .send()
            .await
            .map_err(|e| api_error(e, &format!("updating Cloud Run job {name}")))?;
        self.wait(&op.name, name).await
    }

    pub async fn delete(&self, name: &str) -> Result<bool> {
        match self.jobs.delete_job().set_name(name).send().await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(api_error(e, &format!("deleting Cloud Run job {name}"))),
        }
    }
}

/// Refuses a job runway does not own for this app and stage.
pub fn check_job_ownership(job: &Job, app: &str, stage: &str) -> Result<()> {
    let id = run::short_revision(&job.name);
    match run::ownership_of(&job.labels, app, stage) {
        Ownership::Owned => Ok(()),
        Ownership::Unmanaged => Err(Error::new(
            ErrorKind::Conflict,
            format!("Cloud Run job {id} already exists and is not managed by runway"),
        )
        .hint("rename the job in runway.yaml, or delete the existing job")),
        Ownership::OtherOwner { app: a, stage: s } => Err(Error::new(
            ErrorKind::Conflict,
            format!(
                "Cloud Run job {id} is managed by runway for app `{a}` stage `{s}`, not app `{app}` stage `{stage}`"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        let mut s = crate::gcp::run::tests::spec();
        s.command = vec!["python".into(), "manage.py".into(), "migrate".into()];
        s.timeout_seconds = 600;
        s
    }

    const SETTINGS: JobSettings = JobSettings {
        tasks: 4,
        parallelism: 2,
        max_retries: 1,
    };

    #[test]
    fn what_runway_sends_reads_back_without_a_change() {
        let s = spec();
        let mut job = desired_job(&s, &SETTINGS, None);
        job.name = "projects/p/locations/europe-west1/jobs/shop-migrate-prod".into();
        let t = job.template.as_ref().unwrap();
        assert_eq!((t.task_count, t.parallelism), (4, 2));
        let task = t.template.as_ref().unwrap();
        assert!(matches!(
            task.retries,
            Some(task_template::Retries::MaxRetries(1))
        ));
        assert_eq!(task.containers[0].command, s.command);
        assert!(
            task.containers[0].ports.is_empty(),
            "a job serves no requests"
        );
        let changes = diff(&observed_flat(&job), &desired_flat(&s, &SETTINGS));
        assert!(changes.is_empty(), "{changes:?}");
        // Serving settings are not compared on jobs.
        assert!(!desired_flat(&s, &SETTINGS).contains_key("concurrency"));
    }

    #[test]
    fn a_changed_setting_is_a_change() {
        let s = spec();
        let live = desired_job(&s, &SETTINGS, None);
        let more = JobSettings {
            tasks: 8,
            ..SETTINGS
        };
        let c = changes(Some(&live), &s, &more);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(
            (c[0].field.as_str(), c[0].after.as_deref()),
            ("tasks", Some("8"))
        );
    }

    #[test]
    fn sandboxes_are_enabled_on_the_job_container() {
        let mut s = spec();
        s.sandbox = true;
        let job = desired_job(&s, &SETTINGS, None);
        assert_eq!(job.launch_stage, LaunchStage::Beta);
        let c = &job.template.unwrap().template.unwrap().containers[0];
        assert_eq!(serde_json::to_value(c).unwrap()["sandboxLauncher"], true);
    }
}
