use anyhow::Result;

use crate::config::*;
use crate::shell::run_in_vm_visible;

/// Set up TAP networking, IP forwarding, and NAT inside the Lima VM.
pub fn setup() -> Result<()> {
    println!("[mvm] Setting up network...");
    run_in_vm_visible(&format!(
        r#"
        set -euo pipefail

        # Create TAP device
        sudo ip link del {tap} 2>/dev/null || true
        sudo ip tuntap add dev {tap} mode tap
        sudo ip addr add {tap_ip}{mask} dev {tap}
        sudo ip link set dev {tap} up

        # Enable IP forwarding
        sudo sh -c "echo 1 > /proc/sys/net/ipv4/ip_forward"
        sudo iptables -P FORWARD ACCEPT

        # Determine host network interface
        HOST_IFACE=$(ip -j route list default | jq -r '.[0].dev')

        # NAT for internet access
        sudo iptables -t nat -D POSTROUTING -o "$HOST_IFACE" -j MASQUERADE 2>/dev/null || true
        sudo iptables -t nat -A POSTROUTING -o "$HOST_IFACE" -j MASQUERADE

        echo "[mvm] Network ready (tap={tap}, host=$HOST_IFACE)."
        "#,
        tap = TAP_DEV,
        tap_ip = TAP_IP,
        mask = MASK_SHORT,
    ))
}

/// Tear down TAP device and iptables rules.
pub fn teardown() -> Result<()> {
    run_in_vm_visible(&format!(
        r#"
        sudo ip link del {tap} 2>/dev/null || true
        HOST_IFACE=$(ip -j route list default 2>/dev/null | jq -r '.[0].dev' 2>/dev/null) || true
        if [ -n "$HOST_IFACE" ]; then
            sudo iptables -t nat -D POSTROUTING -o "$HOST_IFACE" -j MASQUERADE 2>/dev/null || true
        fi
        "#,
        tap = TAP_DEV,
    ))
}
