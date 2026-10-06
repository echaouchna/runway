# Limitations and status

For 0.x versions, the configuration format may change in minor releases;
changes are listed in the [changelog](https://github.com/echaouchna/runway/blob/main/CHANGELOG.md).

## Limitations

**Scope**

- One Cloud Run HTTP service per configuration (one copy per stage). No
  Cloud Run jobs or worker pools, no custom domains or load balancers, no
  Serverless VPC Access connectors (Direct VPC egress only). See the
  [roadmap](roadmap.md).
- Not supported yet on the service: GPUs, NFS and in-memory volumes, session
  affinity, manual scaling.
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

**No state file**

- `runway.yaml` is the complete list of access on what runway owns (IAP
  members, tags bound to the service, adders of secrets it created, roles of
  a runtime account it created): anything else there is removed by the next
  main deploy, including access added by hand. Elsewhere it only revokes the
  grants it recorded on the service itself (see
  [Removing access](configuration.md#removing-access)).
- Roles granted to the runtime account on resources runway never granted on
  are not found (that would need Cloud Asset Inventory) and stay.
- A first deploy that fails before the service exists cannot record its
  grants: in provenance mode they then look pre-existing and are not revoked.
- Drift is detected and corrected by the next `plan` / `deploy`, not
  continuously.
- `deploy` deletes no resources: it only removes access and tag bindings as
  above. `undeploy` deletes only the service, the runtime service account
  runway created for that app and stage (marker in its description), its
  grants, and optionally the app's images. Buckets and secrets are always
  kept. Bucket locations cannot change.

**Builds and images**

- The default buildpacks builder (`gcr.io/buildpacks/builder:latest`) is a
  moving tag, republished often: each new digest makes the next deploy
  rebuild, even with unchanged source (`deploy` says so). Pin
  `service.builder` for reproducible builds and fewer rebuilds. Build-time
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
- Release binaries and the container image's binary are about 12 MB (size
  optimized, with link-time optimization); a local `cargo build --release` is
  about 41 MB. Most of it is the Google Cloud SDK crates.

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
- Previews (`deploy --preview`: tag URL, no traffic) from GitLab CI, repeated
  on the same branch: a preview with nothing changed reuses its revision
  ("no configuration changes"), and a base image update upstream triggers a
  rebuild of unchanged source.
- A source build running alongside the rest of provisioning.
- GitLab CI on GKE runners with Workload Identity and impersonation, using
  the container image.

**Implemented and tested offline, not yet verified live**

- Canaries and `runway traffic` (pinning, promote), `preview prune` and
  `preview delete`.
- Removing access: revocation of recorded grants, removal of access and tags
  `runway.yaml` does not list, and their timing (main deploys only).
- Direct VPC egress, Cloud SQL connections, custom audiences, billing
  (`billing`, `startup_cpu_boost`), `execution_environment` and `sandbox`
  (including whether Cloud Run accepts `sandboxLauncher` sent through the v2
  API with the `BETA` launch stage).
- Reusing a matching revision on main deploys and canaries (seen live for
  previews only), and provisioning waves running in parallel.
- Secrets: creation, adders, the stop-until-a-value step, version pinning,
  secret files; grants implied by secrets and volumes.
- Generic sidecars (`service.sidecars`), including Cloud Run's handling of
  their startup checks and start order.
- The OpenTelemetry Collector sidecar (its default configuration was
  validated with the real `otelcol-google` binary).
- Cloud Storage volume mounts, image deletion (`undeploy --delete-images`),
  release tags (`--tag`, `--tag-rc`).
- Docker Hub digest resolution (anonymous token flow), Workload Identity
  Federation credentials (standard ADC `external_account` handling), the
  GitHub Actions snippets in these docs.
- Local CPU/memory validation rules; Cloud Run's server-side validation is
  authoritative (rejections surface as exit code 6).
- `doctor` permission names and the principal lookup through `tokeninfo`
  (some credential types report no email).
- The build log excerpt on failure (log ingestion delay may leave it empty;
  the log URL is always printed).

If you use one of these, please report what you see (an issue is welcome,
including "it worked").

See [Architecture](architecture.md) for the design.
