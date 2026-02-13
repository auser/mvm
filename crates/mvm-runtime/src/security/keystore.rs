use anyhow::{Context, Result};
use zeroize::Zeroizing;

/// Trait for providing encryption keys for tenant data volumes.
pub trait KeyProvider: Send + Sync {
    /// Get the data encryption key for a tenant.
    /// Returns 32 bytes (256-bit key for AES-256-XTS which uses 512-bit key internally).
    /// Wrapped in Zeroizing to ensure key material is wiped from memory on drop.
    fn get_data_key(&self, tenant_id: &str) -> Result<Zeroizing<Vec<u8>>>;
}

/// Reads keys from environment variables: MVM_TENANT_KEY_<TENANT_ID> (hex-encoded).
/// Suitable for dev/staging environments.
pub struct EnvKeyProvider;

impl KeyProvider for EnvKeyProvider {
    fn get_data_key(&self, tenant_id: &str) -> Result<Zeroizing<Vec<u8>>> {
        let var_name = format!(
            "MVM_TENANT_KEY_{}",
            tenant_id.to_uppercase().replace('-', "_")
        );
        let hex = std::env::var(&var_name)
            .with_context(|| format!("Missing encryption key env var: {}", var_name))?;
        let key = hex_decode(&hex).with_context(|| format!("Invalid hex in {}", var_name))?;
        Ok(Zeroizing::new(key))
    }
}

/// Reads keys from files at /var/lib/mvm/keys/<tenant_id>.key (raw binary).
/// Suitable for node-local key provisioning.
pub struct FileKeyProvider;

impl KeyProvider for FileKeyProvider {
    fn get_data_key(&self, tenant_id: &str) -> Result<Zeroizing<Vec<u8>>> {
        let path = format!("/var/lib/mvm/keys/{}.key", tenant_id);
        let output =
            crate::shell::run_in_vm_stdout(&format!("xxd -p {} 2>/dev/null | tr -d '\\n'", path))
                .with_context(|| format!("Failed to read key file: {}", path))?;
        let key =
            hex_decode(output.trim()).with_context(|| format!("Invalid key data in {}", path))?;
        Ok(Zeroizing::new(key))
    }
}

/// Get the appropriate key provider based on environment.
/// Uses FileKeyProvider if key files directory exists, otherwise EnvKeyProvider.
pub fn default_provider() -> Box<dyn KeyProvider> {
    // Check if file-based keys are provisioned
    if let Ok(out) =
        crate::shell::run_in_vm_stdout("test -d /var/lib/mvm/keys && echo yes || echo no")
        && out.trim() == "yes"
    {
        return Box::new(FileKeyProvider);
    }
    Box::new(EnvKeyProvider)
}

/// Check if encryption is available for a tenant (key exists).
pub fn has_key(tenant_id: &str) -> bool {
    let provider = default_provider();
    provider.get_data_key(tenant_id).is_ok()
}

/// Decode hex string to bytes.
fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        anyhow::bail!("Hex string has odd length: {}", hex.len());
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks(2) {
        let s = std::str::from_utf8(chunk)?;
        let byte = u8::from_str_radix(s, 16).with_context(|| format!("Invalid hex byte: {}", s))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_decode_valid() {
        assert_eq!(
            hex_decode("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(hex_decode("00ff").unwrap(), vec![0x00, 0xff]);
    }

    #[test]
    fn test_hex_decode_empty() {
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn test_hex_decode_odd_length() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn test_hex_decode_invalid_chars() {
        assert!(hex_decode("zzzz").is_err());
    }

    #[test]
    fn test_env_key_provider_missing() {
        unsafe { std::env::remove_var("MVM_TENANT_KEY_ACME") };
        let provider = EnvKeyProvider;
        assert!(provider.get_data_key("acme").is_err());
    }

    #[test]
    fn test_env_key_provider_present() {
        unsafe {
            std::env::set_var(
                "MVM_TENANT_KEY_TESTX",
                "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            )
        };
        let provider = EnvKeyProvider;
        let key = provider.get_data_key("testx").unwrap();
        assert_eq!(key.len(), 32);
        unsafe { std::env::remove_var("MVM_TENANT_KEY_TESTX") };
    }
}
