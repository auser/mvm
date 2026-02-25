1Claude Prompts (copy-paste for each task)
Start a new Claude Code session for each task. Each prompt is self-contained.

Phase 1: Code Gaps
1a. Template cache key:


In mvm-core/src/template.rs, add a `cache_key() -> String` method to `TemplateRevision` that computes sha256 of (flake_lock_hash + profile + role). Then in mvm-build/src/template_reuse.rs, update reuse_template_artifacts() to compare cache keys before copying artifacts — if the cache key doesn't match, don't reuse. Add a unit test showing two revisions with same flake but different profiles produce different cache keys. Run clippy and tests when done.
1b. Wire Etcd config:


In crates/mvm-coordinator/src/config.rs, add `etcd_endpoints: Option<Vec<String>>` and `etcd_prefix: Option<String>` fields to CoordinatorConfig (with #[serde(default)]). Then in crates/mvm-coordinator/src/server.rs at line 32-35 where it says "TODO: Add config support for EtcdStateStore", make it conditional: if etcd_endpoints is Some, call EtcdStateStore::connect() instead of MemStateStore::new(). Add a test that config with etcd_endpoints parses correctly. Run clippy and tests when done.
1c. Surface builder failure logs:


When nix build fails inside the builder VM, the user gets a generic error without seeing the Nix error output. Fix this in two places:
1. In crates/mvm-build/src/backend/ssh.rs — on build failure, capture stderr and include the last 50 lines in the error context
2. In crates/mvm-build/src/vsock_builder.rs — collect Log { line } frames during the build and include them in the error message on failure
Run clippy and tests when done.
1d. Add --log-format CLI flag:


The JSON logging layer exists in mvm-core::observability::logging but isn't exposed via CLI. Add a `--log-format <human|json>` global flag to the Cli struct in crates/mvm-cli/src/commands.rs and pass it to logging::init() in crates/mvm-cli/src/logging.rs during startup. Default should be "human". Run clippy and tests when done.
1e. Sync regression tests:


Add regression tests in crates/mvm-cli/src/doctor.rs (as a #[cfg(test)] module) that cover:
1. Lima detection logic (present vs absent)
2. Rustup/cargo path resilience (no .cargo/env needed)
3. Doctor report output format verification
Use mocking where needed to avoid requiring actual Lima. Run clippy and tests when done.
Phase 2: Integration Tests
2a. Instance lifecycle tests:


Create crates/mvm-runtime/tests/lifecycle.rs with 5 integration tests using the shell_mock infrastructure from crates/mvm-runtime/src/shell_mock.rs:
1. test_full_lifecycle_happy_path — create → start → warm → sleep → wake → stop → destroy
2. test_invalid_transition_rejected — Running → Sleeping (must go through Warm first)
3. test_quota_enforcement — start fails when tenant quota exceeded
4. test_instance_destroy_cleanup — verify TAP, cgroup, disks cleaned up
5. test_network_identity_preserved — IP and MAC same after sleep/wake
Read the shell_mock.rs and instance/lifecycle.rs code first to understand the patterns. Run clippy and tests when done.
2b. Agent reconcile tests:


Create crates/mvm-agent/tests/reconcile.rs with 5 integration tests:
1. test_reconcile_scale_up — desired 3 running, actual 0 → creates 3
2. test_reconcile_scale_down — desired 1 running, actual 3 → stops 2
3. test_reconcile_wake_sleeping — desired 2 running, 0 running + 2 sleeping → wakes 2
4. test_reconcile_signed_required — unsigned reconcile rejected in production mode
5. test_reconcile_quota_limit — reconcile respects tenant quota during scale-up
Read the agent.rs reconcile loop and existing tests first to understand patterns. Run clippy and tests when done.
2c. Build pipeline tests:


Create crates/mvm-build/tests/pipeline.rs with 5 integration tests:
1. test_cache_hit_skips_build — flake.lock unchanged → no build
2. test_template_reuse_skips_build — matching template → artifacts copied, no build
3. test_cache_key_mismatch_triggers_build — different profile → forces rebuild
4. test_force_rebuild_ignores_cache — --force always rebuilds
5. test_build_revision_recorded — after build, revision.json exists with correct metadata
Read the existing test infrastructure (FakeEnv in build.rs, cache.rs tests) first. Run clippy and tests when done.
2d. Coordinator tests:


Create crates/mvm-coordinator/tests/routing.rs with 3 integration tests:
1. test_wake_coalescing — 3 concurrent requests for same tenant share one wake operation
2. test_idle_sweep — connection closes → idle timer starts → state transitions to Idle
3. test_route_lookup — configured routes resolve correctly
Read server.rs, wake.rs, idle.rs, and routing.rs first to understand the patterns. Run clippy and tests when done.
2e. CLI integration tests:


Extend crates/mvm-cli/tests/cli.rs with 2 new integration tests:
1. test_tenant_pool_instance_commands — create/list/info/destroy for all three entity levels
2. test_template_lifecycle_commands — create/list/info/build/delete
Use assert_cmd patterns consistent with the existing tests in that file. Run clippy and tests when done.
Phase 3: Documentation
3a. Deployment guide:


Write docs/deployment.md covering: single-node deployment (install.sh node → systemd → agent serve), multi-node (coordinator + N agents), TLS certificate setup (mvm agent certs init), Etcd cluster for coordinator persistence, systemd service management, and environment variable reference for all MVM_* vars. Follow the style of existing docs in docs/ directory. Read docs/architecture.md and docs/security.md for reference.
3b. Troubleshooting runbook:


Write docs/runbook.md covering common failure scenarios: instance stuck in Warm/Sleeping (force-stop), build failures (inspect logs, clear cache, force rebuild), network issues (mvm net verify --deep, bridge/TAP diagnostics), stale PIDs (mvm doctor), LUKS key rotation, and coordinator failover. Follow the style of existing docs. Read the CLI commands and error handling code for accurate guidance.
3c. CHANGELOG and release:


Write CHANGELOG.md with entries for sprints 1-13 (read specs/sprints/ for history). Then bump the version to v0.3.0 in root Cargo.toml and verify it propagates. Run `cargo clippy --workspace -- -D warnings && cargo test --workspace` to verify everything passes. Commit the version bump and changelog.