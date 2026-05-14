//! `xtask check-forbidden-deps`
//!
//! Keep database stacks that are outside mvm's threat model out of the
//! dependency graph. This is intentionally lockfile-based so it catches
//! transitive pulls before code review has to inspect `cargo tree`.

use anyhow::{Context, Result, bail};
use std::path::Path;

const FORBIDDEN_PREFIXES: &[&str] = &["sea-"];
const FORBIDDEN_SUBSTRINGS: &[&str] = &["mysql"];

pub fn run(workspace: &Path) -> Result<()> {
    let lock_path = workspace.join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    let mut forbidden = Vec::new();
    for name in package_names(&lock) {
        if is_forbidden(name) {
            forbidden.push(name.to_string());
        }
    }
    forbidden.sort();
    forbidden.dedup();

    if !forbidden.is_empty() {
        bail!(
            "check-forbidden-deps: forbidden package(s) in Cargo.lock: {}",
            forbidden.join(", ")
        );
    }

    eprintln!("check-forbidden-deps: clean (no sea-* or mysql packages in Cargo.lock)");
    Ok(())
}

fn package_names(lock: &str) -> impl Iterator<Item = &str> {
    lock.lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
}

fn is_forbidden(name: &str) -> bool {
    FORBIDDEN_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || FORBIDDEN_SUBSTRINGS
            .iter()
            .any(|needle| name.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_extracts_lockfile_names() {
        let lock = r#"
[[package]]
name = "mvm"

[[package]]
name = "sea-orm"
"#;
        let names: Vec<_> = package_names(lock).collect();
        assert_eq!(names, vec!["mvm", "sea-orm"]);
    }

    #[test]
    fn forbidden_matches_sea_prefix_and_mysql_substring() {
        assert!(is_forbidden("sea-orm"));
        assert!(is_forbidden("sqlx-mysql"));
        assert!(is_forbidden("mysql_common"));
        assert!(!is_forbidden("sqlx-sqlite"));
        assert!(!is_forbidden("mvm-policy"));
    }
}
