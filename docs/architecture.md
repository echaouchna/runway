# Architecture

runway is a single Rust binary. It turns a declarative `runway.yaml` into one
Cloud Run service per stage, plus the resources that service needs, by
calling Google Cloud APIs directly with Application Default Credentials.

There is **no state backend**: the live project is the source of truth and
every command re-reads it.

## Data flow

```text
runway.yaml ─► config::load ─► config::resolve(stage, CLI overrides) ─► Deployment
               (strict serde)   (stage merge, ${…} interpolation,        (resolved and valid)
                                 defaults, validation)

Deployment ─► provision::{api_step, pre_steps, post_steps} ─► Step list (check / apply)
           ─► plan::ServiceSpec::from_deployment ─► desired state
                                                      │
live Service (Cloud Run v2) ─► gcp::run::observed_flat ┴► plan::diff ─► Plan (text / JSON)
                            └► gcp::run::current_traffic ─► traffic::plan ─► traffic split

deploy:
  APIs ║ registry login ─► inspect (service + source hash + image lookup, concurrently)
       ─► waves (provision::waves, Provisioner::apply_all):
            project tags ─► buckets ║ secrets ║ repository ║ accounts
            ─► grants (one lane per IAM policy) ─► secret values present?
          the build starts once its own prerequisites exist and runs ║ the rest
       ─► service tags ─► create/update (update mask, etag, template reuse)
       ─► wait until ready ─► tags ║ IAP ─► public/private access
  (║ = concurrently)
```

## Modules

| Module | Responsibility |
|---|---|
| `cli`, `output`, `style` | clap definitions, dispatch, text/JSON rendering, colors, exit codes |
| `config` | Schema (`deny_unknown_fields`), stage merge, `${…}` interpolation (`interp`), defaults, validation with precise paths |
| `naming` | Deterministic names: service, image, labels, source objects, build tags, IAM resource names |
| `plan` | Desired state (`ServiceSpec`), normalized flat representation, diff, plan rendering |
| `traffic` | Pure traffic logic: full rollouts, previews, canaries, promote, splits, tag names |
| `build` | Deterministic source scan and hash, parallel compression (`package`), base-image digests (`inputs`), Cloud Build (`cloudbuild`), release tags (`release`) |
| `gcp` | Client construction (ADC, impersonation), Cloud Run request construction and observation (`run`), registry digest adapter, IAM policy edits, buckets, logs, error classification |
| `deploy` | Reconciliation of the service: ownership, create/update with ambiguity handling, readiness, invoker IAM |
| `provision` | Stateless steps with a read-only `check` (plan) and an idempotent `apply` (deploy): APIs, project and service tags, buckets, secrets, repository, service accounts, grants, secret values, IAP; removal of access and tags `runway.yaml` does not list on what runway owns (`unlisted`) and of grants it recorded elsewhere |
| `retry`, `poll` | Per-step retries with backoff and error classification; bounded polling |
| `describe` | Offline ASCII/Mermaid diagram and explanation of a stack |
| `commands` | One module per CLI command |

## Design decisions

- **Official SDKs first.** Cloud Run, Cloud Build, Cloud Storage, Logging,
  Resource Manager, IAM, Secret Manager, Service Usage, Artifact Registry,
  BigQuery and IAP use the official `google-cloud-*` Rust crates. Small REST
  adapters exist only where no SDK call exists: the OCI registry v2 protocol
  (digest lookup, tag listing) and Service Usage `v1beta1
  generateServiceIdentity`.
- **No state file.** Ownership is encoded in labels (`managed-by=runway`,
  `runway-app`, `runway-stage`) and in a marker in the description of service
  accounts runway creates. runway refuses to modify what it does not own.
- **Steps, not state.** Provisioning is a list of steps; each re-reads before
  writing, so partial failures resume naturally. Grants, tags and IAP members
  are additive because removals would need a record of what was added.
- **Template reuse.** When only service-level fields change (traffic,
  ingress, IAP), the live revision template is sent back untouched, so Cloud
  Run creates no revision. Previews and canaries get a marker annotation so
  they always run in a revision of their own.
- **Traffic without state.** The split is derived from the live traffic, the
  latest ready revision and the requested mode; entries following `latest`
  are pinned before a preview or canary is added, so a branch can never take
  production traffic.
- **Content-addressed builds.** The tar stream is deterministic (sorted
  entries, fixed metadata); its SHA-256, combined with the digests of the base
  images, names the uploaded object, the image tag and a Cloud Build tag.
  Unchanged inputs are not rebuilt; an in-flight build of the same inputs is
  attached to, not duplicated.
- **Concurrent independent work.** Plans, deploys, `info`, `doctor` and the
  readiness poll issue independent requests concurrently, so latency is
  bounded by the slowest call. Provisioning runs in dependency waves
  (`provision::waves`). Writes to one IAM policy are serialized in a lane;
  different policies run together. A source build overlaps the provisioning
  it does not depend on. Results are reported in step order, and a failure
  stops before the next wave.
- **Bounded waits, ambiguity handled.** Every poll has a deadline and honours
  Ctrl-C. Mutations with an ambiguous outcome (timeouts, transport errors,
  5xx) are followed by a read before any retry.
- **Retries that understand Google Cloud.** IAM propagation, freshly enabled
  APIs and freshly bound tags are retried with backoff; configuration errors,
  conflicts and "a person must act" situations (an empty secret) fail fast.
- **Explicit uncertainty.** A plan computed before a build marks the image as
  pending and the plan as not exact.
- **Testability.** Clients are built with `from_stub` or pointed at a mock
  HTTP server in tests, so request construction, error handling and
  reconciliation are checked against the real SDK request types offline.
