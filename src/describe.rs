//! Offline description of a resolved stack: a diagram (ASCII art or Mermaid)
//! and a plain-language explanation of what runway deploys and manages.

use crate::config::{Artifact, Deployment, RoleTarget};
use crate::naming;
use crate::provision::{all_steps, required_apis};
use crate::style::Painter;
use serde::Serialize;

const PLAIN: Painter = Painter { enabled: false };

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
        Artifact::Build(_) => format!("{}:src-<hash>", d.build_image().unwrap_or_default()),
        Artifact::Promote(p) => format!(
            "{}@<digest stage {} serves>",
            d.build_image().unwrap_or_default(),
            p.from
        ),
    }
}

/// `LOCATION/REPOSITORY` of the build repository, with its project when it
/// is not the deployment project.
fn build_repository(d: &Deployment, b: &crate::config::BuildConfig) -> String {
    match b.artifact_project == d.project {
        true => format!("{}/{}", b.artifact_location, b.artifact_repository),
        false => format!(
            "{}/{}/{}",
            b.artifact_location, b.artifact_project, b.artifact_repository
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
    for c in &s.cloud_sql {
        out.push((format!("socket /cloudsql/{c}"), format!("Cloud SQL {c}")));
    }
    if let Some(v) = &s.vpc {
        out.push((
            format!("egress {}", v.egress),
            format!("VPC {} / {}", v.network, v.subnet),
        ));
    }
    out
}

/// How the service's custom domains are served.
fn domain_lines(d: &Deployment) -> Vec<String> {
    use crate::config::DomainMode;
    let s = &d.service;
    let mut out = Vec::new();
    let how = match (&d.domains.mode, &d.domains.existing) {
        (DomainMode::LoadBalancer, _) => format!(
            "through the load balancer runway creates for the stage (`{}-lb`), with a managed certificate",
            naming::service_id(&d.app, &d.stage)
        ),
        (DomainMode::ExistingLoadBalancer, Some(e)) => format!(
            "through URL map `{}` (runway adds and removes only its own routes)",
            e.url_map
        ),
        (DomainMode::ExistingLoadBalancer, None) => "through an existing load balancer".into(),
        (DomainMode::DomainMapping, _) => "through Cloud Run domain mappings".into(),
    };
    let (urls, domains): (Vec<_>, Vec<_>) = s.domains.iter().partition(|e| e.is_cloud_run_url());
    if !domains.is_empty() {
        let list: Vec<String> = domains.iter().map(|e| format!("`{e}`")).collect();
        out.push(format!("Custom domains {} {how}.", list.join(", ")));
    }
    for u in urls {
        out.push(format!("Cloud Run custom URL `https://{u}`."));
    }
    if let Some(w) = &s.preview_domain {
        out.push(format!(
            "Previews also answer on `<tag>.{}`.",
            w.trim_start_matches("*.")
        ));
    }
    if !out.is_empty() {
        out.push(match &d.domains.dns {
            Some(z) => format!(
                "DNS records are written to Cloud DNS zone `{}` (project `{}`).",
                z.zone, z.project
            ),
            None => {
                "DNS records are printed by `plan` and `deploy`, to create at your DNS provider."
                    .into()
            }
        });
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

#[cfg(test)]
fn boxed(lines: &[String], double: bool) -> Vec<String> {
    boxed_with(lines, double, PLAIN, |_, l| l.to_string())
}

/// A box around `lines` (wrapped). `style` colors line `i` of `lines` (not
/// continuation lines); widths are measured on the plain text. The double
/// box (the service) gets cyan borders, the others dim ones.
fn boxed_with(
    lines: &[String],
    double: bool,
    p: Painter,
    style: impl Fn(usize, &str) -> String,
) -> Vec<String> {
    let lines: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .flat_map(|(i, l)| wrap(l, BOX_WIDTH).into_iter().map(move |w| (i, w)))
        .collect();
    let w = lines
        .iter()
        .map(|(_, l)| l.chars().count())
        .max()
        .unwrap_or(0);
    let (h, v) = if double { ('=', "#") } else { ('-', "|") };
    let border = |s: &str| if double { p.cyan(s) } else { p.dim(s) };
    let edge = border(&format!("+{}+", h.to_string().repeat(w + 2)));
    let mut out = vec![edge.clone()];
    for (i, l) in &lines {
        let pad = w - l.chars().count();
        let text = if l.starts_with("  ") {
            l.clone()
        } else {
            style(*i, l)
        };
        out.push(format!(
            "{} {text}{} {}",
            border(v),
            " ".repeat(pad),
            border(v)
        ));
    }
    out.push(edge);
    out
}

/// `image: ...`, `runs as: ...`: the label dimmed.
fn labeled(p: Painter, line: &str) -> String {
    match line.split_once(": ") {
        Some((label, rest)) if ["image", "runs as", "env", "tags"].contains(&label) => {
            format!("{} {rest}", p.dim(&format!("{label}:")))
        }
        _ => line.to_string(),
    }
}

fn indent(lines: Vec<String>, n: usize) -> Vec<String> {
    lines
        .into_iter()
        .map(|l| format!("{}{l}", " ".repeat(n)))
        .collect()
}

/// The diagram without colors (JSON output, files, pipes).
pub fn ascii(d: &Deployment) -> String {
    ascii_with(d, PLAIN)
}

/// The diagram; with colors, the service stands out, permissions and their
/// targets are highlighted and the lines and boxes recede.
pub fn ascii_with(d: &Deployment, p: Painter) -> String {
    let s = &d.service;
    let mut out: Vec<String> = Vec::new();
    let bar = || format!("        {}", p.dim("|"));
    let down = |label: &str| {
        if label.is_empty() {
            format!("        {}", p.dim("v"))
        } else {
            format!("        {}  {label}", p.dim("v"))
        }
    };
    let plain = |_: usize, l: &str| l.to_string();

    // Who can reach the service.
    out.extend(indent(
        boxed_with(
            &[format!("Clients ({})", access_label(d))],
            false,
            p,
            |_, l| match l.strip_prefix("Clients") {
                Some(rest) => format!("{}{rest}", p.bold("Clients")),
                None => l.to_string(),
            },
        ),
        2,
    ));
    out.push(bar());
    out.push(down(if s.iap.enabled {
        "IAP authenticates every request"
    } else if s.public {
        "unauthenticated HTTPS"
    } else {
        "HTTPS with an identity token"
    }));

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
    out.extend(indent(
        boxed_with(&svc, true, p, |i, l| match i {
            0 => match l.strip_prefix("Cloud Run service ") {
                Some(id) => format!("{}{}", p.bold("Cloud Run service "), p.bold_cyan(id)),
                None => l.to_string(),
            },
            _ => labeled(p, l),
        }),
        2,
    ));

    // What it talks to.
    let edges = runtime_edges(d);
    if !edges.is_empty() {
        out.push(bar());
        let lw = edges
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        for (i, (label, target)) in edges.iter().enumerate() {
            let branch = if i + 1 == edges.len() { "`--" } else { "+--" };
            let pad = " ".repeat(lw - label.chars().count());
            out.push(format!(
                "        {} {}{pad}{}{}",
                p.dim(branch),
                p.yellow(label),
                p.dim(" --> "),
                p.cyan(target)
            ));
        }
    }

    // How the image is produced.
    out.push(String::new());
    match &d.artifact {
        Artifact::Build(b) => {
            out.push(format!("  {}", p.bold("Build pipeline (runway deploy):")));
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
                format!("Artifact Registry {}", build_repository(d, b)),
                "Cloud Run revision".to_string(),
            ];
            for (i, st) in stages.iter().enumerate() {
                out.extend(indent(
                    boxed_with(std::slice::from_ref(st), false, p, plain),
                    4,
                ));
                if i + 1 < stages.len() {
                    out.push(bar());
                    out.push(down(""));
                }
            }
        }
        Artifact::Image { reference, .. } => {
            out.push(format!(
                "  {} {} {}",
                p.bold("Image:"),
                p.cyan(reference),
                p.dim("(resolved to an immutable digest at deploy)")
            ));
        }
        Artifact::Promote(pr) => {
            out.push(format!(
                "  {} {} {}",
                p.bold("Image:"),
                p.cyan(&format!("what stage {} serves", pr.from)),
                p.dim(&format!(
                    "(copied into {}, no build)",
                    d.build_image().unwrap_or_default()
                ))
            ));
        }
    }

    if !d.buckets.is_empty() {
        out.push(String::new());
        out.push(format!("  {}", p.bold("Buckets managed by runway:")));
        for b in d.buckets.values() {
            out.push(format!(
                "    {}  {}",
                p.cyan(&format!("gs://{}", b.name)),
                p.dim(&format!(
                    "({}, uniform access, public access prevention)",
                    b.location
                ))
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
            "    ar[(\"Artifact Registry<br/>{}\")]",
            build_repository(d, b)
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
                "The image is built from `{}` with {}: the source is uploaded to `gs://{}`, built by Cloud Build as `{}`, pushed to `{}`, then deployed by digest.",
                b.context_dir.display(), b.strategy, b.source_bucket, b.build_service_account, d.build_image().unwrap_or_default()
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
        Artifact::Promote(p) => {
            build.push(format!(
                "Nothing is built: the image stage `{}` serves (its one revision with all the traffic) is copied, same digest, into `{}` and deployed from there. With `--tag-rc`/`--tag`, it must be that stage's candidate of the version, or come from the commit being deployed.",
                p.from,
                d.build_image().unwrap_or_default()
            ));
            if p.create_resources {
                build.push("runway creates that repository if it is missing; no source bucket, build account or Cloud Build are needed.".into());
            }
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
            RoleTarget::RunService { .. } | RoleTarget::RunJob { .. } => "one service or job",
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

    let mut network = Vec::new();
    if let Some(v) = &s.vpc {
        network.push(format!(
            "Direct VPC egress through `{}` (subnet `{}`): {}{}.",
            v.network,
            v.subnet,
            if v.egress == "all-traffic" {
                "all outbound traffic goes through the VPC"
            } else {
                "traffic to private ranges goes through the VPC, the rest to the internet"
            },
            if v.network_tags.is_empty() {
                String::new()
            } else {
                format!(", network tags {}", v.network_tags.join(", "))
            }
        ));
    }
    for c in &s.cloud_sql {
        network.push(format!(
            "Cloud SQL `{c}`: a socket at `/cloudsql/{c}`; the runtime account gets `roles/cloudsql.client` in the instance's project."
        ));
    }
    if !s.custom_audiences.is_empty() {
        network.push(format!(
            "ID tokens for {} are accepted besides the `run.app` URL.",
            s.custom_audiences.join(", ")
        ));
    }
    network.extend(domain_lines(d));
    if !network.is_empty() {
        out.push(Section {
            title: "Networking".into(),
            items: network,
        });
    }
    if s.sandbox {
        out.push(Section {
            title: "Sandboxes".into(),
            items: vec![
                "The app can run untrusted code with `/usr/local/gcp/bin/sandbox` (Cloud Run sandboxes, preview). Sandboxes share the app container's CPU and memory, cannot read its environment variables, secrets or the metadata server, and have no network access unless started with `--allow-egress`.".into(),
            ],
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
    explanation_with(sections, crate::style::out())
}

/// Without colors the text is Markdown (`code` spans, `-` lists); with
/// colors, code spans are cyan instead of quoted and list numbers dim.
fn explanation_with(sections: &[Section], p: Painter) -> String {
    let mut s = String::new();
    for sec in sections {
        s.push_str(&format!("{}\n", p.bold(&sec.title)));
        for it in &sec.items {
            let numbered = it
                .split_once(". ")
                .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            match numbered {
                Some((n, rest)) => s.push_str(&format!(
                    "  {} {}\n",
                    p.dim(&format!("{n}.")),
                    code_spans(p, rest)
                )),
                None => s.push_str(&format!("  {} {}\n", p.dim("-"), code_spans(p, it))),
            }
        }
        s.push('\n');
    }
    s
}

/// `code` spans in cyan, without their backticks, when colors are on.
fn code_spans(p: Painter, s: &str) -> String {
    if !p.enabled || !s.matches('`').count().is_multiple_of(2) {
        return s.to_string();
    }
    s.split('`')
        .enumerate()
        .map(|(i, part)| {
            if i % 2 == 1 {
                p.cyan(part)
            } else {
                part.to_string()
            }
        })
        .collect()
}

/// What a job runs, how, and what schedules run it.
pub fn explain_job(d: &Deployment, r: &crate::config::Resolved) -> Vec<Section> {
    let crate::config::WorkloadKind::Job(j) = &d.kind else {
        return Vec::new();
    };
    let s = &d.service;
    let mut items = vec![
        format!(
            "Cloud Run job `{}` in {}/{}: {} task(s) per execution{}, up to {} retries each, {}s per task.",
            d.service_id,
            d.project,
            d.region,
            j.tasks,
            match j.parallelism {
                0 => String::new(),
                n => format!(", {n} at a time"),
            },
            j.max_retries,
            s.timeout_seconds
        ),
        match &d.artifact {
            crate::config::Artifact::Image { reference, .. } => format!("Image `{reference}`."),
            crate::config::Artifact::Build(b) => {
                format!("Built from `{}` ({}).", b.context_dir.display(), b.strategy)
            }
            crate::config::Artifact::Promote(p) => {
                format!("Promoted from stage `{}` (no build).", p.from)
            }
        },
        format!("Runs as `{}`.", s.service_account),
    ];
    if !s.command.is_empty() || !s.args.is_empty() {
        items.push(format!(
            "Command: `{}`.",
            crate::plan::args_display(&[s.command.clone(), s.args.clone()].concat())
        ));
    }
    for sch in r
        .schedules
        .iter()
        .filter(|x| x.target.resource_id() == d.service_id)
    {
        items.push(format!(
            "Run by schedule `{}`: `{}` ({}).",
            sch.key, sch.schedule, sch.time_zone
        ));
    }
    let mut out = vec![Section {
        title: format!("Job {}", d.name()),
        items,
    }];
    out.extend(explain(d).into_iter().filter(|sec| {
        ["Identity", "Secrets", "Storage", "Networking", "Sandboxes"].contains(&sec.title.as_str())
    }));
    out
}

/// The schedules of the stage and the account they call targets with.
pub fn explain_schedules(r: &crate::config::Resolved) -> Vec<Section> {
    let Some(sc) = &r.scheduler else {
        return Vec::new();
    };
    let mut items: Vec<String> = r
        .schedules
        .iter()
        .map(|s| {
            let what = match &s.target {
                crate::config::ScheduleTarget::Job { name, .. } => format!("runs job `{name}`"),
                crate::config::ScheduleTarget::Service {
                    name, method, path, ..
                } => format!("calls `{method} {path}` on service `{name}` with an ID token"),
            };
            format!(
                "`{}` ({}, {}{}) {what}.",
                s.key,
                s.schedule,
                s.time_zone,
                if s.paused { ", paused" } else { "" }
            )
        })
        .collect();
    items.push(format!(
        "Cloud Scheduler ({}) calls as `{}`{}, which gets `roles/run.invoker` on each target only.",
        sc.region,
        sc.service_account,
        if sc.create {
            " (created by runway)"
        } else {
            ""
        }
    ));
    vec![Section {
        title: "Schedules".into(),
        items,
    }]
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
            .deployments[0]
            .clone();
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

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    const COLOR: Painter = Painter { enabled: true };

    #[test]
    fn colored_diagram_keeps_the_plain_layout() {
        let (_d, d) = deployment();
        let colored = ascii_with(&d, COLOR);
        assert_ne!(colored, ascii(&d), "colors are applied");
        assert_eq!(strip_ansi(&colored), ascii(&d), "boxes stay aligned");
        assert!(
            colored.contains(&COLOR.bold_cyan("gcptree-prod")),
            "{colored}"
        );
        assert!(colored.contains(&COLOR.yellow("roles/bigquery.dataViewer")));
        assert!(colored.contains(&COLOR.cyan("dataset billing-data-1234.billingdata")));
        assert!(colored.contains(&COLOR.dim("runs as:")));
    }

    #[test]
    fn explanation_is_markdown_without_colors_and_highlighted_with() {
        let (_d, d) = deployment();
        let sections = explain(&d);
        let plain = explanation_with(&sections, PLAIN);
        assert!(plain.contains("  - `GCPTREE_TABLE` = `billing-data-1234.billingdata.t`"));
        assert!(plain.contains("  1. "), "numbered steps");
        let colored = explanation_with(&sections, COLOR);
        assert!(colored.contains(&COLOR.cyan("GCPTREE_TABLE")), "{colored}");
        assert!(
            !strip_ansi(&colored).contains('`'),
            "no backticks with colors"
        );
        assert_eq!(strip_ansi(&colored), plain.replace('`', ""));
    }

    #[test]
    fn unbalanced_backticks_are_left_alone() {
        assert_eq!(code_spans(COLOR, "a `b"), "a `b");
        assert_eq!(code_spans(PLAIN, "`a`"), "`a`");
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
