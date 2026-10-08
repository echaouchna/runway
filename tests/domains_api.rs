//! Wire-level tests of custom domains: Cloud Run domain mappings (REST),
//! routes added to a load balancer runway does not own, and DNS records.
//! Real SDK clients against a mock server.

use runway::config::{self, Overrides, Resolved};
use runway::domains::{Endpoints, Engine, Recorded, desired};
use runway::gcp::Session;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MAPPINGS: &str = "/apis/domains.cloudrun.com/v1/namespaces/my-gcp-project/domainmappings";
const ZONE: &str = "/dns/v1/projects/my-dns-project/managedZones/example-com/rrsets";
const COMPUTE: &str = "/compute/v1/projects/my-gcp-project";

fn resolved(body: &str) -> (tempfile::TempDir, Resolved) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("runway.yaml");
    std::fs::write(
        &p,
        format!(
            r#"
version: 1
app: shop
provider: {{project: my-gcp-project, region: europe-west1}}
defaults:
  image: europe-west1-docker.pkg.dev/my-gcp-project/apps/shop:1
  service_account: rt@my-gcp-project.iam.gserviceaccount.com
{body}
stages: {{ prod: {{}} }}
"#
        ),
    )
    .unwrap();
    let (_, r) = config::load_and_resolve(&p, "prod", &Overrides::default()).unwrap();
    (dir, r)
}

fn ok(v: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}

fn status(code: u16, s: &str) -> ResponseTemplate {
    ResponseTemplate::new(code)
        .set_body_json(json!({"error": {"code": code, "message": s, "status": s}}))
}

/// Anything not mocked does not exist.
async fn server() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(status(404, "NOT_FOUND"))
        .with_priority(100)
        .mount(&s)
        .await;
    s
}

fn owned() -> Value {
    json!({"managed-by": "runway", "runway-app": "shop", "runway-stage": "prod"})
}

async fn engine<'a>(s: &MockServer, session: &'a Session, apply: bool) -> Engine<'a> {
    let ep = Endpoints {
        compute: Some(s.uri()),
        certificates: Some(s.uri()),
        dns: Some(s.uri()),
        run: Some(s.uri()),
    };
    Engine::new(
        session,
        "my-gcp-project",
        "europe-west1",
        "shop",
        "prod",
        apply,
        &ep,
    )
    .await
    .unwrap()
}

fn requests(all: &[Request], verb: &str, prefix: &str) -> Vec<Value> {
    all.iter()
        .filter(|r| r.method.as_str() == verb && r.url.path().starts_with(prefix))
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        .collect()
}

#[tokio::test]
async fn a_domain_mapping_is_planned_then_created_with_runway_s_labels() {
    let s = server().await;
    Mock::given(method("POST"))
        .and(path(MAPPINGS))
        .respond_with(ok(json!({})))
        .mount(&s)
        .await;
    let (_d, r) =
        resolved("domains: {mode: domain-mapping}\nservice: {domains: [shop.example.com]}");
    let want = desired(&r);
    let session = Session::from_static_token("t").unwrap();

    let plan = engine(&s, &session, false)
        .await
        .reconcile(&want, &Recorded::default(), false, true)
        .await
        .unwrap();
    assert_eq!(
        plan.changes,
        ["+ domain mapping shop.example.com -> shop-prod"]
    );
    let manual = plan.manual();
    assert_eq!(manual.len(), 1, "{:?}", plan.records);
    assert!(manual[0].state.contains("no `domains.dns` zone"));
    let all = s.received_requests().await.unwrap();
    assert!(
        requests(&all, "POST", MAPPINGS).is_empty(),
        "a plan writes nothing"
    );

    engine(&s, &session, true)
        .await
        .reconcile(&want, &Recorded::default(), false, true)
        .await
        .unwrap();
    let all = s.received_requests().await.unwrap();
    let created = requests(&all, "POST", MAPPINGS);
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["metadata"]["name"], "shop.example.com");
    assert_eq!(created[0]["metadata"]["labels"], owned());
    assert_eq!(created[0]["spec"]["routeName"], "shop-prod");
}

#[tokio::test]
async fn a_mapping_runway_did_not_create_is_refused() {
    let s = server().await;
    Mock::given(method("GET"))
        .and(path(format!("{MAPPINGS}/shop.example.com")))
        .respond_with(ok(
            json!({"metadata": {"name": "shop.example.com"}, "spec": {"routeName": "other"}}),
        ))
        .mount(&s)
        .await;
    let (_d, r) =
        resolved("domains: {mode: domain-mapping}\nservice: {domains: [shop.example.com]}");
    let session = Session::from_static_token("t").unwrap();
    let e = engine(&s, &session, true)
        .await
        .reconcile(&desired(&r), &Recorded::default(), false, true)
        .await
        .unwrap_err();
    assert_eq!(e.kind, runway::error::ErrorKind::Conflict);
    assert!(e.permanent);
}

#[tokio::test]
async fn records_runway_may_not_write_are_left_to_be_created_elsewhere() {
    let s = server().await;
    Mock::given(method("GET"))
        .and(path(format!("{MAPPINGS}/shop.example.com")))
        .respond_with(ok(json!({
            "metadata": {"name": "shop.example.com", "labels": owned()},
            "spec": {"routeName": "shop-prod"},
            "status": {"resourceRecords": [{"type": "CNAME", "rrdata": "ghs.googlehosted.com."}]}
        })))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(ZONE))
        .respond_with(status(403, "PERMISSION_DENIED"))
        .mount(&s)
        .await;
    let (_d, r) = resolved(
        "domains: {mode: domain-mapping, dns: {zone: example-com, project: my-dns-project}}\nservice: {domains: [shop.example.com]}",
    );
    let session = Session::from_static_token("t").unwrap();
    let report = engine(&s, &session, true)
        .await
        .reconcile(&desired(&r), &Recorded::default(), false, true)
        .await
        .unwrap();
    let manual = report.manual();
    assert_eq!(manual.len(), 1, "{:?}", report.records);
    assert_eq!(
        (
            manual[0].name.as_str(),
            manual[0].kind.as_str(),
            manual[0].data.as_str()
        ),
        ("shop.example.com", "CNAME", "ghs.googlehosted.com.")
    );
    assert!(
        manual[0].state.contains("may not write"),
        "{}",
        manual[0].state
    );
}

#[tokio::test]
async fn routes_go_into_an_existing_load_balancer_next_to_the_others() {
    let s = server().await;
    for p in [
        format!("{COMPUTE}/regions/europe-west1/networkEndpointGroups"),
        format!("{COMPUTE}/global/backendServices"),
    ] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(ok(json!({"name": "op", "status": "DONE"})))
            .mount(&s)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!("{COMPUTE}/global/urlMaps/shared-lb")))
        .respond_with(ok(json!({
            "name": "shared-lb",
            "fingerprint": "Zm9v",
            "defaultService": "projects/my-platform/global/backendServices/default",
            "hostRules": [{"hosts": ["blog.example.com"], "pathMatcher": "blog"}],
            "pathMatchers": [{"name": "blog", "defaultService": "projects/my-gcp-project/global/backendServices/blog"}]
        })))
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("{COMPUTE}/global/urlMaps/shared-lb")))
        .respond_with(ok(json!({"name": "op", "status": "DONE"})))
        .mount(&s)
        .await;
    let (_d, r) = resolved(
        "domains:\n  mode: existing-load-balancer\n  load_balancer: {url_map: shared-lb}\nservice:\n  domains: [shop.example.com, shop.example.com/api]\n",
    );
    let session = Session::from_static_token("t").unwrap();
    let report = engine(&s, &session, true)
        .await
        .reconcile(&desired(&r), &Recorded::default(), false, true)
        .await
        .unwrap();
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("load_balancer.address")),
        "{:?}",
        report.notes
    );

    let all = s.received_requests().await.unwrap();
    let neg = &requests(&all, "POST", &format!("{COMPUTE}/regions"))[0];
    assert_eq!(neg["networkEndpointType"], "SERVERLESS");
    assert_eq!(neg["cloudRun"]["service"], "shop-prod");
    let backend = &requests(&all, "POST", &format!("{COMPUTE}/global/backendServices"))[0];
    assert_eq!(backend["loadBalancingScheme"], "EXTERNAL_MANAGED");
    let put = &requests(&all, "PUT", &format!("{COMPUTE}/global/urlMaps"))[0];
    assert_eq!(put["fingerprint"], "Zm9v", "the update is conditional");
    assert_eq!(
        put["defaultService"], "projects/my-platform/global/backendServices/default",
        "the platform's default is kept"
    );
    let hosts: Vec<&Value> = put["hostRules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| &h["hosts"][0])
        .collect();
    assert_eq!(
        hosts,
        [&json!("blog.example.com"), &json!("shop.example.com")]
    );
    let ours = &put["pathMatchers"][1];
    assert!(
        ours["description"]
            .as_str()
            .unwrap()
            .contains("managed-by=runway")
    );
    assert_eq!(ours["pathRules"][0]["paths"], json!(["/api", "/api/*"]));
}

/// Mappings runway created (two with records), a custom URL, and someone
/// else's mapping; `old.example.com`'s record is still runway's, the other
/// was repointed. The record is found for `dns_reads` reads, then deleted.
async fn mapping_removal_server(dns_reads: u64, dns_delete: ResponseTemplate) -> MockServer {
    let s = server().await;
    Mock::given(method("GET"))
        .and(path(MAPPINGS))
        .respond_with(ok(json!({"items": [
            {"metadata": {"name": "old.example.com", "labels": owned()}, "spec": {"routeName": "shop-prod"},
             "status": {"resourceRecords": [{"type": "CNAME", "rrdata": "ghs.googlehosted.com."}]}},
            {"metadata": {"name": "moved.example.com", "labels": owned()}, "spec": {"routeName": "shop-prod"},
             "status": {"resourceRecords": [{"type": "CNAME", "rrdata": "ghs.googlehosted.com."}]}},
            {"metadata": {"name": "my-shop.cloud.run", "labels": owned()}, "spec": {"routeName": "shop-prod"}},
            {"metadata": {"name": "theirs.example.com"}, "spec": {"routeName": "shop-prod"}}
        ]})))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{ZONE}/old.example.com./CNAME")))
        .respond_with(dns_delete)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ok(json!({})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{ZONE}/old.example.com./CNAME")))
        .respond_with(ok(json!({"name": "old.example.com.", "type": "CNAME", "rrdatas": ["ghs.googlehosted.com."]})))
        .up_to_n_times(dns_reads)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{ZONE}/moved.example.com./CNAME")))
        .respond_with(ok(json!({"name": "moved.example.com.", "type": "CNAME", "rrdatas": ["elsewhere.example.net."]})))
        .mount(&s)
        .await;
    s
}

/// runway.yaml has no domains left; the record says runway managed domain
/// mappings, and names the zone.
fn mappings_record() -> Recorded {
    Recorded {
        mappings: true,
        dns: Some(runway::config::DnsZone {
            zone: "example-com".into(),
            project: "my-dns-project".into(),
        }),
        ..Default::default()
    }
}

fn deleted(all: &[Request]) -> Vec<String> {
    all.iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .map(|r| r.url.path().to_string())
        .collect()
}

#[tokio::test]
async fn removed_mappings_take_their_records_first_and_only_runway_s() {
    // Read by the read-only pass and before deleting; gone afterwards.
    let s = mapping_removal_server(2, ok(json!({}))).await;
    let (_d, r) = resolved("service: {}");
    let session = Session::from_static_token("t").unwrap();
    let report = engine(&s, &session, true)
        .await
        .reconcile(&desired(&r), &mappings_record(), true, true)
        .await
        .unwrap();
    assert_eq!(
        report.changes,
        [
            "- DNS CNAME old.example.com",
            "- domain mapping old.example.com",
            "- domain mapping moved.example.com",
        ]
    );
    assert_eq!(
        report.kept.len(),
        1,
        "the custom URL stays: {:?}",
        report.kept
    );
    let all = s.received_requests().await.unwrap();
    assert_eq!(
        deleted(&all),
        [
            format!("{ZONE}/old.example.com./CNAME"),
            format!("{MAPPINGS}/old.example.com"),
            format!("{MAPPINGS}/moved.example.com"),
        ],
        "the record before its mapping; not someone else's mapping, nor a record pointing elsewhere"
    );
    assert!(
        all.iter().all(|r| !r.url.path().starts_with("/compute")
            && !r.url.path().contains("/locations/global/")),
        "a stage with domain mappings reads no load balancer nor certificate"
    );
}

#[tokio::test]
async fn a_failed_record_deletion_keeps_the_mapping_for_the_next_run() {
    let s = mapping_removal_server(10, status(500, "INTERNAL")).await;
    let (_d, r) = resolved("service: {}");
    let session = Session::from_static_token("t").unwrap();
    let e = engine(&s, &session, true)
        .await
        .reconcile(&desired(&r), &mappings_record(), true, true)
        .await;
    assert!(e.is_err());
    let all = s.received_requests().await.unwrap();
    assert!(deleted(&all).contains(&format!("{ZONE}/old.example.com./CNAME")));
    assert!(
        !deleted(&all).iter().any(|p| p.starts_with(MAPPINGS)),
        "no mapping deleted: the next run still knows its records ({:?})",
        deleted(&all)
    );
}

#[tokio::test]
async fn a_half_removed_load_balancer_is_still_removed() {
    let s = server().await;
    // The URL map is gone (an earlier removal failed after deleting it); the
    // HTTPS proxy and the IP address are left, and a proxy runway did not
    // create has the HTTP proxy's name.
    let marker = runway::gcp::scheduler::marker("shop", "prod");
    Mock::given(method("GET"))
        .and(path(format!(
            "{COMPUTE}/global/targetHttpsProxies/shop-prod-https"
        )))
        .respond_with(ok(
            json!({"name": "shop-prod-https", "description": marker}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{COMPUTE}/global/targetHttpProxies/shop-prod-http"
        )))
        .respond_with(ok(
            json!({"name": "shop-prod-http", "description": "made by hand"}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{COMPUTE}/global/addresses/shop-prod-ip")))
        .respond_with(ok(
            json!({"name": "shop-prod-ip", "address": "203.0.113.7", "description": marker}),
        ))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ok(json!({"name": "op", "status": "DONE"})))
        .mount(&s)
        .await;
    let (_d, r) = resolved("service: {}");
    let previous = Recorded {
        load_balancer: true,
        ..Default::default()
    };
    let session = Session::from_static_token("t").unwrap();
    let report = engine(&s, &session, true)
        .await
        .reconcile(&desired(&r), &previous, true, true)
        .await
        .unwrap();
    assert_eq!(
        report.changes,
        [
            "- HTTPS proxy shop-prod-https",
            "- global IP address shop-prod-ip"
        ]
    );
    let all = s.received_requests().await.unwrap();
    assert_eq!(
        deleted(&all),
        [
            format!("{COMPUTE}/global/targetHttpsProxies/shop-prod-https"),
            format!("{COMPUTE}/global/addresses/shop-prod-ip"),
        ]
    );
}

#[tokio::test]
async fn a_failed_record_lookup_keeps_the_mapping_for_the_next_run() {
    let s = mapping_removal_server(10, ok(json!({}))).await;
    Mock::given(method("GET"))
        .and(path(format!("{ZONE}/old.example.com./CNAME")))
        .respond_with(status(500, "INTERNAL"))
        .with_priority(1)
        .mount(&s)
        .await;
    let (_d, r) = resolved("service: {}");
    let session = Session::from_static_token("t").unwrap();
    let e = engine(&s, &session, true)
        .await
        .reconcile(&desired(&r), &mappings_record(), true, true)
        .await
        .unwrap_err();
    assert!(
        e.message.contains("reading DNS record old.example.com"),
        "{}",
        e.message
    );
    let all = s.received_requests().await.unwrap();
    assert!(
        deleted(&all).is_empty(),
        "nothing deleted: the next run still knows the record ({:?})",
        deleted(&all)
    );
}
