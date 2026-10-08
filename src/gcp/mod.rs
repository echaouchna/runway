//! Google Cloud clients and shared helpers.
//!
//! All clients share one set of Application Default Credentials (ADC), so a
//! single token cache serves every API. ADC covers `gcloud auth
//! application-default login`, service account keys referenced by
//! `GOOGLE_APPLICATION_CREDENTIALS`, metadata-server credentials, and external
//! account (Workload Identity Federation) configurations used in CI.

pub mod bucket;
pub mod domain_mapping;
pub mod iam;
pub mod jobs;
pub mod logging;
pub mod registry;
pub mod run;
pub mod scheduler;

use crate::error::{Error, ErrorKind};
use google_cloud_auth::credentials::{AccessTokenCredentials, Builder as CredBuilder, Credentials};
use google_cloud_gax::error::rpc::{Code, StatusDetails};
use google_cloud_gax::retry_policy::{Aip194Strict, RetryPolicyExt};
use std::time::Duration;

pub type GaxError = google_cloud_gax::error::Error;

pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Hint shown whenever credentials are missing or rejected.
pub const ADC_HINT: &str = "authenticate with `gcloud auth application-default login` (local), set GOOGLE_APPLICATION_CREDENTIALS, or configure Workload Identity Federation in CI (for example google-github-actions/auth)";

/// Service account impersonation: the caller's ADC obtains short-lived tokens
/// for `target` (optionally through a chain of `delegates`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impersonation {
    pub target: String,
    pub delegates: Vec<String>,
}

impl Impersonation {
    /// `SA` or `DELEGATE1,...,SA` (gcloud's syntax: the last account is the target).
    pub fn parse(spec: &str) -> Result<Self, String> {
        let accounts: Vec<String> = spec
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let Some((target, delegates)) = accounts.split_last() else {
            return Err("impersonation needs a service account email".into());
        };
        for a in &accounts {
            crate::config::validate::service_account_email(a)?;
        }
        Ok(Self {
            target: target.clone(),
            delegates: delegates.to_vec(),
        })
    }

    pub fn hint(&self) -> String {
        format!(
            "impersonating {}: the caller needs roles/iam.serviceAccountTokenCreator on it{}, and iamcredentials.googleapis.com must be enabled",
            self.target,
            if self.delegates.is_empty() {
                String::new()
            } else {
                " (through each delegate in the chain)".into()
            }
        )
    }
}

/// Loads Application Default Credentials.
pub fn credentials() -> Result<AccessTokenCredentials, Error> {
    CredBuilder::default()
        .with_scopes([CLOUD_PLATFORM_SCOPE])
        .build_access_token_credentials()
        .map_err(|e| {
            Error::prerequisite(format!("cannot load Google Cloud credentials: {e}")).hint(ADC_HINT)
        })
}

/// Obtains an access token, mapping failures to actionable errors.
pub async fn access_token(creds: &AccessTokenCredentials) -> Result<String, Error> {
    creds.access_token().await.map(|t| t.token).map_err(|e| {
        Error::prerequisite(format!("cannot obtain an access token: {e}")).hint(ADC_HINT)
    })
}

/// Retry policy for idempotent calls: transient errors, bounded in time and attempts.
pub fn retry_policy() -> impl google_cloud_gax::retry_policy::RetryPolicy {
    Aip194Strict
        .with_time_limit(Duration::from_secs(60))
        .with_attempt_limit(5)
}

/// Shared credentials plus a plain HTTP client for the registry adapter.
#[derive(Clone)]
pub struct Session {
    pub token_credentials: AccessTokenCredentials,
    pub credentials: Credentials,
    pub http: reqwest::Client,
    /// Set when the session impersonates a service account.
    pub impersonating: Option<Impersonation>,
}

impl Session {
    pub fn from_adc() -> Result<Self, Error> {
        Self::from_token_credentials(credentials()?)
    }

    /// ADC, optionally impersonating a service account.
    pub fn connect(impersonate: Option<&Impersonation>) -> Result<Self, Error> {
        let Some(imp) = impersonate else {
            return Self::from_adc();
        };
        let source = CredBuilder::default()
            .with_scopes([CLOUD_PLATFORM_SCOPE])
            .build()
            .map_err(|e| {
                Error::prerequisite(format!("cannot load Google Cloud credentials: {e}"))
                    .hint(ADC_HINT)
            })?;
        let creds =
            google_cloud_auth::credentials::impersonated::Builder::from_source_credentials(source)
                .with_target_principal(&imp.target)
                .with_delegates(imp.delegates.clone())
                .with_scopes([CLOUD_PLATFORM_SCOPE])
                .build_access_token_credentials()
                .map_err(|e| {
                    Error::prerequisite(format!(
                        "cannot set up impersonation of {}: {e}",
                        imp.target
                    ))
                    .hint(imp.hint())
                })?;
        let mut s = Self::from_token_credentials(creds)?;
        s.impersonating = Some(imp.clone());
        Ok(s)
    }

    /// Fetches a token now so that credential problems are reported up front.
    pub async fn verify(&self) -> Result<(), Error> {
        match self.token().await {
            Ok(_) => Ok(()),
            Err(mut e) => {
                if let Some(imp) = &self.impersonating {
                    e.message = format!("{} (impersonating {})", e.message, imp.target);
                    e.hints.insert(0, imp.hint());
                }
                Err(e)
            }
        }
    }

    /// A session that always presents `token` (tests and mock servers).
    pub fn from_static_token(token: impl Into<String>) -> Result<Self, Error> {
        Self::from_token_credentials(AccessTokenCredentials::from(StaticToken(token.into())))
    }

    fn from_token_credentials(token_credentials: AccessTokenCredentials) -> Result<Self, Error> {
        let credentials = Credentials::from(token_credentials.clone());
        let http = reqwest::Client::builder()
            .user_agent(concat!("runway/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| Error::internal(format!("cannot create HTTP client: {e}")))?;
        Ok(Self {
            token_credentials,
            credentials,
            http,
            impersonating: None,
        })
    }

    pub async fn token(&self) -> Result<String, Error> {
        access_token(&self.token_credentials).await
    }
}

/// Credentials that always return the same bearer token.
#[derive(Debug, Clone)]
pub struct StaticToken(String);

impl google_cloud_auth::credentials::CredentialsProvider for StaticToken {
    async fn headers(
        &self,
        _extensions: http::Extensions,
    ) -> Result<
        google_cloud_auth::credentials::CacheableResource<http::HeaderMap>,
        google_cloud_auth::errors::CredentialsError,
    > {
        let mut h = http::HeaderMap::new();
        let v = http::HeaderValue::from_str(&format!("Bearer {}", self.0)).map_err(|e| {
            google_cloud_auth::errors::CredentialsError::from_msg(false, e.to_string())
        })?;
        h.insert(http::header::AUTHORIZATION, v);
        Ok(google_cloud_auth::credentials::CacheableResource::New {
            entity_tag: google_cloud_auth::credentials::EntityTag::new(),
            data: h,
        })
    }

    async fn universe_domain(&self) -> Option<String> {
        None
    }
}

impl google_cloud_auth::credentials::AccessTokenCredentialsProvider for StaticToken {
    async fn access_token(
        &self,
    ) -> Result<
        google_cloud_auth::credentials::AccessToken,
        google_cloud_auth::errors::CredentialsError,
    > {
        Ok(google_cloud_auth::credentials::AccessToken {
            token: self.0.clone(),
        })
    }
}

/// Builds an SDK client with shared credentials and runway's retry policy.
#[macro_export]
macro_rules! build_client {
    ($client:ty, $session:expr) => {{
        <$client>::builder()
            .with_credentials($session.credentials.clone())
            .with_retry_policy($crate::gcp::retry_policy())
            .build()
            .await
            .map_err(|e| {
                $crate::error::Error::internal(format!(
                    "cannot create {} client: {e}",
                    stringify!($client)
                ))
            })
    }};
}

/// Like [`build_client!`], with an optional endpoint override.
#[macro_export]
macro_rules! build_client_at {
    ($client:ty, $session:expr, $endpoint:expr) => {{
        let mut b = <$client>::builder()
            .with_credentials($session.credentials.clone())
            .with_retry_policy($crate::gcp::retry_policy());
        if let Some(ep) = $endpoint {
            b = b.with_endpoint(ep);
        }
        b.build().await.map_err(|e| {
            $crate::error::Error::internal(format!(
                "cannot create {} client: {e}",
                stringify!($client)
            ))
        })
    }};
}

pub fn status_code(e: &GaxError) -> Option<Code> {
    e.status().map(|s| s.code)
}

pub fn is_not_found(e: &GaxError) -> bool {
    status_code(e) == Some(Code::NotFound) || e.http_status_code() == Some(404)
}

/// The API serving the request is not enabled on the project.
pub fn is_service_disabled(e: &GaxError) -> bool {
    error_reason(e).is_some_and(|(r, _)| r == "SERVICE_DISABLED" || r == "API_DISABLED")
}

/// The request may or may not have been applied by the server.
pub fn is_ambiguous(e: &GaxError) -> bool {
    if e.is_timeout() || e.is_io() || e.is_transport() || e.is_exhausted() {
        return true;
    }
    matches!(
        status_code(e),
        Some(Code::Unavailable | Code::DeadlineExceeded | Code::Internal | Code::Unknown)
    ) || e.http_status_code().is_some_and(|c| c >= 500)
}

/// Optimistic-concurrency conflicts (stale etag) that can be retried after a fresh read.
pub fn is_concurrency_conflict(e: &GaxError) -> bool {
    matches!(status_code(e), Some(Code::Aborted))
        || e.http_status_code() == Some(409)
        || (status_code(e) == Some(Code::FailedPrecondition)
            && e.status()
                .is_some_and(|s| s.message.to_ascii_lowercase().contains("etag")))
}

fn error_reason(e: &GaxError) -> Option<(String, Option<String>)> {
    e.status()?.details.iter().find_map(|d| match d {
        StatusDetails::ErrorInfo(info) => Some((
            info.reason.clone(),
            info.metadata
                .get("permission")
                .or_else(|| info.metadata.get("service"))
                .cloned(),
        )),
        _ => None,
    })
}

/// Converts an SDK error into a runway error with actionable hints.
///
/// `action` describes what runway was doing, e.g. "reading Cloud Run service x".
pub fn api_error(e: GaxError, action: &str) -> Error {
    let status = e.status().cloned();
    let message = status
        .as_ref()
        .map(|s| s.message.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| e.to_string());
    let reason = error_reason(&e);
    let code = status.as_ref().map(|s| s.code);

    if e.is_authentication() || code == Some(Code::Unauthenticated) {
        return Error::new(
            ErrorKind::Prerequisite,
            format!("authentication failed while {action}: {message}"),
        )
        .hint(ADC_HINT);
    }
    if let Some((r, extra)) = &reason
        && (r == "SERVICE_DISABLED" || r == "API_DISABLED")
    {
        let api = extra.clone().unwrap_or_else(|| "the required API".into());
        return Error::new(
            ErrorKind::Prerequisite,
            format!("{api} is not enabled while {action}: {message}"),
        )
        .hint(format!(
            "enable it with `gcloud services enable {api}` and retry"
        ));
    }
    // Organization policy violations never clear by waiting.
    if let Some(i) = message.find("constraints/") {
        let constraint: String = message[i..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_'))
            .collect();
        let mut err = Error::new(
            ErrorKind::Deploy,
            format!("an organization policy blocks this while {action}: {message}"),
        )
        .permanent();
        if constraint == "constraints/run.allowedIngress" {
            err = err.hint("set `service.ingress` to a value the policy allows (usually `internal` or `internal-and-cloud-load-balancing`)");
        }
        return err
            .hint(format!(
                "inspect the policy: gcloud org-policies describe {} --project <project> --effective",
                constraint.trim_start_matches("constraints/")
            ))
            .with_source(e);
    }
    match code {
        Some(Code::PermissionDenied) => {
            let mut err = Error::new(
                ErrorKind::Prerequisite,
                format!("permission denied while {action}: {message}"),
            );
            if let Some((_, Some(perm))) = &reason {
                err = err.hint(format!("the deploying principal needs `{perm}`"));
            }
            err.hint(format!(
                "see {}/permissions/ and run `runway doctor` for a full prerequisite check",
                crate::DOCS_URL
            ))
        }
        Some(Code::NotFound) => Error::new(
            ErrorKind::NotFound,
            format!("not found while {action}: {message}"),
        ),
        Some(Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange) => Error::new(
            ErrorKind::Deploy,
            format!("Google Cloud rejected the request while {action}: {message}"),
        ),
        Some(Code::ResourceExhausted) => Error::new(
            ErrorKind::Deploy,
            format!("quota exhausted while {action}: {message}"),
        )
        .hint("check quotas in the Cloud console or retry later"),
        _ => Error::new(ErrorKind::Internal, format!("{action} failed: {message}")),
    }
    .with_source(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impersonation_specs() {
        let i = Impersonation::parse("deployer@p.iam.gserviceaccount.com").unwrap();
        assert_eq!(i.target, "deployer@p.iam.gserviceaccount.com");
        assert!(i.delegates.is_empty());
        let i = Impersonation::parse(
            "hop@p.iam.gserviceaccount.com, deployer@p.iam.gserviceaccount.com",
        )
        .unwrap();
        assert_eq!(i.target, "deployer@p.iam.gserviceaccount.com");
        assert_eq!(i.delegates, ["hop@p.iam.gserviceaccount.com"]);
        assert!(Impersonation::parse("").is_err());
        assert!(Impersonation::parse("me@gmail.com").is_err());
        assert!(i.hint().contains("roles/iam.serviceAccountTokenCreator"));
    }
    use google_cloud_gax::error::rpc::Status;

    fn svc_err(code: Code, msg: &str) -> GaxError {
        GaxError::service(Status::default().set_code(code).set_message(msg))
    }

    #[test]
    fn classifies_errors() {
        assert!(is_not_found(&svc_err(Code::NotFound, "x")));
        assert!(is_ambiguous(&svc_err(Code::Unavailable, "x")));
        assert!(is_ambiguous(&GaxError::timeout("slow")));
        assert!(!is_ambiguous(&svc_err(Code::InvalidArgument, "x")));
        assert!(is_concurrency_conflict(&svc_err(Code::Aborted, "x")));
        assert!(is_concurrency_conflict(&svc_err(
            Code::FailedPrecondition,
            "etag mismatch"
        )));

        let e = api_error(svc_err(Code::PermissionDenied, "denied"), "reading service");
        assert_eq!(e.kind, ErrorKind::Prerequisite);
        assert!(
            e.message
                .contains("permission denied while reading service")
        );
        assert!(e.hints.iter().any(|h| h.contains("runway doctor")));

        let e = api_error(
            svc_err(Code::InvalidArgument, "bad cpu"),
            "updating service",
        );
        assert_eq!(e.kind, ErrorKind::Deploy);
        assert!(e.message.contains("bad cpu"));

        let e = api_error(
            svc_err(
                Code::FailedPrecondition,
                "Constraint constraints/run.allowedIngress violated for attempting CreateService with annotation \"run.googleapis.com/ingress\" set to the default value all.",
            ),
            "creating Cloud Run service gcptree-prod",
        );
        assert!(e.permanent, "org policy violations are not retried");
        assert!(!crate::retry::is_retryable(&e));
        assert!(
            e.hints.iter().any(|h| h.contains("service.ingress")),
            "{:?}",
            e.hints
        );
        assert!(
            e.hints
                .iter()
                .any(|h| h.contains("org-policies describe run.allowedIngress")),
            "{:?}",
            e.hints
        );

        let e = api_error(svc_err(Code::Unauthenticated, "no"), "x");
        assert!(
            e.hints
                .iter()
                .any(|h| h.contains("application-default login"))
        );
    }
}
