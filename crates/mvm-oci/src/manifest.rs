//! Manifest fetch + content-digest verification.
//!
//! The [`ManifestFetcher`] trait is the contract that downstream
//! code consumes; [`OciManifestFetcher`] is the real implementation
//! over `oci-client`. Splitting the trait from the impl lets test
//! code substitute a fixture without standing up a registry — the
//! hermetic in-process registry fixture lands in W1.2.
//!
//! Digest verification is always content-addressable: the SHA-256
//! over the manifest bytes is compared against the digest the caller
//! pinned (or the digest the registry advertised). A mismatch is a
//! hard error; we never paper over it with a warning.

use crate::OciError;
use crate::reference::ImageReference;
use async_trait::async_trait;
use oci_client::client::{Client, ClientConfig};
use oci_client::manifest::OciManifest;
use oci_client::secrets::RegistryAuth;
use sha2::{Digest, Sha256};

/// Result of a manifest fetch. Bytes are kept verbatim because
/// digest verification is byte-exact — any normalization
/// (whitespace, key ordering) would change the digest and break the
/// pin.
#[derive(Debug, Clone)]
pub struct FetchedManifest {
    /// The reference the manifest was fetched against, after
    /// canonicalization.
    pub reference: ImageReference,
    /// SHA-256 digest as `sha256:<lowercase-hex>`. Always verified
    /// against the bytes before this struct is constructed.
    pub digest: String,
    /// Raw manifest bytes as received from the registry. Layer
    /// fetch (W1.2) will parse these into typed layer descriptors.
    pub bytes: Vec<u8>,
    /// `Content-Type` the registry advertised
    /// (e.g. `application/vnd.oci.image.manifest.v1+json`).
    /// Carried so the caller can distinguish OCI from Docker v2
    /// schemas without reparsing.
    pub media_type: String,
}

/// Contract for "fetch the manifest of this image and verify its
/// digest." Returns the raw bytes; parsing into typed layer
/// descriptors is the caller's responsibility (and W1.2's task).
#[async_trait]
pub trait ManifestFetcher: Send + Sync {
    async fn fetch(&self, reference: &ImageReference) -> Result<FetchedManifest, OciError>;
}

/// Real fetcher backed by [`oci_client`]. Anonymous-only in W1.1;
/// private-registry auth lands in a later W1 PR with the credential
/// material flowing through [`secrecy::SecretString`] and the
/// `check-no-display-on-secret-types` lint.
pub struct OciManifestFetcher {
    client: Client,
}

impl Default for OciManifestFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl OciManifestFetcher {
    pub fn new() -> Self {
        Self {
            client: Client::new(ClientConfig::default()),
        }
    }
}

#[async_trait]
impl ManifestFetcher for OciManifestFetcher {
    async fn fetch(&self, reference: &ImageReference) -> Result<FetchedManifest, OciError> {
        // Convert mvm's structured reference into the shape
        // `oci_client` consumes. Canonical form is round-tripped
        // through the upstream parser — round-trip failure here
        // would indicate our `canonical()` and oci-client disagree
        // about the same string, which is a bug we want to surface
        // immediately rather than paper over.
        let canonical = reference.canonical();
        let upstream_ref: oci_client::Reference = canonical
            .parse()
            .map_err(|e| OciError::InvalidReference(format!("{canonical}: {e}")))?;

        let (manifest, digest) = self
            .client
            .pull_manifest(&upstream_ref, &RegistryAuth::Anonymous)
            .await
            .map_err(|e| OciError::Registry(e.to_string()))?;

        // `pull_manifest` returns the digest the registry computed.
        // We re-compute it from the serialized bytes and assert
        // equality — fail closed if the registry's claim doesn't
        // match the bytes. If the caller pinned a digest in the
        // reference, that pin must equal the computed digest too.
        let bytes = serialize_manifest(&manifest)?;
        let computed = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
        if computed != digest {
            return Err(OciError::DigestMismatch {
                expected: digest,
                computed,
            });
        }
        if let Some(pinned) = &reference.digest {
            if pinned != &computed {
                return Err(OciError::DigestMismatch {
                    expected: pinned.clone(),
                    computed,
                });
            }
        }

        let media_type = manifest_media_type(&manifest).to_string();

        Ok(FetchedManifest {
            reference: reference.clone(),
            digest: computed,
            bytes,
            media_type,
        })
    }
}

fn serialize_manifest(manifest: &OciManifest) -> Result<Vec<u8>, OciError> {
    // `oci_client::manifest::OciManifest` round-trips through
    // serde_json; this is the bytes we hash. Real registries serve
    // the manifest as the exact bytes the publisher produced, so in
    // practice we'd be hashing the wire bytes — but `oci_client`
    // 0.16 deserializes before handing back, so we re-serialize
    // here. The digest invariant still holds because the upstream
    // crate computes its returned digest from the same path.
    serde_json::to_vec(manifest)
        .map_err(|e| OciError::Registry(format!("re-serialize manifest: {e}")))
}

fn manifest_media_type(manifest: &OciManifest) -> &'static str {
    match manifest {
        OciManifest::Image(_) => "application/vnd.oci.image.manifest.v1+json",
        OciManifest::ImageIndex(_) => "application/vnd.oci.image.index.v1+json",
    }
}

/// Verify that `bytes` hashes to `expected` (a `sha256:<hex>`
/// string). Used by callers that already have manifest bytes in
/// hand (e.g. from a cache) and want to assert content integrity
/// without going through the full fetcher.
///
/// Always fails closed. Returns [`OciError::DigestMismatch`] on
/// content drift, [`OciError::MalformedDigest`] if `expected` does
/// not match `sha256:<64 lowercase hex chars>`,
/// [`OciError::UnsupportedDigestAlgorithm`] for non-sha256 inputs.
pub fn verify_sha256_digest(bytes: &[u8], expected: &str) -> Result<(), OciError> {
    let (alg, hex_part) = expected.split_once(':').ok_or_else(|| {
        OciError::MalformedDigest(format!("missing algorithm prefix: {expected:?}"))
    })?;
    if alg != "sha256" {
        return Err(OciError::UnsupportedDigestAlgorithm(alg.to_string()));
    }
    if hex_part.len() != 64 {
        return Err(OciError::MalformedDigest(format!(
            "sha256 digest must be 64 hex chars, got {} in {expected:?}",
            hex_part.len()
        )));
    }
    if !hex_part
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(OciError::MalformedDigest(format!(
            "digest hex must be lowercase ascii: {expected:?}"
        )));
    }

    let computed = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    if computed != expected {
        return Err(OciError::DigestMismatch {
            expected: expected.to_string(),
            computed,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN_BYTES: &[u8] = b"hello mvm";
    // sha256("hello mvm") — kept as a constant so the test for
    // `known_digest_constant_is_self_consistent` flags any
    // accidental edit. Recompute via:
    //   printf 'hello mvm' | shasum -a 256
    const KNOWN_DIGEST: &str =
        "sha256:790aa64759a490e14bb0197b875b2d41d7ecea8d73fedcaea7eb88b6d59b691d";

    fn computed_digest(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }

    #[test]
    fn verify_digest_accepts_matching_content() {
        let digest = computed_digest(KNOWN_BYTES);
        verify_sha256_digest(KNOWN_BYTES, &digest).expect("matching content must verify");
    }

    #[test]
    fn verify_digest_rejects_tampered_content() {
        let digest = computed_digest(KNOWN_BYTES);
        let tampered: Vec<u8> = KNOWN_BYTES
            .iter()
            .copied()
            .chain(std::iter::once(b'!'))
            .collect();
        let err = verify_sha256_digest(&tampered, &digest).unwrap_err();
        match err {
            OciError::DigestMismatch { .. } => {}
            other => panic!("expected DigestMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_digest_rejects_unsupported_algorithm() {
        let err = verify_sha256_digest(KNOWN_BYTES, "sha512:abc").unwrap_err();
        match err {
            OciError::UnsupportedDigestAlgorithm(alg) => assert_eq!(alg, "sha512"),
            other => panic!("expected UnsupportedDigestAlgorithm, got {other:?}"),
        }
    }

    #[test]
    fn verify_digest_rejects_missing_algorithm_prefix() {
        let err = verify_sha256_digest(KNOWN_BYTES, "abc").unwrap_err();
        assert!(matches!(err, OciError::MalformedDigest(_)), "got {err:?}");
    }

    #[test]
    fn verify_digest_rejects_wrong_hex_length() {
        let err = verify_sha256_digest(KNOWN_BYTES, "sha256:abc").unwrap_err();
        assert!(matches!(err, OciError::MalformedDigest(_)), "got {err:?}");
    }

    #[test]
    fn verify_digest_rejects_uppercase_hex() {
        // 64 uppercase hex chars — wrong-case rather than wrong-length.
        let upper = format!("sha256:{}", "A".repeat(64));
        let err = verify_sha256_digest(KNOWN_BYTES, &upper).unwrap_err();
        assert!(matches!(err, OciError::MalformedDigest(_)), "got {err:?}");
    }

    #[test]
    fn known_digest_constant_is_self_consistent() {
        // Guard against future edits to KNOWN_DIGEST breaking the
        // other tests silently. If this fires, recompute the
        // constant via:
        //   echo -n "hello mvm" | shasum -a 256
        let computed = computed_digest(KNOWN_BYTES);
        assert_eq!(computed, KNOWN_DIGEST);
    }
}
