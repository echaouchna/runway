# Releasing and publishing (maintainers)

## One-time repository setup

1. **Repository location**: links, badges, the container image and
   `DOCS_URL` (`src/lib.rs`, used in error hints) point to
   `github.com/echaouchna/runway` and `runway.echaouchna.dev`. After a
   move (for example to an organization), update them all:
   `git grep -lw echaouchna | xargs sed -i 's/\bechaouchna\b/NEW-OWNER/g'`.
2. **Runners and repository variables** (Settings → Secrets and variables →
   Actions → Variables). The workflows run on GitHub-hosted runners unless
   told otherwise; every option is a repository variable:

   | Variable | Effect |
   |---|---|
   | `RUNNER_LINUX`, `RUNNER_LINUX_ARM` | Runner labels (for example `blacksmith-4vcpu-ubuntu-2404`, after installing the Blacksmith app on the organization) |
   | `RUNNER_LINUX_2204`, `RUNNER_LINUX_ARM_2204` | Runners for release binaries (Ubuntu 22.04, for glibc compatibility) |
   | `BUILD_ARM64` | `true`: build Linux aarch64 release binaries while the repository is private (public repositories always do; the container image is always multi-platform) |
   | `BUILD_MACOS` | `true`: build macOS binaries while private (macOS minutes count 10x) |
   | `PUBLISH_EDGE` | `true`: publish edge builds of `main` on every push: the `:edge` image (each push adds a package version) and, when the binary changed, the `edge` binaries and `runway-edge` formula |
   | `PAGES_ENABLED` | `true`: publish the homepage and documentation |
   | `HOMEBREW_TAP` | Tap repository that receives `Formula/runway.rb` (stable releases) and `Formula/runway-edge.rb` (edge builds), for example `echaouchna/homebrew-tap` (needs the secret below) |

   **Secret** `HOMEBREW_TAP_TOKEN` (Settings → Secrets and variables →
   Actions → Secrets): a fine-grained personal access token with access to
   the tap repository only and the permission Contents: Read and write
   (GitHub → Settings → Developer settings → Fine-grained tokens). Renew it
   before it expires; the `homebrew` job fails with an authentication error
   otherwise.

3. **Branch protection** on `main` (a ruleset): require a pull request, the
   `ci-ok` status check (the one check that aggregates CI, skipped jobs
   included), linear history; block force pushes and deletions; allow squash
   merges only.
4. **Security**: enable private vulnerability reporting, Dependabot alerts
   and secret scanning (Settings → Code security).
5. **Discussions**: enable them (the issue chooser points questions there).

## Releasing a version

1. Move the `[Unreleased]` entries of `CHANGELOG.md` into a new section
   `## [X.Y.Z] - YYYY-MM-DD`, update the links at the bottom, and set
   `version = "X.Y.Z"` in `Cargo.toml` (run `cargo build` to refresh
   `Cargo.lock`). Merge.
2. Tag the merge commit: `git tag -s vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`
   (`vX.Y.Z-rc1` for a pre-release).
3. Workflows:
   - `release`: checks that the tag, `Cargo.toml` and the changelog agree,
     builds Linux (x86_64, aarch64; glibc 2.35+) and macOS (arm64, x86_64)
     binaries, and publishes a GitHub release with the changelog section as
     notes and `SHA256SUMS`. For stable versions, the `homebrew` job then
     writes `Formula/runway.rb` (URLs and checksums of the built targets,
     shell completions, a `--version` test) to `HOMEBREW_TAP`, so
     `brew install echaouchna/tap/runway` gets the new version.
   - `image`: builds `ghcr.io/echaouchna/runway` for linux/amd64 and
     linux/arm64 in one job on an amd64 runner (arm64 is cross-compiled, no
     emulation) and tags it `X.Y.Z`, `X.Y` and `latest` (pre-releases: only
     `X.Y.Z-rcN`). A manual run publishes `edge`; pushes to `main` do too
     when `PUBLISH_EDGE` is `true`.

Both release binaries and edge binaries come from the reusable workflow
`build.yml`, and both formulae from `homebrew.yml`.

## Edge builds of main

With `PUBLISH_EDGE=true`, every push to `main` that changes the binary
(`src/`, `Cargo.*`, `.cargo/`, the build workflows) runs `edge`, and a manual
run on `main` does too:

- version `<Cargo.toml version>-edge.<run number>`, for example
  `0.1.0-edge.42`; `runway --version` adds the commit
  (`0.1.0-edge.42 (1a2b3c4)`, set at build time through `RUNWAY_VERSION`);
- one rolling GitHub **pre-release** `edge`: its tag is moved to the built
  commit and its archives are replaced (never marked latest);
- `Formula/runway-edge.rb` in `HOMEBREW_TAP`.

Order: the new archives are uploaded, then the formula points at them, then
the previous archives are deleted, so `brew install` never hits a missing
file. If the formula push fails, the previous archives are kept. Runs never
overlap: a newer push waits and then publishes the newest commit.

## Public container image

GitHub Container Registry packages have their **own visibility**, separate
from the repository (granular permissions):

`ghcr.io/echaouchna/runway` is public and can be pulled without signing in:

```sh
docker run --rm ghcr.io/echaouchna/runway:edge runway --help
```

Manual builds from `main` publish `edge`; tagged releases publish versioned
images. Keep installation examples on an existing tag. After a stable tagged
release publishes `latest`, that tag can also be used in examples.

Artifact attestations (build provenance for binaries and the image) need a
public repository (or GitHub Enterprise Cloud); the workflows skip them while
the repository is private.

## Documentation site (GitHub Pages)

The `pages` workflow publishes the homepage (`site/`) at
`https://runway.echaouchna.dev/` and the documentation (`docs/`, MkDocs
Material) at `/docs/`.

- GitHub Pages from a **private** repository requires a paid plan (Pro,
  Team or Enterprise), and the site is **public** anyway (only Enterprise
  Cloud offers access-controlled Pages).
- The workflow therefore does nothing until the repository variable
  `PAGES_ENABLED` is `true` (Settings → Secrets and variables → Actions →
  Variables). Then: Settings → Pages → Source: GitHub Actions.
- Preview locally: `pip install -r docs/requirements.txt && mkdocs serve`
  (docs) and open `site/index.html` (homepage).
- Custom domain `runway.echaouchna.dev`: a DNS `CNAME` record to
  `echaouchna.github.io`, set in Settings → Pages → Custom domain (with
  Enforce HTTPS). Sites deployed by a workflow need no `CNAME` file. The old
  `echaouchna.github.io/runway` addresses redirect to it. To change the
  domain, update it there and every link (`git grep runway.echaouchna.dev`:
  `DOCS_URL` in `src/lib.rs`, `site_url` in `mkdocs.yml`, `Cargo.toml`, the
  Homebrew formula in `homebrew.yml`, the README, the demo's last frame).

## Going public checklist

- [ ] Badges and links work (they point to `github.com/echaouchna/runway`).
- [ ] No internal identifiers in the code, tests, examples or docs (project
      IDs, organization IDs, group emails, internal URLs). `docs/slides/` is
      an internal presentation: remove it or move it elsewhere.
- [ ] **Git history**: earlier commits may contain internal identifiers and
      the author email of an employer. Either publish a fresh history
      (squash into one initial commit) or rewrite it (`git filter-repo`) and
      set the public author identity (`git config user.email`).
- [ ] Employer approval to open-source the code under Apache-2.0, if
      applicable (copyright owner, contributor agreement policy).
- [ ] Name check: "runway" is used by other projects and companies; check
      trademarks, the crates.io name and the GitHub organization before
      announcing (rename now is cheaper than later).
- [ ] Repository → Public; container package → Public; `PAGES_ENABLED=true`.
- [ ] Decide on crates.io publishing (`publish = false` in `Cargo.toml`).
- [ ] The GitLab CI file (`.gitlab-ci.yml`) is only needed for an internal
      GitLab mirror; remove it otherwise.
