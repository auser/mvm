//! virtio-fs share mount handler — D of the e2b parity plan.
//!
//! Production-safe. Every host-supplied path runs through
//! `mvm_security::policy::MountPathPolicy` before the agent
//! touches `mount(2)` or `umount(2)`, so a compromised host can't
//! mount over `/etc`, `/usr`, or any other verity-protected
//! subtree (claim 3 of the security model).
//!
//! # Tag format
//!
//! virtio-fs `tag` is the 36-byte device tag the host advertises
//! when attaching the device. The agent enforces a conservative
//! charset (lowercase alphanumeric + hyphens, 1–32 chars) so a
//! bad tag fails policy rather than `mount(2)`'s opaque error
//! shape.
//!
//! # What this module does NOT do
//!
//! - Spawn `virtiofsd` on the host. That's mvm-runtime's job —
//!   the agent runs strictly inside the guest.
//! - Track which shares are attached. The host-side share registry
//!   (`crates/mvm-runtime/src/vm/share_registry.rs`) owns that;
//!   the agent is stateless across calls.

use std::path::Path;

use mvm_security::policy::{MountPathError, validate_mount_path};

use crate::vsock::{ShareErrorKind, ShareResult};

/// Maximum length of a virtio-fs tag (the kernel imposes 36
/// bytes; we cap shorter to keep the printable subset uniform).
const MAX_TAG_LEN: usize = 32;

/// Validate the virtio-fs tag string. Conservative charset: the
/// kernel accepts a wider set, but we restrict to lowercase
/// alphanumeric + hyphens so tags survive shell quoting and
/// audit-line interpolation cleanly.
fn validate_tag(tag: &str) -> Result<(), ShareResult> {
    if tag.is_empty() || tag.len() > MAX_TAG_LEN {
        return Err(ShareResult::Error {
            kind: ShareErrorKind::BadTag,
            message: format!("tag length {} outside [1, {MAX_TAG_LEN}]", tag.len()),
        });
    }
    if !tag
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(ShareResult::Error {
            kind: ShareErrorKind::BadTag,
            message: format!("tag {tag:?} must be lowercase alphanumeric + hyphens"),
        });
    }
    if tag.starts_with('-') {
        return Err(ShareResult::Error {
            kind: ShareErrorKind::BadTag,
            message: format!("tag {tag:?} must not start with a hyphen"),
        });
    }
    Ok(())
}

fn map_policy_error(err: MountPathError) -> ShareResult {
    let kind = match &err {
        MountPathError::Empty
        | MountPathError::NotAbsolute { .. }
        | MountPathError::EmbeddedNul { .. }
        | MountPathError::PathTraversal { .. } => ShareErrorKind::BadPath,
        MountPathError::Denied { .. } | MountPathError::OutsideAllowRoots { .. } => {
            ShareErrorKind::PolicyDenied
        }
    };
    ShareResult::Error {
        kind,
        message: err.to_string(),
    }
}

/// Perform validation + mount(2). Pure-logic for the validation
/// chunk; `mount(2)` itself goes through a small `MountFs` trait
/// so unit tests can stub the syscall.
pub fn handle_mount(tag: &str, guest_path: &str, read_only: bool) -> ShareResult {
    if let Err(e) = validate_tag(tag) {
        return e;
    }
    let canonical = match validate_mount_path(guest_path) {
        Ok(c) => c,
        Err(e) => return map_policy_error(e),
    };
    perform_mount(&OsMountFs, tag, &canonical, read_only)
}

/// Perform validation + umount(2).
pub fn handle_unmount(guest_path: &str, force: bool) -> ShareResult {
    let canonical = match validate_mount_path(guest_path) {
        Ok(c) => c,
        Err(e) => return map_policy_error(e),
    };
    perform_unmount(&OsMountFs, &canonical, force)
}

// ============================================================================
// MountFs trait — abstracts mount(2)/umount(2) for unit tests.
// ============================================================================

pub trait MountFs {
    fn ensure_dir(&self, path: &Path) -> std::io::Result<()>;
    fn mount(&self, tag: &str, path: &Path, read_only: bool) -> std::io::Result<()>;
    /// Returns `Ok(true)` on success, `Ok(false)` when the kernel
    /// reported `EBUSY` and `force == false`.
    fn umount(&self, path: &Path, force: bool) -> std::io::Result<bool>;
}

/// Production `MountFs` — uses `mount(2)`/`umount2(2)` directly on
/// Linux, returns ENOSYS-shaped errors elsewhere so non-Linux
/// hosts get a clean diagnostic rather than a panic.
pub struct OsMountFs;

#[cfg(target_os = "linux")]
impl MountFs for OsMountFs {
    fn ensure_dir(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn mount(&self, tag: &str, path: &Path, read_only: bool) -> std::io::Result<()> {
        use std::ffi::CString;
        let source = CString::new(tag).map_err(std::io::Error::other)?;
        let target = CString::new(path.as_os_str().to_string_lossy().as_bytes())
            .map_err(std::io::Error::other)?;
        let fstype = CString::new("virtiofs").map_err(std::io::Error::other)?;
        let mut flags: libc::c_ulong = 0;
        if read_only {
            flags |= libc::MS_RDONLY;
        }
        let rc = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                flags,
                std::ptr::null(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn umount(&self, path: &Path, force: bool) -> std::io::Result<bool> {
        use std::ffi::CString;
        let target = CString::new(path.as_os_str().to_string_lossy().as_bytes())
            .map_err(std::io::Error::other)?;
        let flags: libc::c_int = if force { libc::MNT_DETACH } else { 0 };
        let rc = unsafe { libc::umount2(target.as_ptr(), flags) };
        if rc == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EBUSY) && !force {
            return Ok(false);
        }
        Err(err)
    }
}

#[cfg(not(target_os = "linux"))]
impl MountFs for OsMountFs {
    fn ensure_dir(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }
    fn mount(&self, _tag: &str, _path: &Path, _read_only: bool) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "virtio-fs share mount is Linux-only",
        ))
    }
    fn umount(&self, _path: &Path, _force: bool) -> std::io::Result<bool> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "virtio-fs share unmount is Linux-only",
        ))
    }
}

fn perform_mount<M: MountFs>(
    fs: &M,
    tag: &str,
    canonical_path: &str,
    read_only: bool,
) -> ShareResult {
    let path = Path::new(canonical_path);
    if let Err(e) = fs.ensure_dir(path) {
        return ShareResult::Error {
            kind: ShareErrorKind::IoError,
            message: format!("ensure_dir({canonical_path}): {e}"),
        };
    }
    match fs.mount(tag, path, read_only) {
        Ok(()) => ShareResult::Mounted {
            canonical_path: canonical_path.to_string(),
        },
        Err(e) => ShareResult::Error {
            kind: ShareErrorKind::MountFailed,
            message: e.to_string(),
        },
    }
}

fn perform_unmount<M: MountFs>(fs: &M, canonical_path: &str, force: bool) -> ShareResult {
    let path = Path::new(canonical_path);
    match fs.umount(path, force) {
        Ok(true) => ShareResult::Unmounted,
        Ok(false) => ShareResult::Error {
            kind: ShareErrorKind::Busy,
            message: format!("{canonical_path}: target busy; pass force=true to lazy-detach"),
        },
        Err(e) => ShareResult::Error {
            kind: ShareErrorKind::IoError,
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `MountFs` stub recording every call so tests can assert
    /// the policy ran *before* any syscall would have fired.
    struct StubMountFs {
        mounts: Mutex<Vec<(String, std::path::PathBuf, bool)>>,
        umounts: Mutex<Vec<(std::path::PathBuf, bool)>>,
        umount_busy_unless_force: bool,
        mount_fails_with: Option<std::io::ErrorKind>,
    }

    impl StubMountFs {
        fn new() -> Self {
            Self {
                mounts: Mutex::new(Vec::new()),
                umounts: Mutex::new(Vec::new()),
                umount_busy_unless_force: false,
                mount_fails_with: None,
            }
        }
    }

    impl MountFs for StubMountFs {
        fn ensure_dir(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
        fn mount(&self, tag: &str, path: &Path, ro: bool) -> std::io::Result<()> {
            if let Some(kind) = self.mount_fails_with {
                return Err(std::io::Error::new(kind, "stub mount failure"));
            }
            self.mounts
                .lock()
                .unwrap()
                .push((tag.to_string(), path.to_path_buf(), ro));
            Ok(())
        }
        fn umount(&self, path: &Path, force: bool) -> std::io::Result<bool> {
            self.umounts
                .lock()
                .unwrap()
                .push((path.to_path_buf(), force));
            if self.umount_busy_unless_force && !force {
                return Ok(false);
            }
            Ok(true)
        }
    }

    #[test]
    fn validate_tag_accepts_typical_shapes() {
        for tag in ["data", "share-1", "mvm-share-0", "abc123"] {
            validate_tag(tag).unwrap_or_else(|e| panic!("expected accept for {tag:?}: {e:?}"));
        }
    }

    #[test]
    fn validate_tag_rejects_bad_shapes() {
        for tag in [
            "",
            "UPPER",
            "with space",
            "-leading",
            &"a".repeat(33),
            "name/slash",
        ] {
            assert!(
                matches!(
                    validate_tag(tag),
                    Err(ShareResult::Error {
                        kind: ShareErrorKind::BadTag,
                        ..
                    })
                ),
                "should reject {tag:?}",
            );
        }
    }

    #[test]
    fn handle_mount_rejects_relative_path_via_policy() {
        let r = handle_mount("data", "relative/path", false);
        match r {
            ShareResult::Error { kind, .. } => assert_eq!(kind, ShareErrorKind::BadPath),
            other => panic!("expected BadPath, got {other:?}"),
        }
    }

    #[test]
    fn handle_mount_rejects_etc_prefix() {
        let r = handle_mount("data", "/etc/mvm/foo", false);
        match r {
            ShareResult::Error { kind, .. } => assert_eq!(kind, ShareErrorKind::PolicyDenied),
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[test]
    fn handle_mount_rejects_outside_allow_roots() {
        let r = handle_mount("data", "/tmp/foo", false);
        match r {
            ShareResult::Error { kind, .. } => assert_eq!(kind, ShareErrorKind::PolicyDenied),
            other => panic!("expected PolicyDenied (outside allow-roots), got {other:?}"),
        }
    }

    #[test]
    fn perform_mount_calls_through_to_stub_on_clean_path() {
        let fs = StubMountFs::new();
        let r = perform_mount(&fs, "data-tag", "/data/foo", true);
        match r {
            ShareResult::Mounted { canonical_path } => {
                assert_eq!(canonical_path, "/data/foo");
            }
            other => panic!("expected Mounted, got {other:?}"),
        }
        let calls = fs.mounts.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "data-tag");
        assert_eq!(calls[0].1, std::path::PathBuf::from("/data/foo"));
        assert!(calls[0].2, "read_only flag should propagate");
    }

    #[test]
    fn perform_mount_propagates_mount_errors() {
        let mut fs = StubMountFs::new();
        fs.mount_fails_with = Some(std::io::ErrorKind::PermissionDenied);
        let r = perform_mount(&fs, "tag", "/data/foo", false);
        match r {
            ShareResult::Error { kind, .. } => assert_eq!(kind, ShareErrorKind::MountFailed),
            other => panic!("expected MountFailed, got {other:?}"),
        }
    }

    #[test]
    fn perform_unmount_returns_busy_without_force() {
        let mut fs = StubMountFs::new();
        fs.umount_busy_unless_force = true;
        let r = perform_unmount(&fs, "/data/foo", false);
        match r {
            ShareResult::Error { kind, .. } => assert_eq!(kind, ShareErrorKind::Busy),
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    #[test]
    fn perform_unmount_succeeds_when_force_passed() {
        let mut fs = StubMountFs::new();
        fs.umount_busy_unless_force = true;
        let r = perform_unmount(&fs, "/data/foo", true);
        assert!(matches!(r, ShareResult::Unmounted));
    }

    #[test]
    fn handle_unmount_rejects_traversal() {
        let r = handle_unmount("/data/../etc", false);
        match r {
            ShareResult::Error { kind, .. } => assert_eq!(kind, ShareErrorKind::BadPath),
            other => panic!("expected BadPath, got {other:?}"),
        }
    }
}
