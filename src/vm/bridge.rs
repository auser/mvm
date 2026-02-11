use anyhow::Result;
use serde::Serialize;

use crate::infra::shell;
use crate::vm::tenant::config::TenantNet;

/// Ensure a per-tenant bridge exists with the coordinator-assigned subnet.
///
/// Idempotent: checks if bridge already exists before creating.
/// Bridge name: br-tenant-<tenant_net_id>
/// Gateway: first usable IP in subnet (e.g., 10.240.3.1/24)
pub fn ensure_tenant_bridge(net: &TenantNet) -> Result<()> {
    let bridge = &net.bridge_name;
    let gateway = &net.gateway_ip;
    let subnet = &net.ipv4_subnet;

    // Parse CIDR prefix length from subnet
    let cidr = subnet.split('/').nth(1).unwrap_or("24");

    shell::run_in_vm(&format!(
        r#"
        # Enable IP forwarding (idempotent)
        echo 1 > /proc/sys/net/ipv4/ip_forward 2>/dev/null || true

        # Create bridge if it doesn't exist
        if ! ip link show {bridge} >/dev/null 2>&1; then
            sudo ip link add {bridge} type bridge
            sudo ip addr add {gateway}/{cidr} dev {bridge}
            sudo ip link set {bridge} up
        fi

        # Ensure NAT rules exist (idempotent with -C check)
        sudo iptables -t nat -C POSTROUTING -s {subnet} ! -o {bridge} -j MASQUERADE 2>/dev/null || \
            sudo iptables -t nat -A POSTROUTING -s {subnet} ! -o {bridge} -j MASQUERADE

        sudo iptables -C FORWARD -i {bridge} ! -o {bridge} -j ACCEPT 2>/dev/null || \
            sudo iptables -A FORWARD -i {bridge} ! -o {bridge} -j ACCEPT

        sudo iptables -C FORWARD ! -i {bridge} -o {bridge} -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || \
            sudo iptables -A FORWARD ! -i {bridge} -o {bridge} -m state --state RELATED,ESTABLISHED -j ACCEPT
        "#,
        bridge = bridge,
        gateway = gateway,
        cidr = cidr,
        subnet = subnet,
    ))?;

    Ok(())
}

/// Destroy a tenant bridge and its NAT rules.
pub fn destroy_tenant_bridge(net: &TenantNet) -> Result<()> {
    let bridge = &net.bridge_name;
    let subnet = &net.ipv4_subnet;

    shell::run_in_vm(&format!(
        r#"
        sudo ip link set {bridge} down 2>/dev/null || true
        sudo ip link del {bridge} 2>/dev/null || true

        sudo iptables -t nat -D POSTROUTING -s {subnet} ! -o {bridge} -j MASQUERADE 2>/dev/null || true
        sudo iptables -D FORWARD -i {bridge} ! -o {bridge} -j ACCEPT 2>/dev/null || true
        sudo iptables -D FORWARD ! -i {bridge} -o {bridge} -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || true
        "#,
        bridge = bridge,
        subnet = subnet,
    ))?;

    Ok(())
}

/// Full network health report for a tenant bridge.
#[derive(Debug, Serialize)]
pub struct BridgeReport {
    pub tenant_id: String,
    pub bridge_name: String,
    pub subnet: String,
    pub gateway: String,
    pub bridge_exists: bool,
    pub bridge_up: bool,
    pub gateway_assigned: bool,
    pub nat_masquerade: bool,
    pub forward_outbound: bool,
    pub forward_established: bool,
    pub tap_devices: Vec<String>,
    pub issues: Vec<String>,
}

/// Verify a tenant bridge is correctly configured.
/// Returns a detailed report of all checks.
pub fn verify_tenant_bridge(net: &TenantNet) -> Result<Vec<String>> {
    let report = full_bridge_report("", net)?;
    Ok(report.issues)
}

/// Generate a full bridge health report for a tenant.
pub fn full_bridge_report(tenant_id: &str, net: &TenantNet) -> Result<BridgeReport> {
    let bridge = &net.bridge_name;
    let subnet = &net.ipv4_subnet;
    let cidr = subnet.split('/').nth(1).unwrap_or("24");
    let expected_gateway = format!("{}/{}", net.gateway_ip, cidr);

    let mut report = BridgeReport {
        tenant_id: tenant_id.to_string(),
        bridge_name: bridge.clone(),
        subnet: subnet.clone(),
        gateway: net.gateway_ip.clone(),
        bridge_exists: false,
        bridge_up: false,
        gateway_assigned: false,
        nat_masquerade: false,
        forward_outbound: false,
        forward_established: false,
        tap_devices: Vec::new(),
        issues: Vec::new(),
    };

    // Check 1: Bridge exists
    let exists = shell::run_in_vm_stdout(&format!(
        "ip link show {} >/dev/null 2>&1 && echo yes || echo no",
        bridge
    ))?;
    report.bridge_exists = exists.trim() == "yes";

    if !report.bridge_exists {
        report
            .issues
            .push(format!("Bridge {} does not exist", bridge));
        return Ok(report);
    }

    // Check 2: Bridge is UP
    let state = shell::run_in_vm_stdout(&format!(
        "ip link show {} | grep -oP '(?<=state )\\w+'",
        bridge
    ))?;
    report.bridge_up = state.trim() == "UP";
    if !report.bridge_up {
        report.issues.push(format!(
            "Bridge {} is not UP (state: {})",
            bridge,
            state.trim()
        ));
    }

    // Check 3: Gateway IP assigned
    let addrs = shell::run_in_vm_stdout(&format!(
        "ip addr show dev {} | grep 'inet ' | awk '{{print $2}}'",
        bridge
    ))?;
    report.gateway_assigned = addrs.contains(&expected_gateway);
    if !report.gateway_assigned {
        report.issues.push(format!(
            "Bridge {} missing gateway {} (found: {})",
            bridge,
            expected_gateway,
            addrs.trim()
        ));
    }

    // Check 4: NAT masquerade rule
    let nat = shell::run_in_vm_stdout(&format!(
        "sudo iptables -t nat -C POSTROUTING -s {} ! -o {} -j MASQUERADE 2>&1 && echo yes || echo no",
        subnet, bridge
    ))?;
    report.nat_masquerade = nat.trim().ends_with("yes");
    if !report.nat_masquerade {
        report.issues.push(format!(
            "Missing NAT masquerade rule for {} on {}",
            subnet, bridge
        ));
    }

    // Check 5: Forward outbound rule
    let fwd_out = shell::run_in_vm_stdout(&format!(
        "sudo iptables -C FORWARD -i {} ! -o {} -j ACCEPT 2>&1 && echo yes || echo no",
        bridge, bridge
    ))?;
    report.forward_outbound = fwd_out.trim().ends_with("yes");
    if !report.forward_outbound {
        report
            .issues
            .push(format!("Missing FORWARD outbound rule for {}", bridge));
    }

    // Check 6: Forward established rule
    let fwd_est = shell::run_in_vm_stdout(&format!(
        "sudo iptables -C FORWARD ! -i {} -o {} -m state --state RELATED,ESTABLISHED -j ACCEPT 2>&1 && echo yes || echo no",
        bridge, bridge
    ))?;
    report.forward_established = fwd_est.trim().ends_with("yes");
    if !report.forward_established {
        report
            .issues
            .push(format!("Missing FORWARD established rule for {}", bridge));
    }

    // Check 7: List TAP devices attached to this bridge
    let taps = shell::run_in_vm_stdout(&format!(
        "ls /sys/class/net/{}/brif/ 2>/dev/null || true",
        bridge
    ))?;
    report.tap_devices = taps
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    Ok(report)
}
