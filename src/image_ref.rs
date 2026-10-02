//! Container image reference parsing (`[registry/]repository[:tag][@digest]`).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Registry host, e.g. `europe-west1-docker.pkg.dev` or `docker.io`.
    pub registry: String,
    /// Repository path within the registry, e.g. `my-project/apps/hello`.
    pub repository: String,
    pub tag: Option<String>,
    /// Digest in `sha256:<64 hex>` form.
    pub digest: Option<String>,
}

impl ImageRef {
    pub fn parse(input: &str) -> Result<Self, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err("image reference is empty".into());
        }
        if s != input || s.chars().any(char::is_whitespace) {
            return Err("image reference must not contain whitespace".into());
        }
        if s.contains("://") {
            return Err("image reference must not include a URL scheme (remove `https://`)".into());
        }
        let (name_tag, digest) = match s.split_once('@') {
            Some((n, d)) => {
                validate_digest(d)?;
                (n, Some(d.to_string()))
            }
            None => (s, None),
        };
        // A tag is after the last ':' that follows the last '/'.
        let last_slash = name_tag.rfind('/');
        let (name, tag) = match name_tag.rfind(':') {
            Some(i) if last_slash.is_none_or(|sl| i > sl) => {
                (&name_tag[..i], Some(name_tag[i + 1..].to_string()))
            }
            _ => (name_tag, None),
        };
        if let Some(t) = &tag {
            validate_tag(t)?;
        }
        let mut parts: Vec<&str> = name.split('/').collect();
        let first = parts[0];
        let registry = if parts.len() > 1
            && (first.contains('.') || first.contains(':') || first == "localhost")
        {
            validate_registry(first).map_err(|e| format!("`{input}`: {e}"))?;
            parts.remove(0);
            first.to_ascii_lowercase()
        } else {
            "docker.io".to_string()
        };
        if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
            return Err(format!("`{input}` has an empty repository path component"));
        }
        for p in &parts {
            validate_path_component(p).map_err(|e| format!("`{input}`: {e}"))?;
        }
        let mut repository = parts.join("/");
        if registry == "docker.io" && parts.len() == 1 {
            repository = format!("library/{repository}");
        }
        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// Host to contact for the registry v2 API.
    pub fn api_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    /// The reference used in manifest requests: digest if present, else tag (default `latest`).
    pub fn manifest_reference(&self) -> &str {
        self.digest
            .as_deref()
            .or(self.tag.as_deref())
            .unwrap_or("latest")
    }

    /// `registry/repository` without tag or digest.
    pub fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// Immutable `registry/repository@digest` form.
    pub fn pinned(&self, digest: &str) -> String {
        format!("{}@{}", self.name(), digest)
    }

    pub fn is_google_registry(&self) -> bool {
        is_google_registry_host(&self.registry)
    }

    /// True if the reference relies on a mutable tag (no digest).
    pub fn is_mutable(&self) -> bool {
        self.digest.is_none()
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// True for Artifact Registry and Container Registry hosts (exact host, no
/// port). Credentials are only ever sent to these hosts.
pub fn is_google_registry_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    validate_registry(&h).is_ok()
        && !h.contains(':')
        && (h == "docker.pkg.dev"
            || h.ends_with("-docker.pkg.dev")
            || h == "gcr.io"
            || h.ends_with(".gcr.io"))
}

/// A registry is `host[:port]`: DNS labels (letters, digits, `-`) separated
/// by dots, or `localhost`, with an optional numeric port.
pub fn validate_registry(r: &str) -> Result<(), String> {
    let (host, port) = match r.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (r, None),
    };
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    let host_ok = !host.is_empty() && host.len() <= 253 && host.split('.').all(label_ok);
    let port_ok =
        port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.parse::<u16>().is_ok_and(|n| n > 0));
    if host_ok && port_ok {
        Ok(())
    } else {
        Err(format!("invalid registry host `{r}`"))
    }
}

pub fn validate_digest(d: &str) -> Result<(), String> {
    match d.strip_prefix("sha256:") {
        Some(hex)
            if hex.len() == 64
                && hex
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) =>
        {
            Ok(())
        }
        _ => Err(format!(
            "invalid digest `{d}`; expected `sha256:` followed by 64 lowercase hex characters"
        )),
    }
}

fn validate_tag(t: &str) -> Result<(), String> {
    let ok = !t.is_empty()
        && t.len() <= 128
        && t.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!("invalid image tag `{t}`"))
    }
}

fn validate_path_component(p: &str) -> Result<(), String> {
    let ok = p
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        && p.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && p.chars().last().is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid repository component `{p}` (use lowercase letters, digits, `.`, `_`, `-`)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_registry_hosts() {
        for bad in [
            "example.com?-docker.pkg.dev/demo:latest",
            "example.com#-docker.pkg.dev/demo",
            "evil.com%2f-docker.pkg.dev/demo",
            "user@evil.com/x-docker.pkg.dev/demo",
            "-bad.example.com/demo",
            "example.com:99999/demo",
            "exa_mple.com/demo",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad} must be rejected");
        }
        let r = ImageRef::parse("Europe-West1-Docker.pkg.dev/p/r/i:1").unwrap();
        assert_eq!(r.registry, "europe-west1-docker.pkg.dev");
        assert!(r.is_google_registry());
        assert!(ImageRef::parse("localhost:5000/app:1").is_ok());
        assert!(!is_google_registry_host("example.com"));
        assert!(!is_google_registry_host("gcr.io.evil.com"));
        assert!(!is_google_registry_host("europe-west1-docker.pkg.dev:8443"));
        assert!(is_google_registry_host("eu.gcr.io"));
    }

    #[test]
    fn parses_artifact_registry_ref() {
        let r = ImageRef::parse("europe-west1-docker.pkg.dev/p/apps/hello:v1").unwrap();
        assert_eq!(r.registry, "europe-west1-docker.pkg.dev");
        assert_eq!(r.repository, "p/apps/hello");
        assert_eq!(r.tag.as_deref(), Some("v1"));
        assert!(r.is_google_registry());
        assert_eq!(r.manifest_reference(), "v1");
    }

    #[test]
    fn parses_docker_hub_short_names() {
        let r = ImageRef::parse("nginx").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.api_host(), "registry-1.docker.io");
        assert_eq!(r.manifest_reference(), "latest");
        assert!(!r.is_google_registry());
    }

    #[test]
    fn parses_registry_with_port_and_digest() {
        let d = format!("sha256:{}", "a".repeat(64));
        let r = ImageRef::parse(&format!("localhost:5000/team/app:1.0@{d}")).unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.tag.as_deref(), Some("1.0"));
        assert_eq!(r.digest.as_deref(), Some(d.as_str()));
        assert_eq!(r.manifest_reference(), d);
        assert_eq!(r.pinned(&d), format!("localhost:5000/team/app@{d}"));
    }

    #[test]
    fn rejects_bad_refs() {
        for bad in [
            "",
            "Upper/Case",
            "https://gcr.io/x",
            "gcr.io/x@sha256:short",
            "gcr.io//x",
            "gcr.io/x:bad tag",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad} should be rejected");
        }
    }
}
