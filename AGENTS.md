# AGENTS.md

Guidance for AI coding agents (and humans) working on this repository.

## Project

runway is a Rust CLI that deploys applications to Google Cloud Run from a
`runway.yaml`. It talks to Google Cloud APIs directly (official
`google-cloud-*` crates, Application Default Credentials). There is no state
file: every command reads the live project and changes only what differs.
Read [docs/architecture.md](docs/architecture.md) before larger changes.

## Commands

```sh
cargo build                                  # debug build
cargo fmt --all                              # format (CI checks it)
cargo clippy --all-targets -- -D warnings    # lints (CI denies warnings)
cargo test                                   # all tests, offline (~200)
cargo test --test provision_api              # one integration test file
cargo test --lib traffic                     # unit tests matching a path
```

Use `-j 4` if the linker runs out of memory. `cargo test` never touches
Google Cloud; `tests/live_gcp.rs` is opt-in and creates billable resources
(see [docs/development.md](docs/development.md)): never run it unless asked.

## Layout

| Path | What |
|---|---|
| `src/config/` | Schema (`schema.rs`, `deny_unknown_fields`), resolution and validation (`mod.rs`, `validate.rs`), interpolation, tests |
| `src/plan.rs`, `src/traffic.rs` | Desired state, flat diff representation, traffic logic (pure, no SDK) |
| `src/gcp/` | SDK client construction, Cloud Run request building/observation (`run.rs`), registry, IAM helpers |
| `src/provision.rs` | Provisioning steps: `check` (read-only, used by `plan`) and `apply` (idempotent, used by `deploy`) |
| `src/deploy.rs` | Service reconciliation and readiness |
| `src/commands/` | One module per CLI command |
| `tests/` | Wire-level tests: real SDK clients against `wiremock` |
| `docs/` | User documentation (published with GitHub Pages), `site/` is the homepage |

## Rules

- **Idempotent and stateless.** Every mutation is preceded by a read. A step
  must be safe to re-run after a partial failure. Never add a state file.
- **Ownership.** Never modify or delete a resource runway does not own
  (labels `managed-by=runway`, `runway-app`, `runway-stage`, or the
  service-account description marker). `undeploy` never deletes data
  (buckets, secrets) or disables APIs.
- **Official SDKs.** Prefer `google-cloud-*` crates; a small REST adapter is
  acceptable only when the SDK lacks the call, and must be documented in
  `docs/architecture.md`.
- **Errors explain the fix.** Use `Error` with a kind (it drives the exit
  code), mark errors that must not be retried with `.permanent()`, and add
  hints with the permission or command that fixes the problem.
- **Plans are honest.** If something is only known during deploy, the plan
  must say so. Never present an assumption as verified.
- **Configuration changes** need: schema + resolution + validation (with the
  YAML path in messages), a config test, docs in `docs/configuration.md`, and
  a `CHANGELOG.md` entry under `[Unreleased]`.
- **Tests.** New behaviour needs a unit test; anything that sends requests
  needs a wire test in `tests/` (mock IAM etags must be base64, e.g. `"BwX1"`).
- **Output.** Results go to stdout, progress and warnings to stderr; JSON
  output must stay parseable. Colors only highlight important information.
- **No secrets in the repo**, including in tests and examples: use
  `my-gcp-project`, `example.com`, `123456789012`.
- **Style.** Match the surrounding code; keep functions small; comments
  explain why, not what. User-facing text is short, plain English.

## Before you finish

1. `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`
2. Docs and `CHANGELOG.md` updated for user-visible changes.
3. In your summary, say what was tested, and what was not (in particular
   anything not exercised against real Google Cloud).
