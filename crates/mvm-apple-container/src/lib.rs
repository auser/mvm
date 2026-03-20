//! Apple Containerization framework bridge for mvm.
//!
//! On macOS 26+ with Apple Silicon, this crate provides FFI bindings to
//! a Swift static library that wraps Apple's Containerization framework.
//! On other platforms, all functions return "not available" errors.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// FFI bindings to the Swift bridge library.
#[cfg(not(apple_container_stub))]
mod ffi {
    use std::os::raw::c_char;

    unsafe extern "C" {
        pub fn mvm_apple_container_is_available() -> bool;
        pub fn mvm_apple_container_free_string(ptr: *mut c_char);
        pub fn mvm_apple_container_start(
            id: *const c_char,
            kernel_path: *const c_char,
            rootfs_path: *const c_char,
            cpus: i32,
            memory_mib: u64,
        ) -> *mut c_char;
        pub fn mvm_apple_container_stop(id: *const c_char) -> *mut c_char;
        pub fn mvm_apple_container_list() -> *mut c_char;
    }
}

/// Read a C string returned by the Swift bridge, convert to Rust String,
/// and free the original.
#[cfg(not(apple_container_stub))]
unsafe fn read_and_free(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { ffi::mvm_apple_container_free_string(ptr) };
    s
}

/// Check if Apple Containers are available on this platform.
pub fn is_available() -> bool {
    #[cfg(not(apple_container_stub))]
    {
        unsafe { ffi::mvm_apple_container_is_available() }
    }
    #[cfg(apple_container_stub)]
    {
        false
    }
}

/// Start a container from a local ext4 rootfs and kernel.
///
/// Returns `Ok(())` on success or an error message on failure.
pub fn start(
    id: &str,
    kernel_path: &str,
    rootfs_path: &str,
    cpus: u32,
    memory_mib: u64,
) -> Result<(), String> {
    #[cfg(not(apple_container_stub))]
    {
        let c_id = CString::new(id).map_err(|e| e.to_string())?;
        let c_kernel = CString::new(kernel_path).map_err(|e| e.to_string())?;
        let c_rootfs = CString::new(rootfs_path).map_err(|e| e.to_string())?;
        let result = unsafe {
            read_and_free(ffi::mvm_apple_container_start(
                c_id.as_ptr(),
                c_kernel.as_ptr(),
                c_rootfs.as_ptr(),
                cpus as i32,
                memory_mib,
            ))
        };
        if result.is_empty() {
            Ok(())
        } else {
            Err(result)
        }
    }
    #[cfg(apple_container_stub)]
    {
        let _ = (id, kernel_path, rootfs_path, cpus, memory_mib);
        Err("Apple Containers not available on this platform".to_string())
    }
}

/// Stop a running container.
pub fn stop(id: &str) -> Result<(), String> {
    #[cfg(not(apple_container_stub))]
    {
        let c_id = CString::new(id).map_err(|e| e.to_string())?;
        let result = unsafe { read_and_free(ffi::mvm_apple_container_stop(c_id.as_ptr())) };
        if result.is_empty() {
            Ok(())
        } else {
            Err(result)
        }
    }
    #[cfg(apple_container_stub)]
    {
        let _ = id;
        Err("Apple Containers not available on this platform".to_string())
    }
}

/// List running container IDs as a JSON array string.
pub fn list_ids() -> Vec<String> {
    #[cfg(not(apple_container_stub))]
    {
        let json = unsafe { read_and_free(ffi::mvm_apple_container_list()) };
        serde_json::from_str(&json).unwrap_or_default()
    }
    #[cfg(apple_container_stub)]
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
        // No containers running in test mode
        assert!(ids.is_empty());
    }
}
