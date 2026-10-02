//! Offline description of a resolved stack: a diagram (ASCII art or Mermaid)
//! and a plain-language explanation of what runway deploys and manages.

use crate::config::{Artifact, Deployment, RoleTarget};
use crate::naming;
use crate::provision::{all_steps, required_apis};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    pub title: String,
    pub items: Vec<String>,
}

fn access_label(d: &Deployment) -> String {
    let s = &d.service;
    if s.iap.enabled {
        let who = if s.iap.members.is_empty() {
            "nobody yet (no IAP members)".to_string()
        } else {
            s.iap.members.join(", ")
        };
        format!("Identity-Aware Proxy: {who}")
    } else if s.public {
        "public: allUsers".into()
    } else {
        "private: callers with roles/run.invoker".into()
    }
}

fn image_label(d: &Deployment) -> String {
    match &d.artifact {
        Artifact::Image { reference, .. } => reference.clone(),
        Artifact::Build(b) => format!(
            "{}:src-<hash>",
            naming::build_image_name(
                &b.artifact_location,
                &d.project,
                &b.artifact_repository,
                &d.app
            )
        ),
    }
}

fn sizing(d: &Deployment) -> String {
    let s = &d.service;
    format!(
        "{} vCPU, {}, {}-{} instances, concurrency {}, timeout {}s, port {}",
        s.cpu, s.memory, s.min_instances, s.max_instances, s.concurrency, s.timeout_seconds, s.port
    )
}

/// Data-plane edges from the service: (label, target).
fn runtime_edges(d: &Deployment) -> Vec<(String, String)> {
    let s = &d.service;
    let mut out = Vec::new();
    for r in &s.identity.roles {
        out.push((r.role.clone(), r.target.to_string()));
    }
    for (env, sec) in &s.secrets {
        out.push((
            format!("secret env {env}"),
            format!("Secret Manager {}@{}", sec.secret, sec.version),
        ));
    }
    for (name, v) in &s.volumes {
        out.push((
            format!(
                "mount {} ({name}, {})",
                v.mount_path,
                if v.read_only { "ro" } else { "rw" }
            ),
            format!("gs://{}", v.bucket),
        ));
    }
    out
}

// ---------------------------------------------------------------- ASCII --

/// Maximum width of a line inside a box.
const BOX_WIDTH: usize = 88;

/// Wraps at ", " or spaces; continuation lines are indented.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in line.split_inclusive([' ']) {
        if cur.chars().count() + word.chars().count() > width && !cur.trim().is_empty() {
            out.push(cur.trim_end().to_string());
            cur = format!("  {word}");
        } else {
            cur.push_str(word);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim_end().to_string());
    }
    out
}

fn boxed(lines: &[String], double: bool) -> Vec<String> {
    let lines: Vec<String> = lines.iter().flat_map(|l| wrap(l, BOX_WIDTH)).collect();
    let lines = &lines[..];
    let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let (h, v) = if double { ('=', '#') } else { ('-', '|') };
    let edge = format!("+{}+", h.to_string().repeat(w + 2));
    let mut out = vec![edge.clone()];
    for l in lines {
        let pad = w - l.chars().count();
        out.push(format!("{v} {l}{} {v}", " ".repeat(pad)));
    }
    out.push(edge);
    out
}

fn indent(lines: Vec<String>, n: usize) -> Vec<String> {
    lines
        .into_iter()
        .map(|l| format!("{}{l}", " ".repeat(n)))
        .collect()
}

pub fn ascii(d: &Deployment) -> String {
    let s = &d.service;
    let mut out: Vec<String> = Vec::new();

    // Who can reach the service.
    out.extend(indent(
        boxed(&[format!("Clients ({})", access_label(d))], false),
        2,
    ));
    out.push("        |".into());
    out.push(format!(
        "        v  {}",
        if s.iap.enabled {
            "IAP authenticates every request"
        } else if s.public {
            "unauthenticated HTTPS"
        } else {
            "HTTPS with an identity token"
        }
    ));

    // The service itself.
    let mut svc = vec![
        format!("Cloud Run service {}", d.service_id),
        format!("{} / {}", d.project, d.region),
        sizing(d),
        format!("image: {}", image_label(d)),
        format!(
            "runs as: {}{}",
            s.service_account,
            if s.identity.create {
                " (created by runway)"
            } else {
                ""
            }
        ),
    ];
    if !s.env.is_empty() {
        svc.push(format!(
            "env: {}",
            s.env.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !s.tags.is_empty() {
        svc.push(format!(
            "tags: {}",
            s.tags
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.extend(indent(boxed(&svc, true), 2));

    // What it talks to.
    let edges = runtime_edges(d);
    if !edges.is_empty() {
        out.push("        |".into());
        let lw = edges
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        for (i, (label, target)) in edges.iter().enumerate() {
            let branch = if i + 1 == edges.len() { "`--" } else { "+--" };
            let pad = " ".repeat(lw - label.chars().count());
            out.push(format!("        {branch} {label}{pad} --> {target}"));
        }
    }

    // How the image is produced.
    out.push(String::new());
    match &d.artifact {
        Artifact::Build(b) => {
            out.push("  Build pipeline (runway deploy):".into());
            let stages = [
                format!("source {}", b.context_dir.display()),
                format!("gs://{}", b.source_bucket),
                format!(
                    "Cloud Build ({}) as {}",
                    match b.strategy {
                        crate::config::BuildStrategy::Dockerfile { .. } => "docker build",
                        crate::config::BuildStrategy::Buildpacks { .. } => "buildpacks",
                    },
                    b.build_service_account
                ),
                format!(
                    "Artifact Registry {}/{}",
                    b.artifact_location, b.artifact_repository
                ),
                "Cloud Run revision".to_string(),
            ];
            for (i, st) in stages.iter().enumerate() {
                out.extend(indent(boxed(std::slice::from_ref(st), false), 4));
                if i + 1 < stages.len() {
                    out.push("        |".into());
                    out.push("        v".into());
                }
            }
        }
        Artifact::Image { reference, .. } => {
            out.push(format!(
                "  Image: {reference} (resolved to an immutable digest at deploy)"
            ));
        }
    }

    if !d.buckets.is_empty() {
        out.push(String::new());
        out.push("  Buckets managed by runway:".into());
        for b in d.buckets.values() {
            out.push(format!(
                "    gs://{}  ({}, uniform access, public access prevention)",
                b.name, b.location
            ));
        }
    }
    out.join("\n") + "\n"
}

// -------------------------------------------------------------- Mermaid --

fn mm(s: &str) -> String {
    s.replace('"', "#quot;")
}

pub fn mermaid(d: &Deployment) -> String {
    let s = &d.service;
    let mut m = vec!["flowchart LR".to_string()];
    let clients = if s.iap.enabled {
        format!(
            "Users: {}",
            if s.iap.members.is_empty() {
                "none".into()
            } else {
                s.iap.members.join(", ")
            }
        )
    } else if s.public {
        "Internet (allUsers)".to_string()
    } else {
        "Callers with roles/run.invoker".to_string()
    };
    m.push(format!("  clients([\"{}\"])", mm(&clients)));
    let mut svc = format!(
        "Cloud Run: {}<br/>{} / {}<br/>{}<br/>runs as {}",
        d.service_id,
        d.project,
        d.region,
        mm(&sizing(d)),
        s.service_account
    );
    if !s.env.is_empty() {
        svc.push_str(&format!(
            "<br/>env: {}",
            s.env.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    m.push(format!("  svc[[\"{svc}\"]]"));
    if s.iap.enabled {
        m.push("  iap{{\"Identity-Aware Proxy\"}}".into());
        m.push("  clients --> iap --> svc".into());
    } else {
        m.push(format!(
            "  clients -->|{}| svc",
            if s.public {
                "public HTTPS"
            } else {
                "identity token"
            }
        ));
    }
    if let Artifact::Build(b) = &d.artifact {
        m.push("  subgraph build [Build pipeline]".into());
        m.push(format!(
            "    src[\"source {}\"]",
            mm(&b.context_dir.display().to_string())
        ));
        m.push(format!("    srcb[(\"gs://{}\")]", b.source_bucket));
        m.push(format!(
            "    cb[\"Cloud Build<br/>as {}\"]",
            b.build_service_account
        ));
        m.push(format!(
            "    ar[(\"Artifact Registry<br/>{}/{}\")]",
            b.artifact_location, b.artifact_repository
        ));
        m.push("    src --> srcb --> cb --> ar".into());
        m.push("  end".into());
        m.push("  ar -->|\"image digest\"| svc".into());
    } else {
        m.push(format!("  img[(\"{}\")]", mm(&image_label(d))));
        m.push("  img -->|\"image digest\"| svc".into());
    }
    for (i, (label, target)) in runtime_edges(d).iter().enumerate() {
        let node = format!("t{i}");
        let shape = if target.starts_with("gs://")
            || target.starts_with("dataset")
            || target.starts_with("bucket")
        {
            format!("[(\"{}\")]", mm(target))
        } else {
            format!("[\"{}\"]", mm(target))
        };
        m.push(format!("  {node}{shape}"));
        m.push(format!("  svc -->|\"{}\"| {node}", mm(label)));
    }
    for (i, b) in d.buckets.values().enumerate() {
        if !runtime_edges(d).iter().any(|(_, t)| t.contains(&b.name)) {
            m.push(format!("  mb{i}[(\"gs://{} (managed)\")]", b.name));
        }
    }
    if !s.tags.is_empty() {
        let tags: Vec<String> = s.tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
        m.push(format!("  tags>\"tags: {}\"]", mm(&tags.join(", "))));
        m.push("  tags -.- svc".into());
    }
    m.join("\n") + "\n"
}

// ---------------------------------------------------------- explanation --

pub fn explain(d: &Deployment) -> Vec<Section> {
    let s = &d.service;
    let mut out = Vec::new();

    out.push(Section {
        title: "Overview".into(),
        items: vec![
            format!(
                "`{}` is deployed as the Cloud Run service `{}` in project `{}`, region `{}` (stage `{}`).",
                d.app, d.service_id, d.project, d.region, d.stage
            ),
            format!("Sizing: {}.", sizing(d)),
            "runway reads the live state before every change, so re-running deploy is safe and only applies what is missing.".into(),
        ],
    });

    let mut build = Vec::new();
    match &d.artifact {
        Artifact::Build(b) => {
            build.push(format!(
                "The image is built from `{}` with {}: the source is uploaded to `gs://{}`, built by Cloud Build as `{}`, pushed to Artifact Registry `{}/{}`, then deployed by digest.",
                b.context_dir.display(), b.strategy, b.source_bucket, b.build_service_account, b.artifact_location, b.artifact_repository
            ));
            build.push("Unchanged source is not rebuilt: the image tag is derived from a hash of the source.".into());
            if b.create_resources {
                build.push("runway creates the source bucket (30-day cleanup), the Docker repository and the build service account with its roles (log writer, repository writer, source reader) if they are missing.".into());
            } else {
                build.push("The repository, source bucket and build service account must already exist (`runway doctor` checks them).".into());
            }
        }
        Artifact::Image { reference, .. } => {
            build.push(format!(
                "The existing image `{reference}` is deployed, pinned to its immutable digest."
            ));
        }
    }
    out.push(Section {
        title: "Build".into(),
        items: build,
    });

    let mut access = vec![format!("Access: {}.", access_label(d))];
    if s.iap.enabled {
        access.push("IAP is enabled on the service; its service agent is allowed to invoke it, and the members above get roles/iap.httpsResourceAccessor. The service is not open to allUsers.".into());
    } else if s.public {
        access.push(
            "Anyone on the internet can call the service (roles/run.invoker for allUsers).".into(),
        );
    } else {
        access.push(
            "Only principals with roles/run.invoker can call it, with an identity token.".into(),
        );
    }
    if !d.project_tags.is_empty() {
        access.push(format!(
            "Project tags bound to `{}` before anything is created, and awaited until effective (organization policy conditions read them): {}.",
            d.project,
            d.project_tags.iter().map(|(k, v)| format!("`{k}={v}`")).collect::<Vec<_>>().join(", ")
        ));
    }
    if !s.tags.is_empty() {
        access.push(format!(
            "Resource Manager tags bound to the service: {} (bound before public access is granted, for organization policies that require them).",
            s.tags.iter().map(|(k, v)| format!("`{k}={v}`")).collect::<Vec<_>>().join(", ")
        ));
    }
    out.push(Section {
        title: "Access".into(),
        items: access,
    });

    let mut identity = vec![format!(
        "The container runs as `{}`{}.",
        s.service_account,
        if s.identity.create {
            ", created by runway if missing"
        } else {
            " (must exist)"
        }
    )];
    for r in &s.identity.roles {
        let why = match &r.target {
            RoleTarget::Dataset { .. } => "dataset-level, nothing else in that project",
            RoleTarget::Bucket { .. } => "bucket-level",
            RoleTarget::Project { .. } => "project-level",
            RoleTarget::Secret { .. } => "single secret",
            RoleTarget::Repository { .. } => "repository-level",
        };
        identity.push(format!("`{}` on {} ({why}).", r.role, r.target));
    }
    for (env, sec) in &s.secrets {
        identity.push(format!(
            "`{env}` comes from Secret Manager `{}` version `{}`; runway never reads the value.",
            sec.secret, sec.version
        ));
    }
    out.push(Section {
        title: "Identity and permissions".into(),
        items: identity,
    });

    if !s.env.is_empty() {
        out.push(Section {
            title: "Configuration (environment variables)".into(),
            items: s
                .env
                .iter()
                .map(|(k, v)| format!("`{k}` = `{v}`"))
                .collect(),
        });
    }

    let mut storage = Vec::new();
    for b in d.buckets.values() {
        let mut extra = Vec::new();
        if let Some(n) = b.delete_after_days {
            extra.push(format!("objects deleted after {n} days"));
        }
        if b.versioning == Some(true) {
            extra.push("versioning".into());
        }
        storage.push(format!(
            "Bucket `gs://{}` (key `{}`) in `{}`, created and kept configured by runway: uniform access, public access prevention{}.",
            b.name,
            b.key,
            b.location,
            if extra.is_empty() { String::new() } else { format!(", {}", extra.join(", ")) }
        ));
    }
    for (name, v) in &s.volumes {
        storage.push(format!(
            "Volume `{name}`: `gs://{}` mounted at `{}` ({}).",
            v.bucket,
            v.mount_path,
            if v.read_only {
                "read-only"
            } else {
                "read-write"
            }
        ));
    }
    if !storage.is_empty() {
        out.push(Section {
            title: "Storage".into(),
            items: storage,
        });
    }

    let apis = required_apis(d);
    out.push(Section {
        title: "APIs".into(),
        items: vec![format!(
            "{} on `{}`: {}.",
            if d.apis.enable {
                "Enabled by runway when missing"
            } else {
                "Must be enabled (checked by `runway doctor`)"
            },
            d.project,
            apis.join(", ")
        )],
    });

    let mut order: Vec<String> = all_steps(d).iter().map(|st| st.describe(d)).collect();
    // Insert the build and roll-out where they happen.
    let first_post = order
        .iter()
        .position(|n| n.starts_with("tag ") || n.starts_with("IAP"))
        .unwrap_or(order.len());
    order.insert(
        first_post,
        format!(
            "deploy the Cloud Run service {} and wait until it is ready",
            d.service_id
        ),
    );
    if matches!(d.artifact, Artifact::Build(_)) {
        order.insert(
            first_post,
            "build the image (skipped if this source was already built)".into(),
        );
    }
    order.push(if s.public && !s.iap.enabled {
        "grant allUsers roles/run.invoker".into()
    } else {
        "make sure allUsers has no invoker access".into()
    });
    out.push(Section {
        title: "Deployment order".into(),
        items: order
            .into_iter()
            .enumerate()
            .map(|(i, o)| format!("{}. {o}", i + 1))
            .collect(),
    });

    out.push(Section {
        title: "Failure handling".into(),
        items: vec![format!(
            "Each step is retried up to {} time(s) (initial delay {}, doubling, max {}); configuration errors, ownership conflicts and failed Docker builds are not retried.",
            d.retry.attempts,
            humantime::format_duration(d.retry.delay),
            humantime::format_duration(d.retry.max_delay)
        )],
    });
    out
}

pub fn explanation_text(sections: &[Section]) -> String {
    let mut s = String::new();
    let p = crate::style::out();
    for sec in sections {
        s.push_str(&format!("{}\n", p.bold(&sec.title)));
        for it in &sec.items {
            let prefix = if it.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                "  "
            } else {
                "  - "
            };
            s.push_str(&format!("{prefix}{it}\n"));
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment() -> (tempfile::TempDir, Deployment) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            r#"
version: 1
app: gcptree
provider:
  project: my-gcp-project
  region: europe-west1
  enable_apis: true
  create_build_resources: true
  artifact_repository: runway
  source_bucket: "${project}-runway-sources"
  build_service_account: "runway-build@${project}.iam.gserviceaccount.com"
buckets:
  cache: { name: "${project}-gcptree-cache" }
service:
  source: .
  service_account: "gcptree-run@${project}.iam.gserviceaccount.com"
  identity:
    create: true
    roles:
      - { role: roles/bigquery.dataViewer, dataset: billing-data-1234.billingdata }
      - { role: roles/storage.objectUser, bucket: "${buckets.cache}" }
  env: { GCPTREE_TABLE: "billing-data-1234.billingdata.t", GCPTREE_CACHE_BUCKET: "gs://${buckets.cache}/gcptree" }
  tags: { "123/allow-public-access": "true" }
  iap: { members: ["group:finops@example.com"] }
stages: { prod: {} }
"#,
        )
        .unwrap();
        let d = crate::config::load_and_resolve(&p, "prod", &Default::default())
            .unwrap()
            .1
            .deployment;
        (dir, d)
    }

    #[test]
    fn ascii_diagram_shows_the_whole_stack() {
        let (_d, d) = deployment();
        let a = ascii(&d);
        assert!(
            a.contains("Identity-Aware Proxy: group:finops@example.com"),
            "{a}"
        );
        assert!(a.contains("Cloud Run service gcptree-prod"));
        assert!(a.contains("my-gcp-project / europe-west1"));
        assert!(a.contains("roles/bigquery.dataViewer"));
        assert!(a.contains("--> dataset billing-data-1234.billingdata"));
        assert!(a.contains("gs://my-gcp-project-runway-sources"));
        assert!(a.contains(
            "Cloud Build (docker build) as runway-build@my-gcp-project.iam.gserviceaccount.com"
        ));
        assert!(a.contains("gs://my-gcp-project-gcptree-cache"));
        assert!(
            a.is_ascii(),
            "pure ASCII so it renders in any terminal or CI log"
        );
    }

    #[test]
    fn long_lines_wrap_inside_boxes() {
        let long = format!(
            "env: {}",
            (0..20)
                .map(|i| format!("VARIABLE_{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let b = boxed(&[long], false);
        assert!(
            b.iter().all(|l| l.chars().count() <= BOX_WIDTH + 4),
            "{b:#?}"
        );
        assert!(b.len() > 3, "wrapped onto several lines");
        assert!(b.join("").contains("VARIABLE_19"));
    }

    #[test]
    fn mermaid_diagram_is_well_formed() {
        let (_d, d) = deployment();
        let m = mermaid(&d);
        assert!(m.starts_with("flowchart LR\n"));
        assert!(m.contains("clients --> iap --> svc"));
        assert!(m.contains("subgraph build [Build pipeline]"));
        assert!(m.contains("ar -->|\"image digest\"| svc"));
        assert!(m.contains("svc -->|\"roles/storage.objectUser\"|"));
        assert!(m.contains("clients([\"Users: group:finops@example.com\"])"));
        // Unquoted labels with `(`, `/` etc. break Mermaid (checked with mermaid-cli).
        assert_eq!(
            m.matches("-->|").count(),
            m.matches("-->|\"").count(),
            "all edge labels quoted"
        );
        assert_eq!(
            m.matches("subgraph").count(),
            m.lines().filter(|l| l.trim() == "end").count()
        );
        assert!(
            !m.contains("|\"\"|") && !m.contains("[\"\"]"),
            "no empty labels"
        );
    }

    #[test]
    fn explanation_covers_order_and_access() {
        let (_d, d) = deployment();
        let text = explanation_text(&explain(&d));
        let order = &text[text.find("Deployment order").expect("order section")..];
        let pos = |s: &str| {
            order
                .find(s)
                .unwrap_or_else(|| panic!("missing `{s}` in:\n{order}"))
        };
        assert!(pos("API(s) enabled") < pos("bucket gs://my-gcp-project-gcptree-cache"));
        assert!(pos("service account gcptree-run@") < pos("grant roles/storage.objectUser"));
        assert!(pos("build the image") < pos("deploy the Cloud Run service"));
        assert!(pos("deploy the Cloud Run service") < pos("tag 123/allow-public-access=true"));
        assert!(text.contains("is not open to allUsers"));
        assert!(
            text.contains("`GCPTREE_CACHE_BUCKET` = `gs://my-gcp-project-gcptree-cache/gcptree`")
        );
    }
}
