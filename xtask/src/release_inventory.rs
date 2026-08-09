use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

#[derive(Clone, Debug, Default)]
pub struct IgnoredPolicy {
    pub(crate) test: String,
    pub(crate) class: String,
    pub(crate) owner: String,
    job: String,
    requires: String,
    covered_by: String,
}

#[derive(Clone, Debug, Default)]
pub struct ConditionalPolicy {
    pub(crate) site: String,
    class: String,
    pub(crate) owner: String,
    job: String,
    requires: String,
    policy: String,
}

#[derive(Clone, Debug)]
pub struct IgnoredTest {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) reason: String,
}

#[derive(Debug)]
pub struct ReleaseInventory {
    pub(crate) ignored: Vec<IgnoredPolicy>,
    pub(crate) ignored_tests: Vec<IgnoredTest>,
    pub(crate) conditionals: Vec<ConditionalPolicy>,
    pub(crate) errors: Vec<String>,
}

impl ReleaseInventory {
    pub(crate) fn load(root: &Path) -> Self {
        let mut errors = Vec::new();
        let ignored = read_inventory(root, "ignored-tests.toml", "[[ignored]]", &mut errors)
            .into_iter()
            .map(|fields| ignored_policy(&fields))
            .collect();
        let conditionals = read_inventory(
            root,
            "conditional-tests.toml",
            "[[conditional]]",
            &mut errors,
        )
        .into_iter()
        .map(|fields| conditional_policy(&fields))
        .collect();
        let ignored_tests = ignored_tests(root, &mut errors);
        let conditional_sites = conditional_sites(root, &mut errors);
        let task_statuses = task_statuses(root, &mut errors);
        let mut inventory = Self {
            ignored,
            ignored_tests,
            conditionals,
            errors,
        };
        inventory.validate(&conditional_sites, &task_statuses);
        inventory
    }

    pub(crate) fn ignored_policy(&self, test: &str) -> Option<&IgnoredPolicy> {
        self.ignored.iter().find(|policy| policy.test == test)
    }

    pub(crate) fn infrastructure_ignored(&self) -> impl Iterator<Item = &IgnoredPolicy> {
        self.ignored
            .iter()
            .filter(|policy| policy.class == "infrastructure")
    }

    pub(crate) fn required_conditionals(&self) -> impl Iterator<Item = &ConditionalPolicy> {
        self.conditionals
            .iter()
            .filter(|policy| policy.policy == "required")
    }

    pub(crate) fn conditional_owner(&self, test: &str) -> Option<&str> {
        self.conditionals.iter().find_map(|policy| {
            let (site, _) = policy.site.rsplit_once('#')?;
            let qualified = test
                .strip_suffix(site)
                .is_some_and(|prefix| prefix.ends_with("::"));
            (test == site || qualified).then_some(policy.owner.as_str())
        })
    }

    pub(crate) fn ignored_owner(&self, test: &str) -> Option<&str> {
        let name = test.rsplit("::").next().unwrap_or(test);
        self.ignored_policy(name)
            .map(|policy| policy.owner.as_str())
    }

    fn validate(
        &mut self,
        conditional_sites: &BTreeSet<String>,
        task_statuses: &BTreeMap<String, String>,
    ) {
        validate_unique(
            self.ignored.iter().map(|policy| policy.test.as_str()),
            "ignored policy",
            &mut self.errors,
        );
        validate_unique_ignored_tests(&self.ignored_tests, &mut self.errors);
        validate_unique(
            self.conditionals.iter().map(|policy| policy.site.as_str()),
            "conditional site",
            &mut self.errors,
        );
        self.validate_ignored(task_statuses);
        self.validate_conditionals(conditional_sites, task_statuses);
    }

    fn validate_ignored(&mut self, task_statuses: &BTreeMap<String, String>) {
        let live: BTreeSet<&str> = self
            .ignored_tests
            .iter()
            .map(|test| test.name.as_str())
            .collect();
        let known: BTreeSet<&str> = self
            .ignored
            .iter()
            .map(|policy| policy.test.as_str())
            .collect();
        for name in live.difference(&known) {
            self.errors
                .push(format!("ignored test {name} has no policy"));
        }
        for name in known.difference(&live) {
            self.errors
                .push(format!("ignored policy {name} names no live test"));
        }
        for policy in &self.ignored {
            if !valid_ignored_policy(policy) {
                self.errors.push(format!(
                    "ignored test {} has an invalid or incomplete {} policy",
                    policy.test, policy.class
                ));
            }
            validate_owner(&policy.test, &policy.owner, task_statuses, &mut self.errors);
        }
    }

    fn validate_conditionals(
        &mut self,
        live: &BTreeSet<String>,
        task_statuses: &BTreeMap<String, String>,
    ) {
        let known: BTreeSet<&str> = self
            .conditionals
            .iter()
            .map(|policy| policy.site.as_str())
            .collect();
        for site in live {
            if !known.contains(site.as_str()) {
                self.errors
                    .push(format!("conditional site {site} has no policy"));
            }
        }
        for site in known {
            if !live.contains(site) {
                self.errors
                    .push(format!("conditional policy {site} names no live site"));
            }
        }
        for policy in &self.conditionals {
            if !valid_conditional_policy(policy) {
                self.errors.push(format!(
                    "conditional site {} has an invalid or incomplete policy",
                    policy.site
                ));
            }
            validate_owner(&policy.site, &policy.owner, task_statuses, &mut self.errors);
        }
    }
}

fn validate_unique_ignored_tests(tests: &[IgnoredTest], errors: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    for test in tests {
        if !seen.insert((test.path.as_str(), test.name.as_str())) {
            errors.push(format!(
                "live ignored test {} at {} is declared more than once",
                test.name, test.path
            ));
        }
    }
}

fn validate_unique<'a>(
    values: impl Iterator<Item = &'a str>,
    kind: &str,
    errors: &mut Vec<String>,
) {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            errors.push(format!("{kind} {value} is declared more than once"));
        }
    }
}

fn validate_owner(
    item: &str,
    owner: &str,
    statuses: &BTreeMap<String, String>,
    errors: &mut Vec<String>,
) {
    match statuses.get(owner).map(String::as_str) {
        Some(status) if status != "cancelled" => {}
        status => errors.push(format!(
            "{item} has owner task {owner:?} with status {status:?}"
        )),
    }
}

fn valid_ignored_policy(policy: &IgnoredPolicy) -> bool {
    if policy.test.is_empty() || policy.owner.is_empty() {
        return false;
    }
    match policy.class.as_str() {
        "infrastructure" => policy.job == "release-qualification" && !policy.requires.is_empty(),
        "covered" | "out-of-scope" => !policy.covered_by.is_empty(),
        "semantic" | "tool" | "unclassified" => true,
        _ => false,
    }
}

fn valid_conditional_policy(policy: &ConditionalPolicy) -> bool {
    if policy.site.is_empty() || policy.owner.is_empty() || policy.requires.is_empty() {
        return false;
    }
    matches!(
        (
            policy.class.as_str(),
            policy.policy.as_str(),
            policy.job.as_str()
        ),
        ("infrastructure", "required", "release-qualification")
            | ("diagnostic", "optional", "manual-diagnostics")
    )
}

fn read_inventory(
    root: &Path,
    name: &str,
    header: &str,
    errors: &mut Vec<String>,
) -> Vec<BTreeMap<String, String>> {
    match fs::read_to_string(root.join(name)) {
        Ok(text) => parse_tables(&text, header),
        Err(error) => {
            errors.push(format!("{name} is unreadable: {error}"));
            Vec::new()
        }
    }
}

fn parse_tables(text: &str, header: &str) -> Vec<BTreeMap<String, String>> {
    text.split(header)
        .skip(1)
        .map(|table| {
            let mut fields = BTreeMap::new();
            let mut multiline = false;
            for line in table.lines() {
                let line = line.trim();
                if multiline {
                    multiline = !line.ends_with("\"\"\"");
                    continue;
                }
                let Some((name, value)) = line.split_once('=') else {
                    continue;
                };
                let value = value.trim();
                if let Some(value) = value.strip_prefix("\"\"\"") {
                    multiline = !value.ends_with("\"\"\"");
                    continue;
                }
                if let Some(value) = value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                {
                    fields.insert(name.trim().to_owned(), value.to_owned());
                }
            }
            fields
        })
        .collect()
}

fn ignored_policy(fields: &BTreeMap<String, String>) -> IgnoredPolicy {
    IgnoredPolicy {
        test: field(fields, "test"),
        class: field(fields, "class"),
        owner: field(fields, "owner"),
        job: field(fields, "job"),
        requires: field(fields, "requires"),
        covered_by: field(fields, "covered_by"),
    }
}

fn conditional_policy(fields: &BTreeMap<String, String>) -> ConditionalPolicy {
    ConditionalPolicy {
        site: field(fields, "site"),
        class: field(fields, "class"),
        owner: field(fields, "owner"),
        job: field(fields, "job"),
        requires: field(fields, "requires"),
        policy: field(fields, "policy"),
    }
}

fn field(fields: &BTreeMap<String, String>, name: &str) -> String {
    fields.get(name).cloned().unwrap_or_default()
}

fn ignored_tests(root: &Path, errors: &mut Vec<String>) -> Vec<IgnoredTest> {
    let mut found = Vec::new();
    for scanned in [
        "crates",
        "xtask",
        "vendor/clarity-wasm/clar2wasm/src",
        "vendor/clarity-wasm/clar2wasm/tests",
    ] {
        walk_ignored(root, &root.join(scanned), &mut found, errors);
    }
    found.sort_by(|left, right| left.path.cmp(&right.path));
    found
}

fn walk_ignored(
    root: &Path,
    directory: &Path,
    found: &mut Vec<IgnoredTest>,
    errors: &mut Vec<String>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("cannot scan {}: {error}", directory.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("cannot scan {}: {error}", directory.display()));
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            walk_ignored(root, &path, found, errors);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            inspect_ignored_source(root, &path, found, errors);
        }
    }
}

fn inspect_ignored_source(
    root: &Path,
    path: &Path,
    found: &mut Vec<IgnoredTest>,
    errors: &mut Vec<String>,
) {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            errors.push(format!("cannot read {}: {error}", path.display()));
            return;
        }
    };
    let shown = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    let lines: Vec<&str> = source.lines().collect();
    for (line, text) in lines.iter().enumerate() {
        let trimmed = text.trim_start();
        if !trimmed.starts_with("#[ignore") {
            continue;
        }
        let reason = trimmed
            .split_once('"')
            .and_then(|(_, rest)| rest.rsplit_once('"').map(|(reason, _)| reason.to_owned()))
            .unwrap_or_else(|| "no reason given".to_owned());
        let name = lines
            .iter()
            .skip(line + 1)
            .take(8)
            .find_map(|next| function_name(next))
            .unwrap_or_else(|| format!("{shown}:{}", line + 1));
        found.push(IgnoredTest {
            path: format!("{shown}:{}", line + 1),
            name,
            reason,
        });
    }
}

fn conditional_sites(root: &Path, errors: &mut Vec<String>) -> BTreeSet<String> {
    let directory = root.join("crates/nano-conformance/tests/conformance");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("cannot scan {}: {error}", directory.display()));
            return BTreeSet::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => errors.push(format!("cannot scan {}: {error}", directory.display())),
        }
    }
    paths.retain(|path| {
        path.extension().is_some_and(|extension| extension == "rs")
            && path
                .file_stem()
                .is_some_and(|stem| stem != "release_inventory")
    });
    paths.sort();
    let mut sites = BTreeSet::new();
    for path in paths {
        inspect_conditional_source(&path, &mut sites, errors);
    }
    sites
}

fn inspect_conditional_source(path: &Path, sites: &mut BTreeSet<String>, errors: &mut Vec<String>) {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            errors.push(format!("cannot read {}: {error}", path.display()));
            return;
        }
    };
    let module = path
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let mut function = "<module>".to_owned();
    let mut ordinals = BTreeMap::new();
    for line in source.lines() {
        if let Some(name) = function_name(line) {
            function = name;
        }
        let trimmed = line.trim_start();
        let calls = line.matches("nano_conformance::skip_gate(").count()
            + usize::from(trimmed.starts_with("skip_gate("))
            + line.matches("nano_conformance::skip_diagnostic(").count()
            + usize::from(trimmed.starts_with("skip_diagnostic("));
        for _ in 0..calls {
            let ordinal = ordinals.entry(function.clone()).or_insert(0usize);
            *ordinal += 1;
            sites.insert(format!("{module}::{function}#{ordinal}"));
        }
    }
}

fn function_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    ["fn ", "async fn ", "pub fn ", "pub async fn "]
        .into_iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))
        .map(|rest| {
            rest.split(['(', '<', ' '])
                .next()
                .unwrap_or(rest)
                .to_owned()
        })
}

fn task_statuses(root: &Path, errors: &mut Vec<String>) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    walk_tasks(&root.join("tasks"), &mut found, errors);
    found
}

fn walk_tasks(directory: &Path, found: &mut BTreeMap<String, String>, errors: &mut Vec<String>) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("cannot scan {}: {error}", directory.display()));
            return;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                errors.push(format!("cannot scan {}: {error}", directory.display()));
                continue;
            }
        };
        if path.is_dir() {
            walk_tasks(&path, found, errors);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            read_task_status(&path, found, errors);
        }
    }
}

fn read_task_status(path: &Path, found: &mut BTreeMap<String, String>, errors: &mut Vec<String>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            errors.push(format!("cannot read {}: {error}", path.display()));
            return;
        }
    };
    let value = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}: ")))
            .map(|value| value.trim().trim_matches('"').to_owned())
    };
    if let (Some(id), Some(status)) = (value("id"), value("status")) {
        found.insert(id, status);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_tables, validate_unique_ignored_tests, IgnoredTest, ReleaseInventory};

    #[test]
    fn table_parser_ignores_multiline_evidence() {
        let tables = parse_tables(
            "[[entry]]\nname = \"one\"\nnote = \"\"\"\nclass = \"not-a-field\"\n\"\"\"\n",
            "[[entry]]",
        );
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].get("name").map(String::as_str), Some("one"));
        assert!(!tables[0].contains_key("class"));
    }

    #[test]
    fn repeated_test_names_at_distinct_source_sites_are_not_duplicate_identities() {
        let test = |path: &str| IgnoredTest {
            path: path.to_owned(),
            name: "same_name".to_owned(),
            reason: "same reason".to_owned(),
        };
        let first = test("tests/example.rs:10");
        let second = test("tests/example.rs:20");
        let mut errors = Vec::new();

        validate_unique_ignored_tests(&[first.clone(), second], &mut errors);
        assert!(errors.is_empty());

        validate_unique_ignored_tests(&[first.clone(), first], &mut errors);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn the_release_report_inventory_matches_the_current_sources() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is under the workspace");
        let inventory = ReleaseInventory::load(root);
        assert!(
            inventory.errors.is_empty(),
            "release inventory errors:\n  {}",
            inventory.errors.join("\n  ")
        );
        assert!(!inventory.ignored_tests.is_empty());
        assert!(!inventory.conditionals.is_empty());
    }
}
