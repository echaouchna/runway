//! End-to-end tests of the CLI binary for commands that need no credentials.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::Path;

fn runway(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("runway").unwrap();
    c.current_dir(dir)
        .env_remove("RUNWAY_CONFIG")
        .env_remove("RUNWAY_STAGE")
        .env_remove("RUNWAY_COLOR")
        .env_remove("GOOGLE_CLOUD_PROJECT")
        .env_remove("CLOUDSDK_CORE_PROJECT")
        .env("NO_COLOR", "1");
    c
}

fn json(out: &[u8]) -> Value {
    serde_json::from_slice(out)
        .unwrap_or_else(|e| panic!("invalid JSON ({e}): {}", String::from_utf8_lossy(out)))
}

#[test]
fn help_lists_commands_and_exit_codes() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::starts_with(" _####  _##### \n"))
        .stdout(predicate::str::contains("deploy"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("Exit codes"))
        .stdout(predicate::str::contains("\x1b").not())
        .stderr(predicate::str::is_empty());
}

#[test]
fn branding_is_limited_to_top_level_help() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::starts_with(" _####  _##### \n"));
    runway(d.path())
        .args(["deploy", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("####").not());
    runway(d.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("runway {}\n", runway::VERSION));
}

#[test]
fn help_logo_colors_follow_flags_and_environment() {
    let d = tempfile::tempdir().unwrap();
    for args in [
        vec!["--color", "always", "--help"],
        vec!["--help", "--color=always"],
        vec!["-vh", "--color", "always"],
    ] {
        runway(d.path())
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("\x1b[48;2;48;48;239m"))
            .stdout(predicate::str::contains("\x1b[48;2;231;236;255m"))
            .stdout(predicate::str::contains("Usage:"))
            .stderr(predicate::str::is_empty());
    }
    runway(d.path())
        .env("RUNWAY_COLOR", "always")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[48;2;48;48;239m"));
    for args in [
        vec!["--color", "never", "--help"],
        vec!["--color", "always", "-o", "json", "--help"],
    ] {
        runway(d.path())
            .env("RUNWAY_COLOR", "always")
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("\x1b").not());
    }
}

#[test]
fn auto_colors_follow_ci_and_force_variables() {
    let d = tempfile::tempdir().unwrap();
    let plain = |d: &Path| {
        let mut c = runway(d);
        c.env_remove("NO_COLOR")
            .env_remove("FORCE_COLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("TERM");
        for ci in [
            "GITLAB_CI",
            "GITHUB_ACTIONS",
            "GITEA_ACTIONS",
            "FORGEJO_ACTIONS",
            "BUILDKITE",
            "CIRCLECI",
            "TF_BUILD",
        ] {
            c.env_remove(ci);
        }
        c
    };
    plain(d.path())
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b").not());
    for (name, value) in [
        ("GITLAB_CI", "true"),
        ("GITHUB_ACTIONS", "true"),
        ("FORCE_COLOR", "1"),
        ("CLICOLOR_FORCE", "1"),
    ] {
        plain(d.path())
            .env(name, value)
            .arg("--help")
            .assert()
            .success()
            .stdout(predicate::str::contains("\x1b[48;2;48;48;239m"));
    }
    plain(d.path())
        .env("GITLAB_CI", "true")
        .env("NO_COLOR", "1")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b").not());
}

#[test]
fn init_never_overwrites_and_generates_valid_config() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("main.py"), "# mine\n").unwrap();
    let out = runway(d.path())
        .args([
            "init",
            "--app",
            "demo",
            "--project",
            "my-demo-project",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report = json(&out);
    assert!(
        report["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap().ends_with("main.py"))
    );
    assert_eq!(
        std::fs::read_to_string(d.path().join("main.py")).unwrap(),
        "# mine\n"
    );
    assert!(d.path().join("Dockerfile").exists());

    // Second run skips everything.
    let out = runway(d.path())
        .args(["init", "-o", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(json(&out)["created"].as_array().unwrap().is_empty());

    runway(d.path())
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ stage dev: demo-dev"));
}

#[test]
fn validate_reports_errors_with_exit_code_3() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("runway.yaml"),
        "version: 1\napp: x1\nprovider: {project: my-gcp-project, region: europe-west1}\nservice:\n  image: nginx\n  memory: 10Gi\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\nstages: {dev: {}}\n",
    )
    .unwrap();
    let out = runway(d.path())
        .args(["validate", "-o", "json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["valid"], false);
    let errs = v["stages"]["dev"]["errors"].as_array().unwrap();
    assert!(errs.iter().any(|e| e["path"] == "service.memory"));
}

#[test]
fn unknown_field_is_a_config_error() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("runway.yaml"),
        "version: 1\napp: x1\nprovider: {}\nservice:\n  imagee: nginx\n",
    )
    .unwrap();
    runway(d.path())
        .arg("validate")
        .assert()
        .code(3)
        .stderr(predicate::str::contains("unknown field `imagee`"));
}

#[test]
fn offline_plan_for_source_build_is_not_exact() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .args(["init", "--project", "my-demo-project"])
        .assert()
        .success();
    let out = runway(d.path())
        .args(["plan", "--stage", "dev", "--offline", "-o", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let p = json(&out);
    assert_eq!(p["exact"], false);
    assert_eq!(p["remote_inspected"], false);
    assert_eq!(p["image"]["state"], "pending_build");
    assert_eq!(p["build"]["will_build"], Value::Null);
    assert!(
        p["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["field"] == "max_instances" && c["after"] == "2")
    );

    let text = runway(d.path())
        .args(["plan", "--stage", "prod", "--offline"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("digest NOT known yet"), "{text}");
    assert!(text.contains("NOT an exact preview"), "{text}");
}

#[test]
fn offline_plan_with_pinned_image_override() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .args(["init", "--project", "my-demo-project"])
        .assert()
        .success();
    let digest = format!("sha256:{}", "c".repeat(64));
    let out = runway(d.path())
        .args([
            "plan",
            "--stage",
            "dev",
            "--offline",
            "-o",
            "json",
            "--image",
        ])
        .arg(format!(
            "europe-west1-docker.pkg.dev/my-demo-project/apps/demo@{digest}"
        ))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let p = json(&out);
    assert_eq!(p["image"]["state"], "pinned");
    assert_eq!(p["image"]["digest"], digest);
    assert!(
        p.get("build").is_none(),
        "--image replaces the source build"
    );
}

#[test]
fn missing_stage_is_a_usage_error() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path()).args(["deploy"]).assert().code(2);
}

#[test]
fn unknown_stage_is_a_config_error() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .args(["init", "--project", "my-demo-project"])
        .assert()
        .success();
    runway(d.path())
        .args(["plan", "--stage", "qa", "--offline"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("defined stages: dev, prod"));
}

#[test]
fn missing_credentials_exit_with_code_4_and_json_error() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .args(["init", "--project", "my-demo-project"])
        .assert()
        .success();
    let out = runway(d.path())
        .env(
            "GOOGLE_APPLICATION_CREDENTIALS",
            d.path().join("missing.json"),
        )
        .args(["info", "--stage", "dev", "-o", "json"])
        .assert()
        .code(4)
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["error"]["kind"], "prerequisite");
    assert!(
        v["error"]["hints"][0]
            .as_str()
            .unwrap()
            .contains("application-default login")
    );
}

#[test]
fn bundled_examples_validate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for ex in [
        "examples/hello-python/runway.yaml",
        "examples/prebuilt-image/runway.yaml",
        "examples/gcptree/runway.yaml",
    ] {
        runway(root).args(["validate", "-c", ex]).assert().success();
    }
}

#[test]
fn describe_renders_ascii_and_mermaid_offline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = runway(root)
        .args(["describe", "-c", "examples/gcptree/runway.yaml"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("Cloud Run service gcptree-prod"), "{text}");
    assert!(text.contains("Deployment order"));
    assert!(text.contains("Identity-Aware Proxy"));

    let out = runway(root)
        .args([
            "describe",
            "-c",
            "examples/gcptree/runway.yaml",
            "--format",
            "mermaid",
            "-o",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["format"], "mermaid");
    assert!(v["diagram"].as_str().unwrap().starts_with("flowchart LR"));
    assert!(
        v["explanation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["title"] == "Deployment order")
    );

    // Several stages: --stage is required.
    runway(root)
        .args(["describe", "-c", "examples/hello-python/runway.yaml"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("pass --stage"));
}

#[test]
fn release_tags_need_a_changelog_and_are_exclusive() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .args(["init", "--project", "my-demo-project"])
        .assert()
        .success();
    runway(d.path())
        .env(
            "GOOGLE_APPLICATION_CREDENTIALS",
            d.path().join("missing.json"),
        )
        .args(["deploy", "--stage", "dev", "--tag"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("needs a changelog"));
    runway(d.path())
        .args(["deploy", "--stage", "dev", "--tag", "--tag-rc"])
        .assert()
        .code(2);
}

#[test]
fn configuration_file_can_have_any_name_or_location() {
    let d = tempfile::tempdir().unwrap();
    let yaml = "version: 1\napp: demo\nprovider: {project: my-demo-project, region: europe-west1}\nservice: {image: nginx:1.27, service_account: rt@my-demo-project.iam.gserviceaccount.com}\nstages: {dev: {}}\n";
    // Default lookup falls back to runway.yml.
    std::fs::write(d.path().join("runway.yml"), yaml).unwrap();
    runway(d.path())
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("runway.yml is valid"));
    // runway.yaml wins when both exist.
    std::fs::write(d.path().join("runway.yaml"), yaml.replace("demo", "other")).unwrap();
    runway(d.path())
        .arg("validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("runway.yaml is valid"));
    // Any file name, with --config or its alias --file, or through RUNWAY_CONFIG.
    std::fs::create_dir(d.path().join("deploy")).unwrap();
    std::fs::write(d.path().join("deploy/api.yml"), yaml).unwrap();
    for args in [["--config", "deploy/api.yml"], ["--file", "deploy/api.yml"]] {
        runway(d.path())
            .args(args)
            .arg("validate")
            .assert()
            .success()
            .stdout(predicate::str::contains("api.yml is valid"));
    }
    runway(d.path())
        .env("RUNWAY_CONFIG", "deploy/api.yml")
        .arg("validate")
        .assert()
        .success();
    // A directory: its runway.yaml / runway.yml.
    std::fs::write(d.path().join("deploy/runway.yml"), yaml).unwrap();
    runway(d.path())
        .args(["-c", "deploy", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("runway.yml is valid"));
    // A missing explicit file is reported by name.
    runway(d.path())
        .args(["-c", "nope.yaml", "validate"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("nope.yaml"));
}

#[test]
fn init_writes_the_configuration_under_the_given_name() {
    let d = tempfile::tempdir().unwrap();
    runway(d.path())
        .args([
            "-c",
            "service.yml",
            "init",
            "--app",
            "demo",
            "--project",
            "my-demo-project",
            "--image",
            "nginx:1.27",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("runway -c service.yml deploy"));
    assert!(d.path().join("service.yml").is_file());
    assert!(!d.path().join("runway.yaml").exists());
    runway(d.path())
        .args(["-c", "service.yml", "validate"])
        .assert()
        .success();
    // Without --config, only runway.yaml / runway.yml are looked up.
    runway(d.path())
        .arg("validate")
        .assert()
        .code(3)
        .stderr(predicate::str::contains("runway.yml"));
}
