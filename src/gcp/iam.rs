//! Invoker IAM policy edits. Only the unconditional `allUsers` member of
//! `roles/run.invoker` is managed; every other binding, member, condition and
//! audit config is preserved exactly.

use google_cloud_iam_v1::model::{Binding, Policy};

pub const INVOKER_ROLE: &str = "roles/run.invoker";
pub const PUBLIC_MEMBER: &str = "allUsers";

pub fn is_public(policy: &Policy) -> bool {
    policy.bindings.iter().any(|b| {
        b.role == INVOKER_ROLE
            && b.condition.is_none()
            && b.members.iter().any(|m| m == PUBLIC_MEMBER)
    })
}

/// Adds or removes `allUsers` from `roles/run.invoker`. Returns true if the policy changed.
pub fn set_public(policy: &mut Policy, public: bool) -> bool {
    if is_public(policy) == public {
        return false;
    }
    if public {
        if let Some(b) = policy
            .bindings
            .iter_mut()
            .find(|b| b.role == INVOKER_ROLE && b.condition.is_none())
        {
            b.members.push(PUBLIC_MEMBER.to_string());
        } else {
            policy.bindings.push(
                Binding::new()
                    .set_role(INVOKER_ROLE)
                    .set_members([PUBLIC_MEMBER]),
            );
        }
    } else {
        for b in policy
            .bindings
            .iter_mut()
            .filter(|b| b.role == INVOKER_ROLE && b.condition.is_none())
        {
            b.members.retain(|m| m != PUBLIC_MEMBER);
        }
        policy.bindings.retain(|b| !b.members.is_empty());
    }
    true
}

/// IAM compares principals case-insensitively and returns emails in lowercase,
/// so `group:Devs@example.com` and `group:devs@example.com` are the same.
pub fn same_member(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Members of `role` in unconditional bindings (conditional ones are not
/// runway's to manage).
pub fn members_of(policy: &Policy, role: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in policy
        .bindings
        .iter()
        .filter(|b| b.role == role && b.condition.is_none())
    {
        for m in &b.members {
            if !out.iter().any(|o| same_member(o, m)) {
                out.push(m.clone());
            }
        }
    }
    out
}

/// Roles `member` holds in unconditional bindings.
pub fn roles_of(policy: &Policy, member: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in policy
        .bindings
        .iter()
        .filter(|b| b.condition.is_none() && b.members.iter().any(|m| same_member(m, member)))
    {
        if !out.contains(&b.role) {
            out.push(b.role.clone());
        }
    }
    out
}

/// Members of `role` (unconditional bindings only) that are missing from the policy.
pub fn missing_members(policy: &Policy, role: &str, members: &[String]) -> Vec<String> {
    members
        .iter()
        .filter(|m| {
            !policy.bindings.iter().any(|b| {
                b.role == role
                    && b.condition.is_none()
                    && b.members.iter().any(|x| same_member(x, m))
            })
        })
        .cloned()
        .collect()
}

/// Adds `members` to the unconditional `role` binding, preserving everything
/// else. Never removes anything. Returns true if the policy changed.
pub fn add_members(policy: &mut Policy, role: &str, members: &[String]) -> bool {
    let missing = missing_members(policy, role, members);
    if missing.is_empty() {
        return false;
    }
    match policy
        .bindings
        .iter_mut()
        .find(|b| b.role == role && b.condition.is_none())
    {
        Some(b) => b.members.extend(missing),
        None => policy
            .bindings
            .push(Binding::new().set_role(role).set_members(missing)),
    }
    true
}

/// Removes `members` from unconditional `role` bindings (case-insensitive),
/// dropping bindings left empty. Returns true if the policy changed.
pub fn remove_members(policy: &mut Policy, role: &str, members: &[String]) -> bool {
    let mut changed = false;
    for b in policy
        .bindings
        .iter_mut()
        .filter(|b| b.role == role && b.condition.is_none())
    {
        let before = b.members.len();
        b.members
            .retain(|m| !members.iter().any(|x| same_member(m, x)));
        changed |= b.members.len() != before;
    }
    policy.bindings.retain(|b| !b.members.is_empty());
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use google_cloud_type::model::Expr;

    fn policy() -> Policy {
        Policy::new()
            .set_version(3)
            .set_etag(bytes::Bytes::from_static(b"etag"))
            .set_bindings([
                Binding::new()
                    .set_role(INVOKER_ROLE)
                    .set_members(["serviceAccount:caller@p.iam.gserviceaccount.com"]),
                Binding::new()
                    .set_role(INVOKER_ROLE)
                    .set_members(["user:temp@example.com"])
                    .set_condition(
                        Expr::new()
                            .set_expression("request.time < timestamp('2030-01-01T00:00:00Z')"),
                    ),
                Binding::new()
                    .set_role("roles/run.developer")
                    .set_members(["group:devs@example.com"]),
            ])
    }

    #[test]
    fn grant_and_revoke_preserve_unrelated_bindings() {
        let original = policy();
        let mut p = original.clone();
        assert!(!is_public(&p));
        assert!(set_public(&mut p, true));
        assert!(is_public(&p));
        assert_eq!(
            p.bindings.len(),
            3,
            "allUsers joins the existing unconditional binding"
        );
        assert_eq!(p.bindings[0].members.len(), 2);
        assert_eq!(p.etag, original.etag);
        assert_eq!(p.version, 3);
        assert!(!set_public(&mut p, true), "idempotent");

        assert!(set_public(&mut p, false));
        assert_eq!(p, original);
    }

    #[test]
    fn revoke_drops_empty_binding_and_ignores_conditional() {
        let mut p = Policy::new().set_bindings([
            Binding::new()
                .set_role(INVOKER_ROLE)
                .set_members([PUBLIC_MEMBER]),
            Binding::new()
                .set_role(INVOKER_ROLE)
                .set_members([PUBLIC_MEMBER])
                .set_condition(Expr::new().set_expression("true")),
        ]);
        assert!(is_public(&p));
        assert!(set_public(&mut p, false));
        assert_eq!(p.bindings.len(), 1);
        assert!(p.bindings[0].condition.is_some());
        assert!(
            !is_public(&p),
            "conditional bindings do not count as public"
        );
    }

    #[test]
    fn add_members_is_additive_and_idempotent() {
        let original = policy();
        let mut p = original.clone();
        let want = vec![
            "serviceAccount:caller@p.iam.gserviceaccount.com".to_string(),
            "serviceAccount:service-1@gcp-sa-iap.iam.gserviceaccount.com".to_string(),
        ];
        assert_eq!(missing_members(&p, INVOKER_ROLE, &want), want[1..]);
        assert!(add_members(&mut p, INVOKER_ROLE, &want));
        assert_eq!(
            p.bindings[0].members.len(),
            2,
            "joined the unconditional binding"
        );
        assert_eq!(
            p.bindings[1], original.bindings[1],
            "conditional binding untouched"
        );
        assert!(
            !add_members(&mut p, INVOKER_ROLE, &want),
            "second call is a no-op"
        );

        let mut empty = Policy::new();
        assert!(add_members(
            &mut empty,
            "roles/storage.objectUser",
            &want[..1]
        ));
        assert_eq!(empty.bindings[0].role, "roles/storage.objectUser");
    }

    #[test]
    fn remove_members_is_targeted() {
        let mut p = policy();
        let sa = vec!["serviceAccount:CALLER@p.iam.gserviceaccount.com".to_string()];
        assert!(remove_members(&mut p, INVOKER_ROLE, &sa));
        assert_eq!(p.bindings.len(), 2, "emptied unconditional binding dropped");
        assert!(
            p.bindings.iter().any(|b| b.condition.is_some()),
            "conditional binding kept"
        );
        assert!(!remove_members(&mut p, INVOKER_ROLE, &sa), "idempotent");
    }

    #[test]
    fn member_comparison_ignores_case() {
        let p = Policy::new().set_bindings([Binding::new()
            .set_role("roles/iap.httpsResourceAccessor")
            .set_members(["group:developers@example.com"])]);
        let want = vec!["group:DEVELOPERS@example.com".to_string()];
        assert!(missing_members(&p, "roles/iap.httpsResourceAccessor", &want).is_empty());
    }

    #[test]
    fn grant_creates_binding_when_missing() {
        let mut p = Policy::new();
        assert!(set_public(&mut p, true));
        assert_eq!(p.bindings.len(), 1);
        assert_eq!(p.bindings[0].role, INVOKER_ROLE);
    }
}
