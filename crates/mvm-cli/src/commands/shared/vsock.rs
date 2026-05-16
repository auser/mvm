//! Vsock helpers for talking to the in-guest agent.
//!
//! Routes through `mvm::vsock_transport::AppleContainerTransport`
//! intentionally — these helpers serve `mvmctl up` flows that today
//! only target the Apple Container backend (Firecracker `up` uses a
//! different code path). If/when a Firecracker `up` lands, swap to
//! `vsock_transport::for_vm`.

use anyhow::Result;

use mvm::vsock_transport::{AppleContainerTransport, VsockTransport};

/// Wait for the guest agent to complete the ADR-053 / plan 74 W1
/// protocol hello over vsock. Returns true once the agent has
/// answered `ProtocolHelloAck` (with at least the `Ping` capability)
/// within `timeout_secs`. A `ProtocolMismatch` answer, a transport
/// error, or an unexpected response counts as "not ready yet" and the
/// probe keeps polling until the deadline.
pub fn wait_for_guest_agent(vm_id: &str, timeout_secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let transport = AppleContainerTransport::new(vm_id);

    while std::time::Instant::now() < deadline {
        if let Ok(mut s) = transport.connect(mvm_guest::vsock::GUEST_AGENT_PORT)
            && mvm_guest::vsock::negotiate_protocol(
                &mut s,
                vec![mvm_guest::vsock::GuestCapability::Ping],
            )
            .is_ok()
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    false
}

/// Tell the guest agent to start a vsock→TCP forwarder for the given port.
pub fn request_port_forward(vm_id: &str, guest_port: u16) -> Result<u32> {
    let transport = AppleContainerTransport::new(vm_id);
    let mut stream = transport.connect(mvm_guest::vsock::GUEST_AGENT_PORT)?;
    mvm_guest::vsock::start_port_forward_on(&mut stream, guest_port)
}

/// Plan 74 W2 / Plan 51 W6 — emit a `LocalAuditKind::NetworkPolicyAllow`
/// audit record for one host→guest vsock RPC. Pairs with
/// `GuestRequest::kind_name()`; the verb name lands in the audit
/// detail as `verb=<kebab-name>`.
///
/// Detail format (mvmd ADR 0022 §"Audit-first principle"):
///
/// ```text
/// scope=rpc,direction=in,kind=vsock,verb=<name>
/// ```
///
/// The `vm` field carries the target VM name. mvmd ADR 0022 §item 2
/// names this as part of the inbound audit story — Plan 37 §6
/// invariant "every state-changing CLI verb emits ≥1 audit" extends
/// to the underlying vsock messages each verb dispatches.
///
/// Pure host-side helper — does not touch the network or the guest;
/// just records the intent. The audit_emit! macro writes to the
/// default audit log path.
///
/// Verbs to migrate (each in a follow-up slice) include `Exec`,
/// `RunEntrypoint`, `RunCode`, `FsRead`/`FsWrite`/`FsList`,
/// `ProcStart`/`ProcSignal`, `MountVolume`/`UnmountVolume`,
/// `ConsoleOpen`. The Ping poll loop in `wait_for_guest_agent`
/// deliberately *doesn't* migrate — every poll iteration would
/// audit-spam (mostly while waiting for the agent to bind), so
/// readiness probes get a separate `AgentReady` LocalAudit event
/// in the `mvmctl up` flow already.
pub fn emit_vsock_rpc_audit(vm_id: &str, request: &mvm_guest::vsock::GuestRequest) {
    let verb = request.kind_name();
    mvm_core::audit_emit!(
        NetworkPolicyAllow,
        vm: vm_id,
        "scope=rpc,direction=in,kind=vsock,verb={verb}",
        verb = verb,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The detail format string follows the convention pinned
    /// in mvm PR #275 + mvmd ADR 0022. The audit-emit macro
    /// writes to the default audit log path so we can't observe
    /// the record's content directly without log-pointer
    /// plumbing — the contract here is that the function
    /// composes cleanly with every variant.
    #[test]
    fn emit_vsock_rpc_audit_does_not_panic_on_common_verbs() {
        let cases = [
            mvm_guest::vsock::GuestRequest::Ping,
            mvm_guest::vsock::GuestRequest::ReadinessStatus,
            mvm_guest::vsock::GuestRequest::EntrypointStatus,
            mvm_guest::vsock::GuestRequest::FsDiff,
            mvm_guest::vsock::GuestRequest::Exec {
                command: "echo hello".to_string(),
                stdin: None,
                timeout_secs: Some(30),
            },
        ];
        for req in cases {
            emit_vsock_rpc_audit("vm-test", &req);
        }
    }
}
