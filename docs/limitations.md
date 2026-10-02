# Limitations and status

runway is in **beta** (0.x). The configuration format may still change in
minor releases; changes are listed in the [changelog](https://github.com/OWNER/runway/blob/main/CHANGELOG.md).

## Limitations

**Scope**

- One Cloud Run HTTP service per configuration (one copy per stage). No
  Cloud Run jobs or worker pools, no VPC access, no custom domains or load
  balancers, no built-in Cloud SQL connections (a Cloud SQL Auth Proxy
  sidecar works). See the [roadmap](roadmap.md).
- Not supported yet on the service: custom audiences, always-on CPU
  (instance-based billing), execution environment selection.
- runway owns the revision template: settings added outside runway
  (extra containers, volumes, VPC settings, template labels or annotations)
  are removed by the next deploy. The same applies to a service taken over
  with `--adopt`.
- Not created by runway: the project, billing, tag keys and values, quotas,
  budgets and the deployer's own roles. APIs, buckets, secrets, the repository
  and service accounts are created only when the configuration asks for them
  (see [Getting started](getting-started.md#prerequisites)).

**Access**

- Public access uses the `allUsers` invoker binding; organizations enforcing
  Domain Restricted Sharing must keep services private (or use IAP).
- IAP uses Cloud Run's direct integration (no load balancer) with the
  Google-managed OAuth client, which serves users of your organization.
- Organizations restricting ingress to `internal` /
  `internal-and-cloud-load-balancing` without a tag exception block internet
  traffic to the `run.app` URL; a load balancer is then needed, which runway
  does not create. With a tag-based exception, use `service.tags` and
  `service.bootstrap` (see [How it works](how-it-works.md)).

**No state file, so additive grants**

- Grants, tag bindings and IAP members are additive: runway cannot tell which
  members it added in the past, so removing an entry from the configuration
  does not revoke it. Revoke manually (`gcloud ... remove-iam-policy-binding`).
- Drift is detected and corrected by the next `plan` / `deploy`, not
  continuously.
- `deploy` never deletes anything. `undeploy` deletes only the service, the
  runtime service account runway created for that app and stage (marker in
  its description), its grants, and optionally the app's images. Buckets and
  secrets are always kept. Bucket locations cannot change.

**Builds and images**

- The default buildpacks builder (`gcr.io/buildpacks/builder:latest`) is a
  moving tag; pin `service.builder` for reproducible builds. Build-time
  environment variables for buildpacks are not configurable yet.
- Builds run in the service region on the default worker pool; machine types
  and private pools are not configurable yet.
- Grants made for `create_build_resources` assume the build service account
  lives in the deployment project.
- Images in third-party registries other than Docker Hub must be reachable by
  Cloud Run (typically through an Artifact Registry remote repository).
- `.dockerignore` handling follows Docker's rules for the common pattern
  forms (anchoring, last match wins, exceptions inside excluded directories);
  rare edge cases of Docker's matcher may differ.
- The compressed build context is written to the system temporary directory
  before upload; keep contexts small with `.runwayignore` / `.dockerignore`.

**Other**

- Volumes: Cloud Storage and secret files only. Each secret file needs its
  own directory.
- `logs --follow` polls every 2 seconds and fetches at most 1,000 new entries
  per poll.
- Plain `env` values are shown in plans and output; anything sensitive belongs
  in `secrets`.
- Developed and tested on Linux and macOS (builds). Windows is untested.
- The binary is about 35 MB because it links several Google Cloud SDK crates.

## What has been verified against Google Cloud

The automated test suite is offline: SDK stubs and a mock HTTP server stand in
for Google's APIs (request paths, bodies and error handling are checked at
the wire level). On top of that, runway has been used to deploy a real
application to a real project, partly through an impersonated service
account.

**Seen working live**

- API enablement; bucket creation and configuration (Storage control, gRPC);
  Artifact Registry repository creation; service account creation (and
  deletion by `undeploy`); grants on projects, buckets, repositories and
  BigQuery datasets.
- Source builds with buildpacks on regional Cloud Build, including the upload
  of the parallel-gzip archive, in-flight build detection and digest lookup
  of an existing image (skipping the rebuild).
- Service creation and updates (update mask, enum encoding, etags), readiness
  detection, plans converging to "no changes" after a deploy.
- IAP (service agent, invoker binding, accessors), service tags on the
  regional Resource Manager endpoint awaited until effective, and the
  `bootstrap` first deploy accepted by a `constraints/run.allowedIngress`
  policy that reads a tag bound to the service only.
- `undeploy`, `info`, `plan`, `doctor`, impersonation.

**Implemented and tested offline, not yet verified live**

- Previews, canaries and `runway traffic` (tag URLs, pinning, promote).
- Secrets: creation, adders, the stop-until-a-value step, version pinning,
  secret files; grants implied by secrets and volumes.
- Generic sidecars (`service.sidecars`), including Cloud Run's handling of
  their startup checks and start order.
- The OpenTelemetry Collector sidecar (its default configuration was
  validated with the real `otelcol-google` binary).
- Cloud Storage volume mounts, image deletion (`undeploy --delete-images`),
  release tags (`--tag`, `--tag-rc`).
- Docker Hub digest resolution (anonymous token flow), Workload Identity
  Federation credentials (standard ADC `external_account` handling), the CI
  snippets in these docs.
- Local CPU/memory validation rules; Cloud Run's server-side validation is
  authoritative (rejections surface as exit code 6).
- `doctor` permission names and the principal lookup through `tokeninfo`
  (some credential types report no email).
- The build log excerpt on failure (log ingestion delay may leave it empty;
  the log URL is always printed).

If you use one of these, please report what you see (an issue is welcome,
including "it worked").

See [Architecture](architecture.md) for the design.
