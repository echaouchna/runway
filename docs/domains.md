# Custom domains

Serve your services on your own domain (`shop.example.com`,
`example.com/api`) instead of `*.run.app`. runway creates everything it needs
(the load balancer, its certificates, DNS records), keeps it in line with
runway.yaml on every deploy, and removes it when you remove the domains or
undeploy.

```yaml
domains:
  dns: {zone: example-com}          # optional: runway writes the DNS records

service:
  domains: [shop.example.com]
```

```console
$ runway plan --stage prod      # what will be created, and the DNS records
$ runway deploy --stage prod
```

## Choose how domains are served

| `domains.mode` | What runway creates | Use it when |
|---|---|---|
| `load-balancer` (default) | a global external Application Load Balancer for the app and stage: IP address, serverless NEGs, backend services, URL map, HTTPS proxy, HTTP-to-HTTPS redirect, Certificate Manager certificates | production; paths routed to different services; previews on your domain. About $18/month per stage plus traffic |
| `existing-load-balancer` | only its NEGs and backend services, and its own host rules in a URL map a platform team owns | your organization runs a shared load balancer |
| `domain-mapping` | Cloud Run domain mappings | quick setups. Preview feature, [10 regions](https://cloud.google.com/run/docs/mapping-custom-domains#limitations), whole hosts only, domain verification needed, not recommended for production by Google |

A name ending in `.cloud.run` (`my-shop.cloud.run`) is a Cloud Run
[custom URL](https://cloud.google.com/run/docs/custom-urls) in every mode: no
DNS, no certificate, no load balancer (preview feature).

The `domains` block is stage-wide; a stage's block replaces the global one.
Domains themselves belong to services, usually set per stage:

```yaml
domains:                                   # every stage, unless it has its own block
  mode: load-balancer
  dns: {zone: example-com, project: my-dns-project}   # project: default provider.project

service: {}
services:
  api: {}

stages:
  prod:
    service:
      domains: [example.com, www.example.com]
    services:
      api:
        domains:
          - api.example.com
          - example.com/api                # /api and /api/* go to this service
  dev:
    service: {domains: [dev.example.com]}
    services:
      api: {preview_domain: "*.preview.example.com"}   # previews on your domain
  demo:
    domains: {mode: domain-mapping}        # replaces the global block for this stage
    service: {domains: [demo.example.com]}
```

A host routes to the service that lists it without a path; paths of the same
host can go to other services. A host or path belongs to one service.

## Certificates

With a load balancer, certificates are Google-managed Certificate Manager
certificates with **DNS authorization**: each domain gets a `CNAME
_acme-challenge.<domain>` record, and the certificate is issued as soon as
that record exists, even before the domain points at the load balancer. You
can therefore set everything up, wait until `plan` no longer reports a
certificate as provisioning, then switch the domain's A record: no downtime.
Wildcards (preview domains) work the same way.

`plan` and `deploy` say which certificates are still provisioning.

## DNS records

With `domains.dns`, runway creates the records in that Cloud DNS managed zone
(give the zone's name, not its DNS name; `project` when the zone lives in
another project):

- an A record per host, to the load balancer's IP;
- the DNS authorization CNAME of each certificate;
- with domain mappings, the records Cloud Run asks for.

Without a zone, or when runway may not read or write it, nothing fails:
`plan` and `deploy` print every record with **ELSEWHERE** and the reason, for
you or another team to create at your DNS provider.

DNS records cannot carry an owner, so runway is careful with them:

- it never changes a record that exists with other data (it tells you);
- it deletes a record only when its data is exactly what runway set (a host
  you repointed elsewhere is left alone).

## An existing load balancer

```yaml
domains:
  mode: existing-load-balancer
  load_balancer:
    url_map: shared-lb                # in the deployment project
    certificate_map: shared-certs     # optional: runway adds its certificates there
    address: 203.0.113.10             # optional: the IP its A records point to
```

runway creates its serverless NEGs and backend services, and adds one host
rule and path matcher per host to the URL map, marked with
`managed-by=runway app=… stage=…` in their description. Everything else in the
URL map (other host rules, the default service) is kept as it is; the update
uses the URL map's fingerprint, so a concurrent change is never overwritten.
A host already routed by a rule runway did not create is refused.

Without `certificate_map`, certificates are the platform team's business.
Without `address`, runway cannot write A records and says so.

## Domain mappings and custom URLs

Domain mappings need the domain to be
[verified](https://cloud.google.com/run/docs/mapping-custom-domains#add-verified)
in Search Console by the deploying account. Cloud Run then gives the records
to create (shown by `plan`/`deploy`, or written to the zone); the certificate
is issued once they resolve.

Custom URLs (`NAME.cloud.run`, 6 to 63 characters) are first come, first
served. A freed name can be claimed by anyone at once, so `undeploy` keeps
them unless you pass `--release-urls`. Removing one from runway.yaml and
deploying deletes it.

## Previews on your domain

```yaml
services:
  api:
    preview_domain: "*.preview.example.com"
```

`runway deploy --preview feature/login` then also answers on
`https://feature-login.preview.example.com` (the preview's tag, as on its
`run.app` URL). runway adds a wildcard certificate and a serverless NEG whose
URL mask routes `<tag>.preview.example.com` to the service's tagged revisions.
Load balancer modes only; with several services, give each its own wildcard.

## Removing domains

- Remove a domain from runway.yaml: the next full `deploy` updates the routes
  and deletes what only that domain used (certificate, DNS authorization,
  backend, records runway set). With `--only`, the routes (and the A records
  of hosts no longer routed) follow runway.yaml, but nothing else is deleted
  until a deploy of everything.
- Remove every domain: the next full deploy deletes runway's load balancer, or
  takes runway's rules out of the existing one (the load balancer itself is
  kept).
- `runway undeploy --stage S --yes` does the same before deleting the
  services. With `--only` or `--orphans`, domains are left (and reported):
  remove them from runway.yaml and deploy.

runway records on the service (annotation `runway.dev/domains`) what it
manages: a load balancer, certificates, domain mappings, the URL and
certificate maps it added routes to, and the DNS zone. Removals look only
for those, so leaving the `existing-load-balancer` mode still cleans up, and
a stage with domain mappings needs no Compute Engine permission. DNS records
are deleted before what they point at: if that fails, nothing else is
removed and the next run tries again.

## Good to know

- Private services: requests through the load balancer carry an ID token for
  your domain, not `run.app`; add the domain to `custom_audiences` when
  callers authenticate with ID tokens.
- To force traffic through the load balancer, set
  `ingress: internal-and-cloud-load-balancing`.
- Load balancer and certificate changes take a few minutes to propagate.
- Permissions: see [Permissions](permissions.md).
