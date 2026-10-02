//! Cloud Storage bucket specification, drift detection and patches.
//!
//! Buckets created by runway always use uniform bucket-level access and
//! enforced public access prevention. Labels are added (never removed), and
//! lifecycle rules other than runway's age-based delete rule are preserved.

use crate::config::BucketConfig;
use crate::naming;
use google_cloud_storage::model::Bucket;
use google_cloud_storage::model::bucket::iam_config::UniformBucketLevelAccess;
use google_cloud_storage::model::bucket::lifecycle::Rule;
use google_cloud_storage::model::bucket::lifecycle::rule::{Action, Condition};
use google_cloud_storage::model::bucket::{IamConfig, Lifecycle, Versioning};
use std::collections::BTreeMap;

pub const PAP_ENFORCED: &str = "enforced";

pub fn bucket_resource(name: &str) -> String {
    format!("projects/_/buckets/{name}")
}

fn delete_rule(days: u32) -> Rule {
    Rule::new()
        .set_action(Action::new().set_type("Delete"))
        .set_condition(Condition::new().set_age_days(days as i32))
}

/// A rule that only deletes objects by age (the kind runway manages).
fn is_age_delete(r: &Rule) -> Option<i32> {
    let action = r.action.as_ref()?;
    let cond = r.condition.as_ref()?;
    let only_age = Condition::new().set_age_days(cond.age_days?);
    (action.r#type == "Delete" && *cond == only_age).then_some(cond.age_days?)
}

/// Why runway must not modify an existing bucket (`None`: runway created it
/// for this app). Updates could otherwise rewrite another owner's labels,
/// access settings or lifecycle rules.
pub fn not_owned_reason(live: &Bucket, app: &str) -> Option<String> {
    let managed = live
        .labels
        .get(naming::LABEL_MANAGED_BY)
        .map(String::as_str)
        == Some(naming::LABEL_MANAGED_BY_VALUE);
    match live.labels.get(naming::LABEL_APP) {
        _ if !managed => Some("not managed by runway".into()),
        Some(a) if a == app => None,
        Some(a) => Some(format!("managed by runway for app `{a}`")),
        None => Some("managed by runway without an app label".into()),
    }
}

/// Labels to apply: the configured ones plus runway's ownership labels.
pub fn desired_labels(cfg: &BucketConfig, app: &str) -> BTreeMap<String, String> {
    let mut l = cfg.labels.clone();
    l.insert(
        crate::naming::LABEL_MANAGED_BY.into(),
        crate::naming::LABEL_MANAGED_BY_VALUE.into(),
    );
    l.insert(crate::naming::LABEL_APP.into(), app.into());
    l
}

fn iam_config() -> IamConfig {
    IamConfig::new()
        .set_uniform_bucket_level_access(UniformBucketLevelAccess::new().set_enabled(true))
        .set_public_access_prevention(PAP_ENFORCED)
}

/// The bucket sent on creation.
pub fn new_bucket(cfg: &BucketConfig, project: &str, app: &str) -> Bucket {
    let mut b = Bucket::new()
        .set_project(format!("projects/{project}"))
        .set_location(cfg.location.to_ascii_uppercase())
        .set_iam_config(iam_config())
        .set_labels(desired_labels(cfg, app));
    if let Some(sc) = &cfg.storage_class {
        b = b.set_storage_class(sc);
    }
    if let Some(v) = cfg.versioning {
        b = b.set_versioning(Versioning::new().set_enabled(v));
    }
    if let Some(days) = cfg.delete_after_days {
        b = b.set_lifecycle(Lifecycle::new().set_rule([delete_rule(days)]));
    }
    b
}

/// Mutable settings that differ from the configuration (update-mask paths).
pub fn drift(live: &Bucket, cfg: &BucketConfig, app: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let iam_ok = live.iam_config.as_ref().is_some_and(|c| {
        c.public_access_prevention == PAP_ENFORCED
            && c.uniform_bucket_level_access
                .as_ref()
                .is_some_and(|u| u.enabled)
    });
    if !iam_ok {
        out.push("iam_config");
    }
    if desired_labels(cfg, app)
        .iter()
        .any(|(k, v)| live.labels.get(k) != Some(v))
    {
        out.push("labels");
    }
    if let Some(days) = cfg.delete_after_days {
        let ages: Vec<i32> = live
            .lifecycle
            .as_ref()
            .map(|l| l.rule.iter().filter_map(is_age_delete).collect())
            .unwrap_or_default();
        if ages != [days as i32] {
            out.push("lifecycle");
        }
    }
    if let Some(v) = cfg.versioning
        && live.versioning.as_ref().map(|x| x.enabled).unwrap_or(false) != v
    {
        out.push("versioning");
    }
    if let Some(sc) = &cfg.storage_class
        && !live.storage_class.eq_ignore_ascii_case(sc)
    {
        out.push("storage_class");
    }
    out
}

/// The patch for the drifted fields: preserves foreign labels and lifecycle rules.
pub fn patch(live: &Bucket, cfg: &BucketConfig, app: &str, fields: &[&str]) -> Bucket {
    let mut b = Bucket::new().set_name(&live.name);
    for f in fields {
        match *f {
            "iam_config" => b = b.set_iam_config(iam_config()),
            "labels" => {
                let mut l: BTreeMap<String, String> = live
                    .labels
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                l.extend(desired_labels(cfg, app));
                b = b.set_labels(l);
            }
            "lifecycle" => {
                let mut rules: Vec<Rule> = live
                    .lifecycle
                    .as_ref()
                    .map(|l| {
                        l.rule
                            .iter()
                            .filter(|r| is_age_delete(r).is_none())
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(days) = cfg.delete_after_days {
                    rules.push(delete_rule(days));
                }
                b = b.set_lifecycle(Lifecycle::new().set_rule(rules));
            }
            "versioning" => {
                b = b.set_versioning(Versioning::new().set_enabled(cfg.versioning.unwrap_or(false)))
            }
            "storage_class" => {
                b = b.set_storage_class(cfg.storage_class.clone().unwrap_or_default())
            }
            _ => {}
        }
    }
    b
}

pub fn same_location(live: &Bucket, cfg: &BucketConfig) -> bool {
    live.location.eq_ignore_ascii_case(&cfg.location)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BucketConfig {
        BucketConfig {
            key: "cache".into(),
            name: "p-gcptree-cache".into(),
            location: "europe-west1".into(),
            storage_class: None,
            versioning: None,
            delete_after_days: Some(30),
            labels: BTreeMap::from([("team".into(), "finops".into())]),
        }
    }

    #[test]
    fn only_buckets_runway_created_for_this_app_are_modified() {
        let labeled = |l: &[(&str, &str)]| {
            Bucket::new().set_labels(l.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        };
        assert_eq!(
            not_owned_reason(
                &labeled(&[("managed-by", "runway"), ("runway-app", "gcptree")]),
                "gcptree"
            ),
            None
        );
        assert!(
            not_owned_reason(&labeled(&[]), "gcptree")
                .unwrap()
                .contains("not managed")
        );
        assert!(
            not_owned_reason(&labeled(&[("runway-app", "gcptree")]), "gcptree").is_some(),
            "labels must include managed-by"
        );
        assert!(
            not_owned_reason(
                &labeled(&[("managed-by", "runway"), ("runway-app", "other")]),
                "gcptree"
            )
            .unwrap()
            .contains("`other`")
        );
    }

    #[test]
    fn new_bucket_is_private_uniform_and_labeled() {
        let b = new_bucket(&cfg(), "acme-sandbox-26c8", "gcptree");
        assert_eq!(b.project, "projects/acme-sandbox-26c8");
        assert_eq!(b.location, "EUROPE-WEST1");
        let iam = b.iam_config.as_ref().unwrap();
        assert_eq!(iam.public_access_prevention, "enforced");
        assert!(iam.uniform_bucket_level_access.as_ref().unwrap().enabled);
        assert_eq!(b.labels["managed-by"], "runway");
        assert_eq!(b.labels["runway-app"], "gcptree");
        assert_eq!(b.labels["team"], "finops");
        let rule = &b.lifecycle.as_ref().unwrap().rule[0];
        assert_eq!(is_age_delete(rule), Some(30));
    }

    #[test]
    fn freshly_created_bucket_has_no_drift() {
        let mut live = new_bucket(&cfg(), "p", "gcptree");
        live.name = bucket_resource("p-gcptree-cache");
        assert!(drift(&live, &cfg(), "gcptree").is_empty());
        assert!(same_location(&live, &cfg()));
    }

    #[test]
    fn drift_and_patch_preserve_foreign_settings() {
        let foreign_rule = Rule::new()
            .set_action(
                Action::new()
                    .set_type("SetStorageClass")
                    .set_storage_class("NEARLINE"),
            )
            .set_condition(Condition::new().set_age_days(90));
        let live = Bucket::new()
            .set_name("projects/_/buckets/p-gcptree-cache")
            .set_location("EUROPE-WEST1")
            .set_labels([("owner", "alice")])
            .set_lifecycle(Lifecycle::new().set_rule([foreign_rule.clone(), delete_rule(7)]));
        let fields = drift(&live, &cfg(), "gcptree");
        assert_eq!(fields, ["iam_config", "labels", "lifecycle"]);
        let p = patch(&live, &cfg(), "gcptree", &fields);
        assert_eq!(p.labels["owner"], "alice", "foreign labels kept");
        assert_eq!(p.labels["managed-by"], "runway");
        let rules = &p.lifecycle.as_ref().unwrap().rule;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], foreign_rule, "foreign rules kept");
        assert_eq!(is_age_delete(&rules[1]), Some(30), "old age rule replaced");
        assert_eq!(
            p.iam_config.as_ref().unwrap().public_access_prevention,
            "enforced"
        );
    }

    #[test]
    fn unset_options_are_not_managed() {
        let mut c = cfg();
        c.delete_after_days = None;
        let mut live = new_bucket(&c, "p", "gcptree");
        live = live.set_lifecycle(Lifecycle::new().set_rule([delete_rule(3)]));
        assert!(
            drift(&live, &c, "gcptree").is_empty(),
            "lifecycle untouched when not configured"
        );
    }
}
