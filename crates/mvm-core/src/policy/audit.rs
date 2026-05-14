use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::security::{GateDecision, ThreatFinding};

// ============================================================================
// Local mvmctl audit log (single-host operations)
// ============================================================================

/// Default path for the local audit log.
///
/// Prefers XDG state directory (`~/.local/state/mvm/log/`). Falls back to
/// legacy `~/.mvm/log/` if an audit log already exists there.
pub fn default_audit_log() -> String {
    // Check legacy location for backward compat
    let legacy = format!("{}/log/audit.jsonl", crate::config::mvm_data_dir());
    if std::path::Path::new(&legacy).exists() {
        return legacy;
    }
    format!("{}/log/audit.jsonl", crate::config::mvm_state_dir())
}

/// Rotate when the audit log exceeds this size.
const ROTATE_THRESHOLD_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB

/// Categories of local mvmctl operations that are audit-logged.
///
/// Plan 37 §6 invariant: "no unaudited control-plane mutation". Every
/// state-changing CLI verb emits one of these. Pure read-only verbs
/// (status / list / inspect / completions / shell-init) are not
/// audited; everything that mutates host state, registry state, the
/// data dir, the network, secrets, snapshots, signing keys, or the
/// audit log itself must be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalAuditKind {
    VmStart,
    VmStop,
    KeyLookup,
    VolumeCreate,
    VolumeOpen,
    UpdateInstall,
    Uninstall,
    // --- DX features (Phase 2) ---
    NetworkCreate,
    NetworkRemove,
    ImageFetch,
    TemplateBuild,
    TemplatePush,
    TemplatePull,
    ConfigChange,
    ConsoleSessionStart,
    ConsoleSessionEnd,
    // --- MCP server (plan 32 / Proposal A) ---
    /// `tools/call run` invocation — every LLM-driven code execution
    /// against a microVM is auditable.
    McpToolsCallRun,
    /// `tools/call run` failed before completing (orchestration error,
    /// not a non-zero guest exit code).
    McpToolsCallRunError,
    /// MCP session opened — first call with a previously-unseen
    /// `session=ID` parameter (plan 32 / Proposal A.2).
    McpSessionStarted,
    /// MCP session closed by the client (`close: true`) or reaped
    /// by the server (idle / max-lifetime / shutdown drain).
    McpSessionClosed,
    // --- Plan 37 future verbs (B21) -----------------------------
    // These kinds are reserved here so the wire format is stable
    // before the corresponding CLI verbs ship. Each will be emitted
    // by its own command in a subsequent PR. Reserving them now
    // lets the parallel agents working on Wave 2.6 (egress proxy)
    // and Wave 3 (supervisor commands) land their verbs without
    // re-bumping the audit schema.
    /// `mvmctl plan submit <signed-plan>` — admission of a signed
    /// `ExecutionPlan`. Distinct from the supervisor's per-state
    /// `plan.admitted` event (B19): this is the local CLI verb that
    /// hands the plan to the supervisor.
    PlanSubmit,
    /// `mvmctl policy apply <signed-bundle>` — install or replace
    /// the active `PolicyBundle`. Plan 37 §10 / §18.
    PolicyApply,
    /// `mvmctl policy rollback` — flip current/previous bundle slot.
    PolicyRollback,
    /// `mvmctl host trust set` — add or remove a trusted signer key
    /// from the supervisor's trust store. Affects which signed plans
    /// the supervisor will admit.
    HostTrustSet,
    /// `mvmctl supervisor restart` — restart the trusted host-side
    /// supervisor process (plan 37 §7B Zone B).
    SupervisorRestart,
    /// `mvmctl quarantine <workload>` — freeze a running workload.
    /// Distinct from `kill`: quarantined workloads can be resumed
    /// for forensics; killed workloads cannot.
    Quarantine,
    /// `mvmctl kill <workload>` — terminal teardown of a running
    /// workload. Plan 37 Addendum B6.
    Kill,
    /// `mvmctl artifact fetch <plan_id>` — retrieve captured
    /// artifacts from the supervisor's artifact store. Plan 37 §21.
    ArtifactFetch,
    /// `mvmctl wake <workload>` / `mvmctl sleep <workload>` —
    /// supervisor-driven snapshot suspend/resume.
    WorkloadWake,
    WorkloadSleep,
    // --- Egress L7 (plan 34 / ADR-006) ---
    /// Host CA for hypervisor-level L7 egress interception was
    /// rotated. ADR-006 §"Decisions" 7 — rotation is explicit, not
    /// implicit; every rotation lands in the audit log with old +
    /// new fingerprints + the list of VMs whose per-VM leaves were
    /// re-signed. Plan 34 §"Files (summary)".
    EgressCaRotated,
    // --- Lifecycle integrity events ---
    /// `mvmctl build` failed before producing a slot/revision. Paired
    /// with the existing `TemplateBuild` success kind so every build
    /// attempt — success or failure — leaves a single audit line.
    TemplateBuildError,
    /// Snapshot integrity verification failed at resume time. Covers
    /// HMAC tag mismatch (tampered bytes or rotated host key), version
    /// mismatch under strict mode, and lower-level I/O / encoding
    /// failures from `mvm_security::snapshot_hmac::verify`. ADR-007 /
    /// plan 41 W4 — refusing to resume a tampered snapshot is a
    /// security signal and must be auditable.
    SnapshotIntegrityFailed,
    /// Pre-flight verification of a downloaded dev image manifest
    /// failed: cosign signature invalid, manifest version pin off,
    /// `not_after` past, or the published version is on the signed
    /// revocation list. Plan 36 / ADR 005 — every refusal is an
    /// auditable event so an operator can correlate "image rejected
    /// at 14:03" with their CDN logs.
    ImageVerifyFailed,
    // --- Registry / cache mutations (Plan 37 §6 invariant fillers) ---
    /// `mvmctl cache prune` removed temporary files / empty subdirs
    /// from `~/.cache/mvm`. Pure read-only `cache info` is not
    /// audited; the prune verb is, because it deletes host bytes.
    CachePrune,
    /// `mvmctl manifest rm` deleted a registry slot
    /// (`~/.mvm/templates/<slot_hash>/`). Optionally also deleted the
    /// source `mvm.toml` when `--manifest-file` is passed.
    SlotRemove,
    /// Orphan-slot sweep deleted one or more slots whose source
    /// `mvm.toml` no longer exists on disk. Emitted by both
    /// `mvmctl manifest prune --orphans` and
    /// `mvmctl cache prune --orphan-builds`. The detail field carries
    /// the count and (for small sweeps) the truncated slot hashes.
    SlotPrune,
    // --- Sandbox SDK foundation (fs/proc/share/pause/TTL/tags) ---
    // The verbs below are state-changing CLI surfaces added by the
    // sandbox-SDK foundation work. Each kind names a single mutation
    // class; the per-call detail is carried in the audit event's
    // `target` and `detail` fields.
    /// `mvmctl manifest alias set` / unset — registry-level alias
    /// mutation that retargets a friendly name to a different slot.
    ManifestAliasSet,
    ManifestAliasRemove,
    /// `mvmctl manifest tag add` / `tag remove` — adds or removes a
    /// label on a manifest entry.
    ManifestTagAdd,
    ManifestTagRemove,
    /// `mvmctl vm fs <write|delete|mkdir|chmod|chown|...>` — any
    /// guest-filesystem mutation through the FsRpc surface. The
    /// `detail` field carries the operation kind and target path.
    VmFsMutate,
    /// `mvmctl cp <src> <dst>` copied a file across the host/guest
    /// boundary. Detail carries direction, guest path, and byte count;
    /// host paths and file contents are deliberately omitted.
    VmFileCopy,
    /// `mvmctl vm snapshot delete` — removes a saved snapshot from
    /// the host's snapshot store.
    SnapshotDelete,
    /// `mvmctl vm proc start` / `vm proc signal <pid> <sig>` /
    /// `vm proc stdin <pid>` — process control RPC mutations on a
    /// running guest.
    VmProcStart,
    VmProcSignal,
    VmProcStdin,
    /// `mvmctl vm set-ttl` — changes the TTL deadline on a running
    /// VM. The reaper picks up the new deadline on its next tick.
    VmTtlSet,
    /// `mvmctl vm volume add` / `volume remove` — mounts or unmounts
    /// a virtio-fs volume into a running guest. (Plan 45 — rename of
    /// the prior `VmShareAdd` / `VmShareRemove` per Path C; no compat
    /// shim, no behavioural change.)
    VmVolumeAdd,
    VmVolumeRemove,
    // --- Plan 46: metering API ---
    /// One per-minute metering bucket sealed and chained into the
    /// audit log. Plan 46 — auditing-grade resource attribution. The
    /// `detail` field carries a JSON-encoded `MeteringBucket`
    /// (`mvm_core::metering::MeteringBucket`) so a forensic pass can
    /// reconstruct per-tenant resource consumption end-to-end without
    /// trusting the per-tenant JSONL rollup file (which the audit
    /// chain authenticates by sealing each bucket here).
    MeteringEpoch,
    // --- Sprint 52 W2: bundle trust store mutations ---
    //
    // `~/.mvm/trusted-publishers/<key_id>.pub` is the host-trust-
    // boundary state for bundle admission (claim 9). Every add /
    // remove leaves an audit line so a forensics pass can answer
    // "which publishers were trusted at the moment of incident."
    /// `mvmctl trust add <pubkey>` — enrolled a publisher's Ed25519
    /// pubkey in the trust store. Detail: `key_id=<32hex>`.
    TrustAdd,
    /// `mvmctl trust remove <key_id>` — un-enrolled a publisher.
    /// Detail: `key_id=<32hex>`.
    TrustRemove,
    /// `mvmctl bundle install <source>` — verified + atomically
    /// extracted a `.mvmpkg` archive into `~/.mvm/bundles/<sha>/`.
    /// Detail: `bundle_sha256=<64hex>,key_id=<32hex>`. Emitted only
    /// on the success arm; verify failures don't reach the emit.
    BundleInstall,
    /// `mvmctl bundle gc <sha>` or `mvmctl bundle gc --all` —
    /// pruned one or more installed bundles from the registry.
    /// Detail: `removed=<count>,shas=<sha1>[,sha2,...]` (truncated
    /// to the first ~5 shas for sweeps).
    BundleGc,
    /// `mvmctl manifest export-oci <template> --out <path>` —
    /// copied a slot's OCI tarball (produced by `mkGuest`'s
    /// `dockerTools.streamLayeredImage`) to a user-supplied path
    /// so a non-KVM host can `docker load` it. Detail:
    /// `template=<slot>,revision=<rev>,bytes=<size>`.
    ImageExportOci,
    // --- Plan 47: dm-thin storage pool ops ---
    /// `mvmctl storage gc` removed one or more orphaned thin volumes
    /// from the pool. Detail carries the removed volume names (or a
    /// truncated count for large sweeps).
    StorageGc,
    /// `mvmctl sandbox gc --apply` removed stale VM name-registry
    /// records for expired or stopped sandboxes. Detail carries the
    /// removed count and, for small sweeps, the VM names.
    SandboxGc,
    /// Pool-full event surfaced from a clone/snapshot attempt. Detail
    /// carries used + capacity bytes. Operators correlate with their
    /// disk-pressure alerts.
    StoragePoolFull,
    // --- Phase 5 session lifecycle (post-fd-3-fix audit posture) ---
    //
    // Dev sessions hold a long-lived microVM with PTY / shell-exec
    // surface available behind the session id. The session id IS the
    // capability: anyone with read access to
    // `$XDG_RUNTIME_DIR/mvm/sessions/<id>.json` can attach. Every
    // dev-shell entry point therefore lands in the audit log so a
    // forensics pass can reconstruct who-attached-when even after
    // the session record is reaped.
    /// `mvmctl session start <template>` registered a new session.
    /// Detail: `mode=prod|dev,template=<id>,session=<id>`.
    SessionStart,
    /// `mvmctl session attach <id>` dispatched a `RunEntrypoint`
    /// call into an existing session. Detail: `session=<id>`.
    SessionAttach,
    /// `mvmctl session exec <id> -- <argv>` ran an ad-hoc shell
    /// command in a dev session. Detail: `session=<id>` (argv is
    /// **not** logged — could contain user-typed secrets).
    SessionExec,
    /// `mvmctl session run-code <id> <code>` ran user-supplied code
    /// in a dev session. Detail: `session=<id>` (code is **not**
    /// logged — same secrecy concern as exec argv).
    SessionRunCode,
    /// `mvmctl session console <id>` opened an interactive PTY into
    /// a dev session. Pair with `ConsoleSessionEnd` (already
    /// emitted) for the close edge. Detail: `session=<id>`.
    SessionConsoleOpen,
    /// `mvmctl session kill <id>` terminated a session. Detail:
    /// `session=<id>`.
    SessionKill,
    /// `mvmctl session reap` (or the lazy host-side reaper running
    /// inside another session verb) tore down an idle session.
    /// Detail: `session=<id>,idle_timeout_secs=<n>`.
    SessionReap,
}

/// A single local audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAuditEvent {
    pub timestamp: String,
    pub kind: LocalAuditKind,
    pub vm_name: Option<String>,
    pub detail: Option<String>,
}

impl LocalAuditEvent {
    /// Create an event stamped with the current UTC time.
    pub fn now(kind: LocalAuditKind, vm_name: Option<String>, detail: Option<String>) -> Self {
        let timestamp = chrono::Utc::now().to_rfc3339();
        Self {
            timestamp,
            kind,
            vm_name,
            detail,
        }
    }
}

/// Append-only local audit log writer.
pub struct LocalAuditLog {
    path: PathBuf,
}

impl LocalAuditLog {
    /// Open (or create) a local audit log at `path`.
    ///
    /// Creates parent directories if they don't exist.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create audit log dir: {}", parent.display()))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Append one JSONL line.  Rotates to `audit.jsonl.1` when the file
    /// exceeds [`ROTATE_THRESHOLD_BYTES`].
    pub fn append(&self, event: &LocalAuditEvent) -> Result<()> {
        self.maybe_rotate()?;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open audit log: {}", self.path.display()))?;

        let line = serde_json::to_string(event).context("Failed to serialize audit event")?;
        writeln!(file, "{line}").context("Failed to write audit event")?;
        Ok(())
    }

    fn maybe_rotate(&self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let meta = std::fs::metadata(&self.path)
            .with_context(|| format!("Failed to stat {}", self.path.display()))?;
        if meta.len() >= ROTATE_THRESHOLD_BYTES {
            let rotated = self.path.with_extension("jsonl.1");
            std::fs::rename(&self.path, &rotated)
                .with_context(|| format!("Failed to rotate audit log to {}", rotated.display()))?;
        }
        Ok(())
    }
}

/// Convenience macro for the most common audit emit shapes.
///
/// The four arms mirror the chained-builder forms but collapse the
/// `event(LocalAuditKind::Variant)…emit()` boilerplate to a single
/// line:
///
/// ```ignore
/// // bare — no vm_name, no detail
/// audit_emit!(CachePrune);
///
/// // detail via format-string
/// audit_emit!(StorageGc, "count={count}");
/// audit_emit!(SlotPrune, "source={src} count={n}", src = "x", n = 4);
///
/// // vm_name + no detail
/// audit_emit!(SlotRemove, vm: slot_hash);
///
/// // vm_name + format-string detail (positional or named args)
/// audit_emit!(
///     SlotRemove,
///     vm: slot_hash,
///     "manifest_path={} manifest_file_deleted={deleted}",
///     path.display(),
///     deleted = file_was_deleted,
/// );
/// ```
///
/// More elaborate compositions (multiple modifiers, conditional
/// fields) should drop down to [`event`] + [`LocalAuditBuilder`]
/// directly. The macro deliberately doesn't add knobs beyond the
/// four shapes — every existing call site falls into one of them.
#[macro_export]
macro_rules! audit_emit {
    // Bare:  audit_emit!(CachePrune)
    ($variant:ident $(,)?) => {
        $crate::policy::audit::event(
            $crate::policy::audit::LocalAuditKind::$variant
        ).emit()
    };

    // vm_name + format-string detail (positional or named format args):
    //   audit_emit!(SlotRemove, vm: hash, "path={}", p);
    //   audit_emit!(SlotRemove, vm: hash, "k={v}", v = "x");
    ($variant:ident, vm: $vm:expr, $($args:tt)+) => {
        $crate::policy::audit::event(
            $crate::policy::audit::LocalAuditKind::$variant
        )
            .vm_name($vm)
            .detail(format!($($args)+))
            .emit()
    };

    // vm_name only:  audit_emit!(SlotRemove, vm: hash)
    ($variant:ident, vm: $vm:expr $(,)?) => {
        $crate::policy::audit::event(
            $crate::policy::audit::LocalAuditKind::$variant
        )
            .vm_name($vm)
            .emit()
    };

    // Format-string detail (positional or named format args):
    //   audit_emit!(StorageGc, "count={count}");
    //   audit_emit!(SlotPrune, "src={src} count={n}", src = "x", n = 4);
    ($variant:ident, $($args:tt)+) => {
        $crate::policy::audit::event(
            $crate::policy::audit::LocalAuditKind::$variant
        )
            .detail(format!($($args)+))
            .emit()
    };
}

/// Start composing a local audit event.
///
/// Preferred over the positional [`emit`] / [`emit_to`] helpers — chains
/// read top-to-bottom and adding a new optional field (e.g. `outcome`)
/// won't churn every call site. The event lands in
/// [`default_audit_log`] when terminated with [`LocalAuditBuilder::emit`];
/// tests redirect via [`LocalAuditBuilder::to_path`] before terminating.
///
/// ```ignore
/// audit::event(LocalAuditKind::CachePrune)
///     .detail(format!("count={count}"))
///     .emit();
/// ```
pub fn event(kind: LocalAuditKind) -> LocalAuditBuilder {
    LocalAuditBuilder {
        kind,
        vm_name: None,
        detail: None,
        path_override: None,
    }
}

/// Fluent builder for [`LocalAuditEvent`]. Construct with [`event`].
#[must_use = "the audit event isn't written until `.emit()` is called"]
pub struct LocalAuditBuilder {
    kind: LocalAuditKind,
    vm_name: Option<String>,
    detail: Option<String>,
    path_override: Option<PathBuf>,
}

impl LocalAuditBuilder {
    /// Attach a VM identifier to the event. Optional.
    pub fn vm_name(mut self, name: impl Into<String>) -> Self {
        self.vm_name = Some(name.into());
        self
    }

    /// Attach a free-form `detail` string. Conventionally a
    /// space-separated `key=value` list (`source=… count=…`).
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Redirect the emission to an explicit path instead of
    /// [`default_audit_log`]. Used by tests so emission can be
    /// observed without mutating `MVM_STATE_DIR` (which serializes
    /// badly across the test runner).
    pub fn to_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path_override = Some(path.into());
        self
    }

    /// Land the event. Best-effort: open/write failures are logged
    /// via `tracing::warn!` and never propagated — audit failures
    /// must not block the operation being logged.
    pub fn emit(self) {
        let path = self
            .path_override
            .unwrap_or_else(|| PathBuf::from(default_audit_log()));
        let event = LocalAuditEvent::now(self.kind, self.vm_name, self.detail);
        match LocalAuditLog::open(&path).and_then(|log| log.append(&event)) {
            Ok(()) => {}
            Err(e) => tracing::warn!("audit log write failed: {e}"),
        }
    }
}

/// Emit a local audit event to the default log path (best-effort).
///
/// Thin shim over [`event`] / [`LocalAuditBuilder::emit`] kept for
/// existing positional call sites. New code should prefer the builder
/// — the chained form scales to additional optional fields without
/// touching every caller.
pub fn emit(kind: LocalAuditKind, vm_name: Option<&str>, detail: Option<&str>) {
    let mut b = event(kind);
    if let Some(n) = vm_name {
        b = b.vm_name(n);
    }
    if let Some(d) = detail {
        b = b.detail(d);
    }
    b.emit();
}

/// Emit a local audit event to an explicit path (best-effort).
///
/// Thin shim over [`event`] / [`LocalAuditBuilder::to_path`] kept for
/// existing positional call sites. Tests should prefer
/// `event(kind).to_path(p).emit()` for parity with production-path
/// composition.
pub fn emit_to(path: &Path, kind: LocalAuditKind, vm_name: Option<&str>, detail: Option<&str>) {
    let mut b = event(kind).to_path(path.to_path_buf());
    if let Some(n) = vm_name {
        b = b.vm_name(n);
    }
    if let Some(d) = detail {
        b = b.detail(d);
    }
    b.emit();
}

/// Audit event types for per-tenant audit logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuditAction {
    // -- Instance lifecycle --
    InstanceCreated,
    InstanceStarted,
    InstanceStopped,
    InstanceWarmed,
    InstanceSlept,
    InstanceWoken,
    InstanceDestroyed,
    // -- Pool/Tenant --
    PoolCreated,
    PoolBuilt,
    PoolDestroyed,
    TenantCreated,
    TenantDestroyed,
    // -- Operational --
    QuotaExceeded,
    SecretsRotated,
    SnapshotCreated,
    SnapshotRestored,
    SnapshotDeleted,
    TransitionDeferred,
    MinRuntimeOverridden,
    // -- Vsock security (Phase 8) --
    VsockSessionStarted,
    VsockSessionEnded,
    VsockFrameReceived,
    CommandBlocked,
    CommandApproved,
    CommandDenied,
    ThreatDetected,
    RateLimitExceeded,
    SessionRecycled,
}

/// A single audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub tenant_id: String,
    pub pool_id: Option<String>,
    pub instance_id: Option<String>,
    pub action: AuditAction,
    pub detail: Option<String>,
    /// Threat findings from the classifier (empty for non-security events).
    #[serde(default)]
    pub threats: Vec<ThreatFinding>,
    /// Gate decision for command-gated events.
    #[serde(default)]
    pub gate_decision: Option<GateDecision>,
    /// Vsock frame sequence number.
    #[serde(default)]
    pub frame_sequence: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_entry_serialization() {
        let entry = AuditEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            tenant_id: "acme".to_string(),
            pool_id: Some("workers".to_string()),
            instance_id: Some("i-abc123".to_string()),
            action: AuditAction::InstanceStarted,
            detail: Some("pid=12345".to_string()),
            threats: vec![],
            gate_decision: None,
            frame_sequence: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"tenant_id\":\"acme\""));
        assert!(json.contains("\"InstanceStarted\""));
    }

    #[test]
    fn test_audit_entry_no_optionals() {
        let entry = AuditEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            tenant_id: "acme".to_string(),
            pool_id: None,
            instance_id: None,
            action: AuditAction::TenantCreated,
            detail: None,
            threats: vec![],
            gate_decision: None,
            frame_sequence: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"pool_id\":null"));
    }

    #[test]
    fn test_all_audit_actions_serialize() {
        let actions = vec![
            AuditAction::InstanceCreated,
            AuditAction::InstanceStarted,
            AuditAction::InstanceStopped,
            AuditAction::InstanceWarmed,
            AuditAction::InstanceSlept,
            AuditAction::InstanceWoken,
            AuditAction::InstanceDestroyed,
            AuditAction::PoolCreated,
            AuditAction::PoolBuilt,
            AuditAction::PoolDestroyed,
            AuditAction::TenantCreated,
            AuditAction::TenantDestroyed,
            AuditAction::QuotaExceeded,
            AuditAction::SecretsRotated,
            AuditAction::SnapshotCreated,
            AuditAction::SnapshotRestored,
            AuditAction::SnapshotDeleted,
            AuditAction::TransitionDeferred,
            AuditAction::MinRuntimeOverridden,
            AuditAction::VsockSessionStarted,
            AuditAction::VsockSessionEnded,
            AuditAction::VsockFrameReceived,
            AuditAction::CommandBlocked,
            AuditAction::CommandApproved,
            AuditAction::CommandDenied,
            AuditAction::ThreatDetected,
            AuditAction::RateLimitExceeded,
            AuditAction::SessionRecycled,
        ];

        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            assert!(!json.is_empty());
        }
    }

    #[test]
    fn test_audit_entry_backward_compat() {
        // Old-format JSON without new fields should still deserialize
        let json = r#"{
            "timestamp": "2025-01-01T00:00:00Z",
            "tenant_id": "acme",
            "pool_id": null,
            "instance_id": null,
            "action": "TenantCreated",
            "detail": null
        }"#;
        let entry: AuditEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.tenant_id, "acme");
        assert!(entry.threats.is_empty());
        assert!(entry.gate_decision.is_none());
        assert!(entry.frame_sequence.is_none());
    }

    #[test]
    fn test_audit_entry_with_security_fields() {
        use crate::security::{GateDecision, Severity, ThreatCategory, ThreatFinding};

        let entry = AuditEntry {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            tenant_id: "acme".to_string(),
            pool_id: None,
            instance_id: Some("i-001".to_string()),
            action: AuditAction::ThreatDetected,
            detail: Some("classified vsock frame".to_string()),
            threats: vec![ThreatFinding {
                category: ThreatCategory::Destructive,
                pattern_id: "rm_rf_root".to_string(),
                severity: Severity::Critical,
                matched_text: "rm -rf /".to_string(),
                context: "literal match".to_string(),
            }],
            gate_decision: Some(GateDecision::Blocked {
                pattern: "rm -rf /".to_string(),
                reason: "destructive".to_string(),
            }),
            frame_sequence: Some(42),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: AuditEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.threats.len(), 1);
        assert_eq!(parsed.threats[0].category, ThreatCategory::Destructive);
        assert!(parsed.gate_decision.is_some());
        assert_eq!(parsed.frame_sequence, Some(42));
    }

    // -------------------------------------------------------------------------
    // LocalAuditEvent / LocalAuditLog tests
    // -------------------------------------------------------------------------

    #[test]
    fn b21_reserved_audit_kinds_serde_roundtrip() {
        // B21 reserves audit kinds for plan-37 future verbs so the
        // wire format stays stable before each CLI verb ships. This
        // test is the contract — older builds must accept any of
        // these snake_case variants without rejecting them.
        let kinds = vec![
            LocalAuditKind::PlanSubmit,
            LocalAuditKind::PolicyApply,
            LocalAuditKind::PolicyRollback,
            LocalAuditKind::HostTrustSet,
            LocalAuditKind::SupervisorRestart,
            LocalAuditKind::Quarantine,
            LocalAuditKind::Kill,
            LocalAuditKind::ArtifactFetch,
            LocalAuditKind::WorkloadWake,
            LocalAuditKind::WorkloadSleep,
        ];
        for kind in kinds {
            let event = LocalAuditEvent::now(kind.clone(), None, None);
            let json = serde_json::to_string(&event).unwrap();
            let parsed: LocalAuditEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.kind, kind, "kind round-trip diverged: {kind:?}");
        }
    }

    #[test]
    fn b21_audit_kinds_use_snake_case_on_the_wire() {
        // Pin the casing — we don't want a future rename to silently
        // break the audit-stream parser of an older mvmctl reading
        // a newer log.
        let kinds_and_strings = vec![
            (LocalAuditKind::PlanSubmit, "plan_submit"),
            (LocalAuditKind::PolicyApply, "policy_apply"),
            (LocalAuditKind::PolicyRollback, "policy_rollback"),
            (LocalAuditKind::HostTrustSet, "host_trust_set"),
            (LocalAuditKind::SupervisorRestart, "supervisor_restart"),
            (LocalAuditKind::Quarantine, "quarantine"),
            (LocalAuditKind::Kill, "kill"),
            (LocalAuditKind::ArtifactFetch, "artifact_fetch"),
            (LocalAuditKind::WorkloadWake, "workload_wake"),
            (LocalAuditKind::WorkloadSleep, "workload_sleep"),
        ];
        for (kind, expected) in kinds_and_strings {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn test_local_audit_event_serializes() {
        let event = LocalAuditEvent::now(
            LocalAuditKind::VmStart,
            Some("my-vm".to_string()),
            Some("flake=.".to_string()),
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: LocalAuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kind, LocalAuditKind::VmStart);
        assert_eq!(parsed.vm_name.as_deref(), Some("my-vm"));
        assert_eq!(parsed.detail.as_deref(), Some("flake=."));
        assert!(!parsed.timestamp.is_empty());
    }

    #[test]
    fn test_local_audit_kind_all_variants_serialize() {
        let kinds = [
            LocalAuditKind::VmStart,
            LocalAuditKind::VmStop,
            LocalAuditKind::KeyLookup,
            LocalAuditKind::VolumeCreate,
            LocalAuditKind::VolumeOpen,
            LocalAuditKind::UpdateInstall,
            LocalAuditKind::Uninstall,
            LocalAuditKind::NetworkCreate,
            LocalAuditKind::NetworkRemove,
            LocalAuditKind::ImageFetch,
            LocalAuditKind::TemplateBuild,
            LocalAuditKind::TemplatePush,
            LocalAuditKind::TemplatePull,
            LocalAuditKind::ConfigChange,
            LocalAuditKind::ConsoleSessionStart,
            LocalAuditKind::ConsoleSessionEnd,
            LocalAuditKind::McpToolsCallRun,
            LocalAuditKind::McpToolsCallRunError,
            LocalAuditKind::McpSessionStarted,
            LocalAuditKind::McpSessionClosed,
            // B21 reserved future verbs.
            LocalAuditKind::PlanSubmit,
            LocalAuditKind::PolicyApply,
            LocalAuditKind::PolicyRollback,
            LocalAuditKind::HostTrustSet,
            LocalAuditKind::SupervisorRestart,
            LocalAuditKind::Quarantine,
            LocalAuditKind::Kill,
            LocalAuditKind::ArtifactFetch,
            LocalAuditKind::WorkloadWake,
            LocalAuditKind::WorkloadSleep,
            // Plan 34 / ADR-006 egress L7.
            LocalAuditKind::EgressCaRotated,
            // Lifecycle integrity gap-fillers.
            LocalAuditKind::TemplateBuildError,
            LocalAuditKind::SnapshotIntegrityFailed,
            LocalAuditKind::ImageVerifyFailed,
            // Registry / cache mutations.
            LocalAuditKind::CachePrune,
            LocalAuditKind::SlotRemove,
            LocalAuditKind::SlotPrune,
            // Phase 5 session lifecycle.
            LocalAuditKind::SessionStart,
            LocalAuditKind::SessionAttach,
            LocalAuditKind::SessionExec,
            LocalAuditKind::SessionRunCode,
            LocalAuditKind::SessionConsoleOpen,
            LocalAuditKind::SessionKill,
            LocalAuditKind::SessionReap,
            // Sprint 52 W2 trust-store mutations.
            LocalAuditKind::TrustAdd,
            LocalAuditKind::TrustRemove,
            // Sprint 52 W2 bundle registry mutations.
            LocalAuditKind::BundleInstall,
            LocalAuditKind::BundleGc,
            // OCI export follow-on.
            LocalAuditKind::ImageExportOci,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            assert!(!json.is_empty());
        }
    }

    #[test]
    fn lifecycle_gap_kinds_use_snake_case_on_the_wire() {
        // Pin the casing for the new gap-fillers exactly like the B21
        // and egress kinds — the audit log is a stable parsed format
        // for downstream tools (`mvmctl audit`, log shippers).
        let kinds_and_strings = [
            (LocalAuditKind::TemplateBuildError, "template_build_error"),
            (
                LocalAuditKind::SnapshotIntegrityFailed,
                "snapshot_integrity_failed",
            ),
            (LocalAuditKind::ImageVerifyFailed, "image_verify_failed"),
            (LocalAuditKind::CachePrune, "cache_prune"),
            (LocalAuditKind::SlotRemove, "slot_remove"),
            (LocalAuditKind::SlotPrune, "slot_prune"),
            (LocalAuditKind::SessionStart, "session_start"),
            (LocalAuditKind::SessionAttach, "session_attach"),
            (LocalAuditKind::SessionExec, "session_exec"),
            (LocalAuditKind::SessionRunCode, "session_run_code"),
            (LocalAuditKind::SessionConsoleOpen, "session_console_open"),
            (LocalAuditKind::SessionKill, "session_kill"),
            (LocalAuditKind::SessionReap, "session_reap"),
            // Sprint 52 W2 trust-store mutations.
            (LocalAuditKind::TrustAdd, "trust_add"),
            (LocalAuditKind::TrustRemove, "trust_remove"),
            // Sprint 52 W2 bundle registry mutations.
            (LocalAuditKind::BundleInstall, "bundle_install"),
            (LocalAuditKind::BundleGc, "bundle_gc"),
            // OCI export follow-on.
            (LocalAuditKind::ImageExportOci, "image_export_oci"),
        ];
        for (kind, expected) in kinds_and_strings {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn emit_to_writes_a_single_jsonl_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.jsonl");
        emit_to(
            &path,
            LocalAuditKind::SnapshotIntegrityFailed,
            Some("vm-x"),
            Some("variant=tag_mismatch"),
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1, "exactly one line per emit");
        assert!(contents.contains("snapshot_integrity_failed"));
        assert!(contents.contains("vm-x"));
        assert!(contents.contains("tag_mismatch"));
    }

    #[test]
    fn builder_with_no_optionals_matches_legacy_emit() {
        // Builder default state is the same wire shape `emit` produces
        // with `(kind, None, None)` — minus the timestamp string,
        // which is `now()` on each call. The kind + null optionals
        // are what matter for the contract.
        let tmp = tempfile::tempdir().unwrap();
        let p_builder = tmp.path().join("builder.jsonl");
        let p_legacy = tmp.path().join("legacy.jsonl");

        event(LocalAuditKind::CachePrune).to_path(&p_builder).emit();
        emit_to(&p_legacy, LocalAuditKind::CachePrune, None, None);

        let builder_line = std::fs::read_to_string(&p_builder).unwrap();
        let legacy_line = std::fs::read_to_string(&p_legacy).unwrap();
        // Drop the timestamp segment from each so we compare shape, not
        // wall-clock drift between the two `chrono::Utc::now()` calls.
        let strip_ts = |s: &str| -> String {
            // Format: {"timestamp":"…","kind":"…",…}
            let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
            let mut obj = v.as_object().unwrap().clone();
            obj.remove("timestamp");
            serde_json::to_string(&obj).unwrap()
        };
        assert_eq!(strip_ts(&builder_line), strip_ts(&legacy_line));
    }

    #[test]
    fn builder_with_vm_name_and_detail_carries_both_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.jsonl");
        event(LocalAuditKind::VmStop)
            .vm_name("vm-42")
            .detail("source=test")
            .to_path(&path)
            .emit();
        let body = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(v["kind"], "vm_stop");
        assert_eq!(v["vm_name"], "vm-42");
        assert_eq!(v["detail"], "source=test");
    }

    #[test]
    fn audit_emit_macro_all_four_arms_compile() {
        // The macro routes through `default_audit_log()`, which depends
        // on process env vars — driving it from a unit test would need
        // a process-global mutex around `set_var`/`remove_var`. Skip
        // the file-contents check here: end-to-end validation lives in
        // `tests/audit_emissions_live.rs`, where each migrated call
        // site is exercised via a subprocess with hermetic HOME.
        //
        // What this test buys: a compile-time shape check that all
        // four arms expand cleanly against `LocalAuditKind` variants
        // and the surrounding builder API. Renaming a variant or
        // changing `LocalAuditBuilder`'s signature surfaces here as a
        // compile error before the migration sites notice.
        if false {
            crate::audit_emit!(CachePrune);
            crate::audit_emit!(StorageGc, "count={n}", n = 3);
            let hash = String::from("deadbeef");
            crate::audit_emit!(SlotRemove, vm: hash.clone());
            crate::audit_emit!(SlotRemove, vm: hash, "path={p}", p = "/x");
        }
    }

    #[test]
    fn builder_terminator_required_must_use_emit() {
        // Compile-time check that `#[must_use]` on the builder
        // surfaces a warning if a caller forgets `.emit()`. We can't
        // assert the warning in unit tests; this test just shows that
        // the type is usable in the documented chained form so a
        // refactor that breaks the API surfaces as a compile error.
        let _b = event(LocalAuditKind::CachePrune).detail("noop");
        // intentionally not calling `.emit()` — confirms the dead
        // builder is the lint surface, not a runtime panic.
    }

    #[test]
    fn test_egress_ca_rotated_uses_snake_case_rename() {
        // Pin the wire form so a future rename can't silently drift the
        // audit log shape — downstream parsers (`mvmctl audit`,
        // out-of-band log shippers) match on the literal string.
        let json = serde_json::to_string(&LocalAuditKind::EgressCaRotated).unwrap();
        assert_eq!(json, "\"egress_ca_rotated\"");
    }

    #[test]
    fn test_local_audit_log_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.jsonl");

        let log = LocalAuditLog::open(&path).unwrap();
        let event = LocalAuditEvent::now(LocalAuditKind::VmStop, Some("vm1".to_string()), None);
        log.append(&event).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("vm_stop"));
        assert!(contents.contains("vm1"));
        // One line per event.
        assert_eq!(contents.lines().count(), 1);

        // Append a second event.
        let event2 = LocalAuditEvent::now(
            LocalAuditKind::UpdateInstall,
            None,
            Some("v1.2.3".to_string()),
        );
        log.append(&event2).unwrap();
        let contents2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents2.lines().count(), 2);
    }

    #[test]
    fn test_local_audit_log_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.jsonl");

        // Write a file that exceeds the rotation threshold.
        let big_content = "x".repeat(ROTATE_THRESHOLD_BYTES as usize + 1);
        std::fs::write(&path, big_content).unwrap();

        let log = LocalAuditLog::open(&path).unwrap();
        let event = LocalAuditEvent::now(LocalAuditKind::Uninstall, None, None);
        log.append(&event).unwrap();

        // The rotated file should exist.
        let rotated = path.with_extension("jsonl.1");
        assert!(rotated.exists(), "rotation file should be created");

        // The new log file should contain only the new event.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("uninstall"));
    }
}
