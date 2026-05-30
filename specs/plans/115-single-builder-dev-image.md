# Plan 115 — Single builder/dev image with mvmctl-embedded Linux binaries

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement ADR-064. Collapse `nix/images/builder/flake.nix` into `nix/images/builder-vm/flake.nix` with two attrs (`default` headless / `dev` interactive); move `mvm-builder-init` and `mvm-egress-proxy` builds from in-flake `rustPlatform.buildRustPackage` to host-cargo via `mvm-cli`'s `build.rs` with `include_bytes!`-embedded payload; delete the Stage 0 dev-image bootstrap path.

**Architecture:** mvmctl carries the in-VM Linux binaries embedded at its own build time. `crates/mvm-cli/build.rs` cross-compiles via `cargo zigbuild` (or native cargo on aarch64-linux hosts), writes outputs to `$OUT_DIR/mvm-host-bins/`, and the runtime extracts them to `~/.cache/mvm/host-bins/<content-hash>/` on first use. Stage 0 receives the extracted dir via virtio-fs at `/mvm-bins` and the flake reads `MVM_HOST_BIN_DIR` under `--impure` to bake the binaries via `extraFiles`. No `rustPlatform.buildRustPackage` for mvm binaries; no `fetchCrate` on the critical path.

**Tech Stack:** Rust (mvm-cli + xtasks), Nix flakes, cargo-zigbuild for cross-compile from macOS arm64 to aarch64-unknown-linux-gnu, libkrun for the VMM, busybox/nix for the rootfs userland.

---

## Spec

Authoritative spec: `specs/adrs/064-single-builder-dev-image.md` (commit `89dafd28` is the most-detailed Path-C version; the current main version is Path A — this plan tracks main).

## Reference reads (orient before starting)

- `specs/adrs/064-single-builder-dev-image.md` — the ADR.
- `specs/adrs/046-builder-vm-via-libkrun.md` — Plan 72's ADR; the rule we're amending.
- `crates/mvm-build/src/libkrun_builder.rs` — current Stage 0 / persistent builder VM driver.
- `crates/mvm-cli/src/commands/env/apple_container.rs` — the dispatch surface that changes most (search for `find_dev_image_flake`, `bootstrap_builder_vm_image`, `ensure_source_checkout_dev_image`).
- `nix/images/builder-vm/flake.nix` — the flake we restructure.
- `nix/images/builder/flake.nix` — the flake we delete.
- `nix/lib/workspace-filter.nix` — drop `nix/images/builder` from its consumer list.
- `nix/packages/mvm-guest-agent.nix` — sibling pattern (still uses `rustPlatform.buildRustPackage`; out of scope here, will follow same shape later).
- CLAUDE.md "Host dependencies (macOS)" — extended with zig + cargo-zigbuild.

## File structure (new + modified + deleted)

**New files:**

- `nix/lib/mvm-host-binaries.nix` — Nix attrset declaring each binary's install path/mode (single source of truth, flake-side view).
- `crates/mvm-cli/build.rs` — cross-compiles mvm-builder-init + mvm-egress-proxy at mvm-cli build time; writes outputs + SHA-256 to `$OUT_DIR/mvm-host-bins/`.
- `crates/mvm-cli/src/host_binaries/mod.rs` — module entry.
- `crates/mvm-cli/src/host_binaries/manifest.rs` — Rust mirror of the Nix manifest (compile-time constant).
- `crates/mvm-cli/src/host_binaries/embedded.rs` — `include_bytes!`'d binaries + their hashes (generated paths from `$OUT_DIR`).
- `crates/mvm-cli/src/host_binaries/extract.rs` — extraction + SHA-verify logic.
- `xtasks/src/check_mvm_host_binaries_sync.rs` — CI lint asserting Rust manifest and Nix attrset match.
- `tests/host_binaries_extract.rs` — integration test for extraction round-trip.

**Modified files:**

- `Cargo.toml` (workspace root) — add `[workspace.metadata.mvm.toolchain]` block.
- `crates/mvm-cli/Cargo.toml` — add `[build-dependencies]` (toml, sha2, etc.).
- `crates/mvm-cli/src/lib.rs` (or `main.rs`) — wire `host_binaries` module.
- `crates/mvm-cli/src/doctor.rs` — add zig / cargo-zigbuild probe.
- `crates/mvm-cli/src/commands/env/apple_container.rs` — delete dev-image dispatch helpers; call `host_binaries::ensure_extracted()` before Stage 0.
- `crates/mvm-build/src/pipeline/dev_build.rs` — pass `MVM_HOST_BIN_DIR` into in-VM nix invocation.
- `crates/mvm-build/src/libkrun_builder.rs` — mount the host-bin dir at `/mvm-bins` via virtio-fs.
- `nix/images/builder-vm/flake.nix` — restructure: two attrs (`default`, `dev`); drop `rustPlatform.buildRustPackage`; read `MVM_HOST_BIN_DIR` under `--impure`; iterate `mvm-host-binaries.nix` into `extraFiles`.
- `nix/lib/workspace-filter.nix` — comment update (drop `nix/images/builder` from the consumer list of three flakes).
- `CLAUDE.md` — extend "Host dependencies (macOS)" with zig + cargo-zigbuild.
- `.github/workflows/ci.yml` — add `host-bins-sync` lint lane (delegates to xtask).

**Deleted files:**

- `nix/images/builder/flake.nix`
- Any helper / fixture explicitly tied to the deleted flake (search-and-destroy).

---

## Execution sequence

Tasks 1–9 are sequential; the dependency graph is roughly:
T1 (toolchain pin) → T2 (manifest) → T3 (sync xtask) → T4 (build.rs) → T5 (host_binaries module) → T6 (flake restructure) → T7 (dispatch refactor) → T8 (delete dead code) → T9 (E2E + ADR-046 footer).

All work should happen on the `worktree-plan-115-single-builder-dev-image` worktree (create at the start of T1; see saved guidance about always working on worktrees, never main).

---

### Task 1: Pin zig + cargo-zigbuild versions in workspace metadata; doctor probe

**Files:**
- Modify: `Cargo.toml` (workspace root) — add `[workspace.metadata.mvm.toolchain]`
- Modify: `crates/mvm-cli/src/doctor.rs` — new probe helper
- Test: `crates/mvm-cli/tests/doctor_zigbuild_probe.rs`

- [ ] **Step 1: Create the worktree**

```bash
git worktree add /Users/auser/work/tinylabs/mvmco/.worktrees/mvm-plan-115-single-builder-dev-image -b worktree-plan-115-single-builder-dev-image main
cd /Users/auser/work/tinylabs/mvmco/.worktrees/mvm-plan-115-single-builder-dev-image
```

- [ ] **Step 2: Write the failing test**

Create `crates/mvm-cli/tests/doctor_zigbuild_probe.rs`:

```rust
use mvm_cli::doctor::{probe_zigbuild, ZigbuildProbe};

#[test]
fn probe_reports_pinned_versions_from_workspace_metadata() {
    let probe = probe_zigbuild();
    // Pinned versions live in Cargo.toml under
    // [workspace.metadata.mvm.toolchain]. The probe surfaces them
    // so a contributor can compare against what `zig --version`
    // reports.
    assert!(!probe.pinned_zig.is_empty(), "pinned zig version missing");
    assert!(!probe.pinned_cargo_zigbuild.is_empty(), "pinned cargo-zigbuild version missing");
}
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cargo test -p mvm-cli --test doctor_zigbuild_probe -- --nocapture
```

Expected: FAIL — `probe_zigbuild` / `ZigbuildProbe` not defined.

- [ ] **Step 4: Add workspace.metadata.mvm.toolchain block**

Append to root `Cargo.toml`:

```toml
[workspace.metadata.mvm.toolchain]
# Pinned cross-compile toolchain for the embedded host-vm binaries
# baked into mvmctl. Bump in lockstep across this file and CI to keep
# the embedded payload reproducible (Plan 115 / ADR-064 claim 11).
zig = "0.13.0"
cargo-zigbuild = "0.20.0"
target = "aarch64-unknown-linux-gnu.2.17"
```

- [ ] **Step 5: Add the probe implementation**

Append to `crates/mvm-cli/src/doctor.rs`:

```rust
/// Pinned cross-compile toolchain versions, parsed at build time from
/// the workspace's [workspace.metadata.mvm.toolchain] block.
pub struct ZigbuildProbe {
    pub pinned_zig: String,
    pub pinned_cargo_zigbuild: String,
    pub pinned_target: String,
    pub installed_zig: Option<String>,
    pub installed_cargo_zigbuild: Option<String>,
}

/// Read the pinned versions baked in at compile time and probe the
/// installed versions on the host. Used by `mvmctl doctor` to warn
/// when contributor toolchain drifts from the pin.
pub fn probe_zigbuild() -> ZigbuildProbe {
    ZigbuildProbe {
        pinned_zig: env!("MVM_PINNED_ZIG").to_string(),
        pinned_cargo_zigbuild: env!("MVM_PINNED_CARGO_ZIGBUILD").to_string(),
        pinned_target: env!("MVM_PINNED_TARGET").to_string(),
        installed_zig: which_version("zig", &["version"]),
        installed_cargo_zigbuild: which_version("cargo-zigbuild", &["--version"]),
    }
}

fn which_version(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
```

- [ ] **Step 6: Expose env vars from build.rs (skeleton)**

Create `crates/mvm-cli/build.rs` (minimal — we'll add cross-compile in T4):

```rust
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let workspace_cargo_toml = workspace_root.join("Cargo.toml");

    let toml_str = std::fs::read_to_string(&workspace_cargo_toml)
        .expect("read workspace Cargo.toml");
    let parsed: toml::Value = toml::from_str(&toml_str)
        .expect("parse workspace Cargo.toml");
    let pin = &parsed["workspace"]["metadata"]["mvm"]["toolchain"];

    let zig = pin["zig"].as_str().expect("zig pin missing");
    let zb = pin["cargo-zigbuild"].as_str().expect("cargo-zigbuild pin missing");
    let tgt = pin["target"].as_str().expect("target pin missing");

    println!("cargo:rustc-env=MVM_PINNED_ZIG={zig}");
    println!("cargo:rustc-env=MVM_PINNED_CARGO_ZIGBUILD={zb}");
    println!("cargo:rustc-env=MVM_PINNED_TARGET={tgt}");

    println!("cargo:rerun-if-changed={}", workspace_cargo_toml.display());
}
```

Add to `crates/mvm-cli/Cargo.toml`:

```toml
[build-dependencies]
toml = "0.8"
```

- [ ] **Step 7: Run test to verify it passes**

```bash
cargo test -p mvm-cli --test doctor_zigbuild_probe -- --nocapture
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/mvm-cli/Cargo.toml crates/mvm-cli/build.rs \
        crates/mvm-cli/src/doctor.rs \
        crates/mvm-cli/tests/doctor_zigbuild_probe.rs
git commit -m "feat(plan-115): pin zig/cargo-zigbuild versions + doctor probe (ADR-064)"
```

---

### Task 2: Create `nix/lib/mvm-host-binaries.nix` and the Rust mirror

**Files:**
- Create: `nix/lib/mvm-host-binaries.nix`
- Create: `crates/mvm-cli/src/host_binaries/mod.rs`
- Create: `crates/mvm-cli/src/host_binaries/manifest.rs`
- Test: `crates/mvm-cli/tests/host_binaries_manifest.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/mvm-cli/tests/host_binaries_manifest.rs`:

```rust
use mvm_cli::host_binaries::manifest::{HOST_BINARIES, HostBinary};

#[test]
fn manifest_lists_mvm_builder_init_and_egress_proxy() {
    let names: Vec<&str> = HOST_BINARIES.iter().map(|b| b.name).collect();
    assert!(names.contains(&"mvm-builder-init"));
    assert!(names.contains(&"mvm-egress-proxy"));
    assert_eq!(HOST_BINARIES.len(), 2,
        "expected exactly two host binaries in this ADR's scope");
}

#[test]
fn manifest_install_paths_match_adr_064() {
    let by_name = |n: &str| -> &HostBinary {
        HOST_BINARIES.iter().find(|b| b.name == n).unwrap()
    };
    assert_eq!(by_name("mvm-builder-init").install_path, "/sbin/mvm-builder-init");
    assert_eq!(by_name("mvm-builder-init").mode, 0o755);
    assert_eq!(by_name("mvm-egress-proxy").install_path, "/sbin/mvm-egress-proxy");
    assert_eq!(by_name("mvm-egress-proxy").mode, 0o755);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p mvm-cli --test host_binaries_manifest -- --nocapture
```

Expected: FAIL — module not found.

- [ ] **Step 3: Create `nix/lib/mvm-host-binaries.nix`**

```nix
# Single source of truth (Nix view) for the mvm-internal Linux
# binaries that mvmctl embeds and bakes into the builder/dev VM
# rootfs via extraFiles. The Rust mirror at
# crates/mvm-cli/src/host_binaries/manifest.rs must agree on the
# name set and install paths; CI enforces parity (see
# xtasks/src/check_mvm_host_binaries_sync.rs).
#
# Adding a binary here is part of the Plan 115 / ADR-064 contract;
# new uses of rustPlatform.buildRustPackage in mvm's flakes are
# forbidden (see ADR-064 §Principle).
{
  mvm-builder-init = {
    install_path = "/sbin/mvm-builder-init";
    mode = "0755";
  };
  mvm-egress-proxy = {
    install_path = "/sbin/mvm-egress-proxy";
    mode = "0755";
  };
}
```

- [ ] **Step 4: Create the Rust mirror**

Create `crates/mvm-cli/src/host_binaries/mod.rs`:

```rust
//! Plan 115 / ADR-064 — mvm's Linux binaries embedded in mvmctl.
//!
//! Three submodules:
//!   - `manifest` — compile-time list of embedded binaries,
//!     mirrored in `nix/lib/mvm-host-binaries.nix`.
//!   - `embedded` — `include_bytes!`'d payload + SHA-256 hashes
//!     produced by `build.rs`.
//!   - `extract` — race-safe extraction to
//!     `~/.cache/mvm/host-bins/<content-hash>/` on first use.

pub mod manifest;
// pub mod embedded;   // added in Task 4
// pub mod extract;    // added in Task 5
```

Create `crates/mvm-cli/src/host_binaries/manifest.rs`:

```rust
//! Compile-time Rust mirror of `nix/lib/mvm-host-binaries.nix`.
//! Parity with the Nix attrset is asserted by the
//! `check-mvm-host-binaries-sync` xtask (Task 3).

#[derive(Debug, Clone, Copy)]
pub struct HostBinary {
    /// Cargo package name + name on disk after extraction.
    pub name: &'static str,
    /// Absolute path inside the builder/dev VM rootfs.
    pub install_path: &'static str,
    /// Unix mode (e.g. 0o755) applied via the flake's extraFiles.
    pub mode: u32,
}

pub const HOST_BINARIES: &[HostBinary] = &[
    HostBinary {
        name: "mvm-builder-init",
        install_path: "/sbin/mvm-builder-init",
        mode: 0o755,
    },
    HostBinary {
        name: "mvm-egress-proxy",
        install_path: "/sbin/mvm-egress-proxy",
        mode: 0o755,
    },
];
```

- [ ] **Step 5: Wire the module into mvm-cli**

Add to `crates/mvm-cli/src/lib.rs` (if it exists; else `main.rs`):

```rust
pub mod host_binaries;
```

- [ ] **Step 6: Run test to verify it passes**

```bash
cargo test -p mvm-cli --test host_binaries_manifest -- --nocapture
```

Expected: PASS — both tests green.

- [ ] **Step 7: Commit**

```bash
git add nix/lib/mvm-host-binaries.nix \
        crates/mvm-cli/src/host_binaries/mod.rs \
        crates/mvm-cli/src/host_binaries/manifest.rs \
        crates/mvm-cli/src/lib.rs \
        crates/mvm-cli/tests/host_binaries_manifest.rs
git commit -m "feat(plan-115): mvm-host-binaries manifest (Nix + Rust mirror, ADR-064)"
```

---

### Task 3: CI sync check (xtask) — Nix attrset ⇔ Rust manifest

**Files:**
- Create: `xtasks/src/check_mvm_host_binaries_sync.rs`
- Modify: `xtasks/src/main.rs` (or equivalent dispatcher) — register the new subcommand
- Modify: `.github/workflows/ci.yml` — add a lane that runs the xtask
- Test: `xtasks/tests/check_sync.rs`

- [ ] **Step 1: Write the failing test**

Create `xtasks/tests/check_sync.rs`:

```rust
use std::process::Command;

#[test]
fn xtask_check_sync_passes_on_main() {
    let status = Command::new("cargo")
        .args(["xtask", "check-mvm-host-binaries-sync"])
        .status()
        .expect("spawn cargo xtask");
    assert!(status.success(),
        "xtask reported a sync drift between Rust manifest and Nix attrset");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p xtasks --test check_sync -- --nocapture
```

Expected: FAIL — `check-mvm-host-binaries-sync` is not a registered xtask command.

- [ ] **Step 3: Implement the xtask**

Create `xtasks/src/check_mvm_host_binaries_sync.rs`:

```rust
//! Plan 115 / ADR-064 CI lint — asserts the Rust manifest in
//! `crates/mvm-cli/src/host_binaries/manifest.rs` and the Nix
//! attrset in `nix/lib/mvm-host-binaries.nix` agree on the set
//! of entries and their install paths. Adding or renaming a
//! binary requires updating both files in the same PR.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub fn run(workspace_root: &Path) -> Result<(), String> {
    let rust_entries = parse_rust_manifest(workspace_root)?;
    let nix_entries = parse_nix_attrset(workspace_root)?;
    if rust_entries != nix_entries {
        return Err(format!(
            "drift between manifests:\n  Rust: {:#?}\n  Nix:  {:#?}\n\n\
             Fix: ensure crates/mvm-cli/src/host_binaries/manifest.rs and \
             nix/lib/mvm-host-binaries.nix list the same entries with the \
             same install_path.",
            rust_entries, nix_entries
        ));
    }
    println!("host-binaries manifests agree ({} entries)", rust_entries.len());
    Ok(())
}

fn parse_rust_manifest(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join("crates/mvm-cli/src/host_binaries/manifest.rs");
    let src = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    // Tiny ad-hoc parser: grep for `name: "<x>"` and
    // `install_path: "<y>"` pairs in source order. Robust enough
    // because the file is hand-written and the format is stable.
    let mut out = BTreeMap::new();
    let mut current_name: Option<String> = None;
    for line in src.lines() {
        if let Some(n) = extract_quoted_after(line, "name:") {
            current_name = Some(n);
        }
        if let Some(p) = extract_quoted_after(line, "install_path:") {
            if let Some(n) = current_name.take() {
                out.insert(n, p);
            }
        }
    }
    Ok(out)
}

fn parse_nix_attrset(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join("nix/lib/mvm-host-binaries.nix");
    let src = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    // The Nix file is tiny + hand-formatted; we do a similar
    // grep-style parse. Replace with a real Nix parser only if
    // the file grows complex.
    let mut out = BTreeMap::new();
    let mut current_name: Option<String> = None;
    for line in src.lines() {
        let t = line.trim();
        if let Some(eq) = t.find(" = {") {
            let n = t[..eq].trim().to_string();
            if !n.is_empty() && !n.starts_with('#') && !n.starts_with('{') {
                current_name = Some(n);
            }
        }
        if let Some(p) = extract_quoted_after(line, "install_path =") {
            if let Some(n) = current_name.take() {
                out.insert(n, p);
            }
        }
    }
    Ok(out)
}

fn extract_quoted_after(line: &str, key: &str) -> Option<String> {
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let q1 = rest.find('"')? + 1;
    let q2 = rest[q1..].find('"')?;
    Some(rest[q1..q1 + q2].to_string())
}
```

- [ ] **Step 4: Wire into the xtask dispatcher**

In `xtasks/src/main.rs`, add the subcommand:

```rust
mod check_mvm_host_binaries_sync;

// in the match:
"check-mvm-host-binaries-sync" => {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    check_mvm_host_binaries_sync::run(&root)
        .map_err(|e| { eprintln!("{e}"); std::process::exit(1); }).unwrap();
}
```

- [ ] **Step 5: Add CI lane**

Append to `.github/workflows/ci.yml` (in the existing test matrix or as a new job):

```yaml
host-bins-sync:
  name: host-bins manifest parity
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - run: cargo xtask check-mvm-host-binaries-sync
```

- [ ] **Step 6: Run test to verify it passes**

```bash
cargo test -p xtasks --test check_sync -- --nocapture
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add xtasks/src/check_mvm_host_binaries_sync.rs \
        xtasks/src/main.rs \
        xtasks/tests/check_sync.rs \
        .github/workflows/ci.yml
git commit -m "feat(plan-115): xtask + CI lane for host-binaries manifest sync (ADR-064)"
```

---

### Task 4: `build.rs` cross-compiles via cargo-zigbuild

**Files:**
- Modify: `crates/mvm-cli/build.rs` — add cross-compile orchestration
- Create: `crates/mvm-cli/src/host_binaries/embedded.rs` — `include_bytes!` + hashes
- Test: `crates/mvm-cli/tests/embedded_binaries.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/mvm-cli/tests/embedded_binaries.rs`:

```rust
use mvm_cli::host_binaries::embedded::EMBEDDED;
use sha2::{Digest, Sha256};

#[test]
fn each_embedded_binary_starts_with_elf_magic() {
    for bin in EMBEDDED.iter() {
        assert!(bin.bytes.len() > 1024, "{}: implausibly small payload", bin.name);
        // aarch64-unknown-linux-gnu binaries are ELF; the first
        // four bytes are 0x7F E L F.
        assert_eq!(&bin.bytes[..4], &[0x7F, b'E', b'L', b'F'],
            "{}: payload is not an ELF binary", bin.name);
    }
}

#[test]
fn embedded_sha256_matches_payload() {
    for bin in EMBEDDED.iter() {
        let mut h = Sha256::new();
        h.update(bin.bytes);
        let actual = hex::encode(h.finalize());
        assert_eq!(actual, bin.sha256_hex,
            "{}: embedded hash drift", bin.name);
    }
}
```

Add `[dev-dependencies] sha2 = "0.10"`, `hex = "0.4"` to `crates/mvm-cli/Cargo.toml`.

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p mvm-cli --test embedded_binaries -- --nocapture
```

Expected: FAIL — `embedded::EMBEDDED` not defined.

- [ ] **Step 3: Extend `build.rs` to cross-compile + emit `embedded.rs`**

Replace `crates/mvm-cli/build.rs` with:

```rust
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap().to_path_buf();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let bins_out = out_dir.join("mvm-host-bins");
    std::fs::create_dir_all(&bins_out).expect("create OUT_DIR/mvm-host-bins");

    let pin = read_pinned_toolchain(&workspace_root);
    println!("cargo:rustc-env=MVM_PINNED_ZIG={}", pin.zig);
    println!("cargo:rustc-env=MVM_PINNED_CARGO_ZIGBUILD={}", pin.cargo_zigbuild);
    println!("cargo:rustc-env=MVM_PINNED_TARGET={}", pin.target);

    let manifest = read_rust_manifest(&workspace_root);
    let mut entries = Vec::new();

    let host_triple = std::env::var("HOST").unwrap();
    let native = host_triple.contains("linux") && host_triple.contains(strip_glibc(&pin.target));

    for name in manifest.iter() {
        let out_file = bins_out.join(name);
        if native {
            run_cargo_build(&workspace_root, name, &pin.target, &out_file);
        } else {
            run_cargo_zigbuild(&workspace_root, name, &pin.target, &out_file);
        }
        let sha = sha256_hex(&out_file);
        entries.push((name.clone(), out_file.clone(), sha));
        println!("cargo:rerun-if-changed=crates/{name}/src");
    }

    let embedded_rs = render_embedded_rs(&entries);
    std::fs::write(out_dir.join("embedded.rs"), embedded_rs).unwrap();
    println!("cargo:rerun-if-changed={}", workspace_root.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", workspace_root.join("crates/mvm-cli/src/host_binaries/manifest.rs").display());
}

struct Pin { zig: String, cargo_zigbuild: String, target: String }

fn read_pinned_toolchain(root: &Path) -> Pin {
    let toml_str = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let v: toml::Value = toml::from_str(&toml_str).unwrap();
    let p = &v["workspace"]["metadata"]["mvm"]["toolchain"];
    Pin {
        zig: p["zig"].as_str().unwrap().to_string(),
        cargo_zigbuild: p["cargo-zigbuild"].as_str().unwrap().to_string(),
        target: p["target"].as_str().unwrap().to_string(),
    }
}

fn read_rust_manifest(root: &Path) -> Vec<String> {
    // Single source of truth for build.rs is the Rust manifest;
    // the Nix attrset's parity is asserted separately by the
    // xtask sync lint (Task 3).
    let src = std::fs::read_to_string(
        root.join("crates/mvm-cli/src/host_binaries/manifest.rs"),
    ).unwrap();
    let mut out = Vec::new();
    for line in src.lines() {
        if let Some(n) = line.split("name:").nth(1) {
            if let Some(q1) = n.find('"') {
                if let Some(q2) = n[q1 + 1..].find('"') {
                    out.push(n[q1 + 1..q1 + 1 + q2].to_string());
                }
            }
        }
    }
    out
}

fn strip_glibc(t: &str) -> &str {
    // "aarch64-unknown-linux-gnu.2.17" → "aarch64-unknown-linux-gnu"
    t.split('.').next().unwrap()
}

fn run_cargo_zigbuild(root: &Path, pkg: &str, target: &str, out: &Path) {
    eprintln!("[build.rs] cargo zigbuild -p {pkg} --target {target}");
    let status = Command::new("cargo")
        .args(["zigbuild", "--release", "--target", target, "-p", pkg])
        .current_dir(root)
        .status()
        .expect("spawn cargo zigbuild — install with `brew install zig && cargo install cargo-zigbuild`");
    assert!(status.success(), "cargo zigbuild failed for {pkg}");
    let built = root.join("target").join(strip_glibc(target)).join("release").join(pkg);
    std::fs::copy(&built, out).expect(&format!("copy {} → {}", built.display(), out.display()));
}

fn run_cargo_build(root: &Path, pkg: &str, target: &str, out: &Path) {
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", strip_glibc(target), "-p", pkg])
        .current_dir(root)
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "cargo build failed for {pkg}");
    let built = root.join("target").join(strip_glibc(target)).join("release").join(pkg);
    std::fs::copy(&built, out).expect(&format!("copy {} → {}", built.display(), out.display()));
}

fn sha256_hex(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(p).expect(&format!("read {}", p.display()));
    let mut h = Sha256::new();
    h.update(&bytes);
    format!("{:x}", h.finalize())
}

fn render_embedded_rs(entries: &[(String, PathBuf, String)]) -> String {
    let mut s = String::new();
    s.push_str("//! Generated by mvm-cli/build.rs. Do not edit.\n\n");
    s.push_str("pub struct EmbeddedBinary { pub name: &'static str, pub bytes: &'static [u8], pub sha256_hex: &'static str }\n\n");
    s.push_str("pub const EMBEDDED: &[EmbeddedBinary] = &[\n");
    for (name, path, sha) in entries {
        s.push_str(&format!(
            "    EmbeddedBinary {{ name: {name:?}, bytes: include_bytes!({path:?}), sha256_hex: {sha:?} }},\n"
        ));
    }
    s.push_str("];\n");
    s
}
```

Update `crates/mvm-cli/Cargo.toml` `[build-dependencies]`:

```toml
[build-dependencies]
toml = "0.8"
sha2 = "0.10"
```

- [ ] **Step 4: Create the `embedded.rs` module wrapper**

Create `crates/mvm-cli/src/host_binaries/embedded.rs`:

```rust
//! Generated by build.rs at compile time. The actual contents
//! (EmbeddedBinary struct + EMBEDDED constant) live in
//! $OUT_DIR/embedded.rs and are included verbatim here.
include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
```

Uncomment `pub mod embedded;` in `crates/mvm-cli/src/host_binaries/mod.rs`.

- [ ] **Step 5: Run test to verify it passes (with toolchain installed)**

If on macOS, ensure: `brew install zig && cargo install cargo-zigbuild`.

```bash
cargo test -p mvm-cli --test embedded_binaries -- --nocapture
```

Expected: PASS — both tests green; the embedded binaries are valid aarch64 ELFs.

- [ ] **Step 6: Commit**

```bash
git add crates/mvm-cli/build.rs \
        crates/mvm-cli/Cargo.toml \
        crates/mvm-cli/src/host_binaries/mod.rs \
        crates/mvm-cli/src/host_binaries/embedded.rs \
        crates/mvm-cli/tests/embedded_binaries.rs
git commit -m "feat(plan-115): build.rs cross-compiles + embeds host binaries via include_bytes!"
```

---

### Task 5: Runtime extraction module

**Files:**
- Create: `crates/mvm-cli/src/host_binaries/extract.rs`
- Test: `crates/mvm-cli/tests/host_binaries_extract.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/mvm-cli/tests/host_binaries_extract.rs`:

```rust
use mvm_cli::host_binaries::extract::ensure_extracted;
use std::os::unix::fs::PermissionsExt;

#[test]
fn ensure_extracted_writes_all_binaries_with_matching_sha() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = ensure_extracted(tmp.path()).expect("extract");

    use mvm_cli::host_binaries::embedded::EMBEDDED;
    for bin in EMBEDDED.iter() {
        let p = dir.join(bin.name);
        assert!(p.exists(), "missing {}", bin.name);
        let bytes = std::fs::read(&p).unwrap();
        let mut h = sha2::Sha256::new();
        sha2::Digest::update(&mut h, &bytes);
        let actual = hex::encode(sha2::Digest::finalize(h));
        assert_eq!(actual, bin.sha256_hex, "{}: SHA drift", bin.name);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755, "{}: wrong mode", bin.name);
    }
}

#[test]
fn ensure_extracted_is_idempotent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir1 = ensure_extracted(tmp.path()).unwrap();
    let dir2 = ensure_extracted(tmp.path()).unwrap();
    assert_eq!(dir1, dir2);
}
```

Add `[dev-dependencies] tempfile = "3"` to `crates/mvm-cli/Cargo.toml`.

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p mvm-cli --test host_binaries_extract -- --nocapture
```

Expected: FAIL — `extract::ensure_extracted` not defined.

- [ ] **Step 3: Implement extraction**

Create `crates/mvm-cli/src/host_binaries/extract.rs`:

```rust
//! Idempotent extraction of embedded host-vm binaries to a
//! content-hashed dir under the supplied cache root (typically
//! `~/.cache/mvm/host-bins`). Re-verifies each binary's SHA-256
//! against the embedded constant on every call — a corrupted or
//! tampered on-disk cache fails closed.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::embedded::EMBEDDED;

pub fn ensure_extracted(cache_root: &Path) -> std::io::Result<PathBuf> {
    let combined_hash = combined_hash_hex();
    let target = cache_root.join(&combined_hash);
    std::fs::create_dir_all(&target)?;
    // Lock the parent + restrict its perms.
    let perm = std::fs::Permissions::from_mode(0o700);
    let _ = std::fs::set_permissions(cache_root, perm.clone());
    let _ = std::fs::set_permissions(&target, perm);

    for bin in EMBEDDED.iter() {
        let final_path = target.join(bin.name);
        if final_path.exists() && verify_sha(&final_path, bin.sha256_hex)? {
            continue;
        }
        write_atomic(&final_path, bin.bytes, 0o755)?;
        if !verify_sha(&final_path, bin.sha256_hex)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("post-extract SHA mismatch for {}", bin.name),
            ));
        }
    }
    Ok(target)
}

fn combined_hash_hex() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for bin in EMBEDDED.iter() {
        h.update(bin.name.as_bytes());
        h.update(bin.sha256_hex.as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn verify_sha(path: &Path, expected_hex: &str) -> std::io::Result<bool> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()) == expected_hex)
}

fn write_atomic(target: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let tmp = target.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        rand_suffix()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    // Atomic rename within the same dir.
    std::fs::rename(&tmp, target)
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{n:x}")
}
```

Uncomment `pub mod extract;` in `crates/mvm-cli/src/host_binaries/mod.rs`.

Add to `crates/mvm-cli/Cargo.toml`:

```toml
[dependencies]
sha2 = "0.10"
hex = "0.4"
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p mvm-cli --test host_binaries_extract -- --nocapture
```

Expected: PASS — both tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/mvm-cli/src/host_binaries/mod.rs \
        crates/mvm-cli/src/host_binaries/extract.rs \
        crates/mvm-cli/Cargo.toml \
        crates/mvm-cli/tests/host_binaries_extract.rs
git commit -m "feat(plan-115): runtime extraction with SHA verify + atomic write"
```

---

### Task 6: Restructure `nix/images/builder-vm/flake.nix`

**Files:**
- Modify: `nix/images/builder-vm/flake.nix` — two attrs, MVM_HOST_BIN_DIR, manifest iteration
- Modify: `nix/lib/workspace-filter.nix` — drop `nix/images/builder` from consumer list

- [ ] **Step 1: Read current flake** to understand the existing structure.

```bash
sed -n '195,260p' nix/images/builder-vm/flake.nix
```

Note the two `rustPlatform.buildRustPackage` call sites (around lines 209, 235) and the `extraFiles` block that uses their outputs.

- [ ] **Step 2: Replace rustPlatform call sites with extraFiles entries that read MVM_HOST_BIN_DIR**

In `nix/images/builder-vm/flake.nix`, **delete** the `mvmBuilderInitFor` and `mvmEgressProxyFor` helper definitions and **delete** the corresponding `rustPlatform.buildRustPackage` blocks. **Add** at the top of `outputs`'s `let`:

```nix
hostBinDir =
  let envPath = builtins.getEnv "MVM_HOST_BIN_DIR";
  in if envPath != ""
     then /. + envPath
     else throw ''
       MVM_HOST_BIN_DIR is not set. Plan 115 / ADR-064 contract:
       mvmctl populates this dir via host_binaries::ensure_extracted()
       before invoking `nix build path:... --impure`. To run nix
       build by hand: extract the embedded binaries from your
       mvmctl with `mvmctl inspect host-bins --extract-to <DIR>`
       and pass MVM_HOST_BIN_DIR=<DIR> --impure.
     '';

hostBinaries = import (workspaceRoot + "/nix/lib/mvm-host-binaries.nix");

hostBinExtraFiles = nixpkgs.lib.mapAttrs' (name: spec:
  nixpkgs.lib.nameValuePair spec.install_path {
    source = hostBinDir + "/${name}";
    mode = spec.mode;
  }
) hostBinaries;
```

Update the `mkBuilderImage` derivation to use `hostBinExtraFiles` (merged with any other extraFiles already there).

Split outputs into two attrs:

```nix
packages = forAllSystems (system:
  let pkgs = import nixpkgs { inherit system; };
  in {
    default = mkBuilderImage { inherit system; interactive = false; };
    dev     = mkBuilderImage { inherit system; interactive = true; };
  });
```

`mkBuilderImage` takes `interactive` and conditionally appends bashInteractive + cargo + rustc + an editor + motd for the `dev` attr.

- [ ] **Step 3: Smoke-test the flake parses**

```bash
nix flake check ./nix/images/builder-vm/ --impure
```

Expected: PASS (or a Nix-specific evaluation success — does not require a successful build).

Note: actual `nix build` only works once Task 7 wires MVM_HOST_BIN_DIR through the host-side dispatch. For now we're just verifying the flake evaluates.

- [ ] **Step 4: Update workspace-filter consumer comment**

In `nix/lib/workspace-filter.nix`, update the leading comment:

```nix
# Single source of truth for filtering the host workspace tree into
# the Nix store when building images. Used by:
#
#   nix/images/builder-vm/flake.nix
#   nix/images/runtime-overlay/flake.nix
#
# (Plan 115 / ADR-064 deleted nix/images/builder/flake.nix; its
# consumer slot is gone.)
```

- [ ] **Step 5: Commit**

```bash
git add nix/images/builder-vm/flake.nix nix/lib/workspace-filter.nix
git commit -m "refactor(builder-vm/flake): two attrs (default/dev), read MVM_HOST_BIN_DIR (ADR-064)"
```

---

### Task 7: Wire `host_binaries::ensure_extracted` into the dispatch path

**Files:**
- Modify: `crates/mvm-build/src/pipeline/dev_build.rs` — accept + pass MVM_HOST_BIN_DIR
- Modify: `crates/mvm-build/src/libkrun_builder.rs` — mount the host-bin dir at `/mvm-bins`
- Modify: `crates/mvm-cli/src/commands/env/apple_container.rs` — call `host_binaries::ensure_extracted()` before invoking the builder VM dispatch

- [ ] **Step 1: Locate every site that calls dev_build**

```bash
grep -rn "dev_build_with_builder_vm\|dev_build_via_builder_vm\|dev_build(" crates/mvm-build/src/pipeline/dev_build.rs crates/mvm-cli/src/commands/env/
```

- [ ] **Step 2: Add `host_bin_dir: PathBuf` to BuilderMounts**

In `crates/mvm-build/src/builder_vm.rs`, extend the `BuilderMounts` struct:

```rust
pub struct BuilderMounts {
    pub flake_src: std::path::PathBuf,
    pub host_nix_store: Option<std::path::PathBuf>,
    pub artifact_out: std::path::PathBuf,
    /// Plan 115 / ADR-064: dir containing the mvm host-vm binaries
    /// extracted from mvmctl's embedded payload, mounted at
    /// `/mvm-bins` inside the builder VM and exposed via
    /// MVM_HOST_BIN_DIR to the flake.
    pub host_bin_dir: std::path::PathBuf,
}
```

Update all construction sites and all consumers (search-and-fix; the compiler tells you).

- [ ] **Step 3: Mount `/mvm-bins` and set MVM_HOST_BIN_DIR in libkrun_builder**

In `crates/mvm-build/src/libkrun_builder.rs`'s `run_build`, after the existing `/work` virtio-fs share setup, add:

```rust
let host_bin_share = VirtioFsShare {
    host_path: mounts.host_bin_dir.clone(),
    guest_path: "/mvm-bins".to_string(),
    read_only: true,
};
shares.push(host_bin_share);
```

And in the env that goes to cmd.sh (search `MVM_WORKSPACE_PATH` for the pattern), add:

```rust
env.insert("MVM_HOST_BIN_DIR".to_string(), "/mvm-bins".to_string());
```

- [ ] **Step 4: Call `ensure_extracted` before dispatch**

In `crates/mvm-cli/src/commands/env/apple_container.rs`'s `cmd_dev_libkrun` (and the Vz sibling), before constructing the mounts:

```rust
use mvm_cli::host_binaries::extract;

let cache_root = format!("{}/host-bins", mvm_core::config::mvm_cache_dir());
let host_bin_dir = extract::ensure_extracted(std::path::Path::new(&cache_root))
    .map_err(|e| anyhow::anyhow!("extract embedded host-vm binaries: {e}"))?;
```

Pass `host_bin_dir` into the mounts struct.

- [ ] **Step 5: Add an integration test for the dispatch path**

Create `crates/mvm-cli/tests/dispatch_host_bin_dir.rs`:

```rust
#[test]
#[cfg(feature = "builder-vm")]
fn dispatch_populates_host_bin_dir_before_builder_call() {
    use mvm_cli::host_binaries::extract::ensure_extracted;
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = ensure_extracted(tmp.path()).unwrap();
    assert!(dir.join("mvm-builder-init").exists());
    assert!(dir.join("mvm-egress-proxy").exists());
}
```

- [ ] **Step 6: Run cargo build + the test**

```bash
cargo build -p mvm-cli
cargo test -p mvm-cli --test dispatch_host_bin_dir -- --nocapture
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/mvm-build/src/builder_vm.rs \
        crates/mvm-build/src/libkrun_builder.rs \
        crates/mvm-build/src/pipeline/dev_build.rs \
        crates/mvm-cli/src/commands/env/apple_container.rs \
        crates/mvm-cli/tests/dispatch_host_bin_dir.rs
git commit -m "feat(plan-115): dispatch extracts embedded host-bins + mounts /mvm-bins (ADR-064)"
```

---

### Task 8: Delete the dev-image flake + the dead Stage-0 dispatch helpers

**Files:**
- Delete: `nix/images/builder/flake.nix`
- Modify: `crates/mvm-cli/src/commands/env/apple_container.rs` — remove `find_dev_image_flake`, `ensure_source_checkout_dev_image`, `resolve_source_checkout_dev_image`, `bootstrap_builder_vm_image_via_dev_image_stage0`
- Remove or update tests that referenced the deleted helpers

- [ ] **Step 1: Grep for callers of the helpers**

```bash
grep -rn "find_dev_image_flake\|ensure_source_checkout_dev_image\|resolve_source_checkout_dev_image\|bootstrap_builder_vm_image_via_dev_image_stage0" crates/ tests/
```

Every caller must be updated to not need the helper (they cease to exist after this task).

- [ ] **Step 2: Delete the dev-image flake**

```bash
git rm nix/images/builder/flake.nix
```

- [ ] **Step 3: Delete the helpers**

Open `crates/mvm-cli/src/commands/env/apple_container.rs` and delete the four functions plus the dispatch branch in `bootstrap_builder_vm_image` that called them. The `bootstrap_builder_vm_image_via_root_dir_stage0` (Alpine path) is now the only path.

- [ ] **Step 4: Update or remove tests**

```bash
grep -rn "find_dev_image_flake\|bootstrap_builder_vm_image_via_dev_image_stage0" crates/mvm-cli/tests/ crates/mvm-cli/src/commands/env/
```

For each match, either rewrite the test against the surviving Alpine path or delete it if it's purely about the deleted dispatch.

- [ ] **Step 5: Cargo build + test**

```bash
cargo build --workspace
cargo test --workspace --no-fail-fast
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
git rm nix/images/builder/flake.nix
git add crates/mvm-cli/src/commands/env/apple_container.rs crates/mvm-cli/tests/
git commit -m "refactor(plan-115): delete dev-image flake + Stage 0 dev-image dispatch (ADR-064)"
```

---

### Task 9: E2E smoke + CLAUDE.md update + ADR-046 footer

**Files:**
- Modify: `CLAUDE.md` — extend "Host dependencies (macOS)" with zig + cargo-zigbuild
- Modify: `specs/adrs/046-builder-vm-via-libkrun.md` — add "Superseded in part by ADR-064" footer
- Create: `crates/mvm-cli/tests/dev_up_smoke.rs` — gated E2E smoke

- [ ] **Step 1: Update CLAUDE.md "Host dependencies (macOS)"**

After the existing Homebrew trio, add:

```markdown
For source-checkout contributors only: zig + cargo-zigbuild are needed
at `cargo build`-of-mvmctl time so `crates/mvm-cli/build.rs` can
cross-compile the embedded host-vm binaries (`mvm-builder-init`,
`mvm-egress-proxy`) for aarch64-unknown-linux-gnu. See
Plan 115 / ADR-064.

```sh
brew install zig
cargo install cargo-zigbuild
```

End-users running a downloaded mvmctl don't need either tool — the
binaries are already embedded.
```

- [ ] **Step 2: Add the ADR-046 footer**

Append to `specs/adrs/046-builder-vm-via-libkrun.md`:

```markdown
---

> **Superseded in part by ADR-064 (Plan 115).** ADR-046's
> "Two artifact layers, two acquisition paths" rule is amended:
> the dev image and the builder VM image collapse into a single
> flake with two attrs (`default` / `dev`); mvm's own Linux
> binaries are embedded in mvmctl at its own build time rather
> than re-built per `dev up`. See ADR-064.
```

- [ ] **Step 3: Add the gated E2E smoke**

Create `crates/mvm-cli/tests/dev_up_smoke.rs`:

```rust
// Plan 115 / ADR-064 E2E smoke. Boots Stage 0, lets it produce
// the builder-VM image with the embedded host-bins baked in,
// asserts the produced rootfs.ext4 has the expected files with
// the expected SHA-256.
//
// Gated on `MVM_E2E_SMOKE=1` because it requires a working
// libkrun + zigbuild toolchain and runs for several minutes.

#[test]
fn dev_up_e2e_smoke() {
    if std::env::var("MVM_E2E_SMOKE").ok().as_deref() != Some("1") {
        eprintln!("skipping E2E smoke; set MVM_E2E_SMOKE=1 to run");
        return;
    }
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_mvmctl"))
        .args(["dev", "up"])
        .status()
        .expect("spawn mvmctl");
    assert!(status.success(), "mvmctl dev up failed");
    // Caller is responsible for `mvmctl dev down` between runs.
}
```

- [ ] **Step 4: Run a local E2E smoke**

```bash
cargo build -p mvm-cli --release
MVM_E2E_SMOKE=1 cargo test -p mvm-cli --release --test dev_up_smoke -- --nocapture
mvmctl dev down  # clean up after the smoke
```

Expected: PASS — `mvmctl dev up` produces the builder/dev VM image successfully without any `crates.io` fetch.

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md specs/adrs/046-builder-vm-via-libkrun.md \
        crates/mvm-cli/tests/dev_up_smoke.rs
git commit -m "feat(plan-115): CLAUDE.md zig dep + ADR-046 footer + E2E smoke"
```

- [ ] **Step 6: Open PR**

```bash
git push -u origin worktree-plan-115-single-builder-dev-image
gh pr create --title "Plan 115 — Single builder/dev image with mvmctl-embedded Linux binaries (ADR-064)" \
  --body "$(cat <<'EOF'
## Summary
- Collapses nix/images/builder/ → nix/images/builder-vm/ with two attrs (default headless / dev interactive)
- Embeds mvm-builder-init + mvm-egress-proxy in mvmctl via build.rs + include_bytes!
- Deletes the dev-image Stage 0 bootstrap chicken-and-egg
- ADR-064 §Decision applied end-to-end

## Test plan
- [x] cargo test --workspace (all green)
- [x] cargo xtask check-mvm-host-binaries-sync
- [x] MVM_E2E_SMOKE=1 cargo test --test dev_up_smoke (manual; see PR description)
- [x] `mvmctl dev up` from a clean cache succeeds without crates.io reachability

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Verification (end-to-end after all tasks land)

- [ ] `cargo test --workspace --no-fail-fast` — all green.
- [ ] `cargo xtask check-mvm-host-binaries-sync` — manifests agree.
- [ ] `mvmctl doctor` — reports pinned + installed zig/cargo-zigbuild versions; no missing-tool warnings on a properly-set-up host.
- [ ] `mvmctl inspect host-bins` — reports the embedded SHA-256 of each binary (this command is planned for a follow-up; not blocking this plan).
- [ ] From a clean `~/.cache/mvm/`: `mvmctl dev up` boots the dev VM with `/sbin/mvm-builder-init` + `/sbin/mvm-egress-proxy` matching the embedded SHAs.
- [ ] `grep -r "rustPlatform.buildRustPackage" nix/` — three matches in the §Principle inventory (`runtime-overlay`, `mvm-guest-agent`, `mvm-addon-dns`); zero matches in the builder-vm path.
- [ ] No `crates.io` reachability required at any point during Stage 0's `nix build`.

## Self-review notes

- **Spec coverage:** ADR-064 §Decision items 1, 2, and 3 all land in Tasks 6, 4+5+7, and 6 respectively. §Component-level diff "New" entries are covered in T2, T3, T4, T5. "Modified" entries are covered in T1, T6, T7, T9. "Deleted" entries are covered in T8.
- **Type consistency:** `HostBinary { name, install_path, mode }` defined in T2, referenced by T4 (`EMBEDDED`), T5 (`extract`), and T7 (`BuilderMounts.host_bin_dir`). Method names: `ensure_extracted` (T5) reused in T7. `MVM_HOST_BIN_DIR` env var named consistently in T6 (flake side) and T7 (host side).
- **Placeholders:** none — every step has either exact code, exact commands, or exact file paths.
- **Future work explicitly out of scope** (don't bundle these in this plan): adding `mvmctl inspect host-bins`, runtime-overlay conversion, mvm-guest-agent / mvm-addon-dns conversion, ADR-064's §Future directions items. Each of those is its own plan.

---

## Plan complete

After all tasks land, the merge candidate is:
- The dev-image vs builder-VM split is dissolved.
- `mvmctl` is a single-binary unit of distribution; end-users download one file.
- `fetchCrate` is off the critical path of `mvmctl dev up`.
- The §Principle inventory in ADR-064 drops 3 entries (sites 1-3 of 6).
