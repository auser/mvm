//! Safe wrappers around the bindgen-generated libkrun FFI.
//!
//! Only compiled when the `libkrun-sys` feature is on. The wrappers
//! translate libkrun's "negative i32 = errno" convention into
//! `Result<_, super::Error>` and own all `CString` conversions for
//! path arguments. Each wrapper is one FFI call deep.
//!
//! `krun_start_enter` is not called from here — it blocks until the
//! guest exits and is the W3+W4 lifecycle scope. The Plan 57 W1
//! deliverable is the bindings + thin wrapper only; consumers exercise
//! the configuration calls today and pick up boot in a later PR.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::missing_safety_doc,
    clippy::too_many_arguments
)]

use std::ffi::CString;
use std::path::Path;

use crate::Error;

mod bindings {
    include!(concat!(env!("OUT_DIR"), "/libkrun_sys.rs"));
}

/// Kernel-format constants exposed to callers without leaking the
/// bindgen-generated identifier names.
#[derive(Debug, Clone, Copy)]
pub enum KernelFormat {
    Raw,
    Elf,
    PeGz,
    ImageBz2,
    ImageGz,
    ImageZstd,
}

impl KernelFormat {
    fn as_u32(self) -> u32 {
        match self {
            KernelFormat::Raw => bindings::KRUN_KERNEL_FORMAT_RAW,
            KernelFormat::Elf => bindings::KRUN_KERNEL_FORMAT_ELF,
            KernelFormat::PeGz => bindings::KRUN_KERNEL_FORMAT_PE_GZ,
            KernelFormat::ImageBz2 => bindings::KRUN_KERNEL_FORMAT_IMAGE_BZ2,
            KernelFormat::ImageGz => bindings::KRUN_KERNEL_FORMAT_IMAGE_GZ,
            KernelFormat::ImageZstd => bindings::KRUN_KERNEL_FORMAT_IMAGE_ZSTD,
        }
    }
}

/// Log-level constants matching `KRUN_LOG_LEVEL_*` in libkrun.h.
#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_u32(self) -> u32 {
        match self {
            LogLevel::Off => 0,
            LogLevel::Error => 1,
            LogLevel::Warn => 2,
            LogLevel::Info => 3,
            LogLevel::Debug => 4,
            LogLevel::Trace => 5,
        }
    }
}

/// Owned libkrun configuration context. Frees the underlying ctx_id on
/// drop so callers don't have to thread cleanup through every error
/// path.
pub struct Context {
    ctx_id: u32,
}

impl Context {
    /// Allocate a new libkrun configuration context.
    pub fn new() -> Result<Self, Error> {
        let rc = unsafe { bindings::krun_create_ctx() };
        if rc < 0 {
            return Err(Error::Krun(rc));
        }
        // krun_create_ctx returns the ctx id as a non-negative i32.
        Ok(Self { ctx_id: rc as u32 })
    }

    pub fn id(&self) -> u32 {
        self.ctx_id
    }

    pub fn set_vm_config(&self, num_vcpus: u8, ram_mib: u32) -> Result<(), Error> {
        check(unsafe { bindings::krun_set_vm_config(self.ctx_id, num_vcpus, ram_mib) })
    }

    pub fn set_root_disk(&self, disk_path: &Path) -> Result<(), Error> {
        let path = cstring(disk_path)?;
        check(unsafe { bindings::krun_set_root_disk(self.ctx_id, path.as_ptr()) })
    }

    pub fn set_kernel(
        &self,
        kernel_path: &Path,
        kernel_format: KernelFormat,
        initramfs: Option<&Path>,
        cmdline: Option<&str>,
    ) -> Result<(), Error> {
        let kernel = cstring(kernel_path)?;
        let initramfs = match initramfs {
            Some(p) => Some(cstring(p)?),
            None => None,
        };
        let cmdline = match cmdline {
            Some(s) => Some(CString::new(s).map_err(|_| Error::InvalidCString)?),
            None => None,
        };
        let initramfs_ptr = initramfs.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        let cmdline_ptr = cmdline.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        check(unsafe {
            bindings::krun_set_kernel(
                self.ctx_id,
                kernel.as_ptr(),
                kernel_format.as_u32(),
                initramfs_ptr,
                cmdline_ptr,
            )
        })
    }

    pub fn add_vsock(&self, tsi_features: u32) -> Result<(), Error> {
        check(unsafe { bindings::krun_add_vsock(self.ctx_id, tsi_features) })
    }

    pub fn add_vsock_port(&self, port: u32, host_path: &Path) -> Result<(), Error> {
        let path = cstring(host_path)?;
        check(unsafe { bindings::krun_add_vsock_port(self.ctx_id, port, path.as_ptr()) })
    }

    pub fn add_virtiofs(&self, tag: &str, host_path: &Path) -> Result<(), Error> {
        let tag = CString::new(tag).map_err(|_| Error::InvalidCString)?;
        let path = cstring(host_path)?;
        check(unsafe { bindings::krun_add_virtiofs(self.ctx_id, tag.as_ptr(), path.as_ptr()) })
    }

    pub fn add_disk(&self, block_id: &str, disk_path: &Path, read_only: bool) -> Result<(), Error> {
        let block = CString::new(block_id).map_err(|_| Error::InvalidCString)?;
        let path = cstring(disk_path)?;
        check(unsafe {
            bindings::krun_add_disk(self.ctx_id, block.as_ptr(), path.as_ptr(), read_only)
        })
    }

    pub fn set_console_output(&self, host_path: &Path) -> Result<(), Error> {
        let path = cstring(host_path)?;
        check(unsafe { bindings::krun_set_console_output(self.ctx_id, path.as_ptr()) })
    }

    /// Returns the shutdown eventfd. The caller is responsible for
    /// closing it (libkrun documents that the fd is owned by the
    /// caller once returned).
    pub fn shutdown_eventfd(&self) -> Result<i32, Error> {
        let rc = unsafe { bindings::krun_get_shutdown_eventfd(self.ctx_id) };
        if rc < 0 { Err(Error::Krun(rc)) } else { Ok(rc) }
    }

    /// Block the calling thread, starting the guest. libkrun's
    /// `krun_start_enter` calls `exit()` on success with the guest's
    /// status; on failure it returns a negative errno. Plan 57 W1
    /// surfaces the wrapper but does not call it from `crate::start` —
    /// that lands in W3 + W4 alongside the registry that owns the
    /// blocking thread.
    pub fn start_enter(&self) -> Result<std::convert::Infallible, Error> {
        let rc = unsafe { bindings::krun_start_enter(self.ctx_id) };
        // On success the call does not return; if we observe one, it's
        // an error path.
        Err(Error::Krun(rc))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let rc = unsafe { bindings::krun_free_ctx(self.ctx_id) };
        if rc < 0 {
            tracing::warn!(
                ctx_id = self.ctx_id,
                rc,
                "krun_free_ctx returned non-zero on drop"
            );
        }
    }
}

/// Set libkrun's global log level. Wraps `krun_set_log_level`.
pub fn set_log_level(level: LogLevel) -> Result<(), Error> {
    check(unsafe { bindings::krun_set_log_level(level.as_u32()) })
}

fn check(rc: i32) -> Result<(), Error> {
    if rc < 0 { Err(Error::Krun(rc)) } else { Ok(()) }
}

fn cstring(path: &Path) -> Result<CString, Error> {
    let s = path.to_str().ok_or(Error::InvalidCString)?;
    CString::new(s).map_err(|_| Error::InvalidCString)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_drop_context() {
        // Smoke test: the FFI links cleanly and an empty context can be
        // allocated + freed. Doesn't boot a VM (that's W3) — just proves
        // the wrapper round-trips through libkrun.
        let ctx = Context::new().expect("libkrun ctx allocation succeeds on a host with libkrun");
        assert!(ctx.id() < u32::MAX);
        // Drop runs krun_free_ctx; assertion is the absence of a panic.
    }

    #[test]
    fn log_level_is_settable() {
        // `set_log_level` is the first FFI call any process does — it
        // touches no resources beyond the global logger. A round-trip
        // here catches a broken link without needing a ctx.
        set_log_level(LogLevel::Warn).expect("krun_set_log_level returns zero");
    }

    #[test]
    fn vm_config_round_trips() {
        let ctx = Context::new().expect("ctx allocation");
        ctx.set_vm_config(1, 128).expect("vm config accepted");
    }
}
