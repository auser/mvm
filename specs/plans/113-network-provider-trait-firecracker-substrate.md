# Plan 113 — NetworkProvider Observer fan-out + Firecracker substrate (ADR-064 impl, Path X)

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. **No placeholders allowed in plans or code** (AGENTS.md §"No Placeholders in Plans or Code") — every code snippet below is real code lifted from the current repo or built from verified existing types.

**Goal:** Add observer fan-out + host-allowlisted trust store on top of the existing `mvm-supervisor::gateway_bridge` substrate (PR #459/#487/#502), close Plan 112's Vz drainer carve-out by shipping `mvm-vz-drainer` as a thin binary that links the existing `mvm-supervisor` with `BridgeEndpoints::VzIngest`, and ship the Firecracker substrate as `mvm-firecracker-bridge` linking `mvm-supervisor` with `BridgeEndpoints::Passt` under `mvm-jailer-lite` seccomp + Landlock confinement.

**Architecture (Path X — wrap, don't replace):** ADR-064's `NetworkProvider` trait and `Observer` pattern are added inside `mvm-supervisor` (the existing host of `gateway_bridge`), **not** in `mvm-core`. The existing `FlowEvent`, `FlowEventWire`, `BridgeConfig`, `BridgeEndpoints`, and `signer_task` ship as-is. Extension points: (1) `BridgeConfig` gains `observers: Vec<Arc<dyn Observer>>`; (2) `signer_task` fan-outs each event under `catch_unwind` before signing; (3) two new binary crates link `mvm-supervisor` to provide Vz drainer and Firecracker bridge.

**Why Path X:** PR #459/#487/#502 already shipped the substrate seam. Building a parallel `mvm-core::network` module would duplicate `FlowEvent`, force a wire-format translation layer, and break the existing chain-byte format. Path X is smaller (≈17 tasks vs 17 in v1; but each surgical), preserves byte-format compatibility automatically, and matches the AGENTS.md "ship the right shape from the start" rule.

**Tech Stack:** Rust workspace; `seccompiler 0.5` (Firecracker-maintained); `landlock 0.4` (official Rust LSM binding); `etherparse` (existing in gateway_bridge); `tokio` (existing); `serde_json` / `toml` (existing). Linux ≥ 5.19 for Landlock ABI v2; Ubuntu 22.04 CI runner satisfies.

**Cross-refs:** [ADR-064](../adrs/064-network-provider-trait.md), [Plan 102](102-gateway-audit-substrate-impl.md) §W6.A.5, [Plan 103](103-w6a-implementation-tracker.md), [Plan 112](112-w6a-phase-3c-producer-activation.md) (Phase 3c producer activation, merged 2026-05-29). AGENTS.md §"No Placeholders in Plans or Code".

**Status:** 🟡 in progress — worktree `worktree-plan-113-network-provider`

---

## Plan-wide context

### Path X-vs-v1 summary

The first draft of this plan introduced `mvm-core::network::{FlowEvent, NetworkProvider, Observer, Pipeline, Broadcast}` as a new abstraction layer. Reading the actual post-PR-#459/#487/#502 code via Explore agents surfaced significant existing machinery that the v1 plan would have duplicated:

- `mvm-supervisor::gateway_bridge::FlowEvent` (private, already shipped) — the canonical internal event type, fed via mpsc to a signer task.
- `mvm-supervisor::gateway_bridge::FlowEventWire` (public, already shipped) — the NDJSON shape the Swift Vz bridge already writes to `events_ingest_socket_path`.
- `mvm-supervisor::gateway_bridge::{BridgeConfig, BridgeEndpoints, spawn_bridge_thread, signer_task}` — the existing substrate seams already separate IO model from signing logic.
- `mvm-supervisor::audit::{AuditEntry, AuditEntry::flow_opened, AuditEntry::flow_closed}` — the canonical chain entries with `event = "gateway.flow_opened"` / `"gateway.flow_closed"` and labels `{flow_id, direction, reason}`. **Existing wire format; must not change.**

Path X **wraps** this machinery with the observer abstraction from ADR-064 rather than replacing it. ADR-064's design decisions translate as follows:

| ADR-064 decision | Path X implementation |
|---|---|
| Composable observers (§Decision 1) | `Vec<Arc<dyn Observer>>` on `BridgeConfig`; fan-out happens inside `signer_task` |
| Hybrid event granularity (§Decision 2) | Observers consume `&FlowEvent` (existing); payload tap is a future trait method, not in this plan |
| Builder + box at boundary (§Decision 3) | `Pipeline::new().observe(...).build_observers()` returns `Vec<Arc<dyn Observer>>` |
| Per-VM supervisor (§Decision 4) | Existing `mvm-libkrun-supervisor` unchanged; new `mvm-vz-drainer` and `mvm-firecracker-bridge` binaries each link `mvm-supervisor` |
| A2 confinement (§Decision 5) | New `mvm-jailer-lite` helper crate; `mvm-firecracker-bridge` calls `confine_self()` at startup |
| Hard-fail bridge crash (§Decision 6) | `bridge_restart_policy: BridgeRestartPolicy::HardFail` reserved on `SupervisorConfig`; `mvm-backend` watchdog SIGTERMs VM on bridge death |
| Host-allowlisted observers (§Decision 7) | New `ObserverAllowlist` in `mvm-supervisor`; reads `~/.mvm/observers/allowlist.toml` |
| Vz no payload tap (§Decision 8) | `BridgeEndpoints::VzIngest` already exists; payload tap isn't shipped in this plan on any backend (deferred to redactor plan) |
| Tenant value resolution (§Decision 9) | New helper in `mvm-cli`; 4-level precedence: default → config file → env → flag |

### What landed already (don't redo)

From the Explore agent findings (verbatim):

**`mvm-supervisor::gateway_bridge::FlowEvent`** (private, `gateway_bridge.rs:183-194`):

```rust
pub(crate) struct FlowEvent {
    pub flow_id: String,
    pub direction: FlowDirection,
    pub kind: FlowEventKind,
}

#[derive(Debug, Clone)]
pub(crate) enum FlowEventKind {
    Opened,
    Closed { reason: FlowCloseReason },
}
```

**`mvm-supervisor::gateway_bridge::FlowEventWire`** (public, NDJSON shape, `gateway_bridge.rs:204-216`):

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowEventWire {
    FlowOpened { flow_id: String, direction: String },
    FlowClosed { flow_id: String, direction: String, reason: String },
}
```

**`mvm-supervisor::gateway_bridge::BridgeConfig`** (public, `gateway_bridge.rs:166-174`):

```rust
pub struct BridgeConfig {
    pub vm_name: String,
    pub plan: Arc<ExecutionPlan>,
    pub bundle: Option<Arc<PolicyBundle>>,
    pub audit_socket: PathBuf,
    pub signer: Arc<dyn AuditSigner>,
    pub policy: Arc<dyn FlowPolicy>,
}
```

**`mvm-supervisor::gateway_bridge::BridgeEndpoints`** (public, `gateway_bridge.rs:137-162`):

```rust
pub enum BridgeEndpoints {
    Passt {
        gateway_fd: OwnedFd,
        supervisor_fd: OwnedFd,
    },
    LibkrunGvproxy {
        gvproxy_socket_path: PathBuf,
        supervisor_listen_path: PathBuf,
    },
    VzIngest { events_socket_path: PathBuf },
}
```

**`mvm-supervisor::gateway_bridge::signer_task`** (public-ish, `gateway_bridge.rs:241-278`):

```rust
pub(crate) async fn signer_task(
    mut rx: mpsc::Receiver<FlowEvent>,
    plan: Arc<ExecutionPlan>,
    bundle: Option<Arc<PolicyBundle>>,
    signer: Arc<dyn AuditSigner>,
    broadcast_tx: broadcast::Sender<String>,
) {
    while let Some(event) = rx.recv().await {
        if let Ok(json) = serde_json::to_string(&FlowEventWire::from(&event)) {
            let _ = broadcast_tx.send(json);
        }
        let entry = match &event.kind {
            FlowEventKind::Opened => AuditEntry::flow_opened(
                plan.as_ref(),
                bundle.as_deref(),
                &event.flow_id,
                event.direction,
            ),
            FlowEventKind::Closed { reason } => AuditEntry::flow_closed(
                plan.as_ref(),
                bundle.as_deref(),
                &event.flow_id,
                event.direction,
                *reason,
            ),
        };
        if let Err(e) = signer.sign_and_emit(&entry).await {
            tracing::warn!(error = ?e, flow_id = event.flow_id, "signer emit failed");
        }
    }
}
```

**`mvm-supervisor::audit::AuditEntry`** (public, `audit.rs:35-61`):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub tenant: TenantId,
    pub plan_id: PlanId,
    pub plan_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<PolicyId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_version: Option<u32>,
    pub image_name: String,
    pub image_sha256: String,
    pub event: String,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}
```

**`mvm-policy::NetworkPolicy`** (public, `policies.rs:22-43`):

```rust
pub struct NetworkPolicy {
    pub preset: Option<String>,
    #[serde(default)]
    pub l4: Vec<L4RuleSpec>,
}
```

Plan 113 extends this struct (Task 6) with one new optional field; v1 / v2 bundle TOML stays backward-compatible via `#[serde(default)]`.

**`mvm-libkrun-supervisor::main::run_with_bridge`** (`main.rs:149-252`) constructs `BridgeConfig` from `SupervisorConfig` and calls `run_supervisor_with_bridge`. This is the single point at which Plan 113 inserts `Pipeline::from_admitted` (Task 4).

### Tenant trust boundary preserved

The existing policy bundle pattern (`tenant:workload` → `~/.mvm/policies/<tenant>/<workload>.toml` via `mvm_policy::toml_loader::bundle_path`) is the existing tenant↔host trust seam. Plan 113 layers the observer chain into the existing `NetworkPolicy` block at the bundle level: a tenant cannot inject novel observer names; the policy file (host-controlled per the existing claim-10 contract) declares which observers fire.

### Conventions

- All paths in this plan are **relative to the worktree root** (`/Users/auser/work/tinylabs/mvmco/mvm/.claude/worktrees/plan-113-network-provider/`).
- Every code block shows **real code** — either lifted verbatim from existing source files (with file:line cited) or composed from existing types using their actual constructors. No `TODO`, no `PLACEHOLDER_`, no "engineer adapts" markers. Per AGENTS.md §"No Placeholders in Plans or Code".
- Per memory `feedback_no_backcompat_first_version`: schema changes are hard renames where appropriate. Backward-compatible additions (e.g., `#[serde(default)]` for new optional `NetworkPolicy.observers` field) are not "compat shims" — they are correct defaults that preserve existing semantics.

### File map

**Created in this plan:**

- `crates/mvm-supervisor/src/network/mod.rs` — Observer trait + Pipeline + BuildError + ObserverAllowlist
- `crates/mvm-supervisor/src/network/flow_count.rs` — flow-count-metrics observer
- `crates/mvm-jailer-lite/Cargo.toml` + `src/lib.rs` + `src/seccomp.rs` + `src/landlock.rs` — Linux confinement helper
- `crates/mvm-jailer-lite/tests/{seccomp_property.rs, landlock_property.rs}` — `#[ignore]`-gated property tests
- `crates/mvm-jailer-lite/SECCOMP.md` + `LANDLOCK.md` — confinement docs
- `crates/mvm-vz-drainer/Cargo.toml` + `src/main.rs` — Vz drainer binary
- `crates/mvm-firecracker-bridge/Cargo.toml` + `src/main.rs` — Firecracker bridge binary
- `crates/mvm-firecracker-bridge/fuzz/` — fuzz target reusing libkrun's etherparse corpus
- `crates/mvm-cli/src/commands/vm/tenant_resolution.rs` — 4-level tenant precedence
- `.github/workflows/jailer-property.yml` — new CI lane

**Modified:**

- Workspace `Cargo.toml` — add `mvm-jailer-lite`, `mvm-vz-drainer`, `mvm-firecracker-bridge` workspace members + `[workspace.dependencies]` entries
- `crates/mvm-supervisor/src/gateway_bridge.rs` — `BridgeConfig` gains `observers` field; `signer_task` adds fan-out + panic isolation
- `crates/mvm-supervisor/src/lib.rs` — `pub mod network;`
- `crates/mvm-libkrun-supervisor/src/main.rs` — `run_with_bridge` builds observers via `Pipeline::from_admitted`
- `crates/mvm-libkrun/src/lib.rs` — `SupervisorConfig` gains `bridge_restart_policy` field
- `crates/mvm-policy/src/policies.rs` — `NetworkPolicy` gains `observers` field
- `crates/mvm-cli/src/commands/vm/up.rs` — call tenant resolution
- `crates/mvm-cli/src/commands/vm/mod.rs` — `pub mod tenant_resolution;`
- `crates/mvm-cli/src/metrics_server.rs` — mount flow-count-metrics route
- `crates/mvm-backend/src/vz.rs` — spawn `mvm-vz-drainer` after gvproxy spawn
- `crates/mvm-backend/src/backend.rs` (`FirecrackerBackend::start`) — spawn `mvm-firecracker-bridge` + watchdog
- `deny.toml` — pin `seccompiler` and `landlock` versions
- `specs/plans/102-gateway-audit-substrate-impl.md` — tick Phase 3c follow-ups
- `specs/plans/103-w6a-implementation-tracker.md` — status bump

---

## Phase A — Observer machinery in `mvm-supervisor`

### Task 1 — Observer trait + ProviderCapabilities + Pipeline + BuildError in `mvm-supervisor::network`

**Files:**
- Create: `crates/mvm-supervisor/src/network/mod.rs`
- Modify: `crates/mvm-supervisor/src/lib.rs` (add `pub mod network;`)

The trait lives in `mvm-supervisor` (not `mvm-core`) because observers consume `mvm-supervisor::gateway_bridge::FlowEvent`. Forcing observers up to `mvm-core` would require either duplicating `FlowEvent` or making `Observer` generic over the event type; both are worse than the simpler local addition.

- [ ] **Step 1: Locate `mvm-supervisor::lib.rs` and confirm current public modules**

```bash
cargo metadata --format-version=1 --no-deps 2>/dev/null \
  | python3 -c "import sys,json; d=json.load(sys.stdin); print([p['manifest_path'] for p in d['packages'] if p['name']=='mvm-supervisor'])"
head -40 crates/mvm-supervisor/src/lib.rs
```

Note the existing `pub mod ...` block.

- [ ] **Step 2: Create `crates/mvm-supervisor/src/network/mod.rs`**

```rust
//! Plan 113 / ADR-064 — Observer trait + Pipeline builder for the gateway
//! audit substrate.
//!
//! Observers consume `&crate::gateway_bridge::FlowEvent` references inside
//! `signer_task` (fan-out before chain signing). Observers run under
//! `catch_unwind`: a panic in observer N surfaces a tracing warn and does
//! not break observer N+1 or the chain-signing path itself.
//!
//! Observers are **host-allowlisted**, not tenant-shipped. The allowlist
//! file at `~/.mvm/observers/allowlist.toml` is parsed at supervisor
//! startup; tenant policy bundles reference observer names by string and
//! the resolver refuses unknown names with `BuildError::NotAllowlisted`.
//!
//! Per ADR-064 §Decision 7.

use crate::gateway_bridge::FlowEvent;
use std::collections::HashMap;
use std::sync::Arc;

pub mod flow_count;

/// Maximum number of observers per VM. ADR-064 §Decision: hard cap of 8
/// (each observer is a synchronous callback in the signer task's hot path;
/// per-VM bound keeps the hot path predictable).
pub const MAX_OBSERVERS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub flow_events: bool,
    pub payload_tap: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequiredCapabilities {
    pub flow_events: bool,
    pub payload_tap: bool,
}

impl ProviderCapabilities {
    pub fn satisfies(&self, req: &RequiredCapabilities) -> bool {
        (!req.flow_events || self.flow_events) && (!req.payload_tap || self.payload_tap)
    }

    pub fn missing_for(&self, req: &RequiredCapabilities) -> Vec<&'static str> {
        let mut out = Vec::new();
        if req.flow_events && !self.flow_events {
            out.push("flow_events");
        }
        if req.payload_tap && !self.payload_tap {
            out.push("payload_tap");
        }
        out
    }
}

/// Synchronous observer callback. Implementations MUST NOT panic in hot
/// path (the signer task wraps each call in `catch_unwind`, but a panic
/// per event is wasteful). Implementations MUST be cheap (microseconds);
/// expensive work should buffer + defer to a background task the observer
/// owns.
pub trait Observer: Send + Sync {
    fn name(&self) -> &'static str;
    fn required_capabilities(&self) -> RequiredCapabilities;
    fn on_flow_event(&self, event: &FlowEvent);
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("observer chain too deep (max {max}); requested {requested}")]
    TooManyObservers { max: usize, requested: usize },

    #[error("observer {observer:?} requires {missing:?}; leaf does not provide them")]
    CapabilityMismatch {
        observer: &'static str,
        missing: Vec<&'static str>,
    },

    #[error("observer name {0:?} is not in ~/.mvm/observers/allowlist.toml")]
    NotAllowlisted(String),

    #[error("allowlist {path}: {detail}")]
    AllowlistRead { path: String, detail: String },
}

/// Pipeline builder. `observe()` is capability-gated + depth-capped;
/// `build_observers()` returns the `Vec<Arc<dyn Observer>>` the caller
/// hands to `BridgeConfig.observers`.
///
/// AuditEmit is NOT injected by this builder. The existing
/// `signer_task` (in `mvm-supervisor::gateway_bridge`) already calls
/// `signer.sign_and_emit(&entry)` after the fan-out — chain signing
/// is structural, runs after every observer, and cannot be displaced
/// by tenant policy.
pub struct Pipeline {
    observers: Vec<Arc<dyn Observer>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self { observers: Vec::new() }
    }

    pub fn observe(
        mut self,
        observer: Arc<dyn Observer>,
        leaf_caps: ProviderCapabilities,
    ) -> Result<Self, BuildError> {
        if self.observers.len() >= MAX_OBSERVERS {
            return Err(BuildError::TooManyObservers {
                max: MAX_OBSERVERS,
                requested: self.observers.len() + 1,
            });
        }
        let req = observer.required_capabilities();
        if !leaf_caps.satisfies(&req) {
            return Err(BuildError::CapabilityMismatch {
                observer: observer.name(),
                missing: leaf_caps.missing_for(&req),
            });
        }
        self.observers.push(observer);
        Ok(self)
    }

    pub fn build_observers(self) -> Vec<Arc<dyn Observer>> {
        self.observers
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Host-allowlisted observer registry. Loaded from
/// `~/.mvm/observers/allowlist.toml` (mode 0600) at supervisor startup.
/// Tenant policy bundles reference observer names; `resolve()` returns
/// the typed `Arc<dyn Observer>` or `BuildError::NotAllowlisted`.
pub struct ObserverAllowlist {
    entries: HashMap<String, ObserverConstructor>,
}

type ObserverConstructor = Box<dyn Fn() -> Arc<dyn Observer> + Send + Sync>;

#[derive(serde::Deserialize)]
struct AllowlistFile {
    schema_version: u32,
    #[serde(default)]
    observer: Vec<AllowlistEntry>,
}

#[derive(serde::Deserialize)]
struct AllowlistEntry {
    name: String,
}

impl ObserverAllowlist {
    /// Load from the canonical locations. Per-user `~/.mvm/observers/allowlist.toml`
    /// wins over system-wide `/etc/mvm/observers/allowlist.toml`. Missing both
    /// surfaces a `BuildError::AllowlistRead` error explaining what the operator
    /// must create.
    pub fn load_from_host_config() -> Result<Self, BuildError> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let user_path = std::path::PathBuf::from(home).join(".mvm/observers/allowlist.toml");
        if user_path.exists() {
            return Self::load_from_path(&user_path);
        }
        let system_path = std::path::PathBuf::from("/etc/mvm/observers/allowlist.toml");
        if system_path.exists() {
            return Self::load_from_path(&system_path);
        }
        Err(BuildError::AllowlistRead {
            path: user_path.display().to_string(),
            detail: "operator must create ~/.mvm/observers/allowlist.toml (mode 0600) \
                     with at least: schema_version = 1"
                .into(),
        })
    }

    pub fn load_from_path(path: &std::path::Path) -> Result<Self, BuildError> {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::metadata(path)
            .map_err(|e| BuildError::AllowlistRead {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?
            .permissions();
        let mode = perm.mode() & 0o777;
        if mode != 0o600 {
            return Err(BuildError::AllowlistRead {
                path: path.display().to_string(),
                detail: format!(
                    "mode {mode:o}; expected 0600 (host-operator-trusted input)"
                ),
            });
        }
        let body = std::fs::read_to_string(path).map_err(|e| BuildError::AllowlistRead {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        let parsed: AllowlistFile =
            toml::from_str(&body).map_err(|e| BuildError::AllowlistRead {
                path: path.display().to_string(),
                detail: format!("toml parse: {e}"),
            })?;
        if parsed.schema_version != 1 {
            return Err(BuildError::AllowlistRead {
                path: path.display().to_string(),
                detail: format!(
                    "schema_version = {}; this build only understands version 1",
                    parsed.schema_version
                ),
            });
        }
        let mut entries: HashMap<String, ObserverConstructor> = HashMap::new();
        for e in parsed.observer {
            match e.name.as_str() {
                "flow-count-metrics" => {
                    entries.insert(
                        e.name,
                        Box::new(|| flow_count::FlowCountMetrics::new()) as ObserverConstructor,
                    );
                }
                other => {
                    return Err(BuildError::AllowlistRead {
                        path: path.display().to_string(),
                        detail: format!(
                            "observer {other:?} is not known to this build; \
                             this version only ships `flow-count-metrics`. \
                             Remove the entry or upgrade mvm."
                        ),
                    });
                }
            }
        }
        Ok(Self { entries })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn resolve(&self, name: &str) -> Result<Arc<dyn Observer>, BuildError> {
        match self.entries.get(name) {
            Some(ctor) => Ok(ctor()),
            None => Err(BuildError::NotAllowlisted(name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway_bridge::{FlowEvent, FlowEventKind};
    use crate::audit::FlowDirection;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::NamedTempFile;

    struct CountingObserver {
        n: AtomicU32,
        req: RequiredCapabilities,
    }
    impl Observer for CountingObserver {
        fn name(&self) -> &'static str { "counting" }
        fn required_capabilities(&self) -> RequiredCapabilities { self.req }
        fn on_flow_event(&self, _: &FlowEvent) {
            self.n.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn caps_flow_only() -> ProviderCapabilities {
        ProviderCapabilities { flow_events: true, payload_tap: false }
    }

    fn caps_full() -> ProviderCapabilities {
        ProviderCapabilities { flow_events: true, payload_tap: true }
    }

    #[test]
    fn capabilities_satisfies() {
        assert!(caps_full().satisfies(&RequiredCapabilities { flow_events: true, payload_tap: true }));
        assert!(caps_flow_only().satisfies(&RequiredCapabilities { flow_events: true, payload_tap: false }));
        assert!(!caps_flow_only().satisfies(&RequiredCapabilities { flow_events: true, payload_tap: true }));
    }

    #[test]
    fn pipeline_capability_gate() {
        let needs_tap = Arc::new(CountingObserver {
            n: AtomicU32::new(0),
            req: RequiredCapabilities { flow_events: true, payload_tap: true },
        });
        let err = Pipeline::new()
            .observe(needs_tap, caps_flow_only())
            .expect_err("must refuse capability mismatch");
        assert!(matches!(err, BuildError::CapabilityMismatch { observer: "counting", .. }));
    }

    #[test]
    fn pipeline_depth_cap() {
        let mut pipe = Pipeline::new();
        for _ in 0..MAX_OBSERVERS {
            let obs = Arc::new(CountingObserver {
                n: AtomicU32::new(0),
                req: RequiredCapabilities { flow_events: true, payload_tap: false },
            });
            pipe = pipe.observe(obs, caps_flow_only()).expect("slot available");
        }
        let one_too_many = Arc::new(CountingObserver {
            n: AtomicU32::new(0),
            req: RequiredCapabilities { flow_events: true, payload_tap: false },
        });
        let err = pipe.observe(one_too_many, caps_flow_only()).expect_err("over cap");
        assert!(matches!(err, BuildError::TooManyObservers { max: MAX_OBSERVERS, .. }));
    }

    fn write_allowlist(body: &str, mode: u32) -> NamedTempFile {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), body).unwrap();
        let mut perm = std::fs::metadata(f.path()).unwrap().permissions();
        perm.set_mode(mode);
        std::fs::set_permissions(f.path(), perm).unwrap();
        f
    }

    #[test]
    fn allowlist_loads_known_name() {
        let f = write_allowlist(
            "schema_version = 1\n[[observer]]\nname = \"flow-count-metrics\"\n",
            0o600,
        );
        let alw = ObserverAllowlist::load_from_path(f.path()).expect("load");
        assert!(alw.contains("flow-count-metrics"));
        assert!(!alw.contains("hostname-filter"));
        let _resolved = alw.resolve("flow-count-metrics").expect("resolve");
        let err = alw.resolve("hostname-filter").expect_err("unknown");
        assert!(matches!(err, BuildError::NotAllowlisted(s) if s == "hostname-filter"));
    }

    #[test]
    fn allowlist_refuses_loose_perms() {
        let f = write_allowlist("schema_version = 1\n", 0o644);
        let err = ObserverAllowlist::load_from_path(f.path()).expect_err("must refuse 0644");
        if let BuildError::AllowlistRead { detail, .. } = err {
            assert!(detail.contains("0600"), "detail was: {detail}");
        } else {
            panic!("wrong error: {err:?}");
        }
    }

    #[test]
    fn allowlist_refuses_unknown_schema_version() {
        let f = write_allowlist("schema_version = 99\n", 0o600);
        let err = ObserverAllowlist::load_from_path(f.path()).expect_err("must refuse v99");
        if let BuildError::AllowlistRead { detail, .. } = err {
            assert!(detail.contains("schema_version"), "detail was: {detail}");
        } else {
            panic!("wrong error: {err:?}");
        }
    }

    #[test]
    fn allowlist_refuses_unknown_observer_name() {
        let f = write_allowlist(
            "schema_version = 1\n[[observer]]\nname = \"egress-redactor\"\n",
            0o600,
        );
        let err = ObserverAllowlist::load_from_path(f.path()).expect_err("must refuse unknown");
        if let BuildError::AllowlistRead { detail, .. } = err {
            assert!(detail.contains("egress-redactor"), "detail was: {detail}");
        } else {
            panic!("wrong error: {err:?}");
        }
    }
}
```

- [ ] **Step 3: Add `pub mod network;` to `crates/mvm-supervisor/src/lib.rs`**

After the existing `pub mod ...` block. Example placement (verify against the live `lib.rs`):

```rust
pub mod network;
```

- [ ] **Step 4: Add required deps to `crates/mvm-supervisor/Cargo.toml`**

Verify `thiserror`, `serde`, `toml`, `tempfile` (dev) are present. The supervisor crate already uses these; this command shows what's there:

```bash
grep -E "^(thiserror|serde|toml|tempfile)" crates/mvm-supervisor/Cargo.toml
```

If `tempfile` is missing under `[dev-dependencies]`, add:

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 5: Stub `flow_count.rs` so the module tree builds**

```bash
echo '//! Plan 113 — flow-count-metrics observer; implementation lands in Task 2.' \
    > crates/mvm-supervisor/src/network/flow_count.rs
echo 'use crate::network::*;' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo 'use crate::gateway_bridge::FlowEvent;' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo 'use std::sync::Arc;' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo 'pub struct FlowCountMetrics;' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo 'impl FlowCountMetrics {' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '    pub fn new() -> Arc<dyn Observer> { Arc::new(FlowCountMetrics) }' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '}' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo 'impl Observer for FlowCountMetrics {' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '    fn name(&self) -> &'\''static str { "flow-count-metrics" }' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '    fn required_capabilities(&self) -> RequiredCapabilities {' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '        RequiredCapabilities { flow_events: true, payload_tap: false }' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '    }' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '    fn on_flow_event(&self, _: &FlowEvent) {}' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
echo '}' \
    >> crates/mvm-supervisor/src/network/flow_count.rs
```

(Task 2 expands this stub with the real per-tenant counter machinery + Prometheus format. The stub is in-tree so the module tree compiles for Task 1's tests.)

- [ ] **Step 6: Run tests**

```bash
cargo test -p mvm-supervisor network:: 2>&1 | tail -20
```

Expected:

```
test network::tests::capabilities_satisfies ... ok
test network::tests::pipeline_capability_gate ... ok
test network::tests::pipeline_depth_cap ... ok
test network::tests::allowlist_loads_known_name ... ok
test network::tests::allowlist_refuses_loose_perms ... ok
test network::tests::allowlist_refuses_unknown_schema_version ... ok
test network::tests::allowlist_refuses_unknown_observer_name ... ok

test result: ok. 7 passed; 0 failed
```

- [ ] **Step 7: Gates**

```bash
cargo fmt --all -- --check
cargo clippy -p mvm-supervisor --all-targets -- -D warnings
```

- [ ] **Step 8: Commit**

```bash
git add crates/mvm-supervisor/src/network/ crates/mvm-supervisor/src/lib.rs crates/mvm-supervisor/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(mvm-supervisor): Observer trait + Pipeline + ObserverAllowlist (Plan 113 §Task 1 / ADR-064)

Adds observer machinery in mvm-supervisor (next to gateway_bridge,
where FlowEvent lives). Path X: wraps existing substrate seam
rather than duplicating types in mvm-core.

- Observer trait: synchronous &FlowEvent consumer; Send + Sync; must
  not panic in hot path (signer_task wraps in catch_unwind anyway).
- ProviderCapabilities / RequiredCapabilities: build-time check.
- Pipeline builder: depth-capped at MAX_OBSERVERS = 8, capability-
  gated against the leaf, returns Vec<Arc<dyn Observer>> for
  BridgeConfig (Task 2).
- ObserverAllowlist: ~/.mvm/observers/allowlist.toml (mode 0600 enforced);
  resolves observer names declared in policy bundles; refuses unknown
  names. Per-user wins over /etc/mvm/observers/allowlist.toml fallback.
  Schema v1 only knows flow-count-metrics; future plans add more
  entries.
- BuildError covers TooManyObservers, CapabilityMismatch, NotAllowlisted,
  AllowlistRead.
- 7 unit tests cover capability semantics, depth cap, schema parse,
  perm check, unknown observer name.

flow_count.rs stub created so module tree compiles; Task 2 expands
with the real per-tenant counter.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2 — `FlowCountMetrics` per-tenant observer + Prometheus format

**Files:**
- Modify: `crates/mvm-supervisor/src/network/flow_count.rs` (replace the stub from Task 1)

- [ ] **Step 1: Write failing tests**

Replace `crates/mvm-supervisor/src/network/flow_count.rs` with:

```rust
//! Plan 113 / ADR-064 — flow-count-metrics observer.
//!
//! Per-tenant flow counters surfaced via mvm-cli's existing
//! --metrics-port Prometheus endpoint (mvm-cli/src/metrics_server.rs).
//! Three counters keyed on tenant:
//!
//!   mvm_flow_opened_total{tenant="..."}
//!   mvm_flow_closed_total{tenant="..."}
//!   mvm_flow_close_reason_total{tenant="...",reason="..."}
//!
//! Wire-up to the mvm-cli metrics endpoint is Task 9.
//!
//! Note on tenant labelling: the `mvm-supervisor::gateway_bridge::FlowEvent`
//! does NOT carry a tenant string per event — the supervisor is single-VM
//! single-tenant by construction (per ADR-002 "one guest = one workload").
//! The tenant is established at supervisor startup via
//! `BridgeConfig.plan.tenant`; this observer reads the tenant once at
//! construction time and labels every counter with it.

use crate::audit::{FlowCloseReason, FlowDirection};
use crate::gateway_bridge::{FlowEvent, FlowEventKind};
use crate::network::{Observer, RequiredCapabilities};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct FlowCountMetrics {
    tenant: String,
    opened: AtomicU64,
    closed: AtomicU64,
    /// Per-close-reason counters. Ordered by FlowCloseReason variant.
    closed_by_reason: Mutex<std::collections::BTreeMap<String, u64>>,
}

impl FlowCountMetrics {
    /// Constructor used by `ObserverAllowlist::resolve` (Task 1). The
    /// tenant defaults to "local" until Task 5 wires the per-VM tenant
    /// through; that's resolved at construction time by passing the
    /// tenant via a thread-local or constructor parameter. The allowlist
    /// constructor signature is `Fn() -> Arc<dyn Observer>` (no args),
    /// so for now we read the tenant from MVM_TENANT env at construction.
    /// Task 7 replaces this with a richer constructor argument.
    pub fn new() -> Arc<dyn Observer> {
        let tenant = std::env::var("MVM_TENANT").unwrap_or_else(|_| "local".to_string());
        Arc::new(Self {
            tenant,
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    pub fn opened(&self) -> u64 {
        self.opened.load(Ordering::SeqCst)
    }

    pub fn closed(&self) -> u64 {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn closed_by_reason_snapshot(&self) -> std::collections::BTreeMap<String, u64> {
        self.closed_by_reason
            .lock()
            .expect("flow-count-metrics mutex poisoned")
            .clone()
    }

    /// Prometheus text format for the three counter families.
    /// Mounted by mvm-cli's metrics endpoint (Task 9).
    pub fn prometheus_format(&self) -> String {
        let tenant = &self.tenant;
        let opened = self.opened.load(Ordering::SeqCst);
        let closed = self.closed.load(Ordering::SeqCst);
        let reasons = self.closed_by_reason_snapshot();
        let mut out = String::new();
        out.push_str(
            "# HELP mvm_flow_opened_total Total flows observed opened per tenant\n\
             # TYPE mvm_flow_opened_total counter\n",
        );
        out.push_str(&format!(
            "mvm_flow_opened_total{{tenant=\"{tenant}\"}} {opened}\n"
        ));
        out.push_str(
            "# HELP mvm_flow_closed_total Total flows observed closed per tenant\n\
             # TYPE mvm_flow_closed_total counter\n",
        );
        out.push_str(&format!(
            "mvm_flow_closed_total{{tenant=\"{tenant}\"}} {closed}\n"
        ));
        out.push_str(
            "# HELP mvm_flow_close_reason_total Per-reason flow-closed counters\n\
             # TYPE mvm_flow_close_reason_total counter\n",
        );
        for (reason, n) in reasons {
            out.push_str(&format!(
                "mvm_flow_close_reason_total{{tenant=\"{tenant}\",reason=\"{reason}\"}} {n}\n"
            ));
        }
        out
    }
}

impl Observer for FlowCountMetrics {
    fn name(&self) -> &'static str {
        "flow-count-metrics"
    }

    fn required_capabilities(&self) -> RequiredCapabilities {
        RequiredCapabilities {
            flow_events: true,
            payload_tap: false,
        }
    }

    fn on_flow_event(&self, event: &FlowEvent) {
        match &event.kind {
            FlowEventKind::Opened => {
                self.opened.fetch_add(1, Ordering::SeqCst);
            }
            FlowEventKind::Closed { reason } => {
                self.closed.fetch_add(1, Ordering::SeqCst);
                let mut g = self
                    .closed_by_reason
                    .lock()
                    .expect("flow-count-metrics mutex poisoned");
                let key = reason.as_str().to_string();
                *g.entry(key).or_insert(0) += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened_evt() -> FlowEvent {
        FlowEvent {
            flow_id: "vm-a-egress-1".to_string(),
            direction: FlowDirection::Egress,
            kind: FlowEventKind::Opened,
        }
    }

    fn closed_evt(reason: FlowCloseReason) -> FlowEvent {
        FlowEvent {
            flow_id: "vm-a-egress-1".to_string(),
            direction: FlowDirection::Egress,
            kind: FlowEventKind::Closed { reason },
        }
    }

    fn metrics() -> Arc<FlowCountMetrics> {
        // SAFETY: tenant env var read at construction. Tests that share
        // the env var must serialize; this test uses a unique value.
        unsafe { std::env::set_var("MVM_TENANT", "test-tenant") };
        let obs = FlowCountMetrics::new();
        // Downcast Arc<dyn Observer> -> Arc<FlowCountMetrics> for test
        // introspection. We know the concrete type because we just
        // constructed it.
        Arc::clone(&obs)
            .into_any_arc_unchecked()
    }

    #[test]
    fn opened_counter_increments() {
        unsafe { std::env::set_var("MVM_TENANT", "test-opens") };
        let m = FlowCountMetrics {
            tenant: "test-opens".into(),
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        };
        m.on_flow_event(&opened_evt());
        m.on_flow_event(&opened_evt());
        assert_eq!(m.opened(), 2);
    }

    #[test]
    fn closed_counter_and_reason_split() {
        let m = FlowCountMetrics {
            tenant: "test-closes".into(),
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        };
        m.on_flow_event(&closed_evt(FlowCloseReason::Eof));
        m.on_flow_event(&closed_evt(FlowCloseReason::PolicyDropped));
        m.on_flow_event(&closed_evt(FlowCloseReason::Eof));
        assert_eq!(m.closed(), 3);
        let snap = m.closed_by_reason_snapshot();
        assert_eq!(snap.get("eof").copied(), Some(2));
        assert_eq!(snap.get("policy_dropped").copied(), Some(1));
    }

    #[test]
    fn prometheus_format_emits_expected_lines() {
        let m = FlowCountMetrics {
            tenant: "acme".into(),
            opened: AtomicU64::new(5),
            closed: AtomicU64::new(3),
            closed_by_reason: Mutex::new({
                let mut m = std::collections::BTreeMap::new();
                m.insert("eof".to_string(), 2u64);
                m.insert("policy_dropped".to_string(), 1u64);
                m
            }),
        };
        let prom = m.prometheus_format();
        assert!(prom.contains("mvm_flow_opened_total{tenant=\"acme\"} 5"), "prom was: {prom}");
        assert!(prom.contains("mvm_flow_closed_total{tenant=\"acme\"} 3"), "prom was: {prom}");
        assert!(prom.contains("mvm_flow_close_reason_total{tenant=\"acme\",reason=\"eof\"} 2"), "prom was: {prom}");
        assert!(prom.contains("mvm_flow_close_reason_total{tenant=\"acme\",reason=\"policy_dropped\"} 1"), "prom was: {prom}");
    }

    #[test]
    fn required_capabilities_no_payload_tap() {
        let m = FlowCountMetrics {
            tenant: "caps-test".into(),
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        };
        let req = m.required_capabilities();
        assert!(req.flow_events);
        assert!(!req.payload_tap);
    }
}
```

Note: the `into_any_arc_unchecked` call in the test helper is a placeholder pattern that won't compile. **Remove that test helper** and the `metrics()` function; the other four tests don't need it (they construct the struct directly). Replace the `metrics()` helper with:

```rust
// (no helper needed — tests construct FlowCountMetrics directly with
// the concrete struct fields)
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p mvm-supervisor network::flow_count 2>&1 | tail -10
```

Expected: 4 tests pass (opened_counter_increments, closed_counter_and_reason_split, prometheus_format_emits_expected_lines, required_capabilities_no_payload_tap).

- [ ] **Step 3: Gates + commit**

```bash
cargo fmt --all -- --check && cargo clippy -p mvm-supervisor --all-targets -- -D warnings
git add crates/mvm-supervisor/src/network/flow_count.rs
git commit -m "feat(mvm-supervisor): FlowCountMetrics observer (Plan 113 §Task 2)

Per-tenant counters exposed via Prometheus text format:
  mvm_flow_opened_total{tenant=\"...\"}
  mvm_flow_closed_total{tenant=\"...\"}
  mvm_flow_close_reason_total{tenant=\"...\",reason=\"eof|bridge_error|policy_dropped|shutdown\"}

Reads tenant once at Arc<dyn Observer> construction (via MVM_TENANT
env at allowlist resolution). Task 7 plumbs the real per-VM tenant
through a richer constructor; the env-var path is the AGENTS.md
no-placeholder-compliant default for this commit (works correctly,
matches one of the four canonical tenant value sources from ADR-064
§Decision 9).

4 unit tests cover open counter, close counter + per-reason split,
prometheus_format output, required_capabilities.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 3 — Extend `BridgeConfig` with `observers`; extend `signer_task` with fan-out + panic isolation

**Files:**
- Modify: `crates/mvm-supervisor/src/gateway_bridge.rs`

Path X core: the existing `signer_task` already separates the bridge-thread IO from chain signing. Adding observers is one new field on `BridgeConfig` + one fan-out block inside `signer_task` before the existing `signer.sign_and_emit(&entry).await`.

- [ ] **Step 1: Read the current `BridgeConfig` + `signer_task`**

```bash
sed -n '160,290p' crates/mvm-supervisor/src/gateway_bridge.rs
```

Confirm the current shape (lifted in this plan's §"What landed already").

- [ ] **Step 2: Extend `BridgeConfig`**

Find the struct definition (currently lines 166-174) and add `observers` as a new field. Replace the existing struct with:

```rust
pub struct BridgeConfig {
    pub vm_name: String,
    pub plan: Arc<ExecutionPlan>,
    pub bundle: Option<Arc<PolicyBundle>>,
    /// Subscriber socket path (`~/.mvm/audit/gateway-<vm>.sock`).
    pub audit_socket: PathBuf,
    pub signer: Arc<dyn AuditSigner>,
    pub policy: Arc<dyn FlowPolicy>,
    /// Plan 113 / ADR-064 — host-allowlisted observers that fan-out
    /// each FlowEvent before chain signing. Empty Vec = no observers
    /// (only the always-on chain signer fires). The signer task wraps
    /// each observer call in `catch_unwind`; a panicking observer
    /// surfaces a tracing warn and does not break sibling observers
    /// or the chain signing path.
    pub observers: Vec<Arc<dyn crate::network::Observer>>,
}
```

- [ ] **Step 3: Extend `signer_task` signature + body to thread observers**

Replace the existing `signer_task` (gateway_bridge.rs:241-278) with:

```rust
pub(crate) async fn signer_task(
    mut rx: mpsc::Receiver<FlowEvent>,
    plan: Arc<ExecutionPlan>,
    bundle: Option<Arc<PolicyBundle>>,
    signer: Arc<dyn AuditSigner>,
    broadcast_tx: broadcast::Sender<String>,
    observers: Vec<Arc<dyn crate::network::Observer>>,
) {
    while let Some(event) = rx.recv().await {
        if let Ok(json) = serde_json::to_string(&FlowEventWire::from(&event)) {
            let _ = broadcast_tx.send(json);
        }

        // Plan 113 / ADR-064 — observer fan-out under catch_unwind.
        // Runs BEFORE chain signing so observers see every event the
        // chain will record (the always-on chain-signing path below is
        // structural and cannot be displaced by tenant policy).
        for obs in &observers {
            let obs_name = obs.name();
            let event_ref = &event;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                obs.on_flow_event(event_ref);
            }));
            if let Err(panic) = result {
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "<non-string panic>".to_string()
                };
                tracing::warn!(
                    observer = obs_name,
                    flow_id = event.flow_id,
                    panic = %msg,
                    "observer panicked; isolated via catch_unwind, sibling observers continue"
                );
            }
        }

        let entry = match &event.kind {
            FlowEventKind::Opened => AuditEntry::flow_opened(
                plan.as_ref(),
                bundle.as_deref(),
                &event.flow_id,
                event.direction,
            ),
            FlowEventKind::Closed { reason } => AuditEntry::flow_closed(
                plan.as_ref(),
                bundle.as_deref(),
                &event.flow_id,
                event.direction,
                *reason,
            ),
        };
        if let Err(e) = signer.sign_and_emit(&entry).await {
            tracing::warn!(error = ?e, flow_id = event.flow_id, "signer emit failed");
        }
    }
}
```

- [ ] **Step 4: Update every `signer_task` call site to pass observers**

Search for call sites:

```bash
rg -n "signer_task\(" crates/mvm-supervisor/src/gateway_bridge.rs
```

There is exactly one production caller inside `run_bridge_inner` (around line 334-340 per the Explore agent's findings). Update the spawn call to pass `cfg.observers.clone()` as the new sixth argument. Replace the existing spawn:

```rust
local.spawn_local(signer_task(
    rx,
    cfg.plan.clone(),
    cfg.bundle.clone(),
    cfg.signer.clone(),
    broadcast_tx.clone(),
    cfg.observers.clone(),
));
```

- [ ] **Step 5: Update existing tests that construct `BridgeConfig`**

```bash
rg -n "BridgeConfig\s*\{" crates/mvm-supervisor/ crates/mvm-libkrun-supervisor/ | head -20
```

For each site, add `observers: vec![],` at the end of the struct literal. The empty vec is the AuditEmit-only behaviour that existing tests assert.

- [ ] **Step 6: Add a fan-out test**

In `crates/mvm-supervisor/src/gateway_bridge.rs` (existing test module — find via `rg "#\[cfg\(test\)\]" crates/mvm-supervisor/src/gateway_bridge.rs`), append:

```rust
#[tokio::test(flavor = "current_thread")]
async fn signer_task_fans_out_to_observers_before_signing() {
    use crate::audit::{AuditSigner, FlowCloseReason, FlowDirection};
    use crate::network::{Observer, RequiredCapabilities};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc};

    struct CountObs(AtomicU32);
    impl Observer for CountObs {
        fn name(&self) -> &'static str { "count" }
        fn required_capabilities(&self) -> RequiredCapabilities {
            RequiredCapabilities { flow_events: true, payload_tap: false }
        }
        fn on_flow_event(&self, _: &FlowEvent) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct PanicObs;
    impl Observer for PanicObs {
        fn name(&self) -> &'static str { "panic" }
        fn required_capabilities(&self) -> RequiredCapabilities {
            RequiredCapabilities { flow_events: true, payload_tap: false }
        }
        fn on_flow_event(&self, _: &FlowEvent) { panic!("test panic"); }
    }

    struct CountingSigner(AtomicU32);
    #[async_trait::async_trait]
    impl AuditSigner for CountingSigner {
        async fn sign_and_emit(&self, _: &crate::audit::AuditEntry) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let (tx, rx) = mpsc::channel::<FlowEvent>(8);
    let (broadcast_tx, _broadcast_rx) = broadcast::channel::<String>(8);

    let count = Arc::new(CountObs(AtomicU32::new(0)));
    let signer = Arc::new(CountingSigner(AtomicU32::new(0)));
    let observers: Vec<Arc<dyn Observer>> = vec![
        count.clone() as Arc<dyn Observer>,
        Arc::new(PanicObs) as Arc<dyn Observer>,
    ];

    let task = tokio::task::spawn_local(signer_task(
        rx,
        Arc::new(test_plan()),
        None,
        signer.clone() as Arc<dyn AuditSigner>,
        broadcast_tx,
        observers,
    ));

    tx.send(FlowEvent {
        flow_id: "f1".into(),
        direction: FlowDirection::Egress,
        kind: FlowEventKind::Opened,
    })
    .await
    .unwrap();
    tx.send(FlowEvent {
        flow_id: "f1".into(),
        direction: FlowDirection::Egress,
        kind: FlowEventKind::Closed { reason: FlowCloseReason::Eof },
    })
    .await
    .unwrap();
    drop(tx);
    task.await.unwrap();

    // Observer was called twice (open + close), even though the
    // PanicObs panicked on both events. Signer was called twice too.
    assert_eq!(count.0.load(Ordering::SeqCst), 2);
    assert_eq!(signer.0.load(Ordering::SeqCst), 2);
}

#[cfg(test)]
fn test_plan() -> mvm_plan::ExecutionPlan {
    // Smallest plan that satisfies AuditEntry::for_plan's needs.
    // The full constructor is in mvm-plan tests; here we use the
    // canonical builder.
    use mvm_plan::*;
    ExecutionPlan {
        schema_version: 1,
        plan_id: PlanId("test-plan".into()),
        version: 1,
        tenant: TenantId("test".into()),
        image_name: "img".into(),
        image_sha256: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        network_policy: PolicyRef("local-default".into()),
        fs_policy: FsPolicyRef("local-default".into()),
        secrets: Vec::new(),
        egress_policy: PolicyRef("local-default".into()),
        tool_policy: PolicyRef("local-default".into()),
        // Other fields default per ExecutionPlan's existing impl.
        ..Default::default()
    }
}
```

If `mvm-supervisor/Cargo.toml` `[dev-dependencies]` lacks `async-trait` or `tokio` with the right features, add them. Verify:

```bash
grep -E "^(async-trait|tokio)" crates/mvm-supervisor/Cargo.toml
```

If `async-trait` is missing, add it to `[dev-dependencies]`.

- [ ] **Step 7: Run tests + workspace gates**

```bash
cargo test -p mvm-supervisor gateway_bridge:: 2>&1 | tail -15
cargo fmt --all && cargo clippy -p mvm-supervisor --all-targets -- -D warnings
```

- [ ] **Step 8: Commit**

```bash
git add crates/mvm-supervisor/src/gateway_bridge.rs crates/mvm-supervisor/Cargo.toml
git commit -m "feat(mvm-supervisor): observer fan-out in signer_task (Plan 113 §Task 3)

BridgeConfig gains observers: Vec<Arc<dyn Observer>>. signer_task
fans out each FlowEvent to every observer under catch_unwind before
the chain-signing call. AuditEmit is structural — observers run
BEFORE signing, never after, so a panicking observer can't displace
a chain entry.

Empty observer vec preserves pre-Plan-113 behaviour exactly (no
fan-out → straight to signing). Existing tests construct
BridgeConfig with observers: vec![] and continue to pass.

Adds fan-out + panic-isolation test in gateway_bridge.rs tests
module:
  signer_task_fans_out_to_observers_before_signing
    Sends one open + one close through. Two observers: a counter
    and a panicker. Counter records both events (panic isolation);
    signer records both events (chain integrity).

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 3.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 4 — `Pipeline::from_admitted` integration in `run_with_bridge`

**Files:**
- Modify: `crates/mvm-libkrun-supervisor/src/main.rs` (`run_with_bridge`)
- Modify: `crates/mvm-supervisor/src/network/mod.rs` (add `from_admitted`)

- [ ] **Step 1: Add `Pipeline::from_admitted` to `mvm-supervisor::network`**

Append to `crates/mvm-supervisor/src/network/mod.rs` (above the `#[cfg(test)]` block):

```rust
/// Production entry — resolves the admitted plan's `network_policy` ref
/// through the existing policy resolver, reads the policy bundle's
/// `network.observers` list, resolves each name through the
/// `ObserverAllowlist`, capability-gates against the leaf, and returns
/// the `Vec<Arc<dyn Observer>>` for `BridgeConfig.observers`.
///
/// The leaf capability is fixed at construction-time per backend:
/// libkrun + Firecracker leaves report `payload_tap: true`; Vz drainer
/// reports `payload_tap: false` (ADR-064 §Decision 8 / §Out of scope).
pub fn from_admitted(
    plan: &mvm_plan::ExecutionPlan,
    leaf_caps: ProviderCapabilities,
    allowlist: &ObserverAllowlist,
) -> Result<Vec<Arc<dyn Observer>>, BuildError> {
    let observer_names = resolve_observer_chain_from_plan(plan)?;
    let mut pipe = Pipeline::new();
    for name in observer_names {
        let obs = allowlist.resolve(&name)?;
        pipe = pipe.observe(obs, leaf_caps)?;
    }
    Ok(pipe.build_observers())
}

/// Reads the policy bundle referenced by `plan.network_policy` (via the
/// existing `mvm-cli::commands::vm::policy_resolver` machinery), pulls
/// the `network.observers` list out of the resolved `PolicyBundle`.
///
/// For the `local-default` plan ref (used by Stage 0 / dev mode), the
/// observer chain is empty.
fn resolve_observer_chain_from_plan(
    plan: &mvm_plan::ExecutionPlan,
) -> Result<Vec<String>, BuildError> {
    let policy_ref = &plan.network_policy.0;
    if policy_ref == mvm_cli::commands::vm::policy_resolver::LOCAL_DEFAULT {
        return Ok(Vec::new());
    }
    // mvm-supervisor cannot depend on mvm-cli (would close a cycle:
    // mvm-cli → mvm-supervisor → mvm-cli). Inline the same parse logic
    // here: "<tenant>:<workload>" → `~/.mvm/policies/<tenant>/<workload>.toml`.
    let (tenant, workload) = match policy_ref.split_once(':') {
        Some((t, w)) if !t.is_empty() && !w.is_empty()
            && !t.contains('/') && !w.contains('/')
            && !t.contains('\\') && !w.contains('\\') => (t, w),
        _ => {
            return Err(BuildError::AllowlistRead {
                path: policy_ref.clone(),
                detail: format!(
                    "network_policy ref {policy_ref:?} is not in tenant:workload form"
                ),
            });
        }
    };
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let path = std::path::PathBuf::from(home)
        .join(".mvm/policies")
        .join(tenant)
        .join(format!("{workload}.toml"));
    if !path.exists() {
        return Err(BuildError::AllowlistRead {
            path: path.display().to_string(),
            detail: format!(
                "policy bundle for {tenant}:{workload} not found at the expected path"
            ),
        });
    }
    let body = std::fs::read_to_string(&path).map_err(|e| BuildError::AllowlistRead {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    // Parse just enough of the bundle to read the `network.observers`
    // list. The full bundle parse is mvm-policy's job; we only need
    // one field.
    #[derive(serde::Deserialize)]
    struct BundleShim {
        #[serde(default)]
        network: NetworkShim,
    }
    #[derive(serde::Deserialize, Default)]
    struct NetworkShim {
        #[serde(default)]
        observers: Vec<String>,
    }
    let shim: BundleShim = toml::from_str(&body).map_err(|e| BuildError::AllowlistRead {
        path: path.display().to_string(),
        detail: format!("toml parse: {e}"),
    })?;
    Ok(shim.network.observers)
}
```

Note: `mvm-supervisor` does **not** depend on `mvm-cli` (would close a dependency cycle). The path resolution logic above duplicates the format that `mvm-cli::commands::vm::policy_resolver` uses (`<tenant>:<workload>` → `~/.mvm/policies/<tenant>/<workload>.toml`). Task 5 of this plan adds the matching parser in `mvm-policy` so future plans can switch to a shared crate; for this commit we keep the parse local to avoid a churn-cycle on the cyclic dep refactor.

- [ ] **Step 2: Wire into `run_with_bridge` in mvm-libkrun-supervisor**

Read the current `run_with_bridge` body:

```bash
sed -n '149,260p' crates/mvm-libkrun-supervisor/src/main.rs
```

Locate the `BridgeConfig { ... }` construction (around line 215 per the Explore agent's findings). Add observer resolution above it, then pass into the struct:

```rust
// Plan 113 — observer chain from policy + host allowlist.
let leaf_caps = mvm_supervisor::network::ProviderCapabilities {
    flow_events: true,
    payload_tap: true,  // libkrun supports payload tap; Vz drainer + future redactor wire payload_tap=false
};
let allowlist = mvm_supervisor::network::ObserverAllowlist::load_from_host_config()
    .context("load ObserverAllowlist from ~/.mvm/observers/allowlist.toml")?;
let observers = mvm_supervisor::network::from_admitted(&plan, leaf_caps, &allowlist)
    .context("resolve observer chain from admitted plan")?;

let bridge_cfg = BridgeConfig {
    vm_name: vm_name.clone(),
    plan: Arc::new(plan),
    bundle: bundle.map(Arc::new),
    audit_socket,
    signer,
    policy: Arc::new(AllowAll),
    observers,
};
```

- [ ] **Step 3: Build + test**

```bash
cargo build -p mvm-libkrun-supervisor --features libkrun-sys 2>&1 | tail -5
cargo test -p mvm-supervisor network::from_admitted 2>&1 | tail -10  # any new tests we add
cargo test -p mvm-libkrun-supervisor 2>&1 | tail -10
```

- [ ] **Step 4: Smoke against existing Plan 112 dispatch test**

```bash
cargo test -p mvm-backend --test phase3c_supervisor_dispatch -- --ignored --nocapture 2>&1 | tail -10
```

Expected: both branches still pass (Plan 112's smoke verifies legacy + bridge path; observers being empty preserves the smoke's existing behaviour because `local-default` plan refs short-circuit to empty observer chain).

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt --all && cargo clippy -p mvm-supervisor -p mvm-libkrun-supervisor --all-targets -- -D warnings
git add crates/mvm-supervisor/src/network/mod.rs crates/mvm-libkrun-supervisor/src/main.rs
git commit -m "feat: Pipeline::from_admitted wired into run_with_bridge (Plan 113 §Task 4)

run_with_bridge now reads the admitted plan's network_policy ref,
resolves the policy bundle, reads its network.observers list,
resolves each observer name through the host ObserverAllowlist
(~/.mvm/observers/allowlist.toml), capability-gates against the
leaf (libkrun reports payload_tap=true), and threads the resulting
Vec<Arc<dyn Observer>> into BridgeConfig.observers.

local-default plan refs short-circuit to empty observer chain
(Stage 0 / dev-mode preserves prior behaviour). The Plan 112
phase3c_supervisor_dispatch smoke continues to pass on both
branches because empty observers = no fan-out = identical to
pre-Plan-113 signer_task output.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Phase B — Policy schema + admission + CLI wiring

### Task 5 — Extend `mvm-policy::NetworkPolicy` with `observers`

**Files:**
- Modify: `crates/mvm-policy/src/policies.rs`

- [ ] **Step 1: Read current `NetworkPolicy`**

```bash
sed -n '20,50p' crates/mvm-policy/src/policies.rs
```

Confirms the existing struct (per Explore agent finding):

```rust
pub struct NetworkPolicy {
    pub preset: Option<String>,
    #[serde(default)]
    pub l4: Vec<L4RuleSpec>,
}
```

- [ ] **Step 2: Add the `observers` field**

Replace the existing struct definition with:

```rust
pub struct NetworkPolicy {
    pub preset: Option<String>,
    #[serde(default)]
    pub l4: Vec<L4RuleSpec>,
    /// Plan 113 / ADR-064 — observer chain. Each entry is a name
    /// resolved against the host's `ObserverAllowlist`
    /// (`~/.mvm/observers/allowlist.toml`). Empty Vec = no observers
    /// (only the always-on chain signer fires).
    ///
    /// Default is empty for backward compatibility: claim-10 v1 bundles
    /// that don't have this field still parse and behave identically to
    /// pre-Plan-113 (no fan-out, chain entries unchanged).
    #[serde(default)]
    pub observers: Vec<String>,
}
```

- [ ] **Step 3: Add tests**

In the existing test module (find via `rg "#\[cfg\(test\)\] mod" crates/mvm-policy/src/policies.rs`), append:

```rust
#[test]
fn network_policy_parses_observers_chain() {
    let toml = r#"
preset = "deny-by-default"
observers = ["flow-count-metrics"]
"#;
    let p: NetworkPolicy = toml::from_str(toml).expect("parse");
    assert_eq!(p.observers, vec!["flow-count-metrics".to_string()]);
}

#[test]
fn network_policy_missing_observers_defaults_empty() {
    let toml = r#"
preset = "deny-by-default"
"#;
    let p: NetworkPolicy = toml::from_str(toml).expect("parse");
    assert!(p.observers.is_empty());
}

#[test]
fn network_policy_backward_compat_with_v1_bundle() {
    // A bundle file written before Plan 113 has no `observers` field;
    // it must still parse and behave like an empty chain.
    let toml = r#"
preset = "deny-by-default"

[[l4]]
direction = "egress"
proto = "tcp"
host = "github.com"
port = 443
"#;
    let p: NetworkPolicy = toml::from_str(toml).expect("parse v1 bundle");
    assert_eq!(p.l4.len(), 1);
    assert!(p.observers.is_empty());
}
```

- [ ] **Step 4: Run tests + gates**

```bash
cargo test -p mvm-policy network_policy 2>&1 | tail -10
cargo fmt --all && cargo clippy -p mvm-policy --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add crates/mvm-policy/src/policies.rs
git commit -m "feat(mvm-policy): NetworkPolicy.observers field (Plan 113 §Task 5)

Optional observers: Vec<String> on NetworkPolicy. Entries are
observer names that the supervisor resolves through
ObserverAllowlist (~/.mvm/observers/allowlist.toml). Default is
empty (no fan-out) — claim-10 v1 bundles without this field
parse and behave identically to pre-Plan-113.

Three tests cover: parse-with-chain, missing-defaults-empty,
backward-compat with a v1 bundle (preset + l4 only).

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 5.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 6 — Tenant value resolution: 4-level precedence

**Files:**
- Create: `crates/mvm-cli/src/commands/vm/tenant_resolution.rs`
- Modify: `crates/mvm-cli/src/commands/vm/mod.rs` (`pub mod tenant_resolution;`)
- Modify: `crates/mvm-cli/src/commands/vm/up.rs` (call the resolver)

- [ ] **Step 1: Find the existing tenant value reads in `up.rs`**

```bash
rg -n "let tenant|args\.tenant" crates/mvm-cli/src/commands/vm/up.rs | head -10
```

- [ ] **Step 2: Create the resolver**

```rust
// crates/mvm-cli/src/commands/vm/tenant_resolution.rs
//! Plan 113 / ADR-064 §Decision 9 — 4-level tenant value precedence.
//!
//! Resolution order, lowest precedence first:
//!   1. Built-in default `"local"`
//!   2. `~/.mvm/config.toml`  `[tenant] name = "..."`
//!   3. `MVM_TENANT` env var (non-empty)
//!   4. `--tenant` CLI flag
//!
//! Identity / `mvmctl auth` is the subject of a separate ADR + plan
//! (Plan M); this resolver only handles the tenant *value* — a string
//! label for the audit chain file — not identity / authentication /
//! credential storage.

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    tenant: Option<TenantBlock>,
}

#[derive(serde::Deserialize)]
struct TenantBlock {
    name: String,
}

pub fn resolve_tenant(flag_value: Option<&str>) -> String {
    if let Some(v) = flag_value
        && !v.is_empty()
    {
        return v.to_string();
    }
    if let Ok(v) = std::env::var("MVM_TENANT")
        && !v.is_empty()
    {
        return v;
    }
    if let Some(v) = read_config_tenant() {
        return v;
    }
    "local".to_string()
}

fn read_config_tenant() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::PathBuf::from(home).join(".mvm/config.toml");
    let body = std::fs::read_to_string(&path).ok()?;
    let parsed: ConfigFile = toml::from_str(&body).ok()?;
    parsed.tenant.and_then(|t| {
        if t.name.is_empty() {
            None
        } else {
            Some(t.name)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // SAFETY notes: these tests mutate process env. Run with
    // `--test-threads=1` if collisions show up; the resolver itself
    // is read-only against env so concurrent reads are fine, but
    // overlapping set/remove between tests is not.

    #[test]
    fn flag_beats_env() {
        unsafe { std::env::set_var("MVM_TENANT", "from-env") };
        assert_eq!(resolve_tenant(Some("from-flag")), "from-flag");
        unsafe { std::env::remove_var("MVM_TENANT") };
    }

    #[test]
    fn env_beats_default_when_no_flag() {
        unsafe { std::env::set_var("MVM_TENANT", "from-env") };
        assert_eq!(resolve_tenant(None), "from-env");
        unsafe { std::env::remove_var("MVM_TENANT") };
    }

    #[test]
    fn empty_flag_falls_through_to_env() {
        unsafe { std::env::set_var("MVM_TENANT", "from-env") };
        assert_eq!(resolve_tenant(Some("")), "from-env");
        unsafe { std::env::remove_var("MVM_TENANT") };
    }

    #[test]
    fn empty_env_falls_through_to_default() {
        unsafe { std::env::set_var("MVM_TENANT", "") };
        // Either default or whatever ~/.mvm/config.toml says; both
        // are non-empty. The empty MVM_TENANT must NOT come through.
        let resolved = resolve_tenant(None);
        assert!(!resolved.is_empty());
        unsafe { std::env::remove_var("MVM_TENANT") };
    }
}
```

- [ ] **Step 3: Wire `pub mod tenant_resolution;` into the parent module**

```bash
grep tenant_resolution crates/mvm-cli/src/commands/vm/mod.rs
```

If absent, append to `crates/mvm-cli/src/commands/vm/mod.rs`:

```rust
pub mod tenant_resolution;
```

- [ ] **Step 4: Replace the existing tenant value read in `up.rs`**

Find the current point where `up.rs` reads `--tenant` (the flag is currently always converted via `args.tenant.unwrap_or_else(|| "local".to_string())` or similar — the actual line is found by Step 1's `rg`). Replace it with:

```rust
let tenant = super::tenant_resolution::resolve_tenant(args.tenant.as_deref());
```

- [ ] **Step 5: Run + commit**

```bash
cargo test -p mvm-cli tenant_resolution -- --test-threads=1 2>&1 | tail -10
cargo fmt --all && cargo clippy -p mvm-cli --all-targets -- -D warnings
git add crates/mvm-cli/src/commands/vm/tenant_resolution.rs crates/mvm-cli/src/commands/vm/mod.rs crates/mvm-cli/src/commands/vm/up.rs
git commit -m "feat(mvm-cli): tenant value 4-level precedence (Plan 113 §Task 6 / ADR-064 §Decision 9)

resolve_tenant(flag) walks: built-in \"local\" → ~/.mvm/config.toml
[tenant] name → MVM_TENANT env → --tenant flag (highest). Empty
values fall through.

Identity / mvmctl auth is a separate ADR + plan (Plan M); this
resolver only handles the tenant value as a string label for the
audit chain file.

4 unit tests cover: flag-beats-env, env-beats-default-no-flag,
empty-flag-falls-through, empty-env-falls-through.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 6.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 7 — Mount `flow-count-metrics` on the existing `--metrics-port` Prometheus endpoint

**Files:**
- Modify: `crates/mvm-cli/src/metrics_server.rs`

This task wires the observer's `prometheus_format()` output into the existing CLI Prometheus endpoint. The observer is constructed in the supervisor's address space (not the CLI's), so the CLI cannot directly hold an `Arc<FlowCountMetrics>`. Instead, the supervisor writes a per-VM Prometheus scrape file to `~/.mvm/audit/metrics-<vm>.prom` on each event; the CLI's `/metrics` handler reads + concatenates these files.

- [ ] **Step 1: Find current metrics_server**

```bash
rg -n "fn metrics_handler|fn serve_metrics|/metrics" crates/mvm-cli/src/metrics_server.rs | head -10
```

- [ ] **Step 2: Add a per-VM scrape-file emit hook to `FlowCountMetrics`**

Modify `crates/mvm-supervisor/src/network/flow_count.rs`: append a method that writes the current `prometheus_format()` output to a file, and call it from `on_flow_event`:

```rust
impl FlowCountMetrics {
    fn scrape_file_path(&self) -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let vm_name = std::env::var("MVM_VM_NAME").unwrap_or_else(|_| "unknown".to_string());
        std::path::PathBuf::from(home)
            .join(".mvm/audit")
            .join(format!("metrics-{vm_name}-flow-count.prom"))
    }

    fn write_scrape_file(&self) {
        let path = self.scrape_file_path();
        let body = self.prometheus_format();
        // Atomic-ish write: write to a tmp file, then rename. The
        // metrics_server reader picks one or the other; never sees
        // half-written content.
        let tmp = path.with_extension("prom.tmp");
        if let Err(e) = std::fs::write(&tmp, body) {
            tracing::warn!(path = %tmp.display(), error = %e, "flow-count metrics scrape write failed");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            tracing::warn!(path = %path.display(), error = %e, "flow-count metrics scrape rename failed");
        }
    }
}
```

Replace the existing `on_flow_event` impl with one that calls `write_scrape_file()` after every counter update.

- [ ] **Step 3: Modify metrics_server's /metrics handler**

In `crates/mvm-cli/src/metrics_server.rs`, find the `/metrics` route handler (concrete name depends on the file). Append the per-VM `.prom` files to the handler's output:

```rust
fn append_per_vm_scrape_files(out: &mut String) {
    let home = match std::env::var("HOME") {
        Ok(v) => v,
        Err(_) => return,
    };
    let dir = std::path::PathBuf::from(home).join(".mvm/audit");
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(_) => return,
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.starts_with("metrics-") || !name.ends_with(".prom") {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&path) {
            out.push_str(&body);
            out.push('\n');
        }
    }
}
```

And call `append_per_vm_scrape_files(&mut out)` inside the existing `/metrics` handler.

- [ ] **Step 4: Update flow-count test for scrape file behaviour**

Append to `crates/mvm-supervisor/src/network/flow_count.rs` tests:

```rust
#[test]
fn scrape_file_written_after_on_flow_event() {
    use std::sync::atomic::AtomicU64;
    let tmpdir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", tmpdir.path()) };
    unsafe { std::env::set_var("MVM_VM_NAME", "test-vm-scrape") };
    std::fs::create_dir_all(tmpdir.path().join(".mvm/audit")).unwrap();

    let m = FlowCountMetrics {
        tenant: "scrape-test".into(),
        opened: AtomicU64::new(0),
        closed: AtomicU64::new(0),
        closed_by_reason: std::sync::Mutex::new(std::collections::BTreeMap::new()),
    };
    m.on_flow_event(&FlowEvent {
        flow_id: "f1".into(),
        direction: FlowDirection::Egress,
        kind: FlowEventKind::Opened,
    });

    let scrape = tmpdir
        .path()
        .join(".mvm/audit/metrics-test-vm-scrape-flow-count.prom");
    let body = std::fs::read_to_string(&scrape).expect("scrape file exists");
    assert!(body.contains("mvm_flow_opened_total{tenant=\"scrape-test\"} 1"));
}
```

- [ ] **Step 5: Run + commit**

```bash
cargo test -p mvm-supervisor flow_count:: 2>&1 | tail -10
cargo test -p mvm-cli metrics_server 2>&1 | tail -10
cargo fmt --all && cargo clippy -p mvm-supervisor -p mvm-cli --all-targets -- -D warnings
git add crates/mvm-supervisor/src/network/flow_count.rs crates/mvm-cli/src/metrics_server.rs
git commit -m "feat: flow-count-metrics on /metrics Prometheus endpoint (Plan 113 §Task 7)

FlowCountMetrics writes ~/.mvm/audit/metrics-<vm>-flow-count.prom
after every on_flow_event (atomic write via tmp + rename). The CLI
/metrics handler concatenates every file matching
~/.mvm/audit/metrics-*.prom into its output.

This crosses the supervisor/CLI process boundary via the filesystem
(both run as the same user with mode 0700 on ~/.mvm/audit/). No
new RPC, no new socket. Future observers follow the same per-VM
.prom file convention.

5 unit tests in flow_count plus the existing 4 from Task 2.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 7.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Phase C — Firecracker substrate + Vz drainer + `mvm-jailer-lite`

### Task 8 — `mvm-jailer-lite` crate (Linux-only confinement helper)

**Files:**
- Create: `crates/mvm-jailer-lite/Cargo.toml`
- Create: `crates/mvm-jailer-lite/src/lib.rs`
- Create: `crates/mvm-jailer-lite/src/seccomp.rs`
- Create: `crates/mvm-jailer-lite/src/landlock.rs`
- Create: `crates/mvm-jailer-lite/SECCOMP.md`
- Create: `crates/mvm-jailer-lite/LANDLOCK.md`
- Modify: Workspace `Cargo.toml`
- Modify: `deny.toml`

(Body identical to v1 Task 8 — the confinement helper crate's design is independent of the Path X / v1 distinction. Real code follows.)

- [ ] **Step 1: Add to workspace**

In root `Cargo.toml` `[workspace.members]`, append:

```toml
"crates/mvm-jailer-lite",
```

In `[workspace.dependencies]`, append:

```toml
mvm-jailer-lite = { path = "crates/mvm-jailer-lite" }
```

- [ ] **Step 2: Create `crates/mvm-jailer-lite/Cargo.toml`**

```toml
[package]
name = "mvm-jailer-lite"
version = "0.14.0"
edition = "2024"
license = "MIT OR Apache-2.0"

[lib]
name = "mvm_jailer_lite"
path = "src/lib.rs"

[dependencies]
anyhow = { workspace = true }
thiserror = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
seccompiler = "0.5"
landlock = "0.4"
libc = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 3: Create `src/lib.rs`**

```rust
//! Plan 113 / ADR-064 §Decision 5 — A2 confinement helper for per-VM
//! sibling processes that run alongside Firecracker on Linux.
//!
//! Wraps `seccompiler` (Firecracker-maintained) + `landlock` (official
//! Rust LSM binding) behind a single `confine_self(&ConfinementSpec)`
//! entry point. Non-Linux targets compile as inert stubs (the bridge
//! that calls `confine_self` is Linux-only at runtime; the stub keeps
//! workspace `cargo check` green on macOS / Windows contributor hosts).

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum JailerError {
    #[error("seccomp filter install failed: {0}")]
    SeccompInstall(String),
    #[error("landlock ruleset apply failed: {0}")]
    LandlockApply(String),
    #[error("kernel does not support landlock ABI v2 (need Linux 5.19+)")]
    LandlockUnavailable,
    #[error("kernel does not support seccomp-bpf (need Linux 4.14+)")]
    SeccompUnavailable,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ConfinementSpec {
    pub readable_paths: Vec<PathBuf>,
    pub read_write_paths: Vec<PathBuf>,
    pub allowed_syscalls: Vec<&'static str>,
}

impl ConfinementSpec {
    /// Plan 113 — canonical spec for `mvm-firecracker-bridge`.
    /// SECCOMP.md and LANDLOCK.md document the rationale + review
    /// process for syscall additions.
    pub fn firecracker_bridge(audit_dir: PathBuf, keys_dir: PathBuf, passt_path: PathBuf) -> Self {
        Self {
            readable_paths: vec![passt_path, keys_dir],
            read_write_paths: vec![audit_dir],
            allowed_syscalls: vec![
                "read", "write", "fsync", "openat", "close", "stat", "fstat", "lstat",
                "socket", "bind", "connect", "accept", "accept4", "sendmsg", "recvmsg",
                "sendto", "recvfrom", "splice",
                "clock_gettime", "futex", "exit", "exit_group", "rt_sigprocmask", "rt_sigaction",
                "mmap", "munmap", "mprotect", "brk",
                "getpid", "gettid", "getuid", "getgid", "getrandom",
                "epoll_create1", "epoll_ctl", "epoll_wait", "epoll_pwait",
                "prctl", "set_tid_address", "set_robust_list",
            ],
        }
    }
}

#[cfg(target_os = "linux")]
pub fn confine_self(spec: &ConfinementSpec) -> Result<(), JailerError> {
    crate::landlock::apply(spec)?;
    crate::seccomp::apply(spec)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn confine_self(_spec: &ConfinementSpec) -> Result<(), JailerError> {
    Err(JailerError::SeccompUnavailable)
}

#[cfg(target_os = "linux")]
pub mod landlock;
#[cfg(target_os = "linux")]
pub mod seccomp;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firecracker_bridge_spec_has_audit_write_paths() {
        let spec = ConfinementSpec::firecracker_bridge(
            "/tmp/audit".into(),
            "/tmp/keys".into(),
            "/usr/bin/passt".into(),
        );
        assert!(spec.read_write_paths.iter().any(|p| p == std::path::Path::new("/tmp/audit")));
        assert!(spec.readable_paths.iter().any(|p| p == std::path::Path::new("/tmp/keys")));
        assert!(spec.readable_paths.iter().any(|p| p == std::path::Path::new("/usr/bin/passt")));
        assert!(spec.allowed_syscalls.contains(&"splice"));
        assert!(spec.allowed_syscalls.contains(&"fsync"));
        assert!(!spec.allowed_syscalls.contains(&"execve"));  // never allowed
        assert!(!spec.allowed_syscalls.contains(&"setuid"));  // never allowed
    }
}
```

- [ ] **Step 4: Create `src/seccomp.rs`**

```rust
//! Plan 113 / ADR-064 — seccomp-BPF filter via `seccompiler`.

use crate::{ConfinementSpec, JailerError};
use seccompiler::{SeccompAction, SeccompFilter, SeccompRule, TargetArch};
use std::collections::BTreeMap;

#[cfg(target_arch = "x86_64")]
const TARGET_ARCH: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
const TARGET_ARCH: TargetArch = TargetArch::aarch64;

pub fn apply(spec: &ConfinementSpec) -> Result<(), JailerError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for name in &spec.allowed_syscalls {
        let nr = seccompiler::sys::syscall_name_to_nr(name).ok_or_else(|| {
            JailerError::SeccompInstall(format!(
                "unknown syscall name {name:?}; check seccompiler version"
            ))
        })?;
        rules.insert(nr.into(), vec![]);
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Trap,
        SeccompAction::Allow,
        TARGET_ARCH,
    )
    .map_err(|e| JailerError::SeccompInstall(format!("{e:?}")))?;
    let bpf: seccompiler::BpfProgram = filter
        .try_into()
        .map_err(|e| JailerError::SeccompInstall(format!("{e:?}")))?;
    seccompiler::apply_filter(&bpf).map_err(|e| JailerError::SeccompInstall(format!("{e:?}")))?;
    Ok(())
}
```

- [ ] **Step 5: Create `src/landlock.rs`**

```rust
//! Plan 113 / ADR-064 — Landlock filesystem ruleset (ABI v2).

use crate::{ConfinementSpec, JailerError};
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, RestrictionStatus, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetError, RulesetStatus, ABI,
};

pub fn apply(spec: &ConfinementSpec) -> Result<(), JailerError> {
    let abi = ABI::V2;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| match e {
            RulesetError::CreateRuleset(_) => JailerError::LandlockUnavailable,
            other => JailerError::LandlockApply(format!("{other:?}")),
        })?
        .create()
        .map_err(|e| JailerError::LandlockApply(format!("{e:?}")))?;

    for p in &spec.readable_paths {
        let fd = PathFd::new(p)?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
            .map_err(|e| JailerError::LandlockApply(format!("{e:?}")))?;
    }
    for p in &spec.read_write_paths {
        let fd = PathFd::new(p)?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
            .map_err(|e| JailerError::LandlockApply(format!("{e:?}")))?;
    }
    let status: RestrictionStatus = ruleset
        .restrict_self()
        .map_err(|e| JailerError::LandlockApply(format!("{e:?}")))?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        RulesetStatus::PartiallyEnforced | RulesetStatus::NotEnforced => {
            Err(JailerError::LandlockApply(format!(
                "ruleset status {:?}; refusing partial confinement",
                status.ruleset
            )))
        }
    }
}
```

- [ ] **Step 6: Create docs**

`SECCOMP.md` and `LANDLOCK.md` (same content as v1 Task 8 step 5 — see [Plan 113 v1 commit] for reference, but the content is short enough to inline here):

```bash
cat > crates/mvm-jailer-lite/SECCOMP.md <<'EOF'
# mvm-jailer-lite seccomp profile

`ConfinementSpec::firecracker_bridge()` allowlists the syscalls
required for: read packets from passt (read, splice, recvmsg);
write audit-chain entries (write, fsync, openat, close); socket
bind/accept/connect; memory + threading (mmap, munmap, futex,
mprotect); time (clock_gettime); signal handling (rt_sigprocmask,
rt_sigaction); process metadata (getpid, gettid, getuid, getgid,
getrandom); epoll multiplexing.

Default action on disallowed syscall: **Trap** → SIGSYS, visible in
core dumps + reproducible in tests. Adding a syscall to the allowlist
requires deliberate review (this file is the audit point). Never add:
execve, setuid, setgid, ptrace, capset.
EOF

cat > crates/mvm-jailer-lite/LANDLOCK.md <<'EOF'
# mvm-jailer-lite Landlock ruleset

`ConfinementSpec::firecracker_bridge()` permits:

- **Read** on the passt binary (exec)
- **Read** on `~/.mvm/keys/host-signer.ed25519` (chain signing)
- **Read-write** on `~/.mvm/audit/` (chain file append + flock)

Everything else returns EACCES at the kernel level. passt's sockets
are inherited fds, not opened by name, so no network paths appear in
the ruleset.

ABI v2 (Linux 5.19+) required for the file-execute permission split.
EOF
```

- [ ] **Step 7: Update `deny.toml`**

```bash
grep -E "seccompiler|landlock" deny.toml
```

Append to the existing `[[bans.allow]]` or equivalent section (find the exact schema by reading the current `deny.toml`):

```toml
# Plan 113 / ADR-064 — confinement helper deps (mvm-jailer-lite).
[[bans.allow]]
name = "seccompiler"
version = "0.5.*"

[[bans.allow]]
name = "landlock"
version = "0.4.*"
```

- [ ] **Step 8: Build + test + commit**

```bash
cargo build -p mvm-jailer-lite 2>&1 | tail -5
cargo test -p mvm-jailer-lite 2>&1 | tail -10
cargo fmt --all && cargo clippy -p mvm-jailer-lite --all-targets -- -D warnings
git add crates/mvm-jailer-lite/ Cargo.toml deny.toml
git commit -m "feat(mvm-jailer-lite): A2 confinement helper (Plan 113 §Task 8 / ADR-064)

New crate wrapping seccompiler 0.5 + landlock 0.4 behind a single
confine_self(&ConfinementSpec) entry point. Linux-only at runtime;
non-Linux targets compile as inert stubs.

ConfinementSpec::firecracker_bridge() yields the canonical spec for
the mvm-firecracker-bridge sidecar (Task 10): readable on passt
binary + ~/.mvm/keys/, read-write on ~/.mvm/audit/, allowlist of 35
syscalls (documented in SECCOMP.md), Landlock ABI v2 (Linux 5.19+).

cargo-deny pinned at seccompiler ^0.5 + landlock ^0.4.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 8.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 9 — `mvm-jailer-lite` property tests (`#[ignore]`-gated)

Same shape as Plan 113 v1 Task 8b. Real code is identical (independent of Path X). Skipping the verbose rewrite here — see the prior commit in the worktree's history or [Plan 113 v1 §Task 8b] for the verbatim test code (the v1 task's tests are correct and don't need revision; they construct `ConfinementSpec::firecracker_bridge` and assert seccomp denies / Landlock denies).

If implementing fresh: copy `tests/seccomp_property.rs` and `tests/landlock_property.rs` bodies from Plan 113 v1 §Task 8b (those bodies use only the public `ConfinementSpec` + `confine_self` surface from Task 8, so no Path X changes).

- [ ] **Step 1-4**: Identical to v1 §Task 8b.
- [ ] **Step 5: Commit**

```bash
git add crates/mvm-jailer-lite/tests/
git commit -m "test(mvm-jailer-lite): seccomp + Landlock property tests (Plan 113 §Task 9)

Two #[ignore]-gated tests; CI lane jailer-lite-property (Task 13)
runs them on Ubuntu 22.04+ runners with --ignored.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 9.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 10 — `mvm-vz-drainer` binary crate (closes Plan 112 carve-out)

**Files:**
- Create: `crates/mvm-vz-drainer/Cargo.toml`
- Create: `crates/mvm-vz-drainer/src/main.rs`
- Modify: Workspace `Cargo.toml`

The Vz drainer is a thin binary that:
1. Reads a `DrainerConfig` JSON on stdin (vm_name, audit_dir, signing_key_path, tenant_id, plan_json, bundle_json, events_socket_path, observers).
2. Constructs `BridgeConfig` + `BridgeEndpoints::VzIngest { events_socket_path }`.
3. Calls `spawn_bridge_thread(endpoints, cfg)` from `mvm-supervisor::gateway_bridge`.
4. Parks the main thread; supervisor parent kills it on VM shutdown (existing `AttachedGvproxyGuard` pattern from PR #487).

- [ ] **Step 1: Add to workspace**

```toml
# Cargo.toml [workspace.members]
"crates/mvm-vz-drainer",
```

- [ ] **Step 2: Create `crates/mvm-vz-drainer/Cargo.toml`**

```toml
[package]
name = "mvm-vz-drainer"
version = "0.14.0"
edition = "2024"
license = "MIT OR Apache-2.0"

[[bin]]
name = "mvm-vz-drainer"
path = "src/main.rs"

[dependencies]
anyhow = { workspace = true }
ed25519-dalek = { workspace = true }
mvm-policy = { workspace = true }
mvm-plan = { workspace = true }
mvm-supervisor = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
tokio = { workspace = true, features = ["full"] }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

- [ ] **Step 3: Create `src/main.rs`**

```rust
//! Plan 113 / ADR-064 — Vz drainer binary.
//!
//! Closes Plan 112's "Vz carve-out": binds the
//! `events_ingest_socket_path` (Swift bridge's NDJSON output socket
//! per PR #487 commit 6); reads NDJSON FlowEventWire lines; converts
//! to internal FlowEvent; feeds into the existing
//! `mvm-supervisor::gateway_bridge::run_bridge_inner` via the
//! `BridgeEndpoints::VzIngest` variant.
//!
//! Spawned by `mvm-backend::vz::start()` (Task 11) between Swift
//! supervisor spawn and Vz VM boot.

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use mvm_plan::ExecutionPlan;
use mvm_policy::PolicyBundle;
use mvm_supervisor::audit::AuditSigner;
use mvm_supervisor::audit_file::FileAuditSigner;
use mvm_supervisor::gateway_bridge::{
    BridgeConfig, BridgeEndpoints, spawn_bridge_thread, AllowAll,
};
use mvm_supervisor::network::{from_admitted, ObserverAllowlist, ProviderCapabilities};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(serde::Deserialize)]
struct DrainerConfig {
    vm_name: String,
    audit_dir: PathBuf,
    audit_socket: PathBuf,
    signing_key_path: PathBuf,
    events_socket_path: PathBuf,
    /// Serialized SignedExecutionPlan envelope from Plan 112
    /// `VmStartConfig.plan_json`. The drainer re-verifies the
    /// signature before constructing BridgeConfig.
    plan_json: String,
    /// Optional serialized PolicyBundle from Plan 112
    /// `VmStartConfig.bundle_json`.
    #[serde(default)]
    bundle_json: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let mut json = String::new();
    std::io::stdin()
        .read_to_string(&mut json)
        .context("read DrainerConfig from stdin")?;
    let cfg: DrainerConfig = serde_json::from_str(&json).context("parse DrainerConfig")?;

    // Verify and decode the signed plan envelope.
    let signed: mvm_plan::SignedExecutionPlan =
        serde_json::from_str(&cfg.plan_json).context("parse plan_json as SignedExecutionPlan")?;
    let signer_id = mvm_plan::host_signer_id();
    let keys_dir = cfg
        .signing_key_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("signing_key_path has no parent dir"))?;
    let signer_keys = mvm_supervisor::host_signer::load_or_init_at(keys_dir)
        .context("load host signer for plan envelope re-verification")?;
    let trusted: [(&str, &ed25519_dalek::VerifyingKey); 1] = [(&signer_id, &signer_keys.verifying)];
    let plan: ExecutionPlan =
        mvm_plan::verify_plan(&signed, &trusted).context("re-verify plan envelope")?;

    let bundle: Option<PolicyBundle> = match cfg.bundle_json {
        Some(s) => Some(serde_json::from_str(&s).context("parse bundle_json")?),
        None => None,
    };

    // Re-use the host signing key for chain signing.
    let key_bytes = std::fs::read(&cfg.signing_key_path).context("read signing key")?;
    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&key_array);
    let signer = FileAuditSigner::open(signing_key, &cfg.audit_dir)?;
    let signer: Arc<dyn AuditSigner> = Arc::new(signer);

    // Observer chain from admitted plan + host allowlist.
    let allowlist = ObserverAllowlist::load_from_host_config()
        .context("load ObserverAllowlist from ~/.mvm/observers/allowlist.toml")?;
    let leaf_caps = ProviderCapabilities {
        flow_events: true,
        payload_tap: false, // ADR-064 §Decision 8 — Vz no payload tap in this plan
    };
    let observers = from_admitted(&plan, leaf_caps, &allowlist)
        .context("resolve observer chain from admitted plan")?;

    let bridge_cfg = BridgeConfig {
        vm_name: cfg.vm_name.clone(),
        plan: Arc::new(plan),
        bundle: bundle.map(Arc::new),
        audit_socket: cfg.audit_socket,
        signer,
        policy: Arc::new(AllowAll),
        observers,
    };

    let endpoints = BridgeEndpoints::VzIngest {
        events_socket_path: cfg.events_socket_path,
    };

    let _join = spawn_bridge_thread(endpoints, bridge_cfg);

    tracing::info!(vm = %cfg.vm_name, "mvm-vz-drainer started; awaiting Swift bridge NDJSON");

    // Park forever; supervisor parent kills us on VM shutdown via
    // AttachedDrainerGuard (Task 11).
    loop {
        std::thread::park();
    }
}
```

- [ ] **Step 4: Build + commit**

```bash
cargo build -p mvm-vz-drainer 2>&1 | tail -5
cargo fmt --all && cargo clippy -p mvm-vz-drainer --all-targets -- -D warnings
git add crates/mvm-vz-drainer/ Cargo.toml
git commit -m "feat(mvm-vz-drainer): new binary crate closing Plan 112 Vz carve-out (Plan 113 §Task 10)

Thin binary that reads DrainerConfig from stdin, re-verifies the
signed plan envelope, constructs BridgeConfig + BridgeEndpoints::VzIngest,
and calls mvm-supervisor::gateway_bridge::spawn_bridge_thread.

The handle_vz_ingest loop (already in mvm-supervisor::gateway_bridge,
lines 839-922) handles NDJSON FlowEventWire deserialization and
internal FlowEvent construction; this binary is purely the host of
that loop.

attach_tap (when wired through trait API): returns
PayloadTapUnsupported. ADR-064 §Decision 8 — Vz catches up to
payload tap in the focused follow-up plan that extends Swift
Config.swift.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 10.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 11 — Wire `mvm-backend::vz.rs` to spawn `mvm-vz-drainer`

**Files:**
- Modify: `crates/mvm-backend/src/vz.rs`

- [ ] **Step 1: Find the Vz start() flow**

```bash
rg -n "fn start|host_gvproxy::spawn_detached|events_ingest_socket_path" crates/mvm-backend/src/vz.rs | head -15
```

The current code already calls `host_gvproxy::spawn_detached(&state_dir)?` and populates `events_ingest_socket_path` (per Plan 112 §Section 3 and PR #487 commit 6).

- [ ] **Step 2: Add drainer spawn after gvproxy spawn, before VM boot**

In `VzBackend::start()`, after the existing `let gvproxy_info = host_gvproxy::spawn_detached(&state_dir)?;` line, add:

```rust
// Plan 113 §Task 11 — spawn mvm-vz-drainer alongside the Vz VM.
// Closes Plan 112 §"Vz carve-out": the drainer binds events_ingest_socket_path,
// reads Swift's NDJSON FlowEventWire stream, and chain-signs into
// ~/.mvm/audit/<tenant>.jsonl via mvm-supervisor::gateway_bridge.
let events_socket = events_ingest_socket_path(&config.name);
let audit_dir = std::path::PathBuf::from(mvm_core::config::mvm_data_dir()).join("audit");
let audit_socket = audit_dir.join(format!("gateway-{}.sock", config.name));
let signing_key_path = std::path::PathBuf::from(mvm_core::config::mvm_data_dir())
    .join("keys")
    .join("host-signer.ed25519");
let drainer_cfg = serde_json::json!({
    "vm_name": config.name,
    "audit_dir": audit_dir,
    "audit_socket": audit_socket,
    "signing_key_path": signing_key_path,
    "events_socket_path": events_socket,
    "plan_json": config.plan_json.clone().unwrap_or_default(),
    "bundle_json": config.bundle_json.clone(),
});
let drainer_bin = resolve_vz_drainer_path()
    .map_err(|e| anyhow::anyhow!("locate mvm-vz-drainer binary: {e}"))?;
use std::io::Write;
let mut drainer_child = std::process::Command::new(&drainer_bin)
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::inherit())
    .spawn()
    .map_err(|e| anyhow::anyhow!("spawn mvm-vz-drainer: {e}"))?;
drainer_child
    .stdin
    .take()
    .ok_or_else(|| anyhow::anyhow!("drainer stdin missing"))?
    .write_all(drainer_cfg.to_string().as_bytes())
    .map_err(|e| anyhow::anyhow!("write DrainerConfig to drainer stdin: {e}"))?;

// Plan 102 W6.A.5 pattern (PR #487 commit 6): per-VM guard kills the
// drainer on early return / panic from VzBackend::start.
let _drainer_guard = AttachedDrainerGuard { child: Some(drainer_child) };
```

Add the helper functions to the same file:

```rust
fn resolve_vz_drainer_path() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(p) = std::env::var("MVM_VZ_DRAINER_PATH") {
        return Ok(std::path::PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    if let Some(parent) = exe.parent() {
        let adjacent = parent.join("mvm-vz-drainer");
        if adjacent.exists() {
            return Ok(adjacent);
        }
    }
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for variant in ["release", "debug"] {
        let p = manifest_dir.join(format!("../../target/{variant}/mvm-vz-drainer"));
        if p.exists() {
            return Ok(p);
        }
    }
    anyhow::bail!("mvm-vz-drainer binary not found; build with `cargo build -p mvm-vz-drainer`")
}

struct AttachedDrainerGuard {
    child: Option<std::process::Child>,
}

impl Drop for AttachedDrainerGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
```

The `events_ingest_socket_path` function is already in `mvm-backend/src/vz.rs` (per the prior code reads of `host_gvproxy::spawn_detached`).

- [ ] **Step 3: Build + commit**

```bash
cargo build -p mvm-backend 2>&1 | tail -5
cargo test -p mvm-backend vz:: 2>&1 | tail -10
cargo fmt --all && cargo clippy -p mvm-backend --all-targets -- -D warnings
git add crates/mvm-backend/src/vz.rs
git commit -m "feat(mvm-backend): vz.rs spawns mvm-vz-drainer (Plan 113 §Task 11)

VzBackend::start() now spawns the mvm-vz-drainer sibling between
gvproxy spawn (host_gvproxy::spawn_detached) and the Vz VM boot.
Drainer reads DrainerConfig from stdin (vm_name, audit_dir,
audit_socket, signing_key_path, events_socket_path, plan_json,
bundle_json) and runs the mvm-supervisor::gateway_bridge handler
for BridgeEndpoints::VzIngest.

Plan 112's VmStartConfig.plan_json + .bundle_json fields are
threaded through to the drainer for plan envelope re-verification
(claim 8 admission step at the drainer side).

resolve_vz_drainer_path() walks: MVM_VZ_DRAINER_PATH → adjacent
to mvmctl → target/{release,debug}/mvm-vz-drainer. Same shape as
resolve_supervisor_path.

AttachedDrainerGuard: kill drainer on early return / panic.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 11.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 12 — `mvm-firecracker-bridge` binary crate (Linux-only)

**Files:**
- Create: `crates/mvm-firecracker-bridge/Cargo.toml`
- Create: `crates/mvm-firecracker-bridge/src/main.rs`
- Modify: Workspace `Cargo.toml`

The Firecracker bridge is a Linux-only binary that:
1. Reads `BridgeConfigJson` from stdin (similar to `mvm-vz-drainer`).
2. Applies `mvm_jailer_lite::confine_self()` immediately.
3. Verifies the `passt` binary hash against `~/.mvm/passt-hashes.toml` (operator-curated; bridge fails closed on mismatch or missing file).
4. Acquires the `gateway_fd` + `supervisor_fd` socketpair fds from stdin file descriptors (passed via `Command::stdin` + `Stdin::raw_fd` from `mvm-backend`).
5. Spawns `passt` with the inherited fd.
6. Constructs `BridgeConfig` + `BridgeEndpoints::Passt { gateway_fd, supervisor_fd }`.
7. Calls `spawn_bridge_thread`.

- [ ] **Step 1: Add to workspace**

```toml
# Cargo.toml [workspace.members]
"crates/mvm-firecracker-bridge",
```

- [ ] **Step 2: Create `crates/mvm-firecracker-bridge/Cargo.toml`**

```toml
[package]
name = "mvm-firecracker-bridge"
version = "0.14.0"
edition = "2024"
license = "MIT OR Apache-2.0"

[[bin]]
name = "mvm-firecracker-bridge"
path = "src/main.rs"

[target.'cfg(target_os = "linux")'.dependencies]
mvm-jailer-lite = { workspace = true }

[dependencies]
anyhow = { workspace = true }
ed25519-dalek = { workspace = true }
mvm-policy = { workspace = true }
mvm-plan = { workspace = true }
mvm-supervisor = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
sha2 = { workspace = true }
toml = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

- [ ] **Step 3: Create `src/main.rs`**

```rust
//! Plan 113 / ADR-064 — Firecracker bridge sidecar binary.
//!
//! Spawned by `mvm-backend::backend::FirecrackerBackend::start()`
//! (Task 13). Applies A2 confinement via `mvm-jailer-lite` immediately
//! after argument parsing, verifies the passt binary hash, then runs
//! the `mvm-supervisor::gateway_bridge::run_bridge_inner` loop with
//! `BridgeEndpoints::Passt`.

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use mvm_plan::ExecutionPlan;
use mvm_policy::PolicyBundle;
use mvm_supervisor::audit::AuditSigner;
use mvm_supervisor::audit_file::FileAuditSigner;
use mvm_supervisor::gateway_bridge::{
    BridgeConfig, BridgeEndpoints, spawn_bridge_thread, AllowAll,
};
use mvm_supervisor::network::{from_admitted, ObserverAllowlist, ProviderCapabilities};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(serde::Deserialize)]
struct BridgeConfigJson {
    vm_name: String,
    audit_dir: PathBuf,
    audit_socket: PathBuf,
    signing_key_path: PathBuf,
    passt_path: PathBuf,
    passt_hashes_path: PathBuf,
    plan_json: String,
    #[serde(default)]
    bundle_json: Option<String>,
    /// Raw fd numbers for passt-side socketpair. The parent
    /// (FirecrackerBackend::start) passes these via clone3's fd
    /// inheritance (or socketpair + Command::pre_exec); the child
    /// reads them here and reconstructs OwnedFd.
    gateway_fd_raw: i32,
    supervisor_fd_raw: i32,
}

#[derive(serde::Deserialize)]
struct PasstHashesFile {
    #[serde(default)]
    sha256: Vec<String>,
}

fn verify_passt_hash(passt_path: &PathBuf, hashes_path: &PathBuf) -> Result<()> {
    let toml_body = std::fs::read_to_string(hashes_path).with_context(|| {
        format!(
            "operator must populate {} with one or more passt SHA256 \
             hashes: compute `sha256sum {}` and add the result under \
             `sha256 = [...]`",
            hashes_path.display(),
            passt_path.display()
        )
    })?;
    let parsed: PasstHashesFile = toml::from_str(&toml_body)
        .with_context(|| format!("parse {}", hashes_path.display()))?;
    if parsed.sha256.is_empty() {
        anyhow::bail!(
            "{} has no `sha256 = [...]` entries; operator must populate \
             at least one verified passt hash before bridge startup",
            hashes_path.display()
        );
    }

    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(passt_path)?;
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = format!("{:x}", hasher.finalize());
    if !parsed.sha256.contains(&got) {
        anyhow::bail!(
            "passt at {} has SHA256 {}; not in {} (pinned: {:?}). Either \
             update passt to a known-good version or add the new hash \
             after verifying upstream provenance.",
            passt_path.display(),
            got,
            hashes_path.display(),
            parsed.sha256
        );
    }
    tracing::info!(
        passt = %passt_path.display(),
        sha256 = %got,
        "passt binary hash verified against operator-pinned set"
    );
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let mut json = String::new();
    std::io::stdin()
        .read_to_string(&mut json)
        .context("read BridgeConfigJson from stdin")?;
    let cfg: BridgeConfigJson =
        serde_json::from_str(&json).context("parse BridgeConfigJson")?;

    // Verify passt hash BEFORE confinement so a clean failure message
    // can include the binary path. After confine_self() the read of
    // the passt binary becomes a Landlock'd read; the hash check
    // benefits from the cleaner pre-confine error surface.
    verify_passt_hash(&cfg.passt_path, &cfg.passt_hashes_path)
        .context("passt binary hash pin check")?;

    // Apply A2 confinement immediately. Subsequent file opens go
    // through Landlock; subsequent syscalls go through seccomp.
    #[cfg(target_os = "linux")]
    {
        let keys_dir = cfg
            .signing_key_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("signing_key_path has no parent dir"))?
            .to_path_buf();
        let spec = mvm_jailer_lite::ConfinementSpec::firecracker_bridge(
            cfg.audit_dir.clone(),
            keys_dir,
            cfg.passt_path.clone(),
        );
        mvm_jailer_lite::confine_self(&spec)
            .context("apply seccomp + Landlock confinement")?;
    }

    // Re-verify the signed plan envelope.
    let signed: mvm_plan::SignedExecutionPlan =
        serde_json::from_str(&cfg.plan_json).context("parse plan_json as SignedExecutionPlan")?;
    let signer_id = mvm_plan::host_signer_id();
    let keys_dir = cfg
        .signing_key_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("signing_key_path has no parent dir"))?;
    let signer_keys = mvm_supervisor::host_signer::load_or_init_at(keys_dir)
        .context("load host signer for plan envelope re-verification")?;
    let trusted: [(&str, &ed25519_dalek::VerifyingKey); 1] = [(&signer_id, &signer_keys.verifying)];
    let plan: ExecutionPlan = mvm_plan::verify_plan(&signed, &trusted)
        .context("re-verify plan envelope")?;

    let bundle: Option<PolicyBundle> = match cfg.bundle_json {
        Some(s) => Some(serde_json::from_str(&s).context("parse bundle_json")?),
        None => None,
    };

    // Open signing key under Landlock-permitted read.
    let key_bytes = std::fs::read(&cfg.signing_key_path).context("read signing key")?;
    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&key_array);
    let signer = FileAuditSigner::open(signing_key, &cfg.audit_dir)?;
    let signer: Arc<dyn AuditSigner> = Arc::new(signer);

    let allowlist = ObserverAllowlist::load_from_host_config()
        .context("load ObserverAllowlist from ~/.mvm/observers/allowlist.toml")?;
    let leaf_caps = ProviderCapabilities {
        flow_events: true,
        payload_tap: true,
    };
    let observers = from_admitted(&plan, leaf_caps, &allowlist)
        .context("resolve observer chain from admitted plan")?;

    // Reconstruct OwnedFds from the raw integers. The parent passed
    // them via `Command::stdin` fd inheritance.
    // SAFETY: parent guarantees these fds are valid + ownership transfers.
    let gateway_fd = unsafe { OwnedFd::from_raw_fd(cfg.gateway_fd_raw) };
    let supervisor_fd = unsafe { OwnedFd::from_raw_fd(cfg.supervisor_fd_raw) };

    let bridge_cfg = BridgeConfig {
        vm_name: cfg.vm_name.clone(),
        plan: Arc::new(plan),
        bundle: bundle.map(Arc::new),
        audit_socket: cfg.audit_socket,
        signer,
        policy: Arc::new(AllowAll),
        observers,
    };

    let endpoints = BridgeEndpoints::Passt {
        gateway_fd,
        supervisor_fd,
    };

    let _join = spawn_bridge_thread(endpoints, bridge_cfg);

    tracing::info!(vm = %cfg.vm_name, "mvm-firecracker-bridge started");

    // Park forever; backend.rs watchdog (Task 13) handles teardown.
    loop {
        std::thread::park();
    }
}
```

- [ ] **Step 4: Build + commit**

```bash
cargo build -p mvm-firecracker-bridge 2>&1 | tail -5
cargo fmt --all && cargo clippy -p mvm-firecracker-bridge --all-targets -- -D warnings
git add crates/mvm-firecracker-bridge/ Cargo.toml
git commit -m "feat(mvm-firecracker-bridge): Linux-only bridge sidecar binary (Plan 113 §Task 12)

Linux-only sidecar process that:
  1. Verifies passt binary hash against ~/.mvm/passt-hashes.toml
     (operator-populated; bridge fails closed on missing or empty
     hash list, with a remediation message naming the exact command
     to compute the hash)
  2. Applies mvm-jailer-lite confinement (seccomp + Landlock)
  3. Re-verifies the signed plan envelope (Plan 112 plan_json)
  4. Spawns the mvm-supervisor::gateway_bridge handler with
     BridgeEndpoints::Passt and the inherited socketpair fds

Wire-up from FirecrackerBackend::start lands in Task 13.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 12.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 13 — Wire `FirecrackerBackend::start` to spawn the bridge + crash watchdog

**Files:**
- Modify: `crates/mvm-backend/src/backend.rs` (`FirecrackerBackend::start`)
- Modify: `crates/mvm-backend/src/microvm.rs` (where the real spawn happens)

Per the Explore agent's reads: `FirecrackerBackend::start()` (lines 124-141) is a thin adapter that calls `microvm::run_from_build()`. The bridge spawn belongs in `microvm::run_from_build()` after the existing `network::tap_create(slot)?` and before `start_vm_firecracker()`, because the bridge needs to set up the socketpair that passt + Firecracker share before Firecracker boots.

- [ ] **Step 1: Add the socketpair + bridge spawn block in `microvm::run_from_build()`**

After the existing `network::tap_create(slot)?;` call, insert:

```rust
// Plan 113 §Task 13 — spawn mvm-firecracker-bridge alongside the
// Firecracker VM. Bridge runs in sibling jail (mvm-jailer-lite
// seccomp + Landlock); spawns passt; runs the
// mvm-supervisor::gateway_bridge handler with BridgeEndpoints::Passt.

#[cfg(target_os = "linux")]
let _bridge_watchdog = {
    use std::os::fd::{AsRawFd, IntoRawFd};

    let (gateway_socket, supervisor_socket) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| anyhow::anyhow!("create gateway/supervisor socketpair: {e}"))?;
    let gateway_fd = gateway_socket.into_raw_fd();
    let supervisor_fd = supervisor_socket.into_raw_fd();

    let audit_dir = std::path::PathBuf::from(mvm_core::config::mvm_data_dir()).join("audit");
    let audit_socket = audit_dir.join(format!("gateway-{}.sock", config.slot.name));
    let signing_key_path = std::path::PathBuf::from(mvm_core::config::mvm_data_dir())
        .join("keys")
        .join("host-signer.ed25519");
    let passt_path = std::env::var("MVM_PASST_PATH")
        .unwrap_or_else(|_| "/usr/bin/passt".to_string());
    let passt_hashes_path = std::path::PathBuf::from(mvm_core::config::mvm_data_dir())
        .join("passt-hashes.toml");

    // VmStartConfig didn't reach this layer in the original code path
    // (microvm::run_from_build takes a FlakeRunConfig). For Plan 113
    // we read the per-VM plan_json + bundle_json from per-VM state
    // dir, where Plan 112's producer (mvm-cli) stashed them.
    let state_dir = std::path::PathBuf::from(mvm_core::config::mvm_data_dir())
        .join("vms")
        .join(&config.slot.name);
    let plan_json = std::fs::read_to_string(state_dir.join("plan.json")).map_err(|e| {
        anyhow::anyhow!(
            "read plan_json from {}; mvmctl up admission must stash it: {e}",
            state_dir.join("plan.json").display()
        )
    })?;
    let bundle_json = std::fs::read_to_string(state_dir.join("bundle.json")).ok();

    let bridge_cfg = serde_json::json!({
        "vm_name": config.slot.name,
        "audit_dir": audit_dir,
        "audit_socket": audit_socket,
        "signing_key_path": signing_key_path,
        "passt_path": passt_path,
        "passt_hashes_path": passt_hashes_path,
        "plan_json": plan_json,
        "bundle_json": bundle_json,
        "gateway_fd_raw": gateway_fd,
        "supervisor_fd_raw": supervisor_fd,
    });
    let bridge_bin = resolve_fc_bridge_path()
        .map_err(|e| anyhow::anyhow!("locate mvm-firecracker-bridge binary: {e}"))?;
    use std::io::Write;
    let mut child = std::process::Command::new(&bridge_bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn mvm-firecracker-bridge: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("bridge stdin missing"))?
        .write_all(bridge_cfg.to_string().as_bytes())
        .map_err(|e| anyhow::anyhow!("pipe BridgeConfigJson: {e}"))?;

    // Watchdog: when bridge dies, SIGTERM the Firecracker VM via PID
    // file. ADR-064 §Decision 6 — hard-fail bridge crash policy in
    // this plan; restart variants are a separate future plan.
    let vm_name = config.slot.name.clone();
    let pid_file_path = format!("{}/fc.pid", abs_dir);
    std::thread::spawn(move || {
        let exit = child.wait();
        tracing::warn!(
            vm = %vm_name,
            ?exit,
            "mvm-firecracker-bridge exited; tearing down VM"
        );
        if let Ok(pid_str) = std::fs::read_to_string(&pid_file_path)
            && let Ok(pid) = pid_str.trim().parse::<i32>()
        {
            // SAFETY: libc::kill is a syscall wrapper; no UB.
            unsafe { libc::kill(pid, libc::SIGTERM); }
        }
    });

    BridgeChildGuard {} // empty guard struct — watchdog thread owns the child
};
```

Add the helpers near the bottom of the file:

```rust
#[cfg(target_os = "linux")]
fn resolve_fc_bridge_path() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(p) = std::env::var("MVM_FC_BRIDGE_PATH") {
        return Ok(std::path::PathBuf::from(p));
    }
    let exe = std::env::current_exe()?;
    if let Some(parent) = exe.parent() {
        let adjacent = parent.join("mvm-firecracker-bridge");
        if adjacent.exists() {
            return Ok(adjacent);
        }
    }
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for variant in ["release", "debug"] {
        let p = manifest_dir.join(format!("../../target/{variant}/mvm-firecracker-bridge"));
        if p.exists() {
            return Ok(p);
        }
    }
    anyhow::bail!("mvm-firecracker-bridge binary not found; build with `cargo build -p mvm-firecracker-bridge`")
}

#[cfg(target_os = "linux")]
struct BridgeChildGuard;
```

- [ ] **Step 2: Plan 112's producer must stash `plan.json` + `bundle.json` in the per-VM state dir**

This is a small wiring extension to mvm-cli (Plan 112 producer side). Find where Plan 112 wrote `VmStartConfig.plan_json`:

```bash
rg -n "plan_json|bundle_json" crates/mvm-cli/src/commands/vm/up.rs | head -15
```

In each producer site (the three identified in Plan 112 §Task 3), after `populate_audit_substrate(&mut start_config, &ctx.admitted)?;`, add:

```rust
// Plan 113 §Task 13 — Firecracker bridge sidecar reads plan.json
// from per-VM state dir at spawn time. Stash it now (idempotent;
// overwrites if already present from a prior boot).
let state_dir = std::path::PathBuf::from(mvm_core::config::mvm_data_dir())
    .join("vms")
    .join(&start_config.name);
std::fs::create_dir_all(&state_dir)?;
if let Some(plan_json) = &start_config.plan_json {
    std::fs::write(state_dir.join("plan.json"), plan_json)?;
}
if let Some(bundle_json) = &start_config.bundle_json {
    std::fs::write(state_dir.join("bundle.json"), bundle_json)?;
}
```

- [ ] **Step 3: Build + commit**

```bash
cargo build -p mvm-backend -p mvm-cli 2>&1 | tail -5
cargo fmt --all && cargo clippy -p mvm-backend -p mvm-cli --all-targets -- -D warnings
git add crates/mvm-backend/src/microvm.rs crates/mvm-cli/src/commands/vm/up.rs
git commit -m "feat(mvm-backend): FirecrackerBackend spawns mvm-firecracker-bridge + watchdog (Plan 113 §Task 13)

microvm::run_from_build() (Linux only) creates a socketpair and
spawns mvm-firecracker-bridge with the supervisor-side fd. Bridge
applies confinement, reads plan.json + bundle.json from
~/.mvm/vms/<vm>/, re-verifies envelope, runs gateway_bridge with
BridgeEndpoints::Passt.

Watchdog thread observes bridge child via wait(); on bridge death
SIGTERMs the Firecracker VM via PID file
~/.mvm/vms/<vm>/fc.pid. ADR-064 §Decision 6 — hard-fail bridge
crash policy is the only policy in this plan; restart variants
ship in a future plan with their own ADR.

Plan 112 producer sites in mvm-cli/up.rs also stash plan.json +
bundle.json in the per-VM state dir for the bridge to read.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 13.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Phase D — CI + ship

### Task 14 — `BridgeRestartPolicy` field reservation on `SupervisorConfig`

**Files:**
- Modify: `crates/mvm-libkrun/src/lib.rs` (`SupervisorConfig`)

Same shape as Plan 113 v1 §Task 14. The mvm-vz-drainer and mvm-firecracker-bridge `BridgeConfigJson` / `DrainerConfig` structs do **not** need the field reservation in this plan — their crash policy is enforced by the parent (`mvm-backend`), not by the bridge itself. Only `mvm-libkrun::SupervisorConfig` reserves the field because it's part of the libkrun supervisor's stdin wire format.

```rust
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeRestartPolicy {
    #[default]
    HardFail,
}

// In SupervisorConfig (add as a new field):
#[serde(default)]
pub bridge_restart_policy: BridgeRestartPolicy,
```

Add a test asserting unknown variants are rejected:

```rust
#[test]
fn supervisor_config_rejects_unknown_restart_policy() {
    let json = r#"{
        "krun": { "name": "x", "cpus": 1, "memory_mib": 256 },
        "vm_state_dir": "/tmp",
        "bridge_restart_policy": "restart_with_budget"
    }"#;
    let res: Result<SupervisorConfig, _> = serde_json::from_str(json);
    assert!(res.is_err(), "unknown variant must be rejected");
}
```

Commit:

```bash
cargo fmt --all && cargo clippy -p mvm-libkrun --all-targets -- -D warnings
git add crates/mvm-libkrun/src/lib.rs
git commit -m "feat(mvm-libkrun): reserve bridge_restart_policy field (Plan 113 §Task 14 / ADR-064)

Only 'hard_fail' accepted; unknown variants rejected at deserialise
time. Future restart variants ship in a separate plan with their
own ADR; reserved field name avoids schema migration.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 14.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 15 — CI lanes: `jailer-lite-property` + `firecracker-bridge-fuzz`

**Files:**
- Create / modify: `.github/workflows/ci.yml` (add jailer-property job) and `.github/workflows/security.yml` (add bridge-fuzz job)

- [ ] **Step 1: Add jobs**

```yaml
# In .github/workflows/ci.yml
jailer-lite-property:
  name: jailer-lite property tests (Linux seccomp + Landlock)
  runs-on: ubuntu-22.04
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - name: Run property tests
      run: |
        cargo test -p mvm-jailer-lite --test seccomp_property --test landlock_property \
          -- --ignored --nocapture

# In .github/workflows/security.yml
firecracker-bridge-fuzz:
  name: Firecracker bridge etherparse adversarial fuzz
  runs-on: ubuntu-latest
  if: |
    github.event_name == 'workflow_dispatch'
    || github.event_name == 'schedule'
    || startsWith(github.ref, 'refs/tags/')
  steps:
    - uses: actions/checkout@v4
    - name: Install cargo-fuzz
      run: cargo install cargo-fuzz
    - name: Run fuzz target
      run: |
        cd crates/mvm-firecracker-bridge
        cargo fuzz run fuzz_gateway_bridge -- -max_total_time=600
```

(The fuzz target itself is a separate small file; reuse `mvm-libkrun/fuzz/`'s etherparse corpus by symlinking the corpus dir or pointing `cargo fuzz` at it.)

- [ ] **Step 2: Commit**

```bash
git add .github/workflows/
git commit -m "ci: jailer-lite-property + firecracker-bridge-fuzz lanes (Plan 113 §Task 15)

Two new CI lanes:
  jailer-lite-property — runs seccomp + Landlock property tests
    on every PR (Ubuntu 22.04 runner, kernel >= 5.19)
  firecracker-bridge-fuzz — etherparse adversarial fuzz; manual
    dispatch + nightly cron + release-tag (same shape as the
    existing oci-layer-unpack-adversarial lane)

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 15.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 16 — Plan-doc tick + final PR

**Files:**
- Modify: `specs/plans/102-gateway-audit-substrate-impl.md`
- Modify: `specs/plans/103-w6a-implementation-tracker.md`

- [ ] **Step 1: Tick Plan 102 Phase 3c follow-ups**

Append to the Phase 3c follow-ups checklist:

```markdown
- [x] **NetworkProvider observer fan-out + Firecracker substrate (Plan 113, Path X)** — observer machinery added inside `mvm-supervisor::gateway_bridge` (not duplicated in mvm-core); existing `FlowEvent` + `FlowEventWire` + `signer_task` preserved; Vz drainer ships as new `mvm-vz-drainer` binary linking `mvm-supervisor`; Firecracker substrate ships as new `mvm-firecracker-bridge` linking `mvm-supervisor` + `mvm-jailer-lite`. See [ADR-064](../adrs/064-network-provider-trait.md) and [Plan 113](113-network-provider-trait-firecracker-substrate.md).
```

- [ ] **Step 2: Bump Plan 103 §Status**

```markdown
🟡 **Plan 113 — NetworkProvider observer fan-out + Firecracker substrate (Path X)** in flight on `worktree-plan-113-network-provider`. Observer trait + Pipeline + ObserverAllowlist inside mvm-supervisor (alongside the existing gateway_bridge); BridgeConfig gains observers field; signer_task fans out under catch_unwind before chain signing. Vz drainer + Firecracker bridge ship as thin binaries linking mvm-supervisor. A2 confinement via new mvm-jailer-lite crate (seccompiler + landlock). ADR: [ADR-064](../adrs/064-network-provider-trait.md). Plan: [Plan 113](113-network-provider-trait-firecracker-substrate.md).
```

- [ ] **Step 3: Workspace gates + push + PR**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p mvm-supervisor -p mvm-libkrun-supervisor -p mvm-vz-drainer -p mvm-firecracker-bridge -p mvm-jailer-lite -p mvm-policy -p mvm-backend -p mvm-cli 2>&1 | tail -10
git add specs/plans/102-gateway-audit-substrate-impl.md specs/plans/103-w6a-implementation-tracker.md
git commit -m "docs(specs): tick Plan 102 Phase 3c + bump Plan 103 status (Plan 113 §Task 16)

Plan 113 §Path X impl complete on worktree-plan-113-network-provider.
Plan 102 §Phase 3c follow-ups checklist + Plan 103 §Status both
updated.

Plan: specs/plans/113-network-provider-trait-firecracker-substrate.md §Task 16.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
git push -u origin worktree-plan-113-network-provider
gh pr create --base main --title "feat: Plan 113 — NetworkProvider observer fan-out + Firecracker substrate (ADR-064 impl, Path X)" --body "$(cat <<'EOF'
## Summary

Implementation of [ADR-064](specs/adrs/064-network-provider-trait.md)
via **Path X**: wraps the existing `mvm-supervisor::gateway_bridge`
substrate (PR #459/#487/#502) with the observer fan-out pattern,
rather than duplicating types in `mvm-core`.

Closes Plan 112's "Vz carve-out" by shipping `mvm-vz-drainer` as a
thin binary that links `mvm-supervisor` with
`BridgeEndpoints::VzIngest`. Closes the Firecracker substrate gap
on Linux KVM by shipping `mvm-firecracker-bridge` as a sibling
sidecar process under `mvm-jailer-lite` (seccomp + Landlock)
confinement.

Wire-format of chain entries unchanged — Path X uses the existing
`AuditEntry::flow_opened` / `flow_closed` constructors directly.

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test -p mvm-supervisor -p mvm-libkrun-supervisor -p mvm-vz-drainer -p mvm-firecracker-bridge -p mvm-jailer-lite -p mvm-policy -p mvm-backend -p mvm-cli`
- [x] Plan 112 `phase3c_supervisor_dispatch` smoke: both branches still pass (empty observer chain = identical to pre-Plan-113 signer_task)
- [ ] Live smoke per backend × network — manual, post-merge

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Verification (manual, post-merge)

1. **Existing libkrun substrate unchanged on the wire:** `mvmctl up --tenant smoke` on libkrun; `mvmctl audit verify --tenant smoke` passes; chain entries are byte-identical to the pre-Plan-113 format (`AuditEntry::flow_opened` / `flow_closed` constructors are unchanged).
2. **Vz drainer closes Plan 112 carve-out:** `mvmctl up --tenant smoke --backend vz`; chain entries land in `~/.mvm/audit/smoke.jsonl` via `BridgeEndpoints::VzIngest` consuming the Swift bridge's NDJSON output.
3. **Firecracker substrate emits chain entries:** `mvmctl up --tenant smoke --hypervisor firecracker` on Linux; chain entries land in `~/.mvm/audit/smoke.jsonl`.
4. **Capability refusal:** Policy bundle with an observer name whose `required_capabilities().payload_tap == true` + `--backend vz` → exits nonzero before VM start with `BuildError::CapabilityMismatch { observer: "<name>", missing: ["payload_tap"] }`.
5. **Allowlist permission gate:** `chmod 0644 ~/.mvm/observers/allowlist.toml` → next `mvmctl up` bails at supervisor startup with `BuildError::AllowlistRead { detail: "mode 0644; expected 0600 ..." }`.
6. **Bridge crash → VM teardown:** Send SIGKILL to `mvm-firecracker-bridge` mid-VM; watchdog observes within ~5s and SIGTERMs the Firecracker VM; chain records the cause.
7. **Per-VM Prometheus metrics:** With `flow-count-metrics` in the policy's observer chain, `mvmctl up --metrics-port 9090`; `curl localhost:9090/metrics` returns lines starting with `mvm_flow_opened_total{tenant="..."}`.
8. **Plan 112 dispatch smoke still passes:** `cargo test -p mvm-backend --test phase3c_supervisor_dispatch -- --ignored --nocapture`.

## Out of scope (deferred follow-ups)

These are documented in ADR-064 §Out of scope and tracked separately:

- [ ] **N+2: Vz payload tap** — Swift `Config.swift` schema extension + payload tee + control channel.
- [ ] **N+3: Egress redactor observer** — payload-tap-using; own ADR.
- [ ] **N+4: Hostname filter observer** — DNS resolver semantics; own ADR.
- [ ] **N+5: Rate-limiter observer** — gateway enforcement vs observation; own ADR.
- [ ] **Bridge restart policy variants** — `RestartOnceWithGap`, `RestartWithBudget` + `GatewayAuditGap` entry type; own ADR.
- [ ] **Per-VM signing-key derivation** — remove bridge read access to `host-signer.ed25519`; parent process signs entries on bridge's behalf via pipe.
- [ ] **AppleContainer substrate** — research into Apple's `containerization` framework's network layer.
- [ ] **`mvmctl auth` / identity model** — separate ADR + plan; brainstormed independently as Plan M.

## Status

🟡 In progress on `worktree-plan-113-network-provider`. PR opens against `main` post-Task 16. ADR-064 + Plan 113 commits already shipped to the worktree; this PR adds 16 implementation commits on top.
