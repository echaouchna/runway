# Workflows

runway deploys [Google Cloud Workflows](https://cloud.google.com/workflows)
from workflow files in your repository. Each workflow can call the stage's
services and run its jobs: runway puts their URLs and names into the
workflow, gives the workflow's account access to what it calls, and keeps
the workflow in sync on every deploy. Every option is in the
[configuration reference](configuration.md#workflows).

## Deploy a workflow

```yaml
jobs:
  export: {command: [python, export.py]}
services:
  api: {source: apps/api}

workflows:
  nightly:                              # shop-nightly-<stage>
    source: workflows/nightly.yaml      # relative to runway.yaml
    env:                                # read with sys.get_env("NAME")
      API_URL: ${services.api.url}
      EXPORT_JOB: ${jobs.export.name}
```

```yaml
# workflows/nightly.yaml
main:
  steps:
    - export:
        call: googleapis.run.v2.projects.locations.jobs.run
        args:
          name: ${sys.get_env("EXPORT_JOB")}
    - notify:
        call: http.post
        args:
          url: ${sys.get_env("API_URL") + "/exports/done"}
          auth: {type: OIDC}
```

```console
$ runway plan --stage prod       # shows the workflow's changes (source, env, account)
$ runway deploy --stage prod
```

- The workflow is named `<app>-<name>-<stage>` and lives in
  `provider.region` (or `region`).
- It is created or updated after the services and jobs it calls exist,
  since it needs their URLs. A plan made before a service exists says so.
- An unchanged workflow is not redeployed, so no new revision is created.
- Workflows change with a full deploy, not a preview or canary. With `--only`, a
  workflow changes when it calls a selected service or job.

## Call services and jobs

A workflow can get the stage's services and jobs in two ways:

- **Environment variables** (recommended): in `env`, use runway's variables.
  The workflow reads them with `sys.get_env("NAME")`, and its file stays plain
  Workflows syntax.
- **Inline references**: write `${runway.services.api.url}` in the workflow
  file. runway replaces it before uploading. Any other `${…}` is a Workflows
  expression and runway leaves it alone. Use this where an expression cannot
  go, or for a value you want written into the file.

| Reference | Value |
|---|---|
| `services.NAME.url` | the service's URL (`https://…run.app`), known once it exists |
| `services.NAME.id`, `services.NAME.name` | its ID (`shop-api-prod`), its resource name (`projects/P/locations/R/services/ID`) |
| `jobs.NAME.name` | the job's resource name: what `googleapis.run.v2.projects.locations.jobs.run` takes |
| `jobs.NAME.id` | its ID (`shop-export-prod`) |
| `jobs.NAME.url` | the Cloud Run Admin API URL that runs it (`…/jobs/ID:run`), for `http.post` with `auth: {type: OAuth2}` |
| `project`, `region`, `app`, `stage`, `vars.X`, `buckets.X`, `secrets.X` | as everywhere in runway.yaml |

The main service is named after the app (`services.shop.url`).

## Permissions

By default, every workflow of a stage runs as one account,
`<app>-<stage>-wf@PROJECT.iam.gserviceaccount.com`, which runway creates.
To give a workflow its own account, set `service_account` (and
`identity: {create: true}` for runway to create it). Set it in
`workflow_defaults` to change the account of every workflow.

runway grants the workflow's account what its references need, on each
resource only:

| It references | Its account gets |
|---|---|
| a service | `roles/run.invoker` on that service |
| a job | `roles/run.jobsExecutorWithOverrides` and `roles/run.viewer` on that job (run it, with overrides, and follow the execution) |
| `call_log_level` other than `none` | `roles/logging.logWriter` on the project |
| `identity.roles` | those roles, as for a service's account |

Who may run a workflow is `invokers`:

```yaml
workflows:
  nightly:
    source: workflows/nightly.yaml
    invokers: [group:data-team@example.com]
```

Workflows has no per-workflow access, so `roles/workflows.invoker` is granted on
the project, and those members can run **every** workflow of the project.
runway warns about this when it validates the file.

As for a service's account, runway records the grants it adds, and a full
deploy revokes those runway.yaml no longer lists. On an account runway did
not create, its roles are only ever added. On an account it created, roles
runway.yaml does not list are removed (see
[Removing access](configuration.md#removing-access)).

The deployer needs `roles/workflows.editor`, and `actAs` on the workflow's
account. See [Permissions](permissions.md#workflows). `runway doctor` checks
both.

## Run a workflow

```console
$ runway run-workflow nightly --stage prod --data '{"full": true}' --wait
✓ started 0b6f…
shop-nightly-prod 0b6f…: succeeded
  result: {"exported": 1200}
  details: https://console.cloud.google.com/workflows/workflow/…
```

`--wait` follows the execution, prints its result, and fails (with the error
and the console link) if it fails.

## Start a workflow on a schedule

```yaml
schedules:
  every-night:
    schedule: "0 3 * * *"
    workflow: nightly
    argument: '{"full": true}'    # optional, JSON
```

Cloud Scheduler starts an execution through the Workflow Executions API with
an OAuth token. Its account gets `roles/workflows.invoker` on the project, the
only level Workflows offers.

## Remove a workflow

Delete it from runway.yaml. The next full deploy deletes the workflow and
revokes the grants runway added for it. This also works when you change a
workflow's `region`, or remove the last workflow of a region: runway looks
for the workflows it created in every region. `runway undeploy --stage S`
deletes every workflow of the stage, and the workflow account if runway
created it (after revoking its project roles). With `--only`, workflows that
call what is removed are kept and listed. An account a remaining workflow,
service or job runs as is never deleted. `invokers` keep `roles/workflows.invoker`, since other
workflows of the project may rely on it.
