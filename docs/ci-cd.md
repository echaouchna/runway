# CI/CD with Workload Identity Federation

runway uses standard ADC, so any external-account credential file works; no
keys or custom credential store are involved.

## Container image

The public image is `ghcr.io/echaouchna/runway:edge` (Debian 13 slim with
git, curl and CA certificates, non-root user, `runway` on `PATH`, no
`ENTRYPOINT` so CI `script:` blocks work). It has no ssh client: clone over
HTTPS (GitLab's `$CI_REPOSITORY_URL` is). It can be pulled without signing in:

```sh
docker run --rm -v "$PWD:/workspace" \
  -v "$HOME/.config/gcloud/application_default_credentials.json:/creds.json:ro" \
  -e GOOGLE_APPLICATION_CREDENTIALS=/creds.json --user "$(id -u)" \
  ghcr.io/echaouchna/runway:edge runway plan --stage dev
```

The `edge` tag tracks development builds; pin a versioned tag when one is
published for your deployment. To build locally, use `docker build -t runway .`.
Add `-t` to `docker run` for colors in your terminal. CI job logs of GitLab,
GitHub Actions and similar systems get colors without it (see
[Commands](commands.md)).

In another project's `.gitlab-ci.yml` (with the WIF credential file from the
GitLab example below):

```yaml
deploy:
  image: ghcr.io/echaouchna/runway:edge
  script:
    - runway deploy --stage prod
```

GitHub Actions:

```yaml
permissions:
  contents: read
  id-token: write
steps:
  - uses: actions/checkout@v4
  - uses: google-github-actions/auth@v2
    with:
      workload_identity_provider: projects/123456789/locations/global/workloadIdentityPools/ci/providers/github
      service_account: ci-deployer@my-gcp-project.iam.gserviceaccount.com
  - run: runway deploy --stage prod -o json
```

(`gha-creds-*.json` written by the auth action is never uploaded.)

GitLab CI:

```yaml
deploy:
  id_tokens:
    GCP_ID_TOKEN:
      aud: https://iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/gitlab/providers/gitlab
  script:
    - echo "$GCP_ID_TOKEN" > /tmp/gcp_id_token
    - |
      cat > /tmp/gcp_creds.json <<EOF
      {"type": "external_account",
       "audience": "//iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/gitlab/providers/gitlab",
       "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
       "token_url": "https://sts.googleapis.com/v1/token",
       "credential_source": {"file": "/tmp/gcp_id_token"},
       "service_account_impersonation_url": "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/ci-deployer@my-gcp-project.iam.gserviceaccount.com:generateAccessToken"}
      EOF
    - export GOOGLE_APPLICATION_CREDENTIALS=/tmp/gcp_creds.json
    - runway deploy --stage prod
```

Keep credential files outside the build context.

## Pipelines

Every command reads the live project, so a pipeline is just the commands you
would type. Use `-o json` where a script reads the result (exit codes are in
[Commands](commands.md#exit-codes)).

**A URL per merge request, production on main** (GitLab CI, with the
credential setup above in a `before_script`):

```yaml
variables:
  RUNWAY_STAGE: prod

plan:
  stage: test
  script: runway plan
  rules: [{if: $CI_PIPELINE_SOURCE == "merge_request_event"}]

preview:
  stage: deploy
  script: runway deploy --preview "$CI_COMMIT_REF_NAME"
  rules: [{if: $CI_PIPELINE_SOURCE == "merge_request_event"}]

deploy:
  stage: deploy
  script: runway deploy
  rules: [{if: $CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH}]

prune-previews:                 # previews of merged or deleted branches
  stage: deploy
  variables: {GIT_DEPTH: 0}     # full history to detect merged branches
  script: runway preview prune --yes
  rules: [{if: $CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH}]
```

**Canary then promote:** `runway deploy --traffic 10` in one job,
`runway traffic --promote` in a manual job after it. See
[Previews, canaries and traffic](traffic.md).

**Releases:** `runway deploy --tag-rc` on main deploys a release candidate to
staging; a manual `runway deploy --tag` releases that exact image to
production. See [Releases](releases.md#in-ci).

**Monorepos:** deploy only the services and jobs a change touched, by path:

```yaml
deploy:
  variables: {GIT_DEPTH: 0}     # the commit to compare with must be in the clone
  script:
    - |
      # The commit to compare with: the merge request's base, else the branch's
      # previous commit. GitLab gives all zeros when there is none (merge request,
      # scheduled and manual pipelines, a branch's first pipeline).
      BASE=${CI_MERGE_REQUEST_DIFF_BASE_SHA:-$CI_COMMIT_BEFORE_SHA}
      case "$BASE" in *[!0]*) ;; *) BASE= ;; esac
      if [ -z "$BASE" ]; then
        # Nothing to compare with: deploy everything (unchanged apps change nothing).
        runway deploy --stage prod
      else
        # A failed diff (for example a base missing from the clone) fails the job.
        CHANGED=$(git diff --name-only "$BASE" HEAD -- apps/) || exit 1
        CHANGED=$(printf '%s\n' "$CHANGED" | paste -sd, -)
        if [ -n "$CHANGED" ]; then
          runway deploy --stage prod --only "$CHANGED"
        else
          echo "no app changed: nothing to deploy"
        fi
      fi
```

A path that no service or job is built from is an error, hence the
`-- apps/` filter (unless something is built from the repository root,
which contains every path).

Or keep one job per app with `--only web` and `rules: changes: [apps/web/**]`.
See [Services, jobs and schedules](services-and-jobs.md#deploy-only-what-changed).

**Jobs:** `runway run-job migrate --wait` runs a job and fails the pipeline
if it fails, for example a database migration before the deploy.
