# mvm-core

Foundation crate for the mvm workspace. Provides pure types, IDs, configuration, protocol definitions, and utilities. Has **zero internal mvm dependencies** — every other crate in the workspace depends on this one.

## Modules

| Module | Purpose |
|--------|---------|
| `agent` | Coordinator-agent protocol types (`DesiredState`, `ReconcileReport`, `AgentRequest`/`AgentResponse`) |
| `audit` | Audit logging types (`AuditEntry`, `AuditAction`) |
| `build_env` | `ShellEnvironment` and `BuildEnvironment` traits for abstracting shell execution |
| `config` | Firecracker version, mvm data directory, production mode detection |
| `idle_metrics` | `IdleMetrics` for tracking instance idle state |
| `instance` | `InstanceStatus`, `InstanceState`, `InstanceNet`, state transition validation |
| `naming` | ID validation, instance/TAP/MAC address generation, path parsing |
| `node` | `NodeInfo`, `NodeStats` for node-level metadata and resource reporting |
| `observability` | Tracing and metrics types |
| `platform` | Platform detection (macOS, Linux with/without KVM) |
| `pool` | `PoolSpec`, `Role`, `RuntimePolicy`, `DesiredCounts`, `BuildRevision`, `ArtifactPaths` |
| `protocol` | Hostd IPC protocol (`HostdRequest`/`HostdResponse`) for privilege-separated operations |
| `retry` | Retry policy utilities |
| `routing` | `RoutingTable`, `Route`, `MatchRule`, `RouteTarget` for gateway routing |
| `signing` | `SignedPayload` type for Ed25519 payload signatures |
| `template` | `TemplateSpec`, `TemplateConfig`, `TemplateVariant`, `TemplateRevision` |
| `tenant` | `TenantConfig`, `TenantQuota`, `TenantNet`, filesystem path helpers |
| `time` | Time utilities |

## Key Traits

```rust
// Base trait for shell execution (used in dev mode)
pub trait ShellEnvironment {
    fn shell_exec(&self, script: &str) -> Result<()>;
    fn shell_exec_stdout(&self, script: &str) -> Result<String>;
    fn shell_exec_visible(&self, script: &str) -> Result<()>;
    fn log_info(&self, msg: &str);
    fn log_success(&self, msg: &str);
}

// Extended trait for orchestrated builds (used in fleet mode)
pub trait BuildEnvironment: ShellEnvironment {
    fn load_pool_spec(&self, tenant_id: &str, pool_id: &str) -> Result<PoolSpec>;
    fn load_tenant_config(&self, tenant_id: &str) -> Result<TenantConfig>;
    fn ensure_bridge(&self, tenant_id: &str) -> Result<()>;
    // ...
}
```

## Design Notes

- Contains orchestration types (tenant, pool, instance, agent, protocol) even though they're only used by mvmd. This avoids a separate shared-types crate and keeps the `mvm` facade dependency simple.
- State transitions are validated via `instance::validate_transition()`.
- `pool::Role` defaults to `Worker` via `#[default]`.
- All IDs are validated through `naming::validate_id()`.
