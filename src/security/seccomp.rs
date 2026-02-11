use anyhow::{Context, Result};

use crate::infra::shell;

const SECCOMP_DIR: &str = "/var/lib/mvm/seccomp";

/// Get the seccomp filter file path for Firecracker based on policy.
///
/// - "baseline": None (Firecracker's built-in default seccomp)
/// - "strict": path to custom restricted profile
pub fn seccomp_filter_path(policy: &str) -> Option<String> {
    match policy {
        "strict" => Some(format!("{}/strict.json", SECCOMP_DIR)),
        _ => None, // "baseline" — use Firecracker's built-in
    }
}

/// Ensure the strict seccomp profile exists on disk.
///
/// Generates a restrictive BPF filter that only allows syscalls
/// required by Firecracker (based on the official recommended set).
pub fn ensure_strict_profile() -> Result<()> {
    let profile = strict_profile_json();

    shell::run_in_vm(&format!(
        r#"
        mkdir -p {dir}
        cat > {dir}/strict.json << 'MVMEOF'
{json}
MVMEOF
        "#,
        dir = SECCOMP_DIR,
        json = profile,
    ))
    .with_context(|| "Failed to write strict seccomp profile")?;

    Ok(())
}

/// Generate the strict seccomp filter JSON for Firecracker.
///
/// This is a restrictive allowlist of syscalls that Firecracker needs.
/// Based on the official Firecracker seccomp recommendations.
fn strict_profile_json() -> &'static str {
    r#"{
  "Vmm": {
    "default_action": "trap",
    "filter_action": "allow",
    "filter": [
      { "syscall": "accept4" },
      { "syscall": "brk" },
      { "syscall": "clock_gettime" },
      { "syscall": "close" },
      { "syscall": "dup" },
      { "syscall": "epoll_create1" },
      { "syscall": "epoll_ctl" },
      { "syscall": "epoll_pwait" },
      { "syscall": "exit" },
      { "syscall": "exit_group" },
      { "syscall": "fallocate" },
      { "syscall": "fcntl" },
      { "syscall": "fstat" },
      { "syscall": "futex" },
      { "syscall": "getrandom" },
      { "syscall": "ioctl" },
      { "syscall": "lseek" },
      { "syscall": "madvise" },
      { "syscall": "mmap" },
      { "syscall": "mprotect" },
      { "syscall": "munmap" },
      { "syscall": "newfstatat" },
      { "syscall": "open" },
      { "syscall": "openat" },
      { "syscall": "pipe2" },
      { "syscall": "read" },
      { "syscall": "readv" },
      { "syscall": "recvfrom" },
      { "syscall": "recvmsg" },
      { "syscall": "rt_sigaction" },
      { "syscall": "rt_sigprocmask" },
      { "syscall": "rt_sigreturn" },
      { "syscall": "sendmsg" },
      { "syscall": "sendto" },
      { "syscall": "set_robust_list" },
      { "syscall": "sigaltstack" },
      { "syscall": "socket" },
      { "syscall": "timerfd_create" },
      { "syscall": "timerfd_settime" },
      { "syscall": "tkill" },
      { "syscall": "write" },
      { "syscall": "writev" }
    ]
  },
  "Api": {
    "default_action": "trap",
    "filter_action": "allow",
    "filter": [
      { "syscall": "accept4" },
      { "syscall": "bind" },
      { "syscall": "close" },
      { "syscall": "epoll_create1" },
      { "syscall": "epoll_ctl" },
      { "syscall": "epoll_pwait" },
      { "syscall": "exit" },
      { "syscall": "exit_group" },
      { "syscall": "futex" },
      { "syscall": "listen" },
      { "syscall": "read" },
      { "syscall": "recvfrom" },
      { "syscall": "rt_sigaction" },
      { "syscall": "rt_sigprocmask" },
      { "syscall": "rt_sigreturn" },
      { "syscall": "sendto" },
      { "syscall": "sigaltstack" },
      { "syscall": "socket" },
      { "syscall": "write" }
    ]
  },
  "Vcpu": {
    "default_action": "trap",
    "filter_action": "allow",
    "filter": [
      { "syscall": "close" },
      { "syscall": "exit" },
      { "syscall": "exit_group" },
      { "syscall": "futex" },
      { "syscall": "ioctl" },
      { "syscall": "read" },
      { "syscall": "rt_sigaction" },
      { "syscall": "rt_sigprocmask" },
      { "syscall": "rt_sigreturn" },
      { "syscall": "sigaltstack" },
      { "syscall": "write" }
    ]
  }
}"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seccomp_filter_path_baseline() {
        assert!(seccomp_filter_path("baseline").is_none());
    }

    #[test]
    fn test_seccomp_filter_path_strict() {
        let path = seccomp_filter_path("strict").unwrap();
        assert!(path.contains("strict.json"));
        assert!(path.starts_with("/var/lib/mvm/seccomp"));
    }

    #[test]
    fn test_seccomp_filter_path_unknown() {
        assert!(seccomp_filter_path("unknown").is_none());
    }

    #[test]
    fn test_strict_profile_valid_json() {
        let json = strict_profile_json();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed.get("Vmm").is_some());
        assert!(parsed.get("Api").is_some());
        assert!(parsed.get("Vcpu").is_some());
    }
}
