# Configuration

```yaml
version: 1                    # schema version (required, only 1)
app: hello-api                # lowercase letters, digits, hyphens; up to 40 chars

vars:                         # optional user variables: ${vars.NAME}
  data_project: my-data-project

provider:
  project: my-gcp-project     # project ID (required)
  region: europe-west1        # Cloud Run and Cloud Build region (required)
  enable_apis: true           # enable missing APIs (default false)
  apis: [telemetry.googleapis.com]              # extra APIs runway cannot infer
  create_build_resources: true                  # create repo, source bucket, build SA + roles (default false)
  impersonate_service_account: deployer@my-gcp-project.iam.gserviceaccount.com   # optional; the CLI flag wins
  tags:                       # optional tags bound to the PROJECT before anything is created,
    "123456789012/allowIngressAllForCloudRun": allow-ingress-all   # then awaited until effective
  # The three names below are required for source builds, unless
  # create_build_resources is true: then they default to `runway`,
  # `${project}-runway-sources` and `runway-build@${project}.iam.gserviceaccount.com`.
  artifact_repository: applications             # source builds
  artifact_location: europe-west1               # optional, defaults to region
  source_bucket: "${project}-runway-sources"   # source builds, name without gs://
  build_service_account: "builds@${project}.iam.gserviceaccount.com"   # source builds

buckets:                      # created and kept configured by runway: ${buckets.KEY}
  # An existing bucket is only updated if runway created it for this app
  # (labels managed-by=runway, runway-app=<app>); any other existing bucket
  # is used as is. Add those labels to let runway manage a bucket you created.
  cache:
    name: "${project}-hello-cache"   # globally unique
    location: europe-west1    # default: provider.region
    storage_class: STANDARD   # optional
    versioning: false         # optional
    delete_after_days: 30     # optional lifecycle rule
    labels: { team: web }     # optional (runway adds managed-by/runway-app)

secrets:                      # created by runway WITHOUT a value: ${secrets.KEY}
  api-key:
    name: "${app}-${stage}-api-key"   # secret ID, default: the key
    adders: [group:devops@example.com]  # get roles/secretmanager.secretVersionAdder
    # locations: [europe-west1]       # default [provider.region]; [] = automatic replication
    # labels: { team: web }

service:
  source: .                   # build context, relative to runway.yaml
  dockerfile: Dockerfile      # relative to source; default: `Dockerfile` if present, else buildpacks
  # builder: gcr.io/buildpacks/builder:latest   # force buildpacks with this builder
  rebuild: on-change          # default: rebuild when the source or a base image changes; `always` = every deploy
  # image: europe-west1-docker.pkg.dev/my-gcp-project/applications/hello:1.2.3   # instead of source
  # command: [gunicorn]       # entrypoint (default: the image's)
  # args: [--workers, "2"]    # its arguments
  port: 8080                  # default 8080; injected as $PORT
  cpu: "1"                    # 1, 2, 4, 6, 8 or 0.08-1 (also "500m"; below 1 needs concurrency 1); default 1
  memory: 512Mi               # 128Mi-32Gi; default 512Mi
  timeout_seconds: 60         # 1-3600; default 300
  concurrency: 80             # 1-1000; default 80
  min_instances: 0            # default 0
  max_instances: 10           # default 10
  public: false               # default false (private)
  ingress: all                # all (default) | internal | internal-and-cloud-load-balancing
  billing: request-based      # request-based (default) | instance-based (CPU always allocated)
  startup_cpu_boost: false    # default false: more CPU while instances start
  # execution_environment: gen2   # gen1 | gen2; default: Cloud Run chooses
  sandbox: false              # default false: Cloud Run sandboxes for untrusted code (preview, gen2)
  bootstrap: {}               # first deploy only: restricted ingress until `tags` are effective
    # ingress: internal       # default: ingress used until the tags are effective
    # image: us-docker.pkg.dev/cloudrun/container/hello   # optional placeholder instead of the real app
  otel_collector:             # optional Google-built OpenTelemetry Collector sidecar
    # version: "0.160.0"      # default: the newest release, looked up at plan/deploy time
    # cpu: "1"                # default 1
    # memory: 512Mi           # default 512Mi
    # config: |               # default: OTLP on localhost:4317/4318 -> Cloud Trace,
    #   ...                   #   Cloud Logging, Managed Service for Prometheus
  sidecars:                   # extra containers next to the app (container name: settings)
    sql-proxy:
      image: gcr.io/cloud-sql-connectors/cloud-sql-proxy:2.14.0
      args: ["--port=5432", "my-project:europe-west1:db"]
      cpu: "0.5"              # default 1
      memory: 256Mi           # default 512Mi
      # command: [...]        # replaces the image entrypoint
      # env: { LOG_LEVEL: info }
      # secrets: { TOKEN: { secret: proxy-token } }   # env vars only; accessor granted
      health_check: { port: 5432 }    # TCP check; add `path: /ready` for HTTP
      # start_before_app: true        # default: the app starts once this check passes
      # volumes: { cache: /cache }    # mount service volumes (name: path)
  health_check:               # optional HTTP probes (default: Cloud Run's TCP startup probe)
    path: /healthz
    startup:                  # defaults: every 10s, timeout 3s, 12 failures, no delay (values up to 240s)
      period_seconds: 10
    liveness: true            # default true (every 30s, timeout 3s, 3 failures); false or a map like startup
  service_account: runtime@my-gcp-project.iam.gserviceaccount.com   # required
  env:
    LOG_LEVEL: info
  secrets:                    # the runtime account gets secretAccessor on each one
    DATABASE_URL:
      secret: database-url    # or projects/<project>/secrets/<id>
      version: "1"            # a number, or "latest"; omitted: newest version pinned at deploy
    API_KEY:
      secret: "${secrets.api-key}"      # a secret runway creates (see `secrets:` above)
    tls:                      # mounted as a file instead of an environment variable
      secret: tls-cert
      path: /secrets/tls/cert.pem       # one secret per directory; default version: latest (read live)

  identity:                   # optional: manage the runtime service account
    create: true              # create service_account if missing
    display_name: hello runtime
    roles:                    # granted to service_account; exactly one target each
      - role: roles/bigquery.dataViewer
        dataset: my-data-project.my_dataset     # or PROJECT:DATASET
      - role: roles/bigquery.jobUser
        project: my-data-project
      - role: roles/storage.objectUser
        bucket: "${buckets.cache}"
      - role: roles/secretmanager.secretAccessor
        secret: database-url  # or projects/<project>/secrets/<id>

  tags:                       # Resource Manager tags bound to the service
    "123456789012/allow-public-access": "true"  # ORG_OR_PROJECT_ID/key: value

  volumes:                    # Cloud Storage mounts (the runtime account gets
    cache:                    # objectViewer if read_only, else objectUser, on the bucket)
      bucket: "${buckets.cache}"        # or any bucket name
      mount_path: /mnt/cache
      read_only: false        # default false
      mount_options: [implicit-dirs]

  iap:                        # Identity-Aware Proxy
    enabled: true             # default true when the block is present
    members: [group:finops@example.com]   # get roles/iap.httpsResourceAccessor

  vpc:                        # Direct VPC egress (no connector)
    network: default          # or projects/HOST_PROJECT/global/networks/NAME (Shared VPC)
    subnet: default           # or projects/HOST_PROJECT/regions/REGION/subnetworks/NAME
    egress: private-ranges-only   # default; or all-traffic
    network_tags: [run-egress]    # optional, for firewall rules

  cloud_sql: [db]             # PROJECT:REGION:INSTANCE, or an instance in this project and region;
                              # socket at /cloudsql/PROJECT:REGION:INSTANCE
  custom_audiences: [https://api.example.com]   # extra ID token audiences (service-level)
  domains: [shop.example.com, example.com/api, my-shop.cloud.run]   # see "Custom domains" below
  preview_domain: "*.preview.example.com"   # previews on your domain (load balancer modes)

# More services, Cloud Run jobs and Cloud Scheduler jobs: see
# "Several services, jobs and schedules" below.
# defaults: {}                # what every service and job inherits
# services: {web: {...}}      # <app>-<name>-<stage>
# jobs: {migrate: {...}}      # <app>-<name>-<stage>
# schedules: {nightly: {schedule: "0 3 * * *", job: migrate}}

# release:                    # `deploy --tag` / `--tag-rc`: see "Releases" below
#   repository: {project: my-release-project, repository: releases}

retry:                        # per-step retries for deploy (top level only)
  attempts: 3                 # total attempts per step, 1-20; default 3
  delay: 5s                   # initial delay, doubles each attempt; default 5s
  max_delay: 60s              # cap; default 60s

stages:                       # at least one; --stage must name one of these
  dev:
    service:
      max_instances: 2
  prod:
    provider:
      project: my-prod-project
    service:
      min_instances: 1
      env:
        LOG_LEVEL: warning
```

## Sidecars

`service.sidecars` adds containers that run next to the application in every
instance (Cloud Run allows 10 containers in total, including the app and the
OpenTelemetry Collector). Typical uses: a database proxy, a local cache, an
authentication or TLS proxy, a log forwarder.

- Containers share the network: the app reaches a sidecar on `localhost`.
  Only the app receives requests; a sidecar's `health_check.port` must
  differ from `service.port`.
- With `start_before_app` (the default), the app container is named `app` and
  Cloud Run starts it after the sidecar's startup check passes. Without a
  `health_check`, the sidecar counts as started as soon as it runs.
- `secrets` are exposed as environment variables (pinned to the newest
  version at deploy time when `version` is omitted) and the runtime service
  account is granted `roles/secretmanager.secretAccessor` on them.
- `volumes` mounts volumes declared under `service.volumes` (the app mounts
  them too, at their `mount_path`).
- In a stage, `sidecars: { NAME: null }` removes an inherited sidecar; a map
  replaces it entirely.
- Plans show each sidecar on one line (`sidecars.NAME`) with its image, CPU,
  memory and a fingerprint of the rest of its settings.

CPU is allocated per container: with request-based billing, sidecars only get
CPU while requests are being served.

## CPU, sandboxes, networking and Cloud SQL

**Billing.** `billing: request-based` (the default) allocates CPU only while
requests are handled; `instance-based` allocates it for the instance's whole
life (background work, OpenTelemetry exporters) and is billed that way. It
needs at least 1 CPU and 512Mi. runway sets it on every container.
`startup_cpu_boost: true` gives more CPU while instances start and for 10
seconds after (billed). Less than 1 CPU needs `concurrency: 1`,
request-based billing and gen1.

**Execution environment.** Without `execution_environment`, Cloud Run
chooses. `gen2` (full Linux compatibility, faster CPU and network, slower cold
starts) needs at least 512Mi; `gen1` (faster cold starts) cannot mount Cloud
Storage volumes.

**Sandboxes (preview).** `sandbox: true` lets the app run untrusted code (code
written by an AI agent, user scripts, a headless browser) in isolated
sandboxes started with the `sandbox` command
(`/usr/local/gcp/bin/sandbox`), for example
`sandbox do -- /usr/bin/python3 script.py`. It is a single switch on the app
container; everything else is chosen per sandbox, in the app's code:

- network: none by default, `--allow-egress` to allow outbound traffic;
- files: a read-only view of the container, `--write` for a temporary
  overlay, `--mount type=bind,source=…,destination=…[,readonly]` to share a
  directory, `--import-tar`, `--export-tar` and `--sync-tar` to keep files
  between runs;
- environment: nothing is inherited (no env vars, secrets or metadata
  server); `--env NAME=value` passes variables;
- lifetime: `sandbox do` runs one command; `sandbox run NAME --detach`,
  `sandbox exec NAME`, `sandbox tar NAME` and `sandbox delete NAME` manage a
  long-lived one.

Sandboxes share the container's CPU and memory (size them for both; no extra
charge) and run on gen2: runway sets `execution_environment: gen2` unless you
set it, rejects `gen1`, and so needs at least 1 CPU and 512Mi. Since the
feature is in preview, runway sets the service's launch stage to `BETA` while
it is enabled. See [Code execution in Cloud
Run](https://docs.cloud.google.com/run/docs/code-execution).

**Direct VPC egress.** `vpc` connects the service to a VPC network without a
connector. runway needs both `network` and `subnet`. A name alone is in the
deployment project; in a Shared VPC, use full resource names in the host
project. runway sends and compares full names, so moving to another host
project is a change, even with the same network name. The
subnet must be in the service region and is `/26` or larger in practice
(Cloud Run uses about two addresses per instance). runway enables
`compute.googleapis.com`. In a Shared VPC, the Cloud Run service agent
(`service-PROJECT_NUMBER@serverless-robot-prod.iam.gserviceaccount.com`)
needs `roles/compute.networkUser` on the host project or subnet; runway does
not grant it. `egress: all-traffic` sends everything through the VPC (a Cloud
NAT is then needed for the internet).

**Cloud SQL.** Each instance in `cloud_sql` is reachable through a Unix socket
at `/cloudsql/PROJECT:REGION:INSTANCE` (the Cloud SQL Auth Proxy built into
Cloud Run, public IP path). runway grants the runtime account
`roles/cloudsql.client` in each instance's project (it allows connecting to
every instance of that project) and enables `sqladmin.googleapis.com` in the
deployment project; for an instance in another project, the Cloud SQL Admin
API must be enabled there too. On gen1, only instances using the per-instance
CA work. For private IP, use `vpc` and connect to the instance's address
instead.

**Custom audiences.** ID tokens whose audience is one of `custom_audiences`
are accepted, besides the `run.app` URL (for a custom domain or a load
balancer). This is a service setting: changing it creates no revision.
runway owns it: audiences set outside runway are removed.

## Custom domains

Domains belong to services; the stage-wide `domains` block says how they are
served (a stage's block replaces the global one). See
[Custom domains](domains.md) for the guide.

```yaml
domains:
  mode: load-balancer         # load-balancer (default) | existing-load-balancer | domain-mapping
  dns:                        # optional: runway writes the records in this Cloud DNS zone
    zone: example-com         # the managed zone's name, not its DNS name
    project: my-dns-project   # default: provider.project
  load_balancer:              # existing-load-balancer only
    url_map: shared-lb        # required; in the deployment project
    certificate_map: shared-certs   # optional: runway adds its certificates to it
    address: 203.0.113.10     # optional: the IP the hosts' A records point to

service:
  domains:
    - shop.example.com        # a whole host
    - example.com/api         # a path prefix: /api and /api/* (load balancer modes)
    - my-shop.cloud.run       # a Cloud Run custom URL (any mode; 6 to 63 characters)
  preview_domain: "*.preview.example.com"   # <tag>.preview.example.com for each preview
```

| Rule | Why |
|---|---|
| A host or host and path belongs to one service | the load balancer routes it to one backend |
| `domain-mapping` needs a [supported region](https://cloud.google.com/run/docs/mapping-custom-domains#limitations), whole hosts, no `preview_domain` | domain mappings map a host to a service |
| `load_balancer` only with `mode: existing-load-balancer`, which requires it | it names the load balancer runway adds routes to |
| Jobs have no domains; a `domains` default applies to services only | jobs have no URL |

Domains change with a full deploy (not a preview or canary); removing one is
done by a full deploy of everything, or by `undeploy`.

## Several services, jobs and schedules

A file can deploy more than the main `service`: other services, Cloud Run
jobs, and Cloud Scheduler jobs that run a job or call a service (the
[Services, jobs and schedules](services-and-jobs.md) guide shows how). Files with
only `service:` work as before: the same service name, labels, images and
plans.

```yaml
defaults:                     # every service and job inherits these
  service_account: shop-runtime@my-gcp-project.iam.gserviceaccount.com
  identity: { create: true }
  env: { LOG_LEVEL: info }

service:                      # shop-<stage>, as before
  source: .
  public: true

services:
  web:                        # shop-web-<stage>
    source: apps/web          # its own folder, Dockerfile and image
    memory: 1Gi

jobs:
  migrate:                    # shop-migrate-<stage>
    source: .                 # same build as `service`: built once
    command: [python, manage.py, migrate]
    tasks: 1                  # tasks per execution (default 1)
    parallelism: 0            # at once (default 0: as many as possible)
    max_retries: 1            # per task (default 3)
    timeout_seconds: 1800     # per task (default 600, up to 168 hours)
    # cpu and memory: at least 1 CPU and 512Mi for a job

schedules:
  nightly:
    schedule: "0 3 * * *"     # unix cron
    time_zone: Europe/Paris   # default Etc/UTC
    job: migrate              # run a job...
  warm:
    schedule: "*/10 * * * *"
    service: web              # ...or call a service (the main one is named after the app)
    path: /tasks/warm         # default /
    method: POST              # default POST; also body, headers
    # retries: 2              # 0-5, default 0
    # attempt_deadline_seconds: 300   # 15-1800, default 180
    # paused: true

scheduler:                    # optional
  service_account: ...        # default: <app>-<stage>-sched@PROJECT, created by runway
  region: europe-west1        # default: provider.region

stages:
  prod:
    defaults: { env: { LOG_LEVEL: warning } }
    services: { web: { min_instances: 1 } }
    jobs: { migrate: { tasks: 2 } }
  dev:
    services: { web: null }   # not in this stage
    schedules: { warm: null }
```

**Names.** The main service is `<app>-<stage>`; the others and jobs are
`<app>-<name>-<stage>` (services up to 49 characters, jobs up to 63). Names
are lowercase letters, digits and hyphens; a service and a job cannot share
one, and none can be the app name (it names the main service). Named
services and jobs carry a `runway-name` label; the main service does not, so
services deployed before named ones existed are unchanged.

**Precedence.** Lowest first: `defaults`, `stages.S.defaults`, the service or
job, `stages.S.<its block>`. Maps (`env`, `secrets`, `volumes`, …) merge key
by key and `null` removes an inherited key. Jobs take from `defaults` only
what jobs have: build or image, `command`/`args`, CPU and memory, `env`,
`secrets`, `volumes`, `service_account` and `identity`, `vpc`, `cloud_sql`,
`execution_environment` and `sandbox` (not `timeout_seconds`: a request
timeout is not a task timeout). A stage schedule replaces the inherited one.

**Images.** Each build has its own package: `<app>` for the main service (as
before), `<app>-<name>` otherwise. Services and jobs with the same build
(same folder, Dockerfile or builder) share one image, built once, in the
package of the first of them in the file.

**Identity.** Put `service_account` and `identity` in `defaults` for one
runtime account, or set them on a service or job for its own. An account
used by several is created once and gets every role they need.

**Schedules.** A job is run through the Cloud Run Admin API with an OAuth
token; a service is called at its URL plus `path`, with an ID token for its
URL. Both use one account per app and stage (`scheduler.service_account`),
which gets `roles/run.invoker` on each target only, and runway enables
`cloudscheduler.googleapis.com`. Scheduler jobs are named
`<app>-<name>-<stage>` and marked as runway's in their description (they have
no labels). A schedule removed from runway.yaml is deleted by the next full
deploy, and its grant revoked.

**Selecting.** `--only` (on `plan`, `deploy`, `describe`, `doctor`,
`undeploy`, `info`, `logs`, `traffic`, `preview`) takes names, or paths: a
path selects what is built from inside it (`--only apps` for every app in
`apps/`), or else what is built from the most specific folder containing it
(`--only apps/web/src/main.py` selects `web`, not a service built from the
root). In a monorepo, CI can deploy only the app a change touched.
`--image` needs `--only` with one name when there are several services and
jobs.

**Previews and canaries.** `--preview NAME` gives every service a tagged URL
(one tag for all) and deploys each job as a copy, `<job>-<tag>`, never
scheduled; `preview delete` and `preview prune` delete these copies with
their preview. `--traffic N` canaries every service and leaves jobs and
schedules unchanged. Jobs and schedules change with a full deploy.

**Removals.** Revocations and the removal of unlisted access (see below)
happen after a full deploy of every service and job (no `--only`), once every
service serves one revision. runway records its grants on one workload: the
main service, else the first service, else the first job; if runway does not
own it (for example an unmanaged main service while `--only` deploys a job),
nothing is recorded or read there. A service or job removed from
runway.yaml is not deleted by deploy, which lists it; remove it with
`runway undeploy --stage S --orphans`. Removing the last schedule works too:
the next full deploy deletes it (in `scheduler.region` if it is still set).

**Undeploy.** Services and jobs go first, then runtime accounts, then
images, so an account shared by two services is only deleted once neither
runs, after the roles each of them declared are revoked. An account is kept while any live service or job (listed in
runway.yaml or not) runs as it. With `--only`, a schedule calling what is
removed is deleted only if runway created it.

## Releases

`deploy --tag` and `--tag-rc` tag the deployed image with the changelog
version. The `release` block chooses where released images are published and
which stage each flag deploys; with a stage mapped to `tag-rc`, `--tag`
releases the latest release candidate without rebuilding. See
[Releases](releases.md) for the flow.

```yaml
release:                      # global (optional)
  repository:                 # where released images are published (copied, same digest)
    project: my-release-project   # default: provider.project
    location: europe-west1        # default: the build repository's location
    repository: releases

stages:
  staging:
    release:
      flag: tag-rc            # `runway deploy --tag-rc` deploys this stage, and no other
  prod:
    release:
      flag: tag               # `runway deploy --tag` deploys this stage, and no other
      from: staging           # optional: the tag-rc stage whose candidates it releases
      repository: {project: my-prod-project, repository: releases}   # replaces the global one
```

Without a `release` block, `--stage` is required and the image is tagged in
the build repository.

## Removing access

`runway.yaml` is the complete list of access on what runway owns: anything
else found there is removed. Elsewhere, runway only removes what it granted
itself.

**Authoritative: what is not listed is removed.** On these, an entry
`runway.yaml` does not list is removed, whoever added it (by hand, another
tool, an earlier deploy):

| What | Removed | Only when |
|---|---|---|
| IAP access | members of `roles/iap.httpsResourceAccessor` on the service's IAP resource not in `iap.members` (all of them while `iap` is disabled, whoever added them) | always (the service is runway's). Without `iap` and with `iap.googleapis.com` disabled there is nothing to check: no IAP access is in effect |
| Service tags | tags bound **directly** to the service not in `service.tags` | always. Tags inherited from the project, folder or organization are not bindings on the service and stay |
| Secret adders | members of `roles/secretmanager.secretVersionAdder` not in `adders` | the secret carries runway's labels for this app and stage (runway created it) |
| Runtime roles | roles of the runtime account that runway does not grant (`identity.roles`, and those implied by secrets and volumes) | runway created the account for this service. The resources checked are the deployment project and every resource `runway.yaml` grants on, now or in an earlier deploy |

Only unconditional bindings are considered: conditional bindings and other
roles on the same resources are never touched. Roles granted to the runtime
account on resources runway has never granted on (a bucket `runway.yaml`
never referenced, say) cannot be found without Cloud Asset Inventory and stay.

!!! warning
    A role or IAP member added by hand to what runway owns is removed by the
    next main deploy. Add it to `runway.yaml` instead. `runway plan` lists
    every removal with `-` first, for example
    `- revoke roles/editor on project my-gcp-project from gcptree-run`.

**Provenance: only what runway granted is removed.** Elsewhere (a runtime
account runway did not create, a secret it did not create), runway removes
only the grants it added itself. It records them on the service (annotation
`runway.dev/grants`, no state file) as soon as they are made:

- the runtime account's roles, both `identity.roles` and those implied by
  secrets and volumes;
- secret `adders`;
- `iap.members`.

A configured grant that was already in place when runway checked it is never
recorded: someone else granted it.

The record is saved even when the deploy fails later, for example on an
empty secret, a failed build, a refused rollout or a post-rollout step. A
write whose answer was lost (timeout, 5xx) counts as runway's if a later
read shows it: runway reads once more before failing, so this holds even
when the deploy stops right there.

Two exceptions leave grants unrecorded; they then look pre-existing to the
next deploy and are not revoked in provenance mode:

- a first deploy that fails before the service exists has nowhere to record;
- a lost write whose confirming read fails too.

**When removals happen.** After the rollout of a **main deploy** (no
`--preview`, no `--traffic`), once a single revision serves all of the
traffic. Until then, nothing is removed:

- previews and canaries never remove access or tags;
- neither does a deploy after which other revisions still serve part of the
  traffic, since they may still need the access.

A recorded grant stays recorded and is revoked by the next main deploy;
`runway plan` for a main deploy lists each removal with `-`. Preview URLs (no
traffic) of older revisions may lose that access. A removal that fails is
retried by the next deploy. IAP, tags, adders and runtime roles are checked
separately: if runway cannot read one of them, the deploy warns, removes
nothing there, and still removes the others (`plan` marks it unchecked). Adding an IAP member or an adder shows just that
member, for example `+ IAP access (grant to group:new@example.com)`.

**What is never removed:**

- Project tags, and any access on resources runway does not own: the project
  policy for other accounts, shared buckets and datasets for other members.
- In provenance mode: access runway did not grant (given by hand or by
  another tool, even if also in `runway.yaml`), and roles of a runtime account
  runway did not create for this service: another service may share that
  account and need the role. `plan` says so.
- The shared build account's roles.

## Stage override precedence

From highest to lowest:

1. Command-line overrides: `--image` on `plan`/`deploy`.
2. `stages.<stage>.provider` / `stages.<stage>.service`.
3. Top-level `provider` / `service`.
4. Built-in defaults (listed above).

Scalars are replaced. `env`, `secrets`, `tags`, `volumes` and `vars` are
merged key by key; a stage can remove an inherited key by setting it to `null`. The
`identity` and `iap` blocks are replaced as a whole by a stage that sets them. The deployment mode is chosen
by the highest layer that sets `image` or `source`/`dockerfile`: a stage that
sets `image` replaces a top-level source build (and vice versa). Setting both
in the same block is an error.

## Variables

`${...}` is replaced in `provider.source_bucket`,
`provider.build_service_account`, bucket names, `service.service_account`,
`env` values, secret names, `identity` (targets and display name), volume
buckets and IAP members. Available: `${project}`, `${region}`, `${app}`,
`${stage}`, `${vars.NAME}` (top-level `vars`, overridden by
`stages.<stage>.vars`; variables may use the built-ins) and `${buckets.KEY}`
(the resolved name of a declared bucket). `$$` is a literal `$`. Unknown
variables are validation errors.

## Validation

`runway validate` checks every stage (or `--stage`) without credentials:
unknown fields (with line/column and the list of valid fields), duplicate
keys, types, value ranges, CPU/memory combinations, name formats (project,
region, repository, bucket, service accounts, secrets, env names), reserved
variables (`PORT`, `K_SERVICE`, `K_REVISION`, `K_CONFIGURATION`), env/secret
name clashes, image references, `min_instances <= max_instances`, the
49-character service-name limit, the presence of the build context and
Dockerfile, and the provider fields required for source builds. It warns
about mutable image tags, `latest` secret versions, credential-looking plain
env vars and billed warm instances.

Secret values never appear in `runway.yaml`, in runway's output or in the
resources it writes: only `secret@version` references are sent to Cloud Run.
