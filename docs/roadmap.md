# Roadmap

runway 0.1 covers one Cloud Run HTTP service per configuration, with its
build, identity, secrets, traffic and guardrails. The items below are
planned, roughly in priority order. Priorities follow user feedback: open or
upvote an issue if something matters to you.

## Next (0.2)

- **More service options**: Serverless VPC Access connectors, GPUs, NFS and
  in-memory volumes, session affinity, manual scaling, and `cloudsql.client`
  limited to the listed instances (IAM conditions).
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
- **Workflows**: a `workflows:` section deploying Google Cloud Workflows
  that call the stage's services and jobs, with an identity granted only what
  each step calls; schedules can then target a workflow.
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
- **Automatic rollout of rotated secrets** (Secret Manager notifications).
- Server-side validation (`validateOnly`) during `plan`.

## Done since 0.1

- Custom domains: a load balancer runway creates, routes in an existing load
  balancer, or Cloud Run domain mappings and `*.cloud.run` custom URLs, with
  Certificate Manager certificates, Cloud DNS records and preview URLs on
  your domain.

- Several services, Cloud Run jobs and Cloud Scheduler jobs per file, with
  `defaults`, `--only` (names or folders, for monorepos), `runway run-job`,
  preview copies of jobs and `undeploy --orphans`. Existing files are
  unchanged.
- Service options: Direct VPC egress (including Shared VPC), Cloud SQL
  connections, custom audiences, `billing` and `startup_cpu_boost`,
  `execution_environment`, and Cloud Run sandboxes (preview) for running
  untrusted code.
- Faster `deploy` and `plan`: independent steps run in waves, and a source
  build overlaps the provisioning it does not depend on.
- No new revision or traffic change unless the URL a deploy targets really
  serves something different.
- Access removed from `runway.yaml` is revoked after a main deploy that
  serves all traffic. On what runway owns (IAP members, the service's tags,
  adders of secrets it created, roles of a runtime account it created),
  anything not listed is removed. Elsewhere, only what runway recorded
  granting.
- Billing fix: services now get request-based billing by default; they were
  deployed with CPU always allocated.
- `deploy` says why it builds (source change, base image updated upstream,
  missing image).
- `runway init` includes the common options as commented examples.
- Smaller container image (`debian:trixie-slim`, about 230 MB).
- Colored `describe` output; documentation at
  [runway.echaouchna.dev](https://runway.echaouchna.dev/docs/).

## Done in 0.1

Builds (Dockerfile and buildpacks), stages and variables, runtime identity
and grants, buckets, secrets (references, files, creation with adders, value
checks, version pinning), API enablement, Artifact Registry, project and
service tags, org-policy-friendly bootstrap, IAP, OpenTelemetry Collector
sidecar, probes, volumes, previews, canaries and `runway traffic`, release
tags, `undeploy`, `describe`, shell completions. See the
[changelog](https://github.com/echaouchna/runway/blob/main/CHANGELOG.md).
