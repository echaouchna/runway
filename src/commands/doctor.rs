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
    if creates_build || matches!(&d.artifact, Artifact::Promote(x) if x.create_resources) {
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

/// Who makes a call: the impersonated account, else the caller's own
/// credentials. `member` is its IAM member for a `gcloud` command.
struct Who {
    name: String,
    member: String,
}

impl Who {
    fn of(session: &Session) -> Self {
        match &session.impersonating {
            Some(i) => Self::account(&i.target),
            None => Self {
                name: "your credentials".into(),
                member: "user:YOU@example.com".into(),
            },
        }
    }
    fn account(email: &str) -> Self {
        Self {
            name: email.to_string(),
            member: format!("serviceAccount:{email}"),
        }
    }
}

/// The predefined role that grants a project permission runway checks, and
/// what it is needed for.
fn grant_for(permission: &str) -> (&'static str, &'static str) {
    match permission {
        "run.services.setIamPolicy" => ("roles/run.admin", "public access or IAP"),
        "run.services.createTagBinding" => ("roles/run.admin", "`service.tags`"),
        p if p.starts_with("run.") => ("roles/run.developer", "deploying Cloud Run services"),
        "logging.logEntries.list" => ("roles/logging.viewer", "showing why a revision failed"),
        "iam.serviceAccounts.create" => (
            "roles/iam.serviceAccountAdmin",
            "creating service accounts (`identity.create`, `create_build_resources`)",
        ),
        "resourcemanager.projects.setIamPolicy" => (
            "roles/resourcemanager.projectIamAdmin",
            "granting project roles (`identity.roles`, the build account's log writer)",
        ),
        "storage.buckets.create" => (
            "roles/storage.admin",
            "creating buckets (`buckets`, the source bucket of `create_build_resources`)",
        ),
        "artifactregistry.repositories.create" => (
            "roles/artifactregistry.admin",
            "creating the image repository (`provider.create_build_resources`)",
        ),
        "serviceusage.services.enable" => (
            "roles/serviceusage.serviceUsageAdmin",
            "enabling APIs (`provider.enable_apis`)",
        ),
        "resourcemanager.projects.createTagBinding" => {
            ("roles/resourcemanager.tagUser", "binding `provider.tags`")
        }
        p if p.starts_with("cloudbuild.") => (
            "roles/cloudbuild.builds.editor",
            "building images from source (`source`)",
        ),
        _ => ("", ""),
    }
}

/// What to grant for `missing` permissions on `project`: one line per role,
/// with the permissions it covers, why, and the command.
fn project_grants(who: &Who, project: &str, missing: &[&str]) -> String {
    let mut roles: Vec<(&str, &str, Vec<&str>)> = Vec::new();
    for p in missing {
        let (role, why) = grant_for(p);
        match roles.iter_mut().find(|(r, _, _)| *r == role) {
            Some((_, _, ps)) => ps.push(p),
            None => roles.push((role, why, vec![p])),
        }
    }
    let mut lines = vec![format!("grant {} on project {project}:", who.name)];
    for (role, why, perms) in roles {
        match role {
            "" => lines.push(format!(
                "  {} (see {}/permissions/)",
                perms.join(", "),
                crate::DOCS_URL
            )),
            _ => {
                lines.push(format!("  {role}: {} ({why})", perms.join(", ")));
                lines.push(format!(
                    "    gcloud projects add-iam-policy-binding {project} --member={} --role={role}",
                    who.member
                ));
            }
        }
    }
    lines.join("\n")
}

/// The command granting `role` on an Artifact Registry repository.
fn repository_grant(t: &crate::commands::release::Target, member: &str, role: &str) -> String {
    format!(
        "gcloud artifacts repositories add-iam-policy-binding {} --location={} --project={} --member={member} --role={role}",
        t.repository, t.location, t.project
    )
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
    let resolved = match config::load_and_resolve(&ctx.config, stage, &Overrides::default()) {
        Ok((_, res)) => {
            r.pass(
                "configuration",
                format!("{} is valid for stage {stage}", ctx.config.display()),
            );
            res
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

    let selected = match resolved.select(&args.stage.only) {
        Ok(s) => s,
        Err(e) => {
            r.fail("configuration", e.message, "fix --only");
            return finish(ctx, r);
        }
    };
    let d = selected[0].clone();
    let apis = crate::provision::stack_required_apis(&resolved);
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
    // The registry work runway does itself runs as this session; `None`
    // when the push account cannot be impersonated (its check says why).
    let push = match &d.push_service_account {
        None => Some(session.clone()),
        Some(sa) => {
            let imp = crate::gcp::Impersonation {
                target: sa.clone(),
                delegates: Vec::new(),
            };
            match session.impersonating.as_ref().map(|i| &i.target) == Some(sa) {
                true => Some(session.clone()),
                false => Session::connect(Some(&imp)).ok(),
            }
        }
    };
    // 3-9. Independent read-only checks, run concurrently.
    let workload_checks = |w: &'_ Deployment| {
        let session = &session;
        let push = push.as_ref();
        let w = w.clone();
        async move {
            let (accounts, repo, registry, bucket, secrets, service, foreign) = tokio::join!(
                check_service_accounts(session, &w),
                check_repository(session, &w),
                check_registry_access(session, push, &w),
                check_bucket(session, &w),
                check_secrets(session, &w),
                check_service(session, &w),
                check_foreign_grants(session, &w),
            );
            let mut out = Vec::new();
            for part in [accounts, repo, registry, bucket, secrets, service, foreign] {
                out.extend(part?.checks);
            }
            Ok::<_, crate::error::Error>((w.name().to_string(), out))
        }
    };
    let (principal, project, apis, perms, push, per_workload) = tokio::join!(
        principal(&session, &token),
        check_project(&session, &d),
        check_apis(&session, &d, &apis),
        check_permissions(&session, &d),
        check_push_account(&session, &d),
        futures::future::join_all(selected.iter().map(|w| workload_checks(w))),
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
    for part in [project, apis, perms, push] {
        r.checks.extend(part?.checks);
    }
    // Identical checks (a shared account or bucket) are reported once; with
    // several workloads, the others say which one they are about.
    let several = selected.len() > 1;
    let mut seen: Vec<(String, String)> = Vec::new();
    for part in per_workload {
        let (name, checks) = part?;
        for mut c in checks {
            let key = (c.name.clone(), c.detail.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            if several {
                c.name = format!("{} ({name})", c.name);
            }
            r.checks.push(c);
        }
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
async fn check_apis(session: &Session, d: &Deployment, apis: &[String]) -> Result<Report> {
    let mut r = Report::default();
    let project_name = format!("projects/{}", d.project);
    // 4. APIs.
    let usage = build_client!(ServiceUsage, session)?;
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
            let who = Who::of(session);
            if missing.is_empty() {
                r.pass(
                    "deployer permissions",
                    format!(
                        "{} has the {} permissions it needs on project {}",
                        who.name,
                        wanted.len(),
                        d.project
                    ),
                );
            } else {
                r.fail(
                    "deployer permissions",
                    format!(
                        "{} lacks on project {}: {}",
                        who.name,
                        d.project,
                        missing.join(", ")
                    ),
                    project_grants(&who, &d.project, &missing),
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
    let accounts = accounts(d);
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
    let who = Who::of(session);
    let act_as = &format!(
        "{} must be able to act as it once created: grant roles/iam.serviceAccountUser on project {} (covers accounts created later):\n  gcloud projects add-iam-policy-binding {} --member={} --role=roles/iam.serviceAccountUser",
        who.name, d.project, d.project, who.member
    );
    let parts = futures::future::join_all(accounts.iter().map(|(label, email, create)| async move {
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
                act_as.clone(),
            ),
            Err(e) if is_not_found(&e) => r.fail(
                *label,
                format!("{email} does not exist"),
                missing_account_hint(create),
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

/// The accounts a workload needs: (label, email, the setting with which
/// runway creates it when it is missing).
fn accounts(d: &Deployment) -> Vec<(&'static str, String, String)> {
    let mut out = vec![(
        "runtime service account",
        d.service.service_account.clone(),
        format!("set `{}.identity.create: true`", d.yaml_path()),
    )];
    if let Artifact::Build(b) = &d.artifact {
        out.push((
            "build service account",
            b.build_service_account.clone(),
            "set `provider.create_build_resources: true`".to_string(),
        ));
    }
    out
}

fn missing_account_hint(create: &str) -> String {
    format!(
        "create it ({}/getting-started/#prerequisites), or let runway create it: {create}",
        crate::DOCS_URL
    )
}

/// `provider.push_service_account`: runway can act as it, tested the way
/// deploy does it (impersonated from the caller's own credentials, not from
/// an impersonated deployer).
async fn check_push_account(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    let Some(email) = &d.push_service_account else {
        return Ok(r);
    };
    let label = "push service account";
    if session.impersonating.as_ref().map(|i| &i.target) == Some(email) {
        r.pass(label, format!("{email} (the impersonated account)"));
    } else {
        let imp = crate::gcp::Impersonation {
            target: email.clone(),
            delegates: Vec::new(),
        };
        let token = match Session::connect(Some(&imp)) {
            Ok(push) => push.verify().await,
            Err(e) => Err(e),
        };
        match token {
            Ok(()) => r.pass(
                label,
                format!("{email}: your credentials can act as it for registry work"),
            ),
            Err(e) => r.fail(
                label,
                format!("cannot act as {email} from your credentials: {}", e.message),
                format!(
                    "grant your credentials roles/iam.serviceAccountTokenCreator on {email} (impersonating another deployer does not help: runway impersonates it from your own credentials), or remove `provider.push_service_account`"
                ),
            ),
        }
    }
    Ok(r)
}

/// The repositories images go to, as `(label, target, deploy creates it)`:
/// the stage's own (builds, promoted copies), then the release repository
/// when it is another one (only `--tag`/`--tag-rc` use it).
fn repositories(d: &Deployment) -> Vec<(&'static str, crate::commands::release::Target, bool)> {
    let create = match &d.artifact {
        Artifact::Build(b) => b.create_resources,
        Artifact::Promote(p) => p.create_resources,
        Artifact::Image { .. } => false,
    };
    let stage = crate::commands::release::stage_target(d);
    let release = crate::commands::release::target(d);
    let same = |a: &crate::commands::release::Target, b: &crate::commands::release::Target| {
        (&a.project, &a.location, &a.repository) == (&b.project, &b.location, &b.repository)
    };
    let mut out = Vec::new();
    if let Some(t) = &stage {
        out.push(("artifact repository", t.clone(), create));
    }
    if let Some(t) = release
        && !stage.as_ref().is_some_and(|s| same(s, &t))
    {
        // `create_build_resources` creates it too (with a stage repository).
        out.push(("release repository", t, create && stage.is_some()));
    }
    out
}

fn repository_name(t: &crate::commands::release::Target) -> String {
    format!(
        "projects/{}/locations/{}/repositories/{}",
        t.project, t.location, t.repository
    )
}

/// The repositories exist, or deploy creates them (as the deployer, who
/// must then be able to read them).
async fn check_repository(session: &Session, d: &Deployment) -> Result<Report> {
    let mut r = Report::default();
    let repos = repositories(d);
    if repos.is_empty() {
        return Ok(r);
    }
    let ar = build_client!(ArtifactRegistry, session)?;
    let who = Who::of(session);
    for (label, t, create) in repos {
        let repo = repository_name(&t);
        let short = format!("{}/{} (project {})", t.location, t.repository, t.project);
        match ar.get_repository().set_name(&repo).send().await {
            Ok(rep) if rep.format != google_cloud_artifactregistry_v1::model::repository::Format::Docker => r.fail(
                label,
                format!("{short} is not a Docker repository"),
                "create a repository with --repository-format=docker",
            ),
            Ok(_) => r.pass(label, format!("{short} (Docker)")),
            Err(e) if is_not_found(&e) && create => r.warn(
                label,
                format!("{short} does not exist yet; deploy creates it"),
                format!(
                    "{} needs artifactregistry.repositories.create on project {} (roles/artifactregistry.admin)",
                    who.name, t.project
                ),
            ),
            Err(e) if is_not_found(&e) => r.fail(
                label,
                format!("{short} does not exist"),
                format!(
                    "gcloud artifacts repositories create {} --repository-format=docker --location={} --project={}",
                    t.repository, t.location, t.project
                ),
            ),
            // With `create_build_resources`, deploy reads it first (and
            // creates it when missing): it needs to see it.
            Err(e) if status_code(&e) == Some(Code::PermissionDenied) && create => r.fail(
                label,
                format!(
                    "{} cannot read {short}, so deploy cannot tell whether to create it (`provider.create_build_resources`)",
                    who.name
                ),
                format!(
                    "if it exists, grant {} roles/artifactregistry.reader on it:\n  {}\nif not, roles/artifactregistry.admin on project {} lets deploy create it:\n  gcloud projects add-iam-policy-binding {} --member={} --role=roles/artifactregistry.admin",
                    who.name,
                    repository_grant(&t, &who.member, "roles/artifactregistry.reader"),
                    t.project,
                    t.project,
                    who.member
                ),
            ),
            Err(e) if status_code(&e) == Some(Code::PermissionDenied) => r.warn(
                label,
                format!("{} cannot read {short}: whether it exists is not known", who.name),
                format!(
                    "deploy does not need to read it as {}; the registry access check below says who must",
                    who.name
                ),
            ),
            Err(e) => r.warn(label, format!("{short}: {}", short_err(&e)), "verify the repository exists"),
        }
    }
    Ok(r)
}

/// The account that pushes (`push_service_account`, else the deployer) can
/// do on each repository what deploy does there, and Cloud Run can pull
/// from those in another project.
async fn check_registry_access(
    session: &Session,
    push: Option<&Session>,
    d: &Deployment,
) -> Result<Report> {
    let mut r = Report::default();
    let repos = repositories(d);
    let Some(push) = push else {
        return Ok(r); // the push account check says why
    };
    if repos.is_empty() {
        return Ok(r);
    }
    let pusher = match &d.push_service_account {
        Some(sa) => Who::account(sa),
        None => Who::of(session),
    };
    let ar = build_client!(ArtifactRegistry, push)?;
    // What deploy grants the push account on the stage repository, and the
    // permissions that grant covers: only those are not missing.
    let planned_role = crate::provision::push_grant(d).map(|(_, b)| b.role);
    let covers = planned_role.as_deref().map_or(&[][..], covered);
    for (label, t, _) in &repos {
        let planned = planned_role
            .as_deref()
            .filter(|_| *label == "artifact repository");
        let granted =
            |perms: &[&str]| planned.is_some() && perms.iter().all(|p| covers.contains(p));
        let (needed, why) = needs(label, d);
        let name = format!("registry access: {}", t.repository);
        let short = format!("{}/{} (project {})", t.location, t.repository, t.project);
        match ar
            .test_iam_permissions()
            .set_resource(repository_name(t))
            .set_permissions(needed.iter().map(|p| p.to_string()))
            .send()
            .await
        {
            Ok(got) => {
                let missing: Vec<&str> = needed
                    .iter()
                    .filter(|p| !got.permissions.iter().any(|g| g == *p))
                    .copied()
                    .collect();
                let role = match missing.iter().any(|p| *p != READ) {
                    true => "roles/artifactregistry.writer",
                    false => "roles/artifactregistry.reader",
                };
                match missing.as_slice() {
                    [] => r.pass(
                        &name,
                        format!("{} can do what deploy does on {short}", pusher.name),
                    ),
                    _ if granted(&missing) => r.pass(
                        &name,
                        format!(
                            "deploy grants {} {} on {short}",
                            pusher.name,
                            planned.unwrap_or_default()
                        ),
                    ),
                    _ => r.fail(
                        &name,
                        format!(
                            "{} lacks on {short}: {} ({why})",
                            pusher.name,
                            missing.join(", ")
                        ),
                        format!(
                            "grant {} {role} on the repository:\n  {}",
                            pusher.name,
                            repository_grant(t, &pusher.member, role)
                        ),
                    ),
                }
            }
            Err(e) if is_not_found(&e) => {} // reported by the repository check
            Err(_) if granted(&needed) => r.pass(
                &name,
                format!(
                    "deploy grants {} {} on {short} once it exists",
                    pusher.name,
                    planned.unwrap_or_default()
                ),
            ),
            Err(e) => {
                let role = match needed.iter().any(|p| *p != READ) {
                    true => "roles/artifactregistry.writer",
                    false => "roles/artifactregistry.reader",
                };
                r.warn(
                    &name,
                    format!("cannot test {}'s access to {short}: {}", pusher.name, short_err(&e)),
                    format!(
                        "it needs {role} there ({why}); on the repository once it exists:\n  {}\nor on the whole project:\n  gcloud projects add-iam-policy-binding {} --member={} --role={role}",
                        repository_grant(t, &pusher.member, role),
                        t.project,
                        pusher.member
                    ),
                )
            }
        }
    }
    // Cloud Run pulls the image as the deployment project's service agent,
    // which reads repositories of its own project only.
    let foreign: Vec<_> = repos
        .iter()
        .filter(|(_, t, _)| t.project != d.project)
        .collect();
    if foreign.is_empty() {
        return Ok(r);
    }
    let projects = build_client!(Projects, session)?;
    let number = match projects
        .get_project()
        .set_name(format!("projects/{}", d.project))
        .send()
        .await
    {
        Ok(p) => p.name.trim_start_matches("projects/").to_string(),
        Err(_) => "PROJECT_NUMBER".to_string(),
    };
    let agent = format!("service-{number}@serverless-robot-prod.iam.gserviceaccount.com");
    let member = format!("serviceAccount:{agent}");
    let ar = build_client!(ArtifactRegistry, session)?;
    for (_, t, _) in foreign {
        let name = format!("Cloud Run pull: {}", t.repository);
        let short = format!("{}/{} (project {})", t.location, t.repository, t.project);
        let hint = format!(
            "Cloud Run pulls as {agent}, which needs roles/artifactregistry.reader there (runway does not grant it), on the repository:\n  {}\nor on project {}:\n  gcloud projects add-iam-policy-binding {} --member={member} --role=roles/artifactregistry.reader",
            repository_grant(t, &member, "roles/artifactregistry.reader"),
            t.project,
            t.project
        );
        // Access may be granted on the repository or on its project (also on
        // a folder, the organization or through a group: not visible here).
        let on_repository = match ar
            .get_iam_policy()
            .set_resource(repository_name(t))
            .send()
            .await
        {
            Ok(policy) => Some(can_pull(&policy.bindings, &member)),
            Err(e) if is_not_found(&e) => continue,
            Err(_) => None,
        };
        let on_project = match on_repository {
            Some(true) => Some(false),
            _ => projects
                .get_iam_policy()
                .set_resource(format!("projects/{}", t.project))
                .send()
                .await
                .ok()
                .map(|policy| can_pull(&policy.bindings, &member)),
        };
        match (on_repository, on_project) {
            (Some(true), _) => r.pass(&name, format!("{agent} can pull from {short}")),
            (_, Some(true)) => r.pass(
                &name,
                format!("{agent} can pull from {short} (granted on project {})", t.project),
            ),
            (Some(false), Some(false)) => r.warn(
                &name,
                format!(
                    "no grant lets {agent} pull from {short}, on the repository or its project; unless one is made on a folder, the organization or through a group, Cloud Run cannot pull"
                ),
                hint,
            ),
            _ => r.warn(
                &name,
                format!("cannot tell whether {agent} can pull from {short} (its IAM policies are not readable)"),
                hint,
            ),
        }
    }
    Ok(r)
}

const READ: &str = "artifactregistry.repositories.downloadArtifacts";
const WRITE: &str = "artifactregistry.repositories.uploadArtifacts";
const TAG: &str = "artifactregistry.tags.create";

/// What the account that pushes does on a repository (`label` from
/// [`repositories`]), and why: what deploy needs there.
fn needs(label: &str, d: &Deployment) -> (Vec<&'static str>, &'static str) {
    let tags_here = crate::commands::release::tags_in_stage_repository(d);
    match (label, &d.artifact) {
        ("release repository", _) => (
            vec![READ, WRITE, TAG],
            "copying images into it and tagging releases (`--tag`, `--tag-rc`)",
        ),
        (_, Artifact::Promote(_)) if tags_here => (
            vec![READ, WRITE, TAG],
            "copying promoted images into it and tagging releases (`--tag`, `--tag-rc`)",
        ),
        (_, Artifact::Promote(_)) => (vec![READ, WRITE], "copying promoted images into it"),
        // Releases copied into another package of the same repository: the
        // copy is an upload.
        _ if tags_here
            && crate::commands::release::target(d).map(|t| t.package)
                != crate::commands::release::stage_target(d).map(|t| t.package) =>
        {
            (
                vec![READ, WRITE, TAG],
                "copying builds into the release package and tagging releases (`--tag`, `--tag-rc`)",
            )
        }
        _ if tags_here => (
            vec![READ, TAG],
            "reading builds and tagging releases (`--tag`, `--tag-rc`)",
        ),
        _ => (
            vec![READ],
            "reading builds to copy them to the release repository",
        ),
    }
}

/// The repository permissions deploy needs that an Artifact Registry role
/// covers.
fn covered(role: &str) -> &'static [&'static str] {
    match role {
        "roles/artifactregistry.writer"
        | "roles/artifactregistry.repoAdmin"
        | "roles/artifactregistry.admin" => &[READ, WRITE, TAG],
        "roles/artifactregistry.reader" => &[READ],
        _ => &[],
    }
}

/// The bindings let `member` pull images (a role with downloadArtifacts).
fn can_pull(bindings: &[google_cloud_iam_v1::model::Binding], member: &str) -> bool {
    bindings.iter().any(|b| {
        matches!(
            b.role.as_str(),
            "roles/artifactregistry.reader"
                | "roles/artifactregistry.writer"
                | "roles/artifactregistry.repoAdmin"
                | "roles/artifactregistry.admin"
                | "roles/viewer"
                | "roles/editor"
                | "roles/owner"
        ) && b.members.iter().any(|m| m == member)
    })
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
    if d.is_job() {
        let jobs = build_client!(google_cloud_run_v2::client::Jobs, session)?;
        match jobs.get_job().set_name(d.service_name()).send().await {
            Ok(job) => match crate::gcp::jobs::check_job_ownership(&job, &d.app, &d.stage) {
                Ok(()) => r.pass(
                    "job",
                    format!("{} exists and is managed by runway", d.service_id),
                ),
                Err(e) => r.fail("job", e.message, "rename the job in runway.yaml"),
            },
            Err(e) if is_not_found(&e) => r.pass(
                "job",
                format!("{} will be created on first deploy", d.service_id),
            ),
            Err(e) => r.warn("job", short_err(&e), "check run.jobs.get permission"),
        }
        return Ok(r);
    }
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
                    format!(
                        "grant roles/resourcemanager.projectIamAdmin on {p}, or have an admin grant the roles listed in `{}.identity.roles`",
                        d.yaml_path()
                    ),
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
                    for (i, line) in h.lines().enumerate() {
                        let line = match i {
                            0 => format!("→ {line}"),
                            _ => format!("  {line}"),
                        };
                        println!("  {:<28} {}", "", p.cyan(&line));
                    }
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
            key: None,
            kind: crate::config::WorkloadKind::Service,
            artifact: if build {
                Artifact::Build(crate::config::BuildConfig {
                    context_dir: ".".into(),
                    strategy: crate::config::BuildStrategy::Dockerfile {
                        path: "Dockerfile".into(),
                    },
                    artifact_location: "europe-west1".into(),
                    artifact_project: "my-gcp-project".into(),
                    artifact_repository: "r".into(),
                    artifact_package: None,
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
                    origin: "`service.image`".into(),
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
                command: Vec::new(),
                args: Vec::new(),
                vpc: None,
                cloud_sql: Vec::new(),
                custom_audiences: Vec::new(),
                domains: Vec::new(),
                preview_domain: None,
            },
            retry: Default::default(),
            apis: Default::default(),
            impersonate: None,
            push_service_account: None,
            promote: None,
            scheduler_region: "europe-west1".into(),
            release: Default::default(),
            domains: Default::default(),
            project_tags: Default::default(),
            buckets: Default::default(),
            secrets: Default::default(),
        }
    }

    #[test]
    fn missing_permissions_say_who_where_which_role_and_why() {
        let who = Who::account("deployer@my-gcp-project.iam.gserviceaccount.com");
        let hint = project_grants(
            &who,
            "my-gcp-project",
            &[
                "artifactregistry.repositories.create",
                "cloudbuild.builds.create",
                "cloudbuild.builds.get",
            ],
        );
        let lines: Vec<&str> = hint.lines().collect();
        assert_eq!(
            lines[0],
            "grant deployer@my-gcp-project.iam.gserviceaccount.com on project my-gcp-project:"
        );
        assert!(lines[1].contains("roles/artifactregistry.admin: artifactregistry.repositories.create (creating the image repository"));
        assert!(lines[2].contains("gcloud projects add-iam-policy-binding my-gcp-project --member=serviceAccount:deployer@my-gcp-project.iam.gserviceaccount.com --role=roles/artifactregistry.admin"));
        assert!(
            lines[3].contains(
                "roles/cloudbuild.builds.editor: cloudbuild.builds.create, cloudbuild.builds.get"
            ),
            "one role, its permissions grouped: {hint}"
        );
        assert_eq!(lines.len(), 5, "{hint}");
    }

    #[test]
    fn the_repositories_checked_and_the_push_account_grant() {
        let mut d = deployment(true, false, false);
        let Artifact::Build(b) = &mut d.artifact else {
            unreachable!()
        };
        b.create_resources = true;
        d.release.repository = Some(crate::config::ReleaseRepository {
            project: "my-release-project".into(),
            location: "europe-west1".into(),
            repository: "releases".into(),
            package: None,
        });
        let repos: Vec<(&str, String, bool)> = repositories(&d)
            .into_iter()
            .map(|(l, t, c)| (l, t.image(), c))
            .collect();
        assert_eq!(
            repos,
            [
                (
                    "artifact repository",
                    "europe-west1-docker.pkg.dev/my-gcp-project/r/a".to_string(),
                    true
                ),
                (
                    "release repository",
                    "europe-west1-docker.pkg.dev/my-release-project/releases/a".to_string(),
                    true
                ),
            ]
        );
        // The push account reads builds out of the repository runway creates.
        d.push_service_account = Some("ci@my-ci-project.iam.gserviceaccount.com".into());
        let steps: Vec<String> = crate::provision::all_steps(&d)
            .iter()
            .map(|s| s.describe(&d))
            .collect();
        assert!(
            steps
                .iter()
                .any(|s| s.contains("roles/artifactregistry.reader")
                    && s.contains("europe-west1/r")
                    && s.contains("ci")),
            "{steps:#?}"
        );
        assert!(
            !steps
                .iter()
                .any(|s| s.contains("releases") && s.contains("ci@")),
            "nothing on the release repository runway does not own: {steps:#?}"
        );
    }

    #[test]
    fn the_push_grant_covers_what_deploy_does_there() {
        let mut d = deployment(true, false, false);
        let Artifact::Build(b) = &mut d.artifact else {
            unreachable!()
        };
        b.create_resources = true;
        assert!(
            crate::provision::push_grant(&d).is_none(),
            "no push account"
        );
        d.push_service_account = Some("ci@my-ci-project.iam.gserviceaccount.com".into());
        let role = |d: &Deployment| crate::provision::push_grant(d).map(|(_, b)| b.role);
        // No release repository: releases are tagged here, which needs writer.
        assert_eq!(role(&d).as_deref(), Some("roles/artifactregistry.writer"));
        assert!(covered("roles/artifactregistry.writer").contains(&TAG));
        // A release repository: builds are only read here.
        d.release.repository = Some(crate::config::ReleaseRepository {
            project: "my-release-project".into(),
            location: "europe-west1".into(),
            repository: "releases".into(),
            package: None,
        });
        assert_eq!(role(&d).as_deref(), Some("roles/artifactregistry.reader"));
        assert!(
            !covered("roles/artifactregistry.reader").contains(&TAG),
            "reader never tags"
        );
        assert_eq!(needs("artifact repository", &d).0, [READ]);

        // A release repository naming the build repository itself (another
        // package): releases are tagged here, as without one.
        d.release.repository = Some(crate::config::ReleaseRepository {
            project: "my-gcp-project".into(),
            location: "europe-west1".into(),
            repository: "r".into(),
            package: Some("team/agent".into()),
        });
        assert!(crate::commands::release::tags_in_stage_repository(&d));
        assert_eq!(role(&d).as_deref(), Some("roles/artifactregistry.writer"));
        let repos = repositories(&d);
        assert_eq!(repos.len(), 1, "one repository, checked once");
        let needed = needs(repos[0].0, &d).0;
        assert!(
            needed.contains(&TAG),
            "doctor requires tagging there: {needed:?}"
        );
        assert!(
            needed.contains(&WRITE),
            "and uploading: the release is copied into another package: {needed:?}"
        );
        assert!(
            needed
                .iter()
                .all(|p| covered("roles/artifactregistry.writer").contains(p)),
            "and the grant deploy makes covers it"
        );
        // The same package: tagged in place, nothing copied.
        if let Some(r) = &mut d.release.repository {
            r.package = None;
        }
        assert_eq!(needs("artifact repository", &d).0, [READ, TAG]);
        let Artifact::Build(b) = &mut d.artifact else {
            unreachable!()
        };
        b.create_resources = false;
        assert!(
            role(&d).is_none(),
            "nothing granted on a repository runway does not create"
        );
    }

    #[test]
    fn pull_access_counts_the_repository_and_its_project() {
        use google_cloud_iam_v1::model::Binding;
        let agent = "serviceAccount:service-123@serverless-robot-prod.iam.gserviceaccount.com";
        let b = |role: &str, m: &str| Binding::new().set_role(role).set_members([m]);
        assert!(can_pull(
            &[b("roles/artifactregistry.reader", agent)],
            agent
        ));
        assert!(
            can_pull(&[b("roles/viewer", agent)], agent),
            "a project viewer pulls"
        );
        assert!(!can_pull(
            &[b("roles/artifactregistry.reader", "user:a@example.com")],
            agent
        ));
        assert!(!can_pull(&[b("roles/run.invoker", agent)], agent));
    }

    #[test]
    fn a_missing_account_hint_names_the_setting_that_creates_it() {
        let hints = |d: &Deployment| -> Vec<String> {
            accounts(d)
                .iter()
                .map(|(_, _, create)| missing_account_hint(create))
                .collect()
        };
        let main = hints(&deployment(true, false, false));
        assert!(
            main[0].contains("`service.identity.create: true`"),
            "{main:?}"
        );
        assert!(
            main[1].contains("`provider.create_build_resources: true`")
                && !main[1].contains("identity"),
            "{main:?}"
        );
        assert!(
            main.iter()
                .all(|h| h.contains("/getting-started/#prerequisites"))
        );

        let mut job = deployment(false, false, false);
        job.key = Some("migrate".into());
        job.kind = crate::config::WorkloadKind::Job(crate::config::JobSettings {
            tasks: 1,
            parallelism: 0,
            max_retries: 3,
        });
        let job = hints(&job);
        assert_eq!(job.len(), 1, "an image deploy has no build account");
        assert!(
            job[0].contains("`jobs.migrate.identity.create: true`"),
            "{job:?}"
        );
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
