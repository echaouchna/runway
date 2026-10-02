# hello-python

A dependency-free Python HTTP service used to demonstrate source deployments.

Run locally:

```sh
PORT=8080 python3 main.py
curl localhost:8080/
```

Or with Docker:

```sh
docker build -t hello-python . && docker run --rm -p 8080:8080 hello-python
```

Deploy (after completing the prerequisites in [Getting started](../../docs/getting-started.md#prerequisites) and
replacing `my-gcp-project` in `runway.yaml`):

```sh
runway validate
runway doctor --stage dev
runway plan --stage dev
runway deploy --stage dev
runway info --stage dev
runway logs --stage dev --since 10m
```

The service is private. Call it with an identity token:

```sh
curl -H "Authorization: Bearer $(gcloud auth print-identity-token)" "$(runway info --stage dev -o json | jq -r .url)"
```
