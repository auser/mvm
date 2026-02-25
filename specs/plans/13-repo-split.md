# Plan: Split mvm into mvm (dev tool) + mvmd (orchestrator)

## Context

The mvm monorepo currently contains both a simple development tool (single-VM lifecycle, Nix builds, templates) and a complex multi-tenant orchestration system (agents, coordinators, tenant/pool/instance management, security hardening). We want to split these into two sibling repos so mvm stays simple and approachable, while the orchestration complexity lives separately in `mvmd`.

## Design Decisions

- **New repo name**: `mvmd` (daemon-style, binary is `mvmd`)
- **mvm-core stays whole**: orchestration types are pure serde structs (~1200 lines), zero runtime cost, avoids third shared-types crate
- **Templates stay in mvm**: `mvm template build` uses `dev_build()` path (local Nix in Lima, no FC builder VMs)
- **Approach**: branch-first — trim mvm on a branch, verify it works, then extract removed code into mvmd repo
- **Facade dependency**: mvmd depends on the single `mvm` facade crate (which re-exports `mvm::core`, `mvm::runtime`, `mvm::build`, `mvm::guest`) — no need to list each sub-crate individually
- **Security**: dev mode does NOT use any security modules (jailer, seccomp, cgroups, etc.) — these are exclusively in the multi-tenant `instance/lifecycle.rs` path, so removing them from mvm is safe
- **Sleep/wake preserved in mvmd**: all sleep/wake functionality (sleep policy, instance state transitions, snapshot save/restore, coordinator wake manager) moves to mvmd

## What Stays in mvm (simplified dev tool)

### CLI Commands (keep)
`bootstrap`, `setup`, `dev`, `start`, `stop`, `ssh`, `ssh-config`, `shell`, `sync`, `status`, `destroy`, `upgrade`, `doctor`, `build`, `run`, `template`, `completions`

### CLI Commands (remove to mvmd)
`tenant`, `pool`, `instance`, `agent`, `coordinator`, `dev-cluster`, `net`, `node`, `events`, `add`, `new`, `deploy`, `connect`

### Crate-by-crate breakdown

**mvm-core** — keep entire crate unchanged
- All modules stay (including orchestration types like `tenant`, `agent`, `protocol`, `routing`, `audit`, `node`, `idle_metrics`, `observability`)
- One change: split `BuildEnvironment` trait into `ShellEnvironment` (base) + `BuildEnvironment` (extends it)

**mvm-build** — keep entire crate unchanged
- `dev_build.rs` uses `ShellEnvironment` trait (dev path)
- `build.rs`/`orchestrator.rs` use `BuildEnvironment` trait (fleet path, used by mvmd)

**mvm-guest** — keep entire crate unchanged
- Used by mvm-build for vsock builder agent protocol

**mvm-runtime** — trim heavily
- **Keep:**
  - `shell.rs`, `shell_mock.rs` — command execution
  - `config.rs` — constants, Lima config, render helpers
  - `ui.rs` — colored output, spinners
  - `build_env.rs` — replace `RuntimeBuildEnv` with simpler `DevShellEnv` (only implements `ShellEnvironment`)
  - `vm/firecracker.rs` — FC binary install, asset download
  - `vm/lima.rs` — Lima VM lifecycle
  - `vm/lima_state.rs` — Lima state parsing
  - `vm/microvm.rs` — single-VM start/stop/ssh
  - `vm/network.rs` — dev-mode TAP/NAT
  - `vm/image.rs` — Mvmfile.toml build pipeline
  - `vm/template/` — template lifecycle + registry (for `mvm template` commands)
- **Remove:**
  - `vm/bridge.rs` — per-tenant bridge management
  - `vm/disk_manager.rs` — shared disk management
  - `vm/instance/` — entire directory (lifecycle, fc_config, net, disk, snapshot, health, parallel)
  - `vm/pool/` — entire directory (lifecycle, artifacts)
  - `vm/tenant/` — entire directory (lifecycle, quota, secrets)
  - `hostd/` — privilege separation daemon (server)
  - `security/` — entire directory (jailer, cgroups, seccomp, audit, encryption, keystore, signing, snapshot_crypto, attestation, certs, metadata)
  - `sleep/` — sleep metrics
  - `worker/` — guest worker hooks
  - `bin/mvm-hostd.rs` — hostd binary

**mvm-cli** — trim to dev commands only
- **Keep:** `commands.rs` (trimmed), `bootstrap.rs`, `doctor.rs`, `ui.rs`, `upgrade.rs`, `logging.rs`, `output.rs`, `template_cmd.rs`
- **Remove:** `dev_cluster.rs`, `display.rs`, `http.rs`
- **Remove deps:** mvm-agent, mvm-coordinator

**Remove entirely from workspace:**
- `mvm-agent` moves to mvmd
- `mvm-coordinator` moves to mvmd

## What Goes in mvmd (NEW separate git repository)

Two completely separate git repos side-by-side:
```
/Users/auser/work/personal/microvm/kv/
  mvm/       ← existing repo (trimmed to dev tool)
  mvmd/      ← NEW repo (orchestration daemon)
```

### mvmd repo structure
```
mvmd/                          ← separate git repo
  .git/
  Cargo.toml                   (workspace root)
  src/lib.rs                   (facade re-exports)
  src/main.rs                  (entry: mvmd_cli::run())
  crates/
    mvmd-runtime/              (orchestration modules from mvm-runtime)
    mvmd-agent/                (was mvm-agent)
    mvmd-coordinator/          (was mvm-coordinator)
    mvmd-cli/                  (orchestration CLI commands)
```

### Single facade dependency
mvmd depends on the `mvm` facade crate, which re-exports all sub-crates:
```toml
[dependencies]
mvm = { git = "https://github.com/auser/mvm" }
```
Then in mvmd code: `use mvm::core::*`, `use mvm::runtime::*`, `use mvm::build::*`, `use mvm::guest::*`

### mvmd-runtime contents (from mvm-runtime)
- `vm/bridge.rs`, `vm/disk_manager.rs`
- `vm/instance/` — lifecycle, fc_config, net, disk, **snapshot** (save/restore for sleep/wake), health, parallel
- `vm/pool/` — lifecycle, artifacts
- `vm/tenant/` — lifecycle, quota, secrets
- `hostd/` — privilege separation (server + client)
- `security/` — all 12 modules (jailer, seccomp, cgroups, audit, encryption, keystore, signing, snapshot_crypto, attestation, certs, metadata)
- `sleep/` — **sleep policy, minimum runtime enforcement, metrics**
- `worker/` — guest worker hooks, vsock agent client
- `build_env.rs` — full `RuntimeBuildEnv` implementing `BuildEnvironment`
- `bin/mvm-hostd.rs`

### mvmd-coordinator sleep/wake (from mvm-coordinator)
- `wake.rs` — on-demand wake manager, wake coalescing
- `idle.rs` — idle timeout tracking, connection lifecycle

### Instance state machine (preserved from mvm-core)
All sleep/wake state transitions stay in `mvm-core::instance` (which mvmd accesses via the facade):
`Running → Warm → Sleeping → (wake) → Running`

### mvmd-cli commands
`tenant`, `pool`, `instance`, `agent`, `coordinator`, `dev-cluster`, `net`, `node`, `events`, `add`, `new`, `deploy`, `connect`

## Implementation Steps

### Step 0: Save plan to specs/plans/
Copy this plan to `specs/plans/13-repo-split.md` in the mvm repo for project records.

### Step 1: Split BuildEnvironment trait
**File:** `crates/mvm-core/src/build_env.rs`

Split into two traits:
```rust
pub trait ShellEnvironment: Send + Sync {
    fn shell_exec(&self, script: &str) -> Result<std::process::Output>;
    fn shell_exec_stdout(&self, script: &str) -> Result<String>;
    fn shell_exec_visible(&self, script: &str) -> Result<std::process::Output>;
    fn log_info(&self, msg: &str);
    fn log_success(&self, msg: &str);
    fn log_warn(&self, msg: &str);
}

pub trait BuildEnvironment: ShellEnvironment {
    fn load_pool_spec(...) -> Result<PoolSpec>;
    fn load_tenant_config(...) -> Result<TenantConfig>;
    fn ensure_bridge(...) -> Result<()>;
    fn setup_tap(...) -> Result<...>;
    fn teardown_tap(...) -> Result<()>;
    fn record_revision(...) -> Result<()>;
}
```

Update `dev_build()` in mvm-build to accept `&dyn ShellEnvironment`.
Keep `pool_build()`/`orchestrator` using `&dyn BuildEnvironment`.
Update `RuntimeBuildEnv` in mvm-runtime to impl both traits.

### Step 2: Verify trait split
Run `cargo build && cargo test` — all 374 tests pass. Pure refactor, no functional change.

### Step 3: Create trim branch
```
git checkout -b refactor/simplify-mvm
```

### Step 4: Remove orchestration crates from workspace
- Remove `mvm-agent` and `mvm-coordinator` from workspace members in root `Cargo.toml`
- Remove their dependencies from mvm-cli's `Cargo.toml`
- Don't delete the crate directories yet (archive/move later)

### Step 5: Trim mvm-cli commands
In `crates/mvm-cli/src/commands.rs`:
- Remove command variants: `Tenant`, `Pool`, `Instance`, `Agent`, `Coordinator`, `DevCluster`, `Net`, `Node`, `Events`, `Add`, `New`, `Deploy`, `Connect`
- Remove corresponding subcommand enums and handler functions
- Remove imports: `bridge`, `pool`, `tenant` from `mvm_runtime::vm`
- Delete `dev_cluster.rs`, `display.rs`, `http.rs`
- Remove their `pub mod` declarations from `crates/mvm-cli/src/lib.rs`

### Step 6: Trim mvm-runtime
In `crates/mvm-runtime/src/vm/mod.rs`:
- Remove: `bridge`, `disk_manager`, `instance`, `pool`, `tenant` module declarations

In `crates/mvm-runtime/src/lib.rs`:
- Remove: `hostd`, `security`, `sleep`, `worker` module declarations

Delete files/directories:
- `vm/bridge.rs`, `vm/disk_manager.rs`
- `vm/instance/`, `vm/pool/`, `vm/tenant/`
- `hostd/`, `security/`, `sleep/`, `worker/`

Replace `RuntimeBuildEnv` in `build_env.rs` with `DevShellEnv` (only impl `ShellEnvironment`).
Remove `mvm-hostd` binary from mvm-runtime `Cargo.toml`.
Prune now-unused deps (ed25519-dalek, aes-gcm, zeroize, quinn, rcgen, rustls, etc.).

### Step 7: Trim root facade
In `src/lib.rs`:
- Remove `pub use mvm_agent as agent` and `pub use mvm_coordinator as coordinator`
- Keep `core`, `runtime`, `build`, `guest` re-exports (mvmd will depend on this facade)

In root `Cargo.toml`:
- Remove mvm-agent and mvm-coordinator from `[dependencies]`
- Remove unused workspace dependencies
- Keep the `[lib]` section so `mvm` remains a usable facade crate for mvmd

### Step 8: Update template build to use dev_build path
In `crates/mvm-cli/src/template_cmd.rs`:
- Ensure `mvm template build` calls `dev_build()` with `DevShellEnv` rather than `pool_build()` with `RuntimeBuildEnv`

### Step 9: Verify simplified mvm
```bash
cargo build
cargo clippy -- -D warnings
cargo test
```
Test manually: `mvm build`, `mvm run --flake .`, `mvm template create/build/list`

### Step 10: Create mvmd repo scaffold
Initialize at `../mvmd/`:
```
mvmd/
  Cargo.toml              (workspace root + mvmd facade)
  src/lib.rs              (facade re-exports)
  src/main.rs             (entry: mvmd_cli::run())
  crates/
    mvmd-runtime/
      Cargo.toml
      src/lib.rs
      src/bin/mvm-hostd.rs
    mvmd-agent/
      Cargo.toml
      src/lib.rs
    mvmd-coordinator/
      Cargo.toml
      src/lib.rs
    mvmd-cli/
      Cargo.toml
      src/lib.rs
```

Root `Cargo.toml`:
```toml
[workspace]
members = ["crates/mvmd-runtime", "crates/mvmd-agent", "crates/mvmd-coordinator", "crates/mvmd-cli"]
resolver = "3"

[workspace.package]
version = "0.1.0"
edition = "2024"

[workspace.dependencies]
mvm = { git = "https://github.com/auser/mvm" }
# ...same shared deps as mvm workspace (tokio, serde, etc.)

[package]
name = "mvmd"

[dependencies]
mvmd-cli = { path = "crates/mvmd-cli" }
mvm.workspace = true
mimalloc.workspace = true
```

### Step 11: Move orchestration code into mvmd crates

**mvmd-runtime** (from mvm-runtime removed modules):
- Copy `vm/bridge.rs`, `vm/disk_manager.rs` → `crates/mvmd-runtime/src/vm/`
- Copy `vm/instance/` directory → `crates/mvmd-runtime/src/vm/instance/`
- Copy `vm/pool/` directory → `crates/mvmd-runtime/src/vm/pool/`
- Copy `vm/tenant/` directory → `crates/mvmd-runtime/src/vm/tenant/`
- Copy `hostd/` directory → `crates/mvmd-runtime/src/hostd/`
- Copy `security/` directory → `crates/mvmd-runtime/src/security/`
- Copy `sleep/` directory → `crates/mvmd-runtime/src/sleep/`
- Copy `worker/` directory → `crates/mvmd-runtime/src/worker/`
- Copy `build_env.rs` (full RuntimeBuildEnv) → `crates/mvmd-runtime/src/build_env.rs`
- Copy `bin/mvm-hostd.rs` → `crates/mvmd-runtime/src/bin/mvm-hostd.rs`
- Cargo.toml depends on: `mvm` (workspace, facade)

**mvmd-agent** (from crates/mvm-agent):
- Copy entire `crates/mvm-agent/src/` → `crates/mvmd-agent/src/`
- Cargo.toml depends on: `mvm` (workspace), `mvmd-runtime`

**mvmd-coordinator** (from crates/mvm-coordinator):
- Copy entire `crates/mvm-coordinator/src/` → `crates/mvmd-coordinator/src/`
- Cargo.toml depends on: `mvm` (workspace), `mvmd-runtime`

**mvmd-cli** (from mvm-cli removed commands):
- Copy orchestration command handlers and subcommand enums from `commands.rs`
- Copy `dev_cluster.rs`, `display.rs`, `http.rs`
- Create new `commands.rs` with orchestration-only Commands enum
- Cargo.toml depends on: `mvm` (workspace), `mvmd-runtime`, `mvmd-agent`, `mvmd-coordinator`

### Step 12: Rewrite imports in mvmd
All moved code needs import path updates:
- `use mvm_core::` → `use mvm::core::`
- `use mvm_runtime::shell` → `use mvm::runtime::shell`
- `use mvm_runtime::config` → `use mvm::runtime::config`
- `use mvm_runtime::ui` → `use mvm::runtime::ui`
- `use mvm_build::` → `use mvm::build::`
- `use mvm_guest::` → `use mvm::guest::`
- `use crate::` stays for intra-crate refs within mvmd-runtime
- Cross-mvmd-crate refs: `use mvmd_runtime::`, `use mvmd_agent::`, etc.

### Step 13: Verify mvmd compiles
```bash
cd ../mvmd && cargo build
cargo clippy -- -D warnings
cargo test
```

### Step 14: Verify both repos end-to-end
1. mvm repo: `cargo build && cargo test && cargo clippy -- -D warnings`
2. mvmd repo: `cargo build && cargo test && cargo clippy -- -D warnings`
3. Manual: `mvm dev`, `mvm build`, `mvm start/stop` (dev tool)
4. Manual: `mvmd tenant create`, `mvmd pool build`, `mvmd agent serve` (orchestration)

## Verification

1. **mvm repo (post-trim):**
   - `cargo build` compiles cleanly
   - `cargo clippy -- -D warnings` — 0 warnings
   - `cargo test` — all remaining tests pass
   - Dev workflow: `mvm dev`, `mvm build`, `mvm start/stop/ssh`, `mvm template create/build/list`

2. **mvmd repo (new):**
   - `cargo build` compiles with facade dep on mvm
   - `cargo clippy -- -D warnings` — 0 warnings
   - `cargo test` — all orchestration tests pass
   - Fleet workflow: `mvmd tenant create`, `mvmd pool build`, `mvmd agent serve`
   - Sleep/wake: instance state transitions, snapshot save/restore, wake manager

## Risks

1. **BuildEnvironment trait split** — touches call sites in mvm-build. Mitigated by doing it first as a pure refactor (Steps 1-2) before any removals.
2. **template_cmd.rs** may deeply reference orchestration build paths — needs careful inspection during Step 8.
3. **Cross-repo version drift** — mitigated by `#[serde(default)]` on new fields (existing practice) and pinned git refs in mvmd.
4. **display.rs** may have shared formatters — inspect before deleting; extract any dev-mode display helpers if needed.
5. **mvm-runtime Cargo.toml cleanup** — many deps become unused after removing security/hostd. Must carefully prune.
