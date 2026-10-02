# Development

## Testing

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The offline suite covers stage resolution and validation, deterministic
packaging and ignore rules, planning and diffs, Cloud Run / Cloud Build
request construction (via SDK stubs and wire-level tests against a mock HTTP
server), reconciliation (ownership, convergence, ambiguous outcomes, etag
conflicts, unhealthy revisions, bounded waits), IAM edits, registry digest
resolution, log filters and the CLI's exit codes and JSON output.

### Live integration test (opt-in, creates billable resources)

Use a disposable project prepared as in [Prerequisites](getting-started.md#prerequisites):

```sh
export RUNWAY_LIVE_CONFIRM=create-billable-resources
export RUNWAY_LIVE_PROJECT=my-disposable-project
export RUNWAY_LIVE_REGION=europe-west1
export RUNWAY_LIVE_RUNTIME_SA=runway-runtime@my-disposable-project.iam.gserviceaccount.com
# optional: also exercise the source build
export RUNWAY_LIVE_REPOSITORY=runway
export RUNWAY_LIVE_BUCKET=my-disposable-project-runway-sources
export RUNWAY_LIVE_BUILD_SA=runway-build@my-disposable-project.iam.gserviceaccount.com
gcloud auth application-default login
cargo test --test live_gcp -- --ignored --nocapture --test-threads=1
```

It runs `doctor`, `plan`, `deploy` (twice, asserting convergence), `info` and
`logs`, then deletes the services it created. Built images and uploaded
archives remain (the bucket lifecycle rule removes archives; delete images with
`gcloud artifacts docker images delete`).

## Logo

The homepage and README use the artwork in `site/assets/`. CLI help uses
terminal cells sampled from the same logo, keeping its silhouette and
wordmark. To regenerate `src/branding.txt`, install Pillow in a temporary
Python environment and run `python design/render-terminal-logo.py`.
Pillow is only needed for regeneration; the CLI embeds the cells at build time.

## Performance

runway's own work is small compared with Cloud Build and Cloud Run, but it
avoids adding latency:

- **Local commands** (`validate`, `plan --offline`, `init`) start and finish in
  about 2 ms for typical projects.
- **Packaging** is split: `plan` only hashes the context (no compression);
  `deploy` compresses with all cores and only when an upload is needed. It
  never holds the archive in memory, and the upload streams from a temporary
  file.
- **Round trips**: independent reads run concurrently. A remote `plan` reads
  the service, its IAM policy and the registry at once; `deploy` reads the
  service while hashing the source; `info` and `doctor` issue their checks in
  parallel; each readiness poll reads the operation and the service together.
  Polling starts at 1 s and backs off to at most 5 s, so completion is noticed
  quickly without approaching API quotas.

## Build profiles and linker

- `cargo build --release`: thin LTO, stripped (41 MB). Use it locally.
- `cargo build --profile dist`: what releases and the container image ship
  (fat LTO, one codegen unit, `opt-level = "z"`, `panic = "abort"`; hashing
  and compression crates at `opt-level = 3`): about 12 MB, same packaging
  speed, a slower clean build.
- `.cargo/config.toml` links with `lld` on aarch64 Linux (incremental builds
  about 3x faster); install `lld` there. x86_64 Linux already uses Rust's
  bundled lld.

## Performance

Measured on a 15-core Linux machine (release build, warm file cache), with a
291 MB context (20k source files, two 100 MB assets, 50k ignored
`node_modules` files):

| Operation | Before | Now |
|-----------|--------|-----|
| `plan --offline` (hash only) | 2.16 s, 392 MiB peak RSS | 0.20 s, 47 MiB |
| Packaging for upload (hash + gzip) | 2.16 s, plus a ~110 MiB copy for the upload | 0.21 s hash + 0.31 s parallel gzip, 56 MiB peak |
| Remote `plan` with 300 ms API latency | ≥ 0.9 s (3 sequential reads) | 0.30 s (one round trip) |

Reproduce with `cargo bench --bench package` (synthetic context) or
`RUNWAY_BENCH_DIR=/path/to/context cargo bench --bench package`. The
round-trip behaviour is checked by `tests/latency.rs`.
