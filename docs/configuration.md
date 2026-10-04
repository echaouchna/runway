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
  port: 8080                  # default 8080; injected as $PORT
  cpu: "1"                    # 1, 2, 4, 6, 8 or 0.08-1 (also "500m"); default 1
  memory: 512Mi               # 128Mi-32Gi; default 512Mi
  timeout_seconds: 60         # 1-3600; default 300
  concurrency: 80             # 1-1000; default 80
  min_instances: 0            # default 0
  max_instances: 10           # default 10
  public: false               # default false (private)
  ingress: all                # all (default) | internal | internal-and-cloud-load-balancing
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
