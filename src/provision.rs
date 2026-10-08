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
use crate::config::{
    Artifact, BucketConfig, Deployment, ManagedSecret, Resolved, RoleBinding, RoleTarget,
    ScheduleConfig, ScheduleTarget,
};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::bucket;
use crate::gcp::{
    GaxError, Session, api_error, iam, is_ambiguous, is_concurrency_conflict, is_not_found,
    is_service_disabled, status_code,
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
use std::sync::{Arc, Mutex};
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
    /// Create an Artifact Registry Docker repository (the build repository,
    /// or a stage's release repository).
    CreateRepository {
        project: String,
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
    /// Revoke a grant runway recorded that is no longer in the configuration,
    /// or access the configuration does not list (see [`Provisioner::unlisted`]).
    Revoke(ManagedGrant),
    /// Delete a tag binding on the service that `service.tags` does not list.
    Untag {
        /// The binding's resource name (`tagBindings/…`).
        binding: String,
        /// `KEY/VALUE` (namespaced) for display.
        value: String,
    },
    /// Create or update a Cloud Scheduler job (after its target exists).
    Schedule(ScheduleConfig),
    /// Delete a Cloud Scheduler job runway created that is no longer configured.
    Unschedule {
        /// Full name `projects/P/locations/R/jobs/ID`.
        name: String,
    },
    /// Custom domains of the stage: load balancer, certificates, domain
    /// mappings, DNS records (see [`crate::domains`]). With `removals`, also
    /// what runway set up that `want` no longer lists.
    Domains {
        want: Box<crate::domains::Desired>,
        previous: crate::domains::Recorded,
        removals: bool,
    },
}

/// Service annotation listing the grants runway manages for the service
/// (see [`managed_grants`]): the live service is the state, no state file.
pub const ANNOTATION_GRANTS: &str = "runway.dev/grants";

/// A grant runway manages: removing it from the configuration revokes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ManagedGrant {
    /// `serviceAccount:…`, `group:…`, `user:…`, `domain:…`.
    pub member: String,
    pub role: String,
    /// The resource; `None` for the service's Identity-Aware Proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<RoleTarget>,
    /// A role of the runtime service account (as opposed to an adder or an
    /// IAP member): only revoked if runway created the account for this
    /// service, since another service may share an account it was given.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub runtime: bool,
    /// For an IAP grant (`target: None`), the service whose IAP resource
    /// holds it; `None` is the main service (`<app>-<stage>`), as in records
    /// written before named services existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// `service` of an IAP grant for this service (see [`ManagedGrant::service`]).
fn iap_service(d: &Deployment) -> Option<String> {
    d.key.as_ref().map(|_| d.service_id.clone())
}

impl ManagedGrant {
    fn on(&self) -> String {
        self.target
            .as_ref()
            .map_or("the IAP resource".into(), |t| t.to_string())
    }
}

/// What runway grants for this service and revokes once it disappears from
/// the configuration: the runtime service account's roles (declared and
/// implied by secrets and volumes), secret adders and IAP members. The build
/// service account's roles are not managed this way: other apps share it.
pub fn managed_grants(d: &Deployment) -> Vec<ManagedGrant> {
    let mut out = Vec::new();
    for step in pre_steps(d) {
        match step {
            Step::Grant { email, binding } if email == d.service.service_account => {
                out.push(ManagedGrant {
                    member: format!("serviceAccount:{email}"),
                    role: binding.role,
                    target: Some(binding.target),
                    runtime: true,
                    service: None,
                });
            }
            Step::GrantMembers { members, binding } => {
                out.extend(members.into_iter().map(|member| ManagedGrant {
                    member,
                    role: binding.role.clone(),
                    target: Some(binding.target.clone()),
                    runtime: false,
                    service: None,
                }));
            }
            _ => {}
        }
    }
    if d.service.iap.enabled && !d.is_job() {
        out.extend(d.service.iap.members.iter().map(|m| ManagedGrant {
            member: m.clone(),
            role: IAP_ACCESSOR_ROLE.into(),
            target: None,
            runtime: false,
            service: iap_service(d),
        }));
    }
    let mut unique = Vec::new();
    for g in out {
        if !unique.contains(&g) {
            unique.push(g);
        }
    }
    unique
}

/// Found by [`Provisioner::unlisted`].
#[derive(Debug, Default)]
pub struct Unlisted {
    /// Removals, one per member, role or tag.
    pub steps: Vec<Step>,
    /// What could not be read, with why: nothing is removed there.
    pub unchecked: Vec<String>,
}

/// `removals` followed by those of `more` they do not already cover (the
/// same member, role and resource).
pub fn merge_removals(mut removals: Vec<Step>, more: Vec<Step>) -> Vec<Step> {
    for step in more {
        let covered = removals.iter().any(|r| match (r, &step) {
            (Step::Revoke(a), Step::Revoke(b)) => {
                iam::same_member(&a.member, &b.member)
                    && normalize_dataset_role(&a.role) == normalize_dataset_role(&b.role)
                    && a.target == b.target
            }
            (a, b) => a == b,
        });
        if !covered {
            removals.push(step);
        }
    }
    removals
}

/// The value of [`ANNOTATION_GRANTS`].
pub fn encode_grants(grants: &[ManagedGrant]) -> String {
    serde_json::to_string(grants).expect("grants serialize")
}

/// Grants recorded on a live service; none when absent or unreadable.
pub fn recorded_grants(
    annotations: &std::collections::HashMap<String, String>,
) -> Vec<ManagedGrant> {
    annotations
        .get(ANNOTATION_GRANTS)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}

/// What to record on the service: configured grants runway granted (earlier,
/// so already recorded, or in this run), and, with `keep_removed`, recorded
/// grants removed from the configuration that are not revoked yet. A
/// configured grant that was already present when runway checked it (granted
/// by hand or by another tool) is never recorded, so never revoked.
pub fn grant_record(
    recorded: &[ManagedGrant],
    desired: &[ManagedGrant],
    added: &[ManagedGrant],
    keep_removed: bool,
) -> Vec<ManagedGrant> {
    let mut out: Vec<ManagedGrant> = desired
        .iter()
        .filter(|g| recorded.contains(g) || added.contains(g))
        .cloned()
        .collect();
    if keep_removed {
        out.extend(recorded.iter().filter(|g| !desired.contains(g)).cloned());
    }
    out
}

/// Revocations: recorded grants that are no longer in the configuration.
/// Access runway never recorded (granted by hand or by another tool) is
/// never revoked.
pub fn revoke_steps(recorded: &[ManagedGrant], d: &Deployment) -> Vec<Step> {
    let desired = managed_grants(d);
    recorded
        .iter()
        .filter(|g| !desired.contains(g))
        .cloned()
        .map(Step::Revoke)
        .collect()
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
            // Members are in the details: plans list only those that change.
            Step::GrantMembers { binding, .. } => {
                format!("grant {} on {}", binding.role, binding.target)
            }
            Step::SecretValues(names) => format!(
                "value of secret(s) {}",
                names
                    .iter()
                    .map(|n| n.rsplit('/').next().unwrap_or(n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Step::CreateRepository {
                project,
                location,
                repository,
            } => {
                if *project == d.project {
                    format!("Artifact Registry repository {location}/{repository}")
                } else {
                    format!("Artifact Registry repository {project}/{location}/{repository}")
                }
            }
            Step::CreateServiceAccount { email, .. } => format!("service account {email}"),
            Step::Grant { email, binding } => {
                let who = email.split('@').next().unwrap_or(email);
                format!("grant {} on {} to {who}", binding.role, binding.target)
            }
            Step::Tag { key, value } => format!("tag {key}={value}"),
            Step::IapInvoker => "IAP service agent can invoke the service".into(),
            Step::IapAccess => format!("IAP access ({IAP_ACCESSOR_ROLE})"),
            Step::Revoke(g) => {
                let who = g
                    .member
                    .strip_prefix("serviceAccount:")
                    .map_or(g.member.as_str(), |e| e.split('@').next().unwrap_or(e));
                format!("revoke {} on {} from {who}", g.role, g.on())
            }
            Step::Untag { value, .. } => format!("unbind tag {value}"),
            Step::Schedule(sc) => {
                format!("schedule {} ({}, {})", sc.key, sc.schedule, sc.time_zone)
            }
            Step::Unschedule { name } => {
                format!(
                    "delete schedule {}",
                    name.rsplit('/').next().unwrap_or(name)
                )
            }
            Step::Domains { want, .. } if want.is_empty() => "remove custom domains".into(),
            Step::Domains { want, .. } => format!(
                "custom domains ({})",
                match want.mode {
                    crate::config::DomainMode::LoadBalancer => "load balancer",
                    crate::config::DomainMode::ExistingLoadBalancer => "existing load balancer",
                    crate::config::DomainMode::DomainMapping => "domain mappings",
                }
            ),
        }
    }

    /// Steps that need the service to exist.
    pub fn needs_service(&self) -> bool {
        matches!(
            self,
            Step::Tag { .. } | Step::IapInvoker | Step::IapAccess | Step::Untag { .. }
        ) || matches!(self, Step::Revoke(g) if g.target.is_none())
    }

    /// Steps of a wave depend only on earlier waves: project tags (which
    /// organization policies may read) before resources, accounts and
    /// secrets before their grants, grants before the secret values check.
    /// After the rollout: service tags before IAP.
    fn wave(&self) -> u8 {
        match self {
            Step::EnableApis(_) | Step::ProjectTag { .. } | Step::Tag { .. } => 0,
            Step::CreateBucket(_)
            | Step::CreateSecret(_)
            | Step::CreateRepository { .. }
            | Step::CreateServiceAccount { .. }
            | Step::IapInvoker
            | Step::IapAccess => 1,
            Step::Grant { .. } | Step::GrantMembers { .. } => 2,
            Step::SecretValues(_) | Step::Schedule(_) | Step::Domains { .. } => 3,
            // After the rollout, once the new revision no longer needs it.
            Step::Revoke(_) | Step::Untag { .. } | Step::Unschedule { .. } => 4,
        }
    }

    /// Steps that write the same IAM policy (or BigQuery access list, which
    /// has no etag) share a lane and run one after another.
    fn lane(&self, d: &Deployment) -> String {
        match self {
            Step::Grant { binding, .. } | Step::GrantMembers { binding, .. } => {
                format!("policy of {}", binding.target)
            }
            Step::IapInvoker => "policy of the service".into(),
            Step::IapAccess => "policy of the IAP resource".into(),
            Step::Revoke(g) => match &g.target {
                Some(t) => format!("policy of {t}"),
                None => "policy of the IAP resource".into(),
            },
            other => other.describe(d),
        }
    }

    /// Needed before the image can be built: project tags, the source bucket,
    /// the repository, the build service account and its grants.
    pub fn build_prerequisite(&self, d: &Deployment) -> bool {
        let Artifact::Build(b) = &d.artifact else {
            return false;
        };
        match self {
            Step::EnableApis(_) | Step::ProjectTag { .. } | Step::CreateRepository { .. } => true,
            Step::CreateBucket(c) => c.name == b.source_bucket,
            Step::CreateServiceAccount { email, .. } | Step::Grant { email, .. } => {
                *email == b.build_service_account
            }
            _ => false,
        }
    }
}

/// Steps that run one after another, with their index in the step list.
pub type Lane<'s> = Vec<(usize, &'s Step)>;

/// Groups steps for concurrent execution: waves run in order, the lanes of a
/// wave concurrently, and the steps of a lane in order. Each step keeps its
/// index in `steps`, so results can be reported in the original order.
pub fn waves<'s>(steps: &'s [Step], d: &Deployment) -> Vec<Vec<Lane<'s>>> {
    let mut by_wave: std::collections::BTreeMap<u8, Vec<(String, Lane<'s>)>> = Default::default();
    for (i, s) in steps.iter().enumerate() {
        let lanes = by_wave.entry(s.wave()).or_default();
        let key = s.lane(d);
        match lanes.iter_mut().find(|(k, _)| *k == key) {
            Some((_, lane)) => lane.push((i, s)),
            None => lanes.push((key, vec![(i, s)])),
        }
    }
    by_wave
        .into_values()
        .map(|lanes| lanes.into_iter().map(|(_, lane)| lane).collect())
        .collect()
}

/// Outcome and detail of granting a role to members: who was added.
fn members_outcome(added: &[String]) -> (StepOutcome, String) {
    if added.is_empty() {
        (
            StepOutcome::Unchanged,
            "every member already has access".into(),
        )
    } else {
        (
            StepOutcome::Changed,
            format!("granted to {}", added.join(", ")),
        )
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
    if d.service.vpc.is_some() {
        add("compute.googleapis.com");
    }
    if !d.service.cloud_sql.is_empty() {
        add("sqladmin.googleapis.com");
    }
    let load_balanced = d.domains.mode != crate::config::DomainMode::DomainMapping
        && (d.service.preview_domain.is_some()
            || d.service.domains.iter().any(|e| !e.is_cloud_run_url()));
    if load_balanced {
        add("compute.googleapis.com");
        add("certificatemanager.googleapis.com");
    }
    // A zone in another project is that project's business.
    if (!d.service.domains.is_empty() || d.service.preview_domain.is_some())
        && d.domains
            .dns
            .as_ref()
            .is_some_and(|z| z.project == d.project)
    {
        add("dns.googleapis.com");
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

/// The custom domains step: when runway.yaml has domains, or when the
/// holder's record says some were set up (to remove them, with `removals`).
pub fn domains_step(
    r: &Resolved,
    previous: Option<crate::domains::Recorded>,
    removals: bool,
) -> Option<Step> {
    let configured = crate::domains::configured(r);
    (configured || (removals && previous.is_some())).then(|| Step::Domains {
        want: Box::new(crate::domains::desired(r)),
        previous: previous.unwrap_or_default(),
        removals,
    })
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
            project: d.project.clone(),
            location: b.artifact_location.clone(),
            repository: b.artifact_repository.clone(),
        });
        // Released images are copied to the stage's release repository.
        if let Some(r) = &d.release.repository
            && (r.project != d.project
                || r.location != b.artifact_location
                || r.repository != b.artifact_repository)
        {
            steps.push(Step::CreateRepository {
                project: r.project.clone(),
                location: r.location.clone(),
                repository: r.repository.clone(),
            });
        }
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

/// `~ field: a -> b; + field: c` for a step's details.
fn change_summary(changes: &[crate::plan::FieldChange]) -> String {
    changes
        .iter()
        .map(|c| match (&c.before, &c.after) {
            (Some(b), Some(a)) => format!("~ {}: {b} -> {a}", c.field),
            (None, Some(a)) => format!("+ {}: {a}", c.field),
            (Some(b), None) => format!("- {}: {b}", c.field),
            (None, None) => c.field.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Role the scheduler invoker gets on each target (it includes running jobs).
pub const INVOKER_ROLE: &str = "roles/run.invoker";

/// APIs the stage needs: those of every service and job, plus Cloud
/// Scheduler for schedules.
pub fn stack_required_apis(r: &Resolved) -> Vec<String> {
    let mut apis: Vec<String> = r.deployments.iter().flat_map(required_apis).collect();
    if !r.schedules.is_empty() {
        apis.push("cloudscheduler.googleapis.com".into());
    }
    apis.sort();
    apis.dedup();
    apis
}

pub fn stack_api_step(r: &Resolved) -> Option<Step> {
    r.first()
        .apis
        .enable
        .then(|| Step::EnableApis(stack_required_apis(r)))
}

/// The pre-deploy steps of every workload, each once (a service account
/// shared by several workloads is created once), plus the scheduler invoker.
pub fn stack_pre_steps(r: &Resolved) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    let d = r.first();
    let invoker =
        r.scheduler
            .as_ref()
            .filter(|sc| sc.create)
            .map(|sc| Step::CreateServiceAccount {
                email: sc.service_account.clone(),
                display_name: format!("runway scheduler ({} {})", d.app, d.stage),
                description: sa_marker(&d.app, Some(&d.stage), "scheduler"),
            });
    for step in r.deployments.iter().flat_map(pre_steps).chain(invoker) {
        let dup = out.iter().any(|x| match (x, &step) {
            (
                Step::CreateServiceAccount { email: a, .. },
                Step::CreateServiceAccount { email: b, .. },
            ) => a == b,
            (a, b) => a == b,
        });
        if !dup {
            out.push(step);
        }
    }
    out
}

/// What the scheduler invoker is granted: `run.invoker` on every target.
pub fn schedule_grant_steps(r: &Resolved) -> Vec<Step> {
    let (Some(sc), Some(d)) = (r.scheduler.as_ref(), r.deployments.first()) else {
        return Vec::new();
    };
    let mut out: Vec<Step> = Vec::new();
    for s in &r.schedules {
        let name = format!(
            "{}/{}/{}",
            d.parent(),
            match s.target {
                ScheduleTarget::Job { .. } => "jobs",
                ScheduleTarget::Service { .. } => "services",
            },
            s.target.resource_id()
        );
        let target = match s.target {
            ScheduleTarget::Job { .. } => RoleTarget::RunJob { name },
            ScheduleTarget::Service { .. } => RoleTarget::RunService { name },
        };
        let step = Step::Grant {
            email: sc.service_account.clone(),
            binding: RoleBinding {
                role: INVOKER_ROLE.into(),
                target,
            },
        };
        if !out.contains(&step) {
            out.push(step);
        }
    }
    out
}

/// After the services and jobs exist: invoker grants, then schedules.
pub fn schedule_steps(r: &Resolved) -> Vec<Step> {
    let mut out = schedule_grant_steps(r);
    out.extend(r.schedules.iter().cloned().map(Step::Schedule));
    out
}

/// What runway records and revokes for the whole stage: every workload's
/// managed grants and the scheduler invoker's.
pub fn stack_managed_grants(r: &Resolved) -> Vec<ManagedGrant> {
    let mut out: Vec<ManagedGrant> = Vec::new();
    let invoker = schedule_grant_steps(r).into_iter().filter_map(|s| match s {
        Step::Grant { email, binding } => Some(ManagedGrant {
            member: format!("serviceAccount:{email}"),
            role: binding.role,
            target: Some(binding.target),
            runtime: false,
            service: None,
        }),
        _ => None,
    });
    for g in r.deployments.iter().flat_map(managed_grants).chain(invoker) {
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

/// Revocations: recorded grants no workload of the stage wants any more.
pub fn stack_revoke_steps(recorded: &[ManagedGrant], r: &Resolved) -> Vec<Step> {
    let desired = stack_managed_grants(r);
    recorded
        .iter()
        .filter(|g| !desired.contains(g))
        .cloned()
        .map(Step::Revoke)
        .collect()
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
    /// Deploy will remove access runway granted earlier (shown with `-`).
    PendingRemoval,
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
    RunJob(String),
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
            PolicyTarget::RunJob(s) => write!(f, "job {}", naming_short(s)),
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
        PolicyTarget::RunJob(_) => "the deployer needs run.jobs.setIamPolicy (roles/run.admin)",
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
#[derive(Default, Clone)]
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
    jobs: Option<google_cloud_run_v2::client::Jobs>,
    scheduler: Option<google_cloud_scheduler_v1::client::CloudScheduler>,
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
    /// Cloud Run (jobs; services use the client given to the provisioner).
    pub run: Option<String>,
    pub scheduler: Option<String>,
    pub compute: Option<String>,
    pub certificate_manager: Option<String>,
    pub dns: Option<String>,
}

pub struct Provisioner<'a> {
    /// The service post-rollout steps (tags, IAP) apply to; stage-wide values
    /// (project, region, secrets) come from it too.
    pub d: &'a Deployment,
    /// Every service, job and schedule of the stage; `None`: just `d`.
    pub stack: Option<&'a Resolved>,
    session: &'a Session,
    run: &'a Services,
    clients: Clients,
    project_number: Arc<OnceCell<String>>,
    service_usage: String,
    /// Managed grants added (not those already present), shared by the
    /// views of [`Provisioner::for_service`]: only what runway granted is
    /// recorded, hence ever revoked.
    granted: Arc<Mutex<Vec<ManagedGrant>>>,
    /// Grant writes whose answer was lost (timeout, transport error, 5xx), as
    /// (policy, role, member): the next read decides. Present means runway's
    /// write committed, so the grant is runway's; missing means it did not.
    uncertain: Arc<Mutex<Vec<(String, String, String)>>>,
    /// What steps want said (DNS records to create elsewhere, certificate
    /// states), shared by the views; see [`Provisioner::take_notes`].
    notes: Arc<Mutex<Vec<String>>>,
    domain_endpoints: crate::domains::Endpoints,
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
        let steps = all_steps(d);
        let mut c = Clients::default();
        let has = |f: &dyn Fn(&Step) -> bool| steps.iter().any(f);
        // IAM and policy clients are always available: revoking a grant that
        // left the configuration may need any of them (building a client
        // sends no request).
        c.iam = Some(build_client_at!(Iam, session, ep.iam.clone())?);
        c.projects = Some(build_client_at!(
            Projects,
            session,
            ep.resource_manager.clone()
        )?);
        c.storage = Some(
            StorageControl::builder()
                .with_credentials(session.credentials.clone())
                .build()
                .await
                .map_err(|e| {
                    Error::internal(format!("cannot create StorageControl client: {e}"))
                })?,
        );
        c.secrets = Some(build_client_at!(
            SecretManagerService,
            session,
            ep.secret_manager.clone()
        )?);
        c.datasets = Some(build_client_at!(
            DatasetService,
            session,
            ep.bigquery.clone()
        )?);
        c.artifact = Some(build_client_at!(
            ArtifactRegistry,
            session,
            ep.artifact_registry.clone()
        )?);
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
        // Also without `service.tags`: tags bound to the service that the
        // configuration does not list are removed. Tag bindings on regional
        // resources use the regional endpoint.
        let regional = ep
            .tag_bindings
            .clone()
            .unwrap_or_else(|| format!("https://{}-cloudresourcemanager.googleapis.com", d.region));
        c.tag_bindings = Some(build_client_at!(TagBindings, session, Some(regional))?);
        if c.tag_values.is_none() {
            c.tag_values = Some(build_client_at!(
                TagValues,
                session,
                ep.resource_manager.clone()
            )?);
        }
        // Also without IAP: members recorded while it was enabled are revoked.
        c.iap = Some(build_client_at!(
            IdentityAwareProxyAdminService,
            session,
            ep.iap.clone()
        )?);
        // Jobs and schedules (also to remove those no longer configured).
        c.jobs = Some(build_client_at!(
            google_cloud_run_v2::client::Jobs,
            session,
            ep.run.clone()
        )?);
        c.scheduler = Some(build_client_at!(
            google_cloud_scheduler_v1::client::CloudScheduler,
            session,
            ep.scheduler.clone()
        )?);
        Ok(Self {
            d,
            stack: None,
            session,
            run,
            clients: c,
            project_number: Default::default(),
            granted: Default::default(),
            uncertain: Default::default(),
            notes: Default::default(),
            domain_endpoints: crate::domains::Endpoints {
                compute: ep.compute.clone(),
                certificates: ep.certificate_manager.clone(),
                dns: ep.dns.clone(),
                run: ep.run.clone(),
            },
            service_usage: ep
                .service_usage
                .clone()
                .unwrap_or_else(|| "https://serviceusage.googleapis.com".into()),
        })
    }

    /// Every service, job and schedule of the stage: stage-wide steps and
    /// removals then consider all of them.
    pub fn with_stack(mut self, r: &'a Resolved) -> Self {
        self.stack = Some(r);
        self
    }

    /// The same provisioner (clients, what it granted) for the tags and IAP
    /// of one service.
    pub fn for_service(&self, d: &'a Deployment) -> Provisioner<'a> {
        Provisioner {
            d,
            stack: self.stack,
            session: self.session,
            run: self.run,
            clients: self.clients.clone(),
            project_number: self.project_number.clone(),
            service_usage: self.service_usage.clone(),
            granted: self.granted.clone(),
            uncertain: self.uncertain.clone(),
            notes: self.notes.clone(),
            domain_endpoints: self.domain_endpoints.clone(),
        }
    }

    /// Messages steps left for the user (each once), then forgets them.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().expect("not poisoned"))
    }

    fn note(&self, n: String) {
        let mut notes = self.notes.lock().expect("not poisoned");
        if !notes.contains(&n) {
            notes.push(n);
        }
    }

    /// Runs the domains engine; DNS records to create elsewhere and
    /// certificate states become notes.
    async fn domains(
        &self,
        want: &crate::domains::Desired,
        previous: &crate::domains::Recorded,
        removals: bool,
        apply: bool,
        keep_cloud_run_urls: bool,
    ) -> Result<crate::domains::Report> {
        let d = self.d;
        let engine = crate::domains::Engine::new(
            self.session,
            &d.project,
            &d.region,
            &d.app,
            &d.stage,
            apply,
            &self.domain_endpoints,
        )
        .await?;
        let report = engine
            .reconcile(want, previous, removals, keep_cloud_run_urls)
            .await?;
        for n in &report.notes {
            self.note(n.clone());
        }
        for rec in report.manual() {
            self.note(format!(
                "DNS: create {} {} -> {} ELSEWHERE: {}",
                rec.kind, rec.name, rec.data, rec.state
            ));
        }
        Ok(report)
    }

    /// `undeploy`: what removing every custom domain of the stage does (or
    /// did, with `apply`). `NAME.cloud.run` URLs stay unless `release_urls`.
    pub async fn teardown_domains(
        &self,
        previous: &crate::domains::Recorded,
        apply: bool,
        release_urls: bool,
    ) -> Result<crate::domains::Report> {
        let want = crate::domains::desired_of(self.d, &[]);
        self.domains(&want, previous, true, apply, !release_urls)
            .await
    }

    fn workloads(&self) -> Vec<&'a Deployment> {
        match self.stack {
            Some(r) => r.deployments.iter().collect(),
            None => vec![self.d],
        }
    }

    /// Grants every workload (and the scheduler) wants.
    fn desired_grants(&self) -> Vec<ManagedGrant> {
        match self.stack {
            Some(r) => stack_managed_grants(r),
            None => managed_grants(self.d),
        }
    }

    /// Clients needed to tear down what `deploy` created (`undeploy`).
    pub async fn for_teardown(
        d: &'a Deployment,
        session: &'a Session,
        run: &'a Services,
    ) -> Result<Self> {
        Self::new(d, session, run).await
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
            PolicyTarget::RunJob(n) => {
                missing_gax(&self.clients.jobs)?
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
            PolicyTarget::RunJob(n) => {
                missing_gax(&self.clients.jobs)?
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
                        format!("grant to {}", missing.join(", ")),
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
        Ok(
            match self.add_members(t, role, members, None).await?.is_empty() {
                true => StepOutcome::Unchanged,
                false => StepOutcome::Changed,
            },
        )
    }

    /// [`Self::ensure_members`], returning the members runway added, including
    /// those of a write whose answer was lost but that committed. With
    /// `grant`, those members are recorded as runway's (see
    /// [`Self::granted`]) as soon as that is known: also when the step then
    /// fails, so that the record saved before exiting includes them.
    async fn add_members(
        &self,
        t: &PolicyTarget,
        role: &str,
        members: &[String],
        grant: Option<&dyn Fn(&str) -> ManagedGrant>,
    ) -> Result<Vec<String>> {
        let key = t.to_string();
        let record = |added: &[String]| {
            if let Some(g) = grant {
                self.record_granted(added.iter().map(|m| g(m)));
            }
        };
        match self.write_members(t, role, members, &key).await {
            Ok(added) => {
                record(&added);
                Ok(added)
            }
            Err(e) => {
                // A write whose answer was lost may have committed. Confirm with
                // one more read before failing: the process may end now, and
                // with it the memory of that write.
                if !self.has_uncertain(&key, role) {
                    return Err(e);
                }
                let Ok(p) = self.get_policy(t).await else {
                    return Err(e);
                };
                let missing = iam::missing_members(&p, role, members);
                let confirmed =
                    self.resolve_uncertain(&key, role, |m| !missing.iter().any(|x| x == m));
                record(&confirmed);
                if missing.is_empty() {
                    // The write committed: the step reached its goal.
                    return Ok(confirmed);
                }
                Err(e)
            }
        }
    }

    /// Read-modify-write of `members` on `role`, with up to four attempts.
    async fn write_members(
        &self,
        t: &PolicyTarget,
        role: &str,
        members: &[String],
        key: &str,
    ) -> Result<Vec<String>> {
        for attempt in 1..=4 {
            let mut p = self.get_policy(t).await.map_err(|e| {
                api_error(e, &format!("reading the IAM policy of {t}")).hint(needed_role_hint(t))
            })?;
            let missing = iam::missing_members(&p, role, members);
            let mut added = self.resolve_uncertain(key, role, |m| !missing.iter().any(|x| x == m));
            if !iam::add_members(&mut p, role, members) {
                return Ok(added);
            }
            match self.set_policy(t, p).await {
                Ok(_) => {
                    for m in missing {
                        if !added.contains(&m) {
                            added.push(m);
                        }
                    }
                    return Ok(added);
                }
                Err(e) => {
                    // Confirmed members stay known for the step's next attempt.
                    self.mark_uncertain(key, role, &added);
                    if is_ambiguous(&e) {
                        // The write may have committed: the next read decides.
                        self.mark_uncertain(key, role, &missing);
                    }
                    // A stale etag (conflict) means nothing was written.
                    if (is_ambiguous(&e) || is_concurrency_conflict(&e)) && attempt < 4 {
                        continue;
                    }
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
            RoleTarget::RunService { name } => Some(PolicyTarget::RunService(name.clone())),
            RoleTarget::RunJob { name } => Some(PolicyTarget::RunJob(name.clone())),
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
                project,
                location,
                repository,
            } => match self.get_repository(project, location, repository).await {
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
            Step::Revoke(g) => self.check_revoke(g).await?,
            Step::Untag { binding, .. } => {
                if self.tag_binding_exists(binding).await? {
                    (
                        StepState::PendingRemoval,
                        "not in service.tags: unbind".into(),
                    )
                } else {
                    (StepState::InSync, "already unbound".into())
                }
            }
            Step::Schedule(sc) => {
                let (live, desired) = self.schedule_state(sc).await?;
                match (live, desired) {
                    (_, None) => (
                        StepState::Pending,
                        "after the service is created (its URL is the target)".into(),
                    ),
                    (None, Some(_)) => (StepState::Pending, "create".into()),
                    (Some(live), Some(want)) => {
                        let changes = crate::plan::diff(&live, &want);
                        if changes.is_empty() {
                            (StepState::InSync, "up to date".into())
                        } else {
                            (StepState::Pending, change_summary(&changes))
                        }
                    }
                }
            }
            Step::Unschedule { name } => {
                match crate::gcp::scheduler::get(&self.scheduler()?, name).await? {
                    Some(_) => (
                        StepState::PendingRemoval,
                        "not in runway.yaml: delete".into(),
                    ),
                    None => (StepState::InSync, "already deleted".into()),
                }
            }
            Step::Domains {
                want,
                previous,
                removals,
            } => {
                let report = self
                    .domains(want, previous, *removals, false, false)
                    .await?;
                match report.changes.is_empty() {
                    true => (StepState::InSync, "up to date".into()),
                    false if want.is_empty() => {
                        (StepState::PendingRemoval, report.changes.join("; "))
                    }
                    false => (StepState::Pending, report.changes.join("; ")),
                }
            }
        })
    }

    fn scheduler(&self) -> Result<google_cloud_scheduler_v1::client::CloudScheduler> {
        missing(&self.clients.scheduler, "CloudScheduler")
    }

    pub fn scheduler_client(&self) -> Result<google_cloud_scheduler_v1::client::CloudScheduler> {
        self.scheduler()
    }

    fn scheduler_config(&self) -> Result<&'a crate::config::SchedulerConfig> {
        self.stack
            .and_then(|r| r.scheduler.as_ref())
            .ok_or_else(|| Error::internal("schedules without a scheduler account"))
    }

    /// Full name of a schedule's scheduler job.
    pub fn schedule_name(&self, sc: &ScheduleConfig) -> Result<String> {
        Ok(crate::gcp::scheduler::job_name(
            &self.d.project,
            &self.scheduler_config()?.region,
            &sc.id,
        ))
    }

    /// The live scheduler job (compared fields) and what runway would send;
    /// `None` desired while a target service has no URL yet. Fails on a
    /// scheduler job runway does not own.
    async fn schedule_state(
        &self,
        sc: &ScheduleConfig,
    ) -> Result<(
        Option<std::collections::BTreeMap<String, String>>,
        Option<std::collections::BTreeMap<String, String>>,
    )> {
        let (live, desired) = self.schedule_request(sc).await?;
        Ok((
            live.as_ref().map(crate::gcp::scheduler::flat),
            desired.map(|j| crate::gcp::scheduler::desired_flat(&j, sc.paused)),
        ))
    }

    async fn schedule_request(
        &self,
        sc: &ScheduleConfig,
    ) -> Result<(
        Option<google_cloud_scheduler_v1::model::Job>,
        Option<google_cloud_scheduler_v1::model::Job>,
    )> {
        let d = self.d;
        let name = self.schedule_name(sc)?;
        let live = crate::gcp::scheduler::get(&self.scheduler()?, &name).await?;
        if let Some(j) = &live
            && !crate::gcp::scheduler::owned(j, &d.app, &d.stage)
        {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("Cloud Scheduler job {} exists and was not created by runway for this app and stage", sc.id),
            )
            .permanent()
            .hint("rename the schedule in runway.yaml, or delete the existing scheduler job"));
        }
        let service_url = match &sc.target {
            ScheduleTarget::Service { service_id, .. } => {
                let svc_name = format!("{}/services/{service_id}", d.parent());
                match self.run.get_service().set_name(&svc_name).send().await {
                    Ok(svc) if !svc.uri.is_empty() => Some(svc.uri),
                    Ok(_) => None,
                    Err(e) if is_not_found(&e) => None,
                    Err(e) => return Err(api_error(e, &format!("reading service {service_id}"))),
                }
            }
            ScheduleTarget::Job { .. } => None,
        };
        let Some(uri) =
            crate::gcp::scheduler::target_uri(sc, &d.project, &d.region, service_url.as_deref())
        else {
            return Ok((live, None));
        };
        let invoker = &self.scheduler_config()?.service_account;
        let desired = crate::gcp::scheduler::desired(
            sc,
            &name,
            &d.app,
            &d.stage,
            invoker,
            &uri,
            service_url.as_deref(),
        );
        Ok((live, Some(desired)))
    }

    /// Scheduler jobs runway created for this stage that are not configured.
    pub async fn orphan_schedules(&self) -> Result<Vec<Step>> {
        // Without a stack (or schedules), every schedule runway created for
        // this stage is left over.
        let d = self.d;
        let region = d.scheduler_region.clone();
        let parent = crate::gcp::scheduler::parent(&d.project, &region);
        let wanted: Vec<String> = self
            .stack
            .map(|r| r.schedules.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|s| crate::gcp::scheduler::job_name(&d.project, &region, &s.id))
            .collect();
        Ok(
            crate::gcp::scheduler::list_owned(&self.scheduler()?, &parent, &d.app, &d.stage)
                .await?
                .into_iter()
                .filter(|j| !wanted.contains(&j.name))
                .map(|j| Step::Unschedule { name: j.name })
                .collect(),
        )
    }

    // ---------- applies (idempotent) ----------

    fn record_granted(&self, grants: impl IntoIterator<Item = ManagedGrant>) {
        let mut granted = self.granted.lock().expect("not poisoned");
        for g in grants {
            if !granted.contains(&g) {
                granted.push(g);
            }
        }
    }

    /// Remembers members whose write on (`policy`, `role`) had a lost answer.
    fn mark_uncertain(&self, policy: &str, role: &str, members: &[String]) {
        let mut u = self.uncertain.lock().expect("not poisoned");
        for m in members {
            let entry = (policy.to_string(), role.to_string(), m.clone());
            if !u.contains(&entry) {
                u.push(entry);
            }
        }
    }

    fn has_uncertain(&self, policy: &str, role: &str) -> bool {
        self.uncertain
            .lock()
            .expect("not poisoned")
            .iter()
            .any(|(p, r, _)| p == policy && r == role)
    }

    /// Resolves the uncertain writes on (`policy`, `role`) against a fresh
    /// read: returns the members now present (runway's write committed).
    fn resolve_uncertain(
        &self,
        policy: &str,
        role: &str,
        present: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut confirmed = Vec::new();
        self.uncertain
            .lock()
            .expect("not poisoned")
            .retain(|(p, r, m)| {
                if p != policy || r != role {
                    return true;
                }
                if present(m) {
                    confirmed.push(m.clone());
                }
                false
            });
        confirmed
    }

    /// Managed grants this provisioner added so far (see [`grant_record`]).
    pub fn granted(&self) -> Vec<ManagedGrant> {
        self.granted.lock().expect("not poisoned").clone()
    }

    /// Applies `steps` in [`waves`], each with its own retries. `report` sees
    /// every completed step, in the order of `steps`, as each wave finishes.
    /// A failure stops before the next wave (lanes already running finish
    /// first) and is returned after the steps that succeeded are reported.
    pub async fn apply_all(
        &self,
        steps: &[Step],
        retry: &crate::retry::RetryConfig,
        progress: &crate::output::Progress,
        report: &dyn Fn(&StepResult),
    ) -> Result<Vec<StepResult>> {
        let mut done = Vec::new();
        for wave in waves(steps, self.d) {
            let lanes = wave.into_iter().map(|lane| async move {
                let mut out = Vec::new();
                for (i, step) in lane {
                    let r =
                        crate::retry::with_retry(retry, progress, &step.describe(self.d), |_| {
                            self.apply(step)
                        })
                        .await;
                    let failed = r.is_err();
                    out.push((i, r));
                    if failed {
                        break;
                    }
                }
                out
            });
            let mut results: Vec<_> = futures::future::join_all(lanes)
                .await
                .into_iter()
                .flatten()
                .collect();
            results.sort_by_key(|(i, _)| *i);
            let mut error = None;
            for (_, r) in results {
                match r {
                    Ok(r) => {
                        report(&r);
                        done.push(r);
                    }
                    Err(e) => {
                        error.get_or_insert(e);
                    }
                }
            }
            if let Some(e) = error {
                return Err(e);
            }
        }
        Ok(done)
    }

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
                let grant = |m: &str| ManagedGrant {
                    member: m.into(),
                    role: binding.role.clone(),
                    target: Some(binding.target.clone()),
                    runtime: false,
                    service: None,
                };
                let added = self
                    .add_members(&t, &binding.role, members, Some(&grant))
                    .await?;
                members_outcome(&added)
            }
            Step::SecretValues(names) => self.require_secret_values(names).await?,
            Step::CreateRepository {
                project,
                location,
                repository,
            } => {
                self.ensure_repository(project, location, repository)
                    .await?
            }
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
                let grant = |m: &str| ManagedGrant {
                    member: m.into(),
                    role: IAP_ACCESSOR_ROLE.into(),
                    target: None,
                    runtime: false,
                    service: iap_service(self.d),
                };
                let added = self
                    .add_members(
                        &PolicyTarget::Iap(r),
                        IAP_ACCESSOR_ROLE,
                        &self.d.service.iap.members,
                        Some(&grant),
                    )
                    .await?;
                members_outcome(&added)
            }
            Step::Revoke(g) => self.revoke(g).await?,
            Step::Untag { binding, value } => self.unbind_tag(binding, value).await?,
            Step::Schedule(sc) => {
                let (live, desired) = self.schedule_request(sc).await?;
                let Some(desired) = desired else {
                    return Err(Error::new(
                        ErrorKind::Deploy,
                        format!("schedule {}: the target service has no URL yet", sc.key),
                    ));
                };
                let want = crate::gcp::scheduler::desired_flat(&desired, sc.paused);
                match &live {
                    Some(l)
                        if crate::plan::diff(&crate::gcp::scheduler::flat(l), &want).is_empty() =>
                    {
                        (StepOutcome::Unchanged, "up to date".into())
                    }
                    _ => {
                        let parent = crate::gcp::scheduler::parent(
                            &self.d.project,
                            &self.scheduler_config()?.region,
                        );
                        crate::gcp::scheduler::apply(
                            &self.scheduler()?,
                            &parent,
                            desired,
                            live.is_some(),
                            sc.paused,
                        )
                        .await?;
                        (
                            StepOutcome::Changed,
                            if live.is_some() { "updated" } else { "created" }.into(),
                        )
                    }
                }
            }
            Step::Domains {
                want,
                previous,
                removals,
            } => {
                let report = self.domains(want, previous, *removals, true, false).await?;
                match report.changes.is_empty() {
                    true => (StepOutcome::Unchanged, "up to date".into()),
                    false => (StepOutcome::Changed, report.changes.join("; ")),
                }
            }
            Step::Unschedule { name } => {
                let (app, stage) = (&self.d.app, &self.d.stage);
                match crate::gcp::scheduler::delete_owned(&self.scheduler()?, name, app, stage)
                    .await?
                {
                    true => (StepOutcome::Changed, "deleted".into()),
                    false => (StepOutcome::Unchanged, "already deleted".into()),
                }
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
        // Roles of runtime accounts and of the scheduler invoker are runway's
        // to record; the build account's are not managed (other apps share it).
        let runtime = self
            .workloads()
            .iter()
            .any(|w| w.service.service_account == email);
        let invoker = self
            .stack
            .and_then(|r| r.scheduler.as_ref())
            .is_some_and(|sc| sc.service_account == email);
        let managed = runtime || invoker;
        let grant = |m: &str| ManagedGrant {
            member: m.into(),
            role: b.role.clone(),
            target: Some(b.target.clone()),
            runtime,
            service: None,
        };
        let grant: Option<&dyn Fn(&str) -> ManagedGrant> = managed.then_some(&grant);
        let member = Self::sa_member(email);
        let granted = || {
            if let Some(g) = grant {
                self.record_granted([g(&member)]);
            }
            Ok((
                StepOutcome::Changed,
                format!("granted {} to {email}", b.role),
            ))
        };
        if let RoleTarget::Dataset { project, dataset } = &b.target {
            let key = b.target.to_string();
            let read = || async {
                self.get_dataset(project, dataset)
                    .await
                    .map(|ds| dataset_grants(&ds.access, &b.role, email))
                    .map_err(|e| api_error(e, &format!("reading {}", b.target)))
            };
            let ds = self
                .get_dataset(project, dataset)
                .await
                .map_err(|e| api_error(e, &format!("reading {}", b.target)))?;
            let present = dataset_grants(&ds.access, &b.role, email);
            // A patch of an earlier attempt whose answer was lost committed.
            let confirmed = !self
                .resolve_uncertain(&key, &b.role, |_| present)
                .is_empty();
            if present {
                return if confirmed {
                    granted()
                } else {
                    Ok((
                        StepOutcome::Unchanged,
                        format!("{} already granted", b.role),
                    ))
                };
            }
            let mut access = ds.access.clone();
            access.push(Access::new().set_role(&b.role).set_user_by_email(email));
            let updated = match missing(&self.clients.datasets, "BigQuery")?
                .patch_dataset()
                .set_project_id(project)
                .set_dataset_id(dataset)
                .set_dataset(Dataset::new().set_access(access))
                .send()
                .await
            {
                Ok(updated) => updated,
                Err(e) => {
                    let ambiguous = is_ambiguous(&e);
                    let err = api_error(e, &format!("granting {} on {}", b.role, b.target))
                        .hint("the deployer needs bigquery.datasets.update (roles/bigquery.dataOwner on the dataset)");
                    // The patch may have committed: confirm before failing, the
                    // process may end now.
                    if ambiguous {
                        match read().await {
                            Ok(true) => return granted(),
                            Ok(false) => {}
                            Err(_) => {
                                self.mark_uncertain(&key, &b.role, std::slice::from_ref(&member))
                            }
                        }
                    }
                    return Err(err);
                }
            };
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
            return granted();
        }
        let t = self
            .grant_target(b)
            .expect("non-dataset targets have a policy");
        let added = self
            .add_members(&t, &b.role, std::slice::from_ref(&member), grant)
            .await?;
        let outcome = if added.is_empty() {
            StepOutcome::Unchanged
        } else {
            StepOutcome::Changed
        };
        Ok((outcome, format!("{} for {email}", b.role)))
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
                // Conditional entries are not runway's: keep them.
                .filter(|a| {
                    !(a.condition.is_none()
                        && normalize_dataset_role(&a.role) == normalize_dataset_role(&b.role)
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
        self.revoke_members(&t, &b.role, &[Self::sa_member(email)])
            .await
    }

    /// Removes `members` from `role` on a policy (read-modify-write with etag).
    async fn revoke_members(
        &self,
        t: &PolicyTarget,
        role: &str,
        members: &[String],
    ) -> Result<StepOutcome> {
        for attempt in 1..=4 {
            let mut p = match self.get_policy(t).await {
                Ok(p) => p,
                Err(e) if is_not_found(&e) => return Ok(StepOutcome::Unchanged),
                Err(e) => return Err(api_error(e, &format!("reading the IAM policy of {t}"))),
            };
            if !iam::remove_members(&mut p, role, members) {
                return Ok(StepOutcome::Unchanged);
            }
            match self.set_policy(t, p).await {
                Ok(_) => return Ok(StepOutcome::Changed),
                Err(e) if (is_concurrency_conflict(&e) || is_ambiguous(&e)) && attempt < 4 => {
                    continue;
                }
                Err(e) => {
                    return Err(
                        api_error(e, &format!("revoking {role} on {t}")).hint(needed_role_hint(t))
                    );
                }
            }
        }
        unreachable!("loop returns")
    }

    /// What runway removes because the configuration does not list it, on
    /// what it owns (authoritative mode; one step per member, role or tag):
    ///
    /// - members of the IAP accessor role on the service's IAP resource;
    /// - adders of the secrets runway created for this app and stage;
    /// - roles of the runtime account, when runway created it for this
    ///   service, on every resource runway knows: configured and recorded
    ///   targets, and the deployment project;
    /// - tags bound directly to the service (inherited ones are not bindings
    ///   on the service and are left alone).
    ///
    /// Only unconditional bindings; other roles are never touched. The service
    /// must exist. Each of the four is read on its own: one that cannot be
    /// read is reported in [`Unlisted::unchecked`], the others still count.
    pub async fn unlisted(&self, recorded: &[ManagedGrant]) -> Unlisted {
        let (mut shared, service) =
            tokio::join!(self.unlisted_shared(recorded), self.unlisted_service());
        shared.steps.extend(service.steps);
        shared.unchecked.extend(service.unchecked);
        shared
    }

    /// Stage-wide removals: secret adders and roles of runtime accounts,
    /// against what every workload of the stage wants.
    pub async fn unlisted_shared(&self, recorded: &[ManagedGrant]) -> Unlisted {
        let (adders, runtime) = tokio::join!(self.unlisted_adders(), self.unlisted_roles(recorded));
        Self::collect_unlisted([
            ("secret adders", adders),
            ("runtime account roles", runtime),
        ])
    }

    /// Removals on this provisioner's service: IAP access and tags.
    pub async fn unlisted_service(&self) -> Unlisted {
        let (iap, tags) = tokio::join!(self.unlisted_iap(), self.unlisted_tags());
        let mut out = Self::collect_unlisted([("IAP access", iap), ("service tags", tags)]);
        // The IAP grants of a named service name it (see `ManagedGrant::service`).
        for s in &mut out.steps {
            if let Step::Revoke(g) = s
                && g.target.is_none()
            {
                g.service = iap_service(self.d);
            }
        }
        out
    }

    fn collect_unlisted<const N: usize>(found: [(&str, Result<Vec<Step>>); N]) -> Unlisted {
        let mut out = Unlisted::default();
        for (what, found) in found {
            match found {
                Ok(steps) => out.steps.extend(steps),
                Err(e) => out.unchecked.push(format!("{what}: {e}")),
            }
        }
        out
    }

    fn unlisted_revoke(
        member: &str,
        role: &str,
        target: Option<RoleTarget>,
        runtime: bool,
    ) -> Step {
        Step::Revoke(ManagedGrant {
            member: member.into(),
            role: role.into(),
            target,
            runtime,
            service: None,
        })
    }

    /// IAP accessors on the service's IAP resource, whether IAP is configured
    /// or not (members granted by hand stay after IAP is disabled).
    async fn unlisted_iap(&self) -> Result<Vec<Step>> {
        let d = self.d;
        let t = PolicyTarget::Iap(iap_resource(
            &self.project_number().await?,
            &d.region,
            &d.service_id,
        ));
        let configured: &[String] = if d.service.iap.enabled {
            &d.service.iap.members
        } else {
            &[]
        };
        let p = match self.get_policy(&t).await {
            Ok(p) => p,
            Err(e) if is_not_found(&e) => return Ok(Vec::new()),
            // IAP never used here: without its API, no IAP access is in
            // effect, and enabling IAP again brings this check back.
            Err(e) if !d.service.iap.enabled && is_service_disabled(&e) => {
                return Ok(Vec::new());
            }
            Err(e) => {
                return Err(api_error(e, &format!("reading the IAM policy of {t}"))
                    .hint(needed_role_hint(&t)));
            }
        };
        Ok(iam::members_of(&p, IAP_ACCESSOR_ROLE)
            .into_iter()
            .filter(|m| !configured.iter().any(|x| iam::same_member(x, m)))
            .map(|m| Self::unlisted_revoke(&m, IAP_ACCESSOR_ROLE, None, false))
            .collect())
    }

    /// Adders of the secrets runway created for this app and stage.
    async fn unlisted_adders(&self) -> Result<Vec<Step>> {
        let d = self.d;
        let mut out = Vec::new();
        let ours = naming::ownership_labels(&d.app, &d.stage);
        for s in d.secrets.values() {
            let created_by_runway = match self.get_secret(&s.name).await {
                Ok(secret) => ours.iter().all(|(k, v)| secret.labels.get(k) == Some(v)),
                Err(e) if is_not_found(&e) => false,
                Err(e) => return Err(api_error(e, &format!("reading secret {}", s.name))),
            };
            if !created_by_runway {
                continue;
            }
            let target = RoleTarget::Secret {
                name: format!("projects/{}/secrets/{}", d.project, s.name),
            };
            let t = PolicyTarget::Secret(format!("projects/{}/secrets/{}", d.project, s.name));
            if let Some(p) = self.read_policy(&t).await? {
                for m in iam::members_of(&p, SECRET_ADDER_ROLE) {
                    if !s.adders.iter().any(|x| iam::same_member(x, &m)) {
                        out.push(Self::unlisted_revoke(
                            &m,
                            SECRET_ADDER_ROLE,
                            Some(target.clone()),
                            false,
                        ));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Roles of the runtime accounts runway created for this stage.
    async fn unlisted_roles(&self, recorded: &[ManagedGrant]) -> Result<Vec<Step>> {
        let mut accounts: Vec<&str> = Vec::new();
        for w in self.workloads() {
            if !accounts.contains(&w.service.service_account.as_str()) {
                accounts.push(&w.service.service_account);
            }
        }
        let mut out = Vec::new();
        for email in accounts {
            out.extend(self.unlisted_roles_of(email, recorded).await?);
        }
        Ok(out)
    }

    async fn unlisted_roles_of(&self, email: &str, recorded: &[ManagedGrant]) -> Result<Vec<Step>> {
        let d = self.d;
        let mut out = Vec::new();
        let owned = match self.service_account_description(email).await? {
            Some(desc) => has_sa_marker(&desc, &d.app, Some(&d.stage), "runtime"),
            None => false,
        };
        if owned {
            let member = Self::sa_member(email);
            // Every configured grant to the account, also as a secret adder.
            let configured: Vec<ManagedGrant> = self
                .desired_grants()
                .into_iter()
                .filter(|g| iam::same_member(&g.member, &member))
                .collect();
            let mut targets: Vec<RoleTarget> = vec![RoleTarget::Project {
                project: d.project.clone(),
            }];
            for t in configured
                .iter()
                .chain(
                    recorded
                        .iter()
                        .filter(|g| g.runtime && iam::same_member(&g.member, &member)),
                )
                .filter_map(|g| g.target.clone())
            {
                if !targets.contains(&t) {
                    targets.push(t);
                }
            }
            for target in targets {
                let wanted = |role: &str| {
                    configured.iter().any(|g| {
                        g.target.as_ref() == Some(&target)
                            && normalize_dataset_role(&g.role) == normalize_dataset_role(role)
                    })
                };
                let held: Vec<String> = match &target {
                    RoleTarget::Dataset { project, dataset } => {
                        match self.get_dataset(project, dataset).await {
                            Ok(ds) => ds
                                .access
                                .iter()
                                .filter(|a| {
                                    a.condition.is_none()
                                        && (a.user_by_email.eq_ignore_ascii_case(email)
                                            || iam::same_member(&a.iam_member, &member))
                                })
                                .map(|a| normalize_dataset_role(&a.role).to_string())
                                .collect(),
                            Err(e) if is_not_found(&e) => Vec::new(),
                            Err(e) => {
                                return Err(api_error(e, &format!("reading {target}")));
                            }
                        }
                    }
                    t => {
                        let policy = self.grant_target(&RoleBinding {
                            role: String::new(),
                            target: t.clone(),
                        });
                        match policy {
                            Some(pt) => match self.read_policy(&pt).await? {
                                Some(p) => iam::roles_of(&p, &member),
                                None => Vec::new(),
                            },
                            None => Vec::new(),
                        }
                    }
                };
                for role in held {
                    if !wanted(&role) {
                        out.push(Self::unlisted_revoke(
                            &member,
                            &role,
                            Some(target.clone()),
                            true,
                        ));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Tags bound directly to the service.
    async fn unlisted_tags(&self) -> Result<Vec<Step>> {
        let mut out = Vec::new();
        let wanted = self.wanted_tag_values().await?;
        for b in self.service_tag_bindings().await? {
            if !wanted.contains(&b.tag_value) {
                let value = if b.tag_value_namespaced_name.is_empty() {
                    self.tag_value_name(&b.tag_value).await
                } else {
                    b.tag_value_namespaced_name.clone()
                };
                out.push(Step::Untag {
                    binding: b.name.clone(),
                    value,
                });
            }
        }
        Ok(out)
    }

    /// An IAM policy, `None` when its resource does not exist.
    async fn read_policy(&self, t: &PolicyTarget) -> Result<Option<Policy>> {
        match self.get_policy(t).await {
            Ok(p) => Ok(Some(p)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => {
                Err(api_error(e, &format!("reading the IAM policy of {t}"))
                    .hint(needed_role_hint(t)))
            }
        }
    }

    /// Resource names (`tagValues/…`) of the values `service.tags` lists.
    async fn wanted_tag_values(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for (key, value) in &self.d.service.tags {
            let namespaced = format!("{key}/{value}");
            let tv = missing(&self.clients.tag_values, "TagValues")?
                .get_namespaced_tag_value()
                .set_name(&namespaced)
                .send()
                .await
                .map_err(|e| api_error(e, &format!("looking up tag value {namespaced}")))?;
            out.push(tv.name);
        }
        Ok(out)
    }

    /// `KEY/VALUE` of a tag value, or its resource name if it cannot be read.
    async fn tag_value_name(&self, value: &str) -> String {
        match missing(&self.clients.tag_values, "TagValues") {
            Ok(c) => match c.get_tag_value().set_name(value).send().await {
                Ok(tv) if !tv.namespaced_name.is_empty() => tv.namespaced_name,
                _ => value.to_string(),
            },
            Err(_) => value.to_string(),
        }
    }

    /// Tags bound directly to the service (none when it does not exist).
    async fn service_tag_bindings(&self) -> Result<Vec<TagBinding>> {
        let parent = tag_parent(&self.d.project, &self.d.region, &self.d.service_id);
        let client = missing(&self.clients.tag_bindings, "TagBindings")?;
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let resp = match client
                .list_tag_bindings()
                .set_parent(&parent)
                .set_page_token(token.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) if is_not_found(&e) => return Ok(out),
                Err(e) => return Err(api_error(e, "listing the service's tag bindings")),
            };
            out.extend(resp.tag_bindings);
            if resp.next_page_token.is_empty() {
                return Ok(out);
            }
            token = resp.next_page_token;
        }
    }

    async fn tag_binding_exists(&self, binding: &str) -> Result<bool> {
        Ok(self
            .service_tag_bindings()
            .await?
            .iter()
            .any(|b| b.name == binding))
    }

    async fn unbind_tag(&self, binding: &str, value: &str) -> Result<(StepOutcome, String)> {
        let res = missing(&self.clients.tag_bindings, "TagBindings")?
            .delete_tag_binding()
            .set_name(binding)
            .poller()
            .until_done()
            .await;
        match res {
            Ok(_) => Ok((StepOutcome::Changed, format!("unbound {value}"))),
            Err(e) if is_not_found(&e) => {
                Ok((StepOutcome::Unchanged, format!("{value} already unbound")))
            }
            Err(e) => Err(api_error(e, &format!("unbinding tag {value}"))
                .hint("the deployer needs roles/resourcemanager.tagUser on the tag value")),
        }
    }

    /// Why a recorded runtime role is kept rather than revoked: the account
    /// was not created by runway for this service (another one may share it
    /// and need the role), or it no longer exists.
    async fn kept_account(&self, g: &ManagedGrant) -> Result<Option<String>> {
        let Some(email) = g
            .member
            .strip_prefix("serviceAccount:")
            .filter(|_| g.runtime)
        else {
            return Ok(None);
        };
        Ok(match self.service_account_description(email).await? {
            Some(desc) if has_sa_marker(&desc, &self.d.app, Some(&self.d.stage), "runtime") => None,
            Some(_) => Some(format!(
                "kept: {email} was not created by runway for this service"
            )),
            None => Some(format!("{email} no longer exists")),
        })
    }

    /// The policy holding a recorded grant (`None` for a BigQuery dataset).
    async fn revoke_target(&self, g: &ManagedGrant) -> Result<Option<PolicyTarget>> {
        Ok(match &g.target {
            Some(RoleTarget::Dataset { .. }) => None,
            Some(t) => self.grant_target(&RoleBinding {
                role: g.role.clone(),
                target: t.clone(),
            }),
            None => Some(PolicyTarget::Iap(iap_resource(
                &self.project_number().await?,
                &self.d.region,
                g.service
                    .as_deref()
                    .unwrap_or(&naming::service_id(&self.d.app, &self.d.stage)),
            ))),
        })
    }

    async fn check_revoke(&self, g: &ManagedGrant) -> Result<(StepState, String)> {
        if let Some(why) = self.kept_account(g).await? {
            return Ok((StepState::InSync, why));
        }
        let present = match (&g.target, self.revoke_target(g).await?) {
            (Some(RoleTarget::Dataset { project, dataset }), _) => {
                let email = g
                    .member
                    .strip_prefix("serviceAccount:")
                    .unwrap_or(&g.member);
                match self.get_dataset(project, dataset).await {
                    Ok(ds) => dataset_grants(&ds.access, &g.role, email),
                    Err(e) if is_not_found(&e) => false,
                    Err(e) => {
                        return Err(api_error(
                            e,
                            &format!("reading dataset {project}.{dataset}"),
                        ));
                    }
                }
            }
            (_, Some(t)) => match self.get_policy(&t).await {
                Ok(p) => {
                    iam::missing_members(&p, &g.role, std::slice::from_ref(&g.member)).is_empty()
                }
                Err(e) if is_not_found(&e) => false,
                Err(e) => {
                    return Err(api_error(e, &format!("reading the IAM policy of {t}"))
                        .hint(needed_role_hint(&t)));
                }
            },
            (_, None) => return Err(Error::internal("unsupported grant target")),
        };
        Ok(if present {
            (
                StepState::PendingRemoval,
                format!("removed from runway.yaml: revoke from {}", g.member),
            )
        } else {
            (
                StepState::InSync,
                format!("{} already has no access", g.member),
            )
        })
    }

    async fn revoke(&self, g: &ManagedGrant) -> Result<(StepOutcome, String)> {
        if let Some(why) = self.kept_account(g).await? {
            return Ok((StepOutcome::Unchanged, why));
        }
        let outcome = match (&g.target, g.member.strip_prefix("serviceAccount:")) {
            (Some(t), Some(email)) if g.runtime => {
                self.revoke_grant(
                    email,
                    &RoleBinding {
                        role: g.role.clone(),
                        target: t.clone(),
                    },
                )
                .await?
            }
            _ => {
                let t = self
                    .revoke_target(g)
                    .await?
                    .ok_or_else(|| Error::internal("unsupported grant target"))?;
                self.revoke_members(&t, &g.role, std::slice::from_ref(&g.member))
                    .await?
            }
        };
        Ok((
            outcome,
            match outcome {
                StepOutcome::Changed => format!("revoked from {}", g.member),
                StepOutcome::Unchanged => format!("{} already has no access", g.member),
            },
        ))
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
        project: &str,
        location: &str,
        repository: &str,
    ) -> std::result::Result<Repository, GaxError> {
        missing_gax(&self.clients.artifact)?
            .get_repository()
            .set_name(format!(
                "projects/{project}/locations/{location}/repositories/{repository}"
            ))
            .send()
            .await
    }

    async fn ensure_repository(
        &self,
        project: &str,
        location: &str,
        repository: &str,
    ) -> Result<(StepOutcome, String)> {
        match self.get_repository(project, location, repository).await {
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
            .set_parent(format!("projects/{project}/locations/{location}"))
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
            .deployments[0]
            .clone();
        (dir, d)
    }

    const FULL: &str = r#"
version: 1
app: gcptree
vars: { job_project: billing-data-1234 }
provider:
  project: my-gcp-project
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

    /// Describes every wave as lanes of step names.
    fn wave_names(steps: &[Step], d: &Deployment) -> Vec<Vec<Vec<String>>> {
        waves(steps, d)
            .into_iter()
            .map(|w| {
                w.into_iter()
                    .map(|lane| lane.into_iter().map(|(_, s)| s.describe(d)).collect())
                    .collect()
            })
            .collect()
    }

    fn wave_of(waves: &[Vec<Vec<String>>], needle: &str) -> usize {
        waves
            .iter()
            .position(|w| w.iter().flatten().any(|n| n.contains(needle)))
            .unwrap_or_else(|| panic!("missing `{needle}` in {waves:#?}"))
    }

    #[test]
    fn waves_keep_dependencies_and_serialize_writes_to_one_policy() {
        let yaml = FULL
            .replace(
                "      - { role: roles/storage.objectUser, bucket: \"${buckets.cache}\" }",
                "      - { role: roles/storage.objectUser, bucket: \"${buckets.cache}\" }\n      - { role: roles/storage.objectViewer, bucket: \"${project}-runway-sources\" }\n  secrets:\n    API_KEY: { secret: \"${secrets.api-key}\" }",
            )
            .replace(
                "buckets:",
                "secrets:\n  api-key: { adders: [group:devs@example.com] }\nbuckets:",
            );
        let (_dir, d) = deployment(&yaml);
        let steps = pre_steps(&d);
        let w = wave_names(&steps, &d);
        let account = wave_of(&w, "service account gcptree-run@");
        let secret = wave_of(&w, "secret api-key");
        assert_eq!(
            account,
            wave_of(&w, "bucket gs://my-gcp-project-gcptree-cache")
        );
        assert_eq!(account, secret, "resources are created together");
        assert!(account < wave_of(&w, "roles/storage.objectUser on bucket"));
        assert!(secret < wave_of(&w, "roles/secretmanager.secretVersionAdder"));
        assert!(wave_of(&w, "roles/secretmanager.secretAccessor") < wave_of(&w, "value of secret"));

        // Two accounts granted on the source bucket: one lane, in step order.
        let grants = &w[wave_of(&w, "roles/storage.objectUser on bucket")];
        let lane = |needle: &str| {
            grants
                .iter()
                .find(|l| l.iter().any(|n| n.contains(needle)))
                .unwrap_or_else(|| panic!("no lane with `{needle}`: {grants:#?}"))
        };
        let sources = lane("on bucket gs://my-gcp-project-runway-sources");
        assert_eq!(sources.len(), 2, "{sources:#?}");
        assert!(
            sources[0].ends_with("to runway-build"),
            "build grant first: {sources:#?}"
        );
        assert!(sources[1].ends_with("to gcptree-run"));
        let secret_policy = lane("on secret projects/my-gcp-project/secrets/api-key");
        assert_eq!(
            secret_policy.len(),
            2,
            "adders and accessor share the policy"
        );
        assert_ne!(
            lane("on bucket gs://my-gcp-project-gcptree-cache"),
            lane("on project billing-data-1234"),
            "different policies run concurrently"
        );
        assert_eq!(
            waves(&steps, &d).iter().flatten().flatten().count(),
            steps.len(),
            "every step exactly once"
        );
    }

    #[test]
    fn a_build_waits_only_for_the_build_resources() {
        let (_dir, d) = deployment(FULL);
        let (build, rest): (Vec<_>, Vec<_>) = pre_steps(&d)
            .into_iter()
            .partition(|s| s.build_prerequisite(&d));
        let names = |v: &[Step]| v.iter().map(|s| s.describe(&d)).collect::<Vec<_>>();
        let (build, rest) = (names(&build), names(&rest));
        for needed in [
            "bucket gs://my-gcp-project-runway-sources",
            "Artifact Registry repository europe-west1/runway",
            "service account runway-build@",
            "roles/logging.logWriter on project my-gcp-project to runway-build",
            "roles/artifactregistry.writer",
            "roles/storage.objectViewer on bucket gs://my-gcp-project-runway-sources",
        ] {
            assert!(
                build.iter().any(|n| n.contains(needed)),
                "{needed}: {build:#?}"
            );
        }
        for later in [
            "bucket gs://my-gcp-project-gcptree-cache",
            "service account gcptree-run@",
            "roles/bigquery.jobUser",
            "roles/storage.objectUser",
        ] {
            assert!(rest.iter().any(|n| n.contains(later)), "{later}: {rest:#?}");
        }
    }

    #[test]
    fn grants_removed_from_the_configuration_are_revoked_once_recorded() {
        let yaml = FULL.replace(
            "  iap: { members: [group:finops@example.com] }",
            "  secrets:\n    API_KEY: { secret: \"${secrets.api-key}\" }\n  iap: { members: [group:finops@example.com] }",
        )
        .replace(
            "buckets:",
            "secrets:\n  api-key: { adders: [group:devs@example.com] }\nbuckets:",
        );
        let (_dir, d) = deployment(&yaml);
        let managed = managed_grants(&d);
        let has =
            |member: &str, role: &str| managed.iter().any(|g| g.member == member && g.role == role);
        let run = "serviceAccount:gcptree-run@my-gcp-project.iam.gserviceaccount.com";
        assert!(has(run, "roles/bigquery.jobUser"), "declared role");
        assert!(
            has(run, "roles/secretmanager.secretAccessor"),
            "implied by a secret"
        );
        assert!(has("group:devs@example.com", SECRET_ADDER_ROLE), "adder");
        assert!(
            has("group:finops@example.com", IAP_ACCESSOR_ROLE),
            "IAP member"
        );
        assert!(
            !managed.iter().any(|g| g.member.contains("runway-build@")),
            "the shared build account is never revoked: {managed:#?}"
        );
        assert!(
            managed
                .iter()
                .filter(|g| g.runtime)
                .all(|g| g.member == run)
        );

        // Round trip through the service annotation.
        let annotations = [(ANNOTATION_GRANTS.to_string(), encode_grants(&managed))].into();
        assert_eq!(recorded_grants(&annotations), managed);
        assert!(recorded_grants(&Default::default()).is_empty());

        // Recorded but no longer configured: revoked. Configured: kept.
        let old = ManagedGrant {
            member: "group:old@example.com".into(),
            role: IAP_ACCESSOR_ROLE.into(),
            target: None,
            runtime: false,
            service: None,
        };
        let mut recorded = managed.clone();
        recorded.push(old.clone());
        let revokes = revoke_steps(&recorded, &d);
        assert_eq!(revokes, [Step::Revoke(old)]);
        assert_eq!(
            revokes[0].describe(&d),
            "revoke roles/iap.httpsResourceAccessor on the IAP resource from group:old@example.com"
        );
        assert!(revokes[0].needs_service());
        assert!(revoke_steps(&managed, &d).is_empty(), "nothing removed");

        // Revocations come after every other step.
        let mut all = post_steps(&d);
        all.extend(revokes);
        let w = wave_names(&all, &d);
        assert!(w.last().unwrap()[0][0].starts_with("revoke "), "{w:#?}");
    }

    #[test]
    fn only_grants_runway_added_are_recorded() {
        let g = |member: &str| ManagedGrant {
            member: member.into(),
            role: IAP_ACCESSOR_ROLE.into(),
            target: None,
            runtime: false,
            service: None,
        };
        let (earlier, added, manual, removed) =
            (g("group:a"), g("group:b"), g("group:c"), g("group:r"));
        let recorded = [earlier.clone(), removed.clone()];
        let desired = [earlier.clone(), added.clone(), manual.clone()];
        // `manual` was already in place when runway checked it: not runway's.
        assert_eq!(
            grant_record(&recorded, &desired, std::slice::from_ref(&added), true),
            [earlier.clone(), added.clone(), removed],
            "removed grants stay recorded until revoked"
        );
        assert_eq!(
            grant_record(&recorded, &desired, std::slice::from_ref(&added), false),
            [earlier, added],
            "once revoked"
        );
    }

    #[test]
    fn service_tags_come_before_iap_which_uses_two_policies() {
        let yaml = FULL.replace(
            "  iap: { members: [group:finops@example.com] }",
            "  tags: { \"123/allow\": \"yes\" }\n  iap: { members: [group:finops@example.com] }",
        );
        let (_dir, d) = deployment(&yaml);
        let w = wave_names(&post_steps(&d), &d);
        assert_eq!(
            w,
            vec![
                vec![vec!["tag 123/allow=yes".to_string()]],
                vec![
                    vec!["IAP service agent can invoke the service".to_string()],
                    vec!["IAP access (roles/iap.httpsResourceAccessor)".to_string()],
                ],
            ]
        );
    }

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
        let cache = pos("bucket gs://my-gcp-project-gcptree-cache");
        let sources = pos("bucket gs://my-gcp-project-runway-sources");
        let repo = pos("Artifact Registry repository europe-west1/runway");
        let build_sa = pos("service account runway-build@");
        let run_sa = pos("service account gcptree-run@");
        let grant_cache =
            pos("roles/storage.objectUser on bucket gs://my-gcp-project-gcptree-cache");
        let grant_repo = pos("roles/artifactregistry.writer on repository europe-west1/runway");
        let grant_sources =
            pos("roles/storage.objectViewer on bucket gs://my-gcp-project-runway-sources");
        let grant_logs = pos("roles/logging.logWriter on project my-gcp-project");
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
            "gs://my-gcp-project-gcptree-cache/gcptree"
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
        assert!(!apis.contains(&"compute.googleapis.com".to_string()));
        assert!(!apis.contains(&"sqladmin.googleapis.com".to_string()));
        let (_dir, mut networked) = deployment(FULL);
        networked.service.vpc = Some(crate::config::VpcConfig {
            network: "default".into(),
            subnet: "default".into(),
            egress: "private-ranges-only".into(),
            network_tags: vec![],
        });
        networked.service.cloud_sql = vec!["p:europe-west1:db".into()];
        let apis = required_apis(&networked);
        assert!(
            apis.contains(&"compute.googleapis.com".to_string()),
            "Direct VPC"
        );
        assert!(
            apis.contains(&"sqladmin.googleapis.com".to_string()),
            "Cloud SQL"
        );
        let (_dir, mut off) = deployment(FULL);
        off.apis.enable = false;
        assert!(api_step(&off).is_none(), "nothing is enabled unless asked");
    }

    const STACK: &str = r#"
version: 1
app: shop
provider:
  project: my-gcp-project
  region: europe-west1
  enable_apis: true
  artifact_repository: applications
  source_bucket: my-gcp-build-sources
  build_service_account: builds@my-gcp-project.iam.gserviceaccount.com
defaults:
  source: .
  service_account: shop-runtime@my-gcp-project.iam.gserviceaccount.com
  identity:
    create: true
    roles: [{role: roles/storage.objectViewer, bucket: shop-data}]
services:
  web:
    iap: {members: [group:team@example.com]}
jobs:
  report:
    identity:
      create: true
      roles: [{role: roles/bigquery.jobUser, project: my-gcp-project}]
schedules:
  nightly: {schedule: "0 3 * * *", job: report}
  warm: {schedule: "*/5 * * * *", service: web}
stages: { prod: {} }
"#;

    fn stack(yaml: &str) -> (tempfile::TempDir, crate::config::Resolved) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(&p, yaml).unwrap();
        let r = crate::config::load_and_resolve(&p, "prod", &Default::default())
            .unwrap_or_else(|e| panic!("{e:?}"))
            .1;
        (dir, r)
    }

    #[test]
    fn stage_steps_are_merged_across_workloads() {
        let (_dir, r) = stack(STACK);
        let steps = stack_pre_steps(&r);
        let creates = |email: &str| {
            steps
                .iter()
                .filter(|s| matches!(s, Step::CreateServiceAccount { email: e, .. } if e == email))
                .count()
        };
        assert_eq!(
            creates("shop-runtime@my-gcp-project.iam.gserviceaccount.com"),
            1,
            "shared: created once"
        );
        assert_eq!(
            creates("shop-prod-sched@my-gcp-project.iam.gserviceaccount.com"),
            1
        );
        // Both workloads' roles on the shared account.
        let grants: Vec<&str> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Grant { binding, .. } => Some(binding.role.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            grants.contains(&"roles/storage.objectViewer")
                && grants.contains(&"roles/bigquery.jobUser")
        );
        assert!(stack_required_apis(&r).contains(&"cloudscheduler.googleapis.com".to_string()));
        assert!(stack_required_apis(&r).contains(&"iap.googleapis.com".to_string()));
    }

    #[test]
    fn the_scheduler_invoker_gets_run_invoker_on_targets_only() {
        let (_dir, r) = stack(STACK);
        let targets: Vec<String> = schedule_grant_steps(&r)
            .iter()
            .map(|s| match s {
                Step::Grant { email, binding } => {
                    assert_eq!(
                        email,
                        "shop-prod-sched@my-gcp-project.iam.gserviceaccount.com"
                    );
                    assert_eq!(binding.role, INVOKER_ROLE);
                    binding.target.to_string()
                }
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(targets, ["job shop-report-prod", "service shop-web-prod"]);
        let steps = schedule_steps(&r);
        assert!(matches!(steps.last(), Some(Step::Schedule(s)) if s.key == "warm"));
        // Recorded, so removing a schedule revokes its grant.
        let managed = stack_managed_grants(&r);
        assert!(managed.iter().any(|g| g.role == INVOKER_ROLE && !g.runtime));
        // A grant only the job wanted is revoked once nobody wants it.
        let mut recorded = managed.clone();
        recorded.push(ManagedGrant {
            member: "serviceAccount:shop-runtime@my-gcp-project.iam.gserviceaccount.com".into(),
            role: "roles/old".into(),
            target: Some(RoleTarget::Project {
                project: "my-gcp-project".into(),
            }),
            runtime: true,
            service: None,
        });
        let revokes = stack_revoke_steps(&recorded, &r);
        assert_eq!(revokes.len(), 1);
        assert!(matches!(&revokes[0], Step::Revoke(g) if g.role == "roles/old"));
    }

    #[test]
    fn iap_grants_of_a_named_service_name_it_and_old_records_still_read() {
        let (_dir, r) = stack(STACK);
        let web = r
            .services()
            .find(|d| d.key.as_deref() == Some("web"))
            .unwrap();
        let iap: Vec<ManagedGrant> = managed_grants(web)
            .into_iter()
            .filter(|g| g.target.is_none())
            .collect();
        assert_eq!(iap.len(), 1);
        assert_eq!(iap[0].service.as_deref(), Some("shop-web-prod"));
        assert!(encode_grants(&iap).contains(r#""service":"shop-web-prod""#));
        // Written before named services: no `service`, meaning the main one.
        let old =
            r#"[{"member":"group:team@example.com","role":"roles/iap.httpsResourceAccessor"}]"#;
        let read = recorded_grants(&[(ANNOTATION_GRANTS.to_string(), old.to_string())].into());
        assert_eq!(read[0].service, None);
        assert_eq!(encode_grants(&read), old, "rewritten unchanged");
    }

    #[test]
    fn creating_build_resources_creates_the_release_repository() {
        let yaml = FULL.replace(
            "stages:",
            "release:\n  repository: {project: my-release-project, repository: releases}\nstages:",
        );
        let (_dir, d) = deployment(&yaml);
        let repos: Vec<(String, String)> = pre_steps(&d)
            .into_iter()
            .filter_map(|s| match s {
                Step::CreateRepository {
                    project,
                    repository,
                    ..
                } => Some((project, repository)),
                _ => None,
            })
            .collect();
        assert!(
            repos.contains(&("my-release-project".into(), "releases".into())),
            "{repos:?}"
        );
        assert!(
            repos.iter().any(|(p, _)| p == "my-gcp-project"),
            "and the build repository"
        );
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
