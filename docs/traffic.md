# Previews, canaries and traffic

runway derives the traffic split from the live service on every deploy (no
state file):

| Deploy | New revision | Everything else |
|--------|--------------|-----------------|
| `runway deploy` | 100% (follows `latest`) | preview URLs kept, pointing at their revisions |
| `runway deploy --preview NAME` | 0%, URL `https://NAME---SERVICE-...run.app` | what was serving is pinned to its revision(s) and keeps serving |
| `runway deploy --traffic 10` | 10%, URL `https://canary---...` | the rest keeps its current split, scaled to 90% |

A preview (or canary) always runs in **its own revision**, even when its code
and configuration are identical to what serves production (a template
annotation `runway.dev/preview: NAME`, or `runway.dev/canary`, makes the
revision distinct). Without that, its URL would point at the production
revision. Only the main deploy (no flag) gets the traffic, and `--traffic N`
changes how much.

`NAME` can be a branch name: `feature/Login_v2` becomes the tag
`feature-login-v2` (shortened with a hash when the tag and the service name
would exceed 46 characters). Redeploying the same branch moves its URL to the
new revision. A deploy whose revision template did not change (for example a
traffic-only or ingress-only change) sends the live template back untouched,
so Cloud Run creates no revision.

**Nothing is redeployed when the URL already serves it.** Before creating a
revision, runway reads the revision behind the URL the deploy changes: the
main URL (one revision serving 100%), the preview's tag, or the canary. If
that revision already runs the same image and configuration (resources,
scaling, environment, secrets, probes, volumes, sidecars, identity), it keeps
serving. No revision is created and no traffic moves; only what really
differs is applied, such as a new canary percentage or ingress. For example:

- Redeploying `feature/login` after another branch was previewed leaves its
  URL on its revision.
- A main deploy after a preview, with production's code unchanged, changes
  nothing.

`plan` shows the same result (a note names the revision kept), and it shows
`+ revision` when a preview or canary needs a revision of its own although
its configuration matches.

Typical CI (GitLab):

```yaml
deploy-branch:                # every branch: its own URL, no traffic
  script: runway deploy --stage prod --preview "$CI_COMMIT_REF_NAME"
  rules: [{ if: '$CI_COMMIT_BRANCH != $CI_DEFAULT_BRANCH' }]
deploy-main:                  # merged: the release gets the traffic,
  variables: { GIT_DEPTH: 0 } # then the previews of merged branches go
  script:
    - runway deploy --stage prod
    - runway preview prune --stage prod --yes --delete-revisions
  rules: [{ if: '$CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH' }]
stop-branch:                  # one branch, by hand
  script: runway preview delete --stage prod "$CI_COMMIT_REF_NAME" --yes
  when: manual
```

Canary on `main`: `runway deploy --stage prod --traffic 10`, watch, then
`runway traffic --stage prod --promote` (or `--traffic 50` first, or roll back
with `runway traffic --stage prod --set <previous-revision>=100`).

Previews share the service: service-level settings (ingress, IAP, public
access, tags) and grants come from the branch's `runway.yaml` like any deploy.
The first deploy of a stage serves the deployed revision whatever the flag
(there is nothing else to serve).

## Managing previews

```console
$ runway preview list --stage prod
$ runway preview delete --stage prod feature/login            # dry run
$ runway preview delete --stage prod feature/login --yes      # remove its URL
$ runway preview prune --stage prod                           # dry run
$ runway preview prune --stage prod --yes --delete-revisions
```

- `delete` accepts branch names (`feature/login`) or tags (`feature-login`).
  `runway undeploy --preview NAME --yes` and `runway traffic --remove-tag` do
  the same for one name.
- `prune` reads the branches with git in the directory of `runway.yaml`
  (after `git fetch --prune`, unless `--no-fetch`) and removes the previews
  whose branch was **merged** into the base branch (`--base`, default: the
  remote's default branch, then `$CI_DEFAULT_BRANCH`, then `main`) or **no
  longer exists** on the remote (`--only-merged` keeps those). Merge detection
  needs the full history (`GIT_DEPTH: 0` in GitLab, `fetch-depth: 0` in
  GitHub Actions); deleted branches are detected in shallow clones too.
- Only previews created by `deploy --preview` are pruned (their revision
  carries a `runway.dev/preview` marker); `canary` and other tags are kept.
- Without `--yes` nothing changes; all removals happen in one traffic update
  (no new revision).
- `--delete-revisions` also deletes each preview's revision once nothing
  points at it: never the latest revisions, never a revision that still has
  traffic or another tag, and only if it carries the preview marker (the
  delete is conditioned on the revision's etag). Without it, preview
  revisions stay in the service's revision list.
