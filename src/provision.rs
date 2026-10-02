//! Stateless provisioning steps around the Cloud Run service.
//!
//! Each [`Step`] has a read-only [`Provisioner::check`] (used by `plan`) and an
//! idempotent [`Provisioner::apply`] (used by `deploy`): apply re-reads the
//! live state and changes only what is missing, so re-running after a partial
//! failure resumes where it stopped. No state file is involved.
//!
//! Grants, tags and IAP members are **additive**: runway adds what the
//! configuration declares and never removes members or tags it did not
//! declare (without a state file it cannot know who added them).

use crate::build_client_at;
use crate::config::{Artifact, BucketConfig, Deployment, ManagedSecret, RoleBinding, RoleTarget};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::bucket;
use crate::gcp::{
    GaxError, Session, api_error, iam, is_ambiguous, is_concurrency_conflict, is_not_found,
    status_code,
};
use crate::naming;
use google_cloud_api_serviceusage_v1::client::ServiceUsage;
use google_cloud_artifactregistry_v1::client::ArtifactRegistry;
use google_cloud_artifactregistry_v1::model::{Repository, repository::Format};
use google_cloud_bigquery_v2::client::DatasetService;
use google_cloud_bigquery_v2::model::{Access, Dataset};
use google_cloud_gax::error::rpc::Code;
use google_cloud_iam_admin_v1::client::Iam;
use google_cloud_iam_admin_v1::model::ServiceAccount;
use google_cloud_iam_v1::model::{GetPolicyOptions, Policy};
use google_cloud_iap_v1::client::IdentityAwareProxyAdminService;
use google_cloud_lro::Poller;
use google_cloud_resourcemanager_v3::client::{Projects, TagBindings, TagValues};
use google_cloud_resourcemanager_v3::model::TagBinding;
use google_cloud_run_v2::client::Services;
use google_cloud_secretmanager_v1::client::SecretManagerService;
use google_cloud_storage::client::StorageControl;
use serde::Serialize;
use tokio::sync::OnceCell;

pub const IAP_ACCESSOR_ROLE: &str = "roles/iap.httpsResourceAccessor";

/// Upper bound for long-running operations (API enablement, repository creation).
const LRO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// How long to wait for a new project tag binding to become effective.
const TAG_PROPAGATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

enum BucketState {
    Missing,
    InSync,
    Drift(Box<google_cloud_storage::model::Bucket>, Vec<&'static str>),
    /// Exists, but runway did not create it for this app: used as is.
    NotOwned(String),
}

/// One provisioning step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Enable APIs on the deployment project.
    EnableApis(Vec<String>),
    /// Bind a Resource Manager tag to the deployment project and wait until
    /// it is effective (organization policy conditions read it).
    ProjectTag { key: String, value: String },
    /// Create a bucket or bring its settings in line.
    CreateBucket(BucketConfig),
    /// Create a Secret Manager secret (without a value).
    CreateSecret(ManagedSecret),
    /// Grant a role to principals that are not service accounts runway
    /// manages (for example the people allowed to add secret values).
    GrantMembers {
        members: Vec<String>,
        binding: RoleBinding,
    },
    /// Stop the deployment until every secret runway created has a value.
    SecretValues(Vec<String>),
    /// Create the Artifact Registry Docker repository.
    CreateRepository {
        location: String,
        repository: String,
    },
    /// Create a service account if it does not exist.
    CreateServiceAccount {
        email: String,
        display_name: String,
        /// Ownership marker stored in the account description (see [`sa_marker`]).
        description: String,
    },
    /// Grant a role to a service account.
    Grant { email: String, binding: RoleBinding },
    /// Bind a Resource Manager tag to the service.
    Tag { key: String, value: String },
    /// Let the IAP service agent invoke the service.
    IapInvoker,
    /// Grant `roles/iap.httpsResourceAccessor` to the configured members.
    IapAccess,
}

impl Step {
    pub fn describe(&self, d: &Deployment) -> String {
        match self {
            Step::EnableApis(apis) => format!("{} API(s) enabled on {}", apis.len(), d.project),
            Step::ProjectTag { key, value } => {
                format!("project tag {key}={value} on {}", d.project)
            }
            Step::CreateBucket(b) => format!("bucket gs://{} ({})", b.name, b.location),
            Step::CreateSecret(s) => format!("secret {}", s.name),
            Step::GrantMembers { members, binding } => format!(
                "grant {} on {} to {}",
                binding.role,
                binding.target,
                members.join(", ")
            ),
            Step::SecretValues(names) => format!(
                "value of secret(s) {}",
                names
                    .iter()
                    .map(|n| n.rsplit('/').next().unwrap_or(n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Step::CreateRepository {
                location,
                repository,
            } => {
                format!("Artifact Registry repository {location}/{repository}")
            }
            Step::CreateServiceAccount { email, .. } => format!("service account {email}"),
            Step::Grant { email, binding } => {
                let who = email.split('@').next().unwrap_or(email);
                format!("grant {} on {} to {who}", binding.role, binding.target)
            }
            Step::Tag { key, value } => format!("tag {key}={value}"),
            Step::IapInvoker => "IAP service agent can invoke the service".into(),
            Step::IapAccess => format!("IAP access for {}", d.service.iap.members.join(", ")),
        }
    }

    /// Steps that need the service to exist.
    pub fn needs_service(&self) -> bool {
        matches!(self, Step::Tag { .. } | Step::IapInvoker | Step::IapAccess)
    }
}

/// Ownership marker written into the description of service accounts runway
/// creates. `undeploy` only deletes accounts carrying a matching marker.
pub fn sa_marker(app: &str, stage: Option<&str>, role: &str) -> String {
    let mut m = format!("managed-by=runway app={app}");
    if let Some(s) = stage {
        m.push_str(&format!(" stage={s}"));
    }
    m.push_str(&format!(" role={role}"));
    m
}

/// True if `description` carries runway's marker for this app, stage and role.
pub fn has_sa_marker(description: &str, app: &str, stage: Option<&str>, role: &str) -> bool {
    let tokens: std::collections::BTreeSet<&str> = description.split_whitespace().collect();
    tokens.contains("managed-by=runway")
        && tokens.contains(format!("app={app}").as_str())
        && tokens.contains(format!("role={role}").as_str())
        && stage.is_none_or(|s| tokens.contains(format!("stage={s}").as_str()))
}

/// APIs the deployment project needs for this configuration, plus declared extras.
pub fn required_apis(d: &Deployment) -> Vec<String> {
    let mut apis: Vec<String> = [
        "run.googleapis.com",
        "logging.googleapis.com",
        "iam.googleapis.com",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut add = |a: &str| apis.push(a.to_string());
    if let Artifact::Build(b) = &d.artifact {
        add("cloudbuild.googleapis.com");
        add("artifactregistry.googleapis.com");
        add("storage.googleapis.com");
        if b.create_resources {
            add("cloudresourcemanager.googleapis.com");
        }
    }
    let roles = &d.service.identity.roles;
    let uses = |f: fn(&RoleTarget) -> bool| roles.iter().any(|r| f(&r.target));
    if d.service.all_secrets().next().is_some()
        || !d.secrets.is_empty()
        || uses(|t| matches!(t, RoleTarget::Secret { .. }))
    {
        add("secretmanager.googleapis.com");
    }
    if uses(|t| matches!(t, RoleTarget::Dataset { .. })) {
        add("bigquery.googleapis.com");
    }
    if uses(|t| matches!(t, RoleTarget::Bucket { .. }))
        || !d.service.volumes.is_empty()
        || !d.buckets.is_empty()
    {
        add("storage.googleapis.com");
    }
    if !d.service.tags.is_empty()
        || !d.project_tags.is_empty()
        || uses(|t| matches!(t, RoleTarget::Project { .. }))
        || !d.buckets.is_empty()
    {
        add("cloudresourcemanager.googleapis.com");
    }
    if d.service.iap.enabled {
        add("iap.googleapis.com");
        add("cloudresourcemanager.googleapis.com");
    }
    if d.service.otel_collector.is_some() {
        add("cloudtrace.googleapis.com");
        add("monitoring.googleapis.com");
        add("logging.googleapis.com");
    }
    apis.extend(d.apis.extra.iter().cloned());
    apis.sort();
    apis.dedup();
    apis
}

/// API enablement runs first, before anything else is read or created.
pub fn api_step(d: &Deployment) -> Option<Step> {
    d.apis.enable.then(|| Step::EnableApis(required_apis(d)))
}

/// Steps that run before the image is built and the service is deployed, in
/// dependency order: project tags (organization policies may depend on them),
/// buckets and repository, then service accounts, then grants.
pub fn pre_steps(d: &Deployment) -> Vec<Step> {
    let mut steps: Vec<Step> = d
        .project_tags
        .iter()
        .map(|(k, v)| Step::ProjectTag {
            key: k.clone(),
            value: v.clone(),
        })
        .collect();
    steps.extend(d.buckets.values().cloned().map(Step::CreateBucket));
    steps.extend(d.secrets.values().cloned().map(Step::CreateSecret));
    let mut grants = Vec::new();
    let mut accounts = Vec::new();
    if let Artifact::Build(b) = &d.artifact
        && b.create_resources
    {
        steps.push(Step::CreateBucket(BucketConfig {
            key: "build-sources".into(),
            name: b.source_bucket.clone(),
            location: d.region.clone(),
            storage_class: None,
            versioning: None,
            delete_after_days: Some(30),
            labels: Default::default(),
        }));
        steps.push(Step::CreateRepository {
            location: b.artifact_location.clone(),
            repository: b.artifact_repository.clone(),
        });
        accounts.push(Step::CreateServiceAccount {
            email: b.build_service_account.clone(),
            display_name: format!("runway builds ({})", d.app),
            description: sa_marker(&d.app, None, "build"),
        });
        let grant = |role: &str, target: RoleTarget| Step::Grant {
            email: b.build_service_account.clone(),
            binding: RoleBinding {
                role: role.into(),
                target,
            },
        };
        grants.push(grant(
            "roles/logging.logWriter",
            RoleTarget::Project {
                project: d.project.clone(),
            },
        ));
        grants.push(grant(
            "roles/artifactregistry.writer",
            RoleTarget::Repository {
                project: d.project.clone(),
                location: b.artifact_location.clone(),
                repository: b.artifact_repository.clone(),
            },
        ));
        grants.push(grant(
            "roles/storage.objectViewer",
            RoleTarget::Bucket {
                bucket: b.source_bucket.clone(),
            },
        ));
    }
    if d.service.identity.create {
        accounts.push(Step::CreateServiceAccount {
            email: d.service.service_account.clone(),
            display_name: d
                .service
                .identity
                .display_name
                .clone()
                .unwrap_or_else(|| format!("runway runtime for {}", d.service_id)),
            description: sa_marker(&d.app, Some(&d.stage), "runtime"),
        });
    }
    grants.extend(
        d.service
            .identity
            .roles
            .iter()
            .cloned()
            .map(|binding| Step::Grant {
                email: d.service.service_account.clone(),
                binding,
            }),
    );
    // Access the configuration implies: the runtime account reads its
    // secrets and the buckets it mounts.
    let runtime = &d.service.service_account;
    if !runtime.is_empty() {
        let mut implied: Vec<RoleBinding> = Vec::new();
        for s in d.service.all_secrets() {
            implied.push(RoleBinding {
                role: "roles/secretmanager.secretAccessor".into(),
                target: RoleTarget::Secret {
                    name: s.full_name(&d.project),
                },
            });
        }
        for v in d.service.volumes.values() {
            implied.push(RoleBinding {
                role: if v.read_only {
                    "roles/storage.objectViewer".into()
                } else {
                    "roles/storage.objectUser".into()
                },
                target: RoleTarget::Bucket {
                    bucket: v.bucket.clone(),
                },
            });
        }
        for binding in implied {
            let step = Step::Grant {
                email: runtime.clone(),
                binding,
            };
            if !grants.contains(&step) {
                grants.push(step);
            }
        }
    }
    for s in d.secrets.values().filter(|s| !s.adders.is_empty()) {
        grants.push(Step::GrantMembers {
            members: s.adders.clone(),
            binding: RoleBinding {
                role: SECRET_ADDER_ROLE.into(),
                target: RoleTarget::Secret {
                    name: format!("projects/{}/secrets/{}", d.project, s.name),
                },
            },
        });
    }
    steps.extend(accounts);
    steps.extend(grants);
    if !d.secrets.is_empty() {
        steps.push(Step::SecretValues(
            d.secrets
                .values()
                .map(|s| format!("projects/{}/secrets/{}", d.project, s.name))
                .collect(),
        ));
    }
    steps
}

/// Role of the people allowed to add values to secrets runway creates.
pub const SECRET_ADDER_ROLE: &str = "roles/secretmanager.secretVersionAdder";

/// Steps that run after the service exists (before public access is applied,
/// so that a tag allowing public access is in place first).
pub fn post_steps(d: &Deployment) -> Vec<Step> {
    let mut steps: Vec<Step> = d
        .service
        .tags
        .iter()
        .map(|(k, v)| Step::Tag {
            key: k.clone(),
            value: v.clone(),
        })
        .collect();
    if d.service.iap.enabled {
        steps.push(Step::IapInvoker);
        if !d.service.iap.members.is_empty() {
            steps.push(Step::IapAccess);
        }
    }
    steps
}

/// Every step in execution order (APIs, pre-deploy, post-deploy).
pub fn all_steps(d: &Deployment) -> Vec<Step> {
    api_step(d)
        .into_iter()
        .chain(pre_steps(d))
        .chain(post_steps(d))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Already in the desired state.
    InSync,
    /// Deploy will make a change.
    Pending,
    /// Could not be determined (for example, missing read permission).
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepCheck {
    pub step: String,
    pub state: StepState,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Unchanged,
    Changed,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub step: String,
    pub outcome: StepOutcome,
    pub detail: String,
}

/// Where an IAM policy lives.
#[derive(Debug, Clone)]
enum PolicyTarget {
    Project(String),
    Bucket(String),
    Secret(String),
    RunService(String),
    Iap(String),
    Repository(String),
}

impl std::fmt::Display for PolicyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyTarget::Project(p) => write!(f, "project {p}"),
            PolicyTarget::Bucket(b) => write!(f, "bucket gs://{b}"),
            PolicyTarget::Secret(s) => write!(f, "secret {s}"),
            PolicyTarget::RunService(s) => write!(f, "service {}", naming_short(s)),
            PolicyTarget::Iap(r) => write!(f, "IAP resource {r}"),
            PolicyTarget::Repository(r) => write!(f, "repository {r}"),
        }
    }
}

fn naming_short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn needed_role_hint(t: &PolicyTarget) -> &'static str {
    match t {
        PolicyTarget::Project(_) => {
            "the deployer needs resourcemanager.projects.setIamPolicy (roles/resourcemanager.projectIamAdmin) on that project"
        }
        PolicyTarget::Bucket(_) => {
            "the deployer needs storage.buckets.setIamPolicy (roles/storage.admin) on that bucket"
        }
        PolicyTarget::Secret(_) => {
            "the deployer needs secretmanager.secrets.setIamPolicy (roles/secretmanager.admin) on that secret"
        }
        PolicyTarget::RunService(_) => {
            "the deployer needs run.services.setIamPolicy (roles/run.admin)"
        }
        PolicyTarget::Iap(_) => {
            "the deployer needs roles/iap.admin, and iap.googleapis.com must be enabled"
        }
        PolicyTarget::Repository(_) => {
            "the deployer needs artifactregistry.repositories.setIamPolicy (roles/artifactregistry.admin) on the repository"
        }
    }
}

/// Normalizes BigQuery dataset access roles (legacy names <-> IAM roles).
pub fn normalize_dataset_role(role: &str) -> &str {
    match role {
        "READER" => "roles/bigquery.dataViewer",
        "WRITER" => "roles/bigquery.dataEditor",
        "OWNER" => "roles/bigquery.dataOwner",
        r => r,
    }
}

/// True if the dataset access list already grants `role` to the service account.
pub fn dataset_grants(access: &[Access], role: &str, email: &str) -> bool {
    let member = format!("serviceAccount:{email}");
    access.iter().any(|a| {
        a.condition.is_none()
            && normalize_dataset_role(&a.role) == normalize_dataset_role(role)
            && (a.user_by_email.eq_ignore_ascii_case(email) || a.iam_member == member)
    })
}

/// IAP resource for a Cloud Run service.
pub fn iap_resource(project_number: &str, region: &str, service_id: &str) -> String {
    format!("projects/{project_number}/iap_web/cloud_run-{region}/services/{service_id}")
}

/// Tag binding parent for a Cloud Run service.
pub fn tag_parent(project: &str, region: &str, service_id: &str) -> String {
    format!("//run.googleapis.com/projects/{project}/locations/{region}/services/{service_id}")
}

fn replication_display(locations: &[String]) -> String {
    if locations.is_empty() {
        "automatic replication".into()
    } else {
        format!("replicated in {}", locations.join(", "))
    }
}

pub fn iap_service_agent(project_number: &str) -> String {
    format!("service-{project_number}@gcp-sa-iap.iam.gserviceaccount.com")
}

/// Clients are created only for the step types a configuration uses.
#[derive(Default)]
struct Clients {
    iam: Option<Iam>,
    projects: Option<Projects>,
    storage: Option<StorageControl>,
    secrets: Option<SecretManagerService>,
    datasets: Option<DatasetService>,
    tag_bindings: Option<TagBindings>,
    /// Global endpoint, for bindings on the project.
    project_tag_bindings: Option<TagBindings>,
    tag_values: Option<TagValues>,
    iap: Option<IdentityAwareProxyAdminService>,
    service_usage: Option<ServiceUsage>,
    artifact: Option<ArtifactRegistry>,
}

/// Optional API endpoint overrides (tests, private endpoints).
#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    pub iam: Option<String>,
    /// Resource Manager (projects, tag values).
    pub resource_manager: Option<String>,
    /// Regional Resource Manager endpoint for tag bindings.
    pub tag_bindings: Option<String>,
    pub bigquery: Option<String>,
    pub iap: Option<String>,
    pub secret_manager: Option<String>,
    /// Base URL of the Service Usage REST API.
    pub service_usage: Option<String>,
    pub artifact_registry: Option<String>,
}

pub struct Provisioner<'a> {
    pub d: &'a Deployment,
    session: &'a Session,
    run: &'a Services,
    clients: Clients,
    project_number: OnceCell<String>,
    service_usage: String,
}

fn missing<T>(c: &Option<T>, what: &str) -> Result<T>
where
    T: Clone,
{
    c.clone()
        .ok_or_else(|| Error::internal(format!("{what} client not initialized")))
}

impl<'a> Provisioner<'a> {
    pub async fn new(d: &'a Deployment, session: &'a Session, run: &'a Services) -> Result<Self> {
        Self::with_endpoints(d, session, run, &Endpoints::default()).await
    }

    pub async fn with_endpoints(
        d: &'a Deployment,
        session: &'a Session,
        run: &'a Services,
        ep: &Endpoints,
    ) -> Result<Self> {
        Self::with_options(d, session, run, ep, false).await
    }

    /// Clients needed to tear down what `deploy` created (`undeploy`).
    pub async fn for_teardown(
        d: &'a Deployment,
        session: &'a Session,
        run: &'a Services,
    ) -> Result<Self> {
        Self::with_options(d, session, run, &Endpoints::default(), true).await
    }

    pub async fn with_options(
        d: &'a Deployment,
        session: &'a Session,
        run: &'a Services,
        ep: &Endpoints,
        teardown: bool,
    ) -> Result<Self> {
        let steps = all_steps(d);
        let mut c = Clients::default();
        let has = |f: &dyn Fn(&Step) -> bool| steps.iter().any(f);
        let grant = |t: fn(&RoleTarget) -> bool| move |s: &Step| matches!(s, Step::Grant { binding, .. } if t(&binding.target));
        if teardown || has(&|s| matches!(s, Step::CreateServiceAccount { .. })) {
            c.iam = Some(build_client_at!(Iam, session, ep.iam.clone())?);
        }
        if has(&grant(|t| matches!(t, RoleTarget::Project { .. })))
            || has(&|s| matches!(s, Step::CreateBucket(_)))
            || d.service.iap.enabled
        {
            c.projects = Some(build_client_at!(
                Projects,
                session,
                ep.resource_manager.clone()
            )?);
        }
        if has(&grant(|t| matches!(t, RoleTarget::Bucket { .. })))
            || has(&|s| matches!(s, Step::CreateBucket(_)))
        {
            c.storage = Some(
                StorageControl::builder()
                    .with_credentials(session.credentials.clone())
                    .build()
                    .await
                    .map_err(|e| {
                        Error::internal(format!("cannot create StorageControl client: {e}"))
                    })?,
            );
        }
        if has(&grant(|t| matches!(t, RoleTarget::Secret { .. })))
            || !d.secrets.is_empty()
            || d.service.all_secrets().next().is_some()
        {
            c.secrets = Some(build_client_at!(
                SecretManagerService,
                session,
                ep.secret_manager.clone()
            )?);
        }
        if has(&grant(|t| matches!(t, RoleTarget::Dataset { .. }))) {
            c.datasets = Some(build_client_at!(
                DatasetService,
                session,
                ep.bigquery.clone()
            )?);
        }
        if teardown
            || has(&grant(|t| matches!(t, RoleTarget::Repository { .. })))
            || has(&|s| matches!(s, Step::CreateRepository { .. }))
        {
            c.artifact = Some(build_client_at!(
                ArtifactRegistry,
                session,
                ep.artifact_registry.clone()
            )?);
        }
        if has(&|s| matches!(s, Step::EnableApis(_))) {
            c.service_usage = Some(build_client_at!(
                ServiceUsage,
                session,
                ep.service_usage.clone()
            )?);
        }
        if !d.project_tags.is_empty() {
            c.project_tag_bindings = Some(build_client_at!(
                TagBindings,
                session,
                ep.resource_manager.clone()
            )?);
            c.tag_values = Some(build_client_at!(
                TagValues,
                session,
                ep.resource_manager.clone()
            )?);
            if c.projects.is_none() {
                c.projects = Some(build_client_at!(
                    Projects,
                    session,
                    ep.resource_manager.clone()
                )?);
            }
        }
        if !d.service.tags.is_empty() {
            // Tag bindings on regional resources use the regional endpoint.
            let regional = ep.tag_bindings.clone().unwrap_or_else(|| {
                format!("https://{}-cloudresourcemanager.googleapis.com", d.region)
            });
            c.tag_bindings = Some(build_client_at!(TagBindings, session, Some(regional))?);
            c.tag_values = Some(build_client_at!(
                TagValues,
                session,
                ep.resource_manager.clone()
            )?);
        }
        if d.service.iap.enabled {
            c.iap = Some(build_client_at!(
                IdentityAwareProxyAdminService,
                session,
                ep.iap.clone()
            )?);
        }
        Ok(Self {
            d,
            session,
            run,
            clients: c,
            project_number: OnceCell::new(),
            service_usage: ep
                .service_usage
                .clone()
                .unwrap_or_else(|| "https://serviceusage.googleapis.com".into()),
        })
    }

    async fn project_number(&self) -> Result<String> {
        self.project_number
            .get_or_try_init(|| async {
                let p = missing(&self.clients.projects, "Projects")?
                    .get_project()
                    .set_name(format!("projects/{}", self.d.project))
                    .send()
                    .await
                    .map_err(|e| api_error(e, &format!("reading project {}", self.d.project)))?;
                Ok::<_, Error>(p.name.trim_start_matches("projects/").to_string())
            })
            .await
            .cloned()
    }

    fn sa_member(email: &str) -> String {
        format!("serviceAccount:{email}")
    }

    // ---------- IAM policy plumbing ----------

    async fn get_policy(&self, t: &PolicyTarget) -> std::result::Result<Policy, GaxError> {
        let v3 = GetPolicyOptions::new().set_requested_policy_version(3);
        match t {
            PolicyTarget::Project(p) => {
                missing_gax(&self.clients.projects)?
                    .get_iam_policy()
                    .set_resource(format!("projects/{p}"))
                    .set_options(v3)
                    .send()
                    .await
            }
            PolicyTarget::Bucket(b) => {
                missing_gax(&self.clients.storage)?
                    .get_iam_policy()
                    .set_resource(format!("projects/_/buckets/{b}"))
                    .set_options(v3)
                    .send()
                    .await
            }
            PolicyTarget::Secret(s) => {
                missing_gax(&self.clients.secrets)?
                    .get_iam_policy()
                    .set_resource(s)
                    .set_options(v3)
                    .send()
                    .await
            }
            PolicyTarget::RunService(n) => {
                self.run
                    .get_iam_policy()
                    .set_resource(n)
                    .set_options(v3)
                    .send()
                    .await
            }
            PolicyTarget::Iap(r) => {
                missing_gax(&self.clients.iap)?
                    .get_iam_policy()
                    .set_resource(r)
                    .set_options(v3)
                    .send()
                    .await
            }
            PolicyTarget::Repository(r) => {
                missing_gax(&self.clients.artifact)?
                    .get_iam_policy()
                    .set_resource(r)
                    .set_options(v3)
                    .send()
                    .await
            }
        }
    }

    async fn set_policy(
        &self,
        t: &PolicyTarget,
        p: Policy,
    ) -> std::result::Result<Policy, GaxError> {
        match t {
            PolicyTarget::Project(r) => {
                missing_gax(&self.clients.projects)?
                    .set_iam_policy()
                    .set_resource(format!("projects/{r}"))
                    .set_policy(p)
                    .send()
                    .await
            }
            PolicyTarget::Bucket(b) => {
                missing_gax(&self.clients.storage)?
                    .set_iam_policy()
                    .set_resource(format!("projects/_/buckets/{b}"))
                    .set_policy(p)
                    .send()
                    .await
            }
            PolicyTarget::Secret(s) => {
                missing_gax(&self.clients.secrets)?
                    .set_iam_policy()
                    .set_resource(s)
                    .set_policy(p)
                    .send()
                    .await
            }
            PolicyTarget::RunService(n) => {
                self.run
                    .set_iam_policy()
                    .set_resource(n)
                    .set_policy(p)
                    .send()
                    .await
            }
            PolicyTarget::Iap(r) => {
                missing_gax(&self.clients.iap)?
                    .set_iam_policy()
                    .set_resource(r)
                    .set_policy(p)
                    .send()
                    .await
            }
            PolicyTarget::Repository(r) => {
                missing_gax(&self.clients.artifact)?
                    .set_iam_policy()
                    .set_resource(r)
                    .set_policy(p)
                    .send()
                    .await
            }
        }
    }

    async fn check_members(
        &self,
        t: &PolicyTarget,
        role: &str,
        members: &[String],
    ) -> (StepState, String) {
        match self.get_policy(t).await {
            Ok(p) => {
                let missing = iam::missing_members(&p, role, members);
                if missing.is_empty() {
                    (StepState::InSync, format!("{role} already granted on {t}"))
                } else {
                    (
                        StepState::Pending,
                        format!("will grant {role} on {t} to {}", missing.join(", ")),
                    )
                }
            }
            Err(e) if is_not_found(&e) => (
                StepState::Pending,
                format!("{t} not found yet; will grant {role}"),
            ),
            // Artifact Registry answers PERMISSION_DENIED (not NOT_FOUND) for the
            // policy of a repository that does not exist yet.
            Err(_)
                if matches!(t, PolicyTarget::Repository(_)) && self.repository_missing(t).await =>
            {
                (
                    StepState::Pending,
                    format!("{t} not created yet; will grant {role}"),
                )
            }
            Err(e) => (
                StepState::Unknown,
                format!("cannot read the policy of {t}: {}", short(&e)),
            ),
        }
    }

    async fn repository_missing(&self, t: &PolicyTarget) -> bool {
        let PolicyTarget::Repository(name) = t else {
            return false;
        };
        match &self.clients.artifact {
            Some(c) => {
                matches!(c.get_repository().set_name(name).send().await, Err(e) if is_not_found(&e))
            }
            None => false,
        }
    }

    /// Read-modify-write with etag; retries stale-etag conflicts.
    async fn ensure_members(
        &self,
        t: &PolicyTarget,
        role: &str,
        members: &[String],
    ) -> Result<StepOutcome> {
        for attempt in 1..=4 {
            let mut p = self.get_policy(t).await.map_err(|e| {
                api_error(e, &format!("reading the IAM policy of {t}")).hint(needed_role_hint(t))
            })?;
            if !iam::add_members(&mut p, role, members) {
                return Ok(StepOutcome::Unchanged);
            }
            match self.set_policy(t, p).await {
                Ok(_) => return Ok(StepOutcome::Changed),
                Err(e) if (is_concurrency_conflict(&e) || is_ambiguous(&e)) && attempt < 4 => {
                    continue;
                }
                Err(e) => {
                    return Err(
                        api_error(e, &format!("granting {role} on {t}")).hint(needed_role_hint(t))
                    );
                }
            }
        }
        unreachable!("loop returns")
    }

    fn grant_target(&self, b: &RoleBinding) -> Option<PolicyTarget> {
        match &b.target {
            RoleTarget::Repository {
                project,
                location,
                repository,
            } => Some(PolicyTarget::Repository(format!(
                "projects/{project}/locations/{location}/repositories/{repository}"
            ))),
            RoleTarget::Project { project } => Some(PolicyTarget::Project(project.clone())),
            RoleTarget::Bucket { bucket } => Some(PolicyTarget::Bucket(bucket.clone())),
            RoleTarget::Secret { name } => Some(PolicyTarget::Secret(name.clone())),
            RoleTarget::Dataset { .. } => None,
        }
    }

    // ---------- checks (read-only) ----------

    /// Read-only state of a step. `service_exists` lets service-scoped steps
    /// report "pending" without calling APIs for a service that is not created yet.
    pub async fn check(&self, step: &Step, service_exists: bool) -> StepCheck {
        let name = step.describe(self.d);
        let (state, detail) = if step.needs_service() && !service_exists {
            (
                StepState::Pending,
                "after the service is created".to_string(),
            )
        } else {
            match self.check_inner(step).await {
                Ok(x) => x,
                Err(e) => (StepState::Unknown, e.message),
            }
        };
        StepCheck {
            step: name,
            state,
            detail,
        }
    }

    async fn check_inner(&self, step: &Step) -> Result<(StepState, String)> {
        Ok(match step {
            Step::EnableApis(apis) => {
                let disabled = self.disabled_apis(apis).await?;
                if disabled.is_empty() {
                    (
                        StepState::InSync,
                        format!("all {} APIs enabled", apis.len()),
                    )
                } else {
                    (
                        StepState::Pending,
                        format!("will enable {}", disabled.join(", ")),
                    )
                }
            }
            Step::CreateSecret(s) => match self.get_secret(&s.name).await {
                Ok(_) => (StepState::InSync, format!("{} exists", s.name)),
                Err(e) if is_not_found(&e) => (
                    StepState::Pending,
                    format!(
                        "will create {} ({}), without a value",
                        s.name,
                        replication_display(&s.locations)
                    ),
                ),
                Err(e) => (
                    StepState::Unknown,
                    format!("cannot read {}: {}", s.name, short(&e)),
                ),
            },
            Step::GrantMembers { members, binding } => match self.grant_target(binding) {
                Some(t) => self.check_members(&t, &binding.role, members).await,
                None => (StepState::Unknown, "unsupported target".into()),
            },
            Step::SecretValues(names) => {
                let mut empty = Vec::new();
                for n in names {
                    match self.newest_enabled_version(n).await {
                        Ok(Some(_)) => {}
                        Ok(None) => empty.push(n.rsplit('/').next().unwrap_or(n).to_string()),
                        Err(e) => return Ok((StepState::Unknown, e.message)),
                    }
                }
                if empty.is_empty() {
                    (StepState::InSync, "every secret has a value".into())
                } else {
                    (
                        StepState::Pending,
                        format!(
                            "no value yet in {}: deploy stops here until a value is added",
                            empty.join(", ")
                        ),
                    )
                }
            }
            Step::CreateBucket(cfg) => match self.bucket_state(cfg).await? {
                BucketState::Missing => (
                    StepState::Pending,
                    format!("will create gs://{} in {}", cfg.name, cfg.location),
                ),
                BucketState::InSync => (StepState::InSync, format!("gs://{} exists", cfg.name)),
                BucketState::NotOwned(why) => (
                    StepState::InSync,
                    format!(
                        "gs://{} exists ({why}): used as is, settings not changed",
                        cfg.name
                    ),
                ),
                BucketState::Drift(_, fields) => (
                    StepState::Pending,
                    format!("will update {} on gs://{}", fields.join(", "), cfg.name),
                ),
            },
            Step::CreateRepository {
                location,
                repository,
            } => match self.get_repository(location, repository).await {
                Ok(r) if r.format == Format::Docker => {
                    (StepState::InSync, format!("{location}/{repository} exists"))
                }
                Ok(_) => (
                    StepState::Unknown,
                    format!("{location}/{repository} exists but is not a Docker repository"),
                ),
                Err(e) if is_not_found(&e) => (
                    StepState::Pending,
                    format!("will create Docker repository {location}/{repository}"),
                ),
                Err(e) => (
                    StepState::Unknown,
                    format!("cannot read the repository: {}", short(&e)),
                ),
            },
            Step::CreateServiceAccount { email, .. } => {
                match missing(&self.clients.iam, "IAM")?
                    .get_service_account()
                    .set_name(naming::service_account_resource(email))
                    .send()
                    .await
                {
                    Ok(sa) if sa.disabled => (
                        StepState::Unknown,
                        format!("{email} exists but is disabled"),
                    ),
                    Ok(_) => (StepState::InSync, format!("{email} exists")),
                    Err(e) if is_not_found(&e) => {
                        (StepState::Pending, format!("will create {email}"))
                    }
                    Err(e) => (
                        StepState::Unknown,
                        format!("cannot read {email}: {}", short(&e)),
                    ),
                }
            }
            Step::Grant { email, binding: b } => match (&b.target, self.grant_target(b)) {
                (RoleTarget::Dataset { project, dataset }, _) => {
                    match self.get_dataset(project, dataset).await {
                        Ok(ds) if dataset_grants(&ds.access, &b.role, email) => (
                            StepState::InSync,
                            format!("{} already granted on {}", b.role, b.target),
                        ),
                        Ok(_) => (
                            StepState::Pending,
                            format!("will grant {} on {}", b.role, b.target),
                        ),
                        Err(e) => (
                            StepState::Unknown,
                            format!("cannot read {}: {}", b.target, short(&e)),
                        ),
                    }
                }
                (_, Some(t)) => {
                    self.check_members(&t, &b.role, &[Self::sa_member(email)])
                        .await
                }
                (_, None) => unreachable!("non-dataset targets have a policy"),
            },
            Step::ProjectTag { key, value } => {
                if self.project_tag_effective(key, value).await? {
                    (
                        StepState::InSync,
                        format!("{key}={value} is effective on the project"),
                    )
                } else {
                    (
                        StepState::Pending,
                        format!(
                            "will bind {key}={value} to the project and wait until it is effective"
                        ),
                    )
                }
            }
            Step::Tag { key, value } => {
                if self.service_tag_effective(key, value).await? {
                    (
                        StepState::InSync,
                        format!("{key}={value} is effective on the service"),
                    )
                } else {
                    (
                        StepState::Pending,
                        format!(
                            "will bind {key}={value} to the service and wait until it is effective"
                        ),
                    )
                }
            }
            Step::IapInvoker => {
                let agent = format!(
                    "serviceAccount:{}",
                    iap_service_agent(&self.project_number().await?)
                );
                self.check_members(
                    &PolicyTarget::RunService(self.d.service_name()),
                    iam::INVOKER_ROLE,
                    &[agent],
                )
                .await
            }
            Step::IapAccess => {
                let r = iap_resource(
                    &self.project_number().await?,
                    &self.d.region,
                    &self.d.service_id,
                );
                self.check_members(
                    &PolicyTarget::Iap(r),
                    IAP_ACCESSOR_ROLE,
                    &self.d.service.iap.members,
                )
                .await
            }
        })
    }

    // ---------- applies (idempotent) ----------

    pub async fn apply(&self, step: &Step) -> Result<StepResult> {
        let name = step.describe(self.d);
        let (outcome, detail) = match step {
            Step::EnableApis(apis) => self.ensure_apis(apis).await?,
            Step::ProjectTag { key, value } => self.ensure_project_tag(key, value).await?,
            Step::CreateBucket(cfg) => self.ensure_bucket(cfg).await?,
            Step::CreateSecret(s) => self.ensure_secret(s).await?,
            Step::GrantMembers { members, binding } => {
                let t = self
                    .grant_target(binding)
                    .ok_or_else(|| Error::internal("unsupported grant target"))?;
                let o = self.ensure_members(&t, &binding.role, members).await?;
                (o, format!("{} on {t}", binding.role))
            }
            Step::SecretValues(names) => self.require_secret_values(names).await?,
            Step::CreateRepository {
                location,
                repository,
            } => self.ensure_repository(location, repository).await?,
            Step::CreateServiceAccount {
                email,
                display_name,
                description,
            } => {
                self.ensure_service_account(email, display_name, description)
                    .await?
            }
            Step::Grant { email, binding } => self.ensure_grant(email, binding).await?,
            Step::Tag { key, value } => self.ensure_tag(key, value).await?,
            Step::IapInvoker => {
                let number = self.project_number().await?;
                self.ensure_iap_service_agent().await?;
                let agent = format!("serviceAccount:{}", iap_service_agent(&number));
                let o = self
                    .ensure_members(
                        &PolicyTarget::RunService(self.d.service_name()),
                        iam::INVOKER_ROLE,
                        std::slice::from_ref(&agent),
                    )
                    .await?;
                (o, format!("{agent} has roles/run.invoker"))
            }
            Step::IapAccess => {
                let r = iap_resource(
                    &self.project_number().await?,
                    &self.d.region,
                    &self.d.service_id,
                );
                let o = self
                    .ensure_members(
                        &PolicyTarget::Iap(r),
                        IAP_ACCESSOR_ROLE,
                        &self.d.service.iap.members,
                    )
                    .await?;
                (o, format!("{IAP_ACCESSOR_ROLE} granted"))
            }
        };
        Ok(StepResult {
            step: name,
            outcome,
            detail,
        })
    }

    async fn ensure_service_account(
        &self,
        email: &str,
        display_name: &str,
        description: &str,
    ) -> Result<(StepOutcome, String)> {
        let client = missing(&self.clients.iam, "IAM")?;
        match client
            .get_service_account()
            .set_name(naming::service_account_resource(email))
            .send()
            .await
        {
            Ok(sa) if sa.disabled => {
                return Err(
                    Error::prerequisite(format!("service account {email} is disabled")).hint(
                        format!("enable it: gcloud iam service-accounts enable {email}"),
                    ),
                );
            }
            Ok(_) => return Ok((StepOutcome::Unchanged, format!("{email} exists"))),
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(api_error(e, &format!("reading service account {email}"))),
        }
        let (local, _) = email.split_once('@').unwrap_or((email, ""));
        let project = crate::config::validate::service_account_project(email).ok_or_else(|| {
            Error::config(format!(
                "cannot create {email}: not a user-managed service account"
            ))
        })?;
        let display = display_name.to_string();
        match client
            .create_service_account()
            .set_name(format!("projects/{project}"))
            .set_account_id(local)
            .set_service_account(
                ServiceAccount::new()
                    .set_display_name(display)
                    .set_description(description),
            )
            .send()
            .await
        {
            Ok(_) => Ok((StepOutcome::Changed, format!("created {email}"))),
            // Created concurrently or by an earlier ambiguous attempt.
            Err(e) if status_code(&e) == Some(Code::AlreadyExists) => {
                Ok((StepOutcome::Unchanged, format!("{email} exists")))
            }
            Err(e) => Err(
                api_error(e, &format!("creating service account {email}")).hint(
                    "the deployer needs iam.serviceAccounts.create (roles/iam.serviceAccountAdmin)",
                ),
            ),
        }
    }

    async fn get_dataset(
        &self,
        project: &str,
        dataset: &str,
    ) -> std::result::Result<Dataset, GaxError> {
        missing_gax(&self.clients.datasets)?
            .get_dataset()
            .set_project_id(project)
            .set_dataset_id(dataset)
            .send()
            .await
    }

    async fn ensure_grant(&self, email: &str, b: &RoleBinding) -> Result<(StepOutcome, String)> {
        if let RoleTarget::Dataset { project, dataset } = &b.target {
            let ds = self
                .get_dataset(project, dataset)
                .await
                .map_err(|e| api_error(e, &format!("reading {}", b.target)))?;
            if dataset_grants(&ds.access, &b.role, email) {
                return Ok((
                    StepOutcome::Unchanged,
                    format!("{} already granted", b.role),
                ));
            }
            let mut access = ds.access.clone();
            access.push(Access::new().set_role(&b.role).set_user_by_email(email));
            let updated = missing(&self.clients.datasets, "BigQuery")?
                .patch_dataset()
                .set_project_id(project)
                .set_dataset_id(dataset)
                .set_dataset(Dataset::new().set_access(access))
                .send()
                .await
                .map_err(|e| {
                    api_error(e, &format!("granting {} on {}", b.role, b.target))
                        .hint("the deployer needs bigquery.datasets.update (roles/bigquery.dataOwner on the dataset)")
                })?;
            // BigQuery has no etag check on patch: verify the entry survived.
            if !dataset_grants(&updated.access, &b.role, email) {
                return Err(Error::new(
                    ErrorKind::Internal,
                    format!(
                        "{} on {} was not applied (concurrent change?)",
                        b.role, b.target
                    ),
                ));
            }
            return Ok((
                StepOutcome::Changed,
                format!("granted {} to {email}", b.role),
            ));
        }
        let t = self
            .grant_target(b)
            .expect("non-dataset targets have a policy");
        let o = self
            .ensure_members(&t, &b.role, &[Self::sa_member(email)])
            .await?;
        Ok((o, format!("{} for {email}", b.role)))
    }

    fn project_resource(number: &str) -> String {
        format!("//cloudresourcemanager.googleapis.com/projects/{number}")
    }

    /// Whether `key=value` is among the project's effective tags (bound
    /// directly or inherited from a folder or the organization).
    async fn project_tag_effective(&self, key: &str, value: &str) -> Result<bool> {
        let parent = Self::project_resource(&self.project_number().await?);
        let want = format!("{key}/{value}");
        let client = missing(&self.clients.project_tag_bindings, "TagBindings")?;
        let mut token = String::new();
        loop {
            let resp = client
                .list_effective_tags()
                .set_parent(&parent)
                .set_page_token(token.clone())
                .send()
                .await
                .map_err(|e| {
                    api_error(e, "listing the project's effective tags")
                        .hint("the deployer needs resourcemanager.projects.get and tag viewer access (roles/resourcemanager.tagViewer)")
                })?;
            if resp
                .effective_tags
                .iter()
                .any(|t| t.namespaced_tag_value.eq_ignore_ascii_case(&want))
            {
                return Ok(true);
            }
            if resp.next_page_token.is_empty() {
                return Ok(false);
            }
            token = resp.next_page_token;
        }
    }

    async fn ensure_project_tag(&self, key: &str, value: &str) -> Result<(StepOutcome, String)> {
        if self.project_tag_effective(key, value).await? {
            return Ok((
                StepOutcome::Unchanged,
                format!("{key}={value} is effective"),
            ));
        }
        let namespaced = format!("{key}/{value}");
        let tv = missing(&self.clients.tag_values, "TagValues")?
            .get_namespaced_tag_value()
            .set_name(&namespaced)
            .send()
            .await
            .map_err(|e| api_error(e, &format!("looking up tag value {namespaced}")))?;
        let parent = Self::project_resource(&self.project_number().await?);
        let op = missing(&self.clients.project_tag_bindings, "TagBindings")?
            .create_tag_binding()
            .set_tag_binding(
                TagBinding::new()
                    .set_parent(&parent)
                    .set_tag_value(&tv.name),
            )
            .poller()
            .until_done();
        match tokio::time::timeout(LRO_TIMEOUT, op).await {
            Err(_) => {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    "timed out binding the project tag",
                ));
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) if status_code(&e) == Some(Code::AlreadyExists) => {}
            Ok(Err(e)) => {
                return Err(api_error(e, &format!("binding tag {namespaced} to the project")).hint(
                    "the deployer needs roles/resourcemanager.tagUser on the tag value and on the project",
                ));
            }
        }
        // Wait until the binding is visible as an effective tag.
        let deadline = std::time::Instant::now() + TAG_PROPAGATION_TIMEOUT;
        loop {
            if self.project_tag_effective(key, value).await? {
                return Ok((
                    StepOutcome::Changed,
                    format!("bound {namespaced}; effective on the project"),
                ));
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!(
                        "{namespaced} is bound but not yet effective after {}s",
                        TAG_PROPAGATION_TIMEOUT.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    /// Returns whether the tag value is bound, and the value's resource name.
    async fn tag_bound(&self, key: &str, value: &str) -> Result<(bool, String)> {
        let namespaced = format!("{key}/{value}");
        let tv = missing(&self.clients.tag_values, "TagValues")?
            .get_namespaced_tag_value()
            .set_name(&namespaced)
            .send()
            .await
            .map_err(|e| {
                api_error(e, &format!("looking up tag value {namespaced}"))
                    .hint("check the tag key/value exist and the deployer has roles/resourcemanager.tagViewer")
            })?;
        let parent = tag_parent(&self.d.project, &self.d.region, &self.d.service_id);
        let mut token = String::new();
        loop {
            let resp = missing(&self.clients.tag_bindings, "TagBindings")?
                .list_tag_bindings()
                .set_parent(&parent)
                .set_page_token(token.clone())
                .send()
                .await
                .map_err(|e| api_error(e, "listing tag bindings"))?;
            if resp.tag_bindings.iter().any(|b| b.tag_value == tv.name) {
                return Ok((true, tv.name));
            }
            if resp.next_page_token.is_empty() {
                return Ok((false, tv.name));
            }
            token = resp.next_page_token;
        }
    }

    /// Whether `key=value` is among the service's effective tags (bound to
    /// the service or inherited from the project, folder or organization).
    async fn service_tag_effective(&self, key: &str, value: &str) -> Result<bool> {
        let parent = tag_parent(&self.d.project, &self.d.region, &self.d.service_id);
        let want = format!("{key}/{value}");
        let client = missing(&self.clients.tag_bindings, "TagBindings")?;
        let mut token = String::new();
        loop {
            let resp = match client
                .list_effective_tags()
                .set_parent(&parent)
                .set_page_token(token.clone())
                .send()
                .await
            {
                Ok(r) => r,
                // The service does not exist yet: nothing is effective on it.
                Err(e) if crate::gcp::is_not_found(&e) => return Ok(false),
                Err(e) => return Err(api_error(e, "listing the service's effective tags")),
            };
            if resp
                .effective_tags
                .iter()
                .any(|t| t.namespaced_tag_value.eq_ignore_ascii_case(&want))
            {
                return Ok(true);
            }
            if resp.next_page_token.is_empty() {
                return Ok(false);
            }
            token = resp.next_page_token;
        }
    }

    async fn ensure_tag(&self, key: &str, value: &str) -> Result<(StepOutcome, String)> {
        if self.service_tag_effective(key, value).await? {
            return Ok((
                StepOutcome::Unchanged,
                format!("{key}={value} is effective"),
            ));
        }
        let (bound, value_name) = self.tag_bound(key, value).await?;
        if !bound {
            self.bind_service_tag(key, value, &value_name).await?;
        }
        // Wait until organization policy evaluation can see it.
        let deadline = std::time::Instant::now() + TAG_PROPAGATION_TIMEOUT;
        loop {
            if self.service_tag_effective(key, value).await? {
                return Ok((
                    StepOutcome::Changed,
                    format!("bound {key}={value}; effective on the service"),
                ));
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!(
                        "{key}={value} is bound but not yet effective after {}s",
                        TAG_PROPAGATION_TIMEOUT.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn bind_service_tag(&self, key: &str, value: &str, value_name: &str) -> Result<()> {
        let parent = tag_parent(&self.d.project, &self.d.region, &self.d.service_id);
        let res = missing(&self.clients.tag_bindings, "TagBindings")?
            .create_tag_binding()
            .set_tag_binding(
                TagBinding::new()
                    .set_parent(&parent)
                    .set_tag_value(value_name),
            )
            .poller()
            .until_done()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(e) if status_code(&e) == Some(Code::AlreadyExists) => Ok(()),
            Err(e) => Err(api_error(e, &format!("binding tag {key}={value}")).hint(
                "the deployer needs roles/resourcemanager.tagUser on the tag value; a key can hold only one value per resource",
            )),
        }
    }

    // ---------- secrets ----------

    async fn get_secret(
        &self,
        id: &str,
    ) -> std::result::Result<google_cloud_secretmanager_v1::model::Secret, GaxError> {
        missing_gax(&self.clients.secrets)?
            .get_secret()
            .set_name(format!("projects/{}/secrets/{id}", self.d.project))
            .send()
            .await
    }

    async fn ensure_secret(&self, s: &ManagedSecret) -> Result<(StepOutcome, String)> {
        use google_cloud_secretmanager_v1::model::{Replication, Secret, replication};
        match self.get_secret(&s.name).await {
            Ok(_) => return Ok((StepOutcome::Unchanged, format!("{} exists", s.name))),
            Err(e) if is_not_found(&e) => {}
            Err(e) => {
                return Err(api_error(e, &format!("reading secret {}", s.name)).hint(
                    "the deployer needs secretmanager.secrets.get (roles/secretmanager.viewer)",
                ));
            }
        }
        let replication = if s.locations.is_empty() {
            Replication::new().set_automatic(replication::Automatic::new())
        } else {
            Replication::new().set_user_managed(
                replication::UserManaged::new().set_replicas(
                    s.locations
                        .iter()
                        .map(|l| replication::user_managed::Replica::new().set_location(l)),
                ),
            )
        };
        let mut labels = naming::ownership_labels(&self.d.app, &self.d.stage);
        labels.extend(s.labels.clone());
        let res = missing(&self.clients.secrets, "Secret Manager")?
            .create_secret()
            .set_parent(format!("projects/{}", self.d.project))
            .set_secret_id(&s.name)
            .set_secret(
                Secret::new()
                    .set_replication(replication)
                    .set_labels(labels),
            )
            .send()
            .await;
        match res {
            Ok(_) => Ok((
                StepOutcome::Changed,
                format!("created {} ({})", s.name, replication_display(&s.locations)),
            )),
            Err(e) if status_code(&e) == Some(Code::AlreadyExists) => {
                Ok((StepOutcome::Unchanged, format!("{} exists", s.name)))
            }
            Err(e) => Err(api_error(e, &format!("creating secret {}", s.name)).hint(
                "the deployer needs secretmanager.secrets.create (roles/secretmanager.admin); an organization policy may restrict replication locations (`locations:`)",
            )),
        }
    }

    /// Newest enabled version number of `projects/P/secrets/S`; `None` if the
    /// secret has no enabled version (or does not exist yet).
    pub async fn newest_enabled_version(&self, name: &str) -> Result<Option<String>> {
        let res = missing(&self.clients.secrets, "Secret Manager")?
            .list_secret_versions()
            .set_parent(name)
            .set_filter("state:ENABLED")
            .set_page_size(1)
            .send()
            .await;
        match res {
            Ok(r) => Ok(r
                .versions
                .first()
                .and_then(|v| v.name.rsplit('/').next().map(String::from))),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(
                api_error(e, &format!("listing the versions of {name}")).hint(
                    "the deployer needs secretmanager.versions.list (roles/secretmanager.viewer)",
                ),
            ),
        }
    }

    async fn require_secret_values(&self, names: &[String]) -> Result<(StepOutcome, String)> {
        let mut empty = Vec::new();
        for n in names {
            if self.newest_enabled_version(n).await?.is_none() {
                empty.push(n.clone());
            }
        }
        if empty.is_empty() {
            return Ok((StepOutcome::Unchanged, "every secret has a value".into()));
        }
        let mut e = Error::new(
            ErrorKind::Prerequisite,
            format!(
                "{} secret(s) created by runway have no value yet: {}. Add the value(s), then re-run the same deploy; it continues from here",
                empty.len(),
                empty
                    .iter()
                    .map(|n| n.rsplit('/').next().unwrap_or(n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .permanent();
        for n in &empty {
            let id = n.rsplit('/').next().unwrap_or(n);
            e = e.hint(format!(
                "printf '%s' 'VALUE' | gcloud secrets versions add {id} --project {} --data-file=-",
                self.d.project
            ));
            if let Some(s) = self.d.secrets.values().find(|s| s.name == id)
                && !s.adders.is_empty()
            {
                e = e.hint(format!(
                    "{id}: values can be added by {}",
                    s.adders.join(", ")
                ));
            }
        }
        Err(e)
    }

    // ---------- teardown (undeploy) ----------

    /// The account's description, or `None` if it does not exist.
    pub async fn service_account_description(&self, email: &str) -> Result<Option<String>> {
        match missing(&self.clients.iam, "IAM")?
            .get_service_account()
            .set_name(naming::service_account_resource(email))
            .send()
            .await
        {
            Ok(sa) => Ok(Some(sa.description)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(api_error(e, &format!("reading service account {email}"))),
        }
    }

    pub async fn delete_service_account(&self, email: &str) -> Result<StepOutcome> {
        match missing(&self.clients.iam, "IAM")?
            .delete_service_account()
            .set_name(naming::service_account_resource(email))
            .send()
            .await
        {
            Ok(_) => Ok(StepOutcome::Changed),
            Err(e) if is_not_found(&e) => Ok(StepOutcome::Unchanged),
            Err(e) => Err(
                api_error(e, &format!("deleting service account {email}")).hint(
                    "the deployer needs iam.serviceAccounts.delete (roles/iam.serviceAccountAdmin)",
                ),
            ),
        }
    }

    /// Removes a role previously granted to `email` (inverse of a `Grant` step).
    pub async fn revoke_grant(&self, email: &str, b: &RoleBinding) -> Result<StepOutcome> {
        if let RoleTarget::Dataset { project, dataset } = &b.target {
            let ds = match self.get_dataset(project, dataset).await {
                Ok(ds) => ds,
                Err(e) if is_not_found(&e) => return Ok(StepOutcome::Unchanged),
                Err(e) => return Err(api_error(e, &format!("reading {}", b.target))),
            };
            let member = Self::sa_member(email);
            let before = ds.access.len();
            let access: Vec<Access> = ds
                .access
                .into_iter()
                .filter(|a| {
                    !(normalize_dataset_role(&a.role) == normalize_dataset_role(&b.role)
                        && (a.user_by_email.eq_ignore_ascii_case(email)
                            || iam::same_member(&a.iam_member, &member)))
                })
                .collect();
            if access.len() == before {
                return Ok(StepOutcome::Unchanged);
            }
            missing(&self.clients.datasets, "BigQuery")?
                .patch_dataset()
                .set_project_id(project)
                .set_dataset_id(dataset)
                .set_dataset(Dataset::new().set_access(access))
                .send()
                .await
                .map_err(|e| api_error(e, &format!("revoking {} on {}", b.role, b.target)))?;
            return Ok(StepOutcome::Changed);
        }
        let t = self
            .grant_target(b)
            .expect("non-dataset targets have a policy");
        let members = [Self::sa_member(email)];
        for attempt in 1..=4 {
            let mut p = match self.get_policy(&t).await {
                Ok(p) => p,
                Err(e) if is_not_found(&e) => return Ok(StepOutcome::Unchanged),
                Err(e) => return Err(api_error(e, &format!("reading the IAM policy of {t}"))),
            };
            if !iam::remove_members(&mut p, &b.role, &members) {
                return Ok(StepOutcome::Unchanged);
            }
            match self.set_policy(&t, p).await {
                Ok(_) => return Ok(StepOutcome::Changed),
                Err(e) if (is_concurrency_conflict(&e) || is_ambiguous(&e)) && attempt < 4 => {
                    continue;
                }
                Err(e) => {
                    return Err(api_error(e, &format!("revoking {} on {t}", b.role))
                        .hint(needed_role_hint(&t)));
                }
            }
        }
        unreachable!("loop returns")
    }

    /// Deletes the app's image package from the repository (all its tags and digests).
    pub async fn delete_images(
        &self,
        location: &str,
        repository: &str,
        package: &str,
    ) -> Result<StepOutcome> {
        let name = format!(
            "projects/{}/locations/{location}/repositories/{repository}/packages/{package}",
            self.d.project
        );
        let op = missing(&self.clients.artifact, "ArtifactRegistry")?
            .delete_package()
            .set_name(&name)
            .poller()
            .until_done();
        match tokio::time::timeout(LRO_TIMEOUT, op).await {
            Err(_) => Err(Error::new(ErrorKind::Timeout, "timed out deleting images")),
            Ok(Ok(_)) => Ok(StepOutcome::Changed),
            Ok(Err(e)) if is_not_found(&e) => Ok(StepOutcome::Unchanged),
            Ok(Err(e)) => Err(api_error(e, &format!("deleting images {name}"))
                .hint("the deployer needs artifactregistry.packages.delete (roles/artifactregistry.repoAdmin)")),
        }
    }

    // ---------- APIs ----------

    async fn disabled_apis(&self, apis: &[String]) -> Result<Vec<String>> {
        let client = missing(&self.clients.service_usage, "ServiceUsage")?;
        let parent = format!("projects/{}", self.d.project);
        let mut disabled = Vec::new();
        for chunk in apis.chunks(20) {
            let resp = client
                .batch_get_services()
                .set_parent(&parent)
                .set_names(chunk.iter().map(|a| format!("{parent}/services/{a}")))
                .send()
                .await
                .map_err(|e| {
                    api_error(e, "reading enabled APIs").hint(
                        "the deployer needs serviceusage.services.get (roles/serviceusage.serviceUsageViewer); serviceusage.googleapis.com must be enabled",
                    )
                })?;
            for svc in resp.services {
                if svc.state != google_cloud_api_serviceusage_v1::model::State::Enabled {
                    disabled.push(svc.name.rsplit('/').next().unwrap_or(&svc.name).to_string());
                }
            }
        }
        disabled.sort();
        Ok(disabled)
    }

    async fn ensure_apis(&self, apis: &[String]) -> Result<(StepOutcome, String)> {
        let disabled = self.disabled_apis(apis).await?;
        if disabled.is_empty() {
            return Ok((
                StepOutcome::Unchanged,
                format!("all {} APIs enabled", apis.len()),
            ));
        }
        let client = missing(&self.clients.service_usage, "ServiceUsage")?;
        for chunk in disabled.chunks(20) {
            let op = client
                .batch_enable_services()
                .set_parent(format!("projects/{}", self.d.project))
                .set_service_ids(chunk.to_vec())
                .poller()
                .until_done();
            tokio::time::timeout(LRO_TIMEOUT, op)
                .await
                .map_err(|_| Error::new(ErrorKind::Timeout, "timed out enabling APIs"))?
                .map_err(|e| {
                    api_error(e, &format!("enabling {}", chunk.join(", "))).hint(
                        "the deployer needs serviceusage.services.enable (roles/serviceusage.serviceUsageAdmin) and the project needs billing",
                    )
                })?;
        }
        Ok((
            StepOutcome::Changed,
            format!("enabled {}", disabled.join(", ")),
        ))
    }

    // ---------- buckets ----------

    async fn bucket_state(&self, cfg: &BucketConfig) -> Result<BucketState> {
        let client = missing(&self.clients.storage, "StorageControl")?;
        let live = match client
            .get_bucket()
            .set_name(bucket::bucket_resource(&cfg.name))
            .send()
            .await
        {
            Ok(b) => b,
            Err(e) if is_not_found(&e) => return Ok(BucketState::Missing),
            Err(e) if status_code(&e) == Some(Code::PermissionDenied) => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "bucket gs://{} exists but is not readable: it belongs to another project, or the deployer lacks storage.buckets.get",
                        cfg.name
                    ),
                )
                .hint("bucket names are global; pick another name (for example prefix it with ${project})"));
            }
            Err(e) => return Err(api_error(e, &format!("reading bucket gs://{}", cfg.name))),
        };
        let number = self.project_number().await?;
        if live.project != format!("projects/{number}")
            && live.project != format!("projects/{}", self.d.project)
        {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!(
                    "bucket gs://{} belongs to another project ({})",
                    cfg.name, live.project
                ),
            )
            .hint("bucket names are global; pick another name"));
        }
        if !bucket::same_location(&live, cfg) {
            return Err(Error::config(format!(
                "bucket gs://{} is in {}, configured location is {} (a bucket's location cannot change)",
                cfg.name, live.location, cfg.location
            )));
        }
        if let Some(reason) = bucket::not_owned_reason(&live, &self.d.app) {
            return Ok(BucketState::NotOwned(reason));
        }
        let fields = bucket::drift(&live, cfg, &self.d.app);
        Ok(if fields.is_empty() {
            BucketState::InSync
        } else {
            BucketState::Drift(Box::new(live), fields)
        })
    }

    async fn ensure_bucket(&self, cfg: &BucketConfig) -> Result<(StepOutcome, String)> {
        let client = missing(&self.clients.storage, "StorageControl")?;
        match self.bucket_state(cfg).await? {
            BucketState::InSync => {
                Ok((StepOutcome::Unchanged, format!("gs://{} exists", cfg.name)))
            }
            BucketState::NotOwned(why) => Ok((
                StepOutcome::Unchanged,
                format!(
                    "gs://{} exists ({why}): used as is, settings not changed",
                    cfg.name
                ),
            )),
            BucketState::Missing => {
                let res = client
                    .create_bucket()
                    .set_parent("projects/_")
                    .set_bucket_id(&cfg.name)
                    .set_bucket(bucket::new_bucket(cfg, &self.d.project, &self.d.app))
                    .send()
                    .await;
                match res {
                    Ok(_) => Ok((
                        StepOutcome::Changed,
                        format!("created gs://{} in {}", cfg.name, cfg.location),
                    )),
                    // Created by an earlier ambiguous attempt (ownership is
                    // re-checked on the next read) or taken by someone else.
                    Err(e) if status_code(&e) == Some(Code::AlreadyExists) || e.http_status_code() == Some(409) => {
                        match self.bucket_state(cfg).await? {
                            BucketState::Missing => Err(Error::new(
                                ErrorKind::Conflict,
                                format!("bucket name gs://{} is already taken", cfg.name),
                            )),
                            _ => Ok((StepOutcome::Unchanged, format!("gs://{} exists", cfg.name))),
                        }
                    }
                    Err(e) => Err(api_error(e, &format!("creating bucket gs://{}", cfg.name))
                        .hint("the deployer needs storage.buckets.create (roles/storage.admin) on the project")),
                }
            }
            BucketState::Drift(live, fields) => {
                let mask = google_cloud_wkt::FieldMask::default()
                    .set_paths(fields.iter().map(|f| f.to_string()));
                client
                    .update_bucket()
                    .set_bucket(bucket::patch(&live, cfg, &self.d.app, &fields))
                    .set_update_mask(mask)
                    .set_if_metageneration_match(live.metageneration)
                    .send()
                    .await
                    .map_err(|e| {
                        api_error(e, &format!("updating bucket gs://{}", cfg.name))
                            .hint("the deployer needs storage.buckets.update (roles/storage.admin)")
                    })?;
                Ok((
                    StepOutcome::Changed,
                    format!("updated {} on gs://{}", fields.join(", "), cfg.name),
                ))
            }
        }
    }

    // ---------- Artifact Registry ----------

    async fn get_repository(
        &self,
        location: &str,
        repository: &str,
    ) -> std::result::Result<Repository, GaxError> {
        missing_gax(&self.clients.artifact)?
            .get_repository()
            .set_name(format!(
                "projects/{}/locations/{location}/repositories/{repository}",
                self.d.project
            ))
            .send()
            .await
    }

    async fn ensure_repository(
        &self,
        location: &str,
        repository: &str,
    ) -> Result<(StepOutcome, String)> {
        match self.get_repository(location, repository).await {
            Ok(r) if r.format == Format::Docker => {
                return Ok((
                    StepOutcome::Unchanged,
                    format!("{location}/{repository} exists"),
                ));
            }
            Ok(_) => {
                return Err(Error::config(format!(
                    "repository {location}/{repository} exists but is not a Docker repository"
                )));
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(api_error(e, "reading the Artifact Registry repository")),
        }
        let op = missing(&self.clients.artifact, "ArtifactRegistry")?
            .create_repository()
            .set_parent(format!("projects/{}/locations/{location}", self.d.project))
            .set_repository_id(repository)
            .set_repository(
                Repository::new()
                    .set_format(Format::Docker)
                    .set_description("Container images built by runway")
                    .set_labels([
                        (naming::LABEL_MANAGED_BY, naming::LABEL_MANAGED_BY_VALUE),
                        (naming::LABEL_APP, self.d.app.as_str()),
                    ]),
            )
            .poller()
            .until_done();
        match tokio::time::timeout(LRO_TIMEOUT, op).await {
            Err(_) => Err(Error::new(ErrorKind::Timeout, "timed out creating the repository")),
            Ok(Ok(_)) => Ok((StepOutcome::Changed, format!("created {location}/{repository}"))),
            Ok(Err(e)) if status_code(&e) == Some(Code::AlreadyExists) => {
                Ok((StepOutcome::Unchanged, format!("{location}/{repository} exists")))
            }
            Ok(Err(e)) => Err(api_error(e, "creating the Artifact Registry repository")
                .hint("the deployer needs artifactregistry.repositories.create (roles/artifactregistry.admin)")),
        }
    }

    /// Makes sure the IAP service agent exists (Service Usage
    /// `generateServiceIdentity`, idempotent). Small REST adapter: the Rust
    /// Service Usage SDK only covers v1, which lacks this method.
    async fn ensure_iap_service_agent(&self) -> Result<()> {
        let token = self.session.token().await?;
        let url = format!(
            "{}/v1beta1/projects/{}/services/iap.googleapis.com:generateServiceIdentity",
            self.service_usage, self.d.project
        );
        let resp = self
            .session
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| Error::internal(format!("cannot reach Service Usage: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        let kind = match status.as_u16() {
            401 | 403 => ErrorKind::Prerequisite,
            400 | 404 => ErrorKind::Deploy,
            _ => ErrorKind::Internal,
        };
        Err(Error::new(kind, format!("creating the IAP service agent failed (HTTP {status}): {body}"))
            .hint("enable iap.googleapis.com and grant the deployer serviceusage.services.use (roles/serviceusage.serviceUsageConsumer)"))
    }
}

fn missing_gax<T: Clone>(c: &Option<T>) -> std::result::Result<T, GaxError> {
    c.clone()
        .ok_or_else(|| GaxError::io("client not initialized for this step"))
}

fn short(e: &GaxError) -> String {
    e.status()
        .map(|s| s.message.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_role_matching() {
        let access = vec![
            Access::new()
                .set_role("READER")
                .set_user_by_email("rt@p.iam.gserviceaccount.com"),
            Access::new()
                .set_role("roles/bigquery.jobUser")
                .set_iam_member("serviceAccount:x@p.iam.gserviceaccount.com"),
        ];
        assert!(dataset_grants(
            &access,
            "roles/bigquery.dataViewer",
            "rt@p.iam.gserviceaccount.com"
        ));
        assert!(dataset_grants(
            &access,
            "READER",
            "RT@p.iam.gserviceaccount.com"
        ));
        assert!(!dataset_grants(
            &access,
            "roles/bigquery.dataEditor",
            "rt@p.iam.gserviceaccount.com"
        ));
        assert!(dataset_grants(
            &access,
            "roles/bigquery.jobUser",
            "x@p.iam.gserviceaccount.com"
        ));
        assert!(!dataset_grants(
            &access,
            "roles/bigquery.dataViewer",
            "other@p.iam.gserviceaccount.com"
        ));
    }

    fn deployment(yaml: &str) -> (tempfile::TempDir, Deployment) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(&p, yaml).unwrap();
        let d = crate::config::load_and_resolve(&p, "prod", &Default::default())
            .unwrap_or_else(|e| panic!("{e:?}"))
            .1
            .deployment;
        (dir, d)
    }

    const FULL: &str = r#"
version: 1
app: gcptree
vars: { job_project: billing-data-1234 }
provider:
  project: acme-sandbox-26c8
  region: europe-west1
  enable_apis: true
  apis: [telemetry.googleapis.com]
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
      - { role: roles/bigquery.jobUser, project: "${vars.job_project}" }
      - { role: roles/storage.objectUser, bucket: "${buckets.cache}" }
  env:
    GCPTREE_CACHE_BUCKET: "gs://${buckets.cache}/gcptree"
    GCPTREE_JOB_PROJECT: "${vars.job_project}"
  iap: { members: [group:finops@example.com] }
stages: { prod: {} }
"#;

    #[test]
    fn steps_run_in_dependency_order() {
        let (_dir, d) = deployment(FULL);
        let names: Vec<String> = all_steps(&d).iter().map(|s| s.describe(&d)).collect();
        let pos = |needle: &str| {
            names
                .iter()
                .position(|n| n.contains(needle))
                .unwrap_or_else(|| panic!("missing step `{needle}` in {names:#?}"))
        };
        assert_eq!(pos("API(s) enabled"), 0, "APIs first");
        let cache = pos("bucket gs://acme-sandbox-26c8-gcptree-cache");
        let sources = pos("bucket gs://acme-sandbox-26c8-runway-sources");
        let repo = pos("Artifact Registry repository europe-west1/runway");
        let build_sa = pos("service account runway-build@");
        let run_sa = pos("service account gcptree-run@");
        let grant_cache =
            pos("roles/storage.objectUser on bucket gs://acme-sandbox-26c8-gcptree-cache");
        let grant_repo = pos("roles/artifactregistry.writer on repository europe-west1/runway");
        let grant_sources =
            pos("roles/storage.objectViewer on bucket gs://acme-sandbox-26c8-runway-sources");
        let grant_logs = pos("roles/logging.logWriter on project acme-sandbox-26c8");
        let grant_job = pos("roles/bigquery.jobUser on project billing-data-1234");
        let iap = pos("IAP service agent");
        for (before, after) in [
            (cache, grant_cache),
            (sources, grant_sources),
            (repo, grant_repo),
            (build_sa, grant_logs),
            (run_sa, grant_job),
            (run_sa, grant_cache),
            (grant_job, iap),
        ] {
            assert!(
                before < after,
                "{} must come before {}",
                names[before],
                names[after]
            );
        }
        // Interpolation reached the service configuration.
        assert_eq!(
            d.service.env["GCPTREE_CACHE_BUCKET"],
            "gs://acme-sandbox-26c8-gcptree-cache/gcptree"
        );
        assert_eq!(d.service.env["GCPTREE_JOB_PROJECT"], "billing-data-1234");
    }

    #[test]
    fn required_apis_follow_the_configuration() {
        let (_dir, d) = deployment(FULL);
        let apis = required_apis(&d);
        for a in [
            "run.googleapis.com",
            "cloudbuild.googleapis.com",
            "artifactregistry.googleapis.com",
            "storage.googleapis.com",
            "iam.googleapis.com",
            "iap.googleapis.com",
            "cloudresourcemanager.googleapis.com",
            "logging.googleapis.com",
            "telemetry.googleapis.com",
        ] {
            assert!(apis.contains(&a.to_string()), "{a} missing from {apis:?}");
        }
        assert!(!apis.contains(&"secretmanager.googleapis.com".to_string()));
        let (_dir, mut off) = deployment(FULL);
        off.apis.enable = false;
        assert!(api_step(&off).is_none(), "nothing is enabled unless asked");
    }

    #[test]
    fn service_account_markers() {
        let m = sa_marker("gcptree", Some("prod"), "runtime");
        assert_eq!(m, "managed-by=runway app=gcptree stage=prod role=runtime");
        assert!(has_sa_marker(&m, "gcptree", Some("prod"), "runtime"));
        assert!(
            !has_sa_marker(&m, "gcptree", Some("dev"), "runtime"),
            "other stage"
        );
        assert!(
            !has_sa_marker(&m, "other", Some("prod"), "runtime"),
            "other app"
        );
        assert!(!has_sa_marker(
            "created by hand",
            "gcptree",
            Some("prod"),
            "runtime"
        ));
        assert!(!has_sa_marker(
            "managed-by=runway app=gcptreex stage=prod role=runtime",
            "gcptree",
            Some("prod"),
            "runtime"
        ));
        let b = sa_marker("gcptree", None, "build");
        assert!(has_sa_marker(&b, "gcptree", None, "build"));
    }

    #[test]
    fn resource_names() {
        assert_eq!(
            iap_resource("123", "europe-west1", "gcptree-prod"),
            "projects/123/iap_web/cloud_run-europe-west1/services/gcptree-prod"
        );
        assert_eq!(
            tag_parent("p", "europe-west1", "gcptree-prod"),
            "//run.googleapis.com/projects/p/locations/europe-west1/services/gcptree-prod"
        );
        assert_eq!(
            iap_service_agent("123"),
            "service-123@gcp-sa-iap.iam.gserviceaccount.com"
        );
    }
}
