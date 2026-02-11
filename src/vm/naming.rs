use anyhow::{Result, bail};

/// Validate a tenant or pool ID: lowercase alphanumeric + hyphens, 1-63 chars.
pub fn validate_id(id: &str, kind: &str) -> Result<()> {
    if id.is_empty() || id.len() > 63 {
        bail!("{} ID must be 1-63 characters, got {}", kind, id.len());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!(
            "{} ID must be lowercase alphanumeric + hyphens: {:?}",
            kind,
            id
        );
    }
    if id.starts_with('-') || id.ends_with('-') {
        bail!("{} ID must not start or end with a hyphen: {:?}", kind, id);
    }
    Ok(())
}

/// Generate a random instance ID: "i-" followed by 8 hex chars.
pub fn generate_instance_id() -> String {
    let bytes: [u8; 4] = rand_bytes();
    format!(
        "i-{}",
        bytes
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    )
}

/// Generate a TAP device name: tn<net_id>i<ip_offset>.
/// Max 12 chars, fits within Linux 15-char IFNAMSIZ limit.
pub fn tap_name(tenant_net_id: u16, ip_offset: u8) -> String {
    format!("tn{}i{}", tenant_net_id, ip_offset)
}

/// Deterministic MAC address from tenant_net_id and ip_offset.
/// Format: 02:xx:xx:xx:xx:xx (locally administered).
pub fn mac_address(tenant_net_id: u16, ip_offset: u8) -> String {
    let net_bytes = tenant_net_id.to_be_bytes();
    format!(
        "02:fc:{:02x}:{:02x}:00:{:02x}",
        net_bytes[0], net_bytes[1], ip_offset
    )
}

/// Simple random bytes using uuid crate (already a dependency).
fn rand_bytes() -> [u8; 4] {
    let id = uuid::Uuid::new_v4();
    let bytes = id.as_bytes();
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

/// Parse a "tenant/pool" or "tenant/pool/instance" path.
pub fn parse_pool_path(path: &str) -> Result<(&str, &str)> {
    let parts: Vec<&str> = path.splitn(3, '/').collect();
    if parts.len() < 2 {
        bail!("Expected <tenant>/<pool>, got {:?}", path);
    }
    Ok((parts[0], parts[1]))
}

/// Parse a "tenant/pool/instance" path.
pub fn parse_instance_path(path: &str) -> Result<(&str, &str, &str)> {
    let parts: Vec<&str> = path.splitn(4, '/').collect();
    if parts.len() < 3 {
        bail!("Expected <tenant>/<pool>/<instance>, got {:?}", path);
    }
    Ok((parts[0], parts[1], parts[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_id_valid() {
        assert!(validate_id("acme", "Tenant").is_ok());
        assert!(validate_id("my-pool-1", "Pool").is_ok());
        assert!(validate_id("a", "Tenant").is_ok());
    }

    #[test]
    fn test_validate_id_invalid() {
        assert!(validate_id("", "Tenant").is_err());
        assert!(validate_id("UPPER", "Tenant").is_err());
        assert!(validate_id("-leading", "Tenant").is_err());
        assert!(validate_id("trailing-", "Tenant").is_err());
        assert!(validate_id("has space", "Tenant").is_err());
        assert!(validate_id(&"a".repeat(64), "Tenant").is_err());
    }

    #[test]
    fn test_tap_name() {
        assert_eq!(tap_name(3, 5), "tn3i5");
        assert_eq!(tap_name(4095, 254), "tn4095i254");
    }

    #[test]
    fn test_tap_name_fits_linux_limit() {
        // Worst case: tn4095i254 = 10 chars, under 15
        let name = tap_name(4095, 254);
        assert!(name.len() <= 15, "TAP name too long: {}", name);
    }

    #[test]
    fn test_mac_address_format() {
        let mac = mac_address(3, 5);
        assert!(mac.starts_with("02:fc:"));
        assert_eq!(mac.len(), 17);
    }

    #[test]
    fn test_generate_instance_id_format() {
        let id = generate_instance_id();
        assert!(id.starts_with("i-"));
        assert_eq!(id.len(), 10); // "i-" + 8 hex chars
    }

    #[test]
    fn test_parse_pool_path() {
        let (t, p) = parse_pool_path("acme/workers").unwrap();
        assert_eq!(t, "acme");
        assert_eq!(p, "workers");
    }

    #[test]
    fn test_parse_instance_path() {
        let (t, p, i) = parse_instance_path("acme/workers/i-a3f7b2c1").unwrap();
        assert_eq!(t, "acme");
        assert_eq!(p, "workers");
        assert_eq!(i, "i-a3f7b2c1");
    }
}
