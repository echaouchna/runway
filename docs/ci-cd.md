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
