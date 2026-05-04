//! `mvmctl down` — stop one or more running VMs.

use anyhow::Result;
use clap::Args as ClapArgs;

use mvm_core::user_config::MvmConfig;
use mvm_core::vm_backend::VmId;
use mvm_runtime::vm::backend::AnyBackend;

use super::Cli;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// VM name to stop (or all VMs if omitted)
    pub name: Option<String>,
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    // Use Apple Container backend on macOS 26+, otherwise default (Firecracker).
    let backend = if mvm_core::platform::current().has_apple_containers() {
        AnyBackend::from_hypervisor("apple-container")
    } else {
        AnyBackend::default_backend()
    };
    match args.name.as_deref() {
        Some(n) => {
            let result = backend.stop(&VmId::from(n));
            // Deregister from the name registry (best-effort)
            let registry_path = mvm_runtime::vm::name_registry::registry_path();
            if let Ok(mut registry) =
                mvm_runtime::vm::name_registry::VmNameRegistry::load(&registry_path)
            {
                registry.deregister(n);
                let _ = registry.save(&registry_path);
            }
            // B21: state-changing CLI verb emits an audit entry. The
            // matching VmStart emit lives in `vm/up.rs`; without this
            // VmStop there is no audit trail of the stop happening.
            // Best-effort — the underlying op already succeeded or
            // failed by the time we reach here.
            mvm_core::audit::emit(
                mvm_core::audit::LocalAuditKind::VmStop,
                Some(n),
                Some(if result.is_ok() { "ok" } else { "stop_failed" }),
            );
            result
        }
        None => {
            // Plan-38 §"Boundary statement": fleet/multi-VM is mvmd's job.
            // `mvmctl down` (no args) just stops every running VM.
            let result = backend.stop_all();
            mvm_core::audit::emit(
                mvm_core::audit::LocalAuditKind::VmStop,
                None,
                Some(if result.is_ok() {
                    "stop_all_ok"
                } else {
                    "stop_all_failed"
                }),
            );
            result
        }
    }
}
