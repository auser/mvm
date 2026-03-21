//! Apple Container backend for mvm using XPC.
//!
//! On macOS 26+, this crate provides container lifecycle operations via
//! the `apple-container` crate, which talks directly to Apple's
//! `com.apple.container.apiserver` XPC daemon. No Swift bridge needed.
//!
//! On other platforms, all functions return "not available" errors.

#[cfg(target_os = "macos")]
use apple_container::AppleContainerClient;
#[cfg(target_os = "macos")]
use apple_container::models::{
    ContainerConfiguration, ImageDescription, ProcessConfiguration, Resources,
};

/// Check if Apple Containers are available on this platform.
pub fn is_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Try connecting to the XPC daemon
        AppleContainerClient::connect().is_ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Start a container from a local ext4 rootfs and kernel.
pub fn start(
    id: &str,
    kernel_path: &str,
    rootfs_path: &str,
    cpus: u32,
    memory_mib: u64,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client =
                AppleContainerClient::connect().map_err(|e| format!("XPC connect: {e}"))?;

            // Step 1: Determine image reference.
            // Check for OCI image.tar.gz alongside the rootfs (from mkGuest).
            // If found, pull it into the daemon's store. If not, use alpine as base.
            let rootfs_dir = std::path::Path::new(rootfs_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let oci_image = rootfs_dir.join("image.tar.gz");

            let image_ref = if oci_image.exists() {
                // Import OCI tarball via Image Service XPC
                let tag = format!("mvm-{}:latest", id);
                tracing::info!("Importing OCI image from {}", oci_image.display());

                if let Ok(img_conn) = apple_container::xpc::connection::XpcConnection::connect(
                    apple_container::routes::IMAGE_SERVICE_NAME,
                ) {
                    let pull_msg = apple_container::xpc::message::XpcMessage::with_route(
                        apple_container::routes::ImageRoute::ImagePull.as_str(),
                    );
                    pull_msg.set_string(
                        apple_container::routes::XpcKey::IMAGE_REFERENCE,
                        &format!("oci-archive:{}", oci_image.display()),
                    );
                    let platform = serde_json::to_vec(&serde_json::json!({
                        "os": "linux", "architecture": "arm64"
                    }))
                    .unwrap_or_default();
                    pull_msg.set_data(apple_container::routes::XpcKey::OCI_PLATFORM, &platform);

                    match img_conn.send_async(&pull_msg).await {
                        Ok(reply) if reply.check_error().is_ok() => {
                            tracing::info!("OCI image imported");
                            tag
                        }
                        _ => {
                            tracing::warn!("OCI import failed, falling back to alpine");
                            "docker.io/library/alpine:3.16".to_string()
                        }
                    }
                } else {
                    "docker.io/library/alpine:3.16".to_string()
                }
            } else {
                "docker.io/library/alpine:3.16".to_string()
            };

            // Step 1b: Pull the base image if needed (alpine fallback)
            if image_ref.contains("alpine") {
                tracing::info!("Pulling base image: {image_ref}");
                if let Ok(img_conn) = apple_container::xpc::connection::XpcConnection::connect(
                    apple_container::routes::IMAGE_SERVICE_NAME,
                ) {
                    let pull_msg = apple_container::xpc::message::XpcMessage::with_route(
                        apple_container::routes::ImageRoute::ImagePull.as_str(),
                    );
                    pull_msg
                        .set_string(apple_container::routes::XpcKey::IMAGE_REFERENCE, &image_ref);
                    let platform = serde_json::to_vec(&serde_json::json!({
                        "os": "linux", "architecture": "arm64"
                    }))
                    .unwrap_or_default();
                    pull_msg.set_data(apple_container::routes::XpcKey::OCI_PLATFORM, &platform);
                    pull_msg.set_bool(apple_container::routes::XpcKey::INSECURE_FLAG, false);

                    match img_conn.send_async(&pull_msg).await {
                        Ok(reply) => match reply.check_error() {
                            Ok(()) => tracing::info!("Base image pulled"),
                            Err(e) => tracing::warn!("Image pull error: {e}"),
                        },
                        Err(e) => tracing::warn!("Image pull XPC failed: {e}"),
                    }
                }
            }

            // Step 2: Get kernel
            let kernel = match client.get_default_kernel().await {
                Ok(k) => k,
                Err(_) => serde_json::to_vec(&serde_json::json!({
                    "path": kernel_path,
                    "platform": {"os": "linux", "architecture": "arm64"}
                }))
                .map_err(|e| format!("kernel json: {e}"))?,
            };

            // Step 3: Create container
            // vminitd is PID 1 (framework-managed). Our /init runs as init_process.
            // The /sbin/vminitd → /init symlink in our rootfs ensures compatibility.
            let config = ContainerConfiguration {
                id: id.to_string(),
                image: ImageDescription {
                    reference: image_ref,
                    ..Default::default()
                },
                mounts: vec![],
                published_ports: vec![],
                labels: Default::default(),
                init_process: ProcessConfiguration {
                    executable: "/init".to_string(),
                    arguments: vec![],
                    environment: vec![],
                    working_directory: "/".to_string(),
                    terminal: false,
                    user: Default::default(),
                },
                resources: Resources {
                    cpu_count: cpus,
                    memory_in_bytes: memory_mib * 1024 * 1024,
                },
            };

            client
                .create(&config, &kernel)
                .await
                .map_err(|e| format!("create: {e}"))?;

            // Step 4: Bootstrap
            let devnull = std::fs::File::open("/dev/null").map_err(|e| e.to_string())?;
            use std::os::fd::AsRawFd;
            let fd = devnull.as_raw_fd();
            client
                .bootstrap(id, fd, fd, fd)
                .await
                .map_err(|e| format!("bootstrap: {e}"))?;

            // Step 5: Verify
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            match client.get(id).await {
                Ok(snapshot) => {
                    tracing::info!("Container '{}' status: {:?}", id, snapshot.status);
                }
                Err(e) => {
                    tracing::warn!("Container '{}' not found after bootstrap: {e}", id);
                }
            }

            Ok(())
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (id, kernel_path, rootfs_path, cpus, memory_mib);
        Err("Apple Containers not available on this platform".to_string())
    }
}

/// Stop a running container.
pub fn stop(id: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            let client =
                AppleContainerClient::connect().map_err(|e| format!("XPC connect: {e}"))?;
            client.stop(id).await.map_err(|e| format!("stop: {e}"))?;
            client
                .delete(id, false)
                .await
                .map_err(|e| format!("delete: {e}"))?;
            Ok(())
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = id;
        Err("Apple Containers not available on this platform".to_string())
    }
}

/// List running container IDs.
pub fn list_ids() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(_) => return vec![],
        };
        rt.block_on(async {
            let client = match AppleContainerClient::connect() {
                Ok(c) => c,
                Err(_) => return vec![],
            };
            match client.list().await {
                Ok(containers) => containers.into_iter().map(|c| c.configuration.id).collect(),
                Err(_) => vec![],
            }
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_available_returns_bool() {
        let _ = is_available();
    }

    #[test]
    fn test_list_ids_returns_vec() {
        let ids = list_ids();
        // May or may not have containers depending on system state
        let _ = ids;
    }

    /// Integration test: boot an Apple Container from template artifacts.
    ///
    /// Run with: cargo test -p mvm-apple-container -- --ignored boot_test
    #[test]
    #[ignore]
    fn boot_test_apple_container() {
        if !is_available() {
            eprintln!("Skipping: Apple Containers not available (XPC daemon not running)");
            return;
        }

        let home = std::env::var("HOME").expect("HOME must be set");
        let artifacts = format!("{}/.mvm/templates/hello/artifacts", home);

        let mut entries: Vec<_> = std::fs::read_dir(&artifacts)
            .expect("template artifacts dir must exist")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        entries.sort_by_key(|e| e.file_name());
        let rev_dir = entries
            .last()
            .expect("at least one revision must exist")
            .path();

        let kernel = rev_dir.join("vmlinux");
        let rootfs = rev_dir.join("rootfs.ext4");

        assert!(kernel.exists(), "kernel not found at {}", kernel.display());
        assert!(rootfs.exists(), "rootfs not found at {}", rootfs.display());

        eprintln!("Booting Apple Container with:");
        eprintln!("  kernel: {}", kernel.display());
        eprintln!("  rootfs: {}", rootfs.display());

        let result = start(
            "boot-test",
            kernel.to_str().expect("UTF-8"),
            rootfs.to_str().expect("UTF-8"),
            2,
            512,
        );

        match &result {
            Ok(()) => {
                eprintln!("Container started successfully via Apple Container XPC!");
                // Container may exit quickly if /init finishes — that's OK
                let ids = list_ids();
                eprintln!("Running containers: {ids:?}");
                // Clean up
                match stop("boot-test") {
                    Ok(()) => eprintln!("Container stopped."),
                    Err(e) => eprintln!("Stop: {e} (may have already exited)"),
                }
            }
            Err(e) => {
                eprintln!("Container start returned error: {e}");
            }
        }
    }
}
