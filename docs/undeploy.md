# Undeploying

`runway undeploy --stage S` is stateless like `deploy`: it decides what may be
deleted from live ownership markers, never from a record of past deploys.
Without `--yes` it prints what would be deleted and kept, and changes
nothing; re-running after a partial failure continues where it stopped.

```console
$ runway undeploy --stage dev                 # the plan: deleted and kept
$ runway undeploy --stage dev --yes           # the whole stage
$ runway undeploy --stage dev --only web --yes    # one service or job
$ runway undeploy --stage dev --orphans --yes     # those runway.yaml no longer lists
$ runway undeploy --stage dev --preview feature/login --yes   # one preview URL only
```

| Resource | What `undeploy --yes` does |
|----------|----------------------------|
| Cloud Run services and jobs | deleted if their labels match the app and stage (refused otherwise); revisions, executions, tag bindings, invoker and IAP policy go with them |
| Custom domains | with the whole stage, first: runway's load balancer, certificates, NEGs and backends, its routes in an existing load balancer (the load balancer is kept), domain mappings, and DNS records whose data is what runway set. `NAME.cloud.run` custom URLs are kept unless `--release-urls`. With `--only` or `--orphans`, kept and reported. See [Custom domains](domains.md#removing-domains) |
| Cloud Scheduler jobs | with the whole stage, every one runway created for it; with `--only`, those calling what is removed, if runway created them (description marker) |
| Runtime service account | deleted only if runway created it for this app and stage (marker `managed-by=runway app=… stage=… role=runtime` in its description) and no other service or job in the region runs as it, whether runway.yaml lists it or not; the roles every removed workload declared are revoked first |
| Runtime service account without the marker | kept, with its grants (runway cannot tell whether they predate it) |
| Scheduler service account | with the whole stage, deleted if runway created it |
| Buckets (`buckets:`, volumes, build sources) | kept: they hold data |
| Secrets | kept: they hold values people added |
| APIs | kept enabled |
| Artifact Registry repositories, build service account | kept: shared by every stage |
| Images | kept, unless `--delete-images` |
| Tag keys/values, IAP service agent | kept |

Order: custom domains first (nothing routes to what is being deleted), then
schedules (nothing calls it), then every service and job, then runtime accounts, then images. An account shared by two
services is only deleted once neither runs.

**Removed from runway.yaml.** A service or job you delete from the file is
not deleted by `deploy`, which lists it. `--orphans` deletes the services and
jobs runway deployed for the stage under a name the file no longer lists, and
the schedules it no longer lists.

**Previews.** `--preview NAME` only removes that preview's URL from each
service, and deletes the preview copies of jobs: nothing else. To remove
several, see [Managing previews](traffic.md#managing-previews).
