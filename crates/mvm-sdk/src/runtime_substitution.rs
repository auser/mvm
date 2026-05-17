//! Runtime-side secret substitution helpers for credential providers.
//!
//! ADR-049's default path resolves placeholders over vsock before a
//! guest SDK signs an outbound request. The transport is provided by
//! the W3 supervisor service; this module defines the SDK contract and
//! the AWS credential shape so SigV4 signers receive real credentials
//! before computing request signatures.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

type Handler = Box<dyn Fn(&str) -> Result<String, SubstitutionError> + Send + Sync + 'static>;

static HANDLERS: OnceLock<Mutex<HashMap<String, Handler>>> = OnceLock::new();

fn handlers() -> &'static Mutex<HashMap<String, Handler>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Errors returned by runtime substitution handler registration and lookup.
#[derive(Debug, thiserror::Error)]
pub enum SubstitutionError {
    #[error("substitution handler name must be non-empty")]
    EmptyHandlerName,
    #[error("no substitution handler registered for `{0}`")]
    MissingHandler(String),
    #[error("substitution handler `{handler}` denied placeholder `{placeholder}`: {reason}")]
    HandlerDenied {
        handler: String,
        placeholder: String,
        reason: String,
    },
    #[error("substitution handler registry lock is poisoned")]
    RegistryPoisoned,
}

/// AWS credentials after placeholder substitution.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Register a credential substitution handler.
///
/// `name` is the provider namespace, for example `aws`. The handler
/// receives one placeholder string and returns the materialized
/// credential value. Cloud SDK adapters must call this from credential
/// loading, before request signing.
pub fn register_substitution_handler<F>(
    name: impl Into<String>,
    handler: F,
) -> Result<(), SubstitutionError>
where
    F: Fn(&str) -> Result<String, SubstitutionError> + Send + Sync + 'static,
{
    let name = name.into();
    if name.is_empty() {
        return Err(SubstitutionError::EmptyHandlerName);
    }
    let mut guard = handlers()
        .lock()
        .map_err(|_| SubstitutionError::RegistryPoisoned)?;
    guard.insert(name, Box::new(handler));
    Ok(())
}

/// Clear all substitution handlers. Public for test isolation.
pub fn clear_substitution_handlers() -> Result<(), SubstitutionError> {
    handlers()
        .lock()
        .map_err(|_| SubstitutionError::RegistryPoisoned)?
        .clear();
    Ok(())
}

/// Resolve one placeholder through a registered handler.
pub fn substitute(name: &str, placeholder: &str) -> Result<String, SubstitutionError> {
    let guard = handlers()
        .lock()
        .map_err(|_| SubstitutionError::RegistryPoisoned)?;
    let handler = guard
        .get(name)
        .ok_or_else(|| SubstitutionError::MissingHandler(name.to_string()))?;
    handler(placeholder)
}

/// Resolve AWS credentials before SigV4 signing.
pub fn aws_credentials_from_placeholders(
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
) -> Result<AwsCredentials, SubstitutionError> {
    Ok(AwsCredentials {
        access_key_id: substitute("aws", access_key_id)?,
        secret_access_key: substitute("aws", secret_access_key)?,
        session_token: session_token
            .map(|token| substitute("aws", token))
            .transpose()?,
    })
}

pub fn is_placeholder(value: &str) -> bool {
    value.starts_with("mvm-secret://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    fn hmac_sha256(key: &[u8], msg: &str) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
        mac.update(msg.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn sigv4_signature(
        secret: &str,
        date: &str,
        region: &str,
        service: &str,
        string_to_sign: &str,
    ) -> String {
        let date_key = hmac_sha256(format!("AWS4{secret}").as_bytes(), date);
        let region_key = hmac_sha256(&date_key, region);
        let service_key = hmac_sha256(&region_key, service);
        let signing_key = hmac_sha256(&service_key, "aws4_request");
        hex(&hmac_sha256(&signing_key, string_to_sign))
    }

    #[test]
    fn aws_credentials_resolve_before_sigv4_signing() {
        clear_substitution_handlers().expect("clear handlers");
        register_substitution_handler("aws", |placeholder| match placeholder {
            "mvm-secret://aws/access-key" => Ok("AKIAIOSFODNN7EXAMPLE".to_string()),
            "mvm-secret://aws/secret-key" => {
                Ok("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string())
            }
            other => Err(SubstitutionError::HandlerDenied {
                handler: "aws".to_string(),
                placeholder: other.to_string(),
                reason: "unknown placeholder".to_string(),
            }),
        })
        .expect("register aws handler");

        let creds = aws_credentials_from_placeholders(
            "mvm-secret://aws/access-key",
            "mvm-secret://aws/secret-key",
            None,
        )
        .expect("resolve aws credentials");
        let canonical = "GET\n/\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD";
        let digest = hex(&Sha256::digest(canonical.as_bytes()));
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n{digest}"
        );

        let signature = sigv4_signature(
            &creds.secret_access_key,
            "20130524",
            "us-east-1",
            "s3",
            &string_to_sign,
        );

        assert_eq!(creds.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(
            signature,
            sigv4_signature(
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                "20130524",
                "us-east-1",
                "s3",
                &string_to_sign,
            )
        );
        assert_ne!(
            signature,
            sigv4_signature(
                "mvm-secret://aws/secret-key",
                "20130524",
                "us-east-1",
                "s3",
                &string_to_sign,
            )
        );
    }

    #[test]
    fn missing_handler_fails_closed() {
        clear_substitution_handlers().expect("clear handlers");
        let result = aws_credentials_from_placeholders(
            "mvm-secret://aws/access-key",
            "mvm-secret://aws/secret-key",
            None,
        );
        match result {
            Err(SubstitutionError::MissingHandler(name)) => assert_eq!(name, "aws"),
            Err(err) => panic!("expected missing aws handler, got {err}"),
            Ok(_) => panic!("missing aws handler must fail"),
        }
    }
}
