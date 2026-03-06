# mvm — Maintenance Mode

Active development has moved to [mvmd](https://github.com/auser/mvmd) (fleet orchestrator).

## Completed Sprints

- [01-foundation.md](sprints/01-foundation.md)
- [02-production-readiness.md](sprints/02-production-readiness.md)
- [03-real-world-validation.md](sprints/03-real-world-validation.md)
- Sprint 4: Security Baseline 90%
- Sprint 5: Final Security Hardening
- [06-minimum-runtime.md](sprints/06-minimum-runtime.md)
- [07-role-profiles.md](sprints/07-role-profiles.md)
- [08-integration-lifecycle.md](sprints/08-integration-lifecycle.md)
- [09-openclaw-support.md](sprints/09-openclaw-support.md)
- [10-coordinator.md](sprints/10-coordinator.md)
- Sprint 11: Dev Environment
- [12-install-release-security.md](sprints/12-install-release-security.md)
- [13-boot-time-optimization.md](sprints/13-boot-time-optimization.md)
- [14-guest-library-and-examples.md](sprints/14-guest-library-and-examples.md)
- [15-real-world-apps.md](sprints/15-real-world-apps.md)

## Current Status (v0.3.6)

| Metric           | Value                    |
| ---------------- | ------------------------ |
| Workspace crates | 6 + root facade          |
| Total tests      | 630                      |
| Clippy warnings  | 0                        |
| Edition          | 2024 (Rust 1.85+)        |
| Examples         | hello, openclaw, paperclip |
| Boot time        | < 10s (< 200ms from snapshot) |
| Binary           | `mvmctl`                 |

## Deferred Backlog

These items may be addressed as needed, driven by mvmd requirements:

- **Config-driven multi-variant builds**: `template.toml` support for building multiple
  variants (gateway, worker) in one command with per-variant resource defaults.

- **mvm-profiles.toml redesign**: Map profiles to flake package attributes instead of
  NixOS module paths. Update Rust parser in mvm-build accordingly.

- **Upstream mvm-core changes**: `UpdateStrategy` types, `DesiredPool.registry_artifact`,
  and `registry_download_revision()` extraction are complete (Phases 71-72a). Further
  fields may be added as needed by mvmd Sprint 13.

## mvmd Sprint 13 — Upstream Fields (Phase 71)

All fields required by mvmd Sprint 13 Phase 71 are already on HEAD. No further
mvm-core changes needed — mvmd just needs to bump its `mvmctl` git dependency.

### DesiredTenant (`mvm-core/src/agent.rs` line 29)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredTenant {
    pub tenant_id: String,
    pub network: DesiredTenantNetwork,
    pub quotas: TenantQuota,
    #[serde(default)]
    pub secrets_hash: Option<String>,
    pub pools: Vec<DesiredPool>,
    #[serde(default)]
    pub preferred_regions: Vec<String>,        // ← Phase 71
}
```

### DesiredPool (`mvm-core/src/agent.rs` line 54)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredPool {
    pub pool_id: String,
    pub flake_ref: String,
    pub profile: String,
    #[serde(default)]
    pub role: Role,
    pub instance_resources: InstanceResources,
    pub desired_counts: DesiredCounts,
    #[serde(default)]
    pub runtime_policy: RuntimePolicy,
    #[serde(default = "default_seccomp")]
    pub seccomp_policy: String,
    #[serde(default = "default_compression")]
    pub snapshot_compression: String,
    #[serde(default)]
    pub routing_table: Option<RoutingTable>,
    #[serde(default)]
    pub secret_scopes: Vec<SecretScope>,
    #[serde(default)]
    pub sleep_policy: Option<SleepPolicyConfig>,       // ← Phase 71
    #[serde(default)]
    pub default_update_strategy: Option<UpdateStrategy>, // ← Phase 71
    #[serde(default)]
    pub registry_artifact: Option<RegistryArtifact>,   // ← Phase 72
}
```

### Related Types (`mvm-core/src/pool.rs`)

- `UpdateStrategy` — tagged enum (`#[serde(tag = "type", rename_all = "snake_case")]`)
  - `Rolling(RollingUpdateStrategy)` — `max_unavailable: 1, max_surge: 1, health_check_timeout_secs: 60`
  - `Canary(CanaryStrategy)` — `canary_count: 1, canary_duration_secs: 300, success_threshold: 0.95`
- `SleepPolicyConfig` — `warm_threshold_secs: 300, sleep_threshold_secs: 900, cpu_threshold: 5.0, net_bytes_threshold: 1024`
- `RegistryArtifact` — `template_id: String, revision: Option<String>`

All new fields use `#[serde(default)]` for backward compat. Serde roundtrip tests exist.

### What mvmd needs to wire (in mvmd repo)

1. Bump `mvmctl` git dep to latest main
2. Wire `preferred_regions` into scheduler placement scoring
3. Wire `default_update_strategy` into agent reconcile rollout decisions
4. Wire `sleep_policy` overrides into per-pool sleep policy thresholds
5. `registry_artifact` already wired via mvmd Phase 72

## Maintenance Policy

Bug fixes and mvm-core type changes (for mvmd compatibility) will continue to be
committed here. New feature development happens in mvmd.
