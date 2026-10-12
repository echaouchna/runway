---
hide:
  - toc
---

<div class="rw-hero" markdown>

<p class="rw-kicker">Documentation</p>

# runway documentation

<p class="rw-lead">runway deploys applications to <strong>Google Cloud Run</strong> from a
<code>runway.yaml</code> next to your code. One file describes your services, jobs and
schedules, how they are built, what they may access and how traffic reaches them.
runway creates what they need, rolls them out, waits until they are healthy and
manages their traffic.</p>

<p class="rw-lead">There is no state file and no cluster: every command reads the live
project and changes only what differs, so running it again is always safe.</p>

[Get started :material-arrow-right:](getting-started.md){ .md-button .md-button--primary }
[Configuration reference](configuration.md){ .md-button }
[Commands](commands.md){ .md-button }

</div>

```console
$ runway init --project my-gcp-project      # writes runway.yaml (and an example app)
$ runway plan --stage dev                   # exact changes, read-only
$ runway deploy --stage dev                 # build, provision, roll out, wait until healthy
$ runway deploy --stage prod --preview "$BRANCH"   # a URL for the branch, no traffic
$ runway deploy --stage prod --traffic 10   # canary
$ runway traffic --stage prod --promote     # finish it
```

## What runway does

<div class="grid cards" markdown>

-   :material-hammer-wrench:{ .lg .middle } __Build and deploy__

    ---

    - Builds from a Dockerfile, or with buildpacks when there is none, on Cloud
      Build; or deploys an existing image. Images are content-addressed: the
      same source and base images are never built twice.
    - Deploys Cloud Run **services**, Cloud Run **jobs** and **Cloud
      Scheduler** jobs that run a job or call a service, from one file. A
      monorepo can deploy only the app a change touched (`--only apps/web`).
    - **Stages** (`dev`, `prod`, …) share one file; each overrides what
      differs (project, region, scaling, environment).

    [:material-arrow-right: Services, jobs and schedules](services-and-jobs.md) ·
    [Stages](configuration.md#stage-override-precedence)

-   :material-shield-check-outline:{ .lg .middle } __Safety__

    ---

    - `runway plan` shows field-level changes to every service, job, schedule,
      image, traffic split and grant before anything happens, and says when
      something is only known at deploy time.
    - Ownership labels and markers keep runway away from what it did not
      create; `undeploy` never deletes buckets, secrets or APIs.
    - An interrupted deploy resumes where it stopped.

    [:material-arrow-right: How it works](how-it-works.md) ·
    [Undeploying](undeploy.md)

-   :material-key-chain-variant:{ .lg .middle } __Identity, access and secrets__

    ---

    - A runtime service account per app (or per service or job), created if
      you want, with roles on exactly one bucket, dataset, secret or project.
    - Private by default; `public: true`, restricted ingress, Identity-Aware
      Proxy for sign-in with Google, custom audiences.
    - Secret Manager references as environment variables or files; secrets
      runway creates empty, with the people allowed to fill them, and a deploy
      that waits (with the exact command) until they have a value.
    - Access removed from `runway.yaml` is revoked.

    [:material-arrow-right: Permissions](permissions.md) ·
    [Secrets](secrets.md) ·
    [Removing access](configuration.md#removing-access)

-   :material-tune-variant:{ .lg .middle } __Runtime__

    ---

    - CPU, memory, scaling, concurrency, timeouts, billing, startup CPU boost,
      execution environment, health checks, Cloud Storage volumes, sidecars
      and an OpenTelemetry Collector.
    - Direct VPC egress (Shared VPC too), Cloud SQL connections, and Cloud Run
      sandboxes for running untrusted code.

    [:material-arrow-right: Configuration](configuration.md) ·
    [Networking and Cloud SQL](configuration.md#cpu-sandboxes-networking-and-cloud-sql)

-   :material-source-branch:{ .lg .middle } __Traffic and releases__

    ---

    - A URL per branch (`--preview`), canaries (`--traffic N`), promotion,
      explicit splits and rollback, without revision churn.
    - Release tags from your changelog; images promoted between stages by
      commit, without a rebuild, and published to the Artifact Registry
      repository of your choice.
    - Custom domains: a load balancer runway creates (or routes in yours), or
      Cloud Run domain mappings and `*.cloud.run` URLs, with managed
      certificates, DNS records and preview URLs on your domain.

    [:material-arrow-right: Traffic](traffic.md) ·
    [Releases](releases.md) ·
    [Custom domains](domains.md)

-   :material-domain:{ .lg .middle } __Organizations__

    ---

    - Organization policies: project and service tags bound and awaited before
      the rollout, and a first deploy that satisfies ingress conditions.
    - Workload Identity Federation in CI, impersonation, a dedicated account
      for pushing images, a container image, JSON output and stable exit
      codes.

    [:material-arrow-right: CI/CD](ci-cd.md)

</div>

## How do I…

| Task | How | Details |
|---|---|---|
| Start a new app | `runway init --project P` | [Getting started](getting-started.md#quick-start) |
| Let runway create APIs, the repository, accounts | `enable_apis`, `create_build_resources`, `identity.create` | [Getting started](getting-started.md#let-runway-create-what-it-needs) |
| Deploy an existing image | `service.image`, or `--image` | [Configuration](configuration.md) |
| See what a deploy would change | `runway plan --stage S` | [Commands](commands.md#plans-and-image-digests) |
| Draw the stack | `runway describe --stage S` | [Commands](commands.md#stack-diagrams) |
| Understand a key, or my whole runway.yaml | `runway explain [KEY]` | [Commands](commands.md#learn-the-configuration) |
| Give an AI assistant the reference | `runway explain --agent [KEY]` | [Commands](commands.md#learn-the-configuration) |
| Check credentials and permissions | `runway doctor --stage S` | [Commands](commands.md) |
| Add a second service or a job | `services:`, `jobs:` | [Services, jobs and schedules](services-and-jobs.md) |
| Run a job on a schedule | `schedules:` | [Services, jobs and schedules](services-and-jobs.md#run-a-job-every-night) |
| Run a job now | `runway run-job NAME --stage S --wait` | [Services, jobs and schedules](services-and-jobs.md#run-a-job-now) |
| Deploy only one app of a monorepo | `--only NAME` or `--only PATH` | [Services, jobs and schedules](services-and-jobs.md#deploy-only-what-changed) |
| Give each branch its own URL | `deploy --preview "$BRANCH"` | [Traffic](traffic.md) |
| Clean up previews of merged branches | `runway preview prune --stage S --yes` | [Traffic](traffic.md#managing-previews) |
| Roll out gradually | `deploy --traffic 10`, then `traffic --promote` | [Traffic](traffic.md) |
| Release what was tested | `deploy --tag-rc`, then `deploy --tag` | [Releases](releases.md) |
| Promote what another stage serves | `stages.S.promote: {from: …}` | [Releases](releases.md#promote-between-stages) |
| Serve a service on my domain | `domains: [shop.example.com]` | [Custom domains](domains.md) |
| Use a secret | `secrets: {NAME: {secret: …}}` | [Secrets](secrets.md) |
| Let people sign in with Google | `iap: {members: […]}` | [Configuration](configuration.md) |
| Reach a VPC or Cloud SQL | `vpc:`, `cloud_sql:` | [Configuration](configuration.md#cpu-sandboxes-networking-and-cloud-sql) |
| Deploy from GitHub Actions or GitLab CI | Workload Identity Federation | [CI/CD](ci-cd.md) |
| Remove an app and keep its data | `runway undeploy --stage S --yes` | [Undeploying](undeploy.md) |

## All pages

<div class="grid cards rw-pages" markdown>

-   [:material-rocket-launch-outline: __Getting started__](getting-started.md)

    Install, first deploy, what runway can create for you

-   [:material-file-cog-outline: __Configuration__](configuration.md)

    Every `runway.yaml` option, stages, variables

-   [:material-console: __Commands__](commands.md)

    The CLI, exit codes, JSON output

-   [:material-layers-triple-outline: __Services, jobs and schedules__](services-and-jobs.md)

    Several apps per file, monorepos, Cloud Run jobs, Cloud Scheduler

-   [:material-call-split: __Previews, canaries and traffic__](traffic.md)

    A URL per branch, gradual rollouts, rollback

-   [:material-tag-arrow-right-outline: __Releases__](releases.md)

    Release tags, promotion between stages, publication without rebuild

-   [:material-web: __Custom domains__](domains.md)

    Load balancers, domain mappings, certificates, DNS records

-   [:material-form-textbox-password: __Secrets__](secrets.md)

    References, files, secrets runway creates, rotation

-   [:material-pipe: __CI/CD__](ci-cd.md)

    Workload Identity Federation, the container image, pipelines

-   [:material-account-key-outline: __Permissions__](permissions.md)

    Deployer, build, runtime and scheduler identities

-   [:material-delete-sweep-outline: __Undeploying__](undeploy.md)

    What is removed and what is kept

-   [:material-cogs: __How it works__](how-it-works.md)

    Deployment steps, failure handling and recovery

-   [:material-sitemap-outline: __Architecture__](architecture.md)

    Modules and design decisions

-   [:material-map-marker-path: __Roadmap__](roadmap.md)

    What comes next

-   [:material-alert-circle-outline: __Limitations and status__](limitations.md)

    What runway does not do yet, what has been verified live

-   [:material-flask-outline: __Development__](development.md)

    Tests, live test, performance

</div>
