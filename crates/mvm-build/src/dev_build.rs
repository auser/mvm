use anyhow::{Context, Result};

use mvm_core::build_env::BuildEnvironment;
use mvm_core::pool::Role;

use crate::nix_manifest::NixManifest;

/// Base directory for dev build artifacts.
const DEV_BUILDS_DIR: &str = "/var/lib/mvm/dev/builds";

/// Result of a dev build via `nix build` in the Lima VM.
#[derive(Debug, Clone)]
pub struct DevBuildResult {
    /// Directory containing artifacts: /var/lib/mvm/dev/builds/<hash>/
    pub build_dir: String,
    /// Path to the kernel image.
    pub vmlinux_path: String,
    /// Path to the root filesystem.
    pub rootfs_path: String,
    /// Nix store hash used as the revision identifier.
    pub revision_hash: String,
    /// Whether the build was a cache hit (artifacts already existed).
    pub cached: bool,
}

/// Build a microVM image from a Nix flake directly in the Lima VM.
///
/// Runs `nix build` with visible output, then copies the resulting
/// kernel and rootfs to a dev build directory keyed by Nix store hash.
/// Re-running the same build is a near-instant cache hit.
pub fn dev_build(
    env: &dyn BuildEnvironment,
    flake_ref: &str,
    profile: &str,
    role: &Role,
) -> Result<DevBuildResult> {
    let attr = resolve_dev_build_attribute(env, flake_ref, role, profile);

    // Step 1: Run nix build with visible output so the user sees progress
    env.log_info(&format!("Building: nix build {}", attr));
    env.shell_exec_visible(&format!("nix build {} --no-link 2>&1", attr,))
        .with_context(|| format!("nix build failed for {}", attr))?;

    // Step 2: Capture the output path (instant, uses Nix cache)
    let output = env
        .shell_exec_stdout(&format!("nix build {} --no-link --print-out-paths", attr,))
        .with_context(|| "Failed to get nix build output path")?;

    let nix_output_path = output
        .lines()
        .rev()
        .find(|l| l.starts_with("/nix/store/"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "nix build did not produce an output path. Output:\n{}",
                output
            )
        })?
        .trim()
        .to_string();

    env.log_info(&format!("Build output: {}", nix_output_path));

    // Step 3: Extract revision hash from /nix/store/<hash>-...
    let revision_hash = extract_revision_hash(&nix_output_path);
    let build_dir = dev_build_dir(&revision_hash);

    // Step 4: Check cache — skip copy if artifacts already exist
    if check_cache(env, &revision_hash)? {
        env.log_success(&format!("Cache hit: {}", build_dir));
        return Ok(DevBuildResult {
            vmlinux_path: format!("{}/vmlinux", build_dir),
            rootfs_path: format!("{}/rootfs.ext4", build_dir),
            build_dir,
            revision_hash,
            cached: true,
        });
    }

    // Step 5: Copy artifacts from Nix store to dev build directory
    copy_dev_artifacts(env, &nix_output_path, &build_dir)?;

    env.log_success(&format!("Artifacts stored at {}", build_dir));

    Ok(DevBuildResult {
        vmlinux_path: format!("{}/vmlinux", build_dir),
        rootfs_path: format!("{}/rootfs.ext4", build_dir),
        build_dir,
        revision_hash,
        cached: false,
    })
}

/// Resolve the Nix attribute for a dev build.
///
/// If `<flake>/mvm-profiles.toml` exists and contains the role+profile,
/// uses the role-aware attribute `<flake>#packages.<system>.tenant-<role>-<profile>`.
/// Otherwise falls back to `<flake>#packages.<system>.tenant-<profile>`.
fn resolve_dev_build_attribute(
    env: &dyn BuildEnvironment,
    flake_ref: &str,
    role: &Role,
    profile: &str,
) -> String {
    let system = nix_system();

    // Try to read mvm-profiles.toml from the flake directory
    let manifest_result = env.shell_exec_stdout(&format!(
        "cat '{}/mvm-profiles.toml' 2>/dev/null || echo __NOT_FOUND__",
        flake_ref.replace('\'', "'\\''"),
    ));

    if let Ok(content) = manifest_result
        && !content.contains("__NOT_FOUND__")
        && let Ok(manifest) = NixManifest::from_toml(&content)
        && manifest.resolve(role, profile).is_ok()
    {
        let attr = format!(
            "{}#packages.{}.tenant-{}-{}",
            flake_ref, system, role, profile
        );
        env.log_info(&format!(
            "Manifest found, using role-aware attribute: {}",
            attr
        ));
        return attr;
    }

    // Fallback: legacy attribute without role
    let attr = format!("{}#packages.{}.tenant-{}", flake_ref, system, profile);
    env.log_info(&format!(
        "No manifest found, using legacy attribute: {}",
        attr
    ));
    attr
}

/// Extract the Nix store hash from an output path like `/nix/store/<hash>-name`.
fn extract_revision_hash(nix_output_path: &str) -> String {
    nix_output_path
        .strip_prefix("/nix/store/")
        .and_then(|s| s.split('-').next())
        .unwrap_or("unknown")
        .to_string()
}

/// Return the dev build directory for a given revision hash.
fn dev_build_dir(revision_hash: &str) -> String {
    format!("{}/{}", DEV_BUILDS_DIR, revision_hash)
}

/// Check whether cached artifacts exist for a revision hash.
fn check_cache(env: &dyn BuildEnvironment, revision_hash: &str) -> Result<bool> {
    let build_dir = dev_build_dir(revision_hash);
    let result = env.shell_exec_stdout(&format!(
        "test -f {dir}/vmlinux && test -f {dir}/rootfs.ext4 && echo yes || echo no",
        dir = build_dir,
    ))?;
    Ok(result.trim() == "yes")
}

/// Copy kernel and rootfs from a Nix store output to the dev build directory.
fn copy_dev_artifacts(
    env: &dyn BuildEnvironment,
    nix_output_path: &str,
    build_dir: &str,
) -> Result<()> {
    env.shell_exec(&format!(
        r#"
        set -euo pipefail
        sudo mkdir -p {dir}
        sudo chown $(whoami) {dir}

        # Copy kernel (try 'kernel' then 'vmlinux')
        if [ -e {out}/kernel ]; then
            cp -L {out}/kernel {dir}/vmlinux
        elif [ -e {out}/vmlinux ]; then
            cp -L {out}/vmlinux {dir}/vmlinux
        else
            echo 'ERROR: kernel not found in build output' >&2
            ls -la {out}/ >&2
            exit 1
        fi

        # Copy rootfs (try 'rootfs' then 'rootfs.ext4')
        if [ -e {out}/rootfs ]; then
            cp -L {out}/rootfs {dir}/rootfs.ext4
        elif [ -e {out}/rootfs.ext4 ]; then
            cp -L {out}/rootfs.ext4 {dir}/rootfs.ext4
        else
            echo 'ERROR: rootfs not found in build output' >&2
            ls -la {out}/ >&2
            exit 1
        fi

        echo "Artifacts:"
        ls -lh {dir}/
        "#,
        out = nix_output_path,
        dir = build_dir,
    ))
    .with_context(|| format!("Failed to copy artifacts to {}", build_dir))
}

/// Return the Nix system identifier for the current architecture.
fn nix_system() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else {
        "x86_64-linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Mock BuildEnvironment for testing dev_build logic without a real VM.
    struct TestEnv {
        stdout_responses: Mutex<HashMap<String, String>>,
        exec_log: Mutex<Vec<String>>,
        logs: Mutex<Vec<String>>,
    }

    impl TestEnv {
        fn new() -> Self {
            Self {
                stdout_responses: Mutex::new(HashMap::new()),
                exec_log: Mutex::new(Vec::new()),
                logs: Mutex::new(Vec::new()),
            }
        }

        fn stub_stdout(&self, pattern: &str, response: &str) {
            self.stdout_responses
                .lock()
                .unwrap()
                .insert(pattern.to_string(), response.to_string());
        }
    }

    impl BuildEnvironment for TestEnv {
        fn shell_exec(&self, script: &str) -> Result<()> {
            self.exec_log.lock().unwrap().push(script.to_string());
            Ok(())
        }

        fn shell_exec_stdout(&self, script: &str) -> Result<String> {
            self.exec_log.lock().unwrap().push(script.to_string());
            let responses = self.stdout_responses.lock().unwrap();
            for (pattern, response) in responses.iter() {
                if script.contains(pattern) {
                    return Ok(response.clone());
                }
            }
            Ok(String::new())
        }

        fn shell_exec_visible(&self, script: &str) -> Result<()> {
            self.exec_log.lock().unwrap().push(script.to_string());
            Ok(())
        }

        fn load_pool_spec(
            &self,
            _tenant_id: &str,
            _pool_id: &str,
        ) -> Result<mvm_core::pool::PoolSpec> {
            unimplemented!("not needed for dev_build tests")
        }

        fn load_tenant_config(&self, _tenant_id: &str) -> Result<mvm_core::tenant::TenantConfig> {
            unimplemented!("not needed for dev_build tests")
        }

        fn ensure_bridge(&self, _net: &mvm_core::tenant::TenantNet) -> Result<()> {
            unimplemented!("not needed for dev_build tests")
        }

        fn setup_tap(
            &self,
            _net: &mvm_core::instance::InstanceNet,
            _bridge_name: &str,
        ) -> Result<()> {
            unimplemented!("not needed for dev_build tests")
        }

        fn teardown_tap(&self, _tap_dev: &str) -> Result<()> {
            unimplemented!("not needed for dev_build tests")
        }

        fn record_revision(
            &self,
            _tenant_id: &str,
            _pool_id: &str,
            _revision: &mvm_core::pool::BuildRevision,
        ) -> Result<()> {
            unimplemented!("not needed for dev_build tests")
        }

        fn log_info(&self, msg: &str) {
            self.logs.lock().unwrap().push(format!("INFO: {}", msg));
        }

        fn log_success(&self, msg: &str) {
            self.logs.lock().unwrap().push(format!("SUCCESS: {}", msg));
        }
    }

    #[test]
    fn test_extract_revision_hash_valid() {
        let hash = extract_revision_hash("/nix/store/abc123def456-tenant-worker-minimal");
        assert_eq!(hash, "abc123def456");
    }

    #[test]
    fn test_extract_revision_hash_no_prefix() {
        let hash = extract_revision_hash("/some/other/path");
        assert_eq!(hash, "unknown");
    }

    #[test]
    fn test_extract_revision_hash_empty() {
        let hash = extract_revision_hash("");
        assert_eq!(hash, "unknown");
    }

    #[test]
    fn test_dev_build_dir() {
        let dir = dev_build_dir("abc123");
        assert_eq!(dir, "/var/lib/mvm/dev/builds/abc123");
    }

    #[test]
    fn test_dev_build_dir_preserves_full_hash() {
        let dir = dev_build_dir("abc123def456ghi789");
        assert_eq!(dir, "/var/lib/mvm/dev/builds/abc123def456ghi789");
    }

    #[test]
    fn test_nix_system() {
        let system = nix_system();
        assert!(
            system == "aarch64-linux" || system == "x86_64-linux",
            "unexpected system: {}",
            system
        );
    }

    #[test]
    fn test_resolve_attribute_with_manifest() {
        let env = TestEnv::new();

        let manifest_toml = r#"
[meta]
version = 1

[profiles.minimal]
module = "guests/profiles/minimal.nix"

[roles.worker]
module = "roles/worker.nix"
"#;

        env.stub_stdout("mvm-profiles.toml", manifest_toml);

        let attr =
            resolve_dev_build_attribute(&env, "/home/user/my-project", &Role::Worker, "minimal");

        let system = nix_system();
        assert_eq!(
            attr,
            format!(
                "/home/user/my-project#packages.{}.tenant-worker-minimal",
                system
            )
        );
    }

    #[test]
    fn test_resolve_attribute_without_manifest() {
        let env = TestEnv::new();
        env.stub_stdout("mvm-profiles.toml", "__NOT_FOUND__");

        let attr =
            resolve_dev_build_attribute(&env, "/home/user/project", &Role::Worker, "minimal");

        let system = nix_system();
        assert_eq!(
            attr,
            format!("/home/user/project#packages.{}.tenant-minimal", system)
        );
    }

    #[test]
    fn test_resolve_attribute_manifest_missing_role() {
        let env = TestEnv::new();

        // Manifest exists but doesn't have Gateway role
        let manifest_toml = r#"
[meta]
version = 1

[profiles.minimal]
module = "guests/profiles/minimal.nix"

[roles.worker]
module = "roles/worker.nix"
"#;

        env.stub_stdout("mvm-profiles.toml", manifest_toml);

        let attr = resolve_dev_build_attribute(&env, "/tmp/flake", &Role::Gateway, "minimal");

        // Should fall back to legacy attribute since Gateway isn't in manifest
        let system = nix_system();
        assert_eq!(
            attr,
            format!("/tmp/flake#packages.{}.tenant-minimal", system)
        );
    }

    #[test]
    fn test_check_cache_hit() {
        let env = TestEnv::new();
        env.stub_stdout("test -f", "yes");

        let cached = check_cache(&env, "abc123").unwrap();
        assert!(cached);
    }

    #[test]
    fn test_check_cache_miss() {
        let env = TestEnv::new();
        env.stub_stdout("test -f", "no");

        let cached = check_cache(&env, "abc123").unwrap();
        assert!(!cached);
    }

    #[test]
    fn test_dev_build_cached() {
        let env = TestEnv::new();

        // nix build --no-link (visible) succeeds
        // nix build --print-out-paths returns the path
        env.stub_stdout(
            "--print-out-paths",
            "/nix/store/abc123-tenant-worker-minimal\n",
        );
        // Cache check returns yes
        env.stub_stdout("test -f", "yes");

        let result = dev_build(&env, "/home/user/project", "minimal", &Role::Worker).unwrap();

        assert!(result.cached);
        assert_eq!(result.revision_hash, "abc123");
        assert_eq!(result.build_dir, "/var/lib/mvm/dev/builds/abc123");
        assert_eq!(
            result.vmlinux_path,
            "/var/lib/mvm/dev/builds/abc123/vmlinux"
        );
        assert_eq!(
            result.rootfs_path,
            "/var/lib/mvm/dev/builds/abc123/rootfs.ext4"
        );
    }

    #[test]
    fn test_dev_build_fresh() {
        let env = TestEnv::new();

        env.stub_stdout("--print-out-paths", "/nix/store/xyz789-tenant-minimal\n");
        // Manifest not found
        env.stub_stdout("mvm-profiles.toml", "__NOT_FOUND__");
        // Cache miss
        env.stub_stdout("test -f", "no");

        let result = dev_build(&env, "/tmp/flake", "minimal", &Role::Worker).unwrap();

        assert!(!result.cached);
        assert_eq!(result.revision_hash, "xyz789");
        assert_eq!(result.build_dir, "/var/lib/mvm/dev/builds/xyz789");

        // Verify a copy script was executed
        let exec_log = env.exec_log.lock().unwrap();
        let has_copy = exec_log.iter().any(|s| s.contains("cp -L"));
        assert!(has_copy, "Expected copy script in exec log");
    }

    #[test]
    fn test_dev_build_result_paths_consistent() {
        let result = DevBuildResult {
            build_dir: "/var/lib/mvm/dev/builds/hash123".to_string(),
            vmlinux_path: "/var/lib/mvm/dev/builds/hash123/vmlinux".to_string(),
            rootfs_path: "/var/lib/mvm/dev/builds/hash123/rootfs.ext4".to_string(),
            revision_hash: "hash123".to_string(),
            cached: false,
        };

        assert!(result.vmlinux_path.starts_with(&result.build_dir));
        assert!(result.rootfs_path.starts_with(&result.build_dir));
    }
}
