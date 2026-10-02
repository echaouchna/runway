# How it works

## How deployments work

**Existing image**: runway resolves the tag to a digest (registry v2
`HEAD …/manifests/<tag>` with the ADC token for Google registries or the
anonymous token flow for Docker Hub) and deploys `repository@sha256:…`.

**Source build**: the build strategy is chosen per stage: an explicit
`dockerfile`, or an explicit `builder` (buildpacks); otherwise the context's
`Dockerfile` if it exists, else buildpacks with
`gcr.io/buildpacks/builder:latest`. Buildpacks run in Cloud Build with
`gcr.io/k8s-skaffold/pack` (`pack build IMAGE --builder … --network
cloudbuild`), and Cloud Build pushes the image and reports its digest, as for
Dockerfile builds. The strategy and builder are part of the source hash, so
switching between them produces a new image.

**When an image is rebuilt.** The image tag `src-<hash>` is a content address
of everything that determines the image:

- the uploaded files (code, dependency manifests, Dockerfile) and the build
  strategy/builder name;
- the **current digests of the base images**: the Dockerfile's `FROM` and
  `COPY --from=<image>` references (with `ARG` defaults substituted), or the
  buildpacks builder. A new upstream image under the same tag (security
  patches in `node:22-slim`, a new `gcr.io/buildpacks/builder:latest`) changes
  the tag, so the next deploy rebuilds.

If an image with that tag already exists, the build is skipped (config-only
changes and promotions to another stage reuse it). The base images are
recorded on the release (`runway.dev/base-images`), and `plan` explains a
rebuild caused by a base image update. `service.rebuild: always` (or
`--force-build`) rebuilds on every deploy. Not detected: dependencies that are
not pinned (no lockfile) and files excluded by `.runwayignore`.

**Release tags.** `deploy --tag` reads the latest version from the changelog
(`CHANGELOG.md`, `CHANGELOG`, `CHANGES.md`, `HISTORY.md`, in the build context
or next to `runway.yaml`): the first heading containing `vX.Y.Z` or `X.Y.Z`,
skipping "Unreleased", used as written. It tags the image with that version
in Artifact Registry, refusing to move a version that already tags another
image. `deploy --tag-rc` tags `X.Y.Z-RC<n>`, `n` being one more than the
highest existing `X.Y.Z-RC*` tag; an image that already has an RC tag for that
version keeps it (re-running a deploy does not create RC2, RC3…). The tag is
shown by `deploy` and `info` (annotation `runway.dev/release`).

1. *Scan* the build context and hash a deterministic tar stream (sorted
   entries, fixed timestamps and owners), so identical source gives an
   identical SHA-256. Nothing is compressed or buffered at this stage; this is
   all `plan` does. Ignore rules:
   - always excluded: `.git/`, `.hg/`, `.svn/`, `.runway/`, `.env`, `.env.*`
     (except `.env.example|sample|template`), `*.pem`, `*.p12`, `*.pfx`,
     `id_rsa*`, `id_ecdsa*`, `id_ed25519*`, `.ssh/`, `.aws/`, `.gcloud/`,
     `.config/gcloud/`, `.netrc`, `application_default_credentials.json`,
     `credentials.json`, `gha-creds-*.json`, `*.tfstate*`, `.terraform/`,
     and the runway config file itself (so config-only changes do not rebuild);
   - then `.runwayignore` (gitignore syntax and semantics: an excluded
     directory cannot be re-entered) if present, otherwise `.dockerignore`
     (Docker semantics: patterns are anchored at the context root, the last
     matching rule wins, and `!` re-includes files even inside excluded
     directories, so an allowlist such as `*` then `!src/main.py` works);
   - the Dockerfile and `.dockerignore` are always included.
2. If `<repo>/<app>:src-<hash>` already exists, skip to step 6. If a build
   of the same source is already running, attach to it.
3. *Compress* the tar stream with parallel gzip (all CPU cores, a standard
   single-member gzip stream) into an anonymous temporary file, re-checking
   that the content still matches the hash, and *upload* it, streamed from
   disk, to `gs://<bucket>/runway/<app>/source-<sha256>.tar.gz`. Compression
   overlaps with the in-flight build lookup.
4. *Submit* a regional Cloud Build (`docker build`, push, build service
   account, `CLOUD_LOGGING_ONLY`), pinned to the uploaded object generation.
5. *Monitor* it with bounded polling; on failure show the status, failing step,
   the last log lines and the log URL. Ctrl-C cancels the build.
6. Use the pushed **digest** from the build results.

**Service reconciliation** (Cloud Run Admin API v2):

1. Read the service. If it exists without runway's labels (or belongs to
   another app/stage) runway stops (`--adopt` takes over an unlabeled one).
2. Compare desired and live state field by field; if nothing differs, no
   request is sent.
3. Create (`serviceId` = `<app>-<stage>`) or update with an update mask
   (`labels, annotations, client, client_version, ingress,
   invoker_iam_disabled, template, traffic`) and the current `etag`.
   Labels and annotations set by others are preserved; runway owns the
   revision template. Traffic goes 100% to the latest ready revision.
4. Wait for the long-running operation and for reconciliation to finish
   (`reconciling=false`, terminal condition succeeded, latest ready revision =
   latest created revision), bounded by `--timeout`.
5. Reconcile the `allUsers` invoker binding with a read-modify-write on the
   service IAM policy (etag-protected, conditional bindings untouched).

**Full step order of `deploy`** (each step reads the live state, changes only
what is missing, and is retried per the `retry` policy):

1. Enable missing APIs (`enable_apis`): nothing else works without them.
2. Inspect: read the service (ownership check) while hashing the source and
   resolving the image.
3. Bind `provider.tags` to the project and wait until they are effective
   (organization policy conditions such as `resource.matchTag(...)` read
   them; a service cannot carry a tag before it exists, so policies checked
   at creation need the tag on the project). If such a tag was bound in this
   run, organization policy refusals of the service are retried while the
   policy engine catches up; otherwise they fail immediately.
4. Create or update buckets (`buckets:`, and the build source bucket).
5. Create the Artifact Registry repository (`create_build_resources`).
6. Create the build and runtime service accounts.
7. Grant roles: the build service account's (log writer, repository writer,
   source reader), then `identity.roles` (IAM policies via read-modify-write
   that preserves other bindings; BigQuery datasets via their access list).
8. Build the image (skipped when it already exists).
9. Create/update the service (including volumes and `iap_enabled`) and wait
   for readiness. When the service already exists, `service.tags` are bound
   and awaited *before* this update (an organization policy may need them to
   accept it); this also resumes an interrupted `bootstrap` first deploy.
10. Bind `service.tags` of a service created in this run and wait until they
   are effective (before public access, so a tag that an organization policy
   requires for `allUsers` is in place first).
11. IAP: make sure the IAP service agent exists and can invoke the service;
    grant `roles/iap.httpsResourceAccessor` to `iap.members`.
12. Public access (`allUsers` invoker added or removed).

`runway describe` prints this order for a given configuration.

**First deploy with `bootstrap`.** Tags can only be bound to a service that
exists, but an organization policy such as `constraints/run.allowedIngress`
may refuse to *create* the service with its real settings until the tag is
there. With `service.bootstrap`, when the service does not exist yet runway
creates the real service with `bootstrap.ingress` (default `internal`), binds
`service.tags` to that service only, waits until they are effective, then
switches ingress to the configured value (a service-level change: no new
revision). Organization policy refusals are retried during that run while the
policy engine catches up. With `bootstrap.image` (for example
`us-docker.pkg.dev/cloudrun/container/hello`), a minimal placeholder is
created instead and the real image and settings are applied after the tags,
useful when the app cannot start before something else exists. Later deploys
skip all this. Use `provider.tags` instead when the tag
must be on the whole project.

**OpenTelemetry Collector sidecar.** `service.otel_collector` adds the
Google-built collector (`otelcol-google`) next to the app container: the app
container is named `app` and starts after the collector's health check
(port 13133) passes. Its configuration is passed through the environment
(`--config=env:RUNWAY_OTELCOL_CONFIG`). The default one receives OTLP on
`localhost:4317`/`4318` and exports traces and logs with the `googlecloud`
exporter and metrics with `googlemanagedprometheus` (validated against
`otelcol-google` 0.160.0; the image's built-in config only prints to stdout).
runway also sets `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318` on the
app unless you set it (with a warning if it points elsewhere), grants the
runtime service account `roles/cloudtrace.agent`,
`roles/monitoring.metricWriter` and `roles/logging.logWriter`, and requires
the Cloud Trace, Monitoring and Logging APIs. Without `version`, the newest
released collector is looked up in the registry at plan/deploy time (falling
back to 0.160.0) and pinned in the service, so a new release rolls out with
the next deploy; set `version` (or `image`) to pin it. With request-based
billing the collector only gets CPU while requests are served; exports
happen in batches every few seconds.

`runway plan` runs the read-only half of every step concurrently and lists
each as `=` done, `+` pending or `?` unknown (for example, when the deployer
cannot read a policy).

## Failure handling and recovery

There is no cross-service transaction; every step is safe to retry and
`runway deploy` is the recovery command.

- **Per-step retries** (`retry` block / `--retries`): a failed step is retried
  with exponential backoff. Retried: transient API errors, timeouts,
  unhealthy revisions, permission and "does not exist" errors (typical right
  after a service account is created or a role is granted, while IAM
  propagates). Not retried: invalid configuration, ownership conflicts, failed
  Docker builds (`FAILURE`), organization policy violations
  (`constraints/...`, reported with the constraint and how to inspect it) and
  Ctrl-C. Because each attempt re-reads the
  live state, a retry never repeats work that already succeeded.
- Idempotent reads are retried by the SDK on transient errors (bounded).
- When a create/update/build submission/IAM write has an **ambiguous
  outcome** (timeout, connection reset, 5xx), runway re-reads the resource or
  looks for the tagged build before trying again. Stale-etag conflicts are
  retried after a fresh read (at most 3 attempts).
- *Build failed*: nothing was deployed; fix and re-run.
- *Build succeeded, deploy failed*: re-run; the image is reused, not rebuilt.
- *Revision unhealthy*: Cloud Run keeps traffic on the previous ready
  revision. runway reports the condition messages, revision log link and
  hints (port, secrets, image access). A re-run with an unchanged
  configuration rolls out a fresh revision (useful after fixing a grant).
- *Interrupted or timed out while waiting*: the rollout continues in Google
  Cloud; check with `runway info`, then re-run deploy to converge.
- *A step after the rollout failed* (tags, IAP, access): reported as a partial
  failure with the live URL; re-running skips everything already done.
