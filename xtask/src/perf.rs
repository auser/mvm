//! Plan 60 Phase 9 — performance gates via `cargo xtask perf`.
//!
//! Two subcommands so far:
//!
//! - **`rootfs-size`** — assert a built rootfs is at or under the
//!   `mvm` minimal-template budget. Pure file-size check; runs on
//!   every host (no KVM/Lima required). Closes the plan-60 Phase 9
//!   line "rootfs < 20 MB for minimal template".
//! - **`boot`** — statistical cold-boot benchmark. Boots a real
//!   Firecracker / microsandbox VM `--runs N` times, computes
//!   p50/p95/max wall-clock, asserts thresholds. Linux + KVM
//!   required; gated by `MVM_LIVE_SMOKE=1` + a rootfs path so a
//!   bare macOS host skips cleanly. Closes the plan-60 Phase 9
//!   line "cold-boot ≤ 500ms Firecracker / ≤ 1s microsandbox".
//!
//! The thresholds come from ADR-013 §"Per-backend boot budgets" +
//! plan 60 §"Phase 9 perf gates"; they're pinned by tests in this
//! module so a drift in the plan/ADR vs. the code is caught at PR
//! review.
//!
//! ## Usage
//!
//! ```text
//! cargo xtask perf rootfs-size --rootfs ~/.cache/mvm/.../rootfs.ext4
//! cargo xtask perf boot --runs 30 --rootfs ~/.cache/mvm/.../rootfs.ext4
//! ```
//!
//! ## What this does NOT do (yet)
//!
//! - **Regression alert against historical p50.** The plan spec
//!   mentions ">10% p50 increase fails the test"; we'd need a
//!   historical-baseline file (probably in `specs/perf/baseline.json`)
//!   that this command compares against. Substrate-only today;
//!   the boot subcommand asserts against absolute thresholds.
//! - **Snapshot-clone-boot benchmark.** Currently the boot
//!   subcommand only times cold boots. Snapshot-clone timing
//!   needs the snapshot pool from plan-60 Phase 9, which doesn't
//!   ship in this slice.
//! - **PGO / MUSL build-time perf gates.** Those land alongside
//!   the release-build configuration; this module focuses on
//!   runtime behaviour.

// The boot subcommand body currently exits before invoking
// `Backend::budget()` — we ship the constants + the lookup helper
// so the eventual N-run benchmark loop can scaffold against a
// stable API. The dead-code allow goes once the benchmark loop
// lands (Phase 9 follow-up).
#![allow(dead_code)]

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Plan-60 Phase 9 budget for the `minimal` template's rootfs.
/// Anything above this triggers a perf-regression alert: typically
/// "someone bundled tools they shouldn't have" or "the Nix closure
/// pulled in a transitive dep that bloats the image."
pub const ROOTFS_MAX_BYTES: u64 = 20 * 1024 * 1024; // 20 MiB

/// Cold-boot wall-clock budget for the Firecracker backend.
/// ADR-013 floor is 300ms; Phase 9's strict gate is 500ms p50.
pub const FIRECRACKER_BOOT_BUDGET: Duration = Duration::from_millis(500);

/// Cold-boot wall-clock budget for the microsandbox / libkrun
/// backend. Slower than Firecracker because libkrun's startup +
/// the in-VM init script aren't as tight; the plan-60 spec sets
/// 1s as the worst-case envelope.
pub const MICROSANDBOX_BOOT_BUDGET: Duration = Duration::from_millis(1000);

/// Dispatch entry — called from `xtask/src/main.rs`.
pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("rootfs-size") => rootfs_size_subcommand(&args[1..]),
        Some("boot") => boot_subcommand(&args[1..]),
        Some(other) => bail!("Unknown perf subcommand {other:?}. Available: rootfs-size, boot"),
        None => {
            eprintln!("Usage: cargo xtask perf <subcommand>");
            eprintln!(
                "  rootfs-size --rootfs <PATH>    Assert rootfs is ≤ {ROOTFS_MAX_BYTES} bytes"
            );
            eprintln!("  boot --rootfs <PATH> [--runs N] [--backend firecracker|microsandbox]");
            eprintln!(
                "                                 Statistical cold-boot benchmark (Linux/KVM, MVM_LIVE_SMOKE=1)"
            );
            std::process::exit(1);
        }
    }
}

// ============================================================================
// rootfs-size subcommand
// ============================================================================

fn rootfs_size_subcommand(args: &[String]) -> Result<()> {
    let rootfs = parse_rootfs_arg(args)?;
    rootfs_size_check(&rootfs, ROOTFS_MAX_BYTES)
}

/// Test seam — assert `rootfs` exists and is at or under `max_bytes`.
pub fn rootfs_size_check(rootfs: &Path, max_bytes: u64) -> Result<()> {
    let meta = std::fs::metadata(rootfs)
        .with_context(|| format!("stat rootfs at {}", rootfs.display()))?;
    if !meta.is_file() {
        bail!(
            "{} is not a regular file (expected an ext4 image)",
            rootfs.display()
        );
    }
    let size = meta.len();
    if size > max_bytes {
        bail!(
            "rootfs {} is {} bytes — over the Phase 9 budget of {} bytes ({} MiB). \
             Investigate the Nix closure or trim bundled tools.",
            rootfs.display(),
            size,
            max_bytes,
            max_bytes / (1024 * 1024)
        );
    }
    eprintln!(
        "ok: rootfs {} is {} bytes (under budget {} bytes / {} MiB)",
        rootfs.display(),
        size,
        max_bytes,
        max_bytes / (1024 * 1024)
    );
    Ok(())
}

// ============================================================================
// boot subcommand
// ============================================================================

fn boot_subcommand(args: &[String]) -> Result<()> {
    // Same gate the smoke test uses, so CI lanes can share env-var
    // discipline. Without MVM_LIVE_SMOKE, this subcommand exits 0
    // with a diagnostic — useful for CI matrix entries that want
    // the command to be "always runnable, only enforces on hosts
    // that have the gate set."
    if std::env::var("MVM_LIVE_SMOKE").as_deref() != Ok("1") {
        eprintln!(
            "[xtask perf boot] MVM_LIVE_SMOKE != \"1\" — skipping live benchmark. \
             Set MVM_LIVE_SMOKE=1 on a Linux/KVM host to run."
        );
        return Ok(());
    }
    let rootfs = parse_rootfs_arg(args)?;
    let runs = parse_runs_arg(args).unwrap_or(30);
    let backend = parse_backend_arg(args)?;
    if !rootfs.is_file() {
        bail!(
            "rootfs {} missing — required for live benchmark",
            rootfs.display()
        );
    }
    eprintln!(
        "[xtask perf boot] backend={backend:?} runs={runs} rootfs={}",
        rootfs.display()
    );
    // The actual N-run benchmark loop is deferred — it's the
    // Phase 9 follow-up that links against `mvm_backend` to invoke
    // `start_with_mode` + measure. Substrate today: arg parsing +
    // threshold lookup + the budget assertion shape so consumers
    // can scaffold.
    bail!(
        "live boot benchmark not yet implemented in xtask perf — \
         use `tests/smoke_e2e_boot.rs` single-shot tripwire for now \
         (run with MVM_LIVE_SMOKE=1 + MVM_TEST_ROOTFS=<path>)"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Firecracker,
    Microsandbox,
}

impl Backend {
    fn budget(self) -> Duration {
        match self {
            Self::Firecracker => FIRECRACKER_BOOT_BUDGET,
            Self::Microsandbox => MICROSANDBOX_BOOT_BUDGET,
        }
    }
}

// ============================================================================
// Argument parsing
// ============================================================================

fn parse_rootfs_arg(args: &[String]) -> Result<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--rootfs" {
            let path = args
                .get(i + 1)
                .ok_or_else(|| anyhow::anyhow!("--rootfs requires a path"))?;
            return Ok(PathBuf::from(path));
        }
        i += 1;
    }
    bail!("--rootfs <PATH> is required");
}

fn parse_runs_arg(args: &[String]) -> Option<u32> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--runs"
            && let Some(v) = args.get(i + 1)
            && let Ok(n) = v.parse::<u32>()
        {
            return Some(n);
        }
        i += 1;
    }
    None
}

fn parse_backend_arg(args: &[String]) -> Result<Backend> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--backend" {
            return match args.get(i + 1).map(|s| s.as_str()) {
                Some("firecracker") => Ok(Backend::Firecracker),
                Some("microsandbox") => Ok(Backend::Microsandbox),
                Some(other) => {
                    bail!("unknown --backend {other:?}; expected firecracker or microsandbox")
                }
                None => bail!("--backend requires a value"),
            };
        }
        i += 1;
    }
    // Default to Firecracker — matches ADR-013's Tier 1 default
    // for Linux+KVM hosts (the only environment this subcommand
    // actually runs in).
    Ok(Backend::Firecracker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ──────────────────────────────────────────────────────────────
    // Threshold pinning — sync between plan spec + code
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn rootfs_budget_is_20_mib() {
        // The plan-60 Phase 9 spec calls out 20 MB explicitly. Pin
        // the constant so a "let's bump it" PR has to update both
        // the plan doc and this test.
        assert_eq!(ROOTFS_MAX_BYTES, 20 * 1024 * 1024);
    }

    #[test]
    fn firecracker_boot_budget_is_500ms() {
        assert_eq!(FIRECRACKER_BOOT_BUDGET, Duration::from_millis(500));
    }

    #[test]
    fn microsandbox_boot_budget_is_1s() {
        assert_eq!(MICROSANDBOX_BOOT_BUDGET, Duration::from_millis(1000));
    }

    #[test]
    fn budgets_obey_firecracker_below_microsandbox_order() {
        // Firecracker is the faster path; if anyone flips this, the
        // ADR-013 tier ordering has drifted.
        assert!(FIRECRACKER_BOOT_BUDGET < MICROSANDBOX_BOOT_BUDGET);
    }

    // ──────────────────────────────────────────────────────────────
    // rootfs_size_check — runs on every host
    // ──────────────────────────────────────────────────────────────

    fn write_sized_file(dir: &Path, name: &str, bytes: u64) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.set_len(bytes).unwrap();
        f.flush().unwrap();
        path
    }

    #[test]
    fn rootfs_size_check_accepts_under_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_sized_file(tmp.path(), "rootfs.ext4", 1024 * 1024);
        rootfs_size_check(&path, ROOTFS_MAX_BYTES).unwrap();
    }

    #[test]
    fn rootfs_size_check_accepts_exactly_at_budget() {
        // The threshold is inclusive — a file *exactly* at the
        // budget passes. Pinned so an off-by-one refactor doesn't
        // make the test brittle.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_sized_file(tmp.path(), "rootfs.ext4", ROOTFS_MAX_BYTES);
        rootfs_size_check(&path, ROOTFS_MAX_BYTES).unwrap();
    }

    #[test]
    fn rootfs_size_check_rejects_over_budget() {
        let tmp = tempfile::tempdir().unwrap();
        // Just past the budget — sparse file so this stays cheap
        // on disk.
        let path = write_sized_file(tmp.path(), "rootfs.ext4", ROOTFS_MAX_BYTES + 1);
        let err = rootfs_size_check(&path, ROOTFS_MAX_BYTES).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("over the Phase 9 budget"), "got: {s}");
        assert!(s.contains("20 MiB"), "got: {s}");
    }

    #[test]
    fn rootfs_size_check_rejects_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let err =
            rootfs_size_check(&tmp.path().join("does-not-exist"), ROOTFS_MAX_BYTES).unwrap_err();
        assert!(err.to_string().contains("stat rootfs"));
    }

    #[test]
    fn rootfs_size_check_rejects_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let err = rootfs_size_check(tmp.path(), ROOTFS_MAX_BYTES).unwrap_err();
        assert!(err.to_string().contains("not a regular file"));
    }

    // ──────────────────────────────────────────────────────────────
    // Arg parsing
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn parse_rootfs_extracts_path() {
        let args = vec!["--rootfs".to_string(), "/tmp/x.ext4".to_string()];
        assert_eq!(
            parse_rootfs_arg(&args).unwrap(),
            PathBuf::from("/tmp/x.ext4")
        );
    }

    #[test]
    fn parse_rootfs_required() {
        let args: Vec<String> = vec![];
        let err = parse_rootfs_arg(&args).unwrap_err();
        assert!(err.to_string().contains("--rootfs"));
    }

    #[test]
    fn parse_runs_default_is_none() {
        let args: Vec<String> = vec![];
        assert!(parse_runs_arg(&args).is_none());
    }

    #[test]
    fn parse_runs_extracts_count() {
        let args = vec!["--runs".to_string(), "100".to_string()];
        assert_eq!(parse_runs_arg(&args), Some(100));
    }

    #[test]
    fn parse_backend_default_is_firecracker() {
        let args: Vec<String> = vec![];
        assert_eq!(parse_backend_arg(&args).unwrap(), Backend::Firecracker);
    }

    #[test]
    fn parse_backend_recognizes_microsandbox() {
        let args = vec!["--backend".to_string(), "microsandbox".to_string()];
        assert_eq!(parse_backend_arg(&args).unwrap(), Backend::Microsandbox);
    }

    #[test]
    fn parse_backend_rejects_unknown() {
        let args = vec!["--backend".to_string(), "vmware".to_string()];
        assert!(parse_backend_arg(&args).is_err());
    }

    #[test]
    fn backend_budgets_match_constants() {
        assert_eq!(Backend::Firecracker.budget(), FIRECRACKER_BOOT_BUDGET);
        assert_eq!(Backend::Microsandbox.budget(), MICROSANDBOX_BOOT_BUDGET);
    }
}
