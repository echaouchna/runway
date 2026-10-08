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
4. `runway deploy --tag`: prod deploys **the image of the latest RC** of
   `1.4.2`, tags it `1.4.2`, and builds nothing.

What was tested is what is released, whatever changed since: the changelog
itself is part of the build context, and base images such as
`gcr.io/buildpacks/builder:latest` are republished upstream, so a rebuild
would give another image. Re-running `--tag` reuses the `1.4.2` tag. Without
a release candidate of the version, `--tag` fails and says how to make one.
`--force-build` is refused with `--tag` then.

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

**Several `tag-rc` stages.** Each repository numbers its candidates on its
own, so candidates found in several repositories must be the same image.
Otherwise `--tag` lists them and asks you to choose:

```yaml
  prod:
    release: {flag: tag, from: staging-eu}   # release staging-eu's candidates
```

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
