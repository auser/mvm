# mvm Sprint 2: Production Readiness

Previous sprint: [SPRINT-1-foundation.md](sprints/SPRINT-1-foundation.md) (complete, merged to main)

Sprint 1 delivered the full foundation: multi-tenant object model, lifecycle API, networking, security hardening, sleep/wake, reconcile loop, QUIC+mTLS daemon, and CI/CD. Sprint 2 focuses on making mvm production-ready.

---

## Phase 1: End-to-End Integration Testing
**Status: PENDING**

Run the full workflow against a real Lima VM to validate the happy path:

- [ ] Bootstrap on fresh macOS (Lima + FC install)
- [ ] Tenant create/list/info/destroy with real network config
- [ ] Pool create/build with the built-in Nix flake (minimal profile)
- [ ] Instance create/start/ssh/stop/destroy lifecycle
- [ ] Sleep/wake round-trip with snapshot verification
- [ ] Agent serve + QUIC client test (send NodeInfo request, verify response)
- [ ] Bridge verify produces clean BridgeReport
- [ ] Fix any issues discovered during integration

## Phase 2: Observability & Logging
**Status: PENDING**

Replace ad-hoc `eprintln!` with structured logging:

- [ ] Add `tracing` + `tracing-subscriber` crates
- [ ] Instrument all lifecycle operations with spans
- [ ] Structured JSON log output for agent daemon mode
- [ ] Request-level tracing in QUIC handler (request type, latency, outcome)
- [ ] Prometheus-style metrics endpoint (instance counts, request latency, error rates)

## Phase 3: CLI Polish & UX
**Status: PENDING**

- [ ] `mvm instance stats` — real metrics from FC API + cgroup (not just stubs)
- [ ] `mvm pool info` — show current revision, instance counts by status, resource usage
- [ ] `mvm tenant info` — show quota usage, pool list, network config
- [ ] `mvm agent status` — show daemon health, uptime, last reconcile result
- [ ] Progress bars for long operations (bootstrap, pool build)
- [ ] Colorized table output for list commands
- [ ] `--output table|json|yaml` flag for all list/info commands

## Phase 4: Error Handling & Resilience
**Status: PENDING**

- [ ] Retry logic for transient failures (FC API socket not ready, network timeouts)
- [ ] Graceful degradation when Lima VM is not running
- [ ] Stale PID detection (FC process died but state says Running)
- [ ] Orphan cleanup: detect instances with no parent pool/tenant
- [ ] Config validation on load (reject corrupt/incomplete state files)
- [ ] Audit log rotation (cap file size, compress old entries)

## Phase 5: Coordinator Client
**Status: PENDING**

Build the QUIC client side for multi-node fleet management:

- [ ] `mvm coordinator` CLI subcommand
- [ ] `coordinator push --desired desired.json --node <addr>` — send desired state to agent
- [ ] `coordinator status --node <addr>` — query node info + stats
- [ ] `coordinator list-instances --node <addr> --tenant <id>` — query instances
- [ ] `coordinator wake --node <addr> --tenant <t> --pool <p> --instance <i>` — urgent wake
- [ ] Connection pooling for multi-node operations
- [ ] Parallel push to multiple nodes

## Phase 6: Performance & Resource Optimization
**Status: PENDING**

- [ ] Lazy Lima VM startup (don't boot until first operation that needs it)
- [ ] Connection keep-alive for QUIC (reuse connections across requests)
- [ ] Parallel instance operations (start/stop multiple instances concurrently)
- [ ] Disk space management (cleanup old revisions, track total disk usage)
- [ ] Memory-mapped rootfs for faster instance boot
- [ ] Warm pool pre-allocation (boot instances before they're needed)

## Phase 7: Documentation & Examples
**Status: PENDING**

- [ ] User guide: writing custom Nix flakes for mvm
- [ ] Example: web server fleet (nginx + app instances)
- [ ] Example: CI runner pool (ephemeral build workers)
- [ ] Troubleshooting guide (common errors, how to debug)
- [ ] API reference for desired state JSON schema
- [ ] Architecture decision records (ADRs) for key design choices
