use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Command,
};

#[derive(Debug, Eq, PartialEq)]
pub struct SourceStatus {
    pub revision: String,
    pub changes: Vec<String>,
}

impl SourceStatus {
    pub const fn clean(&self) -> bool {
        self.changes.is_empty()
    }
}

pub fn source_status(root: &Path) -> Result<SourceStatus, String> {
    let revision = git(root, &["rev-parse", "HEAD"])?;
    let mut changes = BTreeSet::new();
    collect_paths(
        root,
        "tracked",
        &["diff", "--name-only", "-z"],
        &mut changes,
    )?;
    collect_paths(
        root,
        "staged",
        &["diff", "--cached", "--name-only", "-z"],
        &mut changes,
    )?;
    collect_paths(
        root,
        "untracked",
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &mut changes,
    )?;
    collect_paths(
        root,
        "ignored",
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        &mut changes,
    )?;
    Ok(SourceStatus {
        revision,
        changes: changes.into_iter().collect(),
    })
}

fn collect_paths(
    root: &Path,
    class: &str,
    arguments: &[&str],
    changes: &mut BTreeSet<String>,
) -> Result<(), String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot inspect {class} source files: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = PathBuf::from(String::from_utf8_lossy(raw).as_ref());
        if build_relevant(&path) {
            changes.insert(format!("{class} {}", path.display()));
        }
    }
    Ok(())
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot run git {}: {error}", arguments.join(" ")))?;
    output.status.success().then_some(()).ok_or_else(|| {
        format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn build_relevant(path: &Path) -> bool {
    if path.components().any(|component| {
        matches!(component, Component::Normal(name) if name == OsStr::new("target"))
    }) || path.ends_with("clar2wasm/src/standard/standard.wasm")
        || path
            .components()
            .any(|component| matches!(component, Component::Normal(name) if name == OsStr::new("proptest-regressions")))
    {
        return false;
    }

    let root_files = [
        ".gitignore",
        "advisory-policy.json",
        "Cargo.lock",
        "Cargo.toml",
        "conditional-tests.toml",
        "deny.toml",
        "flake.lock",
        "flake.nix",
        "ignored-tests.toml",
        "known-differentials.toml",
        "rust-toolchain.toml",
    ];
    if root_files.iter().any(|name| path == Path::new(name)) {
        return true;
    }
    [
        ".cargo", ".github", "crates", "hacknet", "release", "scripts", "vendor", "xtask",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use super::source_status;

    fn git(root: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(root)
            .status()
            .expect("run git");
        assert!(status.success(), "git {} failed", arguments.join(" "));
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temporary repository");
        git(root.path(), &["init", "--quiet"]);
        git(root.path(), &["config", "user.name", "release test"]);
        git(
            root.path(),
            &["config", "user.email", "release@example.invalid"],
        );
        fs::create_dir(root.path().join("crates")).expect("create source directory");
        fs::write(root.path().join("Cargo.toml"), "[workspace]\n").expect("write manifest");
        fs::write(root.path().join("crates/lib.rs"), "pub fn clean() {}\n").expect("write source");
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "--quiet", "-m", "seed"]);
        root
    }

    #[test]
    fn clean_source_ignores_non_build_notes() {
        let root = repository();
        fs::create_dir(root.path().join("notes")).expect("create notes");
        fs::write(root.path().join("notes/result.txt"), "diagnostic\n").expect("write note");
        assert!(source_status(root.path()).expect("inspect source").clean());
    }

    #[test]
    fn tracked_staged_untracked_and_ignored_source_are_each_dirty() {
        for (class, prepare) in [
            ("tracked", 0_u8),
            ("staged", 1),
            ("untracked", 2),
            ("ignored", 3),
        ] {
            let root = repository();
            match prepare {
                0 => fs::write(root.path().join("crates/lib.rs"), "pub fn changed() {}\n")
                    .expect("change source"),
                1 => {
                    fs::write(root.path().join("crates/lib.rs"), "pub fn staged() {}\n")
                        .expect("change source");
                    git(root.path(), &["add", "crates/lib.rs"]);
                }
                2 => fs::write(root.path().join("crates/new.rs"), "pub fn new() {}\n")
                    .expect("write untracked source"),
                3 => {
                    fs::write(root.path().join(".gitignore"), "crates/ignored.rs\n")
                        .expect("ignore source");
                    git(root.path(), &["add", ".gitignore"]);
                    git(root.path(), &["commit", "--quiet", "-m", "ignore source"]);
                    fs::write(
                        root.path().join("crates/ignored.rs"),
                        "pub fn hidden() {}\n",
                    )
                    .expect("write ignored source");
                }
                _ => unreachable!(),
            }
            let status = source_status(root.path()).expect("inspect source");
            assert!(
                status
                    .changes
                    .iter()
                    .any(|change| change.starts_with(class)),
                "{class} source was not reported: {:?}",
                status.changes
            );
        }
    }
}
