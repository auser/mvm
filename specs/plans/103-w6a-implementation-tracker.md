# Plan 103 — W6.A gateway audit substrate (implementation tracker)

Implementation tracker for [Plan 102](102-gateway-audit-substrate-impl.md)
W6.A — gateway audit substrate plumbing. Single-source-of-truth
progress checklist for the 9-commit PR landing the no-bypass,
observable, mediable substrate across all three backends
(libkrun+passt, libkrun+gvproxy, Vz+gvproxy).

## Spec + design references

- [Plan 102](102-gateway-audit-substrate-impl.md) — design contract
  (W6.A substrate plumbing + W6.B real flow extraction)
- [Plan 101](101-in-guest-volume-encryption-and-gateway-audit.md) —
  parent plan (claim 10 leg 2 wave map)
- [ADR-058](../adrs/058-claim-10-bytes-leaving-trust-boundary.md) —
  threat model for claim 10
- [ADR-002](../adrs/002-microvm-security-posture.md) — claim list
  (extended to include claim 10)
- [SPRINT.md](../SPRINT.md) — Sprint 56 §W3 sprint slot

## Substrate guarantees this PR commits to

1. **No bypass.** Every byte traverses an auditable bridge. TSI
   removed; supervisor admission refuses non-bridged net configs.
2. **Observable.** Every flow open/close emits a signed event into
   the per-tenant chain + live subscribers see NDJSON.
3. **Mediable.** `FlowPolicy` hook trait at the bridge layer with
   `AllowAll` default; Plan 74 / SNI / L7 plug in later.
4. **Per-tenant isolated.** No cross-tenant coupling introduced.

## Pre-flight

- [x] `gh pr view 452` — merged 2026-05-26
- [x] `git worktree list` + `gh pr list --state open` — no
      collisions on TSI removal or Plan 102 area
- [x] Worktree created at
      `.claude/worktrees/plan-101-w6a-substrate/` off `origin/main`
      (5ea4b84f)
- [ ] Read `crates/mvm-libkrun/src/sys.rs` — confirm
      `add_net_unixstream_fd` + `add_net_unixgram_path` dup
      semantics
- [ ] Read `crates/mvm-supervisor/src/audit.rs` — `AuditEntry` /
      `AuditSigner` shape
- [ ] Read `crates/mvm-supervisor/src/audit_recorder.rs` —
      unified `Recorder` substrate (pick emission path)
- [ ] Read `crates/mvm-core/src/config.rs:62-117` —
      `mvm_data_dir()` + `ensure_private_dir()`
- [ ] Read `crates/mvm-vz-supervisor/Package.swift` + `Sources/` —
      Swift `socketpair` + `XCTest` async patterns
- [ ] Lock grep list for TSI removal:
      `tsi` / `Tsi` / `TSI` / `MVM_NETWORKING`
- [ ] `mvmd` repo state confirmed clean before Phase 6 write

## Phase 6 — mvmd cross-repo spec (first action after this commit)

- [ ] Pick next free plan number in
      `tinylabscom/mvmd/specs/plans/` (current latest = 45)
- [ ] Write `tinylabscom/mvmd/specs/plans/<NN>-network-manager.md`
      with the verbatim content in Plan 102's working tracker
- [ ] Commit + push in mvmd repo
- [ ] Update mvmd `specs/SPRINT.md` if a sprint slot is open

## Commit sequence

### Commit 0 — this file (tracker setup)

- [x] Plan 103 tracker file written
- [x] SPRINT.md Sprint 56 §W3 updated with Plan 103 reference
- [x] SPRINT.md Sprint 56 §W3 §"Out of scope for this sprint"
      extended with 4 ADR-058 items
- [x] SPRINT.md Sprint 56 §Non-goals (explicit) refined
- [ ] Commit + push the worktree branch (initial PR open)

### Commit 1 — Kill TSI

- [ ] `crates/mvm-build/src/libkrun_builder.rs:134-207` —
      remove `NetworkingPreference::Tsi` variant
- [ ] Remove `=tsi` arm in `resolve_networking_mode`
- [ ] Remove default-Tsi dispatch arm
- [ ] `crates/mvm-cli/src/doctor.rs` — hard error if no gateway
      binary locatable
- [ ] Docs sweep:
      - [ ] `CLAUDE.md` (TSI paragraph)
      - [ ] `specs/adrs/055-passt-virtio-net.md` (§"TSI escape hatch")
      - [ ] `specs/plans/72-builder-vm-via-libkrun.md`
      - [ ] `specs/plans/76-secure-fast-boot-and-dx.md`
      - [ ] `specs/plans/88-gvproxy-macos-backend.md`
      - [ ] `specs/plans/97-vz-backend.md`
- [ ] Delete tests touching `MVM_NETWORKING=tsi` (list in PR
      description)
- [ ] New test: `tsi_no_longer_resolvable` (parse `=tsi` → error)
- [ ] `cargo test --workspace` green

### Commit 2 — `FileAuditSigner` cross-process flock

- [ ] `crates/mvm-supervisor/src/audit_file.rs:125-172` —
      wrap `sign_and_emit` body in
      `rustix::fs::flock(fd, LockExclusive)`
- [ ] Re-read chain tail under the lock to refresh cursor
- [ ] In-memory `cursors: Mutex<HashMap>` becomes fast-path hint
- [ ] Test: `flock_serializes_concurrent_writers`
- [ ] Test: `two_processes_writing_same_tenant_produce_valid_chain`
      (fork + exec helper writes N entries, parent writes M
      interleaved, `verify_audit_chain` returns N+M)
- [ ] `cargo test --workspace` green

### Commit 3 — Chain enum (`AuditAction::Flow*`)

- [ ] `crates/mvm-core/src/policy/audit.rs:679` —
      add `AuditAction::FlowOpened { flow_id, direction }`
- [ ] Add `AuditAction::FlowClosed { flow_id, direction, reason }`
- [ ] Add `pub enum FlowDirection { Egress, Ingress }`
- [ ] Add `pub enum FlowCloseReason { Eof, BridgeError,
      PolicyDropped, Shutdown }`
- [ ] Test: `flow_audit_actions_serde_roundtrip` (mirror
      `test_all_audit_actions_serialize` at line 776)
- [ ] `cargo test --workspace` green

### Commit 4 — `mvm-supervisor::gateway_audit` (subscriber sink)

- [ ] Create `crates/mvm-supervisor/src/gateway_audit.rs`
- [ ] `pub struct GatewayAuditSink { listener, tx: broadcast }`
- [ ] `pub fn bind(path: &Path)` — pre-unlinks stale path, chmods
      0700, ensures parent dir 0700
- [ ] `pub fn subscribe() -> broadcast::Receiver<String>`
- [ ] `pub async fn run(self) -> !` — accept loop, per-subscriber
      forward task
- [ ] Bounded broadcast(256), drop-oldest, `Lagged` → drop
      subscriber
- [ ] Test: `bind_pre_unlinks_stale_socket`
- [ ] Test: `accepts_many_subscribers_fans_out_jsonl`
- [ ] Test: `slow_subscriber_drops_then_recovers`
- [ ] `cargo test --workspace` green

### Commit 5 — `mvm-supervisor::gateway_bridge` + `FlowPolicy` hook

- [ ] Create `crates/mvm-supervisor/src/gateway_bridge.rs`
- [ ] `pub trait FlowPolicy: Send + Sync { fn evaluate(...) }`
- [ ] `pub enum FlowAction { Allow, Drop { reason } }`
- [ ] `pub struct FlowDecisionCtx` with `dest_ip`, `dest_port`,
      `sni_hostname`, `url_path` (all `Option` — forward-compat)
- [ ] `pub struct AllowAll; impl FlowPolicy for AllowAll`
- [ ] `pub enum BridgeEndpoints { Passt, LibkrunGvproxy, VzIngest }`
- [ ] `pub struct BridgeConfig { vm_name, tenant_id, audit_socket,
      events_ingest_socket, signer, policy }`
- [ ] `pub fn spawn_bridge_thread(endpoints, cfg) -> JoinHandle<()>`
- [ ] Dedicated `std::thread` + current-thread tokio runtime +
      `LocalSet` running bridge / signer / sink tasks
- [ ] **Passt bridge**: `tokio::io::copy_bidirectional` between
      `UnixStream`s; first-byte-per-direction tracking with
      `AtomicBool`; Drop guard emits `FlowClosed { BridgeError }`
- [ ] **LibkrunGvproxy bridge**: bind `UnixDatagram` at
      `supervisor_listen_path`; second `UnixDatagram` `connect()`s
      to gvproxy; cache libkrun's autobind peer addr on first
      packet (`recv_from`); shuffle both directions
- [ ] **VzIngest bridge**: bind `UnixListener` at
      `events_socket_path`; accept one connection; magic-byte
      handshake `MVM_VZ_BRIDGE_V1\n` (reject mismatch); reject
      second connection with `EBUSY`; drain NDJSON into mpsc
- [ ] Per-flow `cfg.policy.evaluate(&ctx)` call (W6.A: always
      Allow via AllowAll)
- [ ] Bridge `send().await` on mpsc — backpressure to guest, no
      drop
- [ ] Bridge thread panic → `std::process::exit(1)`
- [ ] Tests:
      - [ ] `passt_bridge_emits_open_close_pair`
      - [ ] `gvproxy_dgram_shuffle_preserves_boundaries`
      - [ ] `vz_ingest_drains_ndjson_into_signer`
      - [ ] `vz_ingest_rejects_missing_handshake`
      - [ ] `vz_ingest_rejects_second_connection_with_ebusy`
      - [ ] `signer_task_sole_writer_under_concurrent_events`
      - [ ] `bridge_panic_exits_process_nonzero`
      - [ ] `allow_all_policy_lets_all_flows_through`
      - [ ] `policy_drop_emits_flowclosed_with_policy_dropped_reason`
      - [ ] `gateway_child_does_not_inherit_audit_socket_fds`
            (CLOEXEC check)
- [ ] `cargo test --workspace` green

### Commit 6 — `run_supervisor_with_bridge` + no-bypass admission

- [ ] `crates/mvm-libkrun/src/lib.rs` `SupervisorConfig`:
      `network: NetworkConfig` (mandatory; was Option)
- [ ] Add `pub tenant_id: TenantId`
- [ ] Add `pub audit_dir: PathBuf`
- [ ] Add `pub gateway_audit_socket: PathBuf`
- [ ] Add `pub gateway_events_socket: PathBuf`
- [ ] Add `pub signing_key_path: PathBuf`
- [ ] `pub fn run_supervisor_with_bridge<F>(cfg, factory)`
- [ ] Admission validation:
      - [ ] `signing_key_path` canonicalizes under `~/.mvm/keys/`
      - [ ] `gateway_audit_socket` + `gateway_events_socket`
            parent must be `~/.mvm/audit/`
      - [ ] `tenant_id` non-empty
      - [ ] No-bypass guard (NetworkConfig has no `None`/`Disabled`
            variant; refuses configs without bridge endpoint)
- [ ] passt path: pre-net + `into_socket()` + inner socketpair +
      hand to factory
- [ ] gvproxy path: spawn gvproxy + bind supervisor listen path +
      `add_net_unixgram_path(supervisor_listen)` + hand to factory
- [ ] `crates/mvm-libkrun/src/bin/mvm-libkrun-supervisor.rs:104`
      — swap `run_supervisor` for `run_supervisor_with_bridge`
- [ ] Confirm `add_net_unixstream_fd` + `add_net_unixgram_path`
      dup semantics (pre-flight read)
- [ ] Backend orchestrator (`crates/mvm-backend/src/libkrun.rs`)
      populates new fields from `ExecutionPlan` + `mvm_data_dir()`
- [ ] Tests:
      - [ ] `supervisor_admission_refuses_config_without_network`
      - [ ] `signing_key_path_outside_mvm_keys_rejected`
- [ ] `cargo test --workspace` green

### Commit 7 — Vz Swift bridge + Rust ingest + backend orchestrator

- [ ] `crates/mvm-vz/src/lib.rs` `NetworkConfig::Gvproxy` — add
      `events_ingest_socket_path: PathBuf`
- [ ] `crates/mvm-vz-supervisor/Sources/mvm-vz-supervisor/Network.swift`
      `makeGvproxyDevice` rewrite:
      - [ ] Open SOCK_DGRAM `socketpair`
      - [ ] One half attached to Vz via
            `VZFileHandleNetworkDeviceAttachment`
      - [ ] Second SOCK_DGRAM `connect()`s to gvproxy `socketPath`
      - [ ] SOCK_STREAM `connect()`s to `eventsIngestSocketPath`,
            sends `MVM_VZ_BRIDGE_V1\n` handshake first
      - [ ] `Task.detached` two shuffle loops with first-packet
            FlowOpened emit + EOF/shutdown FlowClosed emit
      - [ ] Task cancellation → final
            `FlowClosed { reason: "shutdown" }`
- [ ] `crates/mvm-backend/src/vz.rs:809-812` — populate
      `network` with `NetworkConfig::Gvproxy { socket_path, mac,
      events_ingest_socket_path }`
- [ ] `spawn_gvproxy_for_vz` shim reusing
      `mvm_libkrun::gvproxy::spawn`
- [ ] Swift XCTest:
      - [ ] `testGvproxyBridgeShufflesDatagrams`
      - [ ] `testEmitsFlowOpenedOnFirstPacket`
      - [ ] `testEmitsFlowClosedOnShutdown`
      - [ ] `testIngestHandshakeSentFirst`
      - [ ] `testIngestReconnect`
- [ ] `cargo test --workspace` green
- [ ] `swift test` (mvm-vz-supervisor) green

### Commit 8 — ADR-058 + Plan 102 + ADR-055 + SPRINT.md amendments

- [ ] `specs/adrs/058-claim-10-bytes-leaving-trust-boundary.md`
      `### Leg 2` — append no-bypass invariant, coverage vs.
      capture, mediable substrate, W6 scope clarification
- [ ] `specs/adrs/058-...` `## Out of scope` — append 5 new
      items (east-west, per-byte capture default, cross-tenant
      mgmt, L7 URL inspection, DoH bypass)
- [ ] `specs/adrs/055-passt-virtio-net.md` — drop §"TSI escape
      hatch", replace with pointer to ADR-058 no-bypass invariant
- [ ] `specs/plans/102-gateway-audit-substrate-impl.md` —
      `### deferred follow-ups` becomes two subsections (W6.B
      follow-ups + Adjacent new plans)
- [ ] SPRINT.md Sprint 56 W3 status updated to reflect W6.A
      impl in flight (Plan 103 reference already in commit 0;
      this is the "shipped" flip when PR merges)

### Commit 9 — PR description + open

- [ ] Live smoke recipe per backend in PR description
- [ ] No-bypass canary documented
- [ ] Tenant isolation rationale table
- [ ] W6.B follow-ups + Adjacent new plans flagged
- [ ] PR description references Plan 103 tracker
- [ ] `gh pr create` opens PR against `main`

## Workspace gates (must all pass before PR open)

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `swift test` (mvm-vz-supervisor)

## Live smoke (per backend; acceptance gates)

- [ ] (a) Linux + libkrun + passt: `nc -U` shows FlowOpened +
      FlowClosed after guest curl
- [ ] (b) macOS + libkrun + gvproxy: same
- [ ] (c) macOS + Vz + gvproxy: same
- [ ] No-bypass canary: `MVM_NETWORKING=tsi` fails with
      "TSI mode is no longer supported."
- [ ] Audit chain integrity: `mvmctl audit verify` exit 0
- [ ] Cross-process flock canary: two VMs same tenant; verify
      exit 0
- [ ] Tamper canary: byte-flip chain → verify exit nonzero

## W6.B follow-ups (next PR; tracked here for visibility)

Mirrored in Plan 102 `### deferred follow-ups #### W6.B
follow-ups` per commit 8.

- [ ] Flow flooding DoS: per-instance rate cap (1000/sec) +
      `FlowFlood` aggregation events for excess
- [ ] Parser fault containment: `std::panic::catch_unwind` around
      etherparse; emit `GatewayAuditFault` on panic; degrade flow
      to pass-through splice
- [ ] Bounded flow table (4096 active flows / instance) with
      oldest-idle eviction and `FlowEvicted` emission
- [ ] Real per-direction byte counters in `FlowClosed` (from
      parsed frame sizes)
- [ ] Parser fuzz target
      `crates/mvm-libkrun/fuzz/fuzz_gateway_bridge.rs` seeded
      from real Ethernet captures
- [ ] Rust→Swift drop command channel for Vz mediation
      actuation
- [ ] Broadcast/ARP/DHCP noise distinguishing — emit
      `FrameOther` for non-IP frames; drop from main per-flow
      event stream
- [ ] Spurious vfkit-handshake `FlowOpened` on passt ingress —
      parser-aware fix

## Adjacent new plans (separate work streams)

Mirrored in Plan 102 `### deferred follow-ups #### Adjacent new
plans` per commit 8.

- [ ] Move gvproxy spawn helper out of mvm-libkrun
      (mvm-providers or new mvm-gateways crate)
- [ ] New plan: SNI inspector in gateway bridge (hostname-level
      allowlist without MITM)
- [ ] Plan 34 Phase 2: finalize TLS MITM in L7EgressProxy with
      workload-CA trust (URL-path allowlist for HTTPS)
- [ ] Plan 74 follow-up: mandatory-deny well-known DoH endpoints
      (1.1.1.1:443, dns.google, etc.)
- [ ] gvproxy supply-chain hardening: pin version+SHA in doctor;
      vendoring evaluation
- [ ] **mvmd-network-manager** cross-repo plan written in
      `tinylabscom/mvmd/specs/plans/` (per-tenant gateway pool,
      egress quotas, tenant-level rollup)

## Status

🟡 in progress — Plan 103 tracker filed 2026-05-26.
Implementation worktree at
`.claude/worktrees/plan-101-w6a-substrate/`.
