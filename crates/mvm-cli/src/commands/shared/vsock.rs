//! Vsock helpers for talking to the in-guest agent.
//!
//! Routes through `mvm::vsock_transport::AppleContainerTransport`
//! intentionally — these helpers serve `mvmctl up` flows that today
//! only target the Apple Container backend (Firecracker `up` uses a
//! different code path). If/when a Firecracker `up` lands, swap to
//! `vsock_transport::for_vm`.

use anyhow::Result;

use mvm::vsock_transport::{AppleContainerTransport, VsockTransport};

/// Wait for the guest agent to complete the ADR-050 / plan 74 W1
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
