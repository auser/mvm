//! `mvmctl exec` — boot a transient microVM, run a single command, tear down.

use anyhow::{Context, Result};
use base64::Engine as _;
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};

use mvm_core::user_config::MvmConfig;
use mvm_core::util::parse_human_size;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::super::env::apple_container::ensure_default_microvm_image;
use super::Cli;
use super::host_signer::{PUBLIC_FILENAME, host_signer_id, load_or_init};
use crate::ui;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Boot a pre-built manifest (path to `mvm.toml`, its directory, or a
    /// legacy slot name). If omitted, the bundled
    /// `nix/images/default-tenant/` image is used (built via Nix on first use,
    /// cached at `~/.cache/mvm/default-microvm/`). Each invocation boots a
    /// fresh transient microVM — never the long-running `mvmctl dev` VM.
    #[arg(short = 'm', long)]
    pub manifest: Option<String>,
    /// vCPU cores (default: 2)
    #[arg(long, default_value = "2")]
    pub cpus: u32,
    /// Memory (supports human-readable: 512M, 1G, …)
    #[arg(long, default_value = "512M")]
    pub memory: String,
    /// Share a host directory into the guest. Format: `HOST_PATH:/GUEST_PATH[:MODE]`
    /// where MODE is `ro` (default, writes are discarded) or `rw` (writes are
    /// rsynced back to the host directory after the command exits — see ADR-002). Repeatable
    #[arg(short = 'd', long)]
    pub add_dir: Vec<String>,
    /// Environment variable to inject (KEY=VALUE). Repeatable. Overrides any env vars
    /// carried by `--launch-plan`.
    #[arg(short, long)]
    pub env: Vec<String>,
    /// Per-command timeout in seconds (default: 60)
    #[arg(long, default_value = "60")]
    pub timeout: u64,
    /// Path to an mvmforge document — either the `launch.json` artifact
    /// from `mvmforge compile` (top-level `entrypoint`) or the Workload IR
    /// manifest from `mvmforge emit` (top-level `apps[]`). The resolved
    /// entrypoint (command, working_dir, env) is invoked instead of a
    /// trailing argv. Mutually exclusive with the trailing `<ARGV>...`.
    #[arg(long, value_name = "PATH", conflicts_with = "argv")]
    pub launch_plan: Option<String>,
    /// Argv to run inside the guest (use `--` to separate). Required unless
    /// `--launch-plan` is supplied.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required_unless_present = "launch_plan"
    )]
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(in crate::commands) enum RunProfile {
    /// No env injection and no host directory shares.
    Restrictive,
    /// Explicit env is allowed; host directory shares must stay read-only.
    Standard,
    /// Dev-mode ergonomics: explicit env and writable host shares are allowed.
    Dev,
    /// Escape hatch for local experiments; requires MVM_ACK_PERMISSIVE_RUN=1.
    Permissive,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct RunArgs {
    /// Boot a pre-built manifest (path to `mvm.toml`, its directory, or a
    /// legacy slot name). If omitted, the bundled default microVM image is used.
    #[arg(short = 'm', long)]
    pub manifest: Option<String>,
    /// vCPU cores (default: 2)
    #[arg(long, default_value = "2")]
    pub cpus: u32,
    /// Memory (supports human-readable: 512M, 1G, ...)
    #[arg(long, default_value = "512M")]
    pub memory: String,
    /// Security profile for the transient run.
    #[arg(long, value_enum, default_value = "standard")]
    pub profile: RunProfile,
    /// Share a host directory into the guest. Format: `HOST_PATH:/GUEST_PATH[:MODE]`.
    /// MODE defaults to `ro`; `rw` is allowed only with `--profile dev` or `permissive`.
    #[arg(short = 'd', long)]
    pub add_dir: Vec<String>,
    /// Explicit environment variable to inject (KEY=VALUE). Repeatable.
    /// Disabled by `--profile restrictive`.
    #[arg(short, long)]
    pub env: Vec<String>,
    /// Per-command timeout in seconds (default: 60)
    #[arg(long, default_value = "60")]
    pub timeout: u64,
    /// Write a signed execution receipt to this path. The receipt records
    /// command/env/mount hashes, output hashes, and exit status; it never
    /// stores raw argv, env values, stdout, or stderr.
    #[arg(long, value_name = "PATH")]
    pub receipt: Option<PathBuf>,
    /// Print a machine-readable, redacted execution summary as JSON.
    ///
    /// Guest stdout/stderr are not streamed in this mode; the JSON carries
    /// only byte counts and hashes. Combine with `--receipt` when a signed
    /// artifact is needed.
    #[arg(long)]
    pub json: bool,
    /// Path to an mvmforge launch document. Mutually exclusive with trailing argv.
    #[arg(long, value_name = "PATH", conflicts_with = "argv")]
    pub launch_plan: Option<String>,
    /// SDK transport mode for `mvmctl run`. v1 wires only
    /// `--mode plan` (synthesize an ExecutionPlan per Sandbox call
    /// and route through `mvm_supervisor::admit_for_run`; no
    /// microVM ever boots). `--mode live` is reserved for the
    /// Followup H-live half (Plan 73); `--mode record` redirects
    /// users to `mvmctl compile` (where record is the default).
    /// When unset, the verb behaves as a transient-sandbox runner
    /// over the trailing argv — its pre-Followup-H semantics.
    #[arg(long = "mode", value_enum)]
    pub mode: Option<RunMode>,
    /// Friendly alias for `--mode live`. Refused in v1 until the
    /// Followup H-live half lands.
    #[arg(long = "dev", conflicts_with_all = ["prod", "mode"])]
    pub dev: bool,
    /// Friendly alias for `--mode record`. `mvmctl run --prod`
    /// redirects users to `mvmctl compile`, where record is the
    /// default.
    #[arg(long = "prod", conflicts_with_all = ["dev", "mode"])]
    pub prod: bool,
    /// Argv to run inside the guest (use `--` to separate). Required unless
    /// `--launch-plan` is supplied. Under `--mode plan`, the first
    /// argv element is a `.py`/`.ts`/`.js` script path.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required_unless_present_any = ["launch_plan", "mode", "dev", "prod"]
    )]
    pub argv: Vec<String>,
}

/// SDK transport modes for `mvmctl run`. Mirrors the `Mode` enum on
/// `mvmctl compile` but specialises the rejection messages to point
/// users at the right verb when they pick the wrong default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(in crate::commands) enum RunMode {
    /// Live transport — Sandbox calls shell out to existing mvmctl
    /// up/exec/down. Reserved for Followup H-live; refused in v1.
    Live,
    /// Plan transport — synthesise one ExecutionPlan per Sandbox
    /// operation and route through `mvm_supervisor::admit_for_run`.
    /// No microVM boots. Useful for dry-running admission gates.
    Plan,
    /// Record transport — capture Sandbox operations into a
    /// recording and lower to a Workload. `mvmctl run --mode
    /// record` redirects users to `mvmctl compile`, whose default
    /// mode is record.
    Record,
}

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct ReceiptArgs {
    #[command(subcommand)]
    pub action: ReceiptAction,
}

#[derive(Subcommand, Debug, Clone)]
pub(in crate::commands) enum ReceiptAction {
    /// Verify a signed execution receipt emitted by `mvmctl run --receipt`.
    Verify {
        /// Receipt JSON path.
        path: PathBuf,
        /// Raw 32-byte Ed25519 public key to trust. Defaults to
        /// `~/.mvm/keys/host-signer.pub`.
        #[arg(long)]
        pubkey: Option<PathBuf>,
    },
}

impl RunArgs {
    fn into_exec_args(self) -> Args {
        Args {
            manifest: self.manifest,
            cpus: self.cpus,
            memory: self.memory,
            add_dir: self.add_dir,
            env: self.env,
            timeout: self.timeout,
            launch_plan: self.launch_plan,
            argv: self.argv,
        }
    }
}

pub(in crate::commands) fn run_receipt(
    _cli: &Cli,
    args: ReceiptArgs,
    _cfg: &MvmConfig,
) -> Result<()> {
    match args.action {
        ReceiptAction::Verify { path, pubkey } => {
            let receipt = verify_run_receipt(&path, pubkey.as_deref())?;
            println!(
                "OK receipt={} signer_id={} exit_code={}",
                receipt.payload.receipt_id,
                receipt.signature.signer_id,
                receipt.payload.outcome.exit_code
            );
            Ok(())
        }
    }
}

pub(in crate::commands) fn run_secure(cli: &Cli, args: RunArgs, cfg: &MvmConfig) -> Result<()> {
    // Followup H — when an SDK transport mode is requested, peel off
    // the SDK-shaped surface before the sandbox-runner validation
    // kicks in. `--dev` (alias for live) is refused in v1; `--prod`
    // (alias for record) redirects to `mvmctl compile`; `--mode plan`
    // routes through the plan-mode admission dry-run.
    if let Some(mode) = resolve_run_mode(&args)? {
        return super::run_plan::dispatch_sdk_mode(mode, &args);
    }
    validate_run_profile(&args)?;
    let receipt_path = args.receipt.clone();
    if args.json || receipt_path.is_some() {
        let receipt_input = ReceiptInput::from_run_args(&args)?;
        let json_requested = args.json;
        let req = build_exec_request(args.into_exec_args(), "`mvmctl run`")?;
        let output = crate::exec::run_captured(req)?;
        if !json_requested && !output.stdout.is_empty() {
            print!("{}", output.stdout);
        }
        if !json_requested && !output.stderr.is_empty() {
            eprint!("{}", output.stderr);
        }
        let summary = RunJsonSummary::from_parts(receipt_input.clone(), &output, receipt_path);
        if let Some(path) = summary.receipt_path.as_deref() {
            write_run_receipt(path, receipt_input, &output)?;
        }
        if json_requested {
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).context("serializing run JSON summary")?
            );
        }
        if output.exit_code != 0 {
            std::process::exit(output.exit_code);
        }
        return Ok(());
    }
    run(cli, args.into_exec_args(), cfg)
}

/// Resolve the `mvmctl run` transport mode from the explicit
/// `--mode` flag, the friendly `--dev` / `--prod` aliases, and the
/// `MVM_SDK_MODE` env-var override. Returns `Ok(None)` when no SDK
/// mode was requested — in that case the verb falls back to the
/// transient-sandbox runner over the trailing argv.
///
/// Env-var precedence matches `mvmctl compile`: `MVM_SDK_MODE`
/// supersedes any flag-only override so a wrapper script can pin a
/// mode without the user retyping `--mode`.
pub(in crate::commands) fn resolve_run_mode(args: &RunArgs) -> Result<Option<RunMode>> {
    if let Ok(env_mode) = std::env::var("MVM_SDK_MODE") {
        return Ok(Some(parse_env_run_mode(&env_mode)?));
    }
    if args.dev {
        anyhow::bail!(
            "`mvmctl run --dev` (alias for --mode live) is blocked — pairs with Followup H-live, \
             which lands once Plan 72 W4/W5 are validated end to end. Use `--mode plan` for the \
             admission dry-run, or `mvmctl compile` for the record path."
        );
    }
    if args.prod {
        anyhow::bail!(
            "`mvmctl run --prod` (alias for --mode record) redirects to `mvmctl compile`, where \
             record is the default mode. Re-run as `mvmctl compile <script>` (the trailing argv \
             on `mvmctl run` is for the live sandbox runner, not for SDK record-mode)."
        );
    }
    match args.mode {
        None => Ok(None),
        Some(RunMode::Live) => anyhow::bail!(
            "`mvmctl run --mode live` is blocked — pairs with Followup H-live, which lands once \
             Plan 72 W4/W5 are validated end to end. Use `--mode plan` for the admission dry-run."
        ),
        Some(RunMode::Record) => anyhow::bail!(
            "`mvmctl run --mode record` is unsupported — `mvmctl compile` is the record-mode verb \
             (record is the default; pass the script as the positional entry)."
        ),
        Some(RunMode::Plan) => Ok(Some(RunMode::Plan)),
    }
}

fn parse_env_run_mode(raw: &str) -> Result<RunMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "live" => anyhow::bail!(
            "MVM_SDK_MODE=live is blocked on `mvmctl run` — pairs with Followup H-live (lands \
             once Plan 72 W4/W5 are validated)."
        ),
        "plan" => Ok(RunMode::Plan),
        "record" => anyhow::bail!(
            "MVM_SDK_MODE=record on `mvmctl run` is unsupported — `mvmctl compile` is the \
             record-mode verb (record is its default)."
        ),
        other => anyhow::bail!(
            "MVM_SDK_MODE={other:?} is not recognized; expected one of: live, plan, record"
        ),
    }
}

fn validate_run_profile(args: &RunArgs) -> Result<()> {
    if args.profile == RunProfile::Permissive
        && std::env::var_os("MVM_ACK_PERMISSIVE_RUN").is_none()
    {
        anyhow::bail!(
            "--profile permissive requires MVM_ACK_PERMISSIVE_RUN=1 so broad local execution is explicit"
        );
    }

    if args.profile == RunProfile::Restrictive {
        if !args.env.is_empty() {
            anyhow::bail!("--profile restrictive does not allow --env");
        }
        if !args.add_dir.is_empty() {
            anyhow::bail!("--profile restrictive does not allow --add-dir");
        }
    }

    if matches!(args.profile, RunProfile::Restrictive | RunProfile::Standard) {
        for spec in &args.add_dir {
            if !crate::exec::AddDir::parse(spec)?.read_only {
                anyhow::bail!(
                    "--add-dir '{spec}' requests rw; use --profile dev for writable host shares"
                );
            }
        }
    }

    Ok(())
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let req = build_exec_request(args, "`mvmctl exec`")?;
    let exit_code = crate::exec::run(req)?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn build_exec_request(args: Args, command_name: &str) -> Result<crate::exec::ExecRequest> {
    let target = match (args.launch_plan.as_ref(), args.argv.is_empty()) {
        (Some(_), false) => {
            anyhow::bail!("--launch-plan and a trailing argv are mutually exclusive");
        }
        (Some(path), true) => {
            let entrypoint = crate::exec::load_launch_plan(std::path::Path::new(path))?;
            crate::exec::ExecTarget::LaunchPlan { entrypoint }
        }
        (None, true) => {
            anyhow::bail!(
                "{command_name} requires a command (after `--`) or `--launch-plan <PATH>`"
            )
        }
        (None, false) => crate::exec::ExecTarget::Inline { argv: args.argv },
    };
    let memory_mib = parse_human_size(&args.memory).context("Invalid --memory")?;
    let mut add_dirs = Vec::with_capacity(args.add_dir.len());
    for spec in &args.add_dir {
        add_dirs.push(crate::exec::AddDir::parse(spec)?);
    }
    let mut env_pairs = Vec::with_capacity(args.env.len());
    for kv in &args.env {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--env '{kv}': expected KEY=VALUE"))?;
        if k.is_empty() {
            anyhow::bail!("--env '{kv}': KEY must not be empty");
        }
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || k.starts_with(|c: char| c.is_ascii_digit())
        {
            anyhow::bail!("--env '{kv}': KEY must match [A-Za-z_][A-Za-z0-9_]* (got '{k}')");
        }
        env_pairs.push((k.to_string(), v.to_string()));
    }
    // Plan 38 §4: --manifest <PATH> accepts a manifest path / dir in
    // addition to legacy names. Resolve up front so the downstream
    // ImageSource::Template carries either a name (legacy) or a slot
    // hash (manifest), and the dispatched lifecycle helpers handle
    // both keys transparently.
    let image = match args.manifest {
        Some(arg) => {
            let resolved = match super::shared::resolve_manifest_arg(&arg)? {
                super::shared::ManifestArgRef::Name(n) => n,
                super::shared::ManifestArgRef::Slot { slot_hash } => slot_hash,
            };
            crate::exec::ImageSource::Template(resolved)
        }
        None => {
            ui::info("No --manifest specified; using bundled default microVM image.");
            let (kernel_path, rootfs_path) = ensure_default_microvm_image()?;
            crate::exec::ImageSource::Prebuilt {
                kernel_path,
                rootfs_path,
                initrd_path: None,
                label: "default-microvm".to_string(),
            }
        }
    };
    Ok(crate::exec::ExecRequest {
        image,
        cpus: args.cpus,
        memory_mib,
        // mvmctl exec is a one-shot transient; no balloon plumbing
        // here yet. The manifest-driven path on mvmctl up is where
        // mem_initial gets sourced for long-running workloads.
        mem_initial_mib: None,
        add_dirs,
        env: env_pairs,
        target,
        timeout_secs: args.timeout,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignedRunReceipt {
    payload: RunReceiptPayload,
    signature: RunReceiptSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunReceiptPayload {
    schema_version: u32,
    receipt_id: String,
    recorded_at: String,
    invocation: ReceiptInput,
    outcome: ReceiptOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiptInput {
    manifest: Option<String>,
    cpus: u32,
    memory: String,
    profile: String,
    command: ReceiptCommand,
    env_keys: Vec<String>,
    add_dirs: Vec<ReceiptAddDir>,
    timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReceiptCommand {
    Inline {
        argv_len: usize,
        argv_sha256: String,
    },
    LaunchPlan {
        path_sha256: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiptAddDir {
    host_path_sha256: String,
    guest_path: String,
    read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiptOutcome {
    exit_code: i32,
    success: bool,
    stdout_sha256: String,
    stderr_sha256: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunReceiptSignature {
    algorithm: String,
    signer_id: String,
    public_key_sha256: String,
    signature_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunJsonSummary {
    schema_version: u32,
    invocation: ReceiptInput,
    outcome: ReceiptOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt_path: Option<PathBuf>,
}

impl RunJsonSummary {
    fn from_parts(
        invocation: ReceiptInput,
        output: &crate::exec::ExecOutput,
        receipt_path: Option<PathBuf>,
    ) -> Self {
        Self {
            schema_version: 1,
            invocation,
            outcome: ReceiptOutcome::from_exec_output(output),
            receipt_path,
        }
    }
}

impl ReceiptInput {
    fn from_run_args(args: &RunArgs) -> Result<Self> {
        let command = if let Some(path) = &args.launch_plan {
            ReceiptCommand::LaunchPlan {
                path_sha256: sha256_hex(path.as_bytes()),
            }
        } else {
            let argv_bytes =
                serde_json::to_vec(&args.argv).context("serializing argv for receipt hash")?;
            ReceiptCommand::Inline {
                argv_len: args.argv.len(),
                argv_sha256: sha256_hex(&argv_bytes),
            }
        };

        let mut env_keys = Vec::with_capacity(args.env.len());
        for kv in &args.env {
            let (key, _) = kv
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--env '{kv}': expected KEY=VALUE"))?;
            env_keys.push(key.to_string());
        }
        env_keys.sort();

        let mut add_dirs = Vec::with_capacity(args.add_dir.len());
        for spec in &args.add_dir {
            let parsed = crate::exec::AddDir::parse(spec)?;
            add_dirs.push(ReceiptAddDir {
                host_path_sha256: sha256_hex(parsed.host_path.as_bytes()),
                guest_path: parsed.guest_path,
                read_only: parsed.read_only,
            });
        }

        Ok(Self {
            manifest: args.manifest.clone(),
            cpus: args.cpus,
            memory: args.memory.clone(),
            profile: args
                .profile
                .to_possible_value()
                .expect("value enum")
                .get_name()
                .to_string(),
            command,
            env_keys,
            add_dirs,
            timeout_secs: args.timeout,
        })
    }
}

impl ReceiptOutcome {
    fn from_exec_output(output: &crate::exec::ExecOutput) -> Self {
        Self {
            exit_code: output.exit_code,
            success: output.exit_code == 0,
            stdout_sha256: sha256_hex(output.stdout.as_bytes()),
            stderr_sha256: sha256_hex(output.stderr.as_bytes()),
            stdout_bytes: output.stdout.len(),
            stderr_bytes: output.stderr.len(),
        }
    }
}

fn write_run_receipt(
    path: &Path,
    invocation: ReceiptInput,
    output: &crate::exec::ExecOutput,
) -> Result<()> {
    let payload = RunReceiptPayload {
        schema_version: 1,
        receipt_id: uuid::Uuid::new_v4().to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
        invocation,
        outcome: ReceiptOutcome::from_exec_output(output),
    };
    let payload_bytes = serde_json::to_vec(&payload).context("serializing run receipt payload")?;
    let signer = load_or_init().context("loading host signer for run receipt")?;
    let signature = signer.signing.sign(&payload_bytes);
    let public_key = signer.verifying.to_bytes();
    let receipt = SignedRunReceipt {
        payload,
        signature: RunReceiptSignature {
            algorithm: "ed25519".to_string(),
            signer_id: host_signer_id(),
            public_key_sha256: sha256_hex(&public_key),
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        },
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating receipt directory {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&receipt).context("serializing run receipt")?;
    std::fs::write(path, bytes).with_context(|| format!("writing receipt {}", path.display()))?;
    Ok(())
}

fn verify_run_receipt(path: &Path, pubkey_path: Option<&Path>) -> Result<SignedRunReceipt> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading receipt {}", path.display()))?;
    let receipt: SignedRunReceipt = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing receipt {}", path.display()))?;
    if receipt.payload.schema_version != 1 {
        anyhow::bail!(
            "unsupported receipt schema_version {}; this build supports 1",
            receipt.payload.schema_version
        );
    }
    if !receipt.signature.algorithm.eq_ignore_ascii_case("ed25519") {
        anyhow::bail!(
            "unsupported receipt signature algorithm '{}'",
            receipt.signature.algorithm
        );
    }
    let verifying = load_receipt_pubkey(pubkey_path)?;
    let public_key = verifying.to_bytes();
    let actual_key_hash = sha256_hex(&public_key);
    if actual_key_hash != receipt.signature.public_key_sha256 {
        anyhow::bail!(
            "receipt was signed by public key {}; trusted key is {}",
            receipt.signature.public_key_sha256,
            actual_key_hash
        );
    }

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&receipt.signature.signature_base64)
        .context("decoding receipt signature")?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|e| anyhow::anyhow!("invalid receipt signature bytes: {e}"))?;
    let payload_bytes =
        serde_json::to_vec(&receipt.payload).context("serializing receipt payload")?;
    verifying
        .verify(&payload_bytes, &signature)
        .map_err(|e| anyhow::anyhow!("receipt signature verification failed: {e}"))?;
    Ok(receipt)
}

fn load_receipt_pubkey(path: Option<&Path>) -> Result<VerifyingKey> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => super::host_signer::default_keys_dir()?.join(PUBLIC_FILENAME),
    };
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading trusted receipt public key {}", path.display()))?;
    let key: [u8; super::host_signer::KEY_BYTES] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("{} must contain exactly 32 bytes", path.display()))?;
    VerifyingKey::from_bytes(&key).with_context(|| format!("parsing {}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_args(profile: RunProfile) -> RunArgs {
        RunArgs {
            manifest: None,
            cpus: 2,
            memory: "512M".to_string(),
            profile,
            add_dir: Vec::new(),
            env: Vec::new(),
            timeout: 60,
            receipt: None,
            json: false,
            launch_plan: None,
            mode: None,
            dev: false,
            prod: false,
            argv: vec!["/bin/true".to_string()],
        }
    }

    #[test]
    fn resolve_run_mode_returns_none_when_no_mode_flag() {
        let args = run_args(RunProfile::Standard);
        let mode = resolve_run_mode(&args).expect("no flag resolves to None");
        assert!(mode.is_none());
    }

    #[test]
    fn resolve_run_mode_returns_plan_when_mode_plan() {
        let mut args = run_args(RunProfile::Standard);
        args.mode = Some(RunMode::Plan);
        let mode = resolve_run_mode(&args).expect("plan resolves").unwrap();
        assert_eq!(mode, RunMode::Plan);
    }

    #[test]
    fn resolve_run_mode_bails_blocked_for_dev_alias() {
        let mut args = run_args(RunProfile::Standard);
        args.dev = true;
        let err = resolve_run_mode(&args).expect_err("--dev must bail");
        let msg = err.to_string();
        assert!(msg.contains("Followup H-live"));
        assert!(msg.contains("--mode plan"));
    }

    #[test]
    fn resolve_run_mode_bails_redirect_for_prod_alias() {
        let mut args = run_args(RunProfile::Standard);
        args.prod = true;
        let err = resolve_run_mode(&args).expect_err("--prod must bail");
        let msg = err.to_string();
        assert!(msg.contains("mvmctl compile"));
    }

    #[test]
    fn resolve_run_mode_bails_blocked_for_mode_live() {
        let mut args = run_args(RunProfile::Standard);
        args.mode = Some(RunMode::Live);
        let err = resolve_run_mode(&args).expect_err("--mode live must bail");
        let msg = err.to_string();
        assert!(msg.contains("Followup H-live"));
    }

    #[test]
    fn resolve_run_mode_bails_redirect_for_mode_record() {
        let mut args = run_args(RunProfile::Standard);
        args.mode = Some(RunMode::Record);
        let err = resolve_run_mode(&args).expect_err("--mode record must bail");
        let msg = err.to_string();
        assert!(msg.contains("mvmctl compile"));
    }

    #[test]
    fn standard_profile_rejects_writable_host_share() {
        let mut args = run_args(RunProfile::Standard);
        args.add_dir.push(".:/work:rw".to_string());

        let err = validate_run_profile(&args).expect_err("standard rejects rw share");
        assert!(err.to_string().contains("requests rw"));
    }

    #[test]
    fn restrictive_profile_rejects_env() {
        let mut args = run_args(RunProfile::Restrictive);
        args.env.push("FOO=bar".to_string());

        let err = validate_run_profile(&args).expect_err("restrictive rejects env");
        assert!(err.to_string().contains("does not allow --env"));
    }

    #[test]
    fn restrictive_profile_rejects_host_share() {
        let mut args = run_args(RunProfile::Restrictive);
        args.add_dir.push(".:/work".to_string());

        let err = validate_run_profile(&args).expect_err("restrictive rejects shares");
        assert!(err.to_string().contains("does not allow --add-dir"));
    }

    #[test]
    fn dev_profile_allows_writable_host_share() {
        let mut args = run_args(RunProfile::Dev);
        args.add_dir.push(".:/work:rw".to_string());

        validate_run_profile(&args).expect("dev allows rw share");
    }

    #[test]
    fn receipt_input_hashes_sensitive_fields() {
        let mut args = run_args(RunProfile::Dev);
        args.argv = vec!["curl".to_string(), "token-secret".to_string()];
        args.env.push("API_TOKEN=secret-value".to_string());
        args.add_dir.push("/private/project:/work:ro".to_string());

        let receipt = ReceiptInput::from_run_args(&args).expect("receipt input");
        let json = serde_json::to_string(&receipt).expect("json");

        assert!(!json.contains("token-secret"));
        assert!(!json.contains("secret-value"));
        assert!(!json.contains("/private/project"));
        assert!(json.contains("API_TOKEN"));
        assert!(json.contains("/work"));
    }

    #[test]
    fn receipt_outcome_hashes_output_without_storing_output() {
        let output = crate::exec::ExecOutput {
            exit_code: 7,
            stdout: "secret stdout".to_string(),
            stderr: "secret stderr".to_string(),
        };

        let outcome = ReceiptOutcome::from_exec_output(&output);
        let json = serde_json::to_string(&outcome).expect("json");

        assert_eq!(outcome.exit_code, 7);
        assert!(!json.contains("secret stdout"));
        assert!(!json.contains("secret stderr"));
        assert_eq!(outcome.stdout_bytes, "secret stdout".len());
        assert_eq!(outcome.stderr_bytes, "secret stderr".len());
    }

    #[test]
    fn run_json_summary_omits_raw_output() {
        let args = run_args(RunProfile::Standard);
        let output = crate::exec::ExecOutput {
            exit_code: 0,
            stdout: "sensitive stdout".to_string(),
            stderr: "sensitive stderr".to_string(),
        };
        let summary = RunJsonSummary::from_parts(
            ReceiptInput::from_run_args(&args).expect("receipt input"),
            &output,
            Some(PathBuf::from("/tmp/receipt.json")),
        );
        let json = serde_json::to_string(&summary).expect("serialize summary");
        assert!(json.contains("stdout_sha256"));
        assert!(json.contains("stderr_sha256"));
        assert!(json.contains("/tmp/receipt.json"));
        assert!(!json.contains("sensitive stdout"));
        assert!(!json.contains("sensitive stderr"));
    }

    #[test]
    fn verify_run_receipt_accepts_valid_signature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let receipt_path = dir.path().join("receipt.json");
        let pubkey_path = dir.path().join("host.pub");
        let signing = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        std::fs::write(&pubkey_path, signing.verifying_key().to_bytes()).expect("pubkey");

        let args = run_args(RunProfile::Standard);
        let payload = RunReceiptPayload {
            schema_version: 1,
            receipt_id: "receipt-1".to_string(),
            recorded_at: "2026-05-14T00:00:00Z".to_string(),
            invocation: ReceiptInput::from_run_args(&args).expect("receipt input"),
            outcome: ReceiptOutcome {
                exit_code: 0,
                success: true,
                stdout_sha256: sha256_hex(b""),
                stderr_sha256: sha256_hex(b""),
                stdout_bytes: 0,
                stderr_bytes: 0,
            },
        };
        let payload_bytes = serde_json::to_vec(&payload).expect("payload");
        let signature = signing.sign(&payload_bytes);
        let receipt = SignedRunReceipt {
            payload,
            signature: RunReceiptSignature {
                algorithm: "ed25519".to_string(),
                signer_id: "host:test".to_string(),
                public_key_sha256: sha256_hex(&signing.verifying_key().to_bytes()),
                signature_base64: base64::engine::general_purpose::STANDARD
                    .encode(signature.to_bytes()),
            },
        };
        std::fs::write(
            &receipt_path,
            serde_json::to_vec_pretty(&receipt).expect("receipt json"),
        )
        .expect("write receipt");

        verify_run_receipt(&receipt_path, Some(&pubkey_path)).expect("valid receipt");
    }

    #[test]
    fn verify_run_receipt_rejects_tampered_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let receipt_path = dir.path().join("receipt.json");
        let pubkey_path = dir.path().join("host.pub");
        let signing = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        std::fs::write(&pubkey_path, signing.verifying_key().to_bytes()).expect("pubkey");

        let args = run_args(RunProfile::Standard);
        let mut payload = RunReceiptPayload {
            schema_version: 1,
            receipt_id: "receipt-1".to_string(),
            recorded_at: "2026-05-14T00:00:00Z".to_string(),
            invocation: ReceiptInput::from_run_args(&args).expect("receipt input"),
            outcome: ReceiptOutcome {
                exit_code: 0,
                success: true,
                stdout_sha256: sha256_hex(b""),
                stderr_sha256: sha256_hex(b""),
                stdout_bytes: 0,
                stderr_bytes: 0,
            },
        };
        let payload_bytes = serde_json::to_vec(&payload).expect("payload");
        let signature = signing.sign(&payload_bytes);
        payload.outcome.exit_code = 1;
        let receipt = SignedRunReceipt {
            payload,
            signature: RunReceiptSignature {
                algorithm: "ed25519".to_string(),
                signer_id: "host:test".to_string(),
                public_key_sha256: sha256_hex(&signing.verifying_key().to_bytes()),
                signature_base64: base64::engine::general_purpose::STANDARD
                    .encode(signature.to_bytes()),
            },
        };
        std::fs::write(
            &receipt_path,
            serde_json::to_vec_pretty(&receipt).expect("receipt json"),
        )
        .expect("write receipt");

        let err = verify_run_receipt(&receipt_path, Some(&pubkey_path))
            .expect_err("tampered receipt rejected");
        assert!(err.to_string().contains("signature verification failed"));
    }
}
