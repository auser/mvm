//! mvm-supervisor — trusted host-side supervisor.
//!
//! Plan 37 §7B (CORNERSTONE). A single host-side process that owns:
//! egress proxy (§15), tool gate (§2.2/§15), keystore releaser (§12.2),
//! audit signer (§22), artifact collector (§21), and the plan
//! execution state machine. **Tenant code never runs in Zone B.**
//!
//! Wave 1.3 of plan 37 lands the *skeleton*: each component is a
//! trait + a `Noop` impl returning a typed error / pass-through, and
//! the plan state machine carries every transition the launch path
//! will eventually walk. The actual lift of `mvm-hostd`'s daemon
//! binary into a `mvm-supervisor` binary, plus systemd unit + launchd
//! plist, lands in Wave 1.4 (Supervisor::launch happy path).
//!
//! Why scaffold-first: each component lifts a sizeable chunk of
//! today's `mvm/src/security/*`. Landing the trait surface
//! first lets every sub-component move under it with a typed contract,
//! rather than the current grab-bag of free functions. The Noop impls
//! are the fail-closed default — a supervisor wired up with default
//! Noop slots refuses every non-trivial operation, so a misconfigured
//! deployment cannot accidentally pass tenant traffic through an
//! unwired component.
//!
//! Structure:
//! - `state` — `PlanState` + `PlanStateMachine` (transition rules
//!   for the supervisor's plan lifecycle).
//! - `egress` — `EgressProxy` trait + `NoopEgressProxy`.
//! - `tool_gate` — `ToolGate` trait + `NoopToolGate`.
//! - `keystore` — `KeystoreReleaser` trait + `NoopKeystoreReleaser`.
//! - `audit` — `AuditSigner` trait + `NoopAuditSigner`.
//! - `artifact` — `ArtifactCollector` trait + `NoopArtifactCollector`.
//! - `supervisor` — `Supervisor` aggregate that owns the slots.

pub mod artifact;
pub mod audit;
pub mod audit_dedup;
pub mod audit_file;
pub mod backend;
pub mod circuit_breaker;
pub mod destination;
pub mod egress;
pub mod injection_guard;
pub mod inspector;
pub mod instance_sampler;
pub mod keystore;
pub mod l7_proxy;
pub mod pii_redactor;
pub mod policy_tool_gate;
pub mod reaper;
pub mod secrets_scanner;
pub mod ssrf_guard;
pub mod state;
pub mod supervisor;
pub mod tool_gate;

pub use artifact::{ArtifactCollector, ArtifactError, NoopArtifactCollector};
pub use audit::{AuditEntry, AuditError, AuditSigner, CapturingAuditSigner, NoopAuditSigner};
pub use audit_dedup::{Decision, DedupKey, RetryStormSummary, RetryStormSuppressor};
pub use audit_file::{FileAuditSigner, SignedEnvelope, VerifyError, verify_audit_chain};
pub use backend::{BackendError, BackendLauncher, NoopBackendLauncher};
pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitState, Clock as CircuitBreakerClock,
    InspectorReporter, SystemClock as CircuitBreakerSystemClock,
};
pub use destination::DestinationPolicy;
pub use egress::{EgressDecision, EgressError, EgressProxy, NoopEgressProxy};
pub use injection_guard::{InjectionGuard, InjectionRule, RuleFamily};
pub use inspector::{Inspector, InspectorChain, InspectorVerdict, RequestCtx};
pub use instance_sampler::{OsSources, Sample, SampleTarget, Sources, sample_once};
pub use keystore::{KeystoreError, KeystoreReleaser, NoopKeystoreReleaser, SecretGrant};
pub use l7_proxy::{
    AuditFields, CapturingEgressAuditSink, ConnectParseError, ConnectRequest, DnsResolver,
    EgressAuditSink, EgressOutcome, EvaluationResult, L7EgressProxy, NoopEgressAuditSink,
    TokioDnsResolver, parse_connect,
};
pub use pii_redactor::{PiiRedactor, PiiRule, PiiValidator};
pub use policy_tool_gate::{
    CapturingToolAuditSink, NoopToolAuditSink, PolicyToolGate, ToolAuditError, ToolAuditFields,
    ToolAuditSink, ToolOutcome,
};
pub use reaper::{
    DEFAULT_INTERVAL as REAPER_DEFAULT_INTERVAL, DEFAULT_JITTER as REAPER_DEFAULT_JITTER,
    ReapOutcome, Reaper, TeardownFn, deregister_only_teardown, jittered_interval,
};
pub use secrets_scanner::{DEFAULT_RULES, SecretRule, SecretsScanner};
pub use ssrf_guard::SsrfGuard;
pub use state::{PlanState, PlanStateMachine, StateTransitionError};
pub use supervisor::{Supervisor, SupervisorError};
pub use tool_gate::{NoopToolGate, ToolDecision, ToolError, ToolGate};
