use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use anyhow::{Context, Result};

/// Nonce size for AES-256-GCM (96 bits / 12 bytes).
const NONCE_SIZE: usize = 12;

/// Encrypt data using AES-256-GCM.
///
/// Returns `[12-byte nonce][ciphertext+tag]`.
/// Key must be exactly 32 bytes (256 bits).
pub fn encrypt(plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 32 {
        anyhow::bail!("AES-256-GCM key must be 32 bytes, got {}", key.len());
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("Failed to create cipher: {}", e))?;

    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    // Format: [nonce][ciphertext+tag]
    let mut output = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt data encrypted with AES-256-GCM.
///
/// Input format: `[12-byte nonce][ciphertext+tag]`.
/// Key must be exactly 32 bytes (256 bits).
pub fn decrypt(encrypted: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 32 {
        anyhow::bail!("AES-256-GCM key must be 32 bytes, got {}", key.len());
    }

    if encrypted.len() < NONCE_SIZE + 16 {
        anyhow::bail!(
            "Encrypted data too short: {} bytes (minimum {})",
            encrypted.len(),
            NONCE_SIZE + 16
        );
    }

    let (nonce_bytes, ciphertext) = encrypted.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("Failed to create cipher: {}", e))?;

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("Decryption failed: authentication tag mismatch"))
}

/// Encrypt a snapshot file in-place.
///
/// Reads plaintext from `path`, encrypts it, writes to `path.enc`,
/// then removes the plaintext file.
pub fn encrypt_snapshot_file(path: &str, key: &[u8]) -> Result<String> {
    let enc_path = format!("{}.enc", path);

    let plaintext = crate::infra::shell::run_in_vm_stdout(&format!("base64 {} 2>/dev/null", path))
        .with_context(|| format!("Failed to read snapshot file: {}", path))?;

    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, plaintext.trim())
        .with_context(|| "Failed to decode snapshot file content")?;

    let encrypted = encrypt(&raw, key)?;

    let enc_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &encrypted);

    crate::infra::shell::run_in_vm(&format!(
        "echo '{}' | base64 -d > {} && chmod 0600 {} && rm -f {}",
        enc_b64, enc_path, enc_path, path
    ))
    .with_context(|| format!("Failed to write encrypted snapshot: {}", enc_path))?;

    Ok(enc_path)
}

/// Decrypt a snapshot file.
///
/// Reads encrypted data from `enc_path`, decrypts it, writes plaintext
/// to `out_path`. The encrypted file is NOT removed (caller decides).
pub fn decrypt_snapshot_file(enc_path: &str, out_path: &str, key: &[u8]) -> Result<()> {
    let enc_b64 =
        crate::infra::shell::run_in_vm_stdout(&format!("base64 {} 2>/dev/null", enc_path))
            .with_context(|| format!("Failed to read encrypted snapshot: {}", enc_path))?;

    let encrypted =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, enc_b64.trim())
            .with_context(|| "Failed to decode encrypted snapshot content")?;

    let plaintext = decrypt(&encrypted, key)?;

    let plain_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &plaintext);

    crate::infra::shell::run_in_vm(&format!(
        "echo '{}' | base64 -d > {} && chmod 0600 {}",
        plain_b64, out_path, out_path
    ))
    .with_context(|| format!("Failed to write decrypted snapshot: {}", out_path))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [
            0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
            0x07, 0x08, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff, 0x00, 0x99,
        ]
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = test_key();
        let plaintext = b"Hello, snapshot encryption!";

        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_empty() {
        let key = test_key();
        let plaintext = b"";

        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_large() {
        let key = test_key();
        let plaintext = vec![0xABu8; 1024 * 1024]; // 1 MiB

        let encrypted = encrypt(&plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypted_not_plaintext() {
        let key = test_key();
        let plaintext = b"RECOGNIZABLE_PATTERN_12345";

        let encrypted = encrypt(plaintext, &key).unwrap();

        // Encrypted output should not contain the plaintext
        let enc_str = String::from_utf8_lossy(&encrypted);
        assert!(!enc_str.contains("RECOGNIZABLE_PATTERN_12345"));
    }

    #[test]
    fn test_nonce_uniqueness() {
        let key = test_key();
        let plaintext = b"same data";

        let enc1 = encrypt(plaintext, &key).unwrap();
        let enc2 = encrypt(plaintext, &key).unwrap();

        // Different nonces should produce different ciphertext
        assert_ne!(enc1, enc2);

        // But both should decrypt to the same plaintext
        assert_eq!(decrypt(&enc1, &key).unwrap(), plaintext);
        assert_eq!(decrypt(&enc2, &key).unwrap(), plaintext);
    }

    #[test]
    fn test_decrypt_wrong_key_fails() {
        let key = test_key();
        let mut wrong_key = test_key();
        wrong_key[0] ^= 0xFF; // Flip a byte

        let plaintext = b"secret data";
        let encrypted = encrypt(plaintext, &key).unwrap();

        let result = decrypt(&encrypted, &wrong_key);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("authentication tag")
        );
    }

    #[test]
    fn test_decrypt_tampered_ciphertext_fails() {
        let key = test_key();
        let plaintext = b"secret data";

        let mut encrypted = encrypt(plaintext, &key).unwrap();
        // Tamper with the ciphertext (after the nonce)
        if encrypted.len() > NONCE_SIZE + 1 {
            encrypted[NONCE_SIZE + 1] ^= 0xFF;
        }

        let result = decrypt(&encrypted, &key);
        assert!(result.is_err());
    }

    #[test]
    fn test_bad_key_length() {
        let short_key = [0u8; 16];
        assert!(encrypt(b"data", &short_key).is_err());
        assert!(decrypt(&[0u8; 40], &short_key).is_err());
    }

    #[test]
    fn test_too_short_encrypted_data() {
        let key = test_key();
        let too_short = [0u8; 10]; // Less than nonce + tag
        assert!(decrypt(&too_short, &key).is_err());
    }

    #[test]
    fn test_encrypted_size() {
        let key = test_key();
        let plaintext = b"test";

        let encrypted = encrypt(plaintext, &key).unwrap();
        // Output = 12 (nonce) + 4 (plaintext) + 16 (tag) = 32
        assert_eq!(encrypted.len(), NONCE_SIZE + plaintext.len() + 16);
    }
}
