# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is 0.x, minor releases may contain breaking changes to the
configuration format or the CLI; they are listed under **Changed** with
migration notes.

## [Unreleased]

### Added

- **Where images go, promotion and who pushes, all optional.**
  `provider.artifact_project` and `provider.artifact_package` (global or per
  stage) push builds to a repository in another project (a shared registry)
  under a chosen image path; Cloud Build still runs in `provider.project`,
  and releases default to the same path. `stages.<name>.promote.from` makes
  a stage deploy another stage's images instead of building: for each
  service and job, what its counterpart *serves* (its one revision with all
  the traffic, as Cloud Run reports it; a split is refused until settled, a
  rollout in progress is waited for), read with that revision's provenance.
  With `--tag-rc`/`--tag`, the version the stage already serves is reused,
  else the source must serve a candidate of the version or an image built
  from the same commit; a version another stage published in a shared
  release repository for another image is refused; otherwise the deploy
  stops and says why. The image is copied (same digest)
  into the stage's own repository and that copy runs; `plan` shows that
  reference, why, and the copies and tags deploy makes. A promoting stage
  builds nothing, so it provisions no source bucket, build account or Cloud
  Build; a service or job with its own `image:` (or `--image`) deploys that
  image instead, in every deploy path. `--force-build` is refused there. A
  stage without `promote` builds, as before. `provider.push_service_account`
  runs the registry work runway does itself (finding, copying, tagging) as
  another account, such as the CI's; `doctor` checks it by impersonating it
  from your own credentials, as deploy does.
- **Promote the tagged commit.** `stages.<name>.promote.commit: checkout`
  promotes the image the source stage ran built from the commit being
  deployed (HEAD, or `RUNWAY_SOURCE_COMMIT` on a tag pipeline), found in its
  Ready revisions even after it moved on; no match or several images stop
  the deploy, and a matching release tag never stands in for the commit.
  `--tag` on a commit tagged with a version (`CI_COMMIT_TAG`, GitHub's tag
  ref, or `git tag --points-at`) requires the changelog's version to match.
- **Revisions say what they run.** Each revision (and job) runway deploys
  records the commit its image comes from and its release
  (`runway.dev/source`, `runway.dev/release` on the revision template),
  shown by `plan` as `provenance.*`. The first deploy after upgrading rolls a
  new revision for it, and so does a new commit with an unchanged image. A
  promoted stage records its image's commit, never the checkout's: when it
  is unknown, its commit record is removed.
- **`--tag`/`--tag-rc` publish any image**: a build, a promotion or a
  configured image (which needs a `release.repository`). With stages mapped
  to `tag-rc`, `--tag` now releases the candidate the `tag-rc` stage
  *serves*, not the latest one of a repository it shares (an untested newer
  RC was released before).
- **Release image path.** `release.repository.package` (global or per stage)
  names the image path in the release repository, nested paths included:
  `package: mr-terraform-agent/agent` makes `deploy --tag`/`--tag-rc` publish
  `LOCATION-docker.pkg.dev/PROJECT/REPOSITORY/mr-terraform-agent/agent:X.Y.Z`
  and deploy that copy. Default: the build's name, as before. A stage that
  builds several images with a `package` set is a validation error.
- **Several runs at once.** Runs on the same stage coordinate through a lease
  on the stage's own Cloud Run service (annotation `runway.dev/lease`,
  written with the etag; no lock file): deploys, canaries, traffic changes
  and teardowns are exclusive, previews share the stage. A busy stage makes
  a run wait and say for whom (`--wait-timeout`, default 30m; `--no-wait`).
  Leases are renewed every 30 seconds and expire after 2 minutes;
  `runway unlock --stage S [--yes]` shows or removes one (on services runway
  manages only); `plan` says when the stage is busy. A run that loses its
  lease stops before its next change; a teardown deletes the service holding
  the lease last.
- **Older commits are refused.** Deploys record their commit
  (`runway.dev/source`); a deploy or canary of a commit older than the one
  serving (by ancestry, else commit date) is refused unless `--allow-older`.
  `RUNWAY_SOURCE_COMMIT`/`RUNWAY_SOURCE_TIME` replace git; `plan` says when a
  deploy would be refused.
- **Clearer plan and deploy output, nothing hidden.** Sidecars appear setting
  by setting instead of a hash, the OpenTelemetry Collector too (a
  configuration you wrote in full, runway's default by name and hash); step details with several changes get a line
  each; the APIs step lists the APIs; `plan` ends with a summary; `deploy`
  shows the time of each phase, every step (already-done ones with their
  details too), every field a rollout sets (also when it creates a service
  or job), and ends with each service's URL, revision, image, access, whole
  traffic split and changes, each job, and every step. `-o json` gains
  `changes` per service and job.

- **`runway explain`.** Learn `runway.yaml` in the terminal: a full-screen
  browser of every key (what it does, default, example, rules, docs link),
  with service keys grouped by topic, search (`/`), and guide topics (stages
  and precedence, variables, names). The search is offline and takes a
  key, a word or what you want to do (`keep an instance warm` finds
  `service.min_instances`): results come best first with why each was
  found, understanding word forms, other names (`ram` for memory) and
  typos; `Esc` goes back to the tree at the key picked. Next to a
  `runway.yaml`, it marks the keys the file sets and its problems, and
  shows where each key is set and its resolved value per stage (`f`: only your keys, `n`: next problem).
  `→`/`Enter` move to the explanation to scroll it with `↑`/`↓`, `←`/`Esc`
  come back. `--agent` prints the reference as Markdown for LLMs and coding
  agents (with the current file's values and problems; with a key, that key
  and the keys below it).
  `runway explain KEY` prints one key (written as in the file, with "did you
  mean" suggestions); in a pipe it lists every key; `-o json` for scripts.

- **Custom domains.** `service.domains` (and `services.<name>.domains`) lists
  hosts, host paths (`example.com/api`) and Cloud Run custom URLs
  (`NAME.cloud.run`); a stage-wide `domains` block chooses how they are
  served: `load-balancer` (default: a global external Application Load
  Balancer runway creates for the app and stage, with an HTTP-to-HTTPS
  redirect), `existing-load-balancer` (runway adds its NEGs, backends and
  host rules to a URL map it does not own, and removes only those) or
  `domain-mapping` (Cloud Run domain mappings). Certificates are Certificate
  Manager certificates with DNS authorization, issued before the domain
  points at the load balancer. With `domains.dns`, runway writes the A and
  authorization records in a Cloud DNS zone (any project); without it, or
  without permission, `plan` and `deploy` print the records to create
  elsewhere. `preview_domain: "*.preview.example.com"` serves each preview on
  `<tag>.preview.example.com`. Domains removed from runway.yaml are cleaned
  up by the next full deploy; `undeploy` removes them first and keeps
  `*.cloud.run` URLs unless `--release-urls` is given. DNS records are only
  changed or deleted when their data is what runway set.
- **Release repositories and promotion.** A `release` block (global, and per
  stage) publishes released images to a chosen Artifact Registry repository:
  runway copies the image there with the same digest (registry protocol,
  layers mounted on the same host) and deploys that copy.
  `stages.<name>.release.flag: tag | tag-rc` maps a stage to `deploy --tag` or
  `--tag-rc`: it becomes the flag's default stage and the only one the flag
  deploys. Once a stage is mapped to `tag-rc`, `deploy --tag` builds nothing:
  it releases the latest `X.Y.Z-RC<n>` of the changelog version (copied from
  the `tag-rc` stage's repository when needed, after provisioning;
  `create_build_resources` also creates the release repository). A `tag`
  stage can name its source with `release.from`; candidates found in several
  repositories must be the same image. Files without a `release` block work
  as before.
- **Several services, jobs and schedules per file.** `services:` and `jobs:`
  (named `<app>-<name>-<stage>`) next to the main `service`, `schedules:`
  that run a job or call a service, and `defaults:` that every service and
  job inherits (precedence: `defaults`, stage `defaults`, the workload, its
  stage block). Files with only `service:` work as before: same names,
  labels, images, grants record and plans.
  - Jobs: build or image, `command`/`args`, `tasks`, `parallelism`,
    `max_retries`, a task `timeout_seconds` (default 600), and the runtime
    settings services have (at least 1 CPU and 512Mi, as Cloud Run requires). `runway run-job NAME [--wait]` runs one;
    `runway logs --only NAME` reads its logs.
  - Schedules: cron, time zone, retries, deadline, paused; one invoker
    account per app and stage (created by runway by default) with
    `roles/run.invoker` on each target only. Removed schedules are deleted
    and their grants revoked by the next full deploy.
  - `--only NAME|PATH` on every command with `--stage`: a path selects what
    is built from inside it, or else from the most specific folder
    containing it, so CI in a monorepo can deploy only the app a change
    touched.
  - Builds that are the same run once; each build has its own image package
    (`<app>` for the main service, `<app>-<name>` otherwise).
  - Previews tag every service and deploy jobs as unscheduled copies
    (`<job>-<tag>`), deleted with the preview; canaries leave jobs and
    schedules alone.
  - `undeploy --orphans` removes services and jobs runway.yaml no longer
    lists; `undeploy` also removes the stage's schedules and scheduler
    account.
- `service.command` and `service.args` set the app container's entrypoint.
- `undeploy` deletes a runtime account shared by several services or jobs
  once all of them are gone, after revoking the roles each of them declared
  (also those of a workload with `identity.create: false`); the plan lists
  the account as deleted and every role as revoked.
- `undeploy` keeps a runtime account while any live job runs as it, even one
  runway.yaml no longer lists (it now lists jobs: `run.jobs.list`, in
  `roles/run.developer`; without it the account is kept).
- Service options:
  - `billing` (`request-based` or `instance-based`) and `startup_cpu_boost`;
  - `execution_environment` (`gen1`, `gen2`);
  - Direct VPC egress with `vpc` (network, subnet, egress, network tags;
    Shared VPC with full resource names);
  - Cloud SQL connections with `cloud_sql` (socket at `/cloudsql/...`; the
    runtime account gets `roles/cloudsql.client`, the Cloud SQL Admin API is
    enabled);
  - `custom_audiences`, a service setting that creates no revision;
  - `sandbox` (Cloud Run sandboxes, preview): the app can run untrusted code
    with the `sandbox` command. It implies gen2 and sets the service's launch
    stage to `BETA`.

  Validation follows Cloud Run's rules (memory and CPU minimums, gen1 and
  gen2 constraints). Below 1 CPU now requires `concurrency: 1`, as Cloud Run
  does. `describe` shows the VPC and Cloud SQL connections.
- `runway init` writes the common optional settings as commented one-line
  examples, each valid once uncommented.
- Demo (under a minute): `runway.yaml`, the `describe` diagram, a first
  deploy, an exact plan, a preview URL, a canary promoted and merged previews
  pruned. A high-resolution GIF in the README, and an interactive asciinema
  player on the homepage (pause, copy text; the player is self-hosted, no
  third-party requests).
- A redesigned "How it works" diagram in the README (light and dark), with
  the plan, the deploy waves and the resulting URLs; generated by
  `design/render-how-it-works.py`.
- Roadmap: Docker Compose support, Cloud Run jobs and Cloud Scheduler.
- `deploy` says why it builds: the source changed, a base image changed
  upstream (with both digests; the buildpacks builder `latest` is
  republished often), `--force-build`, `rebuild: always`, or the image is
  missing from the registry.
- Development: local builds keep line tables only and no debug info for
  dependencies, so `target/` takes 3.9 GB instead of 8.5 GB from scratch and
  builds about 30% faster (`docs/development.md` has the clean-up commands).

### Changed

- **macOS: Apple silicon only.** Releases and the Homebrew formulas no longer
  include an Intel (x86_64) macOS build; on an Intel Mac, build from source
  (`cargo install --locked --git https://github.com/echaouchna/runway runway`) or use
  the container image.

- Smaller container image: `debian:trixie-slim` with only git, curl and CA
  certificates, instead of `buildpack-deps:trixie-scm` (which also shipped
  Mercurial, Subversion, wget, gnupg and an ssh client): about 230 MB instead
  of 404 MB. Clone over HTTPS in jobs using the image. Building the arm64
  image now uses QEMU emulation for the package install.
- **Authoritative access on what runway owns.** After a main deploy that
  serves all traffic, runway removes what `runway.yaml` does not list, whoever
  added it: IAP members on the service's IAP resource, tags bound directly to
  the service (inherited tags stay), adders of secrets runway created, and
  roles of a runtime account runway created (on the deployment project and
  every resource runway grants on). IAP members left after IAP is disabled
  are removed too. Only unconditional bindings and dataset entries;
  conditional ones, other roles and other resources are never touched. `plan` lists each removal with `-`.
  Previews and canaries never remove anything. Elsewhere, runway still only
  revokes what it granted itself. **A role or IAP member added by hand to
  these is removed by the next main deploy: add it to `runway.yaml`.**
- Faster `deploy` and `plan`: independent steps run together. Provisioning
  runs in dependency waves: buckets, secrets, the repository and service
  accounts together, then grants together, with writes to the same IAM
  policy one at a time. A source build starts as soon as its own resources
  exist and overlaps the rest of the provisioning. The registry login runs
  while APIs are enabled, post-rollout tags and IAP steps run together, secret
  versions are looked up together, and `plan` checks run during the first
  reads. Output keeps the step order; a failure stops before the next wave.
- No revision or traffic change unless the URL a deploy targets really
  serves something different: when the revision behind the main URL, the
  preview's tag or the canary already runs the desired image and
  configuration, it keeps serving and only real differences are applied (for
  example, redeploying a branch after another one was previewed, or a main
  deploy right after a preview). `deploy` reports the revision behind the
  URL. `plan` shows the same, and now also shows `+ revision` when a preview
  or canary gets a revision of its own, which it used to report as no change.
- Removing an IAP member, a role of the runtime account or a secret adder
  from `runway.yaml` now revokes it, after the rollout of a main deploy once
  one revision serves all traffic. Previews, canaries and partial rollouts
  never revoke, since a revision still serving may need the access. runway
  records on the service the grants it adds (annotation
  `runway.dev/grants`, no state file), as soon as it adds them and also when
  a later step fails, and only revokes what it recorded. A grant write whose
  answer was lost (timeout, 5xx) counts as runway's when a later read shows
  it, including the read runway makes before failing, so ownership survives
  even when the deploy stops there. Kept: access already in place when runway
  checked it (granted by hand or by other tools, even if also configured),
  roles of a runtime account runway did not create for this service, and the
  shared build account's roles. Recorded IAP members are revoked even after
  IAP is disabled. `plan` lists revocations with `-`, and adding a member
  shows only that member (`+ IAP access (grant to group:new@example.com)`).
  The IAP and adder steps no longer list every member in their name. In
  JSON plans, the new step state `pending_removal` marks a revocation.
- The homepage and documentation moved to https://runway.echaouchna.dev
  (documentation at `/docs/`); links in the CLI (`--help`, error hints,
  `runway init`), the Homebrew formula and the README point there. The old
  `echaouchna.github.io/runway` addresses redirect.

### Fixed

- **Messages name keys by their full path.** `doctor` said "set
  identity.create / provider.create_build_resources" for any missing account:
  it now names the one setting that creates it (`service.identity.create`,
  `services.<name>.…`, `jobs.<name>.…`, or `provider.create_build_resources`)
  and links the docs instead of README sections that do not exist. Likewise:
  a missing image names where it is set (`stages.prod.service.image`,
  `defaults.image`, `--image`), `PORT` in `env` points at `port` in the same
  block (not for jobs, which have none), the startup-failure hint names the
  service's own `port`, removed tags and `undeploy` name the workload's block,
  and validation messages say `provider.create_build_resources`.
- **Billing:** services were deployed with instance-based billing (CPU always
  allocated, billed for the instance's whole life) instead of Cloud Run's
  request-based default: with resource limits set, Cloud Run needs `cpuIdle`
  explicitly, which runway never sent. runway now always sends it; the next
  plan shows `~ billing: instance-based -> request-based` for existing
  services and the next deploy creates a revision. Set `billing:
  instance-based` to keep the previous behaviour.

## [0.1.1] - 2026-10-04

### Changed

- `runway describe` colors its diagram and explanation in terminals and CI
  logs: the service stands out, permissions are yellow and what they apply
  to cyan, names and values are cyan instead of quoted, lines and boxes are
  dimmed. Without colors (files, pipes, `NO_COLOR`, `--color never`) and in
  JSON, the output is unchanged: plain Markdown and ASCII.

### Added

- Documentation: setting up shell completions for every shell, including
  fish when it does not come from Homebrew, and previewing
  `describe --format mermaid` from the terminal with mermaid-cli and a
  terminal image viewer.

## [0.1.0] - 2026-10-04

First release: deploy applications to Google Cloud Run from a single
`runway.yaml`, with no state file. Every command reads the live project and
changes only what differs.

### Added

- **Configuration**: `runway.yaml` (schema version 1) with stages, stage
  overrides, `vars` and `${…}` interpolation (`${project}`, `${region}`,
  `${app}`, `${stage}`, `${vars.X}`, `${buckets.X}`, `${secrets.X}`), strict
  validation with precise paths. `--config` accepts any file name or a
  directory (alias `--file`); without it, `runway.yaml` and then
  `runway.yml` are looked up. `init` writes the configuration under the name
  given with `--config`.
- **Commands**: `init`, `validate`, `doctor`, `plan`, `deploy`, `info`,
  `logs`, `traffic`, `preview`, `describe` (ASCII and Mermaid), `undeploy`,
  `completions` (bash, zsh, fish, nushell, xonsh, elvish, powershell).
- **Plans**: field-level diff against the live service, including traffic,
  image digests, grants, access and runway's annotations; a plan says when
  something is only known at deploy time.
- **Builds**: Dockerfile or Google Cloud buildpacks on regional Cloud Build;
  deterministic, content-addressed source archives; base-image digests in the
  image identity; reuse of existing images and of in-flight builds of the
  same app; `rebuild: always`; `.runwayignore` and `.dockerignore` with
  Docker's rules (last match wins, exceptions such as `!src/**/main.py`
  re-include files inside excluded directories).
- **Releases**: `deploy --tag` / `--tag-rc` derive the version from the
  changelog and tag the image; the release annotation is kept by
  configuration-only updates.
- **Service**: CPU, memory, port, timeout, concurrency, scaling, ingress,
  HTTP startup and liveness probes, environment variables, Cloud Storage
  volumes, an OpenTelemetry Collector sidecar (newest release looked up at
  deploy time, or pinned) and `service.sidecars` (any image, with resources,
  command, env, secrets, startup check, start order and volume mounts).
- **Identity**: runtime service account creation and least-privilege grants
  on projects, buckets, BigQuery datasets, secrets and repositories; grants
  implied by secrets (accessor) and volumes (object viewer/user);
  impersonation (`--impersonate-service-account`).
- **Secrets**: references as environment variables or files; top-level
  `secrets:` created empty with configurable `adders`; deploy stops until
  every runway-created secret has a value; environment secrets pinned to the
  newest enabled version at deploy time.
- **Traffic**: `deploy --preview NAME` (tagged URL, no traffic, own
  revision; branch names become tags), `deploy --traffic N` (canary),
  `runway traffic` (`--promote`, `--set`, `--remove-tag`). `runway preview
  list|delete|prune` lists preview URLs, deletes them by branch or tag, or
  prunes those of branches merged or deleted (`--delete-revisions` also
  deletes unused preview revisions).
- **Project setup**: API enablement, buckets, Artifact Registry repository,
  build source bucket and build service account (`create_build_resources`).
- **Guardrails**: private by default, `public: true`, Identity-Aware Proxy,
  project and service Resource Manager tags awaited until effective,
  `bootstrap` for organization policies that need a tag before
  `ingress: all`. runway only modifies or deletes what it owns (labels and
  markers): existing buckets it did not create are used as is, and
  `undeploy` re-checks ownership and deletes with an etag precondition.
  `undeploy` never deletes data or disables APIs.
- **Operations**: per-step retries with backoff, JSON output (one document
  per command, errors included), stable exit codes.
- **Colors**: status, diffs, help of every command (headers, flags,
  values) and usage errors are colored in terminals and in CI job logs that
  display colors (GitLab CI, GitHub Actions, Gitea/Forgejo Actions,
  Buildkite, CircleCI, Azure Pipelines), never in output redirected to a
  file or in JSON. `--color auto|always|never`, `RUNWAY_COLOR`, `NO_COLOR`
  (an empty value does not disable colors), `CLICOLOR_FORCE` and
  `FORCE_COLOR` are honoured. Top-level help shows the runway logo in
  terminal cells, with a monochrome ASCII version without colors.
- **Distribution**:
  - Homebrew formula for macOS and Linux (arm64, x86_64), with bash, zsh and
    fish completions: `brew install echaouchna/tap/runway`.
  - Release binaries for Linux (x86_64, aarch64; glibc 2.35+) and macOS
    (arm64, x86_64), with `SHA256SUMS` and build provenance. Binaries and
    the image are built with the `dist` profile: about 12 MB instead of
    41 MB.
  - Container image `ghcr.io/echaouchna/runway` for CI pipelines: Debian 13
    (trixie, `buildpack-deps:trixie-scm`, with git for `runway preview
    prune` and curl), no entrypoint, colors with `docker run -t` (no
    `NO_COLOR`), always published for linux/amd64 and linux/arm64 (one amd64
    runner cross-compiles arm64, with no emulation).
  - Edge builds of `main`: the `:edge` image, binaries in a rolling `edge`
    pre-release and `echaouchna/tap/runway-edge`; `runway --version` shows
    their build and commit (`0.1.0-edge.42 (1a2b3c4)`).
- **Documentation**: site with a homepage, guides and the configuration
  reference (https://echaouchna.github.io/runway/); a README diagram
  explains configuration, live-state comparison, planning and the deployment
  lifecycle; installation instructions for Homebrew, binaries, the container
  image and source builds; examples and tests use generic placeholders, not
  organization-specific identifiers. The blue runway r monogram and bold
  lowercase wordmark are used across the README, homepage, documentation,
  favicons and CLI help (SVG sources and a regeneration script keep all
  variants consistent). The homepage's code examples fit their cards on
  desktops and tablets (they scroll only on narrow phones, with a dark
  scrollbar).
- **Development**: tools pinned with mise (`mise.toml`, `mise.lock`), with
  tasks for checks, scans, docs and dependency updates; documentation
  tooling pinned with hashes (full dependency tree, `docs/requirements.txt`
  generated from `docs/requirements.in`), which fixes PYSEC-2026-215 /
  GHSA-65pc-fj4g-8rjx (idna 3.20); builds on aarch64 Linux link with `lld`
  (`.cargo/config.toml`). GitHub Actions: GitHub-hosted runners by default
  (configurable), checks run only when their files change, one aggregated
  `ci-ok` check, actions pinned by commit SHA, monthly grouped Dependabot
  updates; aarch64 and macOS release binaries are opt-in while the
  repository is private.

### Security

- Registry hosts in image references are validated, and Google credentials
  are only sent when the requested URL's host is an Artifact Registry or
  Container Registry host (a crafted reference could send them elsewhere).

[Unreleased]: https://github.com/echaouchna/runway/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/echaouchna/runway/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/echaouchna/runway/releases/tag/v0.1.0
