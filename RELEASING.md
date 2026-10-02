# Releasing and publishing (maintainers)

## One-time repository setup

1. **Placeholders**: replace `OWNER` (GitHub owner) and `OWNER-DOMAIN`
   (contact domain in `CODE_OF_CONDUCT.md`) everywhere:
   `git grep -l 'OWNER' | xargs sed -i 's/OWNER-DOMAIN/example.org/g; s/OWNER/your-org/g'`
   (`src/lib.rs` holds `DOCS_URL`, used in error hints).
2. **Blacksmith**: install the [Blacksmith GitHub app](https://www.blacksmith.sh)
   on the organization and grant it this repository. Blacksmith runs on
   GitHub organizations (not personal accounts) and works with private
   repositories. The workflows use `blacksmith-*` runner labels; without the
   app, jobs stay queued. To fall back to GitHub-hosted runners, replace
   `blacksmith-Nvcpu-ubuntu-2404[-arm]` with `ubuntu-24.04[-arm]` and
   `useblacksmith/rust-cache` with `Swatinem/rust-cache@v2`.
3. **Branch protection** on `main`: require the `ci` checks, one review,
   linear history; allow squash merges only.
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
     notes and `SHA256SUMS`.
   - `image`: builds `ghcr.io/OWNER/runway` natively for amd64 and arm64 and
     tags it `X.Y.Z`, `X.Y` and `latest` (pre-releases: only `X.Y.Z-rcN`).
     Every commit on `main` also publishes `edge` and `sha-<short>`.

## Container image while the repository is private

GitHub Container Registry packages have their **own visibility**, separate
from the repository (granular permissions):

- The first push from the workflow creates `ghcr.io/OWNER/runway`, linked to
  the repository through the `org.opencontainers.image.source` label, with
  the repository's visibility: **private**.
- **Closed beta**: keep the package private and give testers access to the
  *package* only (package settings → Manage access → add users or teams with
  Read). They pull with a personal access token that has `read:packages`:
  `echo $TOKEN | docker login ghcr.io -u USER --password-stdin`. They do not
  need access to the repository.
- **Public image, private code**: you can make the package public while the
  repository stays private (package settings → Change visibility → Public;
  for organization packages, the organization must allow public packages:
  Settings → Packages). Anyone can then pull anonymously.
- **Making a package public cannot be undone** (GitHub does not allow a
  public package to become private again). Do it when you are ready.
- Public packages are free; private packages count against the account's
  Packages storage and transfer quota.

Release binaries of a private repository are only downloadable by people
with access to the repository; during the beta, share the container image
(package access) or invite testers to the repository.

Artifact attestations (build provenance for binaries and the image) need a
public repository (or GitHub Enterprise Cloud); the workflows skip them while
the repository is private.

## Documentation site (GitHub Pages)

The `pages` workflow publishes the homepage (`site/`) at
`https://OWNER.github.io/runway/` and the documentation (`docs/`, MkDocs
Material) at `/docs/`.

- GitHub Pages from a **private** repository requires a paid plan (Pro,
  Team or Enterprise), and the site is **public** anyway (only Enterprise
  Cloud offers access-controlled Pages).
- The workflow therefore does nothing until the repository variable
  `PAGES_ENABLED` is `true` (Settings → Secrets and variables → Actions →
  Variables). Then: Settings → Pages → Source: GitHub Actions.
- Preview locally: `pip install -r docs/requirements.txt && mkdocs serve`
  (docs) and open `site/index.html` (homepage).
- A custom domain needs a `site/CNAME` file and DNS records; update
  `DOCS_URL` and `site_url` in `mkdocs.yml`.

## Going public checklist

- [ ] Placeholders replaced (`OWNER`, `OWNER-DOMAIN`); badges and links work.
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
