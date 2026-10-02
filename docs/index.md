# runway documentation

runway deploys an application to **Google Cloud Run** from a single
`runway.yaml`: it builds the image (or uses an existing one), creates what
the service needs (APIs, buckets, secrets, service accounts, grants, tags),
rolls out a new revision, waits until it is healthy and manages its traffic.

There is no state file and no cluster: every command reads the live project
and changes only what differs, so running it again is always safe.

```console
$ runway plan --stage dev
$ runway deploy --stage dev
$ runway deploy --stage prod --preview "$BRANCH"   # its own URL, no traffic
$ runway deploy --stage prod --traffic 10          # canary
$ runway traffic --stage prod --promote
```

## Start here

| Page | What you will find |
|---|---|
| [Getting started](getting-started.md) | Install, first deploy, what runway can create for you |
| [Configuration](configuration.md) | Every `runway.yaml` option, stages, variables |
| [Commands](commands.md) | The CLI, exit codes, JSON output |

## Guides

| Guide | Topic |
|---|---|
| [Previews, canaries and traffic](traffic.md) | A URL per branch, gradual rollouts, rollback |
| [Secrets](secrets.md) | References, files, secrets runway creates, rotation |
| [CI/CD](ci-cd.md) | Workload Identity Federation, the container image, pipeline examples |
| [Permissions](permissions.md) | Deployer, build and runtime identities |
| [Undeploying](undeploy.md) | What is removed and what is kept |

## Reference

| Page | Topic |
|---|---|
| [How it works](how-it-works.md) | Deployment steps, failure handling and recovery |
| [Architecture](architecture.md) | Modules and design decisions |
| [Roadmap](roadmap.md) | What comes next |
| [Limitations and status](limitations.md) | What runway does not do yet, what has been verified live |
| [Development](development.md) | Tests, live test, performance |
