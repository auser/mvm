use anyhow::{Context, Result};
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use super::protocol::{self, HOSTD_SOCKET_PATH, HostdRequest, HostdResponse};
use crate::vm::bridge;
use crate::vm::instance::lifecycle as inst;
use crate::vm::naming;

/// Start the hostd privileged executor daemon.
///
/// Listens on a Unix domain socket and dispatches privileged operations.
/// This process runs as root with minimal capabilities.
pub async fn serve(socket_path: Option<&str>) -> Result<()> {
    let path = socket_path.unwrap_or(HOSTD_SOCKET_PATH);

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(path);

    // Ensure parent directory exists
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create socket directory: {}", parent.display()))?;
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("Failed to bind Unix socket at {}", path))?;

    // Set socket permissions: group-readable/writable for mvm group
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .with_context(|| "Failed to set socket permissions")?;
    }

    info!(socket = %path, "Hostd listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream).await {
                        warn!(error = %e, "Connection error");
                    }
                });
            }
            Err(e) => {
                error!(error = %e, "Failed to accept connection");
            }
        }
    }
}

/// Handle a single connection: read one request, execute, send response.
async fn handle_connection(stream: tokio::net::UnixStream) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    let request = protocol::recv_request(&mut reader).await?;

    let response = tokio::task::spawn_blocking(move || execute(request))
        .await
        .with_context(|| "Executor task failed")?;

    protocol::send_response(&mut writer, &response).await?;

    Ok(())
}

/// Execute a single hostd request by dispatching to the appropriate privileged function.
fn execute(request: HostdRequest) -> HostdResponse {
    match request {
        HostdRequest::StartInstance {
            tenant_id,
            pool_id,
            instance_id,
        } => {
            if let Err(e) = validate_ids(&tenant_id, &pool_id, Some(&instance_id)) {
                return HostdResponse::Error {
                    message: e.to_string(),
                };
            }
            match inst::instance_start(&tenant_id, &pool_id, &instance_id) {
                Ok(()) => HostdResponse::Ok,
                Err(e) => HostdResponse::Error {
                    message: format!("start failed: {}", e),
                },
            }
        }
        HostdRequest::StopInstance {
            tenant_id,
            pool_id,
            instance_id,
        } => {
            if let Err(e) = validate_ids(&tenant_id, &pool_id, Some(&instance_id)) {
                return HostdResponse::Error {
                    message: e.to_string(),
                };
            }
            match inst::instance_stop(&tenant_id, &pool_id, &instance_id) {
                Ok(()) => HostdResponse::Ok,
                Err(e) => HostdResponse::Error {
                    message: format!("stop failed: {}", e),
                },
            }
        }
        HostdRequest::SleepInstance {
            tenant_id,
            pool_id,
            instance_id,
            force,
        } => {
            if let Err(e) = validate_ids(&tenant_id, &pool_id, Some(&instance_id)) {
                return HostdResponse::Error {
                    message: e.to_string(),
                };
            }
            match inst::instance_sleep(&tenant_id, &pool_id, &instance_id, force) {
                Ok(()) => HostdResponse::Ok,
                Err(e) => HostdResponse::Error {
                    message: format!("sleep failed: {}", e),
                },
            }
        }
        HostdRequest::WakeInstance {
            tenant_id,
            pool_id,
            instance_id,
        } => {
            if let Err(e) = validate_ids(&tenant_id, &pool_id, Some(&instance_id)) {
                return HostdResponse::Error {
                    message: e.to_string(),
                };
            }
            match inst::instance_wake(&tenant_id, &pool_id, &instance_id) {
                Ok(()) => HostdResponse::Ok,
                Err(e) => HostdResponse::Error {
                    message: format!("wake failed: {}", e),
                },
            }
        }
        HostdRequest::DestroyInstance {
            tenant_id,
            pool_id,
            instance_id,
            wipe_volumes,
        } => {
            if let Err(e) = validate_ids(&tenant_id, &pool_id, Some(&instance_id)) {
                return HostdResponse::Error {
                    message: e.to_string(),
                };
            }
            match inst::instance_destroy(&tenant_id, &pool_id, &instance_id, wipe_volumes) {
                Ok(()) => HostdResponse::Ok,
                Err(e) => HostdResponse::Error {
                    message: format!("destroy failed: {}", e),
                },
            }
        }
        HostdRequest::SetupNetwork { tenant_id, net } => {
            if let Err(e) = naming::validate_id(&tenant_id, "Tenant") {
                return HostdResponse::Error {
                    message: e.to_string(),
                };
            }
            match bridge::ensure_tenant_bridge(&net) {
                Ok(()) => HostdResponse::Ok,
                Err(e) => HostdResponse::Error {
                    message: format!("setup network failed: {}", e),
                },
            }
        }
        HostdRequest::TeardownNetwork { tenant_id, net } => {
            if let Err(e) = naming::validate_id(&tenant_id, "Tenant") {
                return HostdResponse::Error {
                    message: e.to_string(),
                };
            }
            match bridge::destroy_tenant_bridge(&net) {
                Ok(()) => HostdResponse::Ok,
                Err(e) => HostdResponse::Error {
                    message: format!("teardown network failed: {}", e),
                },
            }
        }
        HostdRequest::Ping => HostdResponse::Pong,
    }
}

/// Validate tenant/pool/instance IDs before executing privileged operations.
fn validate_ids(tenant_id: &str, pool_id: &str, instance_id: Option<&str>) -> Result<()> {
    naming::validate_id(tenant_id, "Tenant")?;
    naming::validate_id(pool_id, "Pool")?;
    if let Some(iid) = instance_id {
        naming::validate_id(iid, "Instance")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_ping() {
        let resp = execute(HostdRequest::Ping);
        assert!(matches!(resp, HostdResponse::Pong));
    }

    #[test]
    fn test_execute_invalid_tenant_id() {
        let resp = execute(HostdRequest::StartInstance {
            tenant_id: "INVALID!!".to_string(),
            pool_id: "workers".to_string(),
            instance_id: "i-abc".to_string(),
        });
        match resp {
            HostdResponse::Error { message } => {
                assert!(message.contains("Tenant"));
            }
            _ => panic!("Expected error for invalid tenant ID"),
        }
    }

    #[test]
    fn test_execute_invalid_pool_id() {
        let resp = execute(HostdRequest::StopInstance {
            tenant_id: "acme".to_string(),
            pool_id: "INVALID!!".to_string(),
            instance_id: "i-abc".to_string(),
        });
        match resp {
            HostdResponse::Error { message } => {
                assert!(message.contains("Pool"));
            }
            _ => panic!("Expected error for invalid pool ID"),
        }
    }

    #[test]
    fn test_validate_ids_valid() {
        assert!(validate_ids("acme", "workers", Some("i-abc123")).is_ok());
    }

    #[test]
    fn test_validate_ids_bad_tenant() {
        assert!(validate_ids("INVALID!!", "workers", Some("i-abc")).is_err());
    }
}
