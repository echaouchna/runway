//! Cloud Logging queries for application and build logs.

use crate::error::{Error, Result};
use crate::gcp::api_error;
use chrono::{DateTime, SecondsFormat, Utc};
use google_cloud_logging_v2::client::LoggingServiceV2;
use google_cloud_logging_v2::model::{LogEntry, log_entry};
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct LogQuery {
    pub project: String,
    pub region: String,
    pub service_id: String,
    pub since: Duration,
    pub limit: usize,
    pub include_requests: bool,
    pub min_severity: Option<String>,
}

/// Cloud Logging filter selecting a service's application logs.
pub fn service_filter(q: &LogQuery, now: DateTime<Utc>) -> String {
    let start = now - chrono::Duration::from_std(q.since).unwrap_or(chrono::Duration::hours(1));
    service_filter_after(q, &start.to_rfc3339_opts(SecondsFormat::Micros, true))
}

fn service_filter_after(q: &LogQuery, ts: &str) -> String {
    let mut f = vec![
        r#"resource.type="cloud_run_revision""#.to_string(),
        format!(r#"resource.labels.service_name="{}""#, q.service_id),
        format!(r#"resource.labels.location="{}""#, q.region),
        format!(r#"timestamp>="{ts}""#),
    ];
    // Google's audit records (who read which object, etc.) are not application logs.
    f.push(format!(
        r#"-logName:"projects/{}/logs/cloudaudit.googleapis.com""#,
        q.project
    ));
    if !q.include_requests {
        f.push(format!(
            r#"-logName="projects/{}/logs/run.googleapis.com%2Frequests""#,
            q.project
        ));
    }
    if let Some(s) = &q.min_severity {
        f.push(format!("severity>={}", s.to_ascii_uppercase()));
    }
    f.join("\n")
}

/// Cloud Logging filter for a Cloud Build's logs (CLOUD_LOGGING_ONLY mode).
pub fn build_filter(build_id: &str) -> String {
    format!("resource.type=\"build\"\nresource.labels.build_id=\"{build_id}\"")
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LogLine {
    pub timestamp: String,
    pub severity: String,
    pub revision: String,
    pub message: String,
    pub insert_id: String,
}

pub fn to_line(e: &LogEntry) -> LogLine {
    let timestamp = e.timestamp.map(String::from).unwrap_or_default();
    let severity = e.severity.name().unwrap_or("DEFAULT").to_string();
    let revision = e
        .resource
        .as_ref()
        .and_then(|r| r.labels.get("revision_name").cloned())
        .unwrap_or_default();
    let message = match &e.payload {
        Some(log_entry::Payload::TextPayload(t)) => t.trim_end().to_string(),
        Some(log_entry::Payload::JsonPayload(j)) => ["message", "msg", "textPayload"]
            .iter()
            .find_map(|k| j.get(*k).and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_else(|| serde_json::Value::Object((**j).clone()).to_string()),
        _ => match &e.http_request {
            Some(h) => {
                let latency = h
                    .latency
                    .as_ref()
                    .map(|d| format!(" {}ms", d.seconds() * 1000 + (d.nanos() / 1_000_000) as i64))
                    .unwrap_or_default();
                format!(
                    "{} {} {}{}",
                    h.request_method, h.status, h.request_url, latency
                )
            }
            None => String::new(),
        },
    };
    // Entries without text (system events, audit records) show their log name.
    let message = if message.is_empty() {
        let log = e
            .log_name
            .rsplit('/')
            .next()
            .unwrap_or(&e.log_name)
            .replace("%2F", "/");
        match &e.payload {
            Some(log_entry::Payload::ProtoPayload(any)) => format!(
                "({log}: {})",
                any.type_url()
                    .unwrap_or("structured entry")
                    .rsplit('/')
                    .next()
                    .unwrap_or("structured entry")
            ),
            _ => format!("({log})"),
        }
    } else {
        message
    };
    LogLine {
        timestamp,
        severity,
        revision,
        message,
        insert_id: e.insert_id.clone(),
    }
}

/// Fetches up to `limit` matching entries, returned oldest first.
pub async fn fetch(
    client: &LoggingServiceV2,
    project: &str,
    filter: &str,
    limit: usize,
) -> Result<Vec<LogLine>> {
    let mut out = Vec::new();
    let mut page_token = String::new();
    loop {
        let resp = client
            .list_log_entries()
            .set_resource_names([format!("projects/{project}")])
            .set_filter(filter)
            .set_order_by("timestamp desc")
            .set_page_size(limit.clamp(1, 1000) as i32)
            .set_page_token(page_token.clone())
            .send()
            .await
            .map_err(|e| api_error(e, "reading logs from Cloud Logging"))?;
        for e in &resp.entries {
            out.push(to_line(e));
            if out.len() >= limit {
                break;
            }
        }
        if out.len() >= limit || resp.next_page_token.is_empty() {
            break;
        }
        page_token = resp.next_page_token;
    }
    out.reverse();
    Ok(out)
}

/// Bounded set of recently printed entry IDs (O(1) lookups, oldest evicted first).
#[derive(Debug, Default)]
pub struct SeenIds {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

impl SeenIds {
    const CAPACITY: usize = 5000;

    /// Records `id`; returns false if it was already present.
    pub fn insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > Self::CAPACITY
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

/// Follows logs: polls for entries newer than the last one seen until cancelled.
pub async fn follow(
    client: &LoggingServiceV2,
    q: &LogQuery,
    mut last_ts: String,
    mut seen: SeenIds,
    mut emit: impl FnMut(&LogLine),
) -> Result<()> {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let filter = service_filter_after(q, &last_ts);
        let lines = fetch(client, &q.project, &filter, 1000).await?;
        for l in lines {
            if !seen.insert(&l.insert_id) {
                continue;
            }
            emit(&l);
            if l.timestamp > last_ts {
                last_ts = l.timestamp.clone();
            }
        }
    }
}

pub fn parse_since(s: &str) -> Result<Duration> {
    let d = humantime::parse_duration(s).map_err(|e| {
        Error::config(format!(
            "invalid --since `{s}`: {e} (examples: 10m, 1h, 2h30m, 1d)"
        ))
    })?;
    if d.is_zero() || d > Duration::from_secs(30 * 24 * 3600) {
        return Err(Error::config("--since must be between 1s and 30d"));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use google_cloud_api::model::MonitoredResource;

    fn q() -> LogQuery {
        LogQuery {
            project: "p".into(),
            region: "europe-west1".into(),
            service_id: "hello-dev".into(),
            since: Duration::from_secs(600),
            limit: 100,
            include_requests: false,
            min_severity: None,
        }
    }

    #[test]
    fn filter_targets_service_and_excludes_request_logs() {
        let now = DateTime::parse_from_rfc3339("2025-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let f = service_filter(&q(), now);
        assert!(f.contains(r#"resource.type="cloud_run_revision""#));
        assert!(f.contains(r#"resource.labels.service_name="hello-dev""#));
        assert!(f.contains(r#"resource.labels.location="europe-west1""#));
        assert!(f.contains(r#"timestamp>="2025-01-01T11:50:00.000000Z""#));
        assert!(f.contains("run.googleapis.com%2Frequests"));
        assert!(f.contains(r#"-logName:"projects/p/logs/cloudaudit.googleapis.com""#));

        let mut q2 = q();
        q2.include_requests = true;
        q2.min_severity = Some("warning".into());
        let f = service_filter(&q2, now);
        assert!(!f.contains("%2Frequests"));
        assert!(f.contains("severity>=WARNING"));
    }

    #[test]
    fn formats_text_json_and_request_entries() {
        let res = MonitoredResource::new().set_labels([("revision_name", "hello-dev-00002-abc")]);
        let e = LogEntry::new()
            .set_resource(res.clone())
            .set_text_payload("hello world\n");
        let l = to_line(&e);
        assert_eq!(l.message, "hello world");
        assert_eq!(l.revision, "hello-dev-00002-abc");

        let mut j = serde_json::Map::new();
        j.insert("message".into(), "structured".into());
        j.insert("severity".into(), "INFO".into());
        let e = LogEntry::new().set_json_payload(j);
        assert_eq!(to_line(&e).message, "structured");

        let mut j = serde_json::Map::new();
        j.insert("k".into(), 1.into());
        let e = LogEntry::new().set_json_payload(j);
        assert_eq!(to_line(&e).message, r#"{"k":1}"#);
    }

    #[test]
    fn seen_ids_dedup_and_evict() {
        let mut s = SeenIds::default();
        assert!(s.insert("a"));
        assert!(!s.insert("a"));
        for i in 0..SeenIds::CAPACITY {
            s.insert(&i.to_string());
        }
        assert!(s.insert("a"), "oldest ids are evicted");
        assert_eq!(s.set.len(), SeenIds::CAPACITY);
    }

    #[test]
    fn empty_entries_show_their_log_name() {
        let e =
            LogEntry::new().set_log_name("projects/p/logs/run.googleapis.com%2Fvarlog%2Fsystem");
        assert_eq!(to_line(&e).message, "(run.googleapis.com/varlog/system)");
    }

    #[test]
    fn since_parsing() {
        assert_eq!(parse_since("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_since("1h30m").unwrap(), Duration::from_secs(5400));
        assert!(parse_since("0s").is_err());
        assert!(parse_since("ten").is_err());
        assert!(parse_since("90d").is_err());
    }
}
