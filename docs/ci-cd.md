# CI/CD with Workload Identity Federation

runway uses standard ADC, so any external-account credential file works; no
keys or custom credential store are involved.

## Container image

The repository's `Dockerfile` builds a small image (Debian slim, CA
certificates, non-root user, `runway` on `PATH`, no `ENTRYPOINT` so CI
`script:` blocks work). The GitLab pipeline publishes it to
`$CI_REGISTRY_IMAGE` (`:latest` on the default branch, `:<tag>` for tags):

```sh
docker build -t runway .
docker run --rm -v "$PWD:/workspace" \
  -v "$HOME/.config/gcloud/application_default_credentials.json:/creds.json:ro" \
  -e GOOGLE_APPLICATION_CREDENTIALS=/creds.json --user "$(id -u)" runway runway plan --stage dev
```

In another project's `.gitlab-ci.yml` (with the WIF credential file from the
GitLab example below):

```yaml
deploy:
  image: registry.gitlab.com/<group>/runway:latest
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
