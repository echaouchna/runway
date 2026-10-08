# Commands

`runway --help` (or `runway -h`) shows the logo as colored half-block terminal cells
above the command list. Help of every command (`runway deploy --help`,
`runway help preview list`) and usage errors have colored headers, flags and
values. Both follow the same color rules as other output (below), with a
monochrome ASCII logo when colors are off. Use `runway --color always --help`
to force the blue runway monogram and light wordmark.

Colors (`--color auto`, the default) are on for terminals and for CI job logs
that display them (GitLab CI, GitHub Actions, Gitea/Forgejo Actions,
Buildkite, CircleCI, Azure Pipelines), except for output redirected to a
file. `NO_COLOR` (any non-empty value) turns them off, then
`CLICOLOR_FORCE` or `FORCE_COLOR` (not `0`) turn them on anywhere, and
`TERM=dumb` turns them off. `--color always|never` (or `RUNWAY_COLOR`) beats
all of these. JSON output is never colored. A container only has a terminal
with `docker run -t`.

Global flags: `-c/--config <path>` (alias `--file`, env `RUNWAY_CONFIG`): the
configuration file, under any name (`-c deploy/api.yaml`), or a directory
containing `runway.yaml` / `runway.yml`; by default `./runway.yaml`, then
`./runway.yml`. Paths inside the file (`source`, `dockerfile`) are relative to
its directory, so one repository can hold several configurations.
`runway -c api.yaml init` writes `api.yaml`. Other global flags:
`-o/--output text|json`, `-q/--quiet` (no progress), `-v` (debug logs),
`--color auto|always|never` (env `RUNWAY_COLOR`; `auto` colors terminals
and CI logs, see above),
`--impersonate-service-account SA` or `DELEGATE,...,SA` (env
`RUNWAY_IMPERSONATE_SERVICE_ACCOUNT`; overrides
`provider.impersonate_service_account`).
`--stage` can also come from `RUNWAY_STAGE`. Results go to stdout; progress
and warnings to stderr, so JSON output stays parseable.

| Command | Description |
|---------|-------------|
| `runway init [--app] [--project] [--region] [--image] [--dir]` | Scaffold `runway.yaml` and a runnable example. Never overwrites files. |
| `runway validate [--stage S]` | Offline validation of all stages. |
| `runway explain [KEY] [--plain \| --agent]` | Offline: learn `runway.yaml`. In a terminal, a browser of every key (what it does, default, example, rules, docs link), with what your file sets: where each key is set, its resolved value per stage, and validation problems. `/` searches, `f` shows only your keys, `n` jumps to the next problem. With a key (`service.vpc`, `services.web.memory`, `stages.prod.service.env`, `variables`), prints it; in a pipe, lists every key; `--agent` prints Markdown for LLMs and coding agents; `-o json` for scripts. See [Learn the configuration](#learn-the-configuration). |
| `runway doctor --stage S` | Read-only checks: config, ADC principal, project, enabled APIs, deployer permissions (`testIamPermissions`), service accounts and `actAs`, repository, bucket, secrets (version state and accessor grants), ownership of an existing service. |
| `runway plan --stage S [--image I] [--offline] [--preview NAME \| --traffic N]` | Field-level diff against the live service (including the traffic split), image resolution, build and access changes. Makes no mutations. `--offline` contacts nothing. |
| `runway deploy --stage S [--image I] [--timeout 10m] [--build-timeout 20m] [--force-build] [--adopt] [--retries N] [--retry-delay 5s] [--tag \| --tag-rc] [--preview NAME \| --traffic N]` | Create the runtime identity and grants, build if needed, create/update, wait for readiness, bind tags, configure IAP, reconcile access, print the URL. `--preview NAME`: no traffic, a URL for the new revision. `--traffic N`: canary with N% of the traffic. `--retries` overrides `retry.attempts` (`--retries 0` disables retries). `--tag`/`--tag-rc`: tag a release (see [Releases](configuration.md#releases)); `--stage` is optional when runway.yaml maps a stage to the flag. |
| `runway preview list\|delete NAME...\|prune --stage S [--yes] [--delete-revisions]` | Branch previews: list them, delete some (by branch or tag), or prune those of branches merged into the base branch or deleted from the remote (`--base`, `--remote`, `--only-merged`, `--no-fetch`). Without `--yes`: dry run. See [Previews](traffic.md#managing-previews). |
| `runway traffic --stage S [--promote \| --set TARGET=PCT ... \| --remove-tag NAME ...]` | Without options: the split and the tagged URLs. `--promote`: 100% to the canary. `--set`: an explicit split (targets: `latest`, a tag, a revision). `--remove-tag`: drop a preview URL. Writes only the traffic (no new revision). |
| `runway info --stage S` | URL, status, serving/latest revision, image, traffic, access. |
| `runway logs --stage S [--since 10m] [--limit 200] [--follow] [--severity WARNING] [--include-requests]` | Application logs (request logs excluded by default; Google audit logs always excluded). |
| `runway run-job NAME --stage S [--wait] [--timeout 1h]` | Run a job of the stage now. `--wait` follows the execution and fails if it fails (with the log URL). |
| `runway completions bash\|zsh\|fish\|nushell\|xonsh\|elvish\|powershell` | Print a completion script; `runway completions --help` shows where to install it for each shell. |
| `runway undeploy --stage S --preview NAME [--yes]` | Only removes a preview's URL (tag); nothing is deleted. |
| `runway undeploy --stage S [--yes] --release-urls` | Also deletes the stage's `NAME.cloud.run` custom URLs, which are kept by default (a freed name can be claimed by anyone). |
| `runway undeploy --stage S --orphans [--yes]` | Only the services and jobs runway deployed for this stage that runway.yaml no longer lists, and their schedules. |
| `runway undeploy --stage S [--yes] [--delete-images] [--retries N]` | Without `--yes`: prints what would be deleted and what is kept (read-only). With `--yes`: deletes the Cloud Run service, revokes the grants of the runtime service account and deletes it **only if runway created it for this app and stage** and nothing else runs as it, then (opt-in) the app's images. Never deletes buckets, never disables APIs, keeps shared build infrastructure and everything that existed before, and lists it all. |
| `runway describe [--stage S] [--format ascii\|mermaid] [--diagram-only]` | Offline: a diagram of the stack (pure ASCII, or Mermaid wrapped in a code block) and an explanation (build, access, identity, configuration, storage, APIs, deployment order, failure handling). `-o json` returns both. See [Stack diagrams](#stack-diagrams). |

With several services and jobs (see [Configuration](configuration.md#several-services-jobs-and-schedules)),
every command that takes `--stage` also takes `--only NAME|PATH` (repeat or
separate with commas; `RUNWAY_ONLY` in CI). `info` and `logs` need it when
there are several services (`logs` also reads a job's logs); `traffic --set`
needs it to name one service.

## Learn the configuration

`runway explain` opens a full-screen browser of `runway.yaml`: keys on the
left (service keys grouped by topic), what the selected one does on the
right, with its default, an example, its rules and a link to these docs.
Next to a `runway.yaml`, keys it sets are marked ●, problems ✗, and each
explanation ends with where the key is set and its value in every stage.

`↑`/`↓` move in the keys; `→` or `Enter` moves to the explanation, where
`↑`/`↓` scroll it and `←` or `Esc` come back (`→` first unfolds a folded
key, `Space` folds and unfolds). The focused panel has a bright border.

```console
$ runway explain                          # browse (q quits, ? lists the keys)
$ runway explain stages.prod.service.memory
service.memory  string  default 512Mi
…
In your file
  service.memory: 512Mi
  stages.prod.service.memory: 1Gi
Per stage (resolved)
  dev   hello-python-dev   512Mi
  prod  hello-python-prod  1Gi
$ runway explain service.memroy           # did you mean service.memory?
```

**For AI agents.** `runway explain --agent` prints the reference as
Markdown: how keys and layers work, then every key with its type, default,
rules, a YAML example and a docs link, plus what the current `runway.yaml`
sets, its value in each stage and its validation problems. With a key, only
that key and the keys below it (`runway explain --agent service.vpc`). Give
it to an assistant before it edits `runway.yaml`, then let it check its work
with `runway validate`.

```console
$ runway explain --agent > runway-reference.md
$ runway explain --agent services.web.secrets   # what an agent needs for one change
```

A key can be written as in the file: `defaults.env`, `services.web.vpc`,
`jobs.migrate.tasks`, `service.secrets.DATABASE_URL.version`. Topics explain
the file as a whole: `start`, `layers` (stages and precedence), `variables`,
`names`.

## Stack diagrams

`runway describe` reads only `runway.yaml`. In a terminal the ASCII diagram
and the explanation are colored: the service stands out, permissions and
what they apply to are highlighted, names and values are cyan, lines and
boxes are dimmed. Without colors (a file, a pipe, `NO_COLOR`,
`--color never`) the same text is plain Markdown, ready to paste into a
README or a pull request.

`--format mermaid` prints a Mermaid flowchart, rendered by GitHub, GitLab
and most documentation tools. To view it from the terminal, render it with
[mermaid-cli](https://github.com/mermaid-js/mermaid-cli)
(`brew install mermaid-cli`, or `npx -p @mermaid-js/mermaid-cli mmdc`):

```sh
runway describe --stage prod --format mermaid -o json | jq -r .diagram > stack.mmd
mmdc -i stack.mmd -o stack.png -t dark -b transparent
```

Then open `stack.png`, or show it in the terminal where images are supported:
`kitten icat stack.png` (kitty, Ghostty), `wezterm imgcat stack.png`
(WezTerm), `imgcat stack.png` (iTerm2), or `chafa stack.png` in any
terminal (character art). Text-only Mermaid renderers do not support the
node shapes runway uses; the ASCII format is the terminal view.

## Plans and image digests

A plan is **exact** only when everything is known: the live service and its
IAM policy were read and the image digest is known. Before a source build the
plan shows `digest NOT known yet` and `exact: false`; if an image for the
identical source was already built, its digest is used and the build is
skipped. For `service.image`, mutable tags are resolved to a digest through
the registry API; if that is impossible the plan says so and Cloud Run
resolves the tag at revision creation.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | unexpected error |
| 2 | invalid command-line usage |
| 3 | invalid configuration |
| 4 | missing credentials, permissions, APIs or prerequisite resources (also `doctor` failures) |
| 5 | Cloud Build failed |
| 6 | Cloud Run rejected the service or the revision is unhealthy |
| 7 | refused to modify a resource runway does not own |
| 8 | timed out |
| 9 | service not found (`info`) |
| 130 | interrupted |

With `-o json`, errors are printed to stdout as
`{"error": {"kind", "message", "hints", "exit_code"}}`.
