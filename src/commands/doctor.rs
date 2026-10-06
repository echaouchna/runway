//! `runway doctor`: verify credentials, APIs, permissions and prerequisite resources.
//!
//! Every check is read-only. Permission checks use `testIamPermissions`, which
//! reports what the *calling* principal may do; permissions of the build and
//! runtime service accounts cannot be verified this way and are listed for a
//! manual review instead (secret accessor grants are inspected where possible).

use crate::build_client;
use crate::cli::{Context, DoctorArgs};
use crate::config::{self, Artifact, Deployment, Overrides, RoleTarget};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::run::{self, Ownership};
use crate::gcp::{Session, is_not_found, status_code};
use crate::output::{OutputFormat, print_json};
use google_cloud_api_serviceusage_v1::client::ServiceUsage;
use google_cloud_artifactregistry_v1::client::ArtifactRegistry;
use google_cloud_gax::error::rpc::Code;
use google_cloud_iam_admin_v1::client::Iam;
use google_cloud_resourcemanager_v3::client::Projects;
use google_cloud_run_v2::client::Services;
use google_cloud_secretmanager_v1::client::SecretManagerService;
use google_cloud_storage::client::StorageControl;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Default, Serialize)]
pub struct Report {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    pub checks: Vec<Check>,
}

impl Report {
    fn add(
        &mut self,
        name: impl Into<String>,
        status: CheckStatus,
        detail: impl Into<String>,
        hint: Option<String>,
    ) {
        self.checks.push(Check {
            name: name.into(),
            status,
            detail: detail.into(),
            hint,
        });
    }
    fn pass(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.add(name, CheckStatus::Pass, detail, None);
    }
    fn warn(
        &mut self,
        name: impl Into<String>,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.add(name, CheckStatus::Warn, detail, Some(hint.into()));
    }
    fn fail(
        &mut self,
        name: impl Into<String>,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.add(name, CheckStatus::Fail, detail, Some(hint.into()));
    }
}

/// APIs that must be enabled for a deployment (shared with `deploy`).
pub fn required_apis(d: &Deployment) -> Vec<String> {
    crate::provision::required_apis(d)
}

/// Project-level permissions the deploying principal needs.
pub fn required_project_permissions(d: &Deployment) -> Vec<&'static str> {
    let mut p = vec![
        "run.services.get",
        "run.services.create",
        "run.services.update",
        "run.services.getIamPolicy",
        "run.operations.get",
        "run.revisions.get",
        "logging.logEntries.list",
    ];
    if d.service.public || d.service.iap.enabled {
        p.push("run.services.setIamPolicy");
    }
    let creates_build = matches!(&d.artifact, Artifact::Build(b) if b.create_resources);
    if (d.service.identity.create
        && crate::config::validate::service_account_project(&d.service.service_account)
            == Some(d.project.as_str()))
        || creates_build
    {
        p.push("iam.serviceAccounts.create");
    }
    if creates_build
        || d.service
            .identity
            .roles
            .iter()
            .any(|r| matches!(&r.target, RoleTarget::Project { project } if *project == d.project))
    {
        p.push("resourcemanager.projects.setIamPolicy");
    }
    if creates_build || !d.buckets.is_empty() {
        p.push("storage.buckets.create");
    }
    if creates_build {
        p.push("artifactregistry.repositories.create");
    }
    if d.apis.enable {
        p.push("serviceusage.services.enable");
    }
    if !d.project_tags.is_empty() {
        p.push("resourcemanager.projects.createTagBinding");
    }
    if !d.service.tags.is_empty() {
        p.push("run.services.createTagBinding");
    }
    if matches!(d.artifact, Artifact::Build(_)) {
        p.extend([
            "cloudbuild.builds.create",
            "cloudbuild.builds.get",
            "cloudbuild.builds.list",
        ]);
    }
    p
}

fn short_err(e: &crate::gcp::GaxError) -> String {
    e.status()
        .map(|s| s.message.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| e.to_string())
}

async fn principal(session: &Session, token: &str) -> Option<String> {
    let resp = session
        .http
        .get("https://oauth2.googleapis.com/tokeninfo")
        .query(&[("access_token", token)])
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("email").and_then(|e| e.as_str()).map(String::from)
}

pub async fn run(ctx: &Context, args: DoctorArgs) -> Result<()> {
    let mut r = Report::default();
    let stage = &args.stage.stage;

    // 1. Configuration.
    let d = match config::load_and_resolve(&ctx.config, stage, &Overrides::default()) {
        Ok((_, res)) => {
            r.pass(
                "configuration",
                format!("{} is valid for stage {stage}", ctx.config.display()),
            );
            res.deployment
        }
        Err(e) => {
            r.fail(
                "configuration",
                e.detailed_message(),
                "fix the configuration (runway validate)",
            );
            return finish(ctx, r);
        }
    };

    // 2. Credentials.
    let session = match crate::commands::connect(ctx, &d).await {
        Ok(s) => s,
        Err(e) => {
            let hint = e
                .hints
                .first()
                .cloned()
                .unwrap_or_else(|| crate::gcp::ADC_HINT.to_string());
            r.fail("credentials", e.message.clone(), hint);
            return finish(ctx, r);
        }
    };
    let token = match session.token().await {
        Ok(t) => t,
        Err(e) => {
            r.fail("credentials", e.message.clone(), crate::gcp::ADC_HINT);
            return finish(ctx, r);
        }
    };
    // 3-9. Independent read-only checks, run concurrently.
    let (principal, project, apis, perms, accounts, repo, bucket, secrets, service) = tokio::join!(
        principal(&session, &token),
        check_project(&session, &d),
        check_apis(&session, &d),
        check_permissions(&session, &d),
        check_service_accounts(&session, &d),
        check_repository(&session, &d),
        check_bucket(&session, &d),
        check_secrets(&session, &d),
        check_service(&session, &d),
    );
    r.principal = principal;
    r.pass(
        "credentials",
        format!(
            "Application Default Credentials loaded ({}){}",
            r.principal
                .clone()
                .unwrap_or_else(|| "principal email not reported".into()),
            session
                .impersonating
                .as_ref()
                .map(|i| format!(", impersonated via the caller's ADC: {}", i.target))
                .unwrap_or_default()
        ),
    );
    let foreign = check_foreign_grants(&session, &d).await;
    for part in [
        project, apis, perms, accounts, repo, bucket, secrets, service, foreign,
    ] {
        r.checks.extend(part?.checks);
    }

    finish(ctx, r)
}

/// Project exists and is accessible.
async fn check_project(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    let project_name = format!("projects/{}", d.project);
    // 3. Project.
    let projects = build_client!(Projects, session)?;
    match projects.get_project().set_name(&project_name).send().await {
        Ok(p) => r.pass("project", format!("{} ({})", p.project_id, p.display_name)),
        Err(e) if status_code(&e) == Some(Code::PermissionDenied) || is_not_found(&e) => r.fail(
            "project",
            format!("cannot access project {}: {}", d.project, short_err(&e)),
            "check provider.project and that the principal has at least roles/browser on it",
        ),
        Err(e) => r.warn(
            "project",
            short_err(&e),
            "is cloudresourcemanager.googleapis.com enabled?",
        ),
    }
    Ok(r)
}

/// Required APIs are enabled.
async fn check_apis(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    let project_name = format!("projects/{}", d.project);
    // 4. APIs.
    let usage = build_client!(ServiceUsage, session)?;
    let apis = required_apis(d);
    match usage
        .batch_get_services()
        .set_parent(&project_name)
        .set_names(apis.iter().map(|a| format!("{project_name}/services/{a}")))
        .send()
        .await
    {
        Ok(resp) => {
            let disabled: Vec<String> = resp
                .services
                .iter()
                .filter(|s| s.state != google_cloud_api_serviceusage_v1::model::State::Enabled)
                .map(|s| s.name.rsplit('/').next().unwrap_or(&s.name).to_string())
                .collect();
            if disabled.is_empty() {
                r.pass("apis", format!("enabled: {}", apis.join(", ")));
            } else if d.apis.enable {
                r.warn(
                    "apis",
                    format!("not enabled yet: {}", disabled.join(", ")),
                    "deploy enables them (provider.enable_apis: true)",
                );
            } else {
                r.fail(
                    "apis",
                    format!("not enabled: {}", disabled.join(", ")),
                    format!(
                        "gcloud services enable {} --project {}",
                        disabled.join(" "),
                        d.project
                    ),
                );
            }
        }
        Err(e) => r.warn(
            "apis",
            format!("cannot read enabled APIs: {}", short_err(&e)),
            format!("make sure these are enabled: {}", apis.join(", ")),
        ),
    }
    Ok(r)
}

/// Deployer permissions on the project.
async fn check_permissions(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    let project_name = format!("projects/{}", d.project);
    // 5. Deployer permissions on the project.
    let projects = build_client!(Projects, session)?;
    let wanted = required_project_permissions(d);
    let mut probe: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
    if !d.service.public {
        probe.push("run.services.setIamPolicy".into());
    }
    match projects
        .test_iam_permissions()
        .set_resource(&project_name)
        .set_permissions(probe)
        .send()
        .await
    {
        Ok(resp) => {
            let missing: Vec<&str> = wanted
                .iter()
                .filter(|p| !resp.permissions.iter().any(|g| g == *p))
                .copied()
                .collect();
            if missing.is_empty() {
                r.pass(
                    "deployer permissions",
                    format!("{} project permissions granted", wanted.len()),
                );
            } else {
                r.fail(
                    "deployer permissions",
                    format!("missing on project: {}", missing.join(", ")),
                    "grant the deployer roles listed in README.md#permissions (roles/run.developer or roles/run.admin, roles/cloudbuild.builds.editor, roles/logging.viewer)",
                );
            }
            if !d.service.public
                && !resp
                    .permissions
                    .iter()
                    .any(|g| g == "run.services.setIamPolicy")
            {
                r.warn(
                    "iam policy updates",
                    "run.services.setIamPolicy is not granted",
                    "fine for private services; needed only to switch public access on or off (roles/run.admin)",
                );
            }
        }
        Err(e) => r.warn(
            "deployer permissions",
            format!("cannot test permissions: {}", short_err(&e)),
            "is cloudresourcemanager.googleapis.com enabled?",
        ),
    }
    Ok(r)
}

/// Service accounts exist and the deployer can act as them.
async fn check_service_accounts(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    // 6. Service accounts (existence + actAs).
    let iam_client = build_client!(Iam, session)?;
    let mut accounts = vec![("runtime service account", d.service.service_account.clone())];
    if let Artifact::Build(b) = &d.artifact {
        accounts.push(("build service account", b.build_service_account.clone()));
    }
    let mut creates: Vec<String> = Vec::new();
    if d.service.identity.create {
        creates.push(d.service.service_account.clone());
    }
    if let Artifact::Build(b) = &d.artifact
        && b.create_resources
    {
        creates.push(b.build_service_account.clone());
    }
    let creates = &creates;
    let iam_client = &iam_client;
    let parts = futures::future::join_all(accounts.iter().map(|(label, email)| async move {
        let mut r = Report::default();
        let resource = crate::naming::service_account_resource(email);
        match iam_client
            .get_service_account()
            .set_name(&resource)
            .send()
            .await
        {
            Ok(sa) if sa.disabled => r.fail(
                *label,
                format!("{email} is disabled"),
                "enable the service account",
            ),
            Ok(_) => {
                match iam_client
                    .test_iam_permissions()
                    .set_resource(&resource)
                    .set_permissions(["iam.serviceAccounts.actAs".to_string()])
                    .send()
                    .await
                {
                    Ok(p) if p.permissions.iter().any(|x| x == "iam.serviceAccounts.actAs") => {
                        r.pass(*label, format!("{email} exists; deployer can act as it"))
                    }
                    Ok(_) => r.fail(
                        *label,
                        format!("deployer cannot act as {email}"),
                        format!("gcloud iam service-accounts add-iam-policy-binding {email} --member=<deployer> --role=roles/iam.serviceAccountUser"),
                    ),
                    Err(e) => r.warn(*label, format!("cannot test actAs on {email}: {}", short_err(&e)), "verify roles/iam.serviceAccountUser manually"),
                }
            }
            Err(e) if is_not_found(&e) && creates.contains(email) => r.warn(
                *label,
                format!("{email} does not exist yet; deploy creates it"),
                "the deployer will need iam.serviceAccounts.actAs on it (roles/iam.serviceAccountUser at project level covers new accounts)",
            ),
            Err(e) if is_not_found(&e) => r.fail(
                *label,
                format!("{email} does not exist"),
                "create it (see README.md#prerequisites), or set identity.create / provider.create_build_resources",
            ),
            Err(e) => r.warn(
                *label,
                format!("cannot read {email}: {}", short_err(&e)),
                "verify it exists and the deployer has roles/iam.serviceAccountUser on it",
            ),
        }
            r
    }))
    .await;
    for part in parts {
        r.checks.extend(part.checks);
    }
    Ok(r)
}

/// Artifact Registry repository (source builds).
async fn check_repository(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    // 7. Build infrastructure.
    if let Artifact::Build(b) = &d.artifact {
        let ar = build_client!(ArtifactRegistry, session)?;
        let repo = format!(
            "projects/{}/locations/{}/repositories/{}",
            d.project, b.artifact_location, b.artifact_repository
        );
        match ar.get_repository().set_name(&repo).send().await {
            Ok(rep) if rep.format != google_cloud_artifactregistry_v1::model::repository::Format::Docker => r.fail(
                "artifact repository",
                format!("{repo} is not a Docker repository"),
                "create a repository with --repository-format=docker",
            ),
            Ok(_) => {
                let perms = ["artifactregistry.repositories.downloadArtifacts".to_string()];
                match ar.test_iam_permissions().set_resource(&repo).set_permissions(perms).send().await {
                    Ok(p) if !p.permissions.is_empty() => r.pass("artifact repository", format!("{repo} (Docker)")),
                    _ => r.warn(
                        "artifact repository",
                        format!("{repo} exists, but the deployer cannot read images"),
                        "grant roles/artifactregistry.reader so runway can reuse existing builds; builds still work",
                    ),
                }
            }
            Err(e) if is_not_found(&e) && b.create_resources => r.warn(
                "artifact repository",
                format!("{repo} does not exist yet; deploy creates it"),
                "requires artifactregistry.repositories.create (checked with the deployer permissions)",
            ),
            Err(e) if is_not_found(&e) => r.fail(
                "artifact repository",
                format!("{repo} does not exist"),
                format!(
                    "gcloud artifacts repositories create {} --repository-format=docker --location={} --project={}",
                    b.artifact_repository, b.artifact_location, d.project
                ),
            ),
            Err(e) => r.warn("artifact repository", short_err(&e), "verify the repository exists"),
        }
    }
    Ok(r)
}

/// Source bucket (source builds).
async fn check_bucket(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    // Source bucket.
    if let Artifact::Build(b) = &d.artifact {
        let control = StorageControl::builder()
            .with_credentials(session.credentials.clone())
            .build()
            .await
            .map_err(|e| Error::internal(format!("cannot create StorageControl client: {e}")))?;
        let bucket = format!("projects/_/buckets/{}", b.source_bucket);
        match control.get_bucket().set_name(&bucket).send().await {
            Ok(_) => match control
                .test_iam_permissions()
                .set_resource(&bucket)
                .set_permissions(["storage.objects.create".to_string(), "storage.objects.delete".to_string()])
                .send()
                .await
            {
                Ok(p) if p.permissions.len() == 2 => {
                    r.pass("source bucket", format!("gs://{} writable", b.source_bucket))
                }
                Ok(p) => r.fail(
                    "source bucket",
                    format!(
                        "deployer lacks {} on gs://{}",
                        ["storage.objects.create", "storage.objects.delete"]
                            .iter()
                            .filter(|x| !p.permissions.iter().any(|g| g == *x))
                            .copied()
                            .collect::<Vec<_>>()
                            .join(", "),
                        b.source_bucket
                    ),
                    "grant roles/storage.objectUser on the bucket (overwriting a re-uploaded archive needs delete)",
                ),
                Err(e) => r.warn("source bucket", short_err(&e), "verify bucket permissions manually"),
            },
            Err(e) if is_not_found(&e) && b.create_resources => r.warn(
                "source bucket",
                format!("gs://{} does not exist yet; deploy creates it", b.source_bucket),
                "requires storage.buckets.create (checked with the deployer permissions)",
            ),
            Err(e) if is_not_found(&e) => r.fail(
                "source bucket",
                format!("gs://{} does not exist", b.source_bucket),
                format!("gcloud storage buckets create gs://{} --location={} --uniform-bucket-level-access", b.source_bucket, d.region),
            ),
            Err(e) => r.warn("source bucket", short_err(&e), "verify the bucket exists and is readable"),
        }
        if b.create_resources {
            r.pass(
                "build service account roles",
                "granted by deploy (logging.logWriter, artifactregistry.writer, storage.objectViewer)",
            );
        } else {
            r.warn(
                "build service account roles",
                format!("cannot verify the roles of {} from here", b.build_service_account),
                "it needs roles/logging.logWriter (project), roles/artifactregistry.writer (repository) and roles/storage.objectViewer (bucket)",
            );
        }
    }
    Ok(r)
}

/// Secret versions and accessor grants.
async fn check_secrets(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    // 8. Secrets.
    if !d.service.secrets.is_empty() {
        let sm = build_client!(SecretManagerService, session)?;
        let sm = &sm;
        let parts = futures::future::join_all(d.service.secrets.iter().map(|(env, s)| async move {
            let mut r = Report::default();
            let secret_name = if s.secret.starts_with("projects/") {
                s.secret.clone()
            } else {
                format!("projects/{}/secrets/{}", d.project, s.secret)
            };
            let label = format!("secret {env}");
            let version = format!("{secret_name}/versions/{}", s.version);
            match sm.get_secret_version().set_name(&version).send().await {
                Ok(v) if v.state == google_cloud_secretmanager_v1::model::secret_version::State::Enabled => {
                    let member = format!("serviceAccount:{}", d.service.service_account);
                    let granted = match sm.get_iam_policy().set_resource(&secret_name).send().await {
                        Ok(p) => Some(p.bindings.iter().any(|b| {
                            matches!(
                                b.role.as_str(),
                                "roles/secretmanager.secretAccessor" | "roles/secretmanager.admin"
                            ) && b.members.contains(&member)
                        })),
                        Err(_) => None,
                    };
                    match granted {
                        Some(true) => r.pass(label, format!("{version} enabled; runtime service account has access")),
                        _ => r.warn(
                            label,
                            format!("{version} enabled; no secretAccessor grant for the runtime service account on the secret yet"),
                            "deploy grants roles/secretmanager.secretAccessor on the secret (the deployer needs secretmanager.secrets.setIamPolicy)".to_string(),
                        ),
                    }
                }
                Ok(v) => r.fail(label, format!("{version} is {:?}", v.state), "use an enabled version"),
                Err(e) if is_not_found(&e)
                    && d.secrets.values().any(|m| secret_name == format!("projects/{}/secrets/{}", d.project, m.name)) =>
                {
                    r.warn(
                        label,
                        format!("{secret_name} is created by runway on deploy, without a value"),
                        "deploy stops until a value is added: gcloud secrets versions add <id> --data-file=-",
                    )
                }
                Err(e) if is_not_found(&e) => r.fail(label, format!("{version} does not exist"), "create the secret/version or fix the reference"),
                Err(e) => r.warn(
                    label,
                    format!("cannot read {version}: {}", short_err(&e)),
                    "the deployer lacks secretmanager.versions.get; the runtime may still have access",
                ),
            }
            r
        }))
        .await;
        for part in parts {
            r.checks.extend(part.checks);
        }
    }
    Ok(r)
}

/// Ownership of an existing service.
async fn check_service(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    // 9. Existing service ownership.
    let run_client = build_client!(Services, session)?;
    match run_client
        .get_service()
        .set_name(d.service_name())
        .send()
        .await
    {
        Ok(svc) => match run::ownership(&svc, &d.app, &d.stage) {
            Ownership::Owned => r.pass(
                "service",
                format!("{} exists and is managed by runway", d.service_id),
            ),
            Ownership::Unmanaged => r.fail(
                "service",
                format!("{} exists but is not managed by runway", d.service_id),
                "deploy with --adopt to take it over, or pick another app/stage name",
            ),
            Ownership::OtherOwner { app, stage } => r.fail(
                "service",
                format!(
                    "{} belongs to runway app `{app}` stage `{stage}`",
                    d.service_id
                ),
                "pick another app or stage name",
            ),
        },
        Err(e) if is_not_found(&e) => r.pass(
            "service",
            format!("{} will be created on first deploy", d.service_id),
        ),
        Err(e) => r.warn(
            "service",
            short_err(&e),
            "check run.services.get permission",
        ),
    }
    Ok(r)
}

/// Grants on other projects need setIamPolicy there; datasets need dataOwner.
async fn check_foreign_grants(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    let mut projects: Vec<String> = d
        .service
        .identity
        .roles
        .iter()
        .filter_map(|b| match &b.target {
            RoleTarget::Project { project } if *project != d.project => Some(project.clone()),
            _ => None,
        })
        .collect();
    projects.sort();
    projects.dedup();
    if !projects.is_empty() {
        let client = build_client!(Projects, session)?;
        for p in projects {
            let res = client
                .test_iam_permissions()
                .set_resource(format!("projects/{p}"))
                .set_permissions(["resourcemanager.projects.setIamPolicy".to_string()])
                .send()
                .await;
            let label = format!("grants on project {p}");
            match res {
                Ok(x) if !x.permissions.is_empty() => r.pass(label, "deployer can grant roles there"),
                Ok(_) => r.fail(
                    label,
                    format!("deployer lacks resourcemanager.projects.setIamPolicy on {p}"),
                    format!("grant roles/resourcemanager.projectIamAdmin on {p}, or have an admin grant the roles listed in identity.roles"),
                ),
                Err(e) => r.warn(label, short_err(&e), "verify the permission manually"),
            }
        }
    }
    for b in &d.service.identity.roles {
        if let RoleTarget::Dataset { project, dataset } = &b.target {
            r.warn(
                format!("grant on dataset {project}.{dataset}"),
                "cannot test dataset permissions from here",
                "the deployer needs bigquery.datasets.update (roles/bigquery.dataOwner on the dataset)",
            );
        }
    }
    Ok(r)
}

fn finish(ctx: &Context, mut r: Report) -> Result<()> {
    r.ok = !r.checks.iter().any(|c| c.status == CheckStatus::Fail);
    match ctx.output {
        OutputFormat::Json => print_json(&r),
        OutputFormat::Text => {
            let p = crate::style::out();
            for c in &r.checks {
                let mark = match c.status {
                    CheckStatus::Pass => p.green("✓"),
                    CheckStatus::Warn => p.yellow("!"),
                    CheckStatus::Fail => p.bold_red("✗"),
                    CheckStatus::Skip => p.dim("-"),
                };
                let name = format!("{:<28}", c.name);
                let name = if c.status == CheckStatus::Fail {
                    p.bold(&name)
                } else {
                    name
                };
                println!("{mark} {name} {}", c.detail);
                if let Some(h) = &c.hint
                    && c.status != CheckStatus::Pass
                {
                    println!("  {:<28} {}", "", p.cyan(&format!("→ {h}")));
                }
            }
            println!();
            println!(
                "{}",
                if r.ok {
                    p.bold_green("All required checks passed.")
                } else {
                    p.bold_red("Some checks failed; fix them before deploying.")
                }
            );
        }
    }
    if r.ok {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::Prerequisite, "prerequisite checks failed").reported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SecretRef, ServiceConfig};
    use crate::image_ref::ImageRef;
    use std::collections::BTreeMap;

    fn deployment(build: bool, public: bool, secrets: bool) -> Deployment {
        Deployment {
            app: "a".into(),
            stage: "dev".into(),
            project: "my-gcp-project".into(),
            region: "europe-west1".into(),
            service_id: "a-dev".into(),
            artifact: if build {
                Artifact::Build(crate::config::BuildConfig {
                    context_dir: ".".into(),
                    strategy: crate::config::BuildStrategy::Dockerfile {
                        path: "Dockerfile".into(),
                    },
                    artifact_location: "europe-west1".into(),
                    artifact_repository: "r".into(),
                    source_bucket: "b".into(),
                    build_service_account: "b@p.iam.gserviceaccount.com".into(),
                    excluded: vec![],
                    create_resources: false,
                    rebuild_always: false,
                })
            } else {
                Artifact::Image {
                    reference: "nginx".into(),
                    parsed: ImageRef::parse("nginx").unwrap(),
                }
            },
            service: ServiceConfig {
                port: 8080,
                cpu: "1".into(),
                memory: "512Mi".into(),
                timeout_seconds: 60,
                concurrency: 80,
                min_instances: 0,
                max_instances: 1,
                public,
                service_account: "rt@p.iam.gserviceaccount.com".into(),
                ingress: "all".into(),
                health_check: None,
                bootstrap: None,
                otel_collector: None,
                sidecars: Default::default(),
                env: BTreeMap::new(),
                secrets: if secrets {
                    BTreeMap::from([(
                        "S".into(),
                        SecretRef {
                            secret: "s".into(),
                            version: "1".into(),
                            ..Default::default()
                        },
                    )])
                } else {
                    BTreeMap::new()
                },
                tags: BTreeMap::new(),
                volumes: BTreeMap::new(),
                iap: Default::default(),
                identity: Default::default(),
                billing: crate::config::BILLING_REQUEST.into(),
                startup_cpu_boost: false,
                execution_environment: None,
                sandbox: false,
                vpc: None,
                cloud_sql: Vec::new(),
                custom_audiences: Vec::new(),
            },
            retry: Default::default(),
            apis: Default::default(),
            impersonate: None,
            project_tags: Default::default(),
            buckets: Default::default(),
            secrets: Default::default(),
        }
    }

    #[test]
    fn requirements_depend_on_configuration() {
        let img = deployment(false, false, false);
        assert!(!required_apis(&img).contains(&"cloudbuild.googleapis.com".to_string()));
        assert!(!required_project_permissions(&img).contains(&"run.services.setIamPolicy"));

        let full = deployment(true, true, true);
        let apis = required_apis(&full);
        for a in [
            "cloudbuild.googleapis.com",
            "artifactregistry.googleapis.com",
            "storage.googleapis.com",
            "secretmanager.googleapis.com",
        ] {
            assert!(apis.contains(&a.to_string()), "{a}");
        }
        let perms = required_project_permissions(&full);
        assert!(perms.contains(&"run.services.setIamPolicy"));
        assert!(perms.contains(&"cloudbuild.builds.create"));
    }
}
