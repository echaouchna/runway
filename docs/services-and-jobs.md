# Services, jobs and schedules

One `runway.yaml` can deploy several Cloud Run services, Cloud Run jobs, and
Cloud Scheduler jobs that run a job or call a service. They share the build
settings, identity, secrets and stages you give them, and `plan`, `deploy`,
previews and `undeploy` handle them together. This page shows how to do the
common things; every option is in the
[configuration reference](configuration.md#several-services-jobs-and-schedules).

A file with only `service:` keeps working exactly as before.

## Add a second service

```yaml
version: 1
app: shop
provider: {project: my-gcp-project, region: europe-west1, enable_apis: true, create_build_resources: true}

defaults:                     # inherited by every service and job
  service_account: "shop-runtime@${project}.iam.gserviceaccount.com"
  identity: {create: true}
  env: {LOG_LEVEL: info}

service:                      # the main service: shop-<stage>
  source: .
  public: true

services:
  web:                        # shop-web-<stage>
    source: apps/web          # its own folder, Dockerfile and image
    memory: 1Gi

stages:
  dev: {}
  prod:
    services: {web: {min_instances: 1}}
```

```console
$ runway plan --stage dev      # one section per service, then the stage-wide steps
$ runway deploy --stage dev
```

- The main service is `<app>-<stage>`; others are `<app>-<name>-<stage>`.
- Settings come from, lowest first: `defaults`, the stage's `defaults`, the
  service, the service's stage block.
- Each build has its own image (`<app>-<name>`); services built from the same
  folder share one image, built once.
- One runtime account in `defaults`, or one per service by setting
  `service_account` and `identity` on it.

## Add a job

```yaml
jobs:
  migrate:                    # shop-migrate-<stage>
    source: .                 # same build as the main service: one image
    command: [python, manage.py, migrate]
    tasks: 1
    max_retries: 1
    timeout_seconds: 1800     # per task
```

A job takes the runtime settings services have (image or build, `env`,
`secrets`, `volumes`, identity, `vpc`, `cloud_sql`, …) and needs at least 1
CPU and 512Mi. `deploy` creates or updates it; it does not run it.

### Run a job now

```console
$ runway run-job migrate --stage prod --wait
✓ started shop-migrate-prod-x7k2p
shop-migrate-prod shop-migrate-prod-x7k2p: succeeded
  tasks: 1 succeeded, 0 failed
$ runway logs --stage prod --only migrate --since 1h
```

`--wait` follows the execution and fails (with the log URL) if it fails.

### Run a job every night

```yaml
schedules:
  nightly:
    schedule: "0 3 * * *"
    time_zone: Europe/Paris
    job: migrate
```

## Call a service on a schedule

```yaml
schedules:
  warm-cache:
    schedule: "*/10 * * * *"
    service: web              # the main service is named after the app (`shop`)
    path: /tasks/warm-cache
    method: POST
    body: '{"full": false}'
```

Cloud Scheduler calls the service at its URL with an ID token. Schedules use
one account per app and stage (`<app>-<stage>-sched`, created by runway),
which gets `roles/run.invoker` on each target only: the service stays
private. `paused: true` creates a schedule paused; a schedule removed from
`runway.yaml` is deleted by the next full deploy.

## Deploy only what changed

In a monorepo, `--only` selects services and jobs by name or by path:

```console
$ runway deploy --stage prod --only web             # by name
$ runway deploy --stage prod --only web,migrate     # several
$ runway plan --stage prod --only apps/web/src/app.py   # by path: what is built from there
```

A path selects what is built from inside it (`--only apps` selects every app
in `apps/`), or else from the most specific folder containing it: a file
under `apps/web` selects `web`, not a service built from the repository root.

In CI, pass the changed paths under the apps' folders (a path that no
service or job is built from is an error). With GitLab CI, and the full
history (`GIT_DEPTH: 0`):

```sh
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

A deploy with `--only` changes only the selected services and jobs (and the
schedules calling them). Removing access that `runway.yaml` no longer lists
waits for a full deploy of everything.

## Previews and canaries

- `--preview NAME` gives **every** selected service a URL for the branch (one
  tag for all) and deploys each job as a copy, `<job>-<tag>`, never
  scheduled. `preview delete` and `preview prune` delete these copies with the
  preview.
- `--traffic N` canaries every selected service; jobs and schedules only
  change with a full deploy.

See [Previews, canaries and traffic](traffic.md).

## Look at the stage

```console
$ runway describe --stage prod     # a diagram per service, each job, the schedules
$ runway info --stage prod --only web
$ runway traffic --stage prod      # every service's split
```

## Remove a service or a job

Delete it from `runway.yaml`: the next `deploy` lists it but does not delete
it. Then:

```console
$ runway undeploy --stage prod --orphans          # what runway.yaml no longer lists
$ runway undeploy --stage prod --orphans --yes
```

To remove one that is still configured: `runway undeploy --stage prod --only
web --yes`. See [Undeploying](undeploy.md).

## Permissions

Jobs need `run.jobs.*` (in `roles/run.developer`). Schedules need
`roles/cloudscheduler.admin`, `actAs` on the scheduler account, and
`setIamPolicy` on their targets. See [Permissions](permissions.md).
