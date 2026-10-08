# runway documentation

runway deploys applications to **Google Cloud Run** from a `runway.yaml`
next to your code. One file describes your services, jobs and schedules, how
they are built, what they may access and how traffic reaches them. runway
creates what they need, rolls them out, waits until they are healthy and
manages their traffic.

There is no state file and no cluster: every command reads the live project
and changes only what differs, so running it again is always safe.

```console
$ runway init --project my-gcp-project      # writes runway.yaml (and an example app)
$ runway plan --stage dev                   # exact changes, read-only
$ runway deploy --stage dev                 # build, provision, roll out, wait until healthy
$ runway deploy --stage prod --preview "$BRANCH"   # a URL for the branch, no traffic
$ runway deploy --stage prod --traffic 10   # canary
$ runway traffic --stage prod --promote     # finish it
```

New here? Start with [Getting started](getting-started.md).

## What runway does

**Build and deploy**

- Builds from a Dockerfile, or with buildpacks when there is none, on Cloud
  Build; or deploys an existing image. Images are content-addressed: the same
  source and base images are never built twice.
- Deploys Cloud Run **services**, Cloud Run **jobs** and **Cloud Scheduler**
  jobs that run a job or call a service, from one file. A monorepo can deploy
  only the app a change touched (`--only apps/web`).
  [Services, jobs and schedules](services-and-jobs.md)
- **Stages** (`dev`, `prod`, …) share one file; each overrides what differs
  (project, region, scaling, environment).
  [Configuration](configuration.md#stage-override-precedence)

**Safety**

- `runway plan` shows field-level changes to every service, job, schedule,
  image, traffic split and grant before anything happens, and says when
  something is only known at deploy time.
- Ownership labels and markers keep runway away from what it did not create;
  `undeploy` never deletes buckets, secrets or APIs.
  [Undeploying](undeploy.md)
- An interrupted deploy resumes where it stopped.
  [How it works](how-it-works.md)

**Identity, access and secrets**

- A runtime service account per app (or per service or job), created if you
  want, with roles on exactly one bucket, dataset, secret or project.
  [Permissions](permissions.md)
- Private by default; `public: true`, restricted ingress, Identity-Aware
  Proxy for sign-in with Google, custom audiences.
- Secret Manager references as environment variables or files; secrets runway
  creates empty, with the people allowed to fill them, and a deploy that
  waits (with the exact command) until they have a value.
  [Secrets](secrets.md)
- Access removed from `runway.yaml` is revoked.
  [Removing access](configuration.md#removing-access)

**Runtime**

- CPU, memory, scaling, concurrency, timeouts, billing, startup CPU boost,
  execution environment, health checks, Cloud Storage volumes, sidecars and
  an OpenTelemetry Collector.
  [Configuration](configuration.md)
- Direct VPC egress (Shared VPC too), Cloud SQL connections, and Cloud Run
  sandboxes for running untrusted code.
  [CPU, sandboxes, networking and Cloud SQL](configuration.md#cpu-sandboxes-networking-and-cloud-sql)

**Traffic and releases**

- A URL per branch (`--preview`), canaries (`--traffic N`), promotion, explicit
  splits and rollback, without revision churn.
  [Previews, canaries and traffic](traffic.md)
- Release tags from your changelog; release candidates promoted to production
  without a rebuild, published to the Artifact Registry repository of your
  choice.
  [Releases](releases.md)
- Custom domains: a load balancer runway creates (or routes in yours), or
  Cloud Run domain mappings and `*.cloud.run` URLs, with managed certificates,
  DNS records and preview URLs on your domain.
  [Custom domains](domains.md)

**Organizations**

- Organization policies: project and service tags bound and awaited before
  the rollout, and a first deploy that satisfies ingress conditions.
- Workload Identity Federation in CI, impersonation, a container image, JSON
  output and stable exit codes.
  [CI/CD](ci-cd.md)

## How do I…

| Task | How | Details |
|---|---|---|
| Start a new app | `runway init --project P` | [Getting started](getting-started.md#quick-start) |
| Let runway create APIs, the repository, accounts | `enable_apis`, `create_build_resources`, `identity.create` | [Getting started](getting-started.md#let-runway-create-what-it-needs) |
| Deploy an existing image | `service.image`, or `--image` | [Configuration](configuration.md) |
| See what a deploy would change | `runway plan --stage S` | [Commands](commands.md#plans-and-image-digests) |
| Draw the stack | `runway describe --stage S` | [Commands](commands.md#stack-diagrams) |
| Check credentials and permissions | `runway doctor --stage S` | [Commands](commands.md) |
| Add a second service or a job | `services:`, `jobs:` | [Services, jobs and schedules](services-and-jobs.md) |
| Run a job on a schedule | `schedules:` | [Services, jobs and schedules](services-and-jobs.md#run-a-job-every-night) |
| Run a job now | `runway run-job NAME --stage S --wait` | [Services, jobs and schedules](services-and-jobs.md#run-a-job-now) |
| Deploy only one app of a monorepo | `--only NAME` or `--only PATH` | [Services, jobs and schedules](services-and-jobs.md#deploy-only-what-changed) |
| Give each branch its own URL | `deploy --preview "$BRANCH"` | [Traffic](traffic.md) |
| Clean up previews of merged branches | `runway preview prune --stage S --yes` | [Traffic](traffic.md#managing-previews) |
| Roll out gradually | `deploy --traffic 10`, then `traffic --promote` | [Traffic](traffic.md) |
| Release what was tested | `deploy --tag-rc`, then `deploy --tag` | [Releases](releases.md) |
| Serve a service on my domain | `domains: [shop.example.com]` | [Custom domains](domains.md) |
| Use a secret | `secrets: {NAME: {secret: …}}` | [Secrets](secrets.md) |
| Let people sign in with Google | `iap: {members: […]}` | [Configuration](configuration.md) |
| Reach a VPC or Cloud SQL | `vpc:`, `cloud_sql:` | [Configuration](configuration.md#cpu-sandboxes-networking-and-cloud-sql) |
| Deploy from GitHub Actions or GitLab CI | Workload Identity Federation | [CI/CD](ci-cd.md) |
| Remove an app and keep its data | `runway undeploy --stage S --yes` | [Undeploying](undeploy.md) |

## All pages

| Page | What you will find |
|---|---|
| [Getting started](getting-started.md) | Install, first deploy, what runway can create for you |
| [Configuration](configuration.md) | Every `runway.yaml` option, stages, variables |
| [Commands](commands.md) | The CLI, exit codes, JSON output |
| [Services, jobs and schedules](services-and-jobs.md) | Several apps per file, monorepos, Cloud Run jobs, Cloud Scheduler |
| [Previews, canaries and traffic](traffic.md) | A URL per branch, gradual rollouts, rollback |
| [Releases](releases.md) | Release tags, release candidates, promotion without rebuild |
| [Custom domains](domains.md) | Load balancers, domain mappings, certificates, DNS records |
| [Secrets](secrets.md) | References, files, secrets runway creates, rotation |
| [CI/CD](ci-cd.md) | Workload Identity Federation, the container image, pipelines |
| [Permissions](permissions.md) | Deployer, build, runtime and scheduler identities |
| [Undeploying](undeploy.md) | What is removed and what is kept |
| [How it works](how-it-works.md) | Deployment steps, failure handling and recovery |
| [Architecture](architecture.md) | Modules and design decisions |
| [Roadmap](roadmap.md) | What comes next |
| [Limitations and status](limitations.md) | What runway does not do yet, what has been verified live |
| [Development](development.md) | Tests, live test, performance |
