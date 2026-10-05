# Roadmap

runway 0.1 covers one Cloud Run HTTP service per configuration, with its
build, identity, secrets, traffic and guardrails. The items below are
planned, roughly in priority order. Priorities follow user feedback: open or
upvote an issue if something matters to you.

## Next (0.2)

- **More service options**: Serverless VPC Access connectors (Direct VPC
  egress, custom audiences, billing, execution environment and Cloud SQL
  connections are done), GPUs, NFS and in-memory volumes, session affinity.
- **Several services per file** (for example an API and a web front end),
  sharing build, identity and secrets.
- **Docker Compose**: deploy the `compose.yaml` you already run locally.
  `runway init --from compose.yaml` writes the equivalent `runway.yaml`, or
  runway reads the Compose file directly. The mapping:
  - services become Cloud Run services or sidecars;
  - `build` becomes source builds;
  - `depends_on` and health checks set the start order;
  - `environment` and `secrets` become env vars and Secret Manager secrets;
  - named volumes become Cloud Storage mounts.

  Plans, previews and guardrails apply as for any configuration. What has no
  Cloud Run equivalent (host networking, privileged containers) is reported,
  not silently dropped.
- **Cloud Run jobs**: a `jobs:` section that shares the image (or source
  build), runtime identity, secrets and environment with the service, with
  tasks, parallelism, timeout, retries and resources. `runway run-job NAME
  [--wait]` and `runway logs --job NAME` complete it. Jobs get the same
  `plan` and least-privilege grants as services.
- **Cloud Scheduler**: a `schedules:` section that triggers a job or calls a
  service endpoint on a cron schedule. It covers the time zone, retries, an
  OIDC token from a dedicated invoker account granted only `run.invoker` on
  its target, and pause/resume. Schedules show up in `plan` and `describe`.
- **Revisions and rollback**: `runway revisions` (digests, creation times,
  tags) and `runway rollback [--to REV]` on top of `runway traffic`.
- **Live verification** of the features implemented but not yet exercised
  against Google Cloud (see [Limitations and status](limitations.md)).

## Later

- **Events**: Pub/Sub push subscriptions and Eventarc triggers with dedicated
  invoker identities.
- **Deployer setup**: Workload Identity Federation pool and provider for CI
  and the deployer's bindings, with a plan/confirm step.
- **Builds**: configurable machine types and private pools, build-time
  environment for buildpacks.
- **Custom domains / load balancer integration**, for organizations whose
  ingress policies require it.
- **Automatic rollout of rotated secrets** (Secret Manager notifications).
- Server-side validation (`validateOnly`) during `plan`.

## Done since 0.1

- Faster `deploy` and `plan`: independent steps run in waves, and a source
  build overlaps the provisioning it does not depend on.
- No new revision or traffic change unless the URL a deploy targets really
  serves something different.
- Grants removed from `runway.yaml` (IAP members, runtime roles, secret
  adders) are revoked, limited to what runway recorded granting.
- Colored `describe` output.

## Done in 0.1

Builds (Dockerfile and buildpacks), stages and variables, runtime identity
and grants, buckets, secrets (references, files, creation with adders, value
checks, version pinning), API enablement, Artifact Registry, project and
service tags, org-policy-friendly bootstrap, IAP, OpenTelemetry Collector
sidecar, probes, volumes, previews, canaries and `runway traffic`, release
tags, `undeploy`, `describe`, shell completions. See the
[changelog](https://github.com/echaouchna/runway/blob/main/CHANGELOG.md).
