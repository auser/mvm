# Plan 70 — Hermetic live coverage for `uninstall → Uninstall` positive

> `mvmctl uninstall --yes` removes `/var/lib/mvm`, `~/.mvm/`, and
> `/usr/local/bin/mvmctl`. Two of those are real system paths whose
> deletion needs sudo; on a developer's machine they point at an
> actual install, so a hermetic test that drives the positive path
> would either prompt for sudo mid-test or destroy the dev's mvmctl
> install. Plan 70 adds an `MVM_UNINSTALL_PATH_PREFIX` env-var that
> rewrites the destination paths into a tempdir, making the positive
> verb hermetically testable.
>
> Roughly 1 day, 2 workstreams.

**Status (2026-05-12)**: not started. No substrate dependencies beyond
PR #106 (`audit_emit!`). ADR-045 architectural decision.

The dry-run negative is already live-pinned (PR #108,
`uninstall_dry_run_does_not_emit_audit_entry`). This plan finishes
the positive.

## Context

`crates/mvm-cli/src/commands/env/uninstall.rs::run` flow:

```
mvmctl uninstall --yes
  ├── microvm::stop() (best-effort, errors logged)
  ├── if /var/lib/mvm exists: sudo rm -rf /var/lib/mvm
  ├── (--all) remove ~/.mvm/ (HOME-rooted; safe in sandbox)
  ├── (--all) sudo rm -f /usr/local/bin/mvmctl
  └── audit_emit!(Uninstall)
```

`~/.mvm/` already works hermetically (HOME is overridden to a
tempdir in `AuditSandbox`). The two sudo-gated paths (`/var/lib/mvm`,
`/usr/local/bin/mvmctl`) are the blockers.

## State of play

### Already in `origin/main`

- `uninstall.rs` with the hard-coded paths.
- `audit_emit!(Uninstall)` at end of `run`.
- Dry-run negative live-pinned (PR #108).

### Missing (integration)

1. No path-prefix override for `/var/lib/mvm` or `/usr/local/bin/mvmctl`.
2. No live test for the positive `Uninstall` emit.

## Workstreams

### W1 — `MVM_UNINSTALL_PATH_PREFIX` env-var override (~½ day)

**Goal**: when set, every absolute path in `uninstall.rs` is
prefixed with the env-var value. Tests point this at a tempdir;
production callers never set it.

**Action**:

- Helper in `uninstall.rs`:
  ```rust
  fn rooted_path(p: &str) -> PathBuf {
      match std::env::var("MVM_UNINSTALL_PATH_PREFIX") {
          Ok(prefix) if !prefix.trim().is_empty() => {
              let stripped = p.strip_prefix('/').unwrap_or(p);
              PathBuf::from(prefix).join(stripped)
          }
          _ => PathBuf::from(p),
      }
  }
  ```
- Every `Path::new("/var/lib/mvm")` / `Path::new("/usr/local/bin/mvmctl")`
  in `uninstall.rs` becomes `rooted_path("/var/lib/mvm")` etc.
- When the env-var is set, *also* skip the `sudo` invocation —
  use plain `std::fs::remove_dir_all` / `remove_file`. The
  rewritten path lives inside the test sandbox so the test user
  has write perms.
- A `ui::warn` line logs the override when set.

**Exit tests** in `uninstall.rs::tests`:

- `rooted_path_returns_input_when_env_unset`.
- `rooted_path_prefixes_when_env_set`.
- `rooted_path_handles_trailing_slash_in_prefix`.

### W2 — Live test for `mvmctl uninstall --yes` (~½ day)

**Goal**: positive `Uninstall` emit pinned end-to-end against a
sandboxed path prefix.

**Action**:

- `tests/audit_emissions_live.rs`:
  ```rust
  #[test]
  fn uninstall_yes_emits_uninstall_audit_entry() {
      let sandbox = AuditSandbox::new();
      let prefix = sandbox.home_path().join("system-root");
      std::fs::create_dir_all(prefix.join("var/lib/mvm")).expect("mkdir stub");
      std::fs::create_dir_all(prefix.join("usr/local/bin")).expect("mkdir bin");
      std::fs::write(prefix.join("usr/local/bin/mvmctl"), b"#!/bin/sh\nexit 0\n")
          .expect("write stub mvmctl");

      let output = sandbox.mvmctl()
          .env("MVM_UNINSTALL_PATH_PREFIX", &prefix)
          .args(["uninstall", "--yes", "--all"])
          .output().expect("spawn");
      assert!(output.status.success(), ...);

      let log = read_audit_log(&sandbox.audit_log_path());
      assert!(count_entries_with_kind(&log, "uninstall") >= 1);

      // Assert the prefixed paths were actually removed.
      assert!(!prefix.join("var/lib/mvm").exists());
      assert!(!prefix.join("usr/local/bin/mvmctl").exists());
  }
  ```
- Module-doc coverage list update.

**Exit criteria**:

- Test passes.
- The existing `uninstall_dry_run_does_not_emit_audit_entry` test
  continues to pass (dry-run path unchanged).
- xtask lints clean.

## Phasing

W1 → W2 in a single PR (~120 lines total).

## Non-goals

- **`microvm::stop()` mocking.** The best-effort stop logs an error
  and continues regardless of outcome; no mock needed.
- **launchd plist removal.** macOS-specific install cleanup is
  separate from the canonical Uninstall row.

## Success criteria

By plan 70 close:

1. `MVM_UNINSTALL_PATH_PREFIX` rewrites destination paths in
   `uninstall.rs`.
2. The positive `Uninstall` emit is live-pinned via the prefix.
3. xtask lints clean.

## Risk notes

- **Prefix-stripping logic.** Be defensive about double-slash paths
  (`//var/lib/mvm`) and missing leading slash. Unit tests cover both.
- **sudo bypass invariant.** When `MVM_UNINSTALL_PATH_PREFIX` is set,
  the sudo branch is skipped *unconditionally*. A misconfigured
  prod user wouldn't ever set this env-var; the bypass is safe.
  The `ui::warn` log makes the override visible.
- **Path-traversal misuse.** The env-var value is treated as a
  prefix; a user setting `MVM_UNINSTALL_PATH_PREFIX=/` would
  invert the override (no-op). Acceptable — the env-var is an
  internal test hook, not a security boundary.
