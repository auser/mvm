//! Plan 60 Phase 3 Slice D — `mvmctl policy {show, verify, update}` CLI.
//!
//! Operator-facing surface over the on-disk policy bundles at
//! `~/.mvm/policies/<tenant>/<workload>.toml`. Today:
//!
//! - **`mvmctl policy show <tenant>:<workload> [--json]`** — load,
//!   parse, pretty-print. Human format by default; `--json` emits
//!   the canonical wire shape. Useful for debugging "did my edit
//!   take?" and for piping into other tools.
//! - **`mvmctl policy verify <tenant>:<workload>`** — load + parse +
//!   schema-version check + run the same policy validators used at
//!   supervisor admission time. Catches typos and unparseable CIDRs
//!   *on the operator's host* rather than at boot inside the
//!   supervisor. Exits non-zero on any error.
//! - **`mvmctl policy explain <tenant>:<workload> [--json]`** —
//!   validate and summarize the admission decision. JSON output is
//!   deliberately redacted: it includes counts and policy posture, not
//!   raw artifact paths or audit destination URLs.
//! - **`mvmctl policy update`** — stubbed; the production update
//!   flow requires an mvmd-signed plan (plan 60 Phase 8 territory).
//!   Errors with a clear pointer; no on-disk side effects.
//!
//! ## Identifier shape
//!
//! Slice A pinned the bundle ref shape as `"<tenant>:<workload>"`
//! — same colon-separated form `PolicyRef` carries. Splitting the
//! flag into `--tenant T --workload W` would diverge from that
//! contract; this module sticks with the single positional.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use mvm_policy::toml_loader::{self, LoadError};
use serde::Serialize;

use mvm_core::user_config::MvmConfig;

use super::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    #[command(subcommand)]
    pub action: PolicyAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum PolicyAction {
    /// Load + pretty-print a tenant policy bundle.
    Show {
        /// `<tenant>:<workload>` identifier (matches the PolicyRef
        /// shape on `ExecutionPlan`).
        bundle: String,
        /// Emit the canonical JSON wire shape instead of the
        /// human-readable summary.
        #[arg(long)]
        json: bool,
    },
    /// Validate a tenant policy bundle: parse + schema-version
    /// check + admission validators. Exits non-zero on any failure.
    Verify {
        /// `<tenant>:<workload>` identifier.
        bundle: String,
    },
    /// Validate and explain the effective admission posture.
    Explain {
        /// `<tenant>:<workload>` identifier.
        bundle: String,
        /// Emit a redacted machine-readable explanation.
        #[arg(long)]
        json: bool,
    },
    /// Update is stubbed in v0 — production updates require an
    /// mvmd-signed plan. See plan 60 Phase 8.
    Update {
        bundle: String,
        /// Path to a TOML file with the new bundle contents.
        /// Accepted for future shape compatibility; the command
        /// refuses unconditionally for v0.
        #[arg(long)]
        from: Option<PathBuf>,
    },
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let base_dir = default_policy_dir()?;
    match args.action {
        PolicyAction::Show { bundle, json } => cmd_show(&base_dir, &bundle, json),
        PolicyAction::Verify { bundle } => cmd_verify(&base_dir, &bundle),
        PolicyAction::Explain { bundle, json } => cmd_explain(&base_dir, &bundle, json),
        PolicyAction::Update { bundle, from } => cmd_update(&bundle, from.as_deref()),
    }
}

/// Resolve the host's `~/.mvm/policies/` base dir. Mirrors the
/// shape `mvm_policy::toml_loader::default_policy_dir` returns,
/// but errors loudly if `$HOME` is unset rather than guessing.
fn default_policy_dir() -> Result<PathBuf> {
    toml_loader::default_policy_dir().context("HOME not set; can't locate ~/.mvm/policies/")
}

/// Parse `<tenant>:<workload>` exactly. Refuses anything that
/// doesn't carry a single non-empty colon-separated pair.
fn parse_bundle_ref(value: &str) -> Result<(&str, &str)> {
    let (tenant, workload) = value.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("bundle identifier {value:?} is not in <tenant>:<workload> form")
    })?;
    if tenant.is_empty() || workload.is_empty() {
        anyhow::bail!("bundle identifier {value:?} has an empty tenant or workload");
    }
    if tenant.contains('/') || workload.contains('/') {
        anyhow::bail!(
            "bundle identifier {value:?} contains '/' — refused to keep the \
             resolved path confined to ~/.mvm/policies/"
        );
    }
    Ok((tenant, workload))
}

fn cmd_show(base_dir: &std::path::Path, bundle_ref: &str, as_json: bool) -> Result<()> {
    let (tenant, workload) = parse_bundle_ref(bundle_ref)?;
    let bundle = load_bundle(base_dir, bundle_ref, tenant, workload)?;
    if as_json {
        let json = serde_json::to_string_pretty(&bundle).context("serializing bundle to JSON")?;
        println!("{json}");
    } else {
        render_human(&bundle, tenant, workload);
    }
    Ok(())
}

fn cmd_verify(base_dir: &std::path::Path, bundle_ref: &str) -> Result<()> {
    let (tenant, workload) = parse_bundle_ref(bundle_ref)?;
    let bundle = load_bundle(base_dir, bundle_ref, tenant, workload)?;
    validate_bundle(&bundle)?;

    eprintln!(
        "OK — bundle {bundle_ref} (schema_version={}, bundle_id={}, \
         bundle_version={}) parses and validates cleanly",
        bundle.schema_version, bundle.bundle_id.0, bundle.bundle_version
    );
    Ok(())
}

fn cmd_explain(base_dir: &std::path::Path, bundle_ref: &str, as_json: bool) -> Result<()> {
    let (tenant, workload) = parse_bundle_ref(bundle_ref)?;
    let bundle = load_bundle(base_dir, bundle_ref, tenant, workload)?;
    let explain = build_explain(bundle_ref, tenant, workload, &bundle)?;

    if as_json {
        let json =
            serde_json::to_string_pretty(&explain).context("serializing policy explanation")?;
        println!("{json}");
    } else {
        render_explain_human(&explain);
    }
    Ok(())
}

fn validate_bundle(bundle: &mvm_policy::PolicyBundle) -> Result<usize> {
    mvm_supervisor::LiveL4Gate::from_specs(&bundle.network.l4)
        .map_err(|e| anyhow::anyhow!("[[network.l4]] translation failed: {e}"))?;
    mvm_supervisor::validate_egress_policy_inspector_names(&bundle.egress)
        .map_err(|e| anyhow::anyhow!("[egress].disabled_inspectors validation failed: {e}"))?;
    mvm_supervisor::validate_audit_policy_stream_destinations(&bundle.audit)
        .map_err(|e| anyhow::anyhow!("[audit].stream_destinations validation failed: {e}"))?;
    let chain = mvm_supervisor::build_inspector_chain_with_pii(&bundle.egress, &bundle.pii, None)
        .map_err(|e| anyhow::anyhow!("[pii] validation failed: {e}"))?;
    Ok(chain.len())
}

fn build_explain(
    bundle_ref: &str,
    tenant: &str,
    workload: &str,
    bundle: &mvm_policy::PolicyBundle,
) -> Result<PolicyExplain> {
    let inspector_chain_len = validate_bundle(bundle)?;
    Ok(PolicyExplain {
        schema_version: 1,
        bundle_ref: bundle_ref.to_string(),
        tenant: tenant.to_string(),
        workload: workload.to_string(),
        bundle_id: bundle.bundle_id.0.clone(),
        bundle_version: bundle.bundle_version,
        validation: ExplainValidation {
            status: "ok",
            checks: vec![
                "schema_version",
                "network_l4",
                "egress_inspectors",
                "pii_policy",
                "audit_destinations",
            ],
        },
        network: ExplainNetwork {
            default_action: "deny",
            preset: bundle.network.preset.clone(),
            l4_rule_count: bundle.network.l4.len(),
            l4_rules: bundle.network.l4.iter().map(ExplainL4Rule::from).collect(),
        },
        egress: ExplainEgress {
            default_action: "deny",
            mode: bundle.egress.mode.clone(),
            allow_plain_http: bundle.egress.allow_plain_http,
            body_cap_bytes: effective_body_cap(&bundle.egress),
            allow_list_count: bundle.egress.allow_list.len(),
            allow_list_ports: sorted_ports(&bundle.egress.allow_list),
            wildcard_port_count: bundle
                .egress
                .allow_list
                .iter()
                .filter(|(_, port)| *port == 0)
                .count(),
            disabled_inspectors: bundle.egress.disabled_inspectors.clone(),
            enabled_inspectors: enabled_inspector_names(&bundle.egress, &bundle.pii),
            inspector_chain_len,
            pii_mode: bundle
                .pii
                .mode
                .clone()
                .unwrap_or_else(|| "detect".to_string()),
            pii_category_count: bundle.pii.categories.len(),
        },
        tool: ExplainTool {
            default_action: "deny",
            allowed_count: bundle.tool.allowed.len(),
            allowed: bundle.tool.allowed.clone(),
        },
        artifact: ExplainArtifact {
            capture_path_count: bundle.artifact.capture_paths.len(),
            retention_days: bundle.artifact.retention_days,
        },
        keys: ExplainKeys {
            rotation_interval_days: bundle.keys.rotation_interval_days,
        },
        audit: ExplainAudit {
            chain_signing: bundle.audit.chain_signing,
            stream_destination_count: bundle.audit.stream_destinations.len(),
            stream_destination_schemes: audit_destination_schemes(&bundle.audit),
        },
    })
}

fn cmd_update(bundle_ref: &str, _from: Option<&std::path::Path>) -> Result<()> {
    // Plan 60 Phase 8 wires `mvmctl policy update` to the mvmd
    // signed-plan flow. For v0 we refuse rather than offering a
    // local-only edit path — operators who want to iterate on a
    // bundle today edit the TOML directly under
    // `~/.mvm/policies/<tenant>/<workload>.toml`, then run
    // `mvmctl policy verify <tenant>:<workload>`.
    anyhow::bail!(
        "`mvmctl policy update {bundle_ref}` is not implemented in v0 — \
         production updates require an mvmd-signed plan (plan 60 Phase 8). \
         Edit ~/.mvm/policies/<tenant>/<workload>.toml directly and run \
         `mvmctl policy verify {bundle_ref}` to validate"
    )
}

fn load_bundle(
    base_dir: &std::path::Path,
    bundle_ref: &str,
    tenant: &str,
    workload: &str,
) -> Result<mvm_policy::PolicyBundle> {
    toml_loader::load_bundle_from_path(base_dir, tenant, workload).map_err(|e| match e {
        LoadError::NotFound { path } => anyhow::anyhow!(
            "no bundle for {bundle_ref} at {} — create the file or check the ref",
            path.display()
        ),
        LoadError::Io { path, detail } => anyhow::anyhow!(
            "reading bundle for {bundle_ref} at {} failed: {detail}",
            path.display()
        ),
        LoadError::Parse { path, detail } => anyhow::anyhow!(
            "parsing bundle for {bundle_ref} at {} failed: {detail}",
            path.display()
        ),
        LoadError::SchemaMismatch { path, got, known } => anyhow::anyhow!(
            "bundle for {bundle_ref} at {} has schema_version {got}; \
             this binary only understands version {known}",
            path.display()
        ),
    })
}

fn render_human(bundle: &mvm_policy::PolicyBundle, tenant: &str, workload: &str) {
    println!("policy bundle  {tenant}:{workload}");
    println!("  schema_version = {}", bundle.schema_version);
    println!("  bundle_id      = {}", bundle.bundle_id.0);
    println!("  bundle_version = {}", bundle.bundle_version);

    println!("  [network]");
    if let Some(preset) = bundle.network.preset.as_deref() {
        println!("    preset = {preset:?}");
    }
    if bundle.network.l4.is_empty() {
        println!("    l4     = []  (default-deny)");
    } else {
        println!("    l4:");
        for (i, rule) in bundle.network.l4.iter().enumerate() {
            let port_repr = if rule.port_lo == 0 && rule.port_hi == 0 {
                "*".to_string()
            } else if rule.port_lo == rule.port_hi {
                rule.port_lo.to_string()
            } else {
                format!("{}-{}", rule.port_lo, rule.port_hi)
            };
            println!("      [{i}] {} {} :{port_repr}", rule.proto, rule.dst_cidr);
        }
    }

    println!("  [egress]");
    if let Some(mode) = bundle.egress.mode.as_deref() {
        println!("    mode             = {mode:?}");
    }
    println!("    allow_plain_http = {}", bundle.egress.allow_plain_http);
    if bundle.egress.allow_list.is_empty() {
        println!("    allow_list       = []  (default-deny)");
    } else {
        println!("    allow_list:");
        for (host, port) in &bundle.egress.allow_list {
            let port_repr = if *port == 0 {
                "*".to_string()
            } else {
                port.to_string()
            };
            println!("      {host}:{port_repr}");
        }
    }
    if !bundle.egress.disabled_inspectors.is_empty() {
        println!(
            "    disabled_inspectors = {:?}",
            bundle.egress.disabled_inspectors
        );
    }

    println!("  [tool]");
    if bundle.tool.allowed.is_empty() {
        println!("    allowed = []  (default-deny — no tool RPCs permitted)");
    } else {
        println!("    allowed = {:?}", bundle.tool.allowed);
    }

    println!("  [artifact]");
    println!("    capture_paths  = {:?}", bundle.artifact.capture_paths);
    println!("    retention_days = {}", bundle.artifact.retention_days);

    println!("  [keys]");
    println!(
        "    rotation_interval_days = {}",
        bundle.keys.rotation_interval_days
    );

    println!("  [audit]");
    println!("    chain_signing       = {}", bundle.audit.chain_signing);
    println!(
        "    stream_destinations = {:?}",
        bundle.audit.stream_destinations
    );
}

fn render_explain_human(explain: &PolicyExplain) {
    println!("policy explain  {}", explain.bundle_ref);
    println!("  validation = {}", explain.validation.status);
    println!(
        "  bundle     = {}@{}",
        explain.bundle_id, explain.bundle_version
    );
    println!("  [network]");
    println!("    default_action = {}", explain.network.default_action);
    println!("    l4_rule_count  = {}", explain.network.l4_rule_count);
    println!("  [egress]");
    println!(
        "    default_action      = {}",
        explain.egress.default_action
    );
    println!(
        "    allow_list_count    = {}",
        explain.egress.allow_list_count
    );
    println!(
        "    allow_plain_http    = {}",
        explain.egress.allow_plain_http
    );
    println!(
        "    enabled_inspectors  = {:?}",
        explain.egress.enabled_inspectors
    );
    println!("    pii_mode            = {}", explain.egress.pii_mode);
    println!("  [tool]");
    println!("    allowed_count = {}", explain.tool.allowed_count);
    println!("  [artifact]");
    println!(
        "    capture_path_count = {}",
        explain.artifact.capture_path_count
    );
    println!(
        "    retention_days     = {}",
        explain.artifact.retention_days
    );
    println!("  [keys]");
    println!(
        "    rotation_interval_days = {}",
        explain.keys.rotation_interval_days
    );
    println!("  [audit]");
    println!(
        "    chain_signing            = {}",
        explain.audit.chain_signing
    );
    println!(
        "    stream_destination_count = {}",
        explain.audit.stream_destination_count
    );
    println!(
        "    stream_destination_schemes = {:?}",
        explain.audit.stream_destination_schemes
    );
}

#[derive(Debug, Serialize)]
struct PolicyExplain {
    schema_version: u32,
    bundle_ref: String,
    tenant: String,
    workload: String,
    bundle_id: String,
    bundle_version: u32,
    validation: ExplainValidation,
    network: ExplainNetwork,
    egress: ExplainEgress,
    tool: ExplainTool,
    artifact: ExplainArtifact,
    keys: ExplainKeys,
    audit: ExplainAudit,
}

#[derive(Debug, Serialize)]
struct ExplainValidation {
    status: &'static str,
    checks: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct ExplainNetwork {
    default_action: &'static str,
    preset: Option<String>,
    l4_rule_count: usize,
    l4_rules: Vec<ExplainL4Rule>,
}

#[derive(Debug, Serialize)]
struct ExplainL4Rule {
    proto: String,
    dst_cidr: String,
    port: ExplainPortRange,
}

impl From<&mvm_policy::L4RuleSpec> for ExplainL4Rule {
    fn from(rule: &mvm_policy::L4RuleSpec) -> Self {
        Self {
            proto: rule.proto.clone(),
            dst_cidr: rule.dst_cidr.clone(),
            port: ExplainPortRange {
                lo: rule.port_lo,
                hi: rule.port_hi,
                wildcard: rule.port_lo == 0 && rule.port_hi == 0,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ExplainPortRange {
    lo: u16,
    hi: u16,
    wildcard: bool,
}

#[derive(Debug, Serialize)]
struct ExplainEgress {
    default_action: &'static str,
    mode: Option<String>,
    allow_plain_http: bool,
    body_cap_bytes: u64,
    allow_list_count: usize,
    allow_list_ports: Vec<u16>,
    wildcard_port_count: usize,
    disabled_inspectors: Vec<String>,
    enabled_inspectors: Vec<&'static str>,
    inspector_chain_len: usize,
    pii_mode: String,
    pii_category_count: usize,
}

#[derive(Debug, Serialize)]
struct ExplainTool {
    default_action: &'static str,
    allowed_count: usize,
    allowed: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ExplainArtifact {
    capture_path_count: usize,
    retention_days: u32,
}

#[derive(Debug, Serialize)]
struct ExplainKeys {
    rotation_interval_days: u32,
}

#[derive(Debug, Serialize)]
struct ExplainAudit {
    chain_signing: bool,
    stream_destination_count: usize,
    stream_destination_schemes: Vec<&'static str>,
}

fn effective_body_cap(policy: &mvm_policy::EgressPolicy) -> u64 {
    if policy.body_cap_bytes == 0 {
        mvm_policy::DEFAULT_BODY_CAP_BYTES
    } else {
        policy.body_cap_bytes
    }
}

fn sorted_ports(allow_list: &[(String, u16)]) -> Vec<u16> {
    let mut ports: Vec<u16> = allow_list.iter().map(|(_, port)| *port).collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn enabled_inspector_names(
    egress: &mvm_policy::EgressPolicy,
    pii: &mvm_policy::PiiPolicy,
) -> Vec<&'static str> {
    mvm_supervisor::KNOWN_INSPECTOR_NAMES
        .iter()
        .copied()
        .filter(|name| {
            !egress
                .disabled_inspectors
                .iter()
                .any(|disabled| disabled == name)
        })
        .filter(|name| *name != "pii_redactor" || pii.mode.as_deref() != Some("disabled"))
        .collect()
}

fn audit_destination_schemes(audit: &mvm_policy::AuditPolicy) -> Vec<&'static str> {
    let mut schemes: Vec<&'static str> = audit
        .stream_destinations
        .iter()
        .filter_map(|destination| {
            mvm_supervisor::KNOWN_AUDIT_STREAM_SCHEMES
                .iter()
                .copied()
                .find(|scheme| destination.starts_with(scheme))
        })
        .map(|scheme| scheme.trim_end_matches("://"))
        .collect();
    schemes.sort_unstable();
    schemes.dedup();
    schemes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bundle(dir: &std::path::Path, tenant: &str, workload: &str, body: &str) {
        let td = dir.join(tenant);
        std::fs::create_dir_all(&td).unwrap();
        std::fs::write(td.join(format!("{workload}.toml")), body).unwrap();
    }

    fn minimal_bundle_toml() -> &'static str {
        r#"
schema_version = 1
bundle_id      = "acme/web-worker"
bundle_version = 1

[network]
[egress]
allow_list = [["api.example.com", 443]]
[pii]
[tool]
allowed = ["web_search"]
[artifact]
[keys]
[audit]
"#
    }

    fn sensitive_bundle_toml() -> &'static str {
        r#"
schema_version = 1
bundle_id      = "acme/web-worker"
bundle_version = 7

[network]
[[network.l4]]
proto    = "tcp"
dst_cidr = "10.0.0.0/24"
port_lo  = 443
port_hi  = 443

[egress]
allow_list = [["private-api.example.internal", 443], ["metrics.example.internal", 0]]
allow_plain_http = false
disabled_inspectors = ["injection_guard"]

[pii]
mode = "redact"
categories = ["email"]

[tool]
allowed = ["web_search", "fetch_url"]

[artifact]
capture_paths = ["/home/user/.ssh", "/var/lib/app/customer-export.json"]
retention_days = 3

[keys]
rotation_interval_days = 30

[audit]
chain_signing = true
stream_destinations = ["https://audit.example.internal/tenant/acme", "file:///var/log/mvm/acme.jsonl"]
"#
    }

    #[test]
    fn parse_bundle_ref_accepts_tenant_workload() {
        assert_eq!(
            parse_bundle_ref("acme:web-worker").unwrap(),
            ("acme", "web-worker")
        );
    }

    #[test]
    fn parse_bundle_ref_rejects_missing_colon() {
        let err = parse_bundle_ref("acme-web-worker").unwrap_err();
        assert!(err.to_string().contains("<tenant>:<workload>"));
    }

    #[test]
    fn parse_bundle_ref_rejects_empty_halves() {
        assert!(parse_bundle_ref(":web-worker").is_err());
        assert!(parse_bundle_ref("acme:").is_err());
    }

    #[test]
    fn parse_bundle_ref_rejects_slash_in_either_half() {
        // Path traversal defence — `acme/../etc:web` shouldn't
        // escape ~/.mvm/policies/.
        assert!(parse_bundle_ref("acme/../etc:web").is_err());
        assert!(parse_bundle_ref("acme:web/worker").is_err());
    }

    #[test]
    fn cmd_show_renders_human_for_minimal_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(tmp.path(), "acme", "web-worker", minimal_bundle_toml());
        // Doesn't panic and reads the file end-to-end.
        cmd_show(tmp.path(), "acme:web-worker", false).unwrap();
    }

    #[test]
    fn cmd_show_emits_json_when_flag_set() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(tmp.path(), "acme", "web-worker", minimal_bundle_toml());
        cmd_show(tmp.path(), "acme:web-worker", true).unwrap();
    }

    #[test]
    fn cmd_show_errors_clearly_on_missing_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let err = cmd_show(tmp.path(), "acme:missing", false).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("no bundle") && s.contains("missing"),
            "want clear not-found error, got: {s}"
        );
    }

    #[test]
    fn cmd_verify_accepts_clean_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(tmp.path(), "acme", "web-worker", minimal_bundle_toml());
        cmd_verify(tmp.path(), "acme:web-worker").unwrap();
    }

    #[test]
    fn cmd_explain_accepts_clean_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(tmp.path(), "acme", "web-worker", minimal_bundle_toml());
        cmd_explain(tmp.path(), "acme:web-worker", false).unwrap();
        cmd_explain(tmp.path(), "acme:web-worker", true).unwrap();
    }

    #[test]
    fn explain_json_redacts_paths_and_audit_destinations() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(tmp.path(), "acme", "web-worker", sensitive_bundle_toml());
        let bundle = load_bundle(tmp.path(), "acme:web-worker", "acme", "web-worker").unwrap();
        let explain = build_explain("acme:web-worker", "acme", "web-worker", &bundle).unwrap();
        let json = serde_json::to_string(&explain).unwrap();

        assert!(json.contains("\"capture_path_count\":2"));
        assert!(json.contains("\"stream_destination_count\":2"));
        assert!(json.contains("\"https\""));
        assert!(json.contains("\"file\""));
        assert!(!json.contains("/home/user/.ssh"));
        assert!(!json.contains("customer-export"));
        assert!(!json.contains("audit.example.internal/tenant/acme"));
        assert!(!json.contains("private-api.example.internal"));
    }

    #[test]
    fn cmd_explain_catches_unknown_disabled_inspector() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(
            tmp.path(),
            "acme",
            "web",
            r#"
schema_version = 1
bundle_id      = "acme/web"
bundle_version = 1

[network]
[egress]
disabled_inspectors = ["ssr_guard"]
[pii]
[tool]
[artifact]
[keys]
[audit]
"#,
        );
        let err = cmd_explain(tmp.path(), "acme:web", true).unwrap_err();
        let chained: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chained.contains("disabled_inspectors"));
        assert!(chained.contains("ssr_guard"));
    }

    #[test]
    fn cmd_explain_catches_bad_audit_destination_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(
            tmp.path(),
            "acme",
            "web",
            r#"
schema_version = 1
bundle_id      = "acme/web"
bundle_version = 1

[network]
[egress]
[pii]
[tool]
[artifact]
[keys]
[audit]
stream_destinations = ["htpps://audit.example.com/ingest"]
"#,
        );
        let err = cmd_explain(tmp.path(), "acme:web", true).unwrap_err();
        let chained: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chained.contains("stream_destinations"));
        assert!(chained.contains("htpps://audit.example.com/ingest"));
    }

    #[test]
    fn cmd_verify_catches_unknown_l4_protocol() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(
            tmp.path(),
            "acme",
            "web",
            r#"
schema_version = 1
bundle_id      = "acme/web"
bundle_version = 1

[network]
[[network.l4]]
proto    = "icmp"
dst_cidr = "10.0.0.0/24"
port_lo  = 0
port_hi  = 0

[egress]
[pii]
[tool]
[artifact]
[keys]
[audit]
"#,
        );
        let err = cmd_verify(tmp.path(), "acme:web").unwrap_err();
        let chained: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            chained.contains("translation failed") || chained.contains("proto"),
            "want l4-translate error, got: {chained}"
        );
    }

    #[test]
    fn cmd_verify_catches_bad_cidr() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(
            tmp.path(),
            "acme",
            "web",
            r#"
schema_version = 1
bundle_id      = "acme/web"
bundle_version = 1

[network]
[[network.l4]]
proto    = "tcp"
dst_cidr = "not-a-cidr"
port_lo  = 443
port_hi  = 443

[egress]
[pii]
[tool]
[artifact]
[keys]
[audit]
"#,
        );
        let err = cmd_verify(tmp.path(), "acme:web").unwrap_err();
        let chained: String = err
            .chain()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chained.contains("translation failed"));
    }

    #[test]
    fn cmd_verify_catches_schema_version_drift() {
        let tmp = tempfile::tempdir().unwrap();
        write_bundle(
            tmp.path(),
            "acme",
            "web",
            r#"
schema_version = 999
bundle_id      = "acme/web"
bundle_version = 1

[network]
[egress]
[pii]
[tool]
[artifact]
[keys]
[audit]
"#,
        );
        let err = cmd_verify(tmp.path(), "acme:web").unwrap_err();
        assert!(err.to_string().contains("schema_version 999"));
    }

    #[test]
    fn cmd_update_refuses_with_mvmd_signed_pointer() {
        let err = cmd_update("acme:web", None).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("not implemented"));
        assert!(s.contains("mvmd-signed"));
        assert!(s.contains("Phase 8"));
    }
}
