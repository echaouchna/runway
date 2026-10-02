//! `runway init`: scaffold a configuration and a small runnable example.
//! Existing files are never overwritten.

use crate::cli::{Context, InitArgs};
use crate::config::validate;
use crate::error::{Error, Result};
use crate::image_ref::ImageRef;
use crate::output::{OutputFormat, print_json};
use serde::Serialize;
use std::path::Path;

pub const EXAMPLE_MAIN_PY: &str = include_str!("../../examples/hello-python/main.py");

pub const EXAMPLE_DOCKERFILE: &str = include_str!("../../examples/hello-python/Dockerfile");

pub const EXAMPLE_DOCKERIGNORE: &str = include_str!("../../examples/hello-python/.dockerignore");

fn source_config(app: &str, project: &str, region: &str) -> String {
    format!(
        r#"# runway.yaml - every option: https://echaouchna.github.io/runway/docs/configuration/
version: 1
app: {app}

# Infrastructure you provide (see README "Prerequisites"). runway never
# creates or deletes these resources.
provider:
  project: {project}
  region: {region}
  artifact_repository: runway
  source_bucket: {project}-runway-sources
  build_service_account: runway-build@{project}.iam.gserviceaccount.com

# The Cloud Run service runway manages: one per stage, named <app>-<stage>.
service:
  source: .
  dockerfile: Dockerfile
  port: 8080
  cpu: "1"
  memory: 512Mi
  timeout_seconds: 60
  concurrency: 80
  min_instances: 0
  max_instances: 10
  public: false
  service_account: runway-runtime@{project}.iam.gserviceaccount.com
  env:
    LOG_LEVEL: info
  # Secret Manager references (values are never read by runway):
  # secrets:
  #   DATABASE_URL:
  #     secret: database-url
  #     version: "1"

# Stage overrides take precedence over the blocks above.
stages:
  dev:
    service:
      max_instances: 2
  prod:
    service:
      min_instances: 1
"#
    )
}

fn image_config(app: &str, project: &str, region: &str, image: &str) -> String {
    format!(
        r#"# runway.yaml - every option: https://echaouchna.github.io/runway/docs/configuration/
version: 1
app: {app}

provider:
  project: {project}
  region: {region}

service:
  image: {image}
  port: 8080
  memory: 512Mi
  max_instances: 10
  public: false
  service_account: runway-runtime@{project}.iam.gserviceaccount.com
  env:
    LOG_LEVEL: info

stages:
  dev:
    service:
      max_instances: 2
  prod:
    service:
      min_instances: 1
"#
    )
}

/// Turns a directory name into a valid app name.
pub fn sanitize_app_name(raw: &str) -> String {
    let mut s: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s
        .trim_matches(|c: char| c == '-' || c.is_ascii_digit())
        .to_string();
    let mut s: String = s.chars().take(30).collect();
    while s.ends_with('-') {
        s.pop();
    }
    if validate::name_component("app", &s, 40).is_ok() {
        s
    } else {
        "my-app".into()
    }
}

#[derive(Serialize)]
struct InitReport {
    created: Vec<String>,
    skipped: Vec<String>,
    app: String,
    project: String,
}

pub fn run(ctx: &Context, args: InitArgs) -> Result<()> {
    std::fs::create_dir_all(&args.dir)?;
    let dir_name = std::fs::canonicalize(&args.dir)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "my-app".into());
    let app = match args.app {
        Some(a) => {
            validate::name_component("app", &a, 40).map_err(Error::config)?;
            a
        }
        None => sanitize_app_name(&dir_name),
    };
    let project = args
        .project
        .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT").ok())
        .or_else(|| std::env::var("CLOUDSDK_CORE_PROJECT").ok())
        .unwrap_or_else(|| "my-gcp-project".into());
    validate::project_id(&project).map_err(|e| Error::config(format!("--project: {e}")))?;
    validate::region(&args.region).map_err(|e| Error::config(format!("--region: {e}")))?;

    // `runway --config api.yaml init` writes api.yaml.
    let config_name = ctx.config_name.as_deref().unwrap_or("runway.yaml");
    let mut files: Vec<(&str, String)> = Vec::new();
    let has_dockerfile = args.dir.join("Dockerfile").exists();
    match &args.image {
        Some(image) => {
            ImageRef::parse(image).map_err(|e| Error::config(format!("--image: {e}")))?;
            files.push((
                config_name,
                image_config(&app, &project, &args.region, image),
            ));
        }
        None => {
            files.push((config_name, source_config(&app, &project, &args.region)));
            if !has_dockerfile {
                files.push(("main.py", EXAMPLE_MAIN_PY.to_string()));
                files.push(("Dockerfile", EXAMPLE_DOCKERFILE.to_string()));
                files.push((".dockerignore", EXAMPLE_DOCKERIGNORE.to_string()));
            }
        }
    }

    let mut report = InitReport {
        created: vec![],
        skipped: vec![],
        app: app.clone(),
        project: project.clone(),
    };
    for (name, content) in files {
        let path = args.dir.join(name);
        if write_new(&path, &content)? {
            report.created.push(path.display().to_string());
        } else {
            report.skipped.push(path.display().to_string());
        }
    }

    match ctx.output {
        OutputFormat::Json => print_json(&report),
        OutputFormat::Text => {
            for c in &report.created {
                println!("{}  {c}", crate::style::out().green("created"));
            }
            for s in &report.skipped {
                println!(
                    "{}  {s} (already exists)",
                    crate::style::out().yellow("skipped")
                );
            }
            if has_dockerfile && args.image.is_none() {
                println!("found an existing Dockerfile; no example application generated");
            }
            println!();
            println!("Next steps:");
            let wrote_config = report.created.iter().any(|c| c.ends_with(config_name));
            if wrote_config && project == "my-gcp-project" {
                println!("  1. Set provider.project in {config_name} (or re-run with --project)");
            } else {
                println!("  1. Review {config_name}");
            }
            println!(
                "  2. Create the prerequisites (or set `provider.create_build_resources: true`): https://echaouchna.github.io/runway/docs/getting-started/#prerequisites"
            );
            let flag = if config_name == "runway.yaml" {
                String::new()
            } else {
                format!(" -c {config_name}")
            };
            println!("  3. runway{flag} validate && runway{flag} doctor --stage dev");
            println!("  4. runway{flag} deploy --stage dev");
        }
    }
    Ok(())
}

/// Writes `content` only if `path` does not exist. Returns whether it was written.
fn write_new(path: &Path, content: &str) -> Result<bool> {
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            f.write_all(content.as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_app_name("My_Cool App"), "my-cool-app");
        assert_eq!(sanitize_app_name("123"), "my-app");
        assert_eq!(sanitize_app_name("2048-game"), "game");
        assert_eq!(sanitize_app_name("runway"), "runway");
    }

    #[test]
    fn generated_configs_are_valid() {
        for (yaml, needs_dockerfile) in [
            (
                source_config("hello", "my-gcp-project", "europe-west1"),
                true,
            ),
            (
                image_config("hello", "my-gcp-project", "europe-west1", "nginx:1.27"),
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            if needs_dockerfile {
                std::fs::write(dir.path().join("Dockerfile"), EXAMPLE_DOCKERFILE).unwrap();
            }
            let path = dir.path().join("runway.yaml");
            std::fs::write(&path, yaml).unwrap();
            let cfg = crate::config::load(&path).unwrap();
            for stage in ["dev", "prod"] {
                crate::config::resolve(&cfg, stage, &Default::default())
                    .unwrap_or_else(|d| panic!("{:#?}", d.errors));
            }
        }
    }
}
