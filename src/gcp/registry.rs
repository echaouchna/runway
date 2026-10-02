//! Minimal OCI / Docker registry v2 adapter for resolving tags to digests.
//!
//! No Google SDK exposes the generic registry protocol, and Cloud Run accepts
//! images from Artifact Registry, Container Registry and Docker Hub, so this
//! small REST adapter performs a manifest `HEAD` (falling back to `GET`) and
//! reads `Docker-Content-Digest`. Google registries are authenticated with the
//! ADC access token; other registries use the standard anonymous bearer-token
//! challenge.

use crate::image_ref::{ImageRef, validate_digest};
use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, WWW_AUTHENTICATE};
use sha2::{Digest, Sha256};

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Credentials were rejected or missing permission (HTTP 401/403).
    Unauthorized(String),
    /// Network or unexpected registry response.
    Other(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Unauthorized(m) => write!(f, "registry denied access: {m}"),
            ResolveError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Resolves image references to immutable digests.
#[async_trait]
pub trait DigestResolver: Send + Sync {
    /// `Ok(None)` means the manifest does not exist.
    async fn resolve(&self, image: &ImageRef) -> Result<Option<String>, ResolveError>;

    /// Tags of the image's repository (empty when not supported).
    async fn tags(&self, _image: &ImageRef) -> Result<Vec<String>, ResolveError> {
        Ok(Vec::new())
    }
}

/// Highest `X.Y.Z` tag (pre-releases and non-semver tags ignored).
pub fn latest_semver(tags: &[String]) -> Option<String> {
    tags.iter()
        .filter_map(|t| {
            let p: Vec<u64> = t
                .split('.')
                .map(|x| x.parse().ok())
                .collect::<Option<_>>()?;
            (p.len() == 3).then(|| ((p[0], p[1], p[2]), t.clone()))
        })
        .max()
        .map(|(_, t)| t)
}

pub struct RegistryClient {
    http: reqwest::Client,
    google_token: Option<String>,
    /// Overrides `https://{host}` (tests only).
    base_url: Option<String>,
}

impl RegistryClient {
    pub fn new(http: reqwest::Client, google_token: Option<String>) -> Self {
        Self {
            http,
            google_token,
            base_url: None,
        }
    }

    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = Some(base.into());
        self
    }

    /// The ADC token for this request, only when the URL actually points at a
    /// Google registry host (never derived from the image string alone). A
    /// configured base URL (tests, private endpoints) is trusted explicitly.
    fn google_token_for(&self, image: &ImageRef, url: &str) -> Option<&String> {
        let host_is_google = reqwest::Url::parse(url)
            .ok()
            .filter(|u| u.scheme() == "https" && u.port().is_none())
            .and_then(|u| u.host_str().map(crate::image_ref::is_google_registry_host))
            .unwrap_or(false);
        let trusted = self.base_url.as_deref().is_some_and(|b| url.starts_with(b));
        (image.is_google_registry() && (host_is_google || trusted))
            .then_some(self.google_token.as_ref())
            .flatten()
    }

    fn manifest_url(&self, image: &ImageRef) -> String {
        let base = self
            .base_url
            .clone()
            .unwrap_or_else(|| format!("https://{}", image.api_host()));
        format!(
            "{base}/v2/{}/manifests/{}",
            image.repository,
            image.manifest_reference()
        )
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        auth: Option<&str>,
    ) -> Result<reqwest::Response, ResolveError> {
        let mut req = self
            .http
            .request(method, url)
            .header(ACCEPT, MANIFEST_ACCEPT);
        if let Some(a) = auth {
            req = req.header(AUTHORIZATION, a);
        }
        req.send()
            .await
            .map_err(|e| ResolveError::Other(format!("cannot reach registry: {e}")))
    }

    /// Handles a `WWW-Authenticate: Bearer realm=...` challenge.
    async fn challenge_token(
        &self,
        headers: &HeaderMap,
        image: &ImageRef,
    ) -> Result<Option<String>, ResolveError> {
        let Some(h) = headers.get(WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()) else {
            return Ok(None);
        };
        let Some(params) = h
            .strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
        else {
            return Ok(None);
        };
        let params = parse_challenge(params);
        let Some(realm) = params
            .iter()
            .find(|(k, _)| k == "realm")
            .map(|(_, v)| v.clone())
        else {
            return Ok(None);
        };
        let mut query: Vec<(String, String)> = params
            .into_iter()
            .filter(|(k, _)| k == "service" || k == "scope")
            .collect();
        if !query.iter().any(|(k, _)| k == "scope") {
            query.push((
                "scope".into(),
                format!("repository:{}:pull", image.repository),
            ));
        }
        let mut req = self.http.get(&realm).query(&query);
        if let Some(t) = self.google_token_for(image, &realm) {
            req = req.basic_auth("oauth2accesstoken", Some(t));
        }
        let resp = req.send().await.map_err(|e| {
            ResolveError::Other(format!("cannot reach registry token endpoint: {e}"))
        })?;
        if !resp.status().is_success() {
            return Err(ResolveError::Unauthorized(format!(
                "token endpoint returned HTTP {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ResolveError::Other(format!("invalid token response: {e}")))?;
        Ok(body
            .get("token")
            .or_else(|| body.get("access_token"))
            .and_then(|v| v.as_str())
            .map(String::from))
    }
}

fn parse_challenge(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_string();
        rest = &rest[eq + 1..];
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            value = r[..end].to_string();
            rest = r.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        rest = rest.trim_start_matches(',').trim_start();
        out.push((key, value));
    }
    out
}

fn digest_header(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|d| validate_digest(d).is_ok())
}

#[async_trait]
impl DigestResolver for RegistryClient {
    async fn tags(&self, image: &ImageRef) -> Result<Vec<String>, ResolveError> {
        let base = self
            .base_url
            .clone()
            .unwrap_or_else(|| format!("https://{}", image.api_host()));
        let url = format!("{base}/v2/{}/tags/list?n=10000", image.repository);
        let mut req = self.http.get(&url);
        if let Some(t) = self.google_token_for(image, &url) {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ResolveError::Other(format!("cannot reach registry: {e}")))?;
        match resp.status() {
            s if s.is_success() => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(ResolveError::Unauthorized(format!(
                    "HTTP {} listing tags",
                    resp.status()
                )));
            }
            s => {
                return Err(ResolveError::Other(format!(
                    "registry returned HTTP {s} listing tags"
                )));
            }
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ResolveError::Other(format!("invalid tag list: {e}")))?;
        Ok(body["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn resolve(&self, image: &ImageRef) -> Result<Option<String>, ResolveError> {
        if let Some(d) = &image.digest {
            return Ok(Some(d.clone()));
        }
        let url = self.manifest_url(image);
        let mut auth = self
            .google_token_for(image, &url)
            .map(|t| format!("Bearer {t}"));
        let mut resp = self
            .request(reqwest::Method::HEAD, &url, auth.as_deref())
            .await?;
        if resp.status() == StatusCode::UNAUTHORIZED
            && let Some(token) = self.challenge_token(resp.headers(), image).await?
        {
            auth = Some(format!("Bearer {token}"));
            resp = self
                .request(reqwest::Method::HEAD, &url, auth.as_deref())
                .await?;
        }
        match resp.status() {
            StatusCode::NOT_FOUND => return Ok(None),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(ResolveError::Unauthorized(format!(
                    "HTTP {} for {}",
                    resp.status(),
                    image.name()
                )));
            }
            s if !s.is_success() => {
                return Err(ResolveError::Other(format!(
                    "registry returned HTTP {s} for {}",
                    image.name()
                )));
            }
            _ => {}
        }
        if let Some(d) = digest_header(&resp) {
            return Ok(Some(d));
        }
        // Some registries omit the header on HEAD: fetch and hash the manifest.
        let resp = self
            .request(reqwest::Method::GET, &url, auth.as_deref())
            .await?;
        if !resp.status().is_success() {
            return Err(ResolveError::Other(format!(
                "registry returned HTTP {} for {}",
                resp.status(),
                image.name()
            )));
        }
        if let Some(d) = digest_header(&resp) {
            return Ok(Some(d));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| ResolveError::Other(format!("cannot read manifest: {e}")))?;
        Ok(Some(format!(
            "sha256:{}",
            crate::build::package::hex(&Sha256::digest(&body))
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    #[tokio::test]
    async fn resolves_google_registry_with_adc_token() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/v2/p/apps/hello/manifests/v1"))
            .and(header("authorization", "Bearer ya29.token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Docker-Content-Digest", digest('a').as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let c = RegistryClient::new(reqwest::Client::new(), Some("ya29.token".into()))
            .with_base_url(server.uri());
        let img = ImageRef::parse("europe-west1-docker.pkg.dev/p/apps/hello:v1").unwrap();
        assert_eq!(c.resolve(&img).await.unwrap(), Some(digest('a')));
    }

    #[tokio::test]
    async fn missing_manifest_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let c = RegistryClient::new(reqwest::Client::new(), Some("t".into()))
            .with_base_url(server.uri());
        let img = ImageRef::parse("europe-west1-docker.pkg.dev/p/apps/hello:src-1").unwrap();
        assert_eq!(c.resolve(&img).await.unwrap(), None);
    }

    #[tokio::test]
    async fn follows_anonymous_bearer_challenge() {
        let server = MockServer::start().await;
        let realm = format!("{}/token", server.uri());
        Mock::given(method("HEAD"))
            .and(path("/v2/library/nginx/manifests/1.27"))
            .and(header("authorization", "Bearer anon"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Docker-Content-Digest", digest('b').as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/v2/library/nginx/manifests/1.27"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(r#"Bearer realm="{realm}",service="registry.docker.io",scope="repository:library/nginx:pull""#).as_str(),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .and(query_param("service", "registry.docker.io"))
            .and(query_param("scope", "repository:library/nginx:pull"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token": "anon"})),
            )
            .mount(&server)
            .await;
        let c = RegistryClient::new(reqwest::Client::new(), None).with_base_url(server.uri());
        let img = ImageRef::parse("nginx:1.27").unwrap();
        assert_eq!(c.resolve(&img).await.unwrap(), Some(digest('b')));
    }

    #[tokio::test]
    async fn falls_back_to_hashing_manifest_body() {
        let server = MockServer::start().await;
        let body = br#"{"schemaVersion":2}"#;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(&server)
            .await;
        let c = RegistryClient::new(reqwest::Client::new(), None).with_base_url(server.uri());
        let img = ImageRef::parse("ghcr.io/acme/app:1").unwrap();
        let expected = format!(
            "sha256:{}",
            crate::build::package::hex(&Sha256::digest(body))
        );
        assert_eq!(c.resolve(&img).await.unwrap(), Some(expected));
    }

    #[tokio::test]
    async fn forbidden_is_unauthorized_and_digest_refs_skip_network() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let c = RegistryClient::new(reqwest::Client::new(), Some("t".into()))
            .with_base_url(server.uri());
        let img = ImageRef::parse("gcr.io/p/app:1").unwrap();
        assert!(matches!(
            c.resolve(&img).await,
            Err(ResolveError::Unauthorized(_))
        ));

        let pinned = ImageRef::parse(&format!("gcr.io/p/app@{}", digest('c'))).unwrap();
        assert_eq!(c.resolve(&pinned).await.unwrap(), Some(digest('c')));
    }

    #[test]
    fn google_token_only_goes_to_google_hosts() {
        let c = RegistryClient::new(reqwest::Client::new(), Some("secret".into()));
        let img = ImageRef::parse("europe-west1-docker.pkg.dev/p/r/i:1").unwrap();
        let ok = "https://europe-west1-docker.pkg.dev/v2/p/r/i/manifests/1";
        assert_eq!(
            c.google_token_for(&img, ok).map(String::as_str),
            Some("secret")
        );
        // A token endpoint (realm) elsewhere, plain HTTP or another port: no token.
        for url in [
            "https://evil.example/token",
            "http://europe-west1-docker.pkg.dev/v2/x",
            "https://europe-west1-docker.pkg.dev:8443/v2/x",
            "https://europe-west1-docker.pkg.dev.evil.example/v2/x",
        ] {
            assert!(c.google_token_for(&img, url).is_none(), "{url}");
        }
        // Non-Google images never get the token.
        let hub = ImageRef::parse("nginx:1").unwrap();
        assert!(
            c.google_token_for(&hub, "https://registry-1.docker.io/v2/x")
                .is_none()
        );
    }

    #[test]
    fn picks_latest_semver() {
        let tags: Vec<String> = [
            "0.144.0",
            "0.160.0",
            "0.156.1",
            "latest",
            "0.161.0-rc1",
            "1.0",
            "0.99.10",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(latest_semver(&tags).as_deref(), Some("0.160.0"));
        assert_eq!(latest_semver(&[]), None);
    }

    #[tokio::test]
    async fn lists_tags_with_google_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/cloud-ops-agents-artifacts/google-cloud-opentelemetry-collector/otelcol-google/tags/list"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"tags": ["0.159.0", "0.160.0", "latest"]})))
            .mount(&server)
            .await;
        let c = RegistryClient::new(reqwest::Client::new(), Some("tok".into()))
            .with_base_url(server.uri());
        let img = ImageRef::parse("us-docker.pkg.dev/cloud-ops-agents-artifacts/google-cloud-opentelemetry-collector/otelcol-google:0.160.0").unwrap();
        let tags = c.tags(&img).await.unwrap();
        assert_eq!(latest_semver(&tags).as_deref(), Some("0.160.0"));
    }

    #[test]
    fn parses_challenges() {
        let p = parse_challenge(
            r#"realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
        );
        assert_eq!(
            p[0],
            ("realm".into(), "https://auth.docker.io/token".into())
        );
        assert_eq!(p[2].1, "repository:library/nginx:pull");
    }
}
