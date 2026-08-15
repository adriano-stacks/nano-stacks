use std::{collections::BTreeMap, fs, path::Path};

use serde_json::Value;
use time::{Date, OffsetDateTime, format_description};

use crate::release_inventory::task_statuses;

pub const ADVISORY_POLICY: &str = "advisory-policy.json";
const SCHEMA: &str = "nano-stacks/advisory-policy/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
struct Exception {
    id: String,
    package: String,
    version: String,
    scope: String,
    reachability: String,
    owner: String,
    expires: Date,
}

#[derive(Debug)]
pub struct AdvisoryPolicy {
    exceptions: BTreeMap<String, Exception>,
}

impl AdvisoryPolicy {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path)
            .map_err(|error| format!("cannot read advisory policy {}: {error}", path.display()))?;
        let value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("advisory policy is not JSON: {error}"))?;
        Self::from_value(&value)
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        if value["schema"] != SCHEMA {
            return Err("advisory policy has an unknown schema".to_owned());
        }
        let values = value["exceptions"]
            .as_array()
            .ok_or_else(|| "advisory policy has no exceptions array".to_owned())?;
        let format = format_description::parse_borrowed::<3>("[year]-[month]-[day]")
            .map_err(|error| format!("cannot define advisory expiry format: {error}"))?;
        let mut exceptions = BTreeMap::new();
        for value in values {
            let exception = Exception {
                id: required(value, "id")?,
                package: required(value, "package")?,
                version: required(value, "version")?,
                scope: required(value, "scope")?,
                reachability: required(value, "reachability")?,
                owner: required(value, "owner")?,
                expires: Date::parse(&required(value, "expires")?, &format)
                    .map_err(|error| format!("advisory expiry is not YYYY-MM-DD: {error}"))?,
            };
            if !matches!(exception.scope.as_str(), "non-release-target" | "test-only") {
                return Err(format!(
                    "advisory {} has unsupported scope {:?}",
                    exception.id, exception.scope
                ));
            }
            if exception.reachability.len() < 40 {
                return Err(format!(
                    "advisory {} has no concrete reachability statement",
                    exception.id
                ));
            }
            if exceptions.insert(exception.id.clone(), exception).is_some() {
                return Err("advisory policy repeats an exception".to_owned());
            }
        }
        Ok(Self { exceptions })
    }

    pub fn verify_owners(&self, workspace: &Path) -> Result<(), String> {
        let mut errors = Vec::new();
        let statuses = task_statuses(workspace, &mut errors);
        if !errors.is_empty() {
            return Err(format!(
                "cannot validate advisory owners: {}",
                errors.join("; ")
            ));
        }
        for exception in self.exceptions.values() {
            match statuses.get(&exception.owner).map(String::as_str) {
                Some("in-progress" | "blocked") => {}
                status => {
                    return Err(format!(
                        "advisory {} has owner task {} with status {status:?}",
                        exception.id, exception.owner
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn verify_report_now(&self, report: &Value) -> Result<(), String> {
        self.verify_report(report, OffsetDateTime::now_utc().date())
    }

    fn verify_report(&self, report: &Value, today: Date) -> Result<(), String> {
        if report["vulnerabilities"]["found"] != false {
            return Err(format!(
                "advisory policy found {} vulnerable package(s)",
                report["vulnerabilities"]["count"].as_u64().unwrap_or(0)
            ));
        }
        match report["settings"]["ignore"].as_array() {
            Some(ignored) if ignored.is_empty() => {}
            Some(_) => return Err("cargo-audit report used an unverified CLI ignore".to_owned()),
            None => return Err("cargo-audit report has no ignore settings".to_owned()),
        }
        let warnings = report["warnings"]
            .as_object()
            .ok_or_else(|| "cargo-audit report has no warnings object".to_owned())?;
        let mut observed = BTreeMap::new();
        for warning in warnings.values().filter_map(Value::as_array).flatten() {
            let id = nested_required(warning, &["advisory", "id"])?;
            let package = nested_required(warning, &["package", "name"])?;
            let version = nested_required(warning, &["package", "version"])?;
            if observed.insert(id.clone(), (package, version)).is_some() {
                return Err(format!("cargo-audit repeated advisory {id}"));
            }
        }

        for (id, exception) in &self.exceptions {
            if exception.expires < today {
                return Err(format!(
                    "advisory {id} exception expired on {}",
                    exception.expires
                ));
            }
            let Some((package, version)) = observed.remove(id) else {
                return Err(format!("advisory {id} exception is stale"));
            };
            if package != exception.package || version != exception.version {
                return Err(format!(
                    "advisory {id} moved from {} {} to {package} {version}",
                    exception.package, exception.version
                ));
            }
        }
        if let Some((id, (package, version))) = observed.into_iter().next() {
            return Err(format!(
                "advisory {id} for {package} {version} has no exception"
            ));
        }
        Ok(())
    }
}

fn required(value: &Value, field: &str) -> Result<String, String> {
    value[field]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("advisory exception has no {field}"))
}

fn nested_required(value: &Value, fields: &[&str]) -> Result<String, String> {
    fields
        .iter()
        .try_fold(value, |value, field| {
            value
                .get(field)
                .ok_or_else(|| format!("cargo-audit warning has no {}", fields.join(".")))
        })?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("cargo-audit warning has invalid {}", fields.join(".")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;
    use time::{Date, Month};

    use super::AdvisoryPolicy;

    fn policy(expires: &str) -> AdvisoryPolicy {
        AdvisoryPolicy::from_value(&json!({
            "schema": "nano-stacks/advisory-policy/v1",
            "exceptions": [{
                "id": "RUSTSEC-2000-0001",
                "package": "old",
                "version": "1.0.0",
                "scope": "test-only",
                "reachability": "Only a deliberately retained test dependency selects this crate.",
                "owner": "130",
                "expires": expires,
            }],
        }))
        .expect("valid policy")
    }

    fn report(id: &str, package: &str, version: &str) -> serde_json::Value {
        json!({
            "settings": { "ignore": [] },
            "vulnerabilities": { "found": false, "count": 0, "list": [] },
            "warnings": { "unmaintained": [{
                "advisory": { "id": id },
                "package": { "name": package, "version": version },
            }] },
        })
    }

    #[test]
    fn only_the_exact_unexpired_warning_is_excepted() {
        let today = Date::from_calendar_date(2026, Month::August, 16).expect("test date");
        let current_policy = policy("2026-09-15");
        assert!(
            current_policy
                .verify_report(&report("RUSTSEC-2000-0001", "old", "1.0.0"), today)
                .is_ok()
        );
        assert!(
            current_policy
                .verify_report(&report("RUSTSEC-2000-0002", "old", "1.0.0"), today)
                .is_err()
        );
        assert!(
            current_policy
                .verify_report(&report("RUSTSEC-2000-0001", "old", "1.0.1"), today)
                .is_err()
        );
        assert!(
            policy("2026-08-15")
                .verify_report(&report("RUSTSEC-2000-0001", "old", "1.0.0"), today)
                .is_err()
        );
    }

    #[test]
    fn stale_exceptions_vulnerabilities_and_cli_ignores_are_refused() {
        let today = Date::from_calendar_date(2026, Month::August, 16).expect("test date");
        let policy = policy("2026-09-15");
        let mut stale = report("RUSTSEC-2000-0001", "old", "1.0.0");
        stale["warnings"] = json!({});
        assert!(policy.verify_report(&stale, today).is_err());
        let mut vulnerable = report("RUSTSEC-2000-0001", "old", "1.0.0");
        vulnerable["vulnerabilities"]["found"] = json!(true);
        assert!(policy.verify_report(&vulnerable, today).is_err());
        let mut ignored = report("RUSTSEC-2000-0001", "old", "1.0.0");
        ignored["settings"]["ignore"] = json!(["RUSTSEC-2000-0001"]);
        assert!(policy.verify_report(&ignored, today).is_err());
    }

    #[test]
    fn checked_in_exceptions_match_the_current_owned_warnings() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is inside the workspace");
        let policy = AdvisoryPolicy::load(&workspace.join("advisory-policy.json"))
            .expect("checked-in advisory policy");
        policy.verify_owners(workspace).expect("open owner tasks");
        let report = json!({
            "settings": { "ignore": [] },
            "vulnerabilities": { "found": false, "count": 0, "list": [] },
            "warnings": { "unmaintained": [
                {
                    "advisory": { "id": "RUSTSEC-2025-0161" },
                    "package": { "name": "libsecp256k1", "version": "0.7.2" },
                },
                {
                    "advisory": { "id": "RUSTSEC-2020-0016" },
                    "package": { "name": "net2", "version": "0.2.39" },
                },
            ] },
        });
        policy
            .verify_report_now(&report)
            .expect("current warnings match unexpired exceptions");
    }
}
