# Getting started

## Install

### Homebrew

On macOS and Linux (arm64 and x86_64):

```sh
brew install echaouchna/tap/runway
runway --version
```

Use the full name: `brew install runway` installs an unrelated app. Bash,
zsh and fish completions are installed too. Upgrade with
`brew upgrade echaouchna/tap/runway`.

To follow `main` instead of releases (rebuilt on every change, not a
release, can break):

```sh
brew uninstall echaouchna/tap/runway   # both install `runway`
brew install echaouchna/tap/runway-edge
runway --version                       # 0.1.0-edge.42 (1a2b3c4): version, build and commit
```

### Binaries

Each [release](https://github.com/echaouchna/runway/releases) has archives
for Linux (x86_64, aarch64; glibc 2.35+) and macOS (arm64, x86_64), and
`SHA256SUMS`. The [edge](https://github.com/echaouchna/runway/releases/tag/edge)
pre-release has the same archives for the latest `main`
(`runway-v0.1.0-edge.N-<target>.tar.gz`, replaced on every change).

```sh
version=v0.1.0 target=aarch64-apple-darwin   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
curl -fsSLO "https://github.com/echaouchna/runway/releases/download/$version/runway-$version-$target.tar.gz"
tar -xzf "runway-$version-$target.tar.gz"
sudo install "runway-$version-$target/runway" /usr/local/bin/
```

The macOS binaries are not notarized: if you downloaded the archive with a
browser, macOS blocks it until you run
`xattr -d com.apple.quarantine /usr/local/bin/runway` (Homebrew and `curl`
downloads are not affected).

### Container image

```sh
docker run --rm -t ghcr.io/echaouchna/runway:edge runway --help
```

The `edge` image, like the `edge` binaries and `runway-edge`, is the latest
build of `main`. See [CI/CD](ci-cd.md) for using it in pipelines.

### From source

With Rust 1.91 or newer:

```sh
# from GitHub
cargo install --locked --git https://github.com/echaouchna/runway runway

# from a local checkout
cargo install --locked --path .
```

### Shell completions

Homebrew installs bash, zsh and fish completions: open a new shell and press
<kbd>Tab</kbd> after `runway `. A fish that is not Homebrew's own does not
read Homebrew's completions; add this to `~/.config/fish/config.fish`:

```fish
if command -q brew
    set -p fish_complete_path (brew --prefix)/share/fish/vendor_completions.d
end
```

With any other installation, write the script once (again after upgrading,
for new commands and flags):

```sh
runway completions fish > ~/.config/fish/completions/runway.fish            # fish
runway completions bash > ~/.local/share/bash-completion/completions/runway # bash
runway completions zsh > "${fpath[1]}/_runway"                              # zsh, then restart it
```

Nushell, xonsh, elvish and PowerShell are supported too:
`runway completions --help` shows where each script goes. Check with
`complete -C 'runway '` in fish, which lists the commands.

The commands below use the local CLI.

runway authenticates with [Application Default Credentials](https://cloud.google.com/docs/authentication/application-default-credentials).
Locally:

```sh
gcloud auth application-default login
```

`gcloud` is only used to create those local credentials (and in the setup
commands below). Deployments call the Google Cloud APIs directly.

## Quick start

```sh
mkdir hello && cd hello
runway init --project my-gcp-project --region europe-west1
# creates runway.yaml, main.py, Dockerfile and .dockerignore (never overwrites)
runway validate                 # offline
runway describe --stage dev     # diagram + explanation of the stack (offline)
runway doctor --stage dev       # checks credentials, APIs, permissions, resources
runway plan --stage dev         # read-only preview
runway deploy --stage dev
runway info --stage dev
runway logs --stage dev --since 10m
```

`runway init --image <image>` generates an image-based configuration instead.
If a `Dockerfile` already exists, `init` only writes `runway.yaml`.

Ready-made examples live in [`examples/`](https://github.com/echaouchna/runway/tree/main/examples):
[`hello-python`](https://github.com/echaouchna/runway/tree/main/examples/hello-python) (source build) and
[`prebuilt-image`](https://github.com/echaouchna/runway/tree/main/examples/prebuilt-image) (existing image) and
[`gcptree`](https://github.com/echaouchna/runway/tree/main/examples/gcptree) (runtime service account with BigQuery, bucket
and project grants, environment variables and Identity-Aware Proxy).

## Prerequisites

By default you provide the project-level infrastructure and runway checks it
(`runway doctor`). Each piece can instead be created by runway, opt-in:
APIs (`provider.enable_apis`), the Artifact Registry repository, build source
bucket and build service account (`provider.create_build_resources`), other
buckets (`buckets:`), and the runtime service account with its grants
(`service.identity`). runway never deletes any of them.

| You provide (once per project)          | Needed for            |
|-----------------------------------------|-----------------------|
| GCP project with billing                | always                |
| Enabled APIs                            | always, unless `enable_apis: true` |
| Runtime service account                 | always, unless `identity.create: true` |
| Artifact Registry Docker repository     | source builds, unless `create_build_resources: true` |
| Cloud Storage bucket for build sources  | source builds, unless `create_build_resources: true` |
| Build service account                   | source builds, unless `create_build_resources: true` |
| Secret Manager secrets and versions     | if `secrets` are used |
| Buckets, datasets, projects that roles are granted on | if `identity.roles` / `volumes` are used |
| Tag keys and values (and any org policy using them) | if `tags` are used |
| IAP-capable project (organization, `iap.googleapis.com`) | if `iap` is used |

| runway manages                                                      | How it is identified |
|---------------------------------------------------------------------|----------------------|
| Cloud Run service `<app>-<stage>` (template, scaling, env, traffic) | labels `managed-by=runway`, `runway-app`, `runway-stage` |
| `allUsers` member of `roles/run.invoker` on that service            | only that member is added/removed |
| APIs (if `enable_apis`)                                             | enabled if missing, never disabled |
| Buckets in `buckets:` and the build source bucket                   | created if missing; uniform access, public access prevention, labels and declared lifecycle/versioning/storage class kept in line; never deleted |
| Artifact Registry repository and build service account (if `create_build_resources`) | created if missing, never deleted |
| Runtime service account (if `identity.create`)                      | created if missing, never deleted |
| Roles of the runtime service account (`identity.roles`)             | added if missing; unlisted ones removed if runway created the account, else only those runway granted ([Removing access](configuration.md#removing-access)) |
| Tag bindings on the service (`tags`)                                 | added if missing; tags bound directly to the service that are not listed are removed |
| Adders of secrets runway created (`adders`)                          | added if missing; unlisted ones removed |
| IAP: `iap_enabled`, IAP agent invoker binding, `httpsResourceAccessor` members | enabled/added; members not in `iap.members` removed |
| Source archives `gs://<bucket>/runway/<app>/source-<sha256>.tar.gz` | content-addressed |
| Images `<location>-docker.pkg.dev/<project>/<repo>/<app>:src-<hash>`| content-addressed tag |
| Cloud Build runs                                                    | tags `runway`, `runway-app-<app>`, `runway-stage-<stage>`, `runway-src-<hash>` |

Exact setup (replace the first four values):

```sh
PROJECT=my-gcp-project
REGION=europe-west1
DEPLOYER=user:you@example.com            # or serviceAccount:ci-deployer@$PROJECT.iam.gserviceaccount.com
BUCKET=$PROJECT-runway-sources

gcloud config set project $PROJECT

# 1. APIs
gcloud services enable run.googleapis.com cloudbuild.googleapis.com \
  artifactregistry.googleapis.com storage.googleapis.com logging.googleapis.com \
  secretmanager.googleapis.com iam.googleapis.com \
  cloudresourcemanager.googleapis.com serviceusage.googleapis.com

# 2. Artifact Registry repository (source builds)
gcloud artifacts repositories create runway --repository-format=docker --location=$REGION

# 3. Build source bucket (source builds), with a 30-day cleanup rule
gcloud storage buckets create gs://$BUCKET --location=$REGION --uniform-bucket-level-access
echo '{"rule":[{"action":{"type":"Delete"},"condition":{"age":30}}]}' > /tmp/lifecycle.json
gcloud storage buckets update gs://$BUCKET --lifecycle-file=/tmp/lifecycle.json

# 4. Service accounts
gcloud iam service-accounts create runway-runtime --display-name="runway runtime"
gcloud iam service-accounts create runway-build --display-name="runway builds"
RUNTIME_SA=runway-runtime@$PROJECT.iam.gserviceaccount.com
BUILD_SA=runway-build@$PROJECT.iam.gserviceaccount.com

# 5. Build identity
gcloud projects add-iam-policy-binding $PROJECT --member=serviceAccount:$BUILD_SA --role=roles/logging.logWriter
gcloud artifacts repositories add-iam-policy-binding runway --location=$REGION \
  --member=serviceAccount:$BUILD_SA --role=roles/artifactregistry.writer
gcloud storage buckets add-iam-policy-binding gs://$BUCKET \
  --member=serviceAccount:$BUILD_SA --role=roles/storage.objectViewer

# 6. Deployment identity (you, or the CI service account)
gcloud projects add-iam-policy-binding $PROJECT --member=$DEPLOYER --role=roles/run.developer   # roles/run.admin if public: true
gcloud projects add-iam-policy-binding $PROJECT --member=$DEPLOYER --role=roles/cloudbuild.builds.editor
gcloud projects add-iam-policy-binding $PROJECT --member=$DEPLOYER --role=roles/logging.viewer
gcloud storage buckets add-iam-policy-binding gs://$BUCKET --member=$DEPLOYER --role=roles/storage.objectUser
gcloud artifacts repositories add-iam-policy-binding runway --location=$REGION \
  --member=$DEPLOYER --role=roles/artifactregistry.reader
gcloud iam service-accounts add-iam-policy-binding $RUNTIME_SA --member=$DEPLOYER --role=roles/iam.serviceAccountUser
gcloud iam service-accounts add-iam-policy-binding $BUILD_SA --member=$DEPLOYER --role=roles/iam.serviceAccountUser

# 7. Runtime identity: one binding per secret it reads
gcloud secrets create database-url --replication-policy=automatic
printf '%s' 'postgres://...' | gcloud secrets versions add database-url --data-file=-
gcloud secrets add-iam-policy-binding database-url \
  --member=serviceAccount:$RUNTIME_SA --role=roles/secretmanager.secretAccessor
```

Grant the runtime service account only what your application needs (for
example `roles/cloudsql.client`). For image-only deployments, skip steps 2, 3
and 5 and the build-related bindings in step 6.
