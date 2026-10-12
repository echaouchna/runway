# Permissions

runway keeps three identities separate:

| Identity | What it does | Minimum roles |
|----------|--------------|---------------|
| **Deployer** (you / CI) | runs `runway`; uploads sources, submits builds, creates/updates the service, reads logs | `roles/run.developer` (+ `run.services.setIamPolicy`, e.g. `roles/run.admin`, to switch public access), `roles/cloudbuild.builds.editor`, `roles/logging.viewer`, `roles/storage.objectUser` on the bucket, `roles/artifactregistry.reader` on the repository, `roles/iam.serviceAccountUser` on the runtime and build service accounts |
| **Build service account** | runs Cloud Build: reads the archive, pushes the image, writes build logs | `roles/storage.objectViewer` on the bucket, `roles/artifactregistry.writer` on the repository, `roles/logging.logWriter` on the project |
| **Runtime service account** | identity of the running container | `roles/secretmanager.secretAccessor` on each referenced secret, plus whatever the app needs |

The Cloud Run service agent pulls images from Artifact Registry in the same
project automatically; for a repository in another project grant
`service-<PROJECT_NUMBER>@serverless-robot-prod.iam.gserviceaccount.com`
`roles/artifactregistry.reader` there.

Optional, for a complete `runway doctor` report: `roles/browser` (project),
`roles/serviceusage.serviceUsageViewer` and `roles/secretmanager.viewer`.

Extra deployer permissions for the optional features (keys written `service.…`
work the same in `services.<name>.…`, `jobs.<name>.…` and `defaults.…`):

| Feature | Deployer needs |
|---------|----------------|
| `service.identity.create` | `roles/iam.serviceAccountAdmin` on the service account's project (and `roles/iam.serviceAccountUser` on the account once created; grant it at project level or re-run after granting) |
| `service.identity.roles` on a project | `roles/resourcemanager.projectIamAdmin` on that project |
| `service.identity.roles` on a bucket | `roles/storage.admin` on that bucket (`storage.buckets.setIamPolicy`) |
| `service.identity.roles` on a dataset | `roles/bigquery.dataOwner` on that dataset |
| `service.identity.roles` on a secret | `roles/secretmanager.admin` on that secret |
| `service.tags` | `roles/resourcemanager.tagUser` on the tag value, `roles/resourcemanager.tagViewer`, and `run.services.createTagBinding` (`roles/run.admin`) |
| `provider.tags` | `roles/resourcemanager.tagUser` on the tag value and on the project (`resourcemanager.projects.createTagBinding`) |
| `service.iap` | `roles/iap.admin`, `roles/run.admin` (invoker binding for the IAP agent), `roles/serviceusage.serviceUsageConsumer` (creates the IAP service agent) |
| Removing what `runway.yaml` does not list ([Removing access](configuration.md#removing-access)) | the same roles as granting (`setIamPolicy` on each resource); `roles/iap.admin` also without `service.iap` when `iap.googleapis.com` is enabled (members left after IAP was disabled are removed); with `service.identity.create`, also `resourcemanager.projects.getIamPolicy` on the deployment project (`roles/iam.securityReviewer`); for tags bound to the service, `run.services.listTagBindings` and `deleteTagBinding` (`roles/run.admin`) and `roles/resourcemanager.tagUser` on the value being removed |
| `provider.artifact_project` | the build service account needs `roles/artifactregistry.writer` on the repository (granted by `create_build_resources`, which also needs `roles/artifactregistry.admin` on that project to create the repository); the stage's Cloud Run service agent needs `roles/artifactregistry.reader` on it (runway does not grant it) |
| `stages.<name>.promote` | `run.services.get` / `run.jobs.get` on the source stage's services and jobs, and on the repositories images are copied from and to, the roles of `release.repository` below (held by `push_service_account` when set) |
| `provider.push_service_account` | the caller needs `roles/iam.serviceAccountTokenCreator` on it (unless it is the impersonated account); it needs `roles/artifactregistry.reader` on the repositories images come from and `roles/artifactregistry.writer` on those they go to. runway does not create it; with `create_build_resources` it grants it on the stage's repository reader (builds copied out to a release repository) or writer (releases tagged there, promoted images copied in), and nothing elsewhere |
| `release.repository` | the deployer: `roles/artifactregistry.reader` on the repository images are copied from and `roles/artifactregistry.writer` on the release repository; in another project, the Cloud Run service agent (`service-PROJECT_NUMBER@serverless-robot-prod.iam.gserviceaccount.com`) needs `roles/artifactregistry.reader` on it to pull (runway does not grant it) |
| Several runs at once (lease, deployed commit) | nothing extra: the lease and the commit record are annotations on the stage's service, written with `run.services.update` (in `roles/run.developer`); on a stage of jobs only, `run.jobs.update` |
| `undeploy` | also `run.jobs.list` (in `roles/run.developer`), to keep an account a job still runs as; without it the account is kept |
| `jobs` | `run.jobs.*` (in `roles/run.developer`) and `iam.serviceAccounts.actAs` on the job's runtime account |
| `schedules` | `roles/cloudscheduler.admin`, `iam.serviceAccounts.actAs` (`roles/iam.serviceAccountUser`) on the scheduler account, `run.services.setIamPolicy`/`run.jobs.setIamPolicy` on the targets (`roles/run.admin`), and with the default account `roles/iam.serviceAccountAdmin` to create it |
| `workflows` | `roles/workflows.editor`, `iam.serviceAccounts.actAs` on each workflow's account, and the grants below; see [Workflows](#workflows) |
| `domains` (load balancer modes) | `roles/compute.loadBalancerAdmin` (NEGs, backend services, URL maps, proxies, forwarding rules, addresses) and `roles/certificatemanager.owner` on the deployment project; with an existing load balancer, `compute.urlMaps.get`/`update` on its URL map |
| `domains` (domain mappings, custom URLs) | `run.domainmappings.*` (in `roles/run.admin`); domain mappings also need the domain verified in Search Console with the deployer as an owner |
| `domains.dns` | `roles/dns.admin` on the zone's project (without it, runway prints the records to create elsewhere) |
| `service.vpc` | nothing extra in the same project (the Cloud Run service agent's default role covers it); Shared VPC: the **service agent** needs `roles/compute.networkUser` on the host project or subnet (runway does not grant it) |
| `service.cloud_sql` | `roles/resourcemanager.projectIamAdmin` on each instance's project (runway grants the runtime account `roles/cloudsql.client` there) |
| `service.volumes` | nothing extra; the **runtime** service account needs access to the bucket (grant it with `service.identity.roles`) |
| impersonation | the caller needs `roles/iam.serviceAccountTokenCreator` on the impersonated account (and on each delegate); `iamcredentials.googleapis.com` must be enabled. The impersonated account then needs the deployer roles in this table |
| `undeploy` | `run.services.delete` (`roles/run.developer`); to remove the runtime identity also `roles/iam.serviceAccountAdmin` and the setIamPolicy permissions used to grant its roles; `--delete-images` needs `roles/artifactregistry.repoAdmin` |
| `provider.enable_apis` | `roles/serviceusage.serviceUsageAdmin` on the deployment project |
| `buckets` | `roles/storage.admin` on the project (`storage.buckets.create`, `update`, `setIamPolicy`) |
| `provider.create_build_resources` | `roles/artifactregistry.admin` (repository + its IAM), `roles/storage.admin`, `roles/iam.serviceAccountAdmin`, `roles/resourcemanager.projectIamAdmin` (log writer grant) |

## Workflows

| Who | Needs | Why |
|-----|-------|-----|
| Deployer | `roles/workflows.editor` on the project | create, update, list (in every region, to find leftovers) and delete workflows |
| Deployer | `workflows.workflows.list` (`roles/workflows.viewer`) | `undeploy`: keep an account a workflow runs as (also one runway.yaml no longer lists); without it, undeploy keeps every account it would have deleted, whether or not runway.yaml has workflows. With the Workflows API disabled, there are no workflows to check |
| Deployer | `iam.serviceAccounts.actAs` (`roles/iam.serviceAccountUser`) on each workflow's account | a workflow runs as it |
| Deployer | `roles/iam.serviceAccountAdmin` | with the default account (`<app>-<stage>-wf`) or `identity.create` |
| Deployer | `run.services.setIamPolicy` / `run.jobs.setIamPolicy` (`roles/run.admin`) on the services and jobs a workflow references | grant the workflow's account access to them |
| Deployer | `roles/resourcemanager.projectIamAdmin` on the project | with `invokers`, `call_log_level`, project `identity.roles`, or a schedule starting a workflow |
| Workflow account | `roles/run.invoker` on each service it references | call it (runway grants it) |
| Workflow account | `roles/run.jobsExecutorWithOverrides` and `roles/run.viewer` on each job it references | run it, with overrides, and follow the execution (runway grants them) |
| Workflow account | `roles/logging.logWriter` on the project | with `call_log_level` (runway grants it) |
| `invokers`, the scheduler account | `roles/workflows.invoker` on the project | run the workflow (runway grants it) |

Workflows has no per-workflow access, so `roles/workflows.invoker` lets its
holders run every workflow of the project. To run a workflow with
`runway run-workflow`, the caller needs `workflows.executions.create`
(`roles/workflows.invoker`).

If a workflow that runs a job fails while waiting for it with a
`run.operations.get` permission error, grant its account `roles/run.viewer`
on the project with `identity.roles`. runway grants it on the job only, and
this has not been checked against every way of calling the Cloud Run
connector.
