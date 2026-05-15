//! In-guest egress lockdown — Plan 73 Followup B.2.y / ADR-047.
//!
//! Defense-in-depth on top of `mvm-egress-proxy`: even if a
//! build step ignores `HTTP_PROXY` / `HTTPS_PROXY` env vars, its
//! packets get DROPped before leaving the VM because of an
//! `OUTPUT`-chain default-deny rule. Only loopback traffic (so
//! the in-VM proxy is reachable from `localhost:8443`) and
//! outbound traffic owned by the proxy's uid are allowed out.
//!
//! Called from `mvm-builder-init`'s boot sequence after the
//! basic `setup_network()`. Failure is **fatal** — without the
//! lockdown, the builder VM's egress allowlist is unenforced and
//! ADR-002's Claim 9 transitive trust onto the builder VM has no
//! defense layer.

use std::process::Command;

/// Dedicated uid the in-VM `mvm-egress-proxy` runs under. The
/// iptables `--uid-owner` rule keys off this value, so it must
/// match the uid the proxy actually executes as
/// (`crates/mvm-builder-init/src/proxy.rs` passes the same
/// constant to `Command::uid` before exec). Picked between
/// `mk-guest.nix`'s `entrypointUid` (default 1000) and
/// `agentUid` (default 1900) to avoid collisions.
pub const PROXY_UID: u32 = 1801;

/// Adapter trait so unit tests can inject a recording / failing
/// fake without binding a real iptables runtime. Each call
/// corresponds to one iptables invocation.
pub trait IptablesRunner {
    fn run(&self, args: &[&str]) -> Result<(), String>;
}

/// Production runner — shells out to `iptables` on `$PATH`. The
/// builder-vm rootfs ships busybox iptables via the flake's
/// `builderPackages` set.
pub struct SystemIptables;

impl IptablesRunner for SystemIptables {
    fn run(&self, args: &[&str]) -> Result<(), String> {
        let output = Command::new("iptables")
            .args(args)
            .output()
            .map_err(|e| format!("spawn iptables {args:?}: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "iptables {args:?} exited {} (stderr: {})",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
        Ok(())
    }
}

/// Install the three `OUTPUT`-chain rules that lock down egress
/// to the proxy uid only. Order matters:
///
/// 1. Loopback ACCEPT — the proxy listens on `127.0.0.1:8443`
///    and must remain reachable from other guest processes.
/// 2. Owner-match ACCEPT — only the proxy's uid gets outbound.
/// 3. Default-deny — set the `OUTPUT` chain's policy to DROP so
///    anything that doesn't match the prior ACCEPTs is dropped.
///
/// Returns an error on the first rule that fails to install. A
/// partial-install state is left behind on failure (e.g., rules
/// 1 and 2 installed but policy not yet flipped) — the caller
/// must treat the error as fatal and refuse to run the workload,
/// because that partial state is more permissive than the
/// pre-call baseline (no rules) and just leaving it in place
/// would let unrestricted egress continue.
pub fn install_egress_lockdown(runner: &dyn IptablesRunner, proxy_uid: u32) -> Result<(), String> {
    let uid_str = proxy_uid.to_string();
    runner.run(&["-A", "OUTPUT", "-o", "lo", "-j", "ACCEPT"])?;
    runner.run(&[
        "-A",
        "OUTPUT",
        "-m",
        "owner",
        "--uid-owner",
        &uid_str,
        "-j",
        "ACCEPT",
    ])?;
    runner.run(&["-P", "OUTPUT", "DROP"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct RecordingRunner {
        invocations: RefCell<Vec<Vec<String>>>,
        fail_at: Option<usize>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                invocations: RefCell::new(Vec::new()),
                fail_at: None,
            }
        }

        fn fail_at(idx: usize) -> Self {
            Self {
                invocations: RefCell::new(Vec::new()),
                fail_at: Some(idx),
            }
        }
    }

    impl IptablesRunner for RecordingRunner {
        fn run(&self, args: &[&str]) -> Result<(), String> {
            let mut invocations = self.invocations.borrow_mut();
            let idx = invocations.len();
            invocations.push(args.iter().map(|s| s.to_string()).collect());
            if Some(idx) == self.fail_at {
                Err(format!("forced failure at invocation {idx}"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn emits_three_rules_in_order() {
        let runner = RecordingRunner::new();
        install_egress_lockdown(&runner, 1801).expect("happy path");
        let invocations = runner.invocations.borrow();
        assert_eq!(invocations.len(), 3);

        // Rule 1: loopback ACCEPT.
        assert!(invocations[0].iter().any(|a| a == "-o"));
        assert!(invocations[0].iter().any(|a| a == "lo"));
        assert!(invocations[0].iter().any(|a| a == "ACCEPT"));

        // Rule 2: owner-match ACCEPT for our uid.
        assert!(invocations[1].iter().any(|a| a == "--uid-owner"));
        assert!(invocations[1].iter().any(|a| a == "1801"));
        assert!(invocations[1].iter().any(|a| a == "ACCEPT"));

        // Rule 3: OUTPUT chain default policy DROP.
        assert!(invocations[2].iter().any(|a| a == "-P"));
        assert!(invocations[2].iter().any(|a| a == "OUTPUT"));
        assert!(invocations[2].iter().any(|a| a == "DROP"));
    }

    #[test]
    fn stops_at_first_failure() {
        let runner = RecordingRunner::fail_at(0);
        let result = install_egress_lockdown(&runner, 1801);
        assert!(result.is_err());
        assert_eq!(runner.invocations.borrow().len(), 1);
    }

    #[test]
    fn stops_at_middle_failure() {
        let runner = RecordingRunner::fail_at(1);
        let result = install_egress_lockdown(&runner, 1801);
        assert!(result.is_err());
        assert_eq!(
            runner.invocations.borrow().len(),
            2,
            "rule 2 attempted, rule 3 (DROP policy) not reached",
        );
    }

    #[test]
    fn uses_provided_uid() {
        let runner = RecordingRunner::new();
        install_egress_lockdown(&runner, 9999).expect("happy path");
        assert!(
            runner.invocations.borrow()[1].iter().any(|a| a == "9999"),
            "uid passed through to --uid-owner",
        );
    }

    #[test]
    fn proxy_uid_constant_avoids_known_uids() {
        // 0 = root (would defeat the rule); 1000 = mkGuest's
        // default entrypointUid; 1900 = its default agentUid.
        assert_ne!(PROXY_UID, 0);
        assert_ne!(PROXY_UID, 1000);
        assert_ne!(PROXY_UID, 1900);
    }
}
