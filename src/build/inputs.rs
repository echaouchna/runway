//! Build inputs beyond the uploaded source: base images.
//!
//! A source hash alone misses upstream updates published under the same tag
//! (`FROM node:22-slim`, `gcr.io/buildpacks/builder:latest`). The base images
//! are resolved to their current digests and folded into the image's content
//! address, so a new base image produces a new tag and therefore a rebuild,
//! while unchanged inputs keep reusing the existing image.

use crate::config::BuildStrategy;
use crate::gcp::registry::DigestResolver;
use crate::image_ref::ImageRef;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaseImage {
    pub reference: String,
    /// Current digest, if it could be resolved.
    pub digest: Option<String>,
}

impl BaseImage {
    pub fn display(&self) -> String {
        match &self.digest {
            Some(d) => format!("{}@{}", self.reference, d),
            None => self.reference.clone(),
        }
    }
}

/// Joins `\` continuation lines and drops comments.
fn logical_lines(dockerfile: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw in dockerfile.lines() {
        let line = raw.trim_end();
        if cur.is_empty() && line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(stripped) = line.strip_suffix('\\') {
            cur.push_str(stripped);
            cur.push(' ');
        } else {
            cur.push_str(line);
            if !cur.trim().is_empty() {
                out.push(cur.trim().to_string());
            }
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn substitute(s: &str, args: &BTreeMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in args {
        out = out
            .replace(&format!("${{{k}}}"), v)
            .replace(&format!("${k}"), v);
    }
    out
}

/// External images a Dockerfile builds from (`FROM`, `COPY/ADD --from=`), in
/// order, without stage references or `scratch`. `ARG` defaults declared
/// before the first `FROM` are substituted.
pub fn dockerfile_bases(dockerfile: &str) -> Vec<String> {
    let mut args = BTreeMap::new();
    let mut stages: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let mut seen_from = false;
    let push = |img: String, stages: &[String], out: &mut Vec<String>| {
        let lower = img.to_ascii_lowercase();
        if lower == "scratch"
            || stages.iter().any(|s| s.eq_ignore_ascii_case(&img))
            || img.is_empty()
        {
            return;
        }
        if !out.contains(&img) {
            out.push(img);
        }
    };
    for line in logical_lines(dockerfile) {
        let mut words = line.split_whitespace();
        let Some(instr) = words.next() else { continue };
        let rest: Vec<&str> = words.collect();
        match instr.to_ascii_uppercase().as_str() {
            "ARG" if !seen_from => {
                for a in rest {
                    if let Some((k, v)) = a.split_once('=') {
                        args.insert(k.to_string(), v.trim_matches('"').to_string());
                    }
                }
            }
            "FROM" => {
                seen_from = true;
                let mut it = rest.iter().filter(|w| !w.starts_with("--"));
                if let Some(img) = it.next() {
                    push(substitute(img, &args), &stages, &mut out);
                }
                if let (Some(as_kw), Some(name)) = (it.next(), it.next())
                    && as_kw.eq_ignore_ascii_case("as")
                {
                    stages.push(name.to_string());
                }
            }
            "COPY" | "ADD" => {
                for w in rest {
                    if let Some(img) = w.strip_prefix("--from=") {
                        let img = substitute(img, &args);
                        // Stage names and indexes are internal; images contain `/` or `:`.
                        if (img.contains('/') || img.contains(':'))
                            && !img.chars().all(|c| c.is_ascii_digit())
                        {
                            push(img, &stages, &mut out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Base image references for a build strategy.
pub fn base_references(strategy: &BuildStrategy, root: &Path) -> Vec<String> {
    match strategy {
        BuildStrategy::Buildpacks { builder } => vec![builder.clone()],
        BuildStrategy::Dockerfile { path } => std::fs::read_to_string(root.join(path))
            .map(|t| dockerfile_bases(&t))
            .unwrap_or_default(),
    }
}

/// Resolves base images to digests (concurrently). Unresolvable references
/// (variables, registry errors) keep `digest: None` and are reported.
pub async fn resolve(
    refs: &[String],
    resolver: Option<&dyn DigestResolver>,
) -> (Vec<BaseImage>, Vec<String>) {
    let mut notes = Vec::new();
    let Some(resolver) = resolver else {
        return (
            refs.iter()
                .map(|r| BaseImage {
                    reference: r.clone(),
                    digest: None,
                })
                .collect(),
            notes,
        );
    };
    let lookups = refs.iter().map(|r| async move {
        let digest = match ImageRef::parse(r) {
            Ok(img) => resolver.resolve(&img).await.map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        (r.clone(), digest)
    });
    let mut out = Vec::new();
    for (reference, res) in futures::future::join_all(lookups).await {
        let digest = match res {
            Ok(Some(d)) => Some(d),
            Ok(None) => {
                notes.push(format!(
                    "base image {reference} was not found in its registry"
                ));
                None
            }
            Err(e) => {
                notes.push(format!(
                    "base image {reference} could not be resolved ({e}); upstream updates to it are not detected"
                ));
                None
            }
        };
        out.push(BaseImage { reference, digest });
    }
    (out, notes)
}

/// Content address of the image: the source hash plus the base images.
/// With no base images it is the source hash itself.
pub fn fingerprint(source_sha256: &str, bases: &[BaseImage]) -> String {
    if bases.is_empty() {
        return source_sha256.to_string();
    }
    let mut h = Sha256::new();
    h.update(b"runway-inputs-v1\n");
    h.update(source_sha256.as_bytes());
    for b in bases {
        h.update(b"\n");
        h.update(b.display().as_bytes());
    }
    crate::build::package::hex(&h.finalize())
}

/// Value of the `runway.dev/base-images` annotation.
pub fn annotation(bases: &[BaseImage]) -> String {
    bases
        .iter()
        .map(BaseImage::display)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Base images whose digest differs between the deployed release's
/// annotation and the current resolution: `(reference, old, new)`.
pub fn changed_since(deployed: &str, current: &[BaseImage]) -> Vec<(String, String, String)> {
    let old: BTreeMap<&str, &str> = deployed
        .split_whitespace()
        .filter_map(|e| e.rsplit_once('@'))
        .collect();
    current
        .iter()
        .filter_map(|b| {
            let new = b.digest.as_deref()?;
            let prev = old.get(b.reference.as_str())?;
            (*prev != new).then(|| (b.reference.clone(), prev.to_string(), new.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dockerfile_bases() {
        let df = r#"
# syntax=docker/dockerfile:1
ARG NODE=22
FROM --platform=linux/amd64 node:${NODE}-slim AS build
RUN npm ci
FROM build AS test
FROM gcr.io/distroless/nodejs22-debian12 \
    AS runtime
COPY --from=build /app /app
COPY --from=ghcr.io/acme/tools:1.2 /bin/tool /bin/tool
COPY --from=0 /x /y
FROM scratch
"#;
        assert_eq!(
            dockerfile_bases(df),
            [
                "node:22-slim",
                "gcr.io/distroless/nodejs22-debian12",
                "ghcr.io/acme/tools:1.2"
            ]
        );
    }

    #[test]
    fn fingerprint_tracks_base_digests() {
        let src = "a".repeat(64);
        assert_eq!(
            fingerprint(&src, &[]),
            src,
            "no bases: unchanged source hash"
        );
        let b1 = vec![BaseImage {
            reference: "node:22-slim".into(),
            digest: Some("sha256:1".into()),
        }];
        let b2 = vec![BaseImage {
            reference: "node:22-slim".into(),
            digest: Some("sha256:2".into()),
        }];
        assert_ne!(
            fingerprint(&src, &b1),
            fingerprint(&src, &b2),
            "new base image, new tag"
        );
        assert_eq!(
            fingerprint(&src, &b1),
            fingerprint(&src, &b1.clone()),
            "deterministic"
        );
        assert_ne!(
            fingerprint(&src, &b1),
            fingerprint(&"b".repeat(64), &b1),
            "source still counts"
        );
    }

    #[test]
    fn detects_base_changes_since_the_deployed_release() {
        let deployed = "node:22-slim@sha256:1 gcr.io/x/y@sha256:9";
        let now = vec![
            BaseImage {
                reference: "node:22-slim".into(),
                digest: Some("sha256:2".into()),
            },
            BaseImage {
                reference: "gcr.io/x/y".into(),
                digest: Some("sha256:9".into()),
            },
        ];
        assert_eq!(
            changed_since(deployed, &now),
            [(
                "node:22-slim".to_string(),
                "sha256:1".to_string(),
                "sha256:2".to_string()
            )]
        );
        assert_eq!(
            annotation(&now),
            "node:22-slim@sha256:2 gcr.io/x/y@sha256:9"
        );
    }
}
