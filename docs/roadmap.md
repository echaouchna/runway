# Roadmap

runway 0.1 covers one Cloud Run HTTP service per configuration, with its
build, identity, secrets, traffic and guardrails. The items below are
planned, roughly in priority order. Priorities follow user feedback: open or
upvote an issue if something matters to you.

## Next (0.2)

- **Service options needed by larger apps**: custom audiences, always-on CPU
  (instance-based billing), execution environment, VPC access (Direct VPC
  egress, connectors), Cloud SQL connections.
- **Several services per file** (for example an API and a web front end),
  sharing build, identity and secrets.
- **Revisions and rollback**: `runway revisions` (digests, creation times,
  tags) and `runway rollback [--to REV]` on top of `runway traffic`.
- **Live verification** of the features implemented but not yet exercised
  against Google Cloud (see [Limitations and status](limitations.md)).
- **Exclusive grants** (opt-in): remove members runway added and that are no
  longer configured, using an ownership marker so removals stay safe.

## Later

- **Cloud Run jobs and scheduling**: a `jobs:` section sharing images and
  secrets with the service, Cloud Scheduler triggers, `runway run-job`.
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

## Done in 0.1

Builds (Dockerfile and buildpacks), stages and variables, runtime identity
and grants, buckets, secrets (references, files, creation with adders, value
checks, version pinning), API enablement, Artifact Registry, project and
service tags, org-policy-friendly bootstrap, IAP, OpenTelemetry Collector
sidecar, probes, volumes, previews, canaries and `runway traffic`, release
tags, `undeploy`, `describe`, shell completions. See the
[changelog](https://github.com/OWNER/runway/blob/main/CHANGELOG.md).
