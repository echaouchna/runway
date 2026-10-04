# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is 0.x, minor releases may contain breaking changes to the
configuration format or the CLI; they are listed under **Changed** with
migration notes.

## [Unreleased]

### Added

- Demo (under a minute): `runway.yaml`, the `describe` diagram, a first
  deploy, an exact plan, a preview URL, a canary promoted and merged previews
  pruned. A high-resolution GIF in the README, and an interactive asciinema
  player on the homepage (pause, copy text; the player is self-hosted, no
  third-party requests).

### Changed

- The homepage and documentation moved to https://runway.echaouchna.dev
  (documentation at `/docs/`); links in the CLI (`--help`, error hints,
  `runway init`), the Homebrew formula and the README point there. The old
  `echaouchna.github.io/runway` addresses redirect.

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
