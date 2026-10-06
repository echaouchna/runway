use super::*;
use std::fs;

const FULL: &str = r#"
version: 1
app: hello-api

provider:
  project: my-gcp-project
  region: europe-west1
  artifact_repository: applications
  source_bucket: my-gcp-build-sources
  build_service_account: builds@my-gcp-project.iam.gserviceaccount.com

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
  service_account: runtime@my-gcp-project.iam.gserviceaccount.com

  env:
    LOG_LEVEL: info

  secrets:
    DATABASE_URL:
      secret: database-url
      version: "1"

stages:
  dev:
    service:
      max_instances: 2
  prod:
    service:
      min_instances: 1
"#;

/// Writes `yaml` as runway.yaml (plus a Dockerfile) into a temp dir and loads it.
fn load_str(yaml: &str) -> (tempfile::TempDir, LoadedConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let path = dir.path().join("runway.yaml");
    fs::write(&path, yaml).unwrap();
    let cfg = load(&path).unwrap();
    (dir, cfg)
}

fn resolve_ok(cfg: &LoadedConfig, stage: &str) -> Resolved {
    match resolve(cfg, stage, &Overrides::default()) {
        Ok(r) => r,
        Err(d) => panic!("unexpected errors: {:#?}", d.errors),
    }
}

fn resolve_err(cfg: &LoadedConfig, stage: &str) -> Vec<Issue> {
    match resolve(cfg, stage, &Overrides::default()) {
        Ok(_) => panic!("expected validation errors"),
        Err(d) => d.errors,
    }
}

fn has_error(errors: &[Issue], path: &str, needle: &str) -> bool {
    errors
        .iter()
        .any(|e| e.path == path && e.message.contains(needle))
}

#[test]
fn full_example_resolves_with_stage_overrides() {
    let (_d, cfg) = load_str(FULL);
    let dev = resolve_ok(&cfg, "dev").deployment;
    assert_eq!(dev.service_id, "hello-api-dev");
    assert_eq!(dev.service.max_instances, 2);
    assert_eq!(dev.service.min_instances, 0);
    assert_eq!(dev.service.timeout_seconds, 60);
    assert_eq!(dev.service.env["LOG_LEVEL"], "info");
    assert_eq!(
        dev.service.secrets["DATABASE_URL"],
        SecretRef {
            secret: "database-url".into(),
            version: "1".into(),
            ..Default::default()
        }
    );
    match &dev.artifact {
        Artifact::Build(b) => {
            assert_eq!(
                b.strategy,
                BuildStrategy::Dockerfile {
                    path: "Dockerfile".into()
                }
            );
            assert_eq!(b.artifact_location, "europe-west1");
            assert_eq!(
                b.excluded,
                ["runway.yaml"],
                "config file is kept out of the build context"
            );
        }
        other => panic!("expected build, got {other:?}"),
    }

    let prod = resolve_ok(&cfg, "prod");
    assert_eq!(prod.deployment.service.min_instances, 1);
    assert_eq!(prod.deployment.service.max_instances, 10);
    assert!(
        prod.warnings
            .iter()
            .any(|w| w.path == "stages.prod.service.min_instances")
    );
}

#[test]
fn defaults_apply_when_fields_are_omitted() {
    let (_d, cfg) = load_str(
        r#"
version: 1
app: tiny
provider: { project: my-gcp-project, region: us-central1 }
service:
  image: us-docker.pkg.dev/cloudrun/container/hello
  service_account: rt@my-gcp-project.iam.gserviceaccount.com
stages: { dev: }
"#,
    );
    let s = resolve_ok(&cfg, "dev").deployment.service;
    assert_eq!(s.port, DEFAULT_PORT);
    assert_eq!(s.cpu, "1");
    assert_eq!(s.memory, "512Mi");
    assert_eq!(s.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
    assert_eq!(s.concurrency, DEFAULT_CONCURRENCY);
    assert_eq!(
        (s.min_instances, s.max_instances),
        (0, DEFAULT_MAX_INSTANCES)
    );
    assert!(!s.public, "services are private by default");
}

#[test]
fn env_and_secret_maps_merge_and_null_removes() {
    let (_d, cfg) = load_str(
        r#"
version: 1
app: app
provider: { project: my-gcp-project, region: us-central1 }
service:
  image: nginx:1.27
  service_account: rt@my-gcp-project.iam.gserviceaccount.com
  env: { A: one, B: two, DEBUG: false }
  secrets:
    S1: { secret: s1, version: 1 }
    S2: { secret: s2, version: "2" }
stages:
  prod:
    provider: { project: my-prod-project }
    service:
      cpu: 2
      env: { B: override, A: null, NEW: 3 }
      secrets: { S2: null }
"#,
    );
    let d = resolve_ok(&cfg, "prod").deployment;
    assert_eq!(
        d.project, "my-prod-project",
        "stage provider overrides base provider"
    );
    assert_eq!(d.service.cpu, "2", "numeric cpu accepted");
    let env: Vec<_> = d
        .service
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    assert_eq!(env, ["B=override", "DEBUG=false", "NEW=3"]);
    assert_eq!(d.service.secrets.len(), 1);
    assert_eq!(d.service.secrets["S1"].version, "1");
}

#[test]
fn unknown_fields_are_rejected_with_location() {
    let err = parse(
        r#"
version: 1
app: x
provider: { project: my-gcp-project, region: us-central1 }
service:
  max_instance: 3
"#,
    )
    .unwrap_err();
    assert!(err.contains("max_instance"), "{err}");
    assert!(err.contains("unknown field"), "{err}");

    let err = parse("version: 1\napp: x\nprovider: {}\nextra: true\n").unwrap_err();
    assert!(err.contains("extra"), "{err}");
}

#[test]
fn invalid_values_are_reported_together() {
    let (_d, cfg) = load_str(
        r#"
version: 2
app: Hello_API
provider: { project: P, region: europe-west1-b }
service:
  image: nginx
  port: 70000
  cpu: "3"
  memory: 64Mi
  timeout_seconds: 0
  concurrency: 5000
  min_instances: 5
  max_instances: 2
  service_account: not-an-email
  env: { PORT: "1", "BAD-NAME": x }
  secrets:
    TOKEN: { secret: "bad/name", version: v1 }
stages: { dev: {} }
"#,
    );
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(&e, "version", "unsupported schema version"));
    assert!(has_error(&e, "app", "lowercase"));
    assert!(has_error(&e, "provider.project", "not a valid project ID"));
    assert!(has_error(&e, "provider.region", "not a valid region"));
    assert!(has_error(&e, "service.port", "between 1 and 65535"));
    assert!(has_error(&e, "service.cpu", "not supported"));
    assert!(has_error(&e, "service.memory", "between 128Mi and 32Gi"));
    assert!(has_error(
        &e,
        "service.timeout_seconds",
        "between 1 and 3600"
    ));
    assert!(has_error(&e, "service.concurrency", "between 1 and 1000"));
    assert!(has_error(&e, "service.min_instances", "must not exceed"));
    assert!(has_error(
        &e,
        "service.service_account",
        "not a service account"
    ));
    assert!(has_error(&e, "service.env.PORT", "service.port"));
    assert!(has_error(
        &e,
        "service.env.BAD-NAME",
        "not a valid environment variable"
    ));
    assert!(has_error(
        &e,
        "service.secrets.TOKEN.secret",
        "not a valid secret"
    ));
    assert!(has_error(
        &e,
        "service.secrets.TOKEN.version",
        "not a valid secret version"
    ));
}

#[test]
fn image_and_source_conflict_in_same_block() {
    let (_d, cfg) = load_str(
        r#"
version: 1
app: app
provider: { project: my-gcp-project, region: us-central1 }
service:
  image: nginx
  source: .
  service_account: rt@my-gcp-project.iam.gserviceaccount.com
stages: { dev: {} }
"#,
    );
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "service.image", "cannot be combined"),
        "{e:#?}"
    );
}

#[test]
fn stage_image_replaces_base_build_and_cli_image_wins() {
    let (_d, cfg) = load_str(
        FULL.replace(
            "  prod:\n    service:\n      min_instances: 1",
            "  prod:\n    service:\n      image: europe-west1-docker.pkg.dev/p/apps/hello:1.0",
        )
        .as_str(),
    );
    let prod = resolve_ok(&cfg, "prod").deployment;
    assert!(
        matches!(prod.artifact, Artifact::Image { ref reference, .. } if reference.ends_with("hello:1.0"))
    );
    let dev = resolve_ok(&cfg, "dev").deployment;
    assert!(matches!(dev.artifact, Artifact::Build(_)));

    let o = Overrides {
        image: Some("nginx@sha256:".to_string() + &"b".repeat(64)),
    };
    let dev = resolve(&cfg, "dev", &o).unwrap().deployment;
    assert!(matches!(dev.artifact, Artifact::Image { ref parsed, .. } if parsed.digest.is_some()));
}

#[test]
fn source_builds_require_build_infrastructure_and_dockerfile() {
    let (dir, cfg) = load_str(
        r#"
version: 1
app: app
provider: { project: my-gcp-project, region: us-central1 }
service:
  source: .
  dockerfile: docker/Dockerfile.prod
  service_account: rt@my-gcp-project.iam.gserviceaccount.com
stages: { dev: {} }
"#,
    );
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(
        &e,
        "provider.artifact_repository",
        "required for source builds"
    ));
    assert!(has_error(
        &e,
        "provider.source_bucket",
        "required for source builds"
    ));
    assert!(has_error(
        &e,
        "provider.build_service_account",
        "required for source builds"
    ));
    assert!(has_error(&e, "service.dockerfile", "not found"));
    drop(dir);
}

#[test]
fn dockerfile_must_stay_inside_context() {
    let (_d, cfg) = load_str(&FULL.replace("dockerfile: Dockerfile", "dockerfile: ../Dockerfile"));
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(
        &e,
        "service.dockerfile",
        "inside the build context"
    ));
}

#[test]
fn unknown_stage_lists_defined_stages() {
    let (_d, cfg) = load_str(FULL);
    let e = resolve_err(&cfg, "staging");
    assert!(
        has_error(&e, "stages", "defined stages: dev, prod"),
        "{e:#?}"
    );
}

#[test]
fn runtime_service_account_is_required() {
    let (_d, cfg) = load_str(&FULL.replace(
        "  service_account: runtime@my-gcp-project.iam.gserviceaccount.com\n",
        "",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(&e, "service.service_account", "required"));
}

#[test]
fn service_name_length_is_bounded() {
    let long_app = "a".repeat(40);
    let (_d, cfg) = load_str(
        &FULL
            .replace("app: hello-api", &format!("app: {long_app}"))
            .replace("  dev:\n", "  development-us:\n"),
    );
    let e = resolve_err(&cfg, "development-us");
    assert!(has_error(&e, "app", "exceeds 49 characters"), "{e:#?}");
}

#[test]
fn env_and_secret_name_collision() {
    let (_d, cfg) = load_str(&FULL.replace("LOG_LEVEL: info", "DATABASE_URL: plain"));
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(
        &e,
        "service.secrets.DATABASE_URL",
        "both in env and secrets"
    ));
}

#[test]
fn warnings_for_sensitive_env_and_latest_secret() {
    let (_d, cfg) = load_str(
        &FULL
            .replace("LOG_LEVEL: info", "API_TOKEN: abc")
            .replace("version: \"1\"", "version: latest"),
    );
    let r = resolve_ok(&cfg, "dev");
    assert!(r.warnings.iter().any(|w| w.path == "service.env.API_TOKEN"));
    assert!(
        r.warnings
            .iter()
            .any(|w| w.path == "service.secrets.DATABASE_URL.version")
    );
}

#[test]
fn missing_config_file_hints_init() {
    let err = load(Path::new("/definitely/not/here/runway.yaml")).unwrap_err();
    assert_eq!(err.kind, crate::error::ErrorKind::Config);
    assert!(err.hints.iter().any(|h| h.contains("runway init")));
}
#[test]
fn parse_errors_are_user_friendly() {
    let e = parse("version: 1\napp: x\nprovider: {}\nservice:\n  port: abc\n").unwrap_err();
    assert!(e.contains("expected an integer"), "{e}");
    let e = parse("version: 1\napp: x\napp: y\nprovider: {}\n").unwrap_err();
    assert!(e.contains("duplicate mapping key: app"), "{e}");
    assert!(!e.contains("DuplicateKeyPolicy"), "{e}");
}

const INFRA: &str = r#"
version: 1
app: gcptree
provider: { project: my-gcp-project, region: europe-west1 }
service:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/gcptree:1
  service_account: gcptree-run@my-gcp-project.iam.gserviceaccount.com
  identity:
    create: true
    roles:
      - { role: roles/bigquery.dataViewer, dataset: "billing-data-1234:billingdata" }
      - { role: roles/secretmanager.secretAccessor, secret: api-key }
  tags: { "123456789012/env": dev, "123456789012/team": finops }
  volumes:
    cache: { bucket: my-cache, mount_path: /mnt/cache }
    assets: { bucket: my-assets, mount_path: /mnt/assets, read_only: true }
  iap: { members: [group:finops@example.com] }
retry: { attempts: 5, delay: 2s }
stages:
  dev: {}
  prod:
    service:
      tags: { "123456789012/env": prod, "123456789012/team": null }
      volumes: { assets: null }
      iap: { enabled: false }
      identity: { create: false }
"#;

#[test]
fn infrastructure_fields_resolve_with_stage_overrides() {
    let (_d, cfg) = load_str(INFRA);
    let dev = resolve_ok(&cfg, "dev").deployment;
    let s = &dev.service;
    assert!(s.identity.create);
    assert_eq!(s.identity.roles.len(), 2);
    assert_eq!(
        s.identity.roles[0].target,
        RoleTarget::Dataset {
            project: "billing-data-1234".into(),
            dataset: "billingdata".into()
        }
    );
    assert_eq!(
        s.identity.roles[1].target,
        RoleTarget::Secret {
            name: "projects/my-gcp-project/secrets/api-key".into()
        },
        "secret IDs resolve in the provider project"
    );
    assert_eq!(s.tags.len(), 2);
    assert!(s.volumes["assets"].read_only);
    assert!(!s.volumes["cache"].read_only, "read_only defaults to false");
    assert!(
        s.iap.enabled,
        "iap.enabled defaults to true when the block is present"
    );
    assert_eq!(dev.retry.attempts, 5);
    assert_eq!(dev.retry.delay, std::time::Duration::from_secs(2));
    assert_eq!(
        dev.retry.max_delay,
        std::time::Duration::from_secs(60),
        "default kept"
    );

    let prod = resolve_ok(&cfg, "prod").deployment;
    let s = &prod.service;
    assert_eq!(s.tags.len(), 1, "null removes an inherited tag");
    assert_eq!(s.tags["123456789012/env"], "prod");
    assert!(
        !s.volumes.contains_key("assets"),
        "null removes an inherited volume"
    );
    assert!(
        !s.iap.enabled && s.iap.members.is_empty(),
        "stage block replaces iap"
    );
    assert!(
        !s.identity.create && s.identity.roles.is_empty(),
        "stage block replaces identity"
    );
}

#[test]
fn retry_defaults_without_block() {
    let (_d, cfg) = load_str(FULL);
    let r = resolve_ok(&cfg, "dev").deployment.retry;
    assert_eq!(r, crate::retry::RetryConfig::default());
}

#[test]
fn duplicate_mount_paths_and_default_sa_creation_are_rejected() {
    let (_d, cfg) = load_str(
        &INFRA
            .replace("mount_path: /mnt/assets", "mount_path: /mnt/cache")
            .replace(
                "gcptree-run@my-gcp-project.iam.gserviceaccount.com",
                "123-compute@developer.gserviceaccount.com",
            ),
    );
    let e = resolve_err(&cfg, "dev");
    assert!(
        e.iter().any(|i| i.message.contains("mounted twice")),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.identity.create", "user-managed"),
        "{e:#?}"
    );
}

const VARS: &str = r#"
version: 1
app: gcptree
vars: { job_project: billing-data-1234, group: "group:finops@example.com" }
provider:
  project: my-gcp-project
  region: europe-west1
  enable_apis: true
  apis: [telemetry.googleapis.com]
buckets:
  cache: { name: "${project}-${app}-cache", delete_after_days: 30, labels: { team: finops } }
service:
  image: nginx@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  service_account: "${app}-run@${project}.iam.gserviceaccount.com"
  identity:
    roles:
      - { role: roles/bigquery.jobUser, project: "${vars.job_project}" }
      - { role: roles/storage.objectUser, bucket: "${buckets.cache}" }
  env:
    GCPTREE_CACHE_BUCKET: "gs://${buckets.cache}/${stage}"
    PRICE: "$$5"
  iap: { members: ["${vars.group}"] }
stages:
  prod: {}
  dev:
    vars: { job_project: billing-dev }
"#;

#[test]
fn variables_interpolate_everywhere_and_stages_override_them() {
    let (_d, cfg) = load_str(VARS);
    let prod = resolve_ok(&cfg, "prod").deployment;
    assert_eq!(
        prod.service.service_account,
        "gcptree-run@my-gcp-project.iam.gserviceaccount.com"
    );
    assert_eq!(prod.buckets["cache"].name, "my-gcp-project-gcptree-cache");
    assert_eq!(
        prod.buckets["cache"].location, "europe-west1",
        "bucket location defaults to the region"
    );
    assert_eq!(prod.buckets["cache"].delete_after_days, Some(30));
    assert_eq!(
        prod.service.env["GCPTREE_CACHE_BUCKET"],
        "gs://my-gcp-project-gcptree-cache/prod"
    );
    assert_eq!(prod.service.env["PRICE"], "$5", "$$ escapes a dollar");
    assert_eq!(prod.service.iap.members, ["group:finops@example.com"]);
    assert_eq!(
        prod.service.identity.roles[0].target,
        RoleTarget::Project {
            project: "billing-data-1234".into()
        }
    );
    assert_eq!(
        prod.service.identity.roles[1].target,
        RoleTarget::Bucket {
            bucket: "my-gcp-project-gcptree-cache".into()
        }
    );
    assert!(prod.apis.enable);
    assert_eq!(prod.apis.extra, ["telemetry.googleapis.com"]);

    let dev = resolve_ok(&cfg, "dev").deployment;
    assert_eq!(
        dev.service.identity.roles[0].target,
        RoleTarget::Project {
            project: "billing-dev".into()
        },
        "stage vars override top-level vars"
    );
    assert_eq!(
        dev.service.env["GCPTREE_CACHE_BUCKET"],
        "gs://my-gcp-project-gcptree-cache/dev"
    );
}

#[test]
fn interpolation_and_bucket_errors_are_reported() {
    let (_d, cfg) = load_str(
        &VARS
            .replace("${vars.job_project}", "${vars.missing}")
            .replace(
                "delete_after_days: 30",
                "delete_after_days: 0, location: \"moon base\"",
            )
            .replace("telemetry.googleapis.com", "telemetry")
            .replace("team: finops", "Team: x"),
    );
    let e = resolve_err(&cfg, "prod");
    assert!(
        has_error(
            &e,
            "service.identity.roles[0].project",
            "unknown variable `${vars.missing}`"
        ),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "buckets.cache.delete_after_days", "between 1 and 36500"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "buckets.cache.location", "not a bucket location"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "provider.apis[0]", "not an API service name"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "buckets.cache.labels.Team", "label key"),
        "{e:#?}"
    );
}

#[test]
fn build_resources_require_a_user_managed_build_account() {
    let (_d, cfg) = load_str(&FULL.replace(
        "  build_service_account: builds@my-gcp-project.iam.gserviceaccount.com",
        "  build_service_account: 123-compute@developer.gserviceaccount.com\n  create_build_resources: true",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "provider.build_service_account", "user-managed"),
        "{e:#?}"
    );
}

#[test]
fn buildpacks_are_used_transparently_without_a_dockerfile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), "{}").unwrap();
    let path = dir.path().join("runway.yaml");
    let yaml = r#"
version: 1
app: gcptree
provider: { project: my-gcp-project, region: europe-west1, create_build_resources: true }
service:
  source: .
  service_account: "gcptree-run@${project}.iam.gserviceaccount.com"
stages: { prod: {}, custom: { service: { builder: "gcr.io/buildpacks/builder:google-22" } } }
"#;
    std::fs::write(&path, yaml).unwrap();
    let cfg = load(&path).unwrap();
    let d = resolve_ok(&cfg, "prod").deployment;
    let Artifact::Build(b) = &d.artifact else {
        panic!("expected a build")
    };
    assert_eq!(
        b.strategy,
        BuildStrategy::Buildpacks {
            builder: DEFAULT_BUILDER.into()
        }
    );
    // Build resource names default when runway creates them.
    assert_eq!(b.artifact_repository, "runway");
    assert_eq!(b.source_bucket, "my-gcp-project-runway-sources");
    assert_eq!(
        b.build_service_account,
        "runway-build@my-gcp-project.iam.gserviceaccount.com"
    );

    let custom = resolve_ok(&cfg, "custom").deployment;
    let Artifact::Build(b) = &custom.artifact else {
        panic!()
    };
    assert_eq!(
        b.strategy,
        BuildStrategy::Buildpacks {
            builder: "gcr.io/buildpacks/builder:google-22".into()
        }
    );

    // A Dockerfile in the context is picked up automatically.
    std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let d = resolve_ok(&cfg, "prod").deployment;
    let Artifact::Build(b) = &d.artifact else {
        panic!()
    };
    assert_eq!(
        b.strategy,
        BuildStrategy::Dockerfile {
            path: "Dockerfile".into()
        }
    );

    // Explicit dockerfile + builder is a conflict; names are required without create_build_resources.
    std::fs::write(
        &path,
        yaml.replace(
            "builder: \"gcr.io/buildpacks/builder:google-22\"",
            "builder: x, dockerfile: Dockerfile",
        )
        .replace(", create_build_resources: true", ""),
    )
    .unwrap();
    let cfg = load(&path).unwrap();
    let e = resolve_err(&cfg, "custom");
    assert!(
        e.iter().any(|i| i.message.contains("mutually exclusive")),
        "{e:#?}"
    );
    let e = resolve_err(&cfg, "prod");
    assert!(
        has_error(&e, "provider.source_bucket", "create_build_resources"),
        "{e:#?}"
    );
}

#[test]
fn ingress_is_configurable_and_validated() {
    let (_d, cfg) = load_str(&FULL.replace(
        "  public: false\n",
        "  public: false\n  ingress: internal-and-cloud-load-balancing\n",
    ));
    assert_eq!(
        resolve_ok(&cfg, "dev").deployment.service.ingress,
        "internal-and-cloud-load-balancing"
    );
    let (_d, cfg) = load_str(FULL);
    assert_eq!(
        resolve_ok(&cfg, "dev").deployment.service.ingress,
        "all",
        "default"
    );
    let (_d, cfg) = load_str(&FULL.replace(
        "  public: false\n",
        "  public: false\n  ingress: everywhere\n",
    ));
    assert!(has_error(
        &resolve_err(&cfg, "dev"),
        "service.ingress",
        "internal-and-cloud-load-balancing"
    ));
}

#[test]
fn health_check_defaults_overrides_and_limits() {
    let with = |hc: &str| FULL.replace("  public: false\n", &format!("  public: false\n{hc}"));
    let (_d, cfg) = load_str(&with("  health_check:\n    path: /healthz\n"));
    let hc = resolve_ok(&cfg, "dev")
        .deployment
        .service
        .health_check
        .unwrap();
    assert_eq!(hc.path, "/healthz");
    assert_eq!(hc.startup, ProbeSettings::STARTUP);
    assert_eq!(
        hc.liveness,
        Some(ProbeSettings::LIVENESS),
        "liveness on by default"
    );

    let (_d, cfg) = load_str(&with(
        "  health_check:\n    path: /healthz\n    startup: { period_seconds: 5, failure_threshold: 24 }\n    liveness: false\n",
    ));
    let hc = resolve_ok(&cfg, "dev")
        .deployment
        .service
        .health_check
        .unwrap();
    assert_eq!(
        (
            hc.startup.period_seconds,
            hc.startup.failure_threshold,
            hc.startup.timeout_seconds
        ),
        (5, 24, 3)
    );
    assert_eq!(hc.liveness, None);

    let (_d, cfg) = load_str(&with(
        "  health_check:\n    path: healthz\n    startup: { period_seconds: 300, timeout_seconds: 400 }\n    liveness: { period_seconds: 5, timeout_seconds: 10 }\n",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "service.health_check.path", "absolute HTTP path"),
        "{e:#?}"
    );
    assert!(
        has_error(
            &e,
            "service.health_check.startup.period_seconds",
            "between 1 and 240"
        ),
        "{e:#?}"
    );
    assert!(
        has_error(
            &e,
            "service.health_check.liveness.timeout_seconds",
            "must not exceed period_seconds"
        ),
        "{e:#?}"
    );

    let (_d, cfg) = load_str(FULL);
    assert!(
        resolve_ok(&cfg, "dev")
            .deployment
            .service
            .health_check
            .is_none(),
        "Cloud Run's default TCP probe"
    );
}

#[test]
fn project_tags_are_validated() {
    let (_d, cfg) = load_str(&FULL.replace(
        "  region: europe-west1\n",
        "  region: europe-west1\n  tags:\n    \"210987654321/allowIngressAllForCloudRun\": allow-ingress-all\n",
    ));
    let d = resolve_ok(&cfg, "dev").deployment;
    assert_eq!(
        d.project_tags["210987654321/allowIngressAllForCloudRun"],
        "allow-ingress-all"
    );
    let (_d, cfg) = load_str(&FULL.replace(
        "  region: europe-west1\n",
        "  region: europe-west1\n  tags:\n    allowIngressAllForCloudRun: allow/ingress\n",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(
        e.iter()
            .filter(|i| i.path == "provider.tags.allowIngressAllForCloudRun")
            .count()
            == 2,
        "{e:#?}"
    );
}

#[test]
fn bootstrap_and_otel_collector_settings() {
    let with =
        |extra: &str| FULL.replace("  public: false\n", &format!("  public: false\n{extra}"));
    let (_d, cfg) = load_str(&with("  bootstrap: {}\n  otel_collector: {}\n"));
    let d = resolve_ok(&cfg, "dev").deployment;
    let bs = d.service.bootstrap.unwrap();
    assert_eq!(
        (bs.image, bs.ingress.as_str()),
        (None, "internal"),
        "default: the real app with internal ingress"
    );
    let o = d.service.otel_collector.unwrap();
    assert!(
        !o.pinned,
        "no version: newest release looked up at plan/deploy time"
    );
    assert_eq!(
        o.image,
        format!("{OTEL_COLLECTOR_IMAGE}:{OTEL_COLLECTOR_VERSION}"),
        "offline fallback"
    );
    assert_eq!(o.config, OTEL_COLLECTOR_DEFAULT_CONFIG);
    assert_eq!(
        d.service.env["OTEL_EXPORTER_OTLP_ENDPOINT"],
        "http://localhost:4318"
    );
    for role in OTEL_COLLECTOR_ROLES {
        assert!(
            d.service.identity.roles.iter().any(|r| r.role == *role),
            "{role}"
        );
    }

    let (_d, cfg) = load_str(&with(
        "  otel_collector: { version: \"0.156.1\", cpu: \"2\", memory: 1Gi }\n",
    ));
    let o = resolve_ok(&cfg, "dev")
        .deployment
        .service
        .otel_collector
        .unwrap();
    assert!(o.pinned);
    assert!(o.image.ends_with(":0.156.1"));
    assert_eq!((o.cpu.as_str(), o.memory.as_str()), ("2", "1Gi"));

    let (_d, cfg) = load_str(&with("  otel_collector: { enabled: false }\n"));
    assert!(
        resolve_ok(&cfg, "dev")
            .deployment
            .service
            .otel_collector
            .is_none()
    );

    let (_d, cfg) = load_str(&with("  bootstrap: { ingress: everywhere }\n"));
    assert!(has_error(
        &resolve_err(&cfg, "dev"),
        "service.bootstrap.ingress",
        "internal"
    ));

    // An explicit endpoint elsewhere is kept, with a warning.
    let (_d, cfg) = load_str(&with("  otel_collector: {}\n").replace(
        "LOG_LEVEL: info",
        "LOG_LEVEL: info\n    OTEL_EXPORTER_OTLP_ENDPOINT: https://telemetry.googleapis.com",
    ));
    let r = resolve_ok(&cfg, "dev");
    assert_eq!(
        r.deployment.service.env["OTEL_EXPORTER_OTLP_ENDPOINT"],
        "https://telemetry.googleapis.com"
    );
    assert!(
        r.warnings
            .iter()
            .any(|w| w.message.contains("bypasses the otel_collector"))
    );
}

#[test]
fn managed_secrets_and_secret_files() {
    let with = |top: &str, secrets: &str| {
        FULL.replacen("service:\n", &format!("{top}service:\n"), 1)
            .replace(
                "      secret: database-url\n      version: \"1\"\n",
                &format!("      secret: database-url\n      version: \"1\"\n{secrets}"),
            )
    };
    let (_d, cfg) = load_str(&with(
        "secrets:\n  token:\n    adders: [group:devops@example.com]\n  cert:\n    name: \"${app}-${stage}-cert\"\n    locations: []\n",
        "    TOKEN: { secret: \"${secrets.token}\" }\n    cert: { secret: \"${secrets.cert}\", path: /secrets/cert/tls.pem }\n",
    ));
    let r = resolve_ok(&cfg, "dev");
    let d = r.deployment;
    assert_eq!(d.secrets["token"].name, "token", "name defaults to the key");
    assert_eq!(d.secrets["token"].locations, ["europe-west1"]);
    assert_eq!(d.secrets["cert"].name, "hello-api-dev-cert");
    assert!(
        d.secrets["cert"].locations.is_empty(),
        "[] = automatic replication"
    );
    assert!(
        r.warnings.iter().any(|w| w.path == "secrets.cert.adders"),
        "warns when nobody can add the value"
    );
    let token = &d.service.secrets["TOKEN"];
    assert_eq!((token.version.as_str(), token.pin_latest), ("latest", true));
    let cert = &d.service.secrets["cert"];
    assert_eq!((cert.version.as_str(), cert.pin_latest), ("latest", false));
    assert_eq!(d.service.secrets["DATABASE_URL"].version, "1");

    // A secret file needs its own directory.
    let (_d, cfg) = load_str(&with("", "    a: { secret: a, path: /a.pem }\n"));
    assert!(has_error(
        &resolve_err(&cfg, "dev"),
        "service.secrets.a.path",
        "own directory"
    ));
    let (_d, cfg) = load_str(&with(
        "",
        "    a: { secret: a, path: /s/a.pem }\n    b: { secret: b, path: /s/b.pem }\n",
    ));
    assert!(has_error(
        &resolve_err(&cfg, "dev"),
        "service.secrets.b.path",
        "already holds"
    ));
    // Env var names are only required for environment variables.
    let (_d, cfg) = load_str(&with("", "    my-file: { secret: a, path: /s/a.pem }\n"));
    resolve_ok(&cfg, "dev");
    let (_d, cfg) = load_str(&with("", "    my-env: { secret: a }\n"));
    assert!(!resolve_err(&cfg, "dev").is_empty());
    // Unknown managed secret reference.
    let (_d, cfg) = load_str(&with("", "    X: { secret: \"${secrets.nope}\" }\n"));
    assert!(has_error(
        &resolve_err(&cfg, "dev"),
        "service.secrets.X.secret",
        "unknown variable"
    ));
    // Managed secrets are created in the deployment project.
    let (_d, cfg) = load_str(&with(
        "secrets:\n  x:\n    name: projects/p/secrets/x\n",
        "",
    ));
    assert!(has_error(
        &resolve_err(&cfg, "dev"),
        "secrets.x.name",
        "secret ID"
    ));
}

#[test]
fn sidecars_resolve_validate_and_override() {
    let with = |sidecars: &str, stage: &str| {
        FULL.replace(
            "  public: false\n",
            &format!("  public: false\n  volumes:\n    cache: {{ bucket: my-cache, mount_path: /mnt/cache }}\n  sidecars:\n{sidecars}"),
        )
        .replace("  dev:\n    service:\n", &format!("  dev:\n    service:\n{stage}"))
    };
    let (_d, cfg) = load_str(&with(
        "    proxy:\n      image: \"envoyproxy/envoy:v1.31.0\"\n      cpu: 0.5\n      memory: 256Mi\n      args: [\"-c\", \"/etc/envoy/${stage}.yaml\"]\n      env: { LOG_LEVEL: debug }\n      secrets: { TOKEN: { secret: proxy-token } }\n      health_check: { port: 9901, path: /ready }\n      volumes: { cache: /cache }\n    sql:\n      image: \"gcr.io/cloud-sql-connectors/cloud-sql-proxy:2.14.0\"\n      health_check: { port: 5432 }\n      start_before_app: false\n",
        "",
    ));
    let d = resolve_ok(&cfg, "dev").deployment;
    let p = &d.service.sidecars["proxy"];
    assert_eq!((p.cpu.as_str(), p.memory.as_str()), ("500m", "256Mi"));
    assert_eq!(p.args, ["-c", "/etc/envoy/dev.yaml"], "interpolated");
    assert!(p.start_before_app, "default: before the app");
    assert!(p.secrets["TOKEN"].pin_latest);
    assert_eq!(
        p.health_check.as_ref().unwrap().path.as_deref(),
        Some("/ready")
    );
    let sql = &d.service.sidecars["sql"];
    assert_eq!(
        (sql.cpu.as_str(), sql.memory.as_str()),
        ("1", "512Mi"),
        "defaults"
    );
    assert!(!sql.start_before_app);
    assert_eq!(sql.health_check.as_ref().unwrap().path, None, "TCP check");
    // The runtime account can read the sidecar's secret.
    assert!(crate::provision::pre_steps(&d).iter().any(|s| {
        s.describe(&d)
            .contains("secretAccessor on secret projects/my-gcp-project/secrets/proxy-token")
    }));

    // A stage removes a sidecar with null.
    let (_d, cfg) = load_str(&with(
        "    sql: { image: \"gcr.io/cloud-sql-connectors/cloud-sql-proxy:2.14.0\" }\n",
        "      sidecars: { sql: null }\n",
    ));
    assert!(
        resolve_ok(&cfg, "dev")
            .deployment
            .service
            .sidecars
            .is_empty()
    );
    assert_eq!(
        resolve_ok(&cfg, "prod").deployment.service.sidecars.len(),
        1
    );

    let err = |sc: &str, path: &str, needle: &str| {
        let (_d, cfg) = load_str(&with(sc, ""));
        let errors = resolve_err(&cfg, "dev");
        assert!(
            has_error(&errors, path, needle),
            "{path}: {needle}: {errors:#?}"
        );
    };
    err(
        "    app: { image: nginx }\n",
        "service.sidecars.app",
        "reserved",
    );
    err(
        "    Bad_Name: { image: nginx }\n",
        "service.sidecars.Bad_Name",
        "container name",
    );
    err(
        "    p: { image: nginx, health_check: { port: 8080 } }\n",
        "service.sidecars.p.health_check.port",
        "application's port",
    );
    err(
        "    p: { image: nginx, volumes: { nope: /x } }\n",
        "service.sidecars.p.volumes.nope",
        "no volume",
    );
    err(
        "    p: { image: nginx, secrets: { T: { secret: s, path: /s/t } } }\n",
        "service.sidecars.p.secrets.T.path",
        "environment variables only",
    );
    let many: String = (0..10)
        .map(|i| format!("    s{i}: {{ image: nginx }}\n"))
        .collect();
    err(&many, "service.sidecars", "at most 10");
}

/// FULL with extra `service:` lines (indented under `service:`).
fn full_with(service_extra: &str) -> String {
    FULL.replacen(
        "  service_account: runtime@my-gcp-project.iam.gserviceaccount.com\n",
        &format!(
            "  service_account: runtime@my-gcp-project.iam.gserviceaccount.com\n{service_extra}"
        ),
        1,
    )
}

#[test]
fn networking_billing_and_cloud_sql_resolve() {
    let (_d, cfg) = load_str(&full_with(
        r#"  billing: instance-based
  startup_cpu_boost: true
  execution_environment: gen2
  vpc:
    network: projects/host-project/global/networks/shared
    subnet: projects/host-project/regions/europe-west1/subnetworks/run
    egress: all-traffic
    network_tags: [run-egress]
  cloud_sql: [db, other-project:europe-west4:reports]
  custom_audiences: [https://api.example.com]
"#,
    ));
    let s = resolve_ok(&cfg, "dev").deployment.service;
    assert_eq!(s.billing, "instance-based");
    assert!(s.startup_cpu_boost);
    assert_eq!(s.execution_environment.as_deref(), Some("gen2"));
    let vpc = s.vpc.unwrap();
    assert_eq!(vpc.egress, "all-traffic");
    assert_eq!(vpc.network_tags, ["run-egress"]);
    assert_eq!(
        s.cloud_sql,
        [
            "my-gcp-project:europe-west1:db",
            "other-project:europe-west4:reports"
        ],
        "an instance name alone is in the deployment project and region"
    );
    assert_eq!(s.custom_audiences, ["https://api.example.com"]);
    let client = |p: &str| RoleBinding {
        role: CLOUD_SQL_CLIENT_ROLE.into(),
        target: RoleTarget::Project { project: p.into() },
    };
    assert!(s.identity.roles.contains(&client("my-gcp-project")));
    assert!(s.identity.roles.contains(&client("other-project")));
}

#[test]
fn networking_defaults_leave_cloud_run_defaults() {
    let (_d, cfg) = load_str(FULL);
    let s = resolve_ok(&cfg, "dev").deployment.service;
    assert_eq!(s.billing, "request-based");
    assert!(!s.startup_cpu_boost);
    assert_eq!(s.execution_environment, None);
    assert!(s.vpc.is_none() && s.cloud_sql.is_empty() && s.custom_audiences.is_empty());
}

#[test]
fn billing_and_execution_environment_follow_cloud_run_rules() {
    let (_d, cfg) = load_str(
        &full_with("  billing: instance-based\n  memory: 256Mi\n").replace("  memory: 512Mi\n", ""),
    );
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(&e, "service.billing", "512Mi"), "{e:#?}");

    let (_d, cfg) = load_str(
        &FULL
            .replace("  cpu: \"1\"\n", "  cpu: \"0.5\"\n")
            .replace("  memory: 512Mi\n", "  memory: 256Mi\n")
            .replace(
                "  service_account: runtime@",
                "  execution_environment: gen2\n  billing: instance-based\n  service_account: runtime@",
            ),
    );
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "service.concurrency", "concurrency: 1"),
        "{e:#?}"
    );
    assert!(has_error(&e, "service.billing", "at least 1 CPU"), "{e:#?}");
    assert!(
        has_error(&e, "service.execution_environment", "gen1"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.execution_environment", "512Mi"),
        "{e:#?}"
    );

    let (_d, cfg) = load_str(&full_with(
        "  execution_environment: gen1\n  volumes:\n    data:\n      bucket: my-bucket\n      mount_path: /data\n",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "service.execution_environment", "gen2"),
        "{e:#?}"
    );

    let (_d, cfg) = load_str(&full_with(
        "  execution_environment: gen3\n  billing: always\n",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(
        &e,
        "service.execution_environment",
        "`gen1` or `gen2`"
    ));
    assert!(has_error(
        &e,
        "service.billing",
        "`request-based` or `instance-based`"
    ));
}

#[test]
fn vpc_cloud_sql_and_audiences_are_validated() {
    let (_d, cfg) = load_str(&full_with(
        r#"  vpc:
    network: Bad_Name
    subnet: projects/host-project/regions/us-central1/subnetworks/run
    egress: everything
    network_tags: [Not-Valid]
  cloud_sql: ["my-gcp-project:db"]
  custom_audiences: ["has space"]
  volumes:
    cloudsql:
      bucket: my-bucket
      mount_path: /cloudsql
"#,
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "service.vpc.network", "not a valid network name"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.vpc.subnet", "service region `europe-west1`"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.vpc.egress", "private-ranges-only"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.vpc.network_tags", "network tag"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.cloud_sql", "PROJECT:REGION:INSTANCE"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.custom_audiences", "without spaces"),
        "{e:#?}"
    );

    // A valid connection with a clashing volume name and mount path.
    let (_d, cfg) = load_str(&full_with(
        "  cloud_sql: [db]\n  volumes:\n    cloudsql:\n      bucket: my-bucket\n      mount_path: /cloudsql\n",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "service.volumes.cloudsql", "reserved"),
        "{e:#?}"
    );
    assert!(
        has_error(&e, "service.volumes.cloudsql.mount_path", "Cloud SQL"),
        "{e:#?}"
    );
}

#[test]
fn a_stage_vpc_block_replaces_the_inherited_one() {
    let yaml = full_with(
        "  vpc:\n    network: default\n    subnet: default\n    network_tags: [a]\n",
    )
    .replace(
        "  prod:\n    service:\n      min_instances: 1\n",
        "  prod:\n    service:\n      min_instances: 1\n      vpc:\n        network: prod-net\n        subnet: prod-subnet\n",
    );
    let (_d, cfg) = load_str(&yaml);
    let dev = resolve_ok(&cfg, "dev").deployment.service.vpc.unwrap();
    assert_eq!(
        (dev.network.as_str(), dev.network_tags.len()),
        ("projects/my-gcp-project/global/networks/default", 1),
        "a name alone is in the deployment project"
    );
    assert_eq!(
        dev.subnet,
        "projects/my-gcp-project/regions/europe-west1/subnetworks/default"
    );
    let prod = resolve_ok(&cfg, "prod").deployment.service.vpc.unwrap();
    assert_eq!(
        prod.network,
        "projects/my-gcp-project/global/networks/prod-net"
    );
    assert!(
        prod.network_tags.is_empty(),
        "the stage block replaces, not merges"
    );
    assert_eq!(prod.egress, "private-ranges-only");
}

#[test]
fn sandboxes_run_on_gen2() {
    let (_d, cfg) = load_str(&full_with("  sandbox: true\n"));
    let s = resolve_ok(&cfg, "dev").deployment.service;
    assert!(s.sandbox);
    assert_eq!(
        s.execution_environment.as_deref(),
        Some("gen2"),
        "set, so the plan shows it"
    );

    let (_d, cfg) = load_str(&full_with("  sandbox: false\n"));
    let s = resolve_ok(&cfg, "dev").deployment.service;
    assert!(!s.sandbox && s.execution_environment.is_none());

    let (_d, cfg) = load_str(&full_with(
        "  sandbox: true\n  execution_environment: gen1\n",
    ));
    let e = resolve_err(&cfg, "dev");
    assert!(
        has_error(&e, "service.execution_environment", "sandboxes need gen2"),
        "{e:#?}"
    );

    let (_d, cfg) = load_str(
        &full_with("  sandbox: true\n")
            .replace("  cpu: \"1\"\n", "  cpu: \"0.5\"\n  concurrency: 1\n")
            .replace("  concurrency: 80\n", "")
            .replace("  memory: 512Mi\n", "  memory: 256Mi\n"),
    );
    let e = resolve_err(&cfg, "dev");
    assert!(has_error(&e, "service.sandbox", "512Mi"), "{e:#?}");
    assert!(has_error(&e, "service.sandbox", "at least 1 CPU"), "{e:#?}");
}
