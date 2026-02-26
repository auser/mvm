use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

pub use mvm_core::signing::SignedPayload;

/// Directory containing trusted coordinator public keys.
const TRUSTED_KEYS_DIR: &str = "/etc/mvm/trusted_keys";

/// Sign a payload with an Ed25519 signing key.
pub fn sign_payload(payload: &[u8], key: &SigningKey, signer_id: &str) -> SignedPayload {
    let signature = key.sign(payload);
    SignedPayload {
        payload: payload.to_vec(),
        signature: signature.to_bytes().to_vec(),
        signer_id: signer_id.to_string(),
    }
}

/// Verify a signed payload against a set of trusted public keys.
///
/// Returns Ok(()) if any trusted key validates the signature.
/// Returns Err if no trusted key validates or the signature is malformed.
pub fn verify_signed_payload(signed: &SignedPayload, trusted_keys: &[VerifyingKey]) -> Result<()> {
    if signed.signature.len() != 64 {
        anyhow::bail!(
            "Invalid signature length: {} (expected 64)",
            signed.signature.len()
        );
    }

    let sig_bytes: [u8; 64] = signed
        .signature
        .as_slice()
        .try_into()
        .with_context(|| "Signature must be exactly 64 bytes")?;

    let signature = Signature::from_bytes(&sig_bytes);

    for key in trusted_keys {
        if key.verify(&signed.payload, &signature).is_ok() {
            return Ok(());
        }
    }

    anyhow::bail!(
        "Signature verification failed: no trusted key matched (signer: {})",
        signed.signer_id
    )
}

/// Load trusted coordinator public keys from the trusted keys directory.
///
/// Each file in `/etc/mvm/trusted_keys/*.pub` contains a base64-encoded
/// Ed25519 public key (32 bytes decoded).
pub fn load_trusted_keys() -> Result<Vec<VerifyingKey>> {
    let output = crate::shell::run_in_vm_stdout(&format!(
        "ls {}/*.pub 2>/dev/null || true",
        TRUSTED_KEYS_DIR
    ))?;

    let mut keys = Vec::new();
    for line in output.lines().filter(|l| !l.is_empty()) {
        let content = crate::shell::run_in_vm_stdout(&format!("cat {}", line))?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .with_context(|| format!("Invalid base64 in key file: {}", line))?;

        if bytes.len() != 32 {
            anyhow::bail!(
                "Key file {} has wrong size: {} bytes (expected 32)",
                line,
                bytes.len()
            );
        }

        let key_bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Key conversion failed"))?;

        let key = VerifyingKey::from_bytes(&key_bytes)
            .with_context(|| format!("Invalid Ed25519 public key in {}", line))?;

        keys.push(key);
    }

    Ok(keys)
}

/// Verify a signed payload and deserialize the inner JSON.
///
/// Loads trusted keys from the filesystem, verifies the signature,
/// then deserializes the payload bytes as `T`.
pub fn verify_and_extract<T: serde::de::DeserializeOwned>(signed: &SignedPayload) -> Result<T> {
    let trusted_keys = load_trusted_keys()?;
    if trusted_keys.is_empty() {
        anyhow::bail!("No trusted keys configured in {}", TRUSTED_KEYS_DIR);
    }
    verify_signed_payload(signed, &trusted_keys)?;
    serde_json::from_slice(&signed.payload).with_context(|| "Failed to deserialize signed payload")
}

/// Verify a signed payload and deserialize, using provided keys (for testing).
pub fn verify_and_extract_with_keys<T: serde::de::DeserializeOwned>(
    signed: &SignedPayload,
    trusted_keys: &[VerifyingKey],
) -> Result<T> {
    verify_signed_payload(signed, trusted_keys)?;
    serde_json::from_slice(&signed.payload).with_context(|| "Failed to deserialize signed payload")
}

/// Generate a new Ed25519 signing keypair.
///
/// Returns (signing_key, verifying_key_base64) for dev/testing use.
pub fn generate_keypair() -> (SigningKey, String) {
    let mut rng = OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();
    let pub_b64 = base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes());
    (signing_key, pub_b64)
}

/// Generate a per-session Ed25519 keypair for vsock authentication.
///
/// Returns the signing key and the base64-encoded public key. The caller
/// is responsible for writing the keys to the secrets drive before VM boot.
pub fn generate_session_keypair() -> (SigningKey, String) {
    generate_keypair()
}

/// Provision vsock session keys to the secrets drive path.
///
/// Writes the guest signing key (base64-encoded secret) and host public key
/// to the given directory, which should be mounted read-only inside the VM
/// at `/mnt/secrets/vsock/`.
pub fn provision_session_keys(
    secrets_dir: &str,
    guest_signing_key: &SigningKey,
    host_pubkey: &VerifyingKey,
) -> Result<()> {
    let guest_secret_b64 =
        base64::engine::general_purpose::STANDARD.encode(guest_signing_key.to_bytes());
    let host_pub_b64 = base64::engine::general_purpose::STANDARD.encode(host_pubkey.as_bytes());

    std::fs::create_dir_all(secrets_dir)
        .with_context(|| format!("Failed to create secrets dir: {}", secrets_dir))?;

    let key_path = format!("{}/session_key.pem", secrets_dir);
    std::fs::write(&key_path, &guest_secret_b64)
        .with_context(|| format!("Failed to write session key to {}", key_path))?;

    let pub_path = format!("{}/host_pubkey.pem", secrets_dir);
    std::fs::write(&pub_path, &host_pub_b64)
        .with_context(|| format!("Failed to write host pubkey to {}", pub_path))?;

    Ok(())
}

/// Load a session signing key from base64-encoded bytes.
pub fn load_session_key(b64_secret: &str) -> Result<SigningKey> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_secret.trim())
        .with_context(|| "Invalid base64 in session key")?;

    if bytes.len() != 32 {
        anyhow::bail!(
            "Session key has wrong size: {} bytes (expected 32)",
            bytes.len()
        );
    }

    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Key conversion failed"))?;

    Ok(SigningKey::from_bytes(&key_bytes))
}

/// Load a verifying key from base64-encoded bytes.
pub fn load_verifying_key(b64_pubkey: &str) -> Result<VerifyingKey> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_pubkey.trim())
        .with_context(|| "Invalid base64 in public key")?;

    if bytes.len() != 32 {
        anyhow::bail!(
            "Public key has wrong size: {} bytes (expected 32)",
            bytes.len()
        );
    }

    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Key conversion failed"))?;

    VerifyingKey::from_bytes(&key_bytes).with_context(|| "Invalid Ed25519 public key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_verify_roundtrip() {
        let (signing_key, _pub_b64) = generate_keypair();
        let verifying_key = signing_key.verifying_key();

        let payload = b"desired state JSON";
        let signed = sign_payload(payload, &signing_key, "test-coordinator");

        let result = verify_signed_payload(&signed, &[verifying_key]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_wrong_key_fails() {
        let (key_a, _) = generate_keypair();
        let (key_b, _) = generate_keypair();

        let payload = b"desired state JSON";
        let signed = sign_payload(payload, &key_a, "coordinator-a");

        // Verify with key_b should fail
        let result = verify_signed_payload(&signed, &[key_b.verifying_key()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no trusted key"));
    }

    #[test]
    fn test_verify_tampered_payload_fails() {
        let (signing_key, _) = generate_keypair();
        let verifying_key = signing_key.verifying_key();

        let payload = b"original payload";
        let mut signed = sign_payload(payload, &signing_key, "test");

        // Tamper with the payload
        signed.payload = b"tampered payload".to_vec();

        let result = verify_signed_payload(&signed, &[verifying_key]);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_with_multiple_trusted_keys() {
        let (key_a, _) = generate_keypair();
        let (key_b, _) = generate_keypair();

        let payload = b"payload";
        let signed = sign_payload(payload, &key_b, "coordinator-b");

        // Should succeed when key_b is in the trusted set
        let trusted = vec![key_a.verifying_key(), key_b.verifying_key()];
        assert!(verify_signed_payload(&signed, &trusted).is_ok());
    }

    #[test]
    fn test_invalid_signature_length() {
        let signed = SignedPayload {
            payload: b"data".to_vec(),
            signature: vec![0u8; 32], // Too short
            signer_id: "test".to_string(),
        };

        let (key, _) = generate_keypair();
        let result = verify_signed_payload(&signed, &[key.verifying_key()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("64"));
    }

    #[test]
    fn test_generate_keypair_produces_valid_key() {
        let (signing_key, pub_b64) = generate_keypair();

        // Public key should be base64-encoded 32 bytes
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&pub_b64)
            .unwrap();
        assert_eq!(decoded.len(), 32);

        // Should be able to sign and verify with the generated key
        let payload = b"test";
        let signed = sign_payload(payload, &signing_key, "gen-test");
        assert!(verify_signed_payload(&signed, &[signing_key.verifying_key()]).is_ok());
    }

    #[test]
    fn test_session_keypair_generation() {
        let (key, pub_b64) = generate_session_keypair();

        // Should produce a valid keypair
        let payload = b"session test data";
        let signed = sign_payload(payload, &key, "session");
        assert!(verify_signed_payload(&signed, &[key.verifying_key()]).is_ok());

        // Public key should be decodable
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&pub_b64)
            .unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn test_provision_and_load_session_keys() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_dir = dir.path().to_str().unwrap();

        let (guest_key, _) = generate_session_keypair();
        let (host_key, _) = generate_session_keypair();
        let host_pubkey = host_key.verifying_key();

        // Provision keys to disk
        provision_session_keys(secrets_dir, &guest_key, &host_pubkey).unwrap();

        // Load them back
        let key_content =
            std::fs::read_to_string(format!("{}/session_key.pem", secrets_dir)).unwrap();
        let loaded_key = load_session_key(&key_content).unwrap();
        assert_eq!(loaded_key.to_bytes(), guest_key.to_bytes());

        let pub_content =
            std::fs::read_to_string(format!("{}/host_pubkey.pem", secrets_dir)).unwrap();
        let loaded_pub = load_verifying_key(&pub_content).unwrap();
        assert_eq!(loaded_pub.as_bytes(), host_pubkey.as_bytes());
    }

    #[test]
    fn test_load_session_key_invalid_base64() {
        let result = load_session_key("not valid base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_session_key_wrong_length() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let result = load_session_key(&short);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("32"));
    }

    #[test]
    fn test_load_verifying_key_wrong_length() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let result = load_verifying_key(&short);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("32"));
    }

    #[test]
    fn test_signed_payload_serialization() {
        let (key, _) = generate_keypair();
        let signed = sign_payload(b"payload", &key, "test-signer");

        let json = serde_json::to_string(&signed).unwrap();
        let parsed: SignedPayload = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.payload, b"payload");
        assert_eq!(parsed.signature.len(), 64);
        assert_eq!(parsed.signer_id, "test-signer");
    }

    #[test]
    fn test_empty_trusted_keys_always_fails() {
        let (key, _) = generate_keypair();
        let signed = sign_payload(b"data", &key, "test");

        let result = verify_signed_payload(&signed, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_and_extract_with_keys() {
        let (key, _) = generate_keypair();
        let data = serde_json::json!({"hello": "world"});
        let payload = serde_json::to_vec(&data).unwrap();
        let signed = sign_payload(&payload, &key, "test");

        let extracted: serde_json::Value =
            verify_and_extract_with_keys(&signed, &[key.verifying_key()]).unwrap();
        assert_eq!(extracted["hello"], "world");
    }

    #[test]
    fn test_verify_and_extract_bad_json() {
        let (key, _) = generate_keypair();
        let signed = sign_payload(b"not valid json", &key, "test");

        let result: Result<serde_json::Value> =
            verify_and_extract_with_keys(&signed, &[key.verifying_key()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("deserialize"));
    }

    #[test]
    fn test_verify_and_extract_wrong_key() {
        let (key_a, _) = generate_keypair();
        let (key_b, _) = generate_keypair();
        let payload = serde_json::to_vec(&serde_json::json!({"x": 1})).unwrap();
        let signed = sign_payload(&payload, &key_a, "test");

        let result: Result<serde_json::Value> =
            verify_and_extract_with_keys(&signed, &[key_b.verifying_key()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no trusted key"));
    }
}
