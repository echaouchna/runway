//! Field-level validation rules. Each function returns a human-readable
//! explanation on failure; callers attach the YAML path.

/// Cloud Run sets these itself and rejects them as user environment variables.
pub const RESERVED_ENV: &[&str] = &["PORT", "K_SERVICE", "K_REVISION", "K_CONFIGURATION"];

const SENSITIVE_MARKERS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "API_KEY",
    "APIKEY",
    "PRIVATE_KEY",
    "CREDENTIAL",
];

fn is_dns_label(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars()
            .last()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `app` and stage names: lowercase DNS-label style, used inside the service ID.
pub fn name_component(what: &str, s: &str, max: usize) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if !is_dns_label(s) {
        return Err(format!(
            "{what} `{s}` must start with a lowercase letter, contain only lowercase letters, digits and `-`, and not end with `-`"
        ));
    }
    if s.contains("--") {
        return Err(format!("{what} `{s}` must not contain `--`"));
    }
    if s.len() > max {
        return Err(format!("{what} `{s}` is longer than {max} characters"));
    }
    Ok(())
}

pub fn project_id(s: &str) -> Result<(), String> {
    if s.contains(':') {
        return Err(format!(
            "domain-scoped project ID `{s}` is not supported (Artifact Registry image paths cannot contain `:`)"
        ));
    }
    if !(6..=30).contains(&s.len()) || !is_dns_label(s) {
        return Err(format!(
            "`{s}` is not a valid project ID (6-30 characters: lowercase letters, digits, hyphens; starts with a letter). Use the project ID, not the display name or number"
        ));
    }
    Ok(())
}

pub fn region(s: &str) -> Result<(), String> {
    // e.g. europe-west1, us-central1, northamerica-northeast2
    let ok = s.contains('-')
        && s.chars().last().is_some_and(|c| c.is_ascii_digit())
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.contains("--");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{s}` is not a valid region (expected something like `europe-west1`; zones such as `europe-west1-b` are not regions)"
        ))
    }
}

pub fn repository_id(s: &str) -> Result<(), String> {
    if s.contains('/') {
        return Err(format!(
            "`{s}` must be the repository ID only (for example `applications`), not a path or URL"
        ));
    }
    if s.is_empty() || s.len() > 63 || !is_dns_label(s) {
        return Err(format!(
            "`{s}` is not a valid Artifact Registry repository ID (lowercase letters, digits, hyphens; up to 63 characters)"
        ));
    }
    Ok(())
}

pub fn bucket_name(s: &str) -> Result<(), String> {
    if let Some(rest) = s.strip_prefix("gs://") {
        return Err(format!("use the bucket name without `gs://` (`{rest}`)"));
    }
    let ok = (3..=63).contains(&s.len())
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .last()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && !s.starts_with("goog");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{s}` is not a valid bucket name (3-63 characters: lowercase letters, digits, `-`, `_`, `.`)"
        ))
    }
}

pub fn service_account_email(s: &str) -> Result<(), String> {
    let Some((local, domain)) = s.split_once('@') else {
        return Err(format!(
            "`{s}` is not a service account email (expected NAME@PROJECT.iam.gserviceaccount.com)"
        ));
    };
    let local_ok = !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let domain_ok = domain.ends_with(".gserviceaccount.com")
        && domain
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.'));
    if local_ok && domain_ok {
        Ok(())
    } else {
        Err(format!(
            "`{s}` is not a service account email (expected NAME@PROJECT.iam.gserviceaccount.com)"
        ))
    }
}

/// Parses a CPU limit into millicores and returns its canonical string.
pub fn cpu(s: &str) -> Result<(u32, String), String> {
    let millis = parse_cpu_millis(s).ok_or_else(|| {
        format!("`{s}` is not a valid CPU value (examples: \"1\", \"2\", \"0.5\", \"500m\")")
    })?;
    let allowed = (80..1000).contains(&millis) || [1000, 2000, 4000, 6000, 8000].contains(&millis);
    if !allowed {
        return Err(format!(
            "CPU `{s}` is not supported by Cloud Run (use 1, 2, 4, 6 or 8, or a fraction between 0.08 and 1)"
        ));
    }
    Ok((millis, canonical_cpu(millis)))
}

pub fn parse_cpu_millis(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m.parse::<u32>().ok().filter(|v| *v > 0);
    }
    let v: f64 = s.parse().ok()?;
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    let millis = (v * 1000.0).round();
    if (millis - v * 1000.0).abs() > 1e-6 || millis > u32::MAX as f64 {
        return None;
    }
    Some(millis as u32)
}

pub fn canonical_cpu(millis: u32) -> String {
    if millis.is_multiple_of(1000) {
        (millis / 1000).to_string()
    } else {
        format!("{millis}m")
    }
}

const MIB: u64 = 1024 * 1024;

/// Parses a memory limit into bytes.
pub fn parse_memory_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("Gi") {
        (n, 1024 * MIB)
    } else if let Some(n) = s.strip_suffix("Mi") {
        (n, MIB)
    } else if let Some(n) = s.strip_suffix("Ki") {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1_000_000_000)
    } else {
        (s.strip_suffix('M')?, 1_000_000)
    };
    let v: u64 = num.parse().ok()?;
    v.checked_mul(mult).filter(|b| *b > 0)
}

pub fn memory(s: &str, cpu_millis: Option<u32>) -> Result<u64, String> {
    let bytes = parse_memory_bytes(s)
        .ok_or_else(|| format!("`{s}` is not a valid memory value (examples: 512Mi, 1Gi, 2Gi)"))?;
    if !(128 * MIB..=32 * 1024 * MIB).contains(&bytes) {
        return Err(format!("memory `{s}` must be between 128Mi and 32Gi"));
    }
    if let Some(cpu) = cpu_millis {
        let gib = 1024 * MIB;
        let min_cpu = if bytes > 24 * gib {
            8000
        } else if bytes > 16 * gib {
            6000
        } else if bytes > 8 * gib {
            4000
        } else if bytes > 4 * gib {
            2000
        } else {
            0
        };
        if cpu < min_cpu {
            return Err(format!(
                "memory `{s}` requires at least {} CPU on Cloud Run",
                min_cpu / 1000
            ));
        }
    }
    Ok(bytes)
}

pub fn env_name(name: &str) -> Result<(), String> {
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(format!(
            "`{name}` is not a valid environment variable name (letters, digits and `_`, not starting with a digit)"
        ));
    }
    if RESERVED_ENV.contains(&name) {
        return Err(format!(
            "`{name}` is reserved by Cloud Run{}",
            if name == "PORT" {
                "; use `service.port` instead"
            } else {
                ""
            }
        ));
    }
    Ok(())
}

pub fn looks_sensitive(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SENSITIVE_MARKERS.iter().any(|m| upper.contains(m))
}

/// Secret ID or `projects/<project>/secrets/<id>`.
pub fn secret_name(s: &str) -> Result<(), String> {
    let id = if let Some(rest) = s.strip_prefix("projects/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() != 3 || parts[1] != "secrets" || parts[0].is_empty() {
            return Err(format!(
                "`{s}` must be a secret ID or `projects/PROJECT/secrets/SECRET` (without `/versions/...`; use `version`)"
            ));
        }
        parts[2]
    } else {
        s
    };
    let ok = !id.is_empty()
        && id.len() <= 255
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{s}` is not a valid secret ID (letters, digits, `-`, `_`; up to 255 characters)"
        ))
    }
}

pub fn secret_version(s: &str) -> Result<(), String> {
    if s == "latest" || (s.parse::<u64>().is_ok_and(|v| v > 0) && !s.starts_with('0')) {
        Ok(())
    } else {
        Err(format!(
            "`{s}` is not a valid secret version (a positive version number such as \"3\", or \"latest\")"
        ))
    }
}

/// `ORG_ID/key` or `PROJECT_ID/key` (namespaced tag key).
pub fn tag_key(k: &str) -> Result<(), String> {
    let parts: Vec<&str> = k.split('/').collect();
    let ok = parts.len() == 2
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && parts[1].len() <= 256
        && !k.chars().any(char::is_whitespace);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{k}` must be a namespaced tag key `ORGANIZATION_ID/key` or `PROJECT_ID/key`"
        ))
    }
}

pub fn tag_value(v: &str) -> Result<(), String> {
    if v.is_empty() || v.len() > 256 || v.contains('/') || v.chars().any(char::is_whitespace) {
        Err(format!("`{v}` must be a tag value short name (no `/`)"))
    } else {
        Ok(())
    }
}

/// Container names: lowercase letters, digits and `-`, starting with a
/// letter, up to 63 characters (a DNS label).
pub fn container_name(n: &str) -> Result<(), String> {
    let ok = !n.is_empty()
        && n.len() <= 63
        && n.starts_with(|c: char| c.is_ascii_lowercase())
        && !n.ends_with('-')
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{n}` is not a valid container name (lowercase letters, digits and `-`, starting with a letter, up to 63 characters)"
        ))
    }
}

/// A Compute Engine resource name (RFC 1035: networks, subnets, tags).
fn rfc1035(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && n.starts_with(|c: char| c.is_ascii_lowercase())
        && !n.ends_with('-')
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A VPC network: a name, or `projects/HOST/global/networks/NAME`.
pub fn vpc_network(n: &str) -> Result<(), String> {
    let name = match n.strip_prefix("projects/") {
        None => n,
        Some(rest) => match rest.split('/').collect::<Vec<_>>()[..] {
            [p, "global", "networks", name] if !p.is_empty() => name,
            _ => {
                return Err(format!(
                    "`{n}` must be a network name or projects/HOST_PROJECT/global/networks/NAME"
                ));
            }
        },
    };
    if rfc1035(name) {
        Ok(())
    } else {
        Err(format!("`{name}` is not a valid network name"))
    }
}

/// A subnet: a name, or `projects/HOST/regions/REGION/subnetworks/NAME` in
/// the service's region.
pub fn vpc_subnet(n: &str, region: &str) -> Result<(), String> {
    let name = match n.strip_prefix("projects/") {
        None => n,
        Some(rest) => match rest.split('/').collect::<Vec<_>>()[..] {
            [p, "regions", r, "subnetworks", name] if !p.is_empty() => {
                if r != region {
                    return Err(format!(
                        "the subnet is in `{r}`; Direct VPC needs a subnet in the service region `{region}`"
                    ));
                }
                name
            }
            _ => {
                return Err(format!(
                    "`{n}` must be a subnet name or projects/HOST_PROJECT/regions/REGION/subnetworks/NAME"
                ));
            }
        },
    };
    if rfc1035(name) {
        Ok(())
    } else {
        Err(format!("`{name}` is not a valid subnet name"))
    }
}

/// `projects/PROJECT/global/networks/NAME` for a network name in `project`;
/// full names are kept (the project is part of a network's identity).
pub fn full_network(n: &str, project: &str) -> String {
    match n.starts_with("projects/") {
        true => n.to_string(),
        false => format!("projects/{project}/global/networks/{n}"),
    }
}

/// `projects/PROJECT/regions/REGION/subnetworks/NAME` for a subnet name.
pub fn full_subnet(n: &str, project: &str, region: &str) -> String {
    match n.starts_with("projects/") {
        true => n.to_string(),
        false => format!("projects/{project}/regions/{region}/subnetworks/{n}"),
    }
}

pub fn network_tag(t: &str) -> Result<(), String> {
    if rfc1035(t) {
        Ok(())
    } else {
        Err(format!(
            "network tag `{t}` must start with a lowercase letter and contain only lowercase letters, digits and `-` (max 63)"
        ))
    }
}

/// A Cloud SQL connection name `PROJECT:REGION:INSTANCE`; the project may be
/// domain-scoped (`example.com:my-project`). Returns the project.
pub fn cloud_sql_instance(n: &str) -> Result<&str, String> {
    let err = || {
        format!(
            "`{n}` must be PROJECT:REGION:INSTANCE (for example my-gcp-project:europe-west1:db)"
        )
    };
    let mut parts = n.rsplitn(3, ':');
    let (instance, region, project) = match (parts.next(), parts.next(), parts.next()) {
        (Some(i), Some(r), Some(p)) => (i, r, p),
        _ => return Err(err()),
    };
    let region_ok = !region.is_empty()
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if project.is_empty() || !region_ok || !rfc1035_long(instance) {
        return Err(err());
    }
    Ok(project)
}

/// Like [`rfc1035`], up to 98 characters (Cloud SQL instance IDs).
fn rfc1035_long(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 98
        && n.starts_with(|c: char| c.is_ascii_lowercase())
        && !n.ends_with('-')
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn volume_name(n: &str) -> Result<(), String> {
    let ok = !n.is_empty()
        && n.len() <= 63
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && n.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "volume name `{n}` must start with a lowercase letter and contain only lowercase letters, digits and `-` (max 63)"
        ))
    }
}

pub fn mount_path(p: &str) -> Result<(), String> {
    const RESERVED: &[&str] = &[
        "/", "/dev", "/proc", "/sys", "/etc", "/bin", "/usr", "/lib", "/var/run",
    ];
    if !p.starts_with('/') {
        return Err(format!("mount path `{p}` must be absolute"));
    }
    if p.contains("..") || p.ends_with('/') && p.len() > 1 {
        return Err(format!(
            "mount path `{p}` must be normalized (no `..`, no trailing `/`)"
        ));
    }
    if RESERVED.contains(&p) {
        return Err(format!("mount path `{p}` is reserved"));
    }
    Ok(())
}

/// IAM principal such as `user:a@b.com`, `group:g@b.com`, `serviceAccount:...`, `domain:b.com`.
pub fn iam_member(m: &str) -> Result<(), String> {
    const PREFIXES: &[&str] = &[
        "user:",
        "group:",
        "serviceAccount:",
        "domain:",
        "principal:",
        "principalSet:",
    ];
    match PREFIXES.iter().find(|p| m.starts_with(**p)) {
        Some(p) if m.len() > p.len() && !m.chars().any(char::is_whitespace) => Ok(()),
        _ => Err(format!(
            "`{m}` must be an IAM principal such as `group:team@example.com` or `user:me@example.com`"
        )),
    }
}

/// `roles/x`, `projects/p/roles/x` or `organizations/o/roles/x`.
pub fn role_name(r: &str) -> Result<(), String> {
    let ok = r
        .strip_prefix("roles/")
        .is_some_and(|x| !x.is_empty() && !x.contains('/'))
        || (r.starts_with("projects/") || r.starts_with("organizations/"))
            && r.split('/').count() == 4
            && r.split('/').nth(2) == Some("roles");
    if ok && !r.chars().any(char::is_whitespace) {
        Ok(())
    } else {
        Err(format!(
            "`{r}` is not a role name (expected `roles/NAME` or a custom role path)"
        ))
    }
}

/// `PROJECT.DATASET` or `PROJECT:DATASET`.
pub fn dataset_ref(s: &str) -> Result<(String, String), String> {
    let (p, d) = s
        .split_once(':')
        .or_else(|| s.split_once('.'))
        .ok_or_else(|| format!("dataset `{s}` must be `PROJECT.DATASET`"))?;
    project_id(p).map_err(|e| format!("dataset `{s}`: {e}"))?;
    let ok = !d.is_empty()
        && d.len() <= 1024
        && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(format!(
            "dataset `{s}`: dataset ID may contain only letters, digits and `_` (a table name is not a dataset)"
        ));
    }
    Ok((p.to_string(), d.to_string()))
}

/// `run.googleapis.com`-style service name.
pub fn api_name(s: &str) -> Result<(), String> {
    let ok = s.ends_with(".googleapis.com")
        && s.len() > ".googleapis.com".len()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{s}` is not an API service name (expected e.g. `telemetry.googleapis.com`)"
        ))
    }
}

/// A region (`europe-west1`), multi-region (`EU`) or dual-region (`EUR4`).
pub fn bucket_location(s: &str) -> Result<(), String> {
    let multi = (2..=10).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric());
    if region(&s.to_ascii_lowercase()).is_ok() || multi {
        Ok(())
    } else {
        Err(format!(
            "`{s}` is not a bucket location (e.g. `europe-west1`, `EU`, `EUR4`)"
        ))
    }
}

/// GCP resource label key and value.
pub fn label(k: &str, v: &str) -> Result<(), String> {
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
    if k.is_empty()
        || k.len() > 63
        || !k.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        || !k.chars().all(ok_char)
    {
        return Err(format!(
            "label key `{k}` must start with a lowercase letter (lowercase, digits, `_`, `-`, max 63)"
        ));
    }
    if v.len() > 63 || !v.chars().all(ok_char) {
        return Err(format!(
            "label value `{v}` may contain only lowercase letters, digits, `_` and `-` (max 63)"
        ));
    }
    Ok(())
}

/// Project that owns a user-managed service account (`NAME@PROJECT.iam.gserviceaccount.com`).
pub fn service_account_project(email: &str) -> Option<&str> {
    email
        .split_once('@')?
        .1
        .strip_suffix(".iam.gserviceaccount.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_values() {
        assert_eq!(cpu("1").unwrap(), (1000, "1".into()));
        assert_eq!(cpu("1000m").unwrap(), (1000, "1".into()));
        assert_eq!(cpu("0.5").unwrap(), (500, "500m".into()));
        assert_eq!(cpu("2").unwrap().0, 2000);
        assert!(cpu("3").is_err());
        assert!(cpu("1.5").is_err());
        assert!(cpu("0.01").is_err());
        assert!(cpu("abc").is_err());
        assert!(cpu("-1").is_err());
    }

    #[test]
    fn memory_values() {
        assert_eq!(memory("512Mi", Some(1000)).unwrap(), 512 * MIB);
        assert!(memory("64Mi", None).is_err());
        assert!(memory("64Gi", None).is_err());
        assert!(memory("512", None).is_err());
        assert!(
            memory("8Gi", Some(1000))
                .unwrap_err()
                .contains("at least 2 CPU")
        );
        assert!(memory("8Gi", Some(2000)).is_ok());
    }

    #[test]
    fn names() {
        assert!(name_component("app", "hello-api", 40).is_ok());
        assert!(name_component("app", "Hello", 40).is_err());
        assert!(name_component("app", "1abc", 40).is_err());
        assert!(name_component("app", "abc-", 40).is_err());
        assert!(project_id("my-gcp-project").is_ok());
        assert!(project_id("My Project").is_err());
        assert!(project_id("example.com:proj").is_err());
        assert!(region("europe-west1").is_ok());
        assert!(region("europe-west1-b").is_err());
        assert!(region("Europe").is_err());
        assert!(
            bucket_name("gs://x")
                .unwrap_err()
                .contains("without `gs://`")
        );
        assert!(bucket_name("my-gcp-build-sources").is_ok());
        assert!(service_account_email("runtime@p.iam.gserviceaccount.com").is_ok());
        assert!(service_account_email("123-compute@developer.gserviceaccount.com").is_ok());
        assert!(service_account_email("someone@gmail.com").is_err());
        assert!(repository_id("applications").is_ok());
        assert!(repository_id("europe-west1-docker.pkg.dev/p/apps").is_err());
    }

    #[test]
    fn infra_names() {
        assert!(tag_key("123456789012/allow-public-access").is_ok());
        assert!(tag_key("my-project/env").is_ok());
        assert!(tag_key("allow-public-access").is_err());
        assert!(tag_value("true").is_ok());
        assert!(tag_value("a/b").is_err());
        assert!(volume_name("cache").is_ok());
        assert!(volume_name("Cache").is_err());
        assert!(mount_path("/mnt/cache").is_ok());
        assert!(mount_path("mnt").is_err());
        assert!(mount_path("/proc").is_err());
        assert!(mount_path("/mnt/").is_err());
        assert!(iam_member("group:finops@example.com").is_ok());
        assert!(iam_member("finops@example.com").is_err());
        assert!(iam_member("user:").is_err());
        assert!(role_name("roles/bigquery.dataViewer").is_ok());
        assert!(role_name("projects/p/roles/custom").is_ok());
        assert!(role_name("bigquery.dataViewer").is_err());
        assert_eq!(
            dataset_ref("billing-data-1234.billingdata").unwrap(),
            ("billing-data-1234".into(), "billingdata".into())
        );
        assert!(dataset_ref("billing-data-1234:billingdata").is_ok());
        assert!(dataset_ref("billing-data-1234.billingdata.gcp_billing_export").is_err());
        assert_eq!(
            service_account_project("a@my-proj.iam.gserviceaccount.com"),
            Some("my-proj")
        );
        assert_eq!(
            service_account_project("1-compute@developer.gserviceaccount.com"),
            None
        );
    }

    #[test]
    fn env_and_secrets() {
        assert!(env_name("LOG_LEVEL").is_ok());
        assert!(env_name("PORT").unwrap_err().contains("service.port"));
        assert!(env_name("1X").is_err());
        assert!(env_name("A-B").is_err());
        assert!(looks_sensitive("DB_PASSWORD"));
        assert!(!looks_sensitive("LOG_LEVEL"));
        assert!(secret_name("database-url").is_ok());
        assert!(secret_name("projects/123/secrets/database-url").is_ok());
        assert!(secret_name("projects/123/secrets/db/versions/1").is_err());
        assert!(secret_version("1").is_ok());
        assert!(secret_version("latest").is_ok());
        assert!(secret_version("0").is_err());
        assert!(secret_version("01").is_err());
        assert!(secret_version("v1").is_err());
    }
}
