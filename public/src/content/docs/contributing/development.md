---
title: Development Guide
description: Getting started as a contributor to mvm.
---

## Prerequisites

- **Rust 1.85+** (Edition 2024) — install via [rustup](https://rustup.rs)
- **macOS or Linux** — macOS for development via Apple Container (26+) or libkrun (pre-26); Linux for native `/dev/kvm`
- **Nix** (optional) — only needed for building microVM images

Run the bootstrap script on a fresh machine:

```bash
./ops/bootstrap/dev-setup.sh
```

## Building and Running

```bash
# Build
just build

# Run CLI
just run -- --help

# Dev mode (auto-bootstraps the dev VM + Firecracker)
just run -- dev

# Release build (stripped, LTO)
just release-build
```

## Testing

```bash
# Run all tests with nextest
just test

# Test a single crate
just test-crate mvm-core

# Run tests matching a filter
just test-filter "test_snapshot"

# Full CI gate (lint + test)
just ci
```

### Test Organization

| Location | Type | What it tests |
|----------|------|---------------|
| `crates/*/src/**/*.rs` (`#[cfg(test)]`) | Unit tests | Internal functions within the crate |
| `crates/*/tests/*.rs` | Integration tests | Public API of each crate |
| `tests/cli.rs` | Binary tests | CLI arg parsing, help output, subcommand structure |

### Testing Conventions

- Unit tests go in `#[cfg(test)] mod tests {}` at the bottom of the source file
- CLI binary tests go in root `tests/cli.rs`
- Use `#[serde(default)]` when adding fields to structs used in test fixtures

## Linting and Formatting

```bash
just fmt          # Format all code
just clippy       # Lint (zero warnings required)
just lint         # Both format check + clippy
```

### Style Rules

- **Edition 2024**: `use` statements don't need `extern crate`; let chains supported
- **No `clippy::too_many_arguments`**: never suppress this lint — refactor into a params struct
- **No `format!()` in `format!()` named args**: extract to a variable first
- **Cross-crate imports**: always use `mvm_core::`, `mvm_runtime::`, etc.

## Architecture Principles

### Multi-Backend

mvmctl's supported local microVM hosts are native Linux with `/dev/kvm` and macOS Apple Silicon. Firecracker is the Linux baseline; Apple Container and libkrun-backed components cover Apple Silicon macOS. Docker remains a Tier 3 convenience fallback, not a microVM isolation boundary. WSL2 nested KVM and a Hyper-V managed Linux builder are future backend work.

### Host vs. VM

All Linux build operations run inside the builder VM on macOS:

```rust
// On Linux this runs directly on the host; on supported macOS hosts it
// routes into the libkrun-backed builder VM.
mvm_runtime::shell::run_in_vm("ip link add br-tenant-1 type bridge")?;
```

On native Linux, `run_in_vm` executes directly on the host. On supported macOS Apple Silicon hosts, it delegates into the builder VM.

### Key Patterns

- **Idempotent operations**: every setup step checks if already done before acting
- **Config drive for metadata**: instance metadata delivered via read-only ext4 disk
- **Vsock over SSH**: guest communication uses vsock, not sshd (all backends)
- **Same rootfs everywhere**: Nix-built ext4 images work on all backends

### Adding New Types

When adding fields to structs in serialized state:

1. Add `#[serde(default)]` to the new field for backward compatibility
2. `cargo test --workspace` to find all broken test constructions
3. Fix each one
4. Add a unit test for the new behavior

## Developer Workflow Commands

### Two-track build vs. release (Plan 77 W7)

mvm separates the **dev track** (contributor source checkout, builds from in-tree flakes via libkrun on every `mvmctl dev up`) from the **release track** (installed-binary users, downloads cosign-verified prebuilts). The two tracks share no artifact path:

- A contributor `mvmctl dev up` **never** downloads a release prebuilt — every microVM image you boot is built from your local `nix/images/{builder,builder-vm}/flake.nix`. Edits to either flake show up on the next `mvmctl dev up`.
- An installed-binary `mvmctl dev up` **never** invokes host Nix — it pulls + cosign-verifies the release-pipeline-cut dev image.

The one place host Nix is involved on the dev track is the **one-time contributor bootstrap** that produces the source-checkout vendored seed image. Run this once per host before your first `mvmctl dev up`:

```bash
# One time only: produce the vendored dev-image seed for the source-checkout
# Stage 0 bootstrap. Needs nix on PATH; run on Linux or on macOS with a
# linux-builder configured. Output lands in nix/images/dev-prebuilt/<arch>/
# (gitignored; per-host artifact).
cargo xtask build-dev-image --arch aarch64   # or --arch x86_64
```

After that, every `mvmctl dev up` rebuilds the dev image (and the builder VM, via Stage 0) from your local flakes via libkrun. The vendored seed is only consulted as the Stage 0 Linux+Nix environment; the *new* dev image goes into `~/.mvm/dev/current/` and subsequent runs use it.

If your `~/.mvm/dev/current/` ever becomes contract-stale (e.g. the dev-image flake adds a new PID-1 binary expectation), Stage 0's W5 seed-contract check (PR #316) refuses to boot it and points you back at `cargo xtask build-dev-image` to refresh the vendored slot. The remediation is structured and the audit log records the failure as `Stage0Failed stage=preflight reason=seed_contract_*` for downstream tooling. See `specs/plans/77-stage0-bootstrap-via-dev-image.md` for the full design.

### Day-to-day commands

```bash
# First-time setup (installs deps, creates the dev VM, default network)
just run -- init

# Image catalog — browse and build images from Nix templates
just run -- image list              # browse bundled catalog
just run -- image search http       # search by name/tag
just run -- image fetch minimal     # build from catalog entry

# Named dev networks
just run -- network create isolated # create a named network
just run -- network list            # list all networks
just run -- up --flake . --network isolated  # attach VM to a network

# Interactive console (PTY-over-vsock, no SSH)
just run -- console myvm            # interactive shell
just run -- console myvm --command "uname -a"  # one-shot exec

# Cache and diagnostics
just run -- cache info              # show cache dir and disk usage
just run -- cache prune             # clean stale temp files
just run -- security status         # security posture evaluation
just run -- doctor                  # dependency checks
```

### Console Access

microVMs have no SSH. Interactive access is via `mvmctl console` which uses PTY-over-vsock:
- Authenticated via the existing Ed25519 vsock protocol
- Dev-mode only (`access.console` must be `true` in the guest security policy)
- Single session per VM, 15-minute idle timeout
- Supports both Firecracker and Apple Container backends

### XDG Directory Layout

Dev tool state uses XDG-compliant paths (override with `MVM_CACHE_DIR`, `MVM_CONFIG_DIR`, etc.):

| Path | Purpose |
|------|---------|
| `~/.cache/mvm/` | Build artifacts, images, VM runtime state |
| `~/.config/mvm/` | User config (`config.toml`) |
| `~/.local/state/mvm/` | Logs, audit trail |
| `~/.local/share/mvm/` | Templates, network definitions, VM name registry |

Legacy `~/.mvm/` paths are auto-detected as fallback.

## CI/CD

| Workflow | Trigger | What it does |
|----------|---------|--------------|
| `ci.yml` | Push to main/feat/*, PRs | check, fmt, clippy, test (macOS + Linux), audit |
| `release.yml` | Tags matching `v*` | Builds 4 platform binaries, creates GitHub Release |
| `publish-crates.yml` | Release published | Publishes to crates.io in dependency order |
| `pages.yml` | Push to main | Deploys docs to GitHub Pages |

## Release Process

```bash
# 1. Bump version in root Cargo.toml [workspace.package]
# 2. Update CHANGELOG.md
# 3. Commit and tag
git add -A && git commit -m "release: v0.3.0"
git tag v0.3.0

# 4. Push (triggers release.yml)
git push && git push --tags
```

The deploy guard (`scripts/deploy-guard.sh`) validates the tag matches the workspace version before publishing.
