# Contributing to runway

Thanks for your interest! Bug reports, documentation fixes, feedback from
real deployments and code are all welcome.

## Before you start

- **Questions and ideas**: open a [discussion](https://github.com/echaouchna/runway/discussions)
  rather than an issue.
- **Bugs**: open an issue with the bug template. Include `runway --version`,
  the command, its output (`-v` helps) and a minimal `runway.yaml`.
  Remove project IDs, emails and anything secret.
- **Features**: open an issue first for anything non-trivial, so we can agree
  on the design (configuration keys are hard to change later).
- **Security issues**: never in public issues, see [SECURITY.md](SECURITY.md).

## Development setup

Tools are pinned with [mise](https://mise.jdx.dev) (`mise.toml`, locked in
`mise.lock`): Rust, Python and uv for the docs, actionlint, shellcheck,
cargo-deny, osv-scanner and trivy. No Google Cloud account is needed to build
and run the tests.

```sh
git clone https://github.com/echaouchna/runway && cd runway
mise trust && mise install
mise run check        # fmt, clippy, tests
```

Before pushing, `mise run ci` runs what CI runs: `check`, `lint` (workflows),
`deny` (dependency licenses and advisories), `docs:build` and `scan`
(osv-scanner and trivy). `mise tasks` lists everything; `mise run
deps:update` updates Rust crates, docs packages and tools. Without mise, Rust
1.91 or newer and `cargo test` are enough for code changes.

The test suite is offline: real SDK clients run against mock servers. The
live test (`tests/live_gcp.rs`) creates billable resources in a project you
provide and is opt-in, see [docs/development.md](docs/development.md).
[AGENTS.md](AGENTS.md) summarizes the project rules (also useful for humans).

## Pull requests

- Keep PRs focused: one change per PR, with tests.
- User-visible changes update the docs (`docs/`) and add a line under
  `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
- Configuration changes: schema, validation with the YAML path in messages,
  a config test and `docs/configuration.md`.
- Anything that calls a Google API needs a wire test in `tests/`.
- Write commit messages in the imperative mood ("add X", "fix Y"); we follow
  [Conventional Commits](https://www.conventionalcommits.org/) loosely
  (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `ci:`).
- Say in the PR what you tested, including whether you ran it against a real
  project.

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE) (inbound = outbound).

## Releases

Maintainers release from `main`:

1. Move `[Unreleased]` entries in `CHANGELOG.md` to a new version section and
   bump `version` in `Cargo.toml`.
2. Merge, then tag: `git tag -s v0.2.0 -m v0.2.0 && git push --tags`.
3. The `release` workflow builds binaries and the container image and
   publishes a GitHub release with the changelog section as notes.
