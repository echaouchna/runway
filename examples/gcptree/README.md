# gcptree on Cloud Run

Example deployment configuration for gcptree in `europe-west1`. Project IDs
and group emails are placeholders; replace them with your own values and
copy `runway.yaml` into your application's source directory before deploying.

```sh
runway describe              # diagram and explanation of the stack
runway doctor --stage prod   # credentials, APIs, permissions
runway plan --stage prod     # what is done, what is pending (read-only)
runway deploy --stage prod
```

`deploy` creates what is missing, in this order (each step is skipped when
already done, and retried on failure):

1. enable the missing APIs (Cloud Run, Cloud Build, Artifact Registry,
   Storage, IAM, IAP, Logging, Resource Manager, BigQuery, Telemetry);
2. create the cache bucket `my-gcp-project-gcptree-cache` and the build
   source bucket (uniform access, public access prevention, `europe-west1`);
3. create the Docker repository `europe-west1/runway`;
4. create the build service account `runway-build@…` and the runtime service
   account `gcptree-run@…`;
5. grant the build account its roles (log writer, repository writer, source
   reader), then the runtime account: `bigquery.dataViewer` on the billing
   dataset, `bigquery.jobUser` on the job project, `storage.objectUser` on the
   cache bucket, `telemetry.tracesWriter`;
6. build the image with Cloud Build and buildpacks (skipped when the source is unchanged);
7. deploy `gcptree-prod` with all `GCPTREE_*` variables and IAP enabled;
8. let the IAP service agent invoke the service and grant the two groups
   `roles/iap.httpsResourceAccessor`; keep `allUsers` out.

Values used in several places (billing project, dataset, table, job project,
FinOps group) are defined once under `vars:`; the cache bucket name is defined
once under `buckets:` and referenced as `${buckets.cache}` in the grant and in
`GCPTREE_CACHE_BUCKET`.

There is no Dockerfile: the image is built with Google Cloud buildpacks
(`npm start`). `.runwayignore` keeps local tool folders out of the upload.
IAP access is granted to `developers@example.com` and
`admins@example.com`.

`runway undeploy --stage prod` shows what would be removed (the service and the
`gcptree-run` account with its grants) and what is kept (the cache bucket and
its data, APIs, build resources, images); add `--yes` to apply.

Before the first deploy, check
`runway doctor`: the deployer needs `resourcemanager.projects.setIamPolicy`
and `cloudbuild.builds.create` on the deployment project,
`resourcemanager.projects.setIamPolicy` on the job project and
`roles/bigquery.dataOwner` on the billing dataset (or an administrator grants
those two roles once).

Set the BigQuery custom quota and budget alert manually; runway does not
create them.
