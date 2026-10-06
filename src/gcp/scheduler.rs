//! Cloud Scheduler jobs: what runway sends and compares, and the calls that
//! apply it. Scheduler jobs have no labels: ownership is a marker in the
//! description, as for the service accounts runway creates.

use crate::config::{ScheduleConfig, ScheduleTarget};
use crate::error::Result;
use crate::gcp::{api_error, is_not_found};
use google_cloud_scheduler_v1::client::CloudScheduler;
use google_cloud_scheduler_v1::model::{
    HttpMethod, HttpTarget, Job, OAuthToken, OidcToken, RetryConfig, http_target, job,
};
use std::collections::BTreeMap;

/// Scope of the OAuth token used to run a Cloud Run job.
const CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Ownership marker written into a scheduler job's description.
pub fn marker(app: &str, stage: &str) -> String {
    format!("managed-by=runway app={app} stage={stage}")
}

pub fn owned(job: &Job, app: &str, stage: &str) -> bool {
    let tokens: std::collections::BTreeSet<&str> = job.description.split_whitespace().collect();
    marker(app, stage).split(' ').all(|t| tokens.contains(t))
}

pub fn parent(project: &str, region: &str) -> String {
    format!("projects/{project}/locations/{region}")
}

pub fn job_name(project: &str, region: &str, id: &str) -> String {
    format!("{}/jobs/{id}", parent(project, region))
}

/// Where a target is called: the Cloud Run Admin API `run` method of a job,
/// or the service URL plus the path. `service_url` is the live service's URL.
pub fn target_uri(
    s: &ScheduleConfig,
    project: &str,
    run_region: &str,
    service_url: Option<&str>,
) -> Option<String> {
    match &s.target {
        ScheduleTarget::Job { job_id, .. } => Some(format!(
            "https://run.googleapis.com/v2/projects/{project}/locations/{run_region}/jobs/{job_id}:run"
        )),
        ScheduleTarget::Service { path, .. } => {
            service_url.map(|u| format!("{}{path}", u.trim_end_matches('/')))
        }
    }
}

fn method(m: &str) -> HttpMethod {
    match m {
        "GET" => HttpMethod::Get,
        "HEAD" => HttpMethod::Head,
        "PUT" => HttpMethod::Put,
        "DELETE" => HttpMethod::Delete,
        "PATCH" => HttpMethod::Patch,
        "OPTIONS" => HttpMethod::Options,
        _ => HttpMethod::Post,
    }
}

/// The scheduler job for a schedule; `uri` from [`target_uri`]. A service is
/// called with an ID token for its URL, a job is run with an OAuth token.
pub fn desired(
    s: &ScheduleConfig,
    name: &str,
    app: &str,
    stage: &str,
    invoker: &str,
    uri: &str,
    service_url: Option<&str>,
) -> Job {
    let mut target = HttpTarget::new().set_uri(uri);
    target = match &s.target {
        ScheduleTarget::Job { .. } => target.set_http_method(HttpMethod::Post).set_oauth_token(
            OAuthToken::new()
                .set_service_account_email(invoker)
                .set_scope(CLOUD_PLATFORM),
        ),
        ScheduleTarget::Service {
            method: m,
            body,
            headers,
            ..
        } => {
            let t = target
                .set_http_method(method(m))
                .set_headers(headers.clone())
                .set_oidc_token(
                    OidcToken::new()
                        .set_service_account_email(invoker)
                        .set_audience(service_url.unwrap_or(uri).trim_end_matches('/')),
                );
            match body {
                Some(b) => t.set_body(bytes::Bytes::from(b.clone().into_bytes())),
                None => t,
            }
        }
    };
    Job::new()
        .set_name(name)
        // The marker's words stand alone: ownership compares whole words.
        .set_description(format!("runway schedule {}: {}", s.key, marker(app, stage)))
        .set_schedule(&s.schedule)
        .set_time_zone(&s.time_zone)
        .set_retry_config(RetryConfig::new().set_retry_count(s.retries as i32))
        .set_attempt_deadline(
            google_cloud_wkt::Duration::new(s.attempt_deadline_seconds as i64, 0)
                .expect("deadline within range"),
        )
        .set_http_target(target)
}

/// What runway compares on a scheduler job.
pub fn flat(j: &Job) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("schedule".into(), j.schedule.clone());
    m.insert(
        "time_zone".into(),
        if j.time_zone.is_empty() {
            "Etc/UTC".into()
        } else {
            j.time_zone.clone()
        },
    );
    m.insert(
        "retries".into(),
        j.retry_config
            .as_ref()
            .map_or(0, |r| r.retry_count)
            .to_string(),
    );
    m.insert(
        "attempt_deadline_seconds".into(),
        j.attempt_deadline
            .as_ref()
            .map_or(180, |d| d.seconds())
            .to_string(),
    );
    m.insert("paused".into(), (j.state == job::State::Paused).to_string());
    if let Some(job::Target::HttpTarget(t)) = &j.target {
        let verb = match &t.http_method {
            HttpMethod::Unspecified => "POST".to_string(),
            other => other.name().unwrap_or("POST").to_string(),
        };
        m.insert("target".into(), format!("{verb} {}", t.uri));
        let auth = match &t.authorization_header {
            Some(http_target::AuthorizationHeader::OauthToken(o)) => {
                format!("OAuth token as {}", o.service_account_email)
            }
            Some(http_target::AuthorizationHeader::OidcToken(o)) => {
                format!("ID token as {} for {}", o.service_account_email, o.audience)
            }
            _ => "none".into(),
        };
        m.insert("auth".into(), auth);
        for (k, v) in &t.headers {
            // Cloud Scheduler adds its own headers (User-Agent and others).
            if !k.eq_ignore_ascii_case("user-agent") && !k.starts_with("X-CloudScheduler") {
                m.insert(format!("headers.{k}"), v.clone());
            }
        }
        if !t.body.is_empty() {
            m.insert("body".into(), String::from_utf8_lossy(&t.body).into_owned());
        }
    } else {
        m.insert("target".into(), "(not an HTTP target)".into());
    }
    m
}

/// [`flat`] of what runway would send, with the desired paused state.
pub fn desired_flat(job: &Job, paused: bool) -> BTreeMap<String, String> {
    let mut m = flat(job);
    m.insert("paused".into(), paused.to_string());
    m
}

pub async fn get(client: &CloudScheduler, name: &str) -> Result<Option<Job>> {
    match client.get_job().set_name(name).send().await {
        Ok(j) => Ok(Some(j)),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(api_error(e, &format!("reading Cloud Scheduler job {name}"))),
    }
}

/// Scheduler jobs runway created for this app and stage.
pub async fn list_owned(
    client: &CloudScheduler,
    parent: &str,
    app: &str,
    stage: &str,
) -> Result<Vec<Job>> {
    let mut out = Vec::new();
    let mut token = String::new();
    loop {
        let resp = match client
            .list_jobs()
            .set_parent(parent)
            .set_page_token(token.clone())
            .send()
            .await
        {
            Ok(r) => r,
            // Without the Cloud Scheduler API there can be no scheduler job.
            Err(e) if is_not_found(&e) || crate::gcp::is_service_disabled(&e) => return Ok(out),
            Err(e) => {
                return Err(api_error(
                    e,
                    &format!("listing Cloud Scheduler jobs in {parent}"),
                ));
            }
        };
        out.extend(resp.jobs.into_iter().filter(|j| owned(j, app, stage)));
        if resp.next_page_token.is_empty() {
            return Ok(out);
        }
        token = resp.next_page_token;
    }
}

/// Creates or updates the scheduler job, then pauses or resumes it.
pub async fn apply(
    client: &CloudScheduler,
    parent: &str,
    desired: Job,
    exists: bool,
    paused: bool,
) -> Result<Job> {
    let name = desired.name.clone();
    let job = if exists {
        client
            .update_job()
            .set_job(desired)
            .send()
            .await
            .map_err(|e| api_error(e, &format!("updating Cloud Scheduler job {name}")))?
    } else {
        client
            .create_job()
            .set_parent(parent)
            .set_job(desired)
            .send()
            .await
            .map_err(|e| api_error(e, &format!("creating Cloud Scheduler job {name}")))?
    };
    let is_paused = job.state == job::State::Paused;
    let job = match (paused, is_paused) {
        (true, false) => client
            .pause_job()
            .set_name(&name)
            .send()
            .await
            .map_err(|e| api_error(e, &format!("pausing Cloud Scheduler job {name}")))?,
        (false, true) => client
            .resume_job()
            .set_name(&name)
            .send()
            .await
            .map_err(|e| api_error(e, &format!("resuming Cloud Scheduler job {name}")))?,
        _ => job,
    };
    Ok(job)
}

/// `None` when the scheduler job does not exist, else whether runway
/// created it for this app and stage.
pub async fn ownership(
    client: &CloudScheduler,
    name: &str,
    app: &str,
    stage: &str,
) -> Result<Option<bool>> {
    Ok(get(client, name).await?.map(|j| owned(&j, app, stage)))
}

/// Deletes a scheduler job after reading it again: only one runway created
/// for this app and stage (false when it is already gone).
pub async fn delete_owned(
    client: &CloudScheduler,
    name: &str,
    app: &str,
    stage: &str,
) -> Result<bool> {
    match ownership(client, name, app, stage).await? {
        None => Ok(false),
        Some(false) => Err(crate::error::Error::new(
            crate::error::ErrorKind::Conflict,
            format!(
                "Cloud Scheduler job {} was not created by runway for app `{app}` stage `{stage}`; not deleting it",
                name.rsplit('/').next().unwrap_or(name)
            ),
        )
        .permanent()),
        Some(true) => delete(client, name).await,
    }
}

pub async fn delete(client: &CloudScheduler, name: &str) -> Result<bool> {
    match client.delete_job().set_name(name).send().await {
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(api_error(
            e,
            &format!("deleting Cloud Scheduler job {name}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(target: ScheduleTarget) -> ScheduleConfig {
        ScheduleConfig {
            key: "nightly".into(),
            id: "shop-nightly-prod".into(),
            schedule: "0 3 * * *".into(),
            time_zone: "Europe/Paris".into(),
            target,
            retries: 2,
            attempt_deadline_seconds: 300,
            paused: false,
        }
    }

    const INVOKER: &str = "shop-prod-sched@my-gcp-project.iam.gserviceaccount.com";

    #[test]
    fn a_job_is_run_through_the_cloud_run_api_with_an_oauth_token() {
        let s = schedule(ScheduleTarget::Job {
            name: "migrate".into(),
            job_id: "shop-migrate-prod".into(),
        });
        let uri = target_uri(&s, "my-gcp-project", "europe-west1", None).unwrap();
        assert_eq!(
            uri,
            "https://run.googleapis.com/v2/projects/my-gcp-project/locations/europe-west1/jobs/shop-migrate-prod:run"
        );
        let name = job_name("my-gcp-project", "europe-west1", &s.id);
        let j = desired(&s, &name, "shop", "prod", INVOKER, &uri, None);
        assert!(owned(&j, "shop", "prod") && !owned(&j, "shop", "dev"));
        let f = flat(&j);
        assert_eq!(f["target"], format!("POST {uri}"));
        assert_eq!(f["auth"], format!("OAuth token as {INVOKER}"));
        assert_eq!(
            (
                f["retries"].as_str(),
                f["attempt_deadline_seconds"].as_str()
            ),
            ("2", "300")
        );
        assert!(crate::plan::diff(&f, &desired_flat(&j, false)).is_empty());
        assert_eq!(desired_flat(&j, true)["paused"], "true");
    }

    #[test]
    fn a_service_is_called_with_an_id_token_for_its_url() {
        let s = schedule(ScheduleTarget::Service {
            name: "web".into(),
            service_id: "shop-web-prod".into(),
            path: "/tasks/warm".into(),
            method: "GET".into(),
            body: None,
            headers: [("X-Source".to_string(), "runway".to_string())].into(),
        });
        assert_eq!(
            target_uri(&s, "p", "europe-west1", None),
            None,
            "no URL before the service exists"
        );
        let url = "https://shop-web-prod-abc-ew.a.run.app";
        let uri = target_uri(&s, "p", "europe-west1", Some(url)).unwrap();
        assert_eq!(uri, format!("{url}/tasks/warm"));
        let j = desired(&s, "n", "shop", "prod", INVOKER, &uri, Some(url));
        let f = flat(&j);
        assert_eq!(f["target"], format!("GET {uri}"));
        assert_eq!(f["auth"], format!("ID token as {INVOKER} for {url}"));
        assert_eq!(f["headers.X-Source"], "runway");
    }
}
