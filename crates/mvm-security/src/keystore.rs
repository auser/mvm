//! Tenant key provisioning for the Phase 2 encryption-at-rest path.
//!
//! `KeyProvider` is the trait the supervisor consumes; `EnvKeyProvider`
//! is the dev/staging implementation that reads hex-encoded keys from
//! env vars. File-based and platform-keystore providers (macOS
//! Keychain / Linux Secret Service / Windows Cred Mgr) are Phase 2
//! proper — they need to sit on the same trait surface but their
//! storage shape depends on Sprint 49's `mvm-storage::VolumeBackend`
//! decisions, so they aren't ported here.
//!
//! Returned keys wrap `Zeroizing<Vec<u8>>` so material is wiped from
//! memory on drop.

use anyhow::{Context, Result};
use zeroize::Zeroizing;

/// Required key size for AES-256-GCM: 256 bits / 32 bytes.
pub const KEY_SIZE: usize = 32;

/// Resolves a tenant's data-encryption key.
pub trait KeyProvider: Send + Sync {
    /// Return the data encryption key for `tenant_id`. Implementations
    /// must wrap the returned bytes in `Zeroizing` so the key is
    /// wiped from memory when the caller drops it.
    fn get_data_key(&self, tenant_id: &str) -> Result<Zeroizing<Vec<u8>>>;
}

/// Reads keys from `MVM_TENANT_KEY_<TENANT_ID>` (hex-encoded, 64
/// chars for 32 bytes). Hyphens in tenant IDs become underscores in
/// the env var name and the ID is uppercased.
pub struct EnvKeyProvider;

impl KeyProvider for EnvKeyProvider {
    fn get_data_key(&self, tenant_id: &str) -> Result<Zeroizing<Vec<u8>>> {
        validate_shell_id(tenant_id)
            .with_context(|| format!("Invalid tenant_id for key lookup: {tenant_id:?}"))?;
        let var = format!(
            "MVM_TENANT_KEY_{}",
            tenant_id.to_uppercase().replace('-', "_")
        );
        let hex = std::env::var(&var)
            .with_context(|| format!("Missing encryption key env var: {var}"))?;
        let key = hex_decode(&hex).with_context(|| format!("Invalid hex in {var}"))?;
        if key.len() != KEY_SIZE {
            anyhow::bail!(
                "Tenant key in {var} must be {KEY_SIZE} bytes ({} hex chars), got {} bytes",
                KEY_SIZE * 2,
                key.len()
            );
        }
        Ok(Zeroizing::new(key))
    }
}

/// Reject any identifier that can't safely be interpolated into a
/// shell command, filesystem path, or env-var name. Accepts only
/// `[A-Za-z0-9_-]` and a non-empty string.
pub fn validate_shell_id(s: &str) -> Result<()> {
    if s.is_empty() {
        anyhow::bail!("identifier must not be empty");
    }
    if let Some(bad) = s
        .chars()
        .find(|c| !c.is_alphanumeric() && *c != '-' && *c != '_')
    {
        anyhow::bail!(
            "identifier contains unsafe character {bad:?} — only alphanumeric, '-', '_' allowed"
        );
    }
    Ok(())
}

/// Hex-decode an even-length ASCII string into bytes.
fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        anyhow::bail!("Hex string has odd length: {}", hex.len());
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks(2) {
        let s = std::str::from_utf8(chunk)?;
        let byte = u8::from_str_radix(s, 16).with_context(|| format!("Invalid hex byte: {s}"))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_KEY_HEX: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    #[test]
    fn hex_decode_valid() {
        assert_eq!(
            hex_decode("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(hex_decode("00ff").unwrap(), vec![0x00, 0xff]);
    }

    #[test]
    fn hex_decode_empty_is_empty() {
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_decode_odd_length_rejected() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn hex_decode_invalid_chars_rejected() {
        assert!(hex_decode("zzzz").is_err());
    }

    #[test]
    fn env_provider_missing_var() {
        unsafe { std::env::remove_var("MVM_TENANT_KEY_ACME") };
        assert!(EnvKeyProvider.get_data_key("acme").is_err());
    }

    #[test]
    fn env_provider_returns_zeroizing_key() {
        unsafe { std::env::set_var("MVM_TENANT_KEY_TESTX", VALID_KEY_HEX) };
        let key = EnvKeyProvider.get_data_key("testx").unwrap();
        assert_eq!(key.len(), KEY_SIZE);
        unsafe { std::env::remove_var("MVM_TENANT_KEY_TESTX") };
    }

    #[test]
    fn env_provider_rejects_wrong_length_key() {
        unsafe { std::env::set_var("MVM_TENANT_KEY_BADLEN", "deadbeef") };
        let err = EnvKeyProvider.get_data_key("badlen").unwrap_err();
        assert!(err.to_string().contains("must be 32 bytes"), "got: {err}");
        unsafe { std::env::remove_var("MVM_TENANT_KEY_BADLEN") };
    }

    #[test]
    fn env_provider_rejects_invalid_tenant_id() {
        // Path traversal / shell-injection candidates must be rejected
        // *before* any env lookup happens.
        assert!(EnvKeyProvider.get_data_key("../../etc").is_err());
        assert!(EnvKeyProvider.get_data_key("foo;rm").is_err());
    }

    #[test]
    fn env_provider_uppercases_and_swaps_hyphens() {
        unsafe { std::env::set_var("MVM_TENANT_KEY_FOO_BAR", VALID_KEY_HEX) };
        // tenant_id "foo-bar" must resolve to MVM_TENANT_KEY_FOO_BAR
        let key = EnvKeyProvider.get_data_key("foo-bar").unwrap();
        assert_eq!(key.len(), KEY_SIZE);
        unsafe { std::env::remove_var("MVM_TENANT_KEY_FOO_BAR") };
    }

    #[test]
    fn validate_shell_id_accepts_alphanumeric_dash_underscore() {
        assert!(validate_shell_id("acme").is_ok());
        assert!(validate_shell_id("tenant-1").is_ok());
        assert!(validate_shell_id("my_tenant_99").is_ok());
        assert!(validate_shell_id("ABC123").is_ok());
    }

    #[test]
    fn validate_shell_id_rejects_empty() {
        assert!(validate_shell_id("").is_err());
    }

    #[test]
    fn validate_shell_id_rejects_shell_metachars() {
        assert!(validate_shell_id("foo;rm -rf /").is_err());
        assert!(validate_shell_id("foo|bar").is_err());
        assert!(validate_shell_id("foo`bar`").is_err());
        assert!(validate_shell_id("foo$bar").is_err());
    }

    #[test]
    fn validate_shell_id_rejects_whitespace() {
        assert!(validate_shell_id("foo bar").is_err());
        assert!(validate_shell_id("foo\tbar").is_err());
    }

    #[test]
    fn validate_shell_id_rejects_dot_and_slash() {
        assert!(validate_shell_id("foo.bar").is_err());
        assert!(validate_shell_id("../../etc/passwd").is_err());
    }
}
