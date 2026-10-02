# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is 0.x, minor releases may contain breaking changes to the
configuration format or the CLI; they are listed under **Changed** with
migration notes.

## [0.1.0]

Initial version (closed beta; not yet released).

### Security

- Registry hosts in image references are validated, and Google credentials
  are only sent when the requested URL's host is an Artifact Registry or
  Container Registry host (a crafted reference could send them elsewhere).

### Fixed

- Existing buckets are only modified when runway created them for this app
  (ownership labels); other buckets are used as is.
- `undeploy` re-reads the service before each delete attempt, checks
  ownership again and deletes with an etag precondition, so a retry cannot
  delete a service recreated in the meantime.
- An existing or in-flight Cloud Build is only reused if it publishes this
  app's image (identical sources in two apps no longer share a build).
- A change of runway annotations alone (for example `deploy --tag` on an
  already deployed image) updates the service (no new revision).
- `.dockerignore` exceptions re-include files inside excluded directories,
  as Docker does (`*` followed by `!src/main.py`).
- `undeploy --preview NAME -o json` prints exactly one JSON document in
  every case (on failure, the error).
- `.dockerignore`: the last matching rule wins, also when it excludes a
  parent directory after an exception (`*`, `!src/main.py`, `src`).
- Plans show changes to runway's annotations (for example a new image
  reference resolving to the same digest), like deploy applies them.
- runway's annotations (including `runway.dev/release`) are kept by
  configuration-only updates and dropped when the image changes, the same way
  in plans and deploys (an environment change used to drop the release).
- `preview prune` keeps a preview while any branch mapping to its tag is
  still open (`feature/login` and `feature-login` share a tag).
- `.dockerignore` exceptions with `**` (`!src/**/main.py`) reach files at
  any depth inside excluded directories.

### Changed

- The README and GitHub Pages homepage use the new runway logo;
  top-level CLI help renders it in cyan and violet terminal cells, with a
  monochrome ASCII version when colors are disabled.
- Release binaries and the container image are built with the `dist`
  profile: about 12 MB instead of 41 MB.
- On aarch64 Linux, builds link with `lld` (`.cargo/config.toml`).

### Added

- A README diagram explains configuration, live-state comparison, planning
  and the deployment lifecycle.
- `runway preview list|delete|prune`: list preview URLs, delete some by
  branch or tag name, or prune the previews of branches merged into the base
  branch or deleted from the remote; `--delete-revisions` also deletes
  unused preview revisions.

- `service.sidecars`: extra containers (any image) next to the application,
  with CPU/memory, command, args, env, secrets, an HTTP or TCP startup check,
  start order (`start_before_app`) and volume mounts.
- `--config` accepts any file name or a directory (alias `--file`); without
  it, `runway.yaml` and then `runway.yml` are looked up. `init` writes the
  configuration under the name given with `--config`.

- **Configuration**: `runway.yaml` (schema version 1) with stages, stage
  overrides, `vars` and `${…}` interpolation (`${project}`, `${region}`,
  `${app}`, `${stage}`, `${vars.X}`, `${buckets.X}`, `${secrets.X}`), strict
  validation with precise paths.
- **Commands**: `init`, `validate`, `doctor`, `plan`, `deploy`, `info`,
  `logs`, `traffic`, `describe` (ASCII and Mermaid), `undeploy`,
  `completions` (bash, zsh, fish, nushell, xonsh, elvish, powershell).
- **Builds**: Dockerfile or Google Cloud buildpacks on regional Cloud Build;
  deterministic, content-addressed source archives; base-image digests in the
  image identity; reuse of existing images and in-flight builds;
  `rebuild: always`; `.runwayignore`.
- **Releases**: `deploy --tag` / `--tag-rc` derive the version from the
  changelog and tag the image.
- **Service**: CPU, memory, port, timeout, concurrency, scaling, ingress,
  HTTP startup and liveness probes, environment variables, Cloud Storage
  volumes, OpenTelemetry Collector sidecar (newest release looked up at
  deploy time, or pinned).
- **Identity**: runtime service account creation and least-privilege grants
  on projects, buckets, BigQuery datasets, secrets and repositories; grants
  implied by secrets (accessor) and volumes (object viewer/user);
  impersonation (`--impersonate-service-account`).
- **Secrets**: references as environment variables or files; top-level
  `secrets:` created empty with configurable `adders`; deploy stops until
  every runway-created secret has a value; environment secrets pinned to the
  newest enabled version at deploy time.
- **Traffic**: `deploy --preview NAME` (tagged URL, no traffic, own
  revision), `deploy --traffic N` (canary), `runway traffic` (`--promote`,
  `--set`, `--remove-tag`), `undeploy --preview NAME`.
- **Project setup**: API enablement, buckets, Artifact Registry repository,
  build source bucket and build service account (`create_build_resources`).
- **Guardrails**: private by default, `public: true`, Identity-Aware Proxy,
  project and service Resource Manager tags awaited until effective,
  `bootstrap` for organization policies that need a tag before
  `ingress: all`, ownership labels and markers.
- **Operations**: per-step retries with backoff, JSON output, stable exit
  codes, colors, container image for CI.

[0.1.0]: https://github.com/OWNER/runway/releases/tag/v0.1.0
