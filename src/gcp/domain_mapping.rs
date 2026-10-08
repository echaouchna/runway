//! Cloud Run domain mappings, including `NAME.cloud.run` custom URLs.
//!
//! A small REST adapter: domain mappings are only in the Cloud Run Admin API
//! v1 (`domains.cloudrun.com/v1`), which the Rust SDK (v2) does not cover.
//! Requests go to the regional endpoint with the session's access token.

use crate::error::{Error, ErrorKind, Result};
use crate::naming;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct DomainMappings<'a> {
    pub http: &'a reqwest::Client,
    pub token: String,
    pub project: String,
    pub region: String,
    /// Overrides `https://{region}-run.googleapis.com` (tests).
    pub base_url: Option<String>,
}

/// A DNS record a mapping needs (from its status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingRecord {
    pub kind: String,
    pub data: String,
}

/// A live mapping, as runway reads it.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub domain: String,
    pub service: String,
    pub owned: bool,
    pub ready: bool,
    pub message: String,
    pub records: Vec<MappingRecord>,
}

impl DomainMappings<'_> {
    fn base(&self) -> String {
        let host = self
            .base_url
            .clone()
            .unwrap_or_else(|| format!("https://{}-run.googleapis.com", self.region));
        format!(
            "{host}/apis/domains.cloudrun.com/v1/namespaces/{}/domainmappings",
            self.project
        )
    }

    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<Option<Value>> {
        let resp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Error::internal(format!("{what}: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(Some(body));
        }
        let message = body["error"]["message"].as_str().unwrap_or("").to_string();
        let kind = match status.as_u16() {
            401 | 403 => ErrorKind::Prerequisite,
            409 => ErrorKind::Conflict,
            _ => ErrorKind::Deploy,
        };
        let mut err = Error::new(kind, format!("{what}: HTTP {status}: {message}"));
        if message.contains("verif") {
            err = err.hint(
                "verify the domain first (https://search.google.com/search-console), with the deploying account as an owner",
            );
        }
        Err(err)
    }

    fn parse(v: &Value, app: &str, stage: &str) -> Mapping {
        let labels = &v["metadata"]["labels"];
        let owned = labels[naming::LABEL_MANAGED_BY] == naming::LABEL_MANAGED_BY_VALUE
            && labels[naming::LABEL_APP] == app
            && labels[naming::LABEL_STAGE] == stage;
        let ready_cond = v["status"]["conditions"]
            .as_array()
            .and_then(|c| c.iter().find(|x| x["type"] == "Ready"));
        Mapping {
            domain: v["metadata"]["name"].as_str().unwrap_or("").to_string(),
            service: v["spec"]["routeName"].as_str().unwrap_or("").to_string(),
            owned,
            ready: ready_cond.is_some_and(|c| c["status"] == "True"),
            message: ready_cond
                .and_then(|c| c["message"].as_str())
                .unwrap_or("")
                .to_string(),
            records: v["status"]["resourceRecords"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| MappingRecord {
                            kind: r["type"].as_str().unwrap_or("").to_string(),
                            data: r["rrdata"].as_str().unwrap_or("").to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub async fn get(&self, domain: &str, app: &str, stage: &str) -> Result<Option<Mapping>> {
        let url = format!("{}/{domain}", self.base());
        Ok(self
            .send(
                self.http.get(&url),
                &format!("reading domain mapping {domain}"),
            )
            .await?
            .map(|v| Self::parse(&v, app, stage)))
    }

    /// Mappings of the project in this region.
    pub async fn list(&self, app: &str, stage: &str) -> Result<Vec<Mapping>> {
        let v = self
            .send(self.http.get(self.base()), "listing domain mappings")
            .await?
            .unwrap_or(Value::Null);
        Ok(v["items"]
            .as_array()
            .map(|a| a.iter().map(|m| Self::parse(m, app, stage)).collect())
            .unwrap_or_default())
    }

    pub async fn create(&self, domain: &str, service: &str, app: &str, stage: &str) -> Result<()> {
        let body = json!({
            "apiVersion": "domains.cloudrun.com/v1",
            "kind": "DomainMapping",
            "metadata": {
                "name": domain,
                "namespace": self.project,
                "labels": naming::ownership_labels(app, stage),
                "annotations": {"run.googleapis.com/launch-stage": "BETA"}
            },
            "spec": {"routeName": service, "certificateMode": "AUTOMATIC"}
        });
        self.send(
            self.http.post(self.base()).json(&body),
            &format!("creating domain mapping {domain}"),
        )
        .await
        .map(|_| ())
    }

    pub async fn delete(&self, domain: &str) -> Result<bool> {
        let url = format!("{}/{domain}", self.base());
        Ok(self
            .send(
                self.http.delete(&url),
                &format!("deleting domain mapping {domain}"),
            )
            .await?
            .is_some())
    }
}
