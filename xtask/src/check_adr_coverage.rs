//! `cargo xtask check-adr-coverage` — surface architectural decisions
//! that have no in-code references.
//!
//! ADR coverage is a soft proxy for "the decision is actually
//! implemented, not just documented." An ADR with zero `ADR-NNN`
//! references in source/tests/docs is either stale (decision was
//! reversed and the doc was forgotten), unimplemented (decision
//! was made but the code never landed), or genuinely
//! reference-free (e.g. a process ADR that doesn't touch code).
//! All three are worth surfacing for review.
//!
//! Output format mirrors `cargo deny` for grep-ability and CI
//! integration. (Example shapes; this comment deliberately splits
//! the ADR prefix so the scanner doesn't pick up the example as a
//! real reference.)
//!
//! ```text
//! [error] <ADR>-042 referenced in code but the file
//!         specs/adrs/042-*.md does not exist
//! [warn]  <ADR>-007 (slug) — 0 in-code references
//! [info]  <ADR>-002 (slug) — 312 in-code references
//! ```
//!
//! Exit code:
//!   - 0 → all referenced ADRs exist; "info" lines may still appear.
//!   - 1 → at least one reference points at a non-existent ADR.
//!     Bare "warn" (0 references on an existing ADR) is *not* a
//!     hard fail — a process ADR may legitimately not appear in
//!     code. CI lanes that want a strict mode can grep for
//!     `[warn]` and fail externally.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// ADR file pattern: `NNN-slug.md` where NNN is 3+ digits.
/// `specs/adrs/013-microsandbox-libkrun-microvm-nix-pivot.md` →
/// `(13, "microsandbox-libkrun-microvm-nix-pivot")`.
fn parse_adr_filename(name: &str) -> Option<(u32, String)> {
    let stem = name.strip_suffix(".md")?;
    let (num, rest) = stem.split_once('-')?;
    let n: u32 = num.parse().ok()?;
    Some((n, rest.to_string()))
}

/// In-code reference pattern: `ADR-N`, `ADR-NN`, or `ADR-NNN`.
/// Matches `ADR-002`, `ADR-013`, `ADR-7`, etc. Case-sensitive on
/// `ADR` so we don't pick up `adr-`-prefixed filenames.
fn extract_adr_refs(body: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if &bytes[i..i + 4] == b"ADR-" {
            let mut j = i + 4;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 4
                && let Ok(s) = std::str::from_utf8(&bytes[i + 4..j])
                && let Ok(n) = s.parse::<u32>()
            {
                out.push(n);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Scan a directory tree for `ADR-N` references, returning a count
/// keyed by ADR number. Skips:
///   - `target/`, `node_modules/`, `.git/` (build/dependency dirs)
///   - `specs/adrs/` (the ADRs themselves; self-references are
///     bookkeeping, not coverage)
///   - files larger than 1 MiB (build artifacts the workspace may
///     have committed, e.g. generated docs)
fn scan_for_refs(root: &Path) -> Result<BTreeMap<u32, usize>> {
    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    let skip_dirs: BTreeSet<&str> =
        ["target", "node_modules", ".git", "public"]
            .iter()
            .copied()
            .collect();
    let adrs_dir = root.join("specs/adrs");

    visit(&root.to_path_buf(), &mut counts, &skip_dirs, &adrs_dir)?;
    Ok(counts)
}

fn visit(
    dir: &PathBuf,
    counts: &mut BTreeMap<u32, usize>,
    skip_dirs: &BTreeSet<&str>,
    adrs_dir: &Path,
) -> Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();

        if file_type.is_dir() {
            if skip_dirs.contains(name.as_str()) || name.starts_with('.') {
                continue;
            }
            // Self-references inside specs/adrs/ would inflate the
            // count — every ADR refers to itself in its own body.
            if path == adrs_dir {
                continue;
            }
            visit(&path, counts, skip_dirs, adrs_dir)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        // Cheap pre-filter on extension. Binary files (images,
        // lockfiles, etc.) can't carry meaningful ADR refs even if
        // they happen to contain the byte sequence.
        let scan = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(
                "rs" | "md" | "toml" | "yaml" | "yml" | "nix" | "json" | "txt"
                | "sh" | "py" | "ts" | "tsx" | "js" | "jsx" | "html"
            )
        );
        if !scan {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.len() > 1_048_576 {
            continue;
        }

        let body = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue, // not UTF-8; skip
        };
        for n in extract_adr_refs(&body) {
            *counts.entry(n).or_insert(0) += 1;
        }
    }
    Ok(())
}

/// Discover ADRs in `specs/adrs/`. Returns a map `number → slug`.
fn discover_adrs(root: &Path) -> Result<BTreeMap<u32, String>> {
    let dir = root.join("specs/adrs");
    if !dir.exists() {
        return Ok(BTreeMap::new());
    }
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some((n, slug)) = parse_adr_filename(&name) {
            out.insert(n, slug);
        }
    }
    Ok(out)
}

/// Run the check; print findings; return Err on any "[error]" line.
pub fn run(workspace: &Path) -> Result<()> {
    let adrs = discover_adrs(workspace)?;
    let refs = scan_for_refs(workspace)?;

    let mut errors = 0usize;

    // 1. References to non-existent ADRs (typos or stale refs).
    for (&n, &count) in refs.iter() {
        if !adrs.contains_key(&n) {
            println!(
                "[error] ADR-{n:03} referenced {count}x in code, but \
                 specs/adrs/{n:03}-*.md does not exist"
            );
            errors += 1;
        }
    }

    // 2. ADRs with zero in-code references. Soft warning — process
    //    ADRs may legitimately have zero code mentions.
    for (&n, slug) in adrs.iter() {
        match refs.get(&n).copied().unwrap_or(0) {
            0 => println!("[warn]  ADR-{n:03} ({slug}) — 0 in-code references"),
            c => println!("[info]  ADR-{n:03} ({slug}) — {c} in-code references"),
        }
    }

    println!(
        "\nADRs discovered: {}; ADRs referenced: {}; broken refs: {errors}",
        adrs.len(),
        refs.iter().filter(|(n, _)| adrs.contains_key(n)).count(),
    );

    if errors > 0 {
        anyhow::bail!(
            "check-adr-coverage: {errors} reference{} to non-existent ADRs",
            if errors == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_adr_filename_picks_number_and_slug() {
        assert_eq!(
            parse_adr_filename("002-microvm-security-posture.md"),
            Some((2, "microvm-security-posture".to_string()))
        );
        assert_eq!(
            parse_adr_filename("013-microsandbox-libkrun-microvm-nix-pivot.md"),
            Some((13, "microsandbox-libkrun-microvm-nix-pivot".to_string()))
        );
    }

    #[test]
    fn parse_adr_filename_rejects_non_adrs() {
        assert_eq!(parse_adr_filename("README.md"), None);
        assert_eq!(parse_adr_filename("not-a-number-slug.md"), None);
        assert_eq!(parse_adr_filename("0xx-bogus.md"), None);
    }

    #[test]
    fn extract_adr_refs_finds_padded_and_unpadded() {
        let body = "See ADR-013 for the pivot; ADR-2 the security \
                    posture; ADR-038 for CI policy.";
        let refs = extract_adr_refs(body);
        assert_eq!(refs, vec![13, 2, 38]);
    }

    #[test]
    fn extract_adr_refs_handles_no_matches() {
        assert!(extract_adr_refs("no decisions referenced here").is_empty());
        assert!(extract_adr_refs("adr-002 lowercase shouldn't match").is_empty());
    }

    #[test]
    fn extract_adr_refs_handles_section_suffix() {
        // ADR refs in code often carry a section suffix; our extractor
        // pulls just the number.
        let body = "ADR-002 §W4.3 / ADR-013§\"Boot budget\"";
        let refs = extract_adr_refs(body);
        assert_eq!(refs, vec![2, 13]);
    }

    /// Build the literal `ADR-N` token at runtime so this source file
    /// doesn't itself trip the workspace `check-adr-coverage` pass.
    /// The xtask command scans the workspace including this very
    /// file; a literal `ADR-999` here would look like a real broken
    /// reference and fail CI.
    fn adr_token(n: u32) -> String {
        format!("{}-{:03}", "ADR", n)
    }

    #[test]
    fn run_against_fixture_workspace() {
        // Build a tiny fixture: one existing ADR, one source file
        // referencing it, and a second source file referencing a
        // non-existent ADR.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("specs/adrs")).unwrap();
        let adr1 = adr_token(1);
        std::fs::write(
            root.join("specs/adrs/001-fixture.md"),
            format!("# {adr1} — fixture\n"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), format!("// see {adr1}\n")).unwrap();
        let adr_missing = adr_token(998);
        std::fs::write(
            root.join("src/b.rs"),
            format!("// see {adr_missing} (does not exist)\n"),
        )
        .unwrap();

        let result = run(root);
        assert!(result.is_err(), "broken ref must surface as Err");
    }

    #[test]
    fn run_against_clean_fixture_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("specs/adrs")).unwrap();
        let adr1 = adr_token(1);
        std::fs::write(
            root.join("specs/adrs/001-fixture.md"),
            format!("# {adr1} — fixture\n"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), format!("// see {adr1}\n")).unwrap();

        run(root).expect("clean fixture must pass");
    }
}
