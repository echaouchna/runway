//! `${...}` interpolation in configuration strings.
//!
//! Variables: `${project}`, `${region}`, `${app}`, `${stage}`,
//! `${vars.NAME}` (top-level `vars`, overridden by `stages.<s>.vars`) and
//! `${buckets.KEY}` (the resolved name of a declared bucket). `$$` is a
//! literal `$`.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct Interp {
    vars: BTreeMap<String, String>,
}

impl Interp {
    pub fn new(project: &str, region: &str, app: &str, stage: &str) -> Self {
        let mut vars = BTreeMap::new();
        vars.insert("project".into(), project.into());
        vars.insert("region".into(), region.into());
        vars.insert("app".into(), app.into());
        vars.insert("stage".into(), stage.into());
        Self { vars }
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(name.into(), value.into());
    }

    /// Replaces every `${name}`; errors on unknown or malformed references.
    pub fn apply(&self, s: &str) -> Result<String, String> {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            if let Some(r) = after.strip_prefix('$') {
                out.push('$');
                rest = r;
            } else if let Some(r) = after.strip_prefix('{') {
                let end = r
                    .find('}')
                    .ok_or_else(|| format!("unterminated `${{` in `{s}`"))?;
                let name = r[..end].trim();
                match self.vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        let known: Vec<String> =
                            self.vars.keys().map(|k| format!("${{{k}}}")).collect();
                        return Err(format!(
                            "unknown variable `${{{name}}}` (available: {})",
                            known.join(", ")
                        ));
                    }
                }
                rest = &r[end + 1..];
            } else {
                out.push('$');
                rest = after;
            }
        }
        out.push_str(rest);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_builtins_vars_and_escapes() {
        let mut i = Interp::new("my-gcp-project", "europe-west1", "gcptree", "prod");
        i.set("vars.job_project", "billing-data-1234");
        i.set("buckets.cache", "my-gcp-project-gcptree-cache");
        assert_eq!(
            i.apply("${project}-gcptree-cache").unwrap(),
            "my-gcp-project-gcptree-cache"
        );
        assert_eq!(
            i.apply("gs://${buckets.cache}/gcptree").unwrap(),
            "gs://my-gcp-project-gcptree-cache/gcptree"
        );
        assert_eq!(
            i.apply("${ vars.job_project }").unwrap(),
            "billing-data-1234"
        );
        assert_eq!(i.apply("cost $$5 and $x").unwrap(), "cost $5 and $x");
        assert_eq!(i.apply("plain").unwrap(), "plain");
        let e = i.apply("${vars.nope}").unwrap_err();
        assert!(
            e.contains("unknown variable `${vars.nope}`") && e.contains("${project}"),
            "{e}"
        );
        assert!(i.apply("${project").unwrap_err().contains("unterminated"));
    }
}
