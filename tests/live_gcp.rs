//! Opt-in live test against a disposable GCP project. It CREATES BILLABLE
//! RESOURCES (Cloud Run services; for the source test also a Cloud Build run,
//! an Artifact Registry image and a Cloud Storage object).
//!
//! It is `#[ignore]`d and additionally requires explicit environment variables:
//!
//! ```sh
//! export RUNWAY_LIVE_CONFIRM=create-billable-resources
//! export RUNWAY_LIVE_PROJECT=my-disposable-project
//! export RUNWAY_LIVE_REGION=europe-west1
//! export RUNWAY_LIVE_RUNTIME_SA=runway-runtime@my-disposable-project.iam.gserviceaccount.com
//! # Optional, enables the source-build test:
//! export RUNWAY_LIVE_REPOSITORY=runway
//! export RUNWAY_LIVE_BUCKET=my-disposable-project-runway-sources
//! export RUNWAY_LIVE_BUILD_SA=runway-build@my-disposable-project.iam.gserviceaccount.com
//! cargo test --test live_gcp -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Services created by the test are deleted at the end (also on failure).

use assert_cmd::Command;
use google_cloud_lro::Poller;
use serde_json::Value;
use std::path::Path;

struct Live {
    project: String,
    region: String,
    runtime_sa: String,
}

fn live() -> Option<Live> {
    if std::env::var("RUNWAY_LIVE_CONFIRM").ok().as_deref() != Some("create-billable-resources") {
        eprintln!(
            "skipping: set RUNWAY_LIVE_CONFIRM=create-billable-resources (see tests/live_gcp.rs)"
        );
        return None;
    }
    let get =
        |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the live test"));
    Some(Live {
        project: get("RUNWAY_LIVE_PROJECT"),
        region: get("RUNWAY_LIVE_REGION"),
        runtime_sa: get("RUNWAY_LIVE_RUNTIME_SA"),
    })
}

fn unique_app(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        % 1_000_000;
    format!("{prefix}{n}")
}

fn runway(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("runway").unwrap();
    c.current_dir(dir).env("NO_COLOR", "1");
    c
}

fn run_json(dir: &Path, args: &[&str]) -> Value {
    let out = runway(dir)
        .args(args)
        .args(["-o", "json"])
        .output()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "runway {args:?} failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Deletes the service when dropped (best effort, also when the test panics).
struct Cleanup {
    name: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let name = self.name.clone();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            match google_cloud_run_v2::client::Services::builder()
                .build()
                .await
            {
                Ok(c) => match c
                    .delete_service()
                    .set_name(&name)
                    .poller()
                    .until_done()
                    .await
                {
                    Ok(_) => eprintln!("deleted {name}"),
                    Err(e) => eprintln!("WARNING: could not delete {name}: {e}"),
                },
                Err(e) => eprintln!("WARNING: could not create client to delete {name}: {e}"),
            }
        });
    }
}

#[test]
#[ignore = "creates billable resources; see module docs"]
fn live_image_deploy_converges() {
    let Some(l) = live() else { return };
    let app = unique_app("rwimg");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("runway.yaml"),
        format!(
            "version: 1\napp: {app}\nprovider:\n  project: {}\n  region: {}\nservice:\n  image: us-docker.pkg.dev/cloudrun/container/hello:latest\n  max_instances: 1\n  service_account: {}\n  env:\n    LOG_LEVEL: debug\nstages:\n  test: {{}}\n",
            l.project, l.region, l.runtime_sa
        ),
    )
    .unwrap();
    let _cleanup = Cleanup {
        name: format!(
            "projects/{}/locations/{}/services/{app}-test",
            l.project, l.region
        ),
    };

    runway(dir.path())
        .args(["doctor", "--stage", "test"])
        .assert()
        .success();

    let plan = run_json(dir.path(), &["plan", "--stage", "test"]);
    assert_eq!(plan["action"], "create");
    assert_eq!(
        plan["image"]["state"], "pinned",
        "public image tag resolves to a digest"
    );

    let first = run_json(dir.path(), &["deploy", "--stage", "test"]);
    assert_eq!(first["change"], "created");
    assert!(first["url"].as_str().unwrap().starts_with("https://"));
    assert!(first["image"].as_str().unwrap().contains("@sha256:"));

    let second = run_json(dir.path(), &["deploy", "--stage", "test"]);
    assert_eq!(second["change"], "unchanged", "repeated deploys converge");

    let plan = run_json(dir.path(), &["plan", "--stage", "test"]);
    assert_eq!(plan["action"], "no_change");
    assert_eq!(plan["exact"], true);

    let info = run_json(dir.path(), &["info", "--stage", "test"]);
    assert_eq!(info["readiness"]["state"], "ready");
    assert_eq!(info["public"], false, "private by default");

    // Logs may take a moment to arrive; the command itself must succeed.
    run_json(dir.path(), &["logs", "--stage", "test", "--since", "15m"]);
}

#[test]
#[ignore = "creates billable resources; see module docs"]
fn live_source_deploy_builds_and_reuses_image() {
    let Some(l) = live() else { return };
    let (Ok(repo), Ok(bucket), Ok(build_sa)) = (
        std::env::var("RUNWAY_LIVE_REPOSITORY"),
        std::env::var("RUNWAY_LIVE_BUCKET"),
        std::env::var("RUNWAY_LIVE_BUILD_SA"),
    ) else {
        eprintln!("skipping source test: RUNWAY_LIVE_REPOSITORY/BUCKET/BUILD_SA not set");
        return;
    };
    let app = unique_app("rwsrc");
    let dir = tempfile::tempdir().unwrap();
    let ex = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello-python");
    for f in ["main.py", "Dockerfile", ".dockerignore"] {
        std::fs::copy(ex.join(f), dir.path().join(f)).unwrap();
    }
    std::fs::write(
        dir.path().join("runway.yaml"),
        format!(
            "version: 1\napp: {app}\nprovider:\n  project: {p}\n  region: {r}\n  artifact_repository: {repo}\n  source_bucket: {bucket}\n  build_service_account: {build_sa}\nservice:\n  source: .\n  max_instances: 1\n  service_account: {sa}\nstages:\n  test: {{}}\n",
            p = l.project,
            r = l.region,
            sa = l.runtime_sa
        ),
    )
    .unwrap();
    let _cleanup = Cleanup {
        name: format!(
            "projects/{}/locations/{}/services/{app}-test",
            l.project, l.region
        ),
    };

    runway(dir.path())
        .args(["doctor", "--stage", "test"])
        .assert()
        .success();
    let plan = run_json(dir.path(), &["plan", "--stage", "test"]);
    assert_eq!(plan["exact"], false, "digest unknown before the build");

    let first = run_json(dir.path(), &["deploy", "--stage", "test"]);
    assert_eq!(first["change"], "created");
    assert_eq!(first["build"]["reused"], false);

    // Config-only change: same source hash, image reused, new revision.
    let cfg = std::fs::read_to_string(dir.path().join("runway.yaml")).unwrap();
    std::fs::write(
        dir.path().join("runway.yaml"),
        cfg.replace("max_instances: 1", "max_instances: 2"),
    )
    .unwrap();
    let second = run_json(dir.path(), &["deploy", "--stage", "test"]);
    assert_eq!(second["change"], "updated");
    assert!(
        second.get("build").is_none(),
        "existing image reused without a build"
    );
}
