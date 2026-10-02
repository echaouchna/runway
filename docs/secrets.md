# Secrets

- **Reference** existing secrets under `service.secrets`, as environment
  variables or as files (`path:`). The runtime service account is granted
  `roles/secretmanager.secretAccessor` on each referenced secret (the deployer
  needs `secretmanager.secrets.setIamPolicy` on them).
- **Create** secrets with the top-level `secrets:` block. runway creates them
  without a value (it never writes or reads values), grants
  `roles/secretmanager.secretVersionAdder` to `adders`, and labels them. If a
  secret runway created has no enabled version, `deploy` **stops** before
  building, prints the `gcloud secrets versions add` command for each, and
  exits with code 4; once the values are added, re-running the same deploy
  continues from there (everything before is idempotent). `undeploy` keeps
  them.
- **Picking up new values.** Cloud Run never restarts instances when a secret
  changes. Environment variables are read when an instance starts; files
  mounted with `latest` are fetched from Secret Manager when read, so the app
  sees a new value without a restart (according to Cloud Run's
  documentation). An environment variable without `version` is pinned at
  deploy time to the newest enabled version (runway lists versions; it needs
  `secretmanager.versions.list`): after adding a value, the next
  `runway deploy` sees the new version number and rolls out a new revision,
  which restarts the instances. Nothing changes when no value changed, so a
  scheduled CI job running `runway deploy` is a cheap way to roll out
  rotations. runway does not react to Secret Manager events by itself.
