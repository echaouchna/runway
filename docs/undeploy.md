# Undeploying

`runway undeploy --stage S` is stateless like `deploy`: it decides what may be
deleted from live ownership markers, never from a record of past deploys.

| Resource | What `undeploy --yes` does |
|----------|----------------------------|
| Cloud Run service | deleted if its labels match the app and stage (refused otherwise); its revisions, tag bindings, invoker and IAP policy go with it |
| Runtime service account | deleted only if its description carries runway's marker for this app and stage (`managed-by=runway app=… stage=… role=runtime`, written when runway created it) and no other service in the region runs as it; its `identity.roles` grants are revoked first |
| Runtime service account without the marker | kept, with its grants (runway cannot tell whether they predate it) |
| Buckets (`buckets:`, volumes, build sources) | kept: they hold data |
| APIs | kept enabled |
| Artifact Registry repository, build service account | kept: shared by every stage |
| The app's images | kept, unless `--delete-images` |
| Tag keys/values, IAP service agent | kept |

Without `--yes` the same table is printed for the live state and nothing
changes. Re-running after a partial failure continues where it stopped.
