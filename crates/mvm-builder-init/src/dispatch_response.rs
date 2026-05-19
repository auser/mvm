//! Plan 89 W2 part 3 — hand-rolled JSON for the
//! `BuilderResponse::Result` wire frame builder-init sends right
//! before reboot.
//!
//! ## Why hand-roll
//!
//! `mvm-builder-init` keeps a ≤ 1.5 MiB rootfs size budget
//! (Plan 72 §W3) and deliberately does not pull `serde_json`
//! (`Cargo.toml` comment at the deps section). The host-side
//! consumer (`mvm_build::builder_protocol::BuilderResponse::Result`)
//! is a typed serde enum; this module mirrors its wire shape
//! exactly so the host's `serde_json::from_slice::<BuilderResponse>`
//! parses what we emit. The `builder_response_matches_typed_serde`
//! test below uses `mvm-build` as a dev-dep to pin the two sides
//! together — if anyone tweaks either struct, that test breaks
//! and points at the resync.
//!
//! ## Single-shot caveats
//!
//! `BuilderResponse::Result` carries a `job_id: JobId(Uuid)` and a
//! `job_timings: JobTimings { dispatch_ms, build_ms, teardown_ms }`.
//! In single-shot mode there is no incoming dispatch with an id and
//! no remote-dispatch round-trip to time, so:
//!
//! - `job_id` is the nil UUID
//!   (`00000000-0000-0000-0000-000000000000`). The host's
//!   single-shot caller knows to ignore it; persistent dispatch
//!   (W3) will populate it from the `BuilderRequest::Run` it
//!   correlates against.
//! - `dispatch_ms` and `teardown_ms` are `0`. Only `build_ms`
//!   carries a meaningful value, measured by the caller as
//!   `job_end_ms - job_start_ms`.

use crate::boot_timings::BootTimings;

/// Owned snapshot of the data needed to hand-roll a
/// `BuilderResponse::Result` JSON frame. The producer (the linux
/// module's `run` path) gathers these fields after `run_job`
/// returns and before `power_off`.
#[derive(Debug, Clone)]
pub(crate) struct DispatchResponse {
    pub exit_code: i32,
    pub stderr_tail: String,
    pub boot_timings: BootTimings,
    pub build_ms: u64,
}

impl DispatchResponse {
    /// Render as the exact wire JSON `mvm_build::builder_protocol::BuilderResponse::Result`
    /// deserializes from. Field order matches the serde-derived
    /// internally-tagged enum encoding (`"kind"` first, then the
    /// variant's struct fields in declaration order).
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str(
            r#"{"kind":"result","job_id":"00000000-0000-0000-0000-000000000000","exit_code":"#,
        );
        out.push_str(&self.exit_code.to_string());
        out.push_str(r#","stderr_tail":""#);
        push_json_string(&mut out, &self.stderr_tail);
        out.push_str(r#"","boot_timings":"#);
        // BootTimingsWire is shaped identically to BootTimings;
        // BootTimings::to_json emits the exact wire we want.
        out.push_str(&self.boot_timings.to_json());
        out.push_str(r#","job_timings":{"dispatch_ms":0,"build_ms":"#);
        out.push_str(&self.build_ms.to_string());
        out.push_str(r#","teardown_ms":0}}"#);
        out
    }
}

/// JSON string-escape per RFC 8259 §7. Inlined rather than calling
/// the existing `json_escape` in `main.rs` because that one is
/// `#[cfg(target_os = "linux")]`-gated under the linux module —
/// this module is cross-platform so `cargo test` on macOS hosts
/// can exercise the roundtrip.
fn push_json_string(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_timings() -> BootTimings {
        let (mut t, _anchor) = BootTimings::new(std::time::Instant::now());
        t.pseudofs_ready_ms = Some(12);
        t.nix_device_ready_ms = Some(18);
        t.nix_seeded_ms = None;
        t.nix_mounted_ms = Some(220);
        t.modules_ready_ms = Some(35);
        t.virtiofs_ready_ms = Some(48);
        t.network_ready_ms = Some(250);
        t.job_start_ms = Some(260);
        t.job_end_ms = Some(8400);
        t.poweroff_start_ms = Some(8410);
        t
    }

    #[test]
    fn dispatch_response_emits_valid_json() {
        let resp = DispatchResponse {
            exit_code: 0,
            stderr_tail: "warning: foo\nwarning: bar".to_string(),
            boot_timings: sample_timings(),
            build_ms: 8140,
        };
        let json = resp.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("must parse");
        assert_eq!(parsed["kind"], "result");
        assert_eq!(parsed["job_id"], "00000000-0000-0000-0000-000000000000");
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["stderr_tail"], "warning: foo\nwarning: bar");
        assert_eq!(parsed["job_timings"]["dispatch_ms"], 0);
        assert_eq!(parsed["job_timings"]["build_ms"], 8140);
        assert_eq!(parsed["job_timings"]["teardown_ms"], 0);
        assert_eq!(parsed["boot_timings"]["init_start_ms"], 0);
        assert_eq!(
            parsed["boot_timings"]["nix_seeded_ms"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn dispatch_response_escapes_control_chars_in_stderr_tail() {
        let resp = DispatchResponse {
            exit_code: 1,
            stderr_tail: "line1\n\"quoted\"\tand\\back\x01slash".to_string(),
            boot_timings: sample_timings(),
            build_ms: 0,
        };
        let json = resp.to_json();
        // Must round-trip through a real JSON parser.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("must parse");
        assert_eq!(
            parsed["stderr_tail"],
            "line1\n\"quoted\"\tand\\back\x01slash"
        );
    }

    #[test]
    fn dispatch_response_handles_empty_stderr_tail() {
        let resp = DispatchResponse {
            exit_code: 0,
            stderr_tail: String::new(),
            boot_timings: sample_timings(),
            build_ms: 100,
        };
        let json = resp.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("must parse");
        assert_eq!(parsed["stderr_tail"], "");
    }

    /// **The cross-validation test.** Hand-rolled JSON must
    /// deserialize as `mvm_build::builder_protocol::BuilderResponse`
    /// with `Result` variant carrying the expected fields. Bumps
    /// either struct's shape (renames, additions, type changes)
    /// trip this test, which is the signal to re-sync.
    #[test]
    fn dispatch_response_parses_as_typed_builder_response() {
        let resp = DispatchResponse {
            exit_code: 7,
            stderr_tail: "uh oh".to_string(),
            boot_timings: sample_timings(),
            build_ms: 1234,
        };
        let json = resp.to_json();
        let typed: mvm_build::builder_protocol::BuilderResponse =
            serde_json::from_str(&json).expect("must parse as typed BuilderResponse");
        match typed {
            mvm_build::builder_protocol::BuilderResponse::Result {
                job_id,
                exit_code,
                stderr_tail,
                boot_timings,
                job_timings,
            } => {
                assert_eq!(job_id.to_string(), "00000000-0000-0000-0000-000000000000");
                assert_eq!(exit_code, 7);
                assert_eq!(stderr_tail, "uh oh");
                assert_eq!(job_timings.dispatch_ms, 0);
                assert_eq!(job_timings.build_ms, 1234);
                assert_eq!(job_timings.teardown_ms, 0);
                let bt = boot_timings.expect("boot_timings present");
                assert_eq!(bt.init_start_ms, Some(0));
                assert_eq!(bt.nix_seeded_ms, None);
                assert_eq!(bt.network_ready_ms, Some(250));
            }
            other => panic!("expected Result variant, got {other:?}"),
        }
    }
}
