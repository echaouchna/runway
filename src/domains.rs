//! Custom domains: a global external Application Load Balancer runway owns,
//! routes added to an existing one, or Cloud Run domain mappings (and
//! `NAME.cloud.run` custom URLs), with Certificate Manager certificates and
//! DNS records.
//!
//! [`Engine::reconcile`] reads the live state and, with `apply`, changes what
//! differs: `plan` and `deploy` run the same code. Every resource carries
//! runway's ownership marker (a description marker on compute resources,
//! labels elsewhere); DNS records, which cannot carry one, are only changed
//! or deleted when their data is what runway sets.

use crate::config::{DnsZone, DomainMode, ExistingLoadBalancer, Resolved};
use crate::error::{Error, ErrorKind, Result};
use crate::gcp::domain_mapping::DomainMappings;
use crate::gcp::{GaxError, api_error, is_not_found, status_code};
use google_cloud_certificatemanager_v1::client::CertificateManager;
use google_cloud_certificatemanager_v1::model::{
    Certificate, CertificateMap, CertificateMapEntry, DnsAuthorization, certificate,
};
use google_cloud_compute_v1::client::{
    BackendServices, GlobalAddresses, GlobalForwardingRules, RegionNetworkEndpointGroups,
    TargetHttpProxies, TargetHttpsProxies, UrlMaps,
};
use google_cloud_compute_v1::model::{
    Address, Backend, BackendService, ForwardingRule, HostRule, HttpRedirectAction,
    NetworkEndpointGroup, NetworkEndpointGroupCloudRun, PathMatcher, PathRule, TargetHttpProxy,
    TargetHttpsProxy, UrlMap, address, backend_service, forwarding_rule, http_redirect_action,
    network_endpoint_group,
};
use google_cloud_dns_v1::client::ResourceRecordSets;
use google_cloud_dns_v1::model::ResourceRecordSet;
use google_cloud_gax::error::rpc::Code;
use google_cloud_lro::Poller;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Service annotation (on the grants holder) recording how domains were
/// served, so that removing them from runway.yaml can still clean up.
pub const ANNOTATION_DOMAINS: &str = "runway.dev/domains";

// ------------------------------------------------------------- desired --

/// What runway.yaml asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Desired {
    pub mode: DomainMode,
    /// `<app>-<stage>`: the prefix of the load balancer's resources.
    pub prefix: String,
    pub backends: Vec<BackendSpec>,
    pub hosts: Vec<HostSpec>,
    /// Hosts (and preview wildcards) runway gets certificates for.
    pub cert_domains: Vec<String>,
    /// `(domain, Cloud Run service ID)`: domain mappings and custom URLs.
    pub mappings: Vec<(String, String)>,
    pub dns: Option<DnsZone>,
    pub existing: Option<ExistingLoadBalancer>,
}

/// A serverless NEG and the backend service using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSpec {
    pub service_id: String,
    pub neg: String,
    pub backend: String,
    /// `<tag>.preview.example.com`: a preview backend.
    pub url_mask: Option<String>,
}

/// The routes of one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    pub host: String,
    pub default_backend: String,
    /// `(paths, backend)`.
    pub paths: Vec<(Vec<String>, String)>,
}

impl Desired {
    pub fn load_balanced(&self) -> bool {
        self.mode != DomainMode::DomainMapping && !self.hosts.is_empty()
    }
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.mappings.is_empty()
    }
}

/// First 8 hex digits of the SHA-256 of `s`: stable resource name suffixes.
pub fn short(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether a stage has anything to do with custom domains.
pub fn configured(r: &Resolved) -> bool {
    r.services()
        .any(|d| !d.service.domains.is_empty() || d.service.preview_domain.is_some())
}

/// The desired domains of a stage.
pub fn desired(r: &Resolved) -> Desired {
    let services: Vec<&crate::config::Deployment> = r.services().collect();
    desired_of(r.first(), &services)
}

/// The desired domains of `services` (`first` carries the stage settings).
pub fn desired_of(
    first: &crate::config::Deployment,
    services: &[&crate::config::Deployment],
) -> Desired {
    let settings = &first.domains;
    let mut out = Desired {
        mode: settings.mode,
        prefix: crate::naming::service_id(&first.app, &first.stage),
        dns: settings.dns.clone(),
        existing: settings.existing.clone(),
        ..Default::default()
    };
    let add_backend = |out: &mut Desired, b: BackendSpec| {
        if !out.backends.iter().any(|x| x.backend == b.backend) {
            out.backends.push(b);
        }
    };
    for d in services {
        let sid = &d.service_id;
        let main = BackendSpec {
            service_id: sid.clone(),
            neg: format!("{sid}-neg"),
            backend: format!("{sid}-be"),
            url_mask: None,
        };
        for e in &d.service.domains {
            if e.is_cloud_run_url() || settings.mode == DomainMode::DomainMapping {
                out.mappings.push((e.host.clone(), sid.clone()));
                continue;
            }
            add_backend(&mut out, main.clone());
            let paths = e.path.as_ref().map(|p| match p.strip_suffix("/*") {
                Some(base) => vec![base.to_string(), p.clone()],
                None => vec![p.clone(), format!("{}/*", p.trim_end_matches('/'))],
            });
            match out.hosts.iter_mut().find(|h| h.host == e.host) {
                Some(h) => match paths {
                    Some(p) => h.paths.push((p, main.backend.clone())),
                    None => h.default_backend = main.backend.clone(),
                },
                None => out.hosts.push(HostSpec {
                    host: e.host.clone(),
                    default_backend: main.backend.clone(),
                    paths: paths
                        .map(|p| vec![(p, main.backend.clone())])
                        .unwrap_or_default(),
                }),
            }
        }
        if let Some(wildcard) = &d.service.preview_domain
            && settings.mode != DomainMode::DomainMapping
        {
            let mask = format!("<tag>.{}", wildcard.trim_start_matches("*."));
            let preview = BackendSpec {
                service_id: sid.clone(),
                neg: format!("{sid}-p{}", &short(&mask)[..6]),
                backend: format!("{sid}-pbe"),
                url_mask: Some(mask),
            };
            out.hosts.push(HostSpec {
                host: wildcard.clone(),
                default_backend: preview.backend.clone(),
                paths: Vec::new(),
            });
            add_backend(&mut out, preview);
        }
    }
    let certify = match settings.mode {
        DomainMode::LoadBalancer => true,
        DomainMode::ExistingLoadBalancer => settings
            .existing
            .as_ref()
            .is_some_and(|e| e.certificate_map.is_some()),
        DomainMode::DomainMapping => false,
    };
    if certify {
        out.cert_domains = out.hosts.iter().map(|h| h.host.clone()).collect();
        out.cert_domains.sort();
        out.cert_domains.dedup();
    }
    out
}

/// Names of the load balancer runway creates for a stage.
pub struct LbNames {
    pub url_map: String,
    pub redirect_map: String,
    pub https_proxy: String,
    pub http_proxy: String,
    pub address: String,
    pub https_rule: String,
    pub http_rule: String,
    pub certificate_map: String,
}

impl LbNames {
    pub fn new(prefix: &str) -> Self {
        Self {
            url_map: format!("{prefix}-lb"),
            redirect_map: format!("{prefix}-redirect"),
            https_proxy: format!("{prefix}-https"),
            http_proxy: format!("{prefix}-http"),
            address: format!("{prefix}-ip"),
            https_rule: format!("{prefix}-https-fr"),
            http_rule: format!("{prefix}-http-fr"),
            certificate_map: format!("{prefix}-certs"),
        }
    }
}

fn path_matcher_name(prefix: &str, host: &str) -> String {
    format!("rw-{prefix}-{}", short(host))
}

pub fn backend_url(project: &str, backend: &str) -> String {
    format!("projects/{project}/global/backendServices/{backend}")
}

fn last(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

fn marked(description: Option<&str>, marker: &str) -> bool {
    let tokens: std::collections::BTreeSet<&str> =
        description.unwrap_or("").split_whitespace().collect();
    marker.split(' ').all(|t| tokens.contains(t))
}

/// Host rules and path matchers for `hosts`, marked as runway's.
pub fn routes(
    project: &str,
    prefix: &str,
    marker: &str,
    hosts: &[HostSpec],
) -> (Vec<HostRule>, Vec<PathMatcher>) {
    let mut rules = Vec::new();
    let mut matchers = Vec::new();
    for h in hosts {
        let pm = path_matcher_name(prefix, &h.host);
        rules.push(
            HostRule::new()
                .set_hosts([h.host.clone()])
                .set_path_matcher(&pm)
                .set_description(marker),
        );
        matchers.push(
            PathMatcher::new()
                .set_name(&pm)
                .set_description(marker)
                .set_default_service(backend_url(project, &h.default_backend))
                .set_path_rules(h.paths.iter().map(|(paths, be)| {
                    PathRule::new()
                        .set_paths(paths.clone())
                        .set_service(backend_url(project, be))
                })),
        );
    }
    (rules, matchers)
}

/// `live`'s host rules and path matchers with runway's replaced by
/// `rules`/`matchers`; the others (someone else's) are kept as they are.
/// Fails when a host runway routes is routed by someone else.
pub fn merge_routes(
    live: &UrlMap,
    marker: &str,
    rules: Vec<HostRule>,
    matchers: Vec<PathMatcher>,
) -> Result<(Vec<HostRule>, Vec<PathMatcher>)> {
    let theirs: Vec<HostRule> = live
        .host_rules
        .iter()
        .filter(|r| !marked(r.description.as_deref(), marker))
        .cloned()
        .collect();
    for r in &rules {
        for h in &r.hosts {
            if theirs.iter().any(|t| t.hosts.contains(h)) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "{h} is already routed by URL map {}, by a rule runway did not create",
                        live.name.as_deref().unwrap_or("")
                    ),
                )
                .permanent()
                .hint("remove that rule, or choose another host"));
            }
        }
    }
    let mut out_rules = theirs;
    out_rules.extend(rules);
    let mut out_matchers: Vec<PathMatcher> = live
        .path_matchers
        .iter()
        .filter(|m| !marked(m.description.as_deref(), marker))
        .cloned()
        .collect();
    out_matchers.extend(matchers);
    Ok((out_rules, out_matchers))
}

/// Comparable form of routes (hosts, matcher, default and path services).
fn routes_key(rules: &[HostRule], matchers: &[PathMatcher]) -> Vec<String> {
    let mut out: Vec<String> = rules
        .iter()
        .map(|r| {
            format!(
                "host {:?} -> {}",
                r.hosts,
                r.path_matcher.as_deref().unwrap_or("")
            )
        })
        .chain(matchers.iter().map(|m| {
            format!(
                "matcher {} default {} paths {:?}",
                m.name.as_deref().unwrap_or(""),
                last(m.default_service.as_deref().unwrap_or("")),
                m.path_rules
                    .iter()
                    .map(|p| (
                        p.paths.clone(),
                        last(p.service.as_deref().unwrap_or("")).to_string()
                    ))
                    .collect::<Vec<_>>()
            )
        }))
        .collect();
    out.sort();
    out
}

/// A DNS record: `(name, type, data)`; a record set is all the data of one
/// name and type.
pub type Rr = (String, String, String);

/// Hosts of runway's routes in `live` that `hosts` no longer has.
fn dropped_hosts(live: &UrlMap, marker: &str, hosts: &[HostSpec]) -> Vec<String> {
    live.host_rules
        .iter()
        .filter(|r| marked(r.description.as_deref(), marker))
        .flat_map(|r| r.hosts.clone())
        .filter(|h| !hosts.iter().any(|x| x.host == *h))
        .collect()
}

/// Records grouped into record sets: `(name, type) -> sorted data`.
pub fn record_sets(records: &[Rr]) -> BTreeMap<(String, String), Vec<String>> {
    let mut sets: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (name, kind, data) in records {
        let name = name.trim_end_matches('.').to_string();
        let data = match kind.as_str() {
            "CNAME" if !data.ends_with('.') && !data.starts_with('(') => format!("{data}."),
            _ => data.clone(),
        };
        let set = sets.entry((name, kind.clone())).or_default();
        if !set.contains(&data) {
            set.push(data);
        }
    }
    for v in sets.values_mut() {
        v.sort();
    }
    sets
}

/// What was set up outside runway's own resources (a load balancer's URL
/// map and certificate map), recorded in [`ANNOTATION_DOMAINS`] so that
/// runway's parts can be removed once runway.yaml no longer names them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recorded {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub url_maps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificate_maps: Vec<String>,
    /// The shared load balancer's IP (its hosts' A records point at it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The zone runway wrote records to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsZone>,
    /// Kinds of resources runway manages for the stage: removals look only
    /// for these (a stage with domain mappings never reads Compute Engine).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub load_balancer: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub certificates: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mappings: bool,
}

impl Recorded {
    pub fn of(d: &Desired) -> Self {
        let e = d.existing.as_ref().filter(|_| d.load_balanced());
        Self {
            url_maps: e.map(|e| e.url_map.clone()).into_iter().collect(),
            certificate_maps: e
                .and_then(|e| e.certificate_map.clone())
                .into_iter()
                .collect(),
            address: e.and_then(|e| e.address.clone()),
            dns: d.dns.clone().filter(|_| !d.is_empty()),
            load_balancer: d.load_balanced(),
            certificates: !d.cert_domains.is_empty(),
            mappings: !d.mappings.is_empty(),
        }
    }
    /// Both records: what a deploy may touch until it is done.
    pub fn union(&self, other: &Recorded) -> Recorded {
        let merge = |a: &[String], b: &[String]| {
            let mut v: Vec<String> = a.iter().chain(b).cloned().collect();
            v.sort();
            v.dedup();
            v
        };
        Recorded {
            url_maps: merge(&self.url_maps, &other.url_maps),
            certificate_maps: merge(&self.certificate_maps, &other.certificate_maps),
            address: other.address.clone().or_else(|| self.address.clone()),
            dns: other.dns.clone().or_else(|| self.dns.clone()),
            load_balancer: self.load_balancer || other.load_balancer,
            certificates: self.certificates || other.certificates,
            mappings: self.mappings || other.mappings,
        }
    }
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("serializes")
    }
    pub fn decode(v: &str) -> Self {
        serde_json::from_str(v).unwrap_or_default()
    }
}

// --------------------------------------------------------------- engine --

/// Optional API endpoint overrides (tests).
#[derive(Debug, Clone, Default)]
pub struct Endpoints {
    pub compute: Option<String>,
    pub certificates: Option<String>,
    pub dns: Option<String>,
    pub run: Option<String>,
}

#[derive(Clone)]
struct Clients {
    negs: RegionNetworkEndpointGroups,
    backends: BackendServices,
    url_maps: UrlMaps,
    https_proxies: TargetHttpsProxies,
    http_proxies: TargetHttpProxies,
    addresses: GlobalAddresses,
    rules: GlobalForwardingRules,
    certs: CertificateManager,
    dns: ResourceRecordSets,
}

/// A DNS record runway needs, and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: String,
    /// `in sync`, `created`, `would create`, … or why it must be created
    /// elsewhere (then `manual` is set).
    pub state: String,
    pub manual: bool,
}

/// What a reconcile found or did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// One line per change (`+`, `~`, `-`), made or to make.
    pub changes: Vec<String>,
    pub records: Vec<Record>,
    pub notes: Vec<String>,
    /// What removals leave in place on purpose: `(resource, reason)`.
    pub kept: Vec<(String, String)>,
    /// Records runway set that what is removed leaves behind.
    #[serde(skip)]
    pub stale: Vec<Rr>,
}

impl Report {
    fn change(&mut self, line: impl Into<String>) {
        self.changes.push(line.into());
    }
    /// Records someone must create outside runway.
    pub fn manual(&self) -> Vec<&Record> {
        self.records.iter().filter(|r| r.manual).collect()
    }
}

pub struct Engine<'a> {
    pub project: &'a str,
    pub region: &'a str,
    pub app: &'a str,
    pub stage: &'a str,
    /// Make the changes (else only report them).
    pub apply: bool,
    c: Clients,
    mappings: DomainMappings<'a>,
    marker: String,
}

macro_rules! get_opt {
    ($req:expr, $what:expr) => {
        match $req.send().await {
            Ok(v) => Some(v),
            Err(e) if absent(&e) => None,
            Err(e) => return Err(api_error(e, &$what)),
        }
    };
}

/// Waits for a compute operation and turns its error into one.
async fn done(
    res: std::result::Result<google_cloud_compute_v1::model::Operation, GaxError>,
    what: &str,
) -> Result<()> {
    let op = res.map_err(|e| api_error(e, what))?;
    match op.error.as_ref().map(|e| &e.errors) {
        Some(errors) if !errors.is_empty() => Err(Error::new(
            ErrorKind::Deploy,
            format!(
                "{what}: {}",
                errors
                    .iter()
                    .map(|e| e.message.clone().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        )),
        _ => Ok(()),
    }
}

impl<'a> Engine<'a> {
    pub async fn new(
        session: &'a crate::gcp::Session,
        project: &'a str,
        region: &'a str,
        app: &'a str,
        stage: &'a str,
        apply: bool,
        ep: &Endpoints,
    ) -> Result<Engine<'a>> {
        use crate::build_client_at;
        let c = Clients {
            negs: build_client_at!(RegionNetworkEndpointGroups, session, ep.compute.clone())?,
            backends: build_client_at!(BackendServices, session, ep.compute.clone())?,
            url_maps: build_client_at!(UrlMaps, session, ep.compute.clone())?,
            https_proxies: build_client_at!(TargetHttpsProxies, session, ep.compute.clone())?,
            http_proxies: build_client_at!(TargetHttpProxies, session, ep.compute.clone())?,
            addresses: build_client_at!(GlobalAddresses, session, ep.compute.clone())?,
            rules: build_client_at!(GlobalForwardingRules, session, ep.compute.clone())?,
            certs: build_client_at!(CertificateManager, session, ep.certificates.clone())?,
            dns: build_client_at!(ResourceRecordSets, session, ep.dns.clone())?,
        };
        Ok(Engine {
            project,
            region,
            app,
            stage,
            apply,
            c,
            mappings: DomainMappings {
                http: &session.http,
                token: session.token().await?,
                project: project.to_string(),
                region: region.to_string(),
                base_url: ep.run.clone(),
            },
            marker: crate::gcp::scheduler::marker(app, stage),
        })
    }

    fn labels(&self) -> BTreeMap<String, String> {
        crate::naming::ownership_labels(self.app, self.stage)
    }

    fn owns_labels(&self, labels: &std::collections::HashMap<String, String>) -> bool {
        self.labels().iter().all(|(k, v)| labels.get(k) == Some(v))
    }

    fn certs_parent(&self) -> String {
        format!("projects/{}/locations/global", self.project)
    }

    /// Reconciles `want`; with `removals`, also removes what runway set up
    /// for this stage that `want` no longer lists (with `keep_cloud_run_urls`,
    /// `NAME.cloud.run` mappings stay: freed, anyone could claim them).
    pub async fn reconcile(
        &self,
        want: &Desired,
        previous: &Recorded,
        removals: bool,
        keep_cloud_run_urls: bool,
    ) -> Result<Report> {
        let dry = self
            .read_only()
            .pass(want, previous, removals, keep_cloud_run_urls)
            .await?;
        if !self.apply || dry.changes.is_empty() {
            return Ok(dry);
        }
        // The DNS records of what is removed go first: once a mapping or a
        // route is deleted, nothing says which records runway set there. A
        // failure here leaves everything for the next run to find again.
        let mut first = Report::default();
        self.delete_stale(want, previous, &dry.stale, &record_keys(&dry), &mut first)
            .await?;
        let mut out = self
            .pass(want, previous, removals, keep_cloud_run_urls)
            .await?;
        first.changes.append(&mut out.changes);
        out.changes = first.changes;
        out.notes.append(&mut first.notes);
        Ok(out)
    }

    /// The same engine, reading only.
    fn read_only(&self) -> Engine<'a> {
        Engine {
            project: self.project,
            region: self.region,
            app: self.app,
            stage: self.stage,
            apply: false,
            c: self.c.clone(),
            mappings: self.mappings.clone(),
            marker: self.marker.clone(),
        }
    }

    async fn pass(
        &self,
        want: &Desired,
        previous: &Recorded,
        removals: bool,
        keep_cloud_run_urls: bool,
    ) -> Result<Report> {
        let mut r = Report::default();
        // Kinds of resources runway managed or manages: removals look for
        // nothing else (and need no permission on anything else).
        let had = previous.union(&Recorded::of(want));
        // Records runway set that what is removed leaves behind.
        let mut stale: Vec<Rr> = Vec::new();
        self.mappings_part(
            want,
            removals && had.mappings,
            keep_cloud_run_urls,
            &mut stale,
            &mut r,
        )
        .await?;
        let mut records: Vec<Rr> = Vec::new();
        let ip = match want.mode {
            DomainMode::LoadBalancer if want.load_balanced() => {
                let (ip, dropped) = self.own_lb(want, &mut r).await?;
                stale.extend(dropped.into_iter().map(|h| (h, "A".into(), ip.clone())));
                Some(ip)
            }
            _ => None,
        };
        if want.mode == DomainMode::ExistingLoadBalancer && want.load_balanced() {
            let dropped = self.existing_lb(want, &mut r).await?;
            if let Some(ip) = want.existing.as_ref().and_then(|e| e.address.clone()) {
                stale.extend(dropped.into_iter().map(|h| (h, "A".into(), ip.clone())));
            }
        }
        if removals && had.load_balancer {
            stale.extend(self.remove_unlisted_lb(want, previous, &mut r).await?);
        }
        // Certificates (own load balancer, or an existing one's map).
        let map = match (&want.mode, &want.existing) {
            (DomainMode::LoadBalancer, _) => Some(LbNames::new(&want.prefix).certificate_map),
            (DomainMode::ExistingLoadBalancer, Some(e)) => e.certificate_map.clone(),
            _ => None,
        };
        if let Some(map) = &map
            && !want.cert_domains.is_empty()
        {
            for (name, data) in self.certificates(want, map, &mut r).await? {
                records.push((name, "CNAME".into(), data));
            }
        }
        if removals && had.certificates {
            stale.extend(
                self.remove_unlisted_certs(want, map.as_deref(), previous, &mut r)
                    .await?,
            );
        }
        // Addresses for the hosts.
        let address = ip
            .filter(|ip| !ip.is_empty())
            .or_else(|| want.existing.as_ref().and_then(|e| e.address.clone()));
        if want.load_balanced() {
            match &address {
                Some(ip) => {
                    for h in &want.hosts {
                        records.push((h.host.clone(), "A".into(), ip.clone()));
                    }
                }
                None if want.mode == DomainMode::LoadBalancer => {
                    for h in &want.hosts {
                        records.push((h.host.clone(), "A".into(), "(the load balancer's IP, known after deploy)".into()));
                    }
                }
                None => r.notes.push(
                    "the existing load balancer's IP is not configured (`domains.load_balancer.address`): point the hosts' DNS records at it yourself".into(),
                ),
            }
        }
        for m in &want.mappings {
            if m.0.ends_with(".cloud.run") || want.mode != DomainMode::DomainMapping {
                continue;
            }
            if let Some(live) = self.mappings.get(&m.0, self.app, self.stage).await? {
                for rec in live.records {
                    records.push((m.0.clone(), rec.kind, rec.data));
                }
            } else {
                records.push((
                    m.0.clone(),
                    "CNAME".into(),
                    "(given by Cloud Run once the mapping exists)".into(),
                ));
            }
        }
        self.dns(want, records, &mut r).await?;
        self.delete_stale(want, previous, &stale, &record_keys(&r), &mut r)
            .await?;
        r.stale = stale;
        Ok(r)
    }

    // --------------------------------------------------- domain mappings

    async fn mappings_part(
        &self,
        want: &Desired,
        removals: bool,
        keep_cloud_run_urls: bool,
        stale: &mut Vec<Rr>,
        r: &mut Report,
    ) -> Result<()> {
        for (domain, service) in &want.mappings {
            match self.mappings.get(domain, self.app, self.stage).await? {
                Some(m) if !m.owned => {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("domain mapping {domain} exists and was not created by runway for this app and stage"),
                    )
                    .permanent());
                }
                Some(m) if m.service == *service => {
                    if !m.ready && !m.message.is_empty() {
                        r.notes.push(format!("{domain}: {}", m.message));
                    }
                }
                Some(_) => {
                    r.change(format!("~ domain mapping {domain} -> {service}"));
                    if self.apply {
                        self.mappings.delete(domain).await?;
                        self.mappings
                            .create(domain, service, self.app, self.stage)
                            .await?;
                    }
                }
                None => {
                    r.change(format!("+ domain mapping {domain} -> {service}"));
                    if self.apply {
                        self.mappings
                            .create(domain, service, self.app, self.stage)
                            .await?;
                    }
                }
            }
        }
        if removals {
            let live = match self.mappings.list(self.app, self.stage).await {
                Ok(l) => l,
                // Without the Cloud Run v1 API there is nothing to remove.
                Err(e) if e.kind == ErrorKind::Prerequisite => Vec::new(),
                Err(e) => return Err(e),
            };
            for m in live.into_iter().filter(|m| m.owned) {
                let wanted = want.mappings.iter().any(|(d, _)| *d == m.domain);
                let keep = keep_cloud_run_urls && m.domain.ends_with(".cloud.run");
                if keep && !wanted {
                    r.kept.push((
                        format!("custom URL {}", m.domain),
                        "once deleted, anyone could claim it (`--release-urls` deletes it)".into(),
                    ));
                } else if !wanted {
                    r.change(format!("- domain mapping {}", m.domain));
                    stale.extend(
                        m.records
                            .iter()
                            .map(|x| (m.domain.clone(), x.kind.clone(), x.data.clone())),
                    );
                    if self.apply {
                        self.mappings.delete(&m.domain).await?;
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------- backends and URL maps

    async fn backends(&self, want: &Desired, r: &mut Report) -> Result<()> {
        let p = self.project;
        for b in &want.backends {
            let neg = get_opt!(
                self.c
                    .negs
                    .get()
                    .set_project(p)
                    .set_region(self.region)
                    .set_network_endpoint_group(&b.neg),
                format!("reading NEG {}", b.neg)
            );
            if let Some(g) = &neg
                && !marked(g.description.as_deref(), &self.marker)
            {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "NEG {} exists and was not created by runway for this app and stage",
                        b.neg
                    ),
                )
                .permanent());
            }
            if neg.is_none() {
                r.change(format!("+ serverless NEG {} -> {}", b.neg, b.service_id));
                if self.apply {
                    let mut cr = NetworkEndpointGroupCloudRun::new().set_service(&b.service_id);
                    if let Some(mask) = &b.url_mask {
                        cr = cr.set_url_mask(mask);
                    }
                    let body = NetworkEndpointGroup::new()
                        .set_name(&b.neg)
                        .set_description(&self.marker)
                        .set_network_endpoint_type(
                            network_endpoint_group::NetworkEndpointType::Serverless,
                        )
                        .set_cloud_run(cr);
                    done(
                        self.c
                            .negs
                            .insert()
                            .set_project(p)
                            .set_region(self.region)
                            .set_body(body)
                            .poller()
                            .until_done()
                            .await,
                        &format!("creating NEG {}", b.neg),
                    )
                    .await?;
                }
            }
            let group = format!(
                "projects/{p}/regions/{}/networkEndpointGroups/{}",
                self.region, b.neg
            );
            let live = get_opt!(
                self.c
                    .backends
                    .get()
                    .set_project(p)
                    .set_backend_service(&b.backend),
                format!("reading backend service {}", b.backend)
            );
            match live {
                None => {
                    r.change(format!("+ backend service {}", b.backend));
                    if self.apply {
                        let body = BackendService::new()
                            .set_name(&b.backend)
                            .set_description(&self.marker)
                            .set_load_balancing_scheme(
                                backend_service::LoadBalancingScheme::ExternalManaged,
                            )
                            .set_backends([Backend::new().set_group(&group)]);
                        done(
                            self.c
                                .backends
                                .insert()
                                .set_project(p)
                                .set_body(body)
                                .poller()
                                .until_done()
                                .await,
                            &format!("creating backend service {}", b.backend),
                        )
                        .await?;
                    }
                }
                Some(bs) if !marked(bs.description.as_deref(), &self.marker) => {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        format!("backend service {} exists and was not created by runway for this app and stage", b.backend),
                    )
                    .permanent());
                }
                Some(bs) => {
                    let groups: Vec<&str> = bs
                        .backends
                        .iter()
                        .filter_map(|x| x.group.as_deref())
                        .map(last)
                        .collect();
                    if groups != [b.neg.as_str()] {
                        r.change(format!("~ backend service {} -> NEG {}", b.backend, b.neg));
                        if self.apply {
                            let body = bs.clone().set_backends([Backend::new().set_group(&group)]);
                            done(
                                self.c
                                    .backends
                                    .update()
                                    .set_project(p)
                                    .set_backend_service(&b.backend)
                                    .set_body(body)
                                    .poller()
                                    .until_done()
                                    .await,
                                &format!("updating backend service {}", b.backend),
                            )
                            .await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The load balancer runway owns; returns its IP address.
    /// Returns its IP address and the hosts it no longer routes.
    async fn own_lb(&self, want: &Desired, r: &mut Report) -> Result<(String, Vec<String>)> {
        let p = self.project;
        let n = LbNames::new(&want.prefix);
        // Address first: DNS records point at it.
        let addr = get_opt!(
            self.c
                .addresses
                .get()
                .set_project(p)
                .set_address(&n.address),
            format!("reading address {}", n.address)
        );
        let ip = match addr {
            Some(a) if !marked(a.description.as_deref(), &self.marker) => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "IP address {} exists and was not created by runway for this app and stage",
                        n.address
                    ),
                )
                .permanent());
            }
            Some(a) => a.address.unwrap_or_default(),
            None => {
                r.change(format!("+ global IP address {}", n.address));
                if !self.apply {
                    String::new()
                } else {
                    let body = Address::new()
                        .set_name(&n.address)
                        .set_description(&self.marker)
                        .set_address_type(address::AddressType::External)
                        .set_ip_version(address::IpVersion::Ipv4)
                        .set_labels(self.labels());
                    done(
                        self.c
                            .addresses
                            .insert()
                            .set_project(p)
                            .set_body(body)
                            .poller()
                            .until_done()
                            .await,
                        "creating the IP address",
                    )
                    .await?;
                    self.c
                        .addresses
                        .get()
                        .set_project(p)
                        .set_address(&n.address)
                        .send()
                        .await
                        .map_err(|e| api_error(e, "reading the IP address"))?
                        .address
                        .unwrap_or_default()
                }
            }
        };
        self.backends(want, r).await?;
        // URL map: every route is runway's.
        let (rules, matchers) = routes(p, &want.prefix, &self.marker, &want.hosts);
        let default = backend_url(p, &want.hosts[0].default_backend);
        let live = get_opt!(
            self.c.url_maps.get().set_project(p).set_url_map(&n.url_map),
            format!("reading URL map {}", n.url_map)
        );
        let dropped = live
            .as_ref()
            .map(|m| dropped_hosts(m, &self.marker, &want.hosts))
            .unwrap_or_default();
        match live {
            None => {
                r.change(format!(
                    "+ URL map {} ({} host(s))",
                    n.url_map,
                    want.hosts.len()
                ));
                if self.apply {
                    let body = UrlMap::new()
                        .set_name(&n.url_map)
                        .set_description(&self.marker)
                        .set_default_service(&default)
                        .set_host_rules(rules)
                        .set_path_matchers(matchers);
                    done(
                        self.c
                            .url_maps
                            .insert()
                            .set_project(p)
                            .set_body(body)
                            .poller()
                            .until_done()
                            .await,
                        "creating the URL map",
                    )
                    .await?;
                }
            }
            Some(m) if !marked(m.description.as_deref(), &self.marker) => {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    format!(
                        "URL map {} exists and was not created by runway for this app and stage",
                        n.url_map
                    ),
                )
                .permanent());
            }
            Some(m) => {
                if routes_key(&m.host_rules, &m.path_matchers) != routes_key(&rules, &matchers)
                    || last(m.default_service.as_deref().unwrap_or("")) != last(&default)
                {
                    r.change(format!("~ URL map {}: routes", n.url_map));
                    if self.apply {
                        let body = m
                            .clone()
                            .set_default_service(&default)
                            .set_host_rules(rules)
                            .set_path_matchers(matchers);
                        done(
                            self.c
                                .url_maps
                                .update()
                                .set_project(p)
                                .set_url_map(&n.url_map)
                                .set_body(body)
                                .poller()
                                .until_done()
                                .await,
                            "updating the URL map",
                        )
                        .await?;
                    }
                }
            }
        }
        // HTTPS proxy with the certificate map.
        let cert_map = format!(
            "//certificatemanager.googleapis.com/{}/certificateMaps/{}",
            self.certs_parent(),
            n.certificate_map
        );
        let url_map_url = format!("projects/{p}/global/urlMaps/{}", n.url_map);
        if get_opt!(
            self.c
                .https_proxies
                .get()
                .set_project(p)
                .set_target_https_proxy(&n.https_proxy),
            "reading the HTTPS proxy"
        )
        .is_none()
        {
            r.change(format!("+ HTTPS proxy {}", n.https_proxy));
            if self.apply {
                // The certificate map must exist first.
                self.ensure_certificate_map(&n.certificate_map, r, false)
                    .await?;
                let body = TargetHttpsProxy::new()
                    .set_name(&n.https_proxy)
                    .set_description(&self.marker)
                    .set_url_map(&url_map_url)
                    .set_certificate_map(&cert_map);
                done(
                    self.c
                        .https_proxies
                        .insert()
                        .set_project(p)
                        .set_body(body)
                        .poller()
                        .until_done()
                        .await,
                    "creating the HTTPS proxy",
                )
                .await?;
            }
        }
        // HTTP: a redirect to HTTPS.
        if get_opt!(
            self.c
                .url_maps
                .get()
                .set_project(p)
                .set_url_map(&n.redirect_map),
            "reading the redirect URL map"
        )
        .is_none()
        {
            r.change(format!("+ HTTP to HTTPS redirect {}", n.redirect_map));
            if self.apply {
                let body = UrlMap::new()
                    .set_name(&n.redirect_map)
                    .set_description(&self.marker)
                    .set_default_url_redirect(
                        HttpRedirectAction::new()
                            .set_https_redirect(true)
                            .set_strip_query(false)
                            .set_redirect_response_code(
                                http_redirect_action::RedirectResponseCode::MovedPermanentlyDefault,
                            ),
                    );
                done(
                    self.c
                        .url_maps
                        .insert()
                        .set_project(p)
                        .set_body(body)
                        .poller()
                        .until_done()
                        .await,
                    "creating the redirect URL map",
                )
                .await?;
            }
        }
        if get_opt!(
            self.c
                .http_proxies
                .get()
                .set_project(p)
                .set_target_http_proxy(&n.http_proxy),
            "reading the HTTP proxy"
        )
        .is_none()
        {
            r.change(format!("+ HTTP proxy {}", n.http_proxy));
            if self.apply {
                let body = TargetHttpProxy::new()
                    .set_name(&n.http_proxy)
                    .set_description(&self.marker)
                    .set_url_map(format!("projects/{p}/global/urlMaps/{}", n.redirect_map));
                done(
                    self.c
                        .http_proxies
                        .insert()
                        .set_project(p)
                        .set_body(body)
                        .poller()
                        .until_done()
                        .await,
                    "creating the HTTP proxy",
                )
                .await?;
            }
        }
        for (rule, port, target) in [
            (
                &n.https_rule,
                "443",
                format!("projects/{p}/global/targetHttpsProxies/{}", n.https_proxy),
            ),
            (
                &n.http_rule,
                "80",
                format!("projects/{p}/global/targetHttpProxies/{}", n.http_proxy),
            ),
        ] {
            if get_opt!(
                self.c.rules.get().set_project(p).set_forwarding_rule(rule),
                format!("reading forwarding rule {rule}")
            )
            .is_none()
            {
                r.change(format!("+ forwarding rule {rule} (port {port})"));
                if self.apply {
                    let body = ForwardingRule::new()
                        .set_name(rule)
                        .set_description(&self.marker)
                        .set_labels(self.labels())
                        .set_ip_address(format!("projects/{p}/global/addresses/{}", n.address))
                        .set_ip_protocol(forwarding_rule::IPProtocol::Tcp)
                        .set_port_range(port)
                        .set_target(target)
                        .set_load_balancing_scheme(
                            forwarding_rule::LoadBalancingScheme::ExternalManaged,
                        );
                    done(
                        self.c
                            .rules
                            .insert()
                            .set_project(p)
                            .set_body(body)
                            .poller()
                            .until_done()
                            .await,
                        &format!("creating forwarding rule {rule}"),
                    )
                    .await?;
                }
            }
        }
        Ok((ip, dropped))
    }

    /// Routes and backends added to a load balancer runway does not own.
    /// Returns the hosts runway no longer routes there.
    async fn existing_lb(&self, want: &Desired, r: &mut Report) -> Result<Vec<String>> {
        let p = self.project;
        let e = want.existing.as_ref().expect("existing load balancer");
        let live = get_opt!(
            self.c.url_maps.get().set_project(p).set_url_map(&e.url_map),
            format!("reading URL map {}", e.url_map)
        )
        .ok_or_else(|| {
            Error::prerequisite(format!(
                "URL map {} (domains.load_balancer.url_map) does not exist",
                e.url_map
            ))
        })?;
        let dropped = dropped_hosts(&live, &self.marker, &want.hosts);
        let (rules, matchers) = routes(p, &want.prefix, &self.marker, &want.hosts);
        let (all_rules, all_matchers) = merge_routes(&live, &self.marker, rules, matchers)?;
        self.backends(want, r).await?;
        if routes_key(&live.host_rules, &live.path_matchers)
            != routes_key(&all_rules, &all_matchers)
        {
            r.change(format!(
                "~ URL map {} (not runway's): runway's routes",
                e.url_map
            ));
            if self.apply {
                let body = live
                    .clone()
                    .set_host_rules(all_rules)
                    .set_path_matchers(all_matchers);
                done(
                    self.c
                        .url_maps
                        .update()
                        .set_project(p)
                        .set_url_map(&e.url_map)
                        .set_body(body)
                        .poller()
                        .until_done()
                        .await,
                    &format!("updating URL map {}", e.url_map),
                )
                .await?;
            }
        }
        Ok(dropped)
    }

    /// Removes runway's routes, backends and load balancer that runway.yaml
    /// no longer asks for. Returns the hosts that lost their routes.
    async fn remove_unlisted_lb(
        &self,
        want: &Desired,
        previous: &Recorded,
        r: &mut Report,
    ) -> Result<Vec<Rr>> {
        let p = self.project;
        let mut stale: Vec<Rr> = Vec::new();
        let n = LbNames::new(&want.prefix);
        let keep_own = want.mode == DomainMode::LoadBalancer && want.load_balanced();
        // Runway's load balancer, when no longer wanted: each part is looked
        // for on its own (a failed removal may have left any of them).
        if !keep_own {
            let own_map = match self
                .c
                .url_maps
                .get()
                .set_project(p)
                .set_url_map(&n.url_map)
                .send()
                .await
            {
                Ok(m) if marked(m.description.as_deref(), &self.marker) => Some(m),
                Ok(_) => None,
                Err(e) if absent(&e) => None,
                Err(e) => return Err(api_error(e, "reading the URL map")),
            };
            let ip = match self
                .c
                .addresses
                .get()
                .set_project(p)
                .set_address(&n.address)
                .send()
                .await
            {
                Ok(a) if marked(a.description.as_deref(), &self.marker) => a.address,
                Ok(_) => None,
                Err(e) if absent(&e) => None,
                Err(e) => return Err(api_error(e, "reading the IP address")),
            };
            if let (Some(m), Some(ip)) = (&own_map, &ip) {
                stale.extend(
                    m.host_rules
                        .iter()
                        .flat_map(|h| h.hosts.clone())
                        .map(|h| (h, "A".to_string(), ip.clone())),
                );
            }
            self.delete_own_lb(&n, r).await?;
        }
        // Runway's routes in a load balancer it does not own.
        let current = Recorded::of(want);
        for name in previous
            .url_maps
            .iter()
            .filter(|m| !current.url_maps.contains(m))
        {
            let Some(live) = get_opt!(
                self.c.url_maps.get().set_project(p).set_url_map(name),
                format!("reading URL map {name}")
            ) else {
                continue;
            };
            let (rules, matchers) = merge_routes(&live, &self.marker, Vec::new(), Vec::new())?;
            if rules.len() != live.host_rules.len() || matchers.len() != live.path_matchers.len() {
                if let Some(ip) = &previous.address {
                    stale.extend(
                        live.host_rules
                            .iter()
                            .filter(|h| marked(h.description.as_deref(), &self.marker))
                            .flat_map(|h| h.hosts.clone())
                            .map(|h| (h, "A".to_string(), ip.clone())),
                    );
                }
                r.change(format!(
                    "- runway's routes from URL map {name} (not runway's: kept)"
                ));
                if self.apply {
                    let body = live
                        .clone()
                        .set_host_rules(rules)
                        .set_path_matchers(matchers);
                    done(
                        self.c
                            .url_maps
                            .update()
                            .set_project(p)
                            .set_url_map(name.as_str())
                            .set_body(body)
                            .poller()
                            .until_done()
                            .await,
                        &format!("updating URL map {name}"),
                    )
                    .await?;
                }
            }
        }
        // Backends and NEGs runway created for this stage, no longer used.
        let wanted_backends: Vec<&str> = want.backends.iter().map(|b| b.backend.as_str()).collect();
        let wanted_negs: Vec<&str> = want.backends.iter().map(|b| b.neg.as_str()).collect();
        let lb_wanted = want.load_balanced();
        let backends = match self.c.backends.list().set_project(p).send().await {
            Ok(l) => l.items,
            Err(e) if absent(&e) => Vec::new(),
            Err(e) => return Err(api_error(e, "listing backend services")),
        };
        for b in backends
            .iter()
            .filter(|b| marked(b.description.as_deref(), &self.marker))
        {
            let name = b.name.clone().unwrap_or_default();
            if lb_wanted && wanted_backends.contains(&name.as_str()) {
                continue;
            }
            r.change(format!("- backend service {name}"));
            if self.apply {
                done(
                    self.c
                        .backends
                        .delete()
                        .set_project(p)
                        .set_backend_service(&name)
                        .poller()
                        .until_done()
                        .await,
                    &format!("deleting backend service {name}"),
                )
                .await?;
            }
        }
        let negs = match self
            .c
            .negs
            .list()
            .set_project(p)
            .set_region(self.region)
            .send()
            .await
        {
            Ok(l) => l.items,
            Err(e) if absent(&e) => Vec::new(),
            Err(e) => return Err(api_error(e, "listing NEGs")),
        };
        for g in negs
            .iter()
            .filter(|g| marked(g.description.as_deref(), &self.marker))
        {
            let name = g.name.clone().unwrap_or_default();
            if lb_wanted && wanted_negs.contains(&name.as_str()) {
                continue;
            }
            r.change(format!("- serverless NEG {name}"));
            if self.apply {
                done(
                    self.c
                        .negs
                        .delete()
                        .set_project(p)
                        .set_region(self.region)
                        .set_network_endpoint_group(&name)
                        .poller()
                        .until_done()
                        .await,
                    &format!("deleting NEG {name}"),
                )
                .await?;
            }
        }
        Ok(stale)
    }

    async fn delete_own_lb(&self, n: &LbNames, r: &mut Report) -> Result<()> {
        let p = self.project;
        // Reverse order of creation: each resource is used by the next one.
        // Only what carries runway's marker (a same-named resource is not).
        for rule in [&n.https_rule, &n.http_rule] {
            if get_opt!(
                self.c.rules.get().set_project(p).set_forwarding_rule(rule),
                format!("reading {rule}")
            )
            .is_some_and(|x| marked(x.description.as_deref(), &self.marker))
            {
                r.change(format!("- forwarding rule {rule}"));
                if self.apply {
                    done(
                        self.c
                            .rules
                            .delete()
                            .set_project(p)
                            .set_forwarding_rule(rule)
                            .poller()
                            .until_done()
                            .await,
                        &format!("deleting {rule}"),
                    )
                    .await?;
                }
            }
        }
        if get_opt!(
            self.c
                .https_proxies
                .get()
                .set_project(p)
                .set_target_https_proxy(&n.https_proxy),
            "reading the HTTPS proxy"
        )
        .is_some_and(|x| marked(x.description.as_deref(), &self.marker))
        {
            r.change(format!("- HTTPS proxy {}", n.https_proxy));
            if self.apply {
                done(
                    self.c
                        .https_proxies
                        .delete()
                        .set_project(p)
                        .set_target_https_proxy(&n.https_proxy)
                        .poller()
                        .until_done()
                        .await,
                    "deleting the HTTPS proxy",
                )
                .await?;
            }
        }
        if get_opt!(
            self.c
                .http_proxies
                .get()
                .set_project(p)
                .set_target_http_proxy(&n.http_proxy),
            "reading the HTTP proxy"
        )
        .is_some_and(|x| marked(x.description.as_deref(), &self.marker))
        {
            r.change(format!("- HTTP proxy {}", n.http_proxy));
            if self.apply {
                done(
                    self.c
                        .http_proxies
                        .delete()
                        .set_project(p)
                        .set_target_http_proxy(&n.http_proxy)
                        .poller()
                        .until_done()
                        .await,
                    "deleting the HTTP proxy",
                )
                .await?;
            }
        }
        for map in [&n.url_map, &n.redirect_map] {
            if get_opt!(
                self.c.url_maps.get().set_project(p).set_url_map(map),
                format!("reading {map}")
            )
            .is_some_and(|x| marked(x.description.as_deref(), &self.marker))
            {
                r.change(format!("- URL map {map}"));
                if self.apply {
                    done(
                        self.c
                            .url_maps
                            .delete()
                            .set_project(p)
                            .set_url_map(map)
                            .poller()
                            .until_done()
                            .await,
                        &format!("deleting {map}"),
                    )
                    .await?;
                }
            }
        }
        if get_opt!(
            self.c
                .addresses
                .get()
                .set_project(p)
                .set_address(&n.address),
            "reading the IP address"
        )
        .is_some_and(|x| marked(x.description.as_deref(), &self.marker))
        {
            r.change(format!("- global IP address {}", n.address));
            if self.apply {
                done(
                    self.c
                        .addresses
                        .delete()
                        .set_project(p)
                        .set_address(&n.address)
                        .poller()
                        .until_done()
                        .await,
                    "deleting the IP address",
                )
                .await?;
            }
        }
        Ok(())
    }

    // --------------------------------------------------- certificates

    async fn ensure_certificate_map(&self, map: &str, r: &mut Report, report: bool) -> Result<()> {
        let name = format!("{}/certificateMaps/{map}", self.certs_parent());
        match self
            .c
            .certs
            .get_certificate_map()
            .set_name(&name)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if absent(&e) => {
                if report {
                    r.change(format!("+ certificate map {map}"));
                }
                if self.apply {
                    self.c
                        .certs
                        .create_certificate_map()
                        .set_parent(self.certs_parent())
                        .set_certificate_map_id(map)
                        .set_certificate_map(
                            CertificateMap::new()
                                .set_description(&self.marker)
                                .set_labels(self.labels()),
                        )
                        .poller()
                        .until_done()
                        .await
                        .map_err(|e| api_error(e, "creating the certificate map"))?;
                }
                Ok(())
            }
            Err(e) => Err(api_error(e, "reading the certificate map")),
        }
    }

    /// Certificates (with DNS authorizations) and map entries for every
    /// domain; returns the authorization records `(name, data)`.
    async fn certificates(
        &self,
        want: &Desired,
        map: &str,
        r: &mut Report,
    ) -> Result<Vec<(String, String)>> {
        let parent = self.certs_parent();
        let own_map = want.mode == DomainMode::LoadBalancer;
        if own_map {
            self.ensure_certificate_map(map, r, true).await?;
        }
        let mut records = Vec::new();
        for domain in &want.cert_domains {
            let id = format!("{}-{}", want.prefix, short(domain));
            let auth_domain = domain.trim_start_matches("*.").to_string();
            // `x.example.com` and `*.x.example.com` share one authorization
            // (one `_acme-challenge.x.example.com` record).
            let auth_id = format!("{}-{}", want.prefix, short(&auth_domain));
            let auth_name = format!("{parent}/dnsAuthorizations/{auth_id}");
            let auth = match self
                .c
                .certs
                .get_dns_authorization()
                .set_name(&auth_name)
                .send()
                .await
            {
                Ok(a) => Some(a),
                Err(e) if absent(&e) => {
                    r.change(format!("+ DNS authorization for {auth_domain}"));
                    if self.apply {
                        self.c
                            .certs
                            .create_dns_authorization()
                            .set_parent(&parent)
                            .set_dns_authorization_id(&auth_id)
                            .set_dns_authorization(
                                DnsAuthorization::new()
                                    .set_domain(&auth_domain)
                                    .set_description(&self.marker)
                                    .set_labels(self.labels()),
                            )
                            .poller()
                            .until_done()
                            .await
                            .map_err(|e| {
                                api_error(
                                    e,
                                    &format!("creating the DNS authorization for {auth_domain}"),
                                )
                            })?;
                        Some(
                            self.c
                                .certs
                                .get_dns_authorization()
                                .set_name(&auth_name)
                                .send()
                                .await
                                .map_err(|e| api_error(e, "reading the DNS authorization"))?,
                        )
                    } else {
                        None
                    }
                }
                Err(e) => return Err(api_error(e, "reading a DNS authorization")),
            };
            match auth.as_ref().and_then(|a| a.dns_resource_record.as_ref()) {
                Some(rec) => {
                    records.push((rec.name.trim_end_matches('.').to_string(), rec.data.clone()))
                }
                None => records.push((
                    format!("_acme-challenge.{auth_domain}"),
                    "(given by Certificate Manager once the authorization exists)".into(),
                )),
            }
            let cert_name = format!("{parent}/certificates/{id}");
            match self
                .c
                .certs
                .get_certificate()
                .set_name(&cert_name)
                .send()
                .await
            {
                Ok(c) => {
                    if let Some(certificate::Type::Managed(m)) = &c.r#type
                        && m.state != certificate::managed_certificate::State::Active
                    {
                        r.notes.push(format!("certificate for {domain}: {:?} (issued once its DNS authorization record exists)", m.state));
                    }
                }
                Err(e) if absent(&e) => {
                    r.change(format!("+ certificate for {domain}"));
                    if self.apply {
                        let managed = certificate::ManagedCertificate::new()
                            .set_domains([domain.clone()])
                            .set_dns_authorizations([auth_name.clone()]);
                        self.c
                            .certs
                            .create_certificate()
                            .set_parent(&parent)
                            .set_certificate_id(&id)
                            .set_certificate(
                                Certificate::new()
                                    .set_description(&self.marker)
                                    .set_labels(self.labels())
                                    .set_managed(managed),
                            )
                            .poller()
                            .until_done()
                            .await
                            .map_err(|e| {
                                api_error(e, &format!("creating the certificate for {domain}"))
                            })?;
                    }
                }
                Err(e) => return Err(api_error(e, "reading a certificate")),
            }
            let entry_name = format!("{parent}/certificateMaps/{map}/certificateMapEntries/{id}");
            match self
                .c
                .certs
                .get_certificate_map_entry()
                .set_name(&entry_name)
                .send()
                .await
            {
                Ok(_) => {}
                Err(e) if absent(&e) => {
                    r.change(format!("+ certificate map entry for {domain}"));
                    if self.apply {
                        self.c
                            .certs
                            .create_certificate_map_entry()
                            .set_parent(format!("{parent}/certificateMaps/{map}"))
                            .set_certificate_map_entry_id(&id)
                            .set_certificate_map_entry(
                                CertificateMapEntry::new()
                                    .set_hostname(domain)
                                    .set_certificates([cert_name.clone()])
                                    .set_description(&self.marker)
                                    .set_labels(self.labels()),
                            )
                            .poller()
                            .until_done()
                            .await
                            .map_err(|e| {
                                api_error(e, &format!("adding {domain} to certificate map {map}"))
                            })?;
                    }
                }
                Err(e) => return Err(api_error(e, "reading a certificate map entry")),
            }
        }
        Ok(records)
    }

    /// Returns the DNS authorization records of what it removes.
    async fn remove_unlisted_certs(
        &self,
        want: &Desired,
        map: Option<&str>,
        previous: &Recorded,
        r: &mut Report,
    ) -> Result<Vec<Rr>> {
        let mut stale: Vec<Rr> = Vec::new();
        let parent = self.certs_parent();
        let wanted: Vec<String> = want
            .cert_domains
            .iter()
            .map(|d| format!("{}-{}", want.prefix, short(d)))
            .collect();
        let is_wanted = |name: &str| wanted.iter().any(|w| last(name) == w);
        let wanted_auths: Vec<String> = want
            .cert_domains
            .iter()
            .map(|d| format!("{}-{}", want.prefix, short(d.trim_start_matches("*."))))
            .collect();
        let n = LbNames::new(&want.prefix);
        let mut maps: Vec<String> = vec![n.certificate_map.clone()];
        maps.extend(map.map(String::from));
        maps.extend(previous.certificate_maps.iter().cloned());
        maps.sort();
        maps.dedup();
        for m in &maps {
            let entries = match self
                .c
                .certs
                .list_certificate_map_entries()
                .set_parent(format!("{parent}/certificateMaps/{m}"))
                .send()
                .await
            {
                Ok(l) => l.certificate_map_entries,
                Err(e) if absent(&e) => continue,
                Err(e) => return Err(api_error(e, "listing certificate map entries")),
            };
            // Entries are wanted only in the map in use.
            let in_use = map == Some(m.as_str());
            for e in entries
                .into_iter()
                .filter(|e| self.owns_labels(&e.labels) && !(in_use && is_wanted(&e.name)))
            {
                r.change(format!("- certificate map entry {}", last(&e.name)));
                if self.apply {
                    self.c
                        .certs
                        .delete_certificate_map_entry()
                        .set_name(&e.name)
                        .poller()
                        .until_done()
                        .await
                        .map_err(|err| api_error(err, "deleting a certificate map entry"))?;
                }
            }
        }
        let certs = match self
            .c
            .certs
            .list_certificates()
            .set_parent(&parent)
            .send()
            .await
        {
            Ok(l) => l.certificates,
            Err(e) if absent(&e) => return Ok(stale),
            Err(e) => return Err(api_error(e, "listing certificates")),
        };
        for c in certs
            .into_iter()
            .filter(|c| self.owns_labels(&c.labels) && !is_wanted(&c.name))
        {
            r.change(format!("- certificate {}", last(&c.name)));
            if self.apply {
                self.c
                    .certs
                    .delete_certificate()
                    .set_name(&c.name)
                    .poller()
                    .until_done()
                    .await
                    .map_err(|e| api_error(e, "deleting a certificate"))?;
            }
        }
        let auths = match self
            .c
            .certs
            .list_dns_authorizations()
            .set_parent(&parent)
            .send()
            .await
        {
            Ok(l) => l.dns_authorizations,
            Err(e) if absent(&e) => return Ok(stale),
            Err(e) => return Err(api_error(e, "listing DNS authorizations")),
        };
        for a in auths.into_iter().filter(|a| {
            self.owns_labels(&a.labels) && !wanted_auths.iter().any(|w| last(&a.name) == w)
        }) {
            r.change(format!("- DNS authorization for {}", a.domain));
            if let Some(rec) = &a.dns_resource_record {
                stale.push((rec.name.clone(), rec.r#type.clone(), rec.data.clone()));
            }
            if self.apply {
                self.c
                    .certs
                    .delete_dns_authorization()
                    .set_name(&a.name)
                    .poller()
                    .until_done()
                    .await
                    .map_err(|e| api_error(e, "deleting a DNS authorization"))?;
            }
        }
        // The map runway created, once no domain needs it.
        if !(want.mode == DomainMode::LoadBalancer && want.load_balanced()) {
            let name = format!("{parent}/certificateMaps/{}", n.certificate_map);
            if let Ok(cm) = self
                .c
                .certs
                .get_certificate_map()
                .set_name(&name)
                .send()
                .await
                && self.owns_labels(&cm.labels)
            {
                r.change(format!("- certificate map {}", n.certificate_map));
                if self.apply {
                    self.c
                        .certs
                        .delete_certificate_map()
                        .set_name(&name)
                        .poller()
                        .until_done()
                        .await
                        .map_err(|e| api_error(e, "deleting the certificate map"))?;
                }
            }
        }
        Ok(stale)
    }

    // -------------------------------------------------------------- DNS

    /// Creates the record sets in the Cloud DNS zone when allowed, else
    /// reports them for someone to create; deletes `stale` record sets whose
    /// data is exactly what runway set (records carry no owner).
    async fn dns(&self, want: &Desired, records: Vec<Rr>, r: &mut Report) -> Result<()> {
        let wanted = record_sets(&records);
        let mut push = |name: &str, kind: &str, data: &[String], state: String, manual: bool| {
            r.records.push(Record {
                name: name.to_string(),
                kind: kind.to_string(),
                data: data.join(" "),
                state,
                manual,
            })
        };
        let mut changes: Vec<String> = Vec::new();
        match &want.dns {
            None => {
                for ((name, kind), data) in &wanted {
                    push(
                        name,
                        kind,
                        data,
                        "create it at your DNS provider (no `domains.dns` zone)".into(),
                        true,
                    );
                }
            }
            Some(zone) => {
                for ((name, kind), data) in &wanted {
                    if data.iter().any(|d| d.starts_with('(')) {
                        push(
                            name,
                            kind,
                            data,
                            "created by deploy once known".into(),
                            false,
                        );
                        continue;
                    }
                    let fqdn = format!("{name}.");
                    let live = self
                        .c
                        .dns
                        .get()
                        .set_project(&zone.project)
                        .set_managed_zone(&zone.zone)
                        .set_name(&fqdn)
                        .set_type(kind)
                        .send()
                        .await;
                    let elsewhere = |why: &str| {
                        format!(
                            "{why} zone {} (project {}): create it elsewhere",
                            zone.zone, zone.project
                        )
                    };
                    match live {
                        Ok(rr) if sorted(&rr.rrdatas) == *data => {
                            push(name, kind, data, "in sync".into(), false)
                        }
                        Ok(rr) => push(
                            name,
                            kind,
                            data,
                            format!(
                                "zone {} has other data ({}); runway does not overwrite records it did not set: change it yourself",
                                zone.zone,
                                rr.rrdatas.join(" ")
                            ),
                            true,
                        ),
                        Err(e) if is_not_found(&e) => {
                            changes.push(format!("+ DNS {kind} {name} -> {}", data.join(" ")));
                            if !self.apply {
                                push(name, kind, data, "would create".into(), false);
                                continue;
                            }
                            let body = ResourceRecordSet::new()
                                .set_name(&fqdn)
                                .set_type(kind)
                                .set_ttl(300)
                                .set_rrdatas(data.clone());
                            match self
                                .c
                                .dns
                                .create()
                                .set_project(&zone.project)
                                .set_managed_zone(&zone.zone)
                                .set_body(body)
                                .send()
                                .await
                            {
                                Ok(_) => push(name, kind, data, "created".into(), false),
                                Err(e) if denied(&e) => push(
                                    name,
                                    kind,
                                    data,
                                    elsewhere("runway may not write to"),
                                    true,
                                ),
                                Err(e) if e.http_status_code() == Some(400) => push(
                                    name,
                                    kind,
                                    data,
                                    format!(
                                        "not accepted by zone {} (is {name} in it?): create it elsewhere",
                                        zone.zone
                                    ),
                                    true,
                                ),
                                Err(e) => {
                                    return Err(api_error(
                                        e,
                                        &format!("creating DNS record {name}"),
                                    ));
                                }
                            }
                        }
                        Err(e) if denied(&e) => {
                            push(name, kind, data, elsewhere("runway may not read"), true)
                        }
                        Err(e) => return Err(api_error(e, &format!("reading DNS record {name}"))),
                    }
                }
            }
        }
        for c in changes {
            r.change(c);
        }
        Ok(())
    }

    /// Deletes `stale` record sets, in the zone runway wrote them to, whose
    /// data is exactly what runway set and that no wanted record (`keep`)
    /// uses.
    async fn delete_stale(
        &self,
        want: &Desired,
        previous: &Recorded,
        stale: &[Rr],
        keep: &[(String, String)],
        r: &mut Report,
    ) -> Result<()> {
        let Some(zone) = want.dns.as_ref().or(previous.dns.as_ref()) else {
            return Ok(());
        };
        for ((name, kind), data) in record_sets(stale) {
            if keep.contains(&(name.clone(), kind.clone())) || data.iter().any(|d| d.is_empty()) {
                continue;
            }
            let fqdn = format!("{name}.");
            let rr = match self
                .c
                .dns
                .get()
                .set_project(&zone.project)
                .set_managed_zone(&zone.zone)
                .set_name(&fqdn)
                .set_type(&kind)
                .send()
                .await
            {
                Ok(rr) => rr,
                Err(e) if is_not_found(&e) => continue,
                Err(e) if denied(&e) => {
                    r.notes.push(format!(
                        "DNS: check {kind} {name} -> {} and delete it yourself if it is still there (runway may not read zone {})",
                        data.join(" "),
                        zone.zone
                    ));
                    continue;
                }
                // Anything else stops here, before what the record points at
                // is deleted: the next run still knows the record.
                Err(e) => return Err(api_error(e, &format!("reading DNS record {name}"))),
            };
            if sorted(&rr.rrdatas) != data {
                continue;
            }
            r.change(format!("- DNS {kind} {name}"));
            if self.apply {
                match self
                    .c
                    .dns
                    .delete()
                    .set_project(&zone.project)
                    .set_managed_zone(&zone.zone)
                    .set_name(&fqdn)
                    .set_type(&kind)
                    .send()
                    .await
                {
                    Ok(_) => {}
                    Err(e) if denied(&e) => r.notes.push(format!(
                        "DNS: delete {kind} {name} yourself (runway may not write to zone {})",
                        zone.zone
                    )),
                    Err(e) => return Err(api_error(e, &format!("deleting DNS record {name}"))),
                }
            }
        }
        Ok(())
    }
}

/// `(name, type)` of the records a report wants.
fn record_keys(r: &Report) -> Vec<(String, String)> {
    r.records
        .iter()
        .map(|x| (x.name.clone(), x.kind.clone()))
        .collect()
}

fn sorted(v: &[String]) -> Vec<String> {
    let mut v = v.to_vec();
    v.sort();
    v
}

/// Missing, or its API is not enabled yet (a first `plan`): nothing to read.
fn absent(e: &GaxError) -> bool {
    is_not_found(e) || crate::gcp::is_service_disabled(e)
}

fn denied(e: &GaxError) -> bool {
    matches!(
        status_code(e),
        Some(Code::PermissionDenied | Code::Unauthenticated)
    ) || e.http_status_code() == Some(403)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(yaml_tail: &str) -> (tempfile::TempDir, Resolved) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("runway.yaml");
        std::fs::write(
            &p,
            format!(
                "version: 1\napp: shop\nprovider: {{project: my-gcp-project, region: europe-west1}}\ndefaults:\n  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop:1\n  service_account: rt@my-gcp-project.iam.gserviceaccount.com\n{yaml_tail}\nstages: {{prod: {{}}}}\n"
            ),
        )
        .unwrap();
        let (_, r) = crate::config::load_and_resolve(&p, "prod", &Default::default()).unwrap();
        (dir, r)
    }

    #[test]
    fn hosts_and_paths_route_to_their_services() {
        let (_d, r) = resolved(
            "service:\n  domains: [example.com, my-shop.cloud.run]\nservices:\n  api:\n    domains: [example.com/api, api.example.com]\n    preview_domain: \"*.preview.example.com\"\n",
        );
        let d = desired(&r);
        assert!(configured(&r));
        assert_eq!(d.prefix, "shop-prod");
        assert_eq!(
            d.mappings,
            [("my-shop.cloud.run".to_string(), "shop-prod".to_string())]
        );
        let host = |h: &str| d.hosts.iter().find(|x| x.host == h).unwrap();
        assert_eq!(host("example.com").default_backend, "shop-prod-be");
        assert_eq!(
            host("example.com").paths,
            [(
                vec!["/api".to_string(), "/api/*".to_string()],
                "shop-api-prod-be".to_string()
            )]
        );
        assert_eq!(host("api.example.com").default_backend, "shop-api-prod-be");
        let preview = host("*.preview.example.com");
        let pb = d
            .backends
            .iter()
            .find(|b| b.backend == preview.default_backend)
            .unwrap();
        assert_eq!(pb.url_mask.as_deref(), Some("<tag>.preview.example.com"));
        assert_eq!(pb.service_id, "shop-api-prod");
        assert_eq!(
            d.cert_domains,
            ["*.preview.example.com", "api.example.com", "example.com"],
            "every host, the preview wildcard too; not the custom URL"
        );
    }

    #[test]
    fn domain_mapping_mode_maps_whole_hosts() {
        let (_d, r) =
            resolved("domains: {mode: domain-mapping}\nservice:\n  domains: [shop.example.com]\n");
        let d = desired(&r);
        assert!(d.hosts.is_empty() && d.cert_domains.is_empty() && !d.load_balanced());
        let rec = Recorded::of(&d);
        assert!(
            rec.mappings && !rec.load_balancer && !rec.certificates,
            "removals will only look for mappings"
        );
        assert_eq!(
            d.mappings,
            [("shop.example.com".to_string(), "shop-prod".to_string())]
        );
    }

    #[test]
    fn runway_s_routes_replace_its_own_and_keep_the_others() {
        let marker = crate::gcp::scheduler::marker("shop", "prod");
        let theirs = HostRule::new()
            .set_hosts(["blog.example.com".to_string()])
            .set_path_matcher("blog")
            .set_description("platform");
        let old_ours = HostRule::new()
            .set_hosts(["old.example.com".to_string()])
            .set_path_matcher("rw-old")
            .set_description(&marker);
        let live = UrlMap::new()
            .set_name("shared")
            .set_host_rules([theirs.clone(), old_ours])
            .set_path_matchers([
                PathMatcher::new()
                    .set_name("blog")
                    .set_description("platform"),
                PathMatcher::new()
                    .set_name("rw-old")
                    .set_description(&marker),
            ]);
        let hosts = [HostSpec {
            host: "shop.example.com".into(),
            default_backend: "shop-prod-be".into(),
            paths: vec![],
        }];
        let (rules, matchers) = routes("my-gcp-project", "shop-prod", &marker, &hosts);
        let (all_rules, all_matchers) = merge_routes(&live, &marker, rules, matchers).unwrap();
        let hosts_of: Vec<Vec<String>> = all_rules.iter().map(|h| h.hosts.clone()).collect();
        assert_eq!(
            hosts_of,
            [
                vec!["blog.example.com".to_string()],
                vec!["shop.example.com".to_string()]
            ]
        );
        assert_eq!(all_matchers.len(), 2);
        assert_eq!(
            all_matchers[1].default_service.as_deref(),
            Some("projects/my-gcp-project/global/backendServices/shop-prod-be")
        );

        // Removing every route keeps the platform's.
        let (rules, matchers) = merge_routes(&live, &marker, vec![], vec![]).unwrap();
        assert_eq!((rules, matchers.len()), (vec![theirs], 1));

        // A host someone else routes is refused.
        let clash = [HostSpec {
            host: "blog.example.com".into(),
            default_backend: "shop-prod-be".into(),
            paths: vec![],
        }];
        let (rules, matchers) = routes("my-gcp-project", "shop-prod", &marker, &clash);
        let e = merge_routes(&live, &marker, rules, matchers).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Conflict);
    }

    #[test]
    fn route_comparison_ignores_url_forms() {
        let marker = "m";
        let hosts = [HostSpec {
            host: "a.example.com".into(),
            default_backend: "be".into(),
            paths: vec![],
        }];
        let (r1, m1) = routes("p", "x", marker, &hosts);
        let m2: Vec<PathMatcher> = m1
            .iter()
            .cloned()
            .map(|m| {
                m.set_default_service(
                    "https://www.googleapis.com/compute/v1/projects/p/global/backendServices/be",
                )
            })
            .collect();
        assert_eq!(routes_key(&r1, &m1), routes_key(&r1, &m2));
    }

    #[test]
    fn records_group_into_record_sets() {
        let rr = |n: &str, k: &str, d: &str| (n.to_string(), k.to_string(), d.to_string());
        let sets = record_sets(&[
            rr("example.com", "A", "216.239.38.21"),
            rr("example.com.", "A", "216.239.32.21"),
            rr(
                "_acme-challenge.example.com",
                "CNAME",
                "x.authorize.certificatemanager.goog",
            ),
            rr(
                "_acme-challenge.example.com",
                "CNAME",
                "x.authorize.certificatemanager.goog.",
            ),
        ]);
        assert_eq!(
            sets[&("example.com".to_string(), "A".to_string())],
            ["216.239.32.21", "216.239.38.21"],
            "an apex mapping's addresses are one record set"
        );
        assert_eq!(
            sets[&(
                "_acme-challenge.example.com".to_string(),
                "CNAME".to_string()
            )],
            ["x.authorize.certificatemanager.goog."],
            "a host shared by a domain and its wildcard is authorized once"
        );
    }

    #[test]
    fn only_runway_s_hosts_can_be_dropped() {
        let marker = crate::gcp::scheduler::marker("shop", "prod");
        let live = UrlMap::new().set_host_rules([
            HostRule::new()
                .set_hosts(["old.example.com".to_string()])
                .set_description(&marker),
            HostRule::new()
                .set_hosts(["kept.example.com".to_string()])
                .set_description(&marker),
            HostRule::new().set_hosts(["blog.example.com".to_string()]),
        ]);
        let hosts = [HostSpec {
            host: "kept.example.com".into(),
            default_backend: "be".into(),
            paths: vec![],
        }];
        assert_eq!(dropped_hosts(&live, &marker, &hosts), ["old.example.com"]);
    }

    #[test]
    fn the_record_of_what_was_set_up_round_trips() {
        let r = Recorded {
            url_maps: vec!["shared".into()],
            address: Some("203.0.113.10".into()),
            dns: Some(DnsZone {
                zone: "example-com".into(),
                project: "my-gcp-project".into(),
            }),
            ..Default::default()
        };
        assert_eq!(Recorded::decode(&r.encode()), r);
        let other = Recorded {
            url_maps: vec!["new".into(), "shared".into()],
            certificate_maps: vec!["certs".into()],
            ..Default::default()
        };
        let both = r.union(&other);
        assert_eq!(both.url_maps, ["new", "shared"]);
        assert_eq!(both.certificate_maps, ["certs"]);
        assert_eq!(
            both.address.as_deref(),
            Some("203.0.113.10"),
            "kept until replaced"
        );
        assert_eq!(Recorded::default().encode(), "{}");
        assert_eq!(Recorded::decode("not json"), Recorded::default());
    }
}
