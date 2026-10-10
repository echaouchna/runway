# Releases

runway tags the images it deploys with the version in your changelog, and
can release to production exactly the image you tested in staging, without
rebuilding it.

## Tag a release

```console
$ runway deploy --stage prod --tag       # tags the deployed image 1.4.2
$ runway deploy --stage staging --tag-rc # tags it 1.4.2-RC1, then RC2, …
```

The version is the first heading of the changelog (`CHANGELOG.md`,
`CHANGELOG`, `CHANGES.md` or `HISTORY.md`, next to `runway.yaml` or in the
build context) that contains `vX.Y.Z` or `X.Y.Z`, skipping "Unreleased":
`## [1.4.2] - 2026-10-01`, `## v1.4.2`, `# 1.4.2 (date)`.

- `--tag` refuses to move a version that already tags another image: bump
  the changelog first.
- `--tag-rc` uses the next number after the highest `X.Y.Z-RC<n>`. Re-running
  the deploy of the same image keeps its RC tag (no RC2, RC3…).
- The tag is shown by `deploy` and `info` (annotation `runway.dev/release`).

Without the configuration below, that is all: the image is built (or reused)
and tagged in the stage's build repository.

## Release what you tested

Map stages to the flags, and `--tag` stops building:

```yaml
stages:
  staging:
    release: {flag: tag-rc}   # `runway deploy --tag-rc` deploys staging
  prod:
    release: {flag: tag}      # `runway deploy --tag` deploys prod
```

The flow:

1. Merge, and bump the changelog to `1.4.2`.
2. `runway deploy --tag-rc`: staging builds and runs `1.4.2-RC1`.
3. Test. A fix? Merge it and run `--tag-rc` again: `1.4.2-RC2`.
4. `runway deploy --tag`: prod deploys **the RC of `1.4.2` that staging
   serves**, tags it `1.4.2`, and builds nothing.

What was tested is what is released, whatever changed since: the changelog
itself is part of the build context, and base images such as
`gcr.io/buildpacks/builder:latest` are republished upstream, so a rebuild
would give another image. The candidate is the one staging *serves* (its
revision with all the traffic), not the latest one in a repository: a newer
RC nobody tested is never released. Re-running `--tag` reuses the `1.4.2`
tag. Without a candidate of the version served by staging, `--tag` fails and
says how to make one. `--force-build` is refused with `--tag` then. This is
the same as `stages.prod.promote.from: staging` (see below), applied only
with `--tag`.

**Stages.** A stage mapped to a flag is the flag's default, so `--stage` is
not needed. Once a stage is mapped to a flag, the flag deploys no other stage
(`runway deploy --tag --stage dev` is refused). With several stages mapped to
the same flag, `--stage` picks one.

## Publish to a release repository

By default, release tags go to each stage's build repository. A release
repository keeps releases apart, for example in the production project:

```yaml
release:
  repository:                 # global: every stage publishes here
    project: my-release-project   # default: provider.project
    location: europe-west1        # default: the build repository's location
    repository: releases

stages:
  staging:
    release: {flag: tag-rc}
  prod:
    release:
      flag: tag
      repository: {project: my-prod-project, repository: releases}   # prod's own
```

The image is copied with the same digest (layers already there are skipped;
on the same registry host they are mounted, not transferred), tagged there,
and the stage deploys that copy. `--tag` finds the candidate where the
`tag-rc` stage published it and copies it into its own repository. With
`provider.create_build_resources: true`, runway creates the release
repository when it is missing.

**The image path.** Released images keep the build's name (`<app>`, or
`<app>-<name>` for named services and jobs) unless `package` names another
path in the release repository, nested paths included:

```yaml
release:
  repository:
    project: my-release-project
    location: europe-west1
    repository: docker-releases
    package: mr-terraform-agent/agent
stages:
  prod: {}    # no tag-rc stage: `--tag` builds, publishes and deploys
```

`runway deploy --stage prod --tag` builds the image (or reuses it), copies it
to `europe-west1-docker.pkg.dev/my-release-project/docker-releases/mr-terraform-agent/agent`,
tags it with the changelog version and deploys that copy. A package names one
image: a stage that builds several is an error.

**Several `tag-rc` stages.** Each repository numbers its candidates on its
own, so candidates found in several repositories must be the same image.
Otherwise `--tag` lists them and asks you to choose:

```yaml
  prod:
    release: {flag: tag, from: staging-eu}   # release staging-eu's candidates
```

## Promote between stages

A stage can deploy another stage's images instead of building:

```yaml
stages:
  dev: {}                         # builds
  uat:
    promote: {from: dev}
  prod:
    promote: {from: uat}
    release: {repository: {project: my-release-project, repository: releases}}
```

`promote` is optional: a stage without it builds, so a project without a
`dev` or `uat` simply leaves it out. A promoting stage builds nothing, so it
needs no source bucket, build account or Cloud Build: only the repository its
copies go to (`provider.artifact_repository`). A service or job with its own
`image:` (or targeted by `--image`) deploys that image, not a promotion.

For each other service and job, the image comes from the same service or job
of the source stage (each from its own, even when two would share a build
here), from what it **serves**: its one revision with all the traffic as
Cloud Run reports it serving (a preview with no traffic is ignored, and a
rollout still in progress is waited for), with that revision's provenance,
read together.

| Deploy | Image |
|---|---|
| `runway deploy --stage uat` | exactly what dev serves |
| `--tag-rc` or `--tag` (version V) | the `V` this stage serves already (a re-run), else what dev serves if it is `V` or a `V-RCn`, else what dev serves if it was built from the commit you deploy |
| `promote: {from: dev, commit: checkout}` | the image dev ran built from the commit you deploy, from its revisions (see [Promote the tagged commit](#promote-the-tagged-commit)) |

Otherwise the deploy stops before changing anything and says why: dev is not
deployed, splits its traffic (a canary: settle it first with `runway traffic
--promote` or `--set`), serves something that is not a candidate of V, or
was built from another commit. A `V` already in a release repository other
stages share is not evidence: if it tags another image than the one chosen,
the deploy stops (bump the version, or give the stage its own release
repository). Workloads released to the same image must be the same image.

The image is copied with the same digest into the stage's own repository
(its build repository, or its release repository with a flag, where it is
tagged), and the stage deploys that copy. `runway plan` shows that
reference, why it was chosen and the copies and tags deploy makes; offline,
it says the image is looked up when deploying. `--force-build` is refused in
a stage that promotes.

**What a revision carries.** Every revision runway deploys records the commit
its image comes from and its release (`runway.dev/source`,
`runway.dev/release` on the revision): that is the evidence a promotion
reads. A promoted stage records its image's commit, not the checkout's; when
it is unknown (an image deployed before revisions carried it), the stage's
commit record is removed instead, so older deploys are not detected until a
deploy records one. A new commit is a new revision even when the image is
unchanged, and `plan` shows it.

**Publishing any image.** `--tag`/`--tag-rc` publish whatever the stage
deploys: a build, a promotion, or a configured image (which needs a
`release.repository`, having no repository of its own).

## Promote the tagged commit

To release exactly the commit a git tag points to, with the image that ran
in UAT for it:

```yaml
stages:
  dev: {}
  uat:
    promote: {from: dev}
  prod:
    promote: {from: uat, commit: checkout}
```

Each deploy of prod promotes the image uat ran built from the commit being
deployed: the checkout's HEAD, or `RUNWAY_SOURCE_COMMIT` (with
`RUNWAY_SOURCE_TIME`) in CI, which on a tag pipeline is the tagged SHA. It is
found in uat's Ready revisions, newest first, even after uat moved on (each
revision records its commit). With or without `--tag`.

There is no fallback: if uat never ran an image built from that commit, or
ran several different ones, the deploy stops and says so. A matching release
tag (`1.2.0-RC1`) never stands in for the commit. A job keeps no history, so
its current image must be built from the commit. Without `commit: checkout`,
a matching candidate still wins, as in the RC flow above (what was tested is
released even if main moved on).

**The version.** `--tag` names the release after the changelog. On a commit
tagged with a version (`v1.2.0` or `1.2.0`, a candidate such as
`v1.2.0-rc.1` included; GitLab's `CI_COMMIT_TAG`, GitHub's tag ref, else
`git tag --points-at`), the changelog's version must be that one, or the
deploy stops before changing anything.

```yaml
# GitLab CI
release:
  script: runway deploy --stage prod --tag
  rules: [{if: $CI_COMMIT_TAG}]
```

## Who pushes

Cloud Build pushes builds as `provider.build_service_account`. The registry
work runway does itself (finding, copying and tagging images for promotions
and releases) uses your credentials, or `provider.push_service_account`:

```yaml
provider:
  push_service_account: gitlab-ci@my-ci-project.iam.gserviceaccount.com
```

It can be your CI's account or one you create; runway does not create it or
grant it anything. Your credentials need
`roles/iam.serviceAccountTokenCreator` on it (unless it is the account
`impersonate_service_account` names), and it needs
`roles/artifactregistry.reader` on the repositories images come from and
`roles/artifactregistry.writer` on those they go to.

## In CI

```yaml
# GitLab CI
stages: [candidate, release]

release-candidate:
  stage: candidate
  script: runway deploy --tag-rc          # staging
  rules: [{if: $CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH}]

release:
  stage: release
  needs: [release-candidate]              # never before this pipeline's candidate exists
  script: runway deploy --tag             # prod: the latest RC, no build
  rules: [{if: $CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH, when: manual}]
```

The release waits for the candidate of the same pipeline: started earlier,
it could release an older candidate, or fail when there is none yet.

See [CI/CD](ci-cd.md) for credentials.

## Permissions

Tagging needs `roles/artifactregistry.writer` on the repository. With a
release repository, the deployer also needs `roles/artifactregistry.reader`
on the repository images are copied from; in another project, the Cloud Run
service agent (`service-PROJECT_NUMBER@serverless-robot-prod.iam.gserviceaccount.com`)
needs `roles/artifactregistry.reader` on the release repository to pull
(runway does not grant it). See [Permissions](permissions.md).
