//! `mvmctl cp` — copy one file between the host and a running VM.

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use std::io::Write;
use std::path::{Path, PathBuf};

use mvm_core::naming::validate_vm_name;
use mvm_core::user_config::MvmConfig;
use mvm_guest::vsock::{FsResult, GuestRequest};

use super::Cli;

const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_MODE: u32 = 0o644;

#[derive(ClapArgs, Debug, Clone)]
pub(in crate::commands) struct Args {
    /// Source endpoint. Remote endpoints use `VM:/absolute/path`.
    pub source: String,
    /// Destination endpoint. Remote endpoints use `VM:/absolute/path`.
    pub destination: String,
    /// Overwrite the destination if it already exists.
    #[arg(long, short)]
    pub force: bool,
    /// Create destination parent directories.
    #[arg(long, short = 'p')]
    pub create_parents: bool,
    /// Maximum bytes to copy. Defaults to 16 MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_BYTES)]
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Endpoint {
    Host(PathBuf),
    Guest { vm: String, path: String },
}

pub(in crate::commands) fn run(_cli: &Cli, args: Args, _cfg: &MvmConfig) -> Result<()> {
    let source = parse_endpoint(&args.source)?;
    let destination = parse_endpoint(&args.destination)?;
    match (&source, &destination) {
        (Endpoint::Host(host), Endpoint::Guest { vm, path }) => {
            copy_host_to_guest(host, vm, path, &args)
        }
        (Endpoint::Guest { vm, path }, Endpoint::Host(host)) => {
            copy_guest_to_host(vm, path, host, &args)
        }
        (Endpoint::Guest { .. }, Endpoint::Guest { .. }) => {
            bail!("mvmctl cp supports exactly one VM endpoint; use `mvmctl fs mv` inside one VM")
        }
        (Endpoint::Host(_), Endpoint::Host(_)) => {
            bail!("mvmctl cp requires one endpoint in `VM:/absolute/path` form")
        }
    }
}

fn copy_host_to_guest(host: &Path, vm: &str, guest_path: &str, args: &Args) -> Result<()> {
    let meta = std::fs::metadata(host)
        .with_context(|| format!("Failed to stat host source {}", host.display()))?;
    if !meta.is_file() {
        bail!("Host source {} is not a regular file", host.display());
    }
    if meta.len() > args.max_bytes {
        bail!(
            "Host source {} is {} bytes, above --max-bytes {}",
            host.display(),
            meta.len(),
            args.max_bytes
        );
    }
    let content = std::fs::read(host)
        .with_context(|| format!("Failed to read host source {}", host.display()))?;
    if !args.force && guest_exists(vm, guest_path)? {
        bail!("Guest destination {vm}:{guest_path} exists; pass --force to overwrite");
    }
    let dir = super::fs::instance_dir_for(vm)?;
    let req = GuestRequest::FsWrite {
        path: guest_path.to_string(),
        content,
        mode: DEFAULT_MODE,
        create_parents: args.create_parents,
        follow_symlinks: false,
    };
    match unwrap_fs(mvm_guest::vsock::send_fs_request(&dir, req)?)? {
        FsResult::Write { bytes_written } => {
            eprintln!("copied {} bytes to {vm}:{guest_path}", bytes_written);
            mvm_core::audit_emit!(
                VmFileCopy,
                vm: vm,
                "direction=host_to_guest path={guest_path} bytes={bytes_written}"
            );
            Ok(())
        }
        other => bail!("Unexpected FsResult variant for Write: {:?}", other),
    }
}

fn copy_guest_to_host(vm: &str, guest_path: &str, host: &Path, args: &Args) -> Result<()> {
    let stat = guest_stat(vm, guest_path)?;
    if !matches!(stat.kind, mvm_guest::vsock::FsEntryKind::File) {
        bail!("Guest source {vm}:{guest_path} is not a regular file");
    }
    if stat.size > args.max_bytes {
        bail!(
            "Guest source {vm}:{guest_path} is {} bytes, above --max-bytes {}",
            stat.size,
            args.max_bytes
        );
    }
    if host.exists() && !args.force {
        bail!(
            "Host destination {} exists; pass --force to overwrite",
            host.display()
        );
    }
    if args.create_parents
        && let Some(parent) = host.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create host destination parent {}",
                parent.display()
            )
        })?;
    }
    let dir = super::fs::instance_dir_for(vm)?;
    let req = GuestRequest::FsRead {
        path: guest_path.to_string(),
        offset: None,
        length: stat.size,
        follow_symlinks: true,
    };
    match unwrap_fs(mvm_guest::vsock::send_fs_request(&dir, req)?)? {
        FsResult::Read { content, .. } => {
            if content.len() as u64 != stat.size {
                bail!(
                    "Guest read returned {} bytes, expected {}",
                    content.len(),
                    stat.size
                );
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true);
            if args.force {
                options.truncate(true);
            } else {
                options.create_new(true);
            }
            let mut file = options
                .open(host)
                .with_context(|| format!("Failed to open host destination {}", host.display()))?;
            file.write_all(&content)
                .with_context(|| format!("Failed to write host destination {}", host.display()))?;
            eprintln!("copied {} bytes to {}", content.len(), host.display());
            mvm_core::audit_emit!(
                VmFileCopy,
                vm: vm,
                "direction=guest_to_host path={guest_path} bytes={}",
                content.len()
            );
            Ok(())
        }
        other => bail!("Unexpected FsResult variant for Read: {:?}", other),
    }
}

fn parse_endpoint(raw: &str) -> Result<Endpoint> {
    if let Some((vm, path)) = raw.split_once(':')
        && !vm.is_empty()
        && path.starts_with('/')
    {
        validate_vm_name(vm).with_context(|| format!("Invalid VM name: {:?}", vm))?;
        return Ok(Endpoint::Guest {
            vm: vm.to_string(),
            path: path.to_string(),
        });
    }
    Ok(Endpoint::Host(PathBuf::from(raw)))
}

fn guest_exists(vm: &str, path: &str) -> Result<bool> {
    let dir = super::fs::instance_dir_for(vm)?;
    let req = GuestRequest::FsStat {
        path: path.to_string(),
        follow_symlinks: false,
    };
    match mvm_guest::vsock::send_fs_request(&dir, req)? {
        FsResult::Stat(_) => Ok(true),
        FsResult::Error {
            kind: mvm_guest::vsock::FsErrorKind::NotFound,
            ..
        } => Ok(false),
        FsResult::Error { kind, message } => bail!("Guest FS error ({:?}): {}", kind, message),
        other => bail!("Unexpected FsResult variant for Stat: {:?}", other),
    }
}

fn guest_stat(vm: &str, path: &str) -> Result<mvm_guest::vsock::FsStat> {
    let dir = super::fs::instance_dir_for(vm)?;
    let req = GuestRequest::FsStat {
        path: path.to_string(),
        follow_symlinks: true,
    };
    match unwrap_fs(mvm_guest::vsock::send_fs_request(&dir, req)?)? {
        FsResult::Stat(stat) => Ok(stat),
        other => bail!("Unexpected FsResult variant for Stat: {:?}", other),
    }
}

fn unwrap_fs(result: FsResult) -> Result<FsResult> {
    if let FsResult::Error { kind, message } = &result {
        bail!("Guest FS error ({:?}): {}", kind, message);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_accepts_guest_absolute_path() {
        assert_eq!(
            parse_endpoint("vm1:/tmp/file").expect("parse"),
            Endpoint::Guest {
                vm: "vm1".to_string(),
                path: "/tmp/file".to_string(),
            }
        );
    }

    #[test]
    fn parse_endpoint_treats_relative_colon_path_as_host_path() {
        assert_eq!(
            parse_endpoint("notes:today.txt").expect("parse"),
            Endpoint::Host(PathBuf::from("notes:today.txt"))
        );
    }

    #[test]
    fn parse_endpoint_rejects_invalid_vm_name() {
        assert!(parse_endpoint("BadName:/tmp/file").is_err());
    }
}
