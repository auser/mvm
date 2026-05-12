//! Virtio-balloon reclaim controller.
//!
//! Workloads that opt into [`VmStartConfig::mem_initial_mib`] boot
//! with a pre-inflated balloon and only commit a fraction of their
//! cap. The host-side reclaim controller adjusts the balloon over
//! the VM's life so the host commits more when it has slack and
//! less when it's under pressure.
//!
//! This module ships the *policy* — a pure decision function that
//! takes a snapshot and returns an action. The wiring to a real
//! poller (sysinfo / Linux PSI / macOS memorystatus) plus the call
//! to `VmBackend::balloon_set_target` lives at the integration site
//! (the supervisor's main loop). Keeping policy and integration
//! separable makes the policy testable without spinning up a VMM.
//!
//! [`VmStartConfig::mem_initial_mib`]:
//!     mvm_core::vm_backend::VmStartConfig::mem_initial_mib

use mvm_core::vm_backend::{BalloonState, VmId};

/// Action to take on a single VM after a controller tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BalloonAction {
    /// No change required — the current target stays.
    Hold,
    /// Inflate the balloon (reclaim from guest). `target_inflate_mib`
    /// is the new absolute target, not a delta.
    Inflate { vm: VmId, target_inflate_mib: u32 },
    /// Deflate the balloon (return memory to guest). `target_inflate_mib`
    /// is the new absolute target.
    Deflate { vm: VmId, target_inflate_mib: u32 },
}

/// Host memory pressure, normalised to 0.0..=1.0. Higher = more
/// pressure (more of host memory is in use). Exact source depends
/// on the host platform; the controller treats this as opaque.
///
/// Reasonable mappings:
/// - Linux: `MemAvailable / MemTotal` inverted (1 − available_ratio).
/// - Linux PSI: the 10-second `memory.pressure.full.avg10` is a
///   stronger signal than a flat used-fraction.
/// - macOS: vm_pressure (translate the qualitative state to a
///   numeric).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct HostPressure(pub f32);

impl HostPressure {
    /// Construct, clamping to `[0.0, 1.0]`. Defensive against
    /// platform shims that return out-of-range values under odd
    /// edge cases.
    pub fn clamped(p: f32) -> Self {
        Self(p.clamp(0.0, 1.0))
    }
}

/// Two-threshold band policy for v1. Above `inflate_above`, claim
/// pages from idle guests; below `deflate_below`, hand them back.
/// Between the two, hold the current target so we don't thrash.
#[derive(Debug, Clone, Copy)]
pub struct BalloonPolicy {
    /// Inflate when host pressure rises above this fraction.
    pub inflate_above: f32,
    /// Deflate when host pressure falls below this fraction.
    pub deflate_below: f32,
    /// MiB to adjust per tick. Keeping the step modest avoids
    /// thrashing the guest's allocator under marginal pressure.
    pub step_mib: u32,
    /// Floor for guest commitment in MiB. The balloon is never
    /// inflated past `max_mib - guest_floor_mib` — under-provisioned
    /// guests OOM-kill workloads, which is worse than the host
    /// taking the pressure.
    pub guest_floor_mib: u32,
}

impl Default for BalloonPolicy {
    fn default() -> Self {
        // Defaults tuned for dev-laptop ergonomics: inflate when the
        // host is at 80% memory (a real squeeze), deflate when it's
        // back below 60% (we have headroom). 64 MiB step + 64 MiB
        // guest floor balances responsiveness against thrash.
        Self {
            inflate_above: 0.80,
            deflate_below: 0.60,
            step_mib: 64,
            guest_floor_mib: 64,
        }
    }
}

impl BalloonPolicy {
    /// Decide the action for a single VM. Pure function: no I/O,
    /// no clock, no allocator state. The caller is responsible for
    /// reading `state` from the backend and `pressure` from the host
    /// before each invocation.
    ///
    /// Invariants:
    /// - Returns `Hold` when no progress can be made (already at
    ///   ceiling or floor in the relevant direction).
    /// - Never produces a target that would push the guest below
    ///   `guest_floor_mib` of commitment.
    /// - The dead-band between `deflate_below` and `inflate_above`
    ///   prevents pressure oscillations from thrashing the balloon.
    pub fn decide(&self, vm: &VmId, state: BalloonState, pressure: HostPressure) -> BalloonAction {
        // Inflate path — claim more pages back from the guest.
        if pressure.0 >= self.inflate_above {
            // Ceiling: leave at least `guest_floor_mib` to the guest.
            let max_inflate = state.max_mib.saturating_sub(self.guest_floor_mib);
            let new_target = state
                .inflated_mib
                .saturating_add(self.step_mib)
                .min(max_inflate);
            if new_target > state.inflated_mib {
                return BalloonAction::Inflate {
                    vm: vm.clone(),
                    target_inflate_mib: new_target,
                };
            }
            return BalloonAction::Hold;
        }

        // Deflate path — return pages to the guest.
        if pressure.0 <= self.deflate_below {
            let new_target = state.inflated_mib.saturating_sub(self.step_mib);
            if new_target < state.inflated_mib {
                return BalloonAction::Deflate {
                    vm: vm.clone(),
                    target_inflate_mib: new_target,
                };
            }
            return BalloonAction::Hold;
        }

        BalloonAction::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(max_mib: u32, inflated_mib: u32) -> BalloonState {
        BalloonState {
            max_mib,
            inflated_mib,
            host_committed_mib: max_mib.saturating_sub(inflated_mib),
        }
    }

    fn vm() -> VmId {
        VmId("test-vm".to_string())
    }

    #[test]
    fn high_pressure_inflates_by_step() {
        let p = BalloonPolicy::default();
        let action = p.decide(&vm(), state(1024, 0), HostPressure(0.85));
        assert_eq!(
            action,
            BalloonAction::Inflate {
                vm: vm(),
                target_inflate_mib: p.step_mib,
            }
        );
    }

    #[test]
    fn low_pressure_deflates_by_step() {
        let p = BalloonPolicy::default();
        let action = p.decide(&vm(), state(1024, 512), HostPressure(0.40));
        assert_eq!(
            action,
            BalloonAction::Deflate {
                vm: vm(),
                target_inflate_mib: 512 - p.step_mib,
            }
        );
    }

    #[test]
    fn dead_band_holds_position() {
        let p = BalloonPolicy::default();
        // 0.70 is between 0.60 and 0.80.
        let action = p.decide(&vm(), state(1024, 256), HostPressure(0.70));
        assert_eq!(action, BalloonAction::Hold);
    }

    #[test]
    fn inflate_caps_at_guest_floor() {
        let p = BalloonPolicy {
            inflate_above: 0.80,
            deflate_below: 0.60,
            step_mib: 64,
            guest_floor_mib: 64,
        };
        // 1024 - 64 floor = 960 ceiling. Inflated already at 940;
        // adding step (64) would land at 1004, but we cap at 960.
        let action = p.decide(&vm(), state(1024, 940), HostPressure(0.95));
        assert_eq!(
            action,
            BalloonAction::Inflate {
                vm: vm(),
                target_inflate_mib: 960,
            }
        );
    }

    #[test]
    fn inflate_at_ceiling_holds() {
        let p = BalloonPolicy::default();
        // Already at the ceiling — no more headroom.
        let ceiling = 1024 - p.guest_floor_mib;
        let action = p.decide(&vm(), state(1024, ceiling), HostPressure(0.95));
        assert_eq!(action, BalloonAction::Hold);
    }

    #[test]
    fn deflate_at_zero_holds() {
        let p = BalloonPolicy::default();
        // Balloon already fully deflated — nothing to return.
        let action = p.decide(&vm(), state(1024, 0), HostPressure(0.10));
        assert_eq!(action, BalloonAction::Hold);
    }

    #[test]
    fn deflate_saturates_at_zero() {
        let p = BalloonPolicy {
            inflate_above: 0.80,
            deflate_below: 0.60,
            step_mib: 256,
            guest_floor_mib: 64,
        };
        // Only 32 MiB inflated; step is 256 — deflating must
        // saturate at 0, not underflow.
        let action = p.decide(&vm(), state(1024, 32), HostPressure(0.10));
        assert_eq!(
            action,
            BalloonAction::Deflate {
                vm: vm(),
                target_inflate_mib: 0,
            }
        );
    }

    #[test]
    fn host_pressure_clamps_out_of_range_input() {
        // Defensive clamping on the wire: a platform shim returning
        // 1.5 or -0.1 must not break the policy.
        let high = HostPressure::clamped(1.5);
        let low = HostPressure::clamped(-0.1);
        assert!((high.0 - 1.0).abs() < f32::EPSILON);
        assert!(low.0.abs() < f32::EPSILON);
    }

    #[test]
    fn exact_threshold_is_inclusive() {
        // At exactly inflate_above and deflate_below, behave as the
        // boundary direction. (`>=` / `<=` semantics.)
        let p = BalloonPolicy::default();
        let inflate = p.decide(&vm(), state(1024, 0), HostPressure(0.80));
        assert!(matches!(inflate, BalloonAction::Inflate { .. }));
        let deflate = p.decide(&vm(), state(1024, 256), HostPressure(0.60));
        assert!(matches!(deflate, BalloonAction::Deflate { .. }));
    }

    #[test]
    fn default_policy_sanity_band() {
        let p = BalloonPolicy::default();
        assert!(p.deflate_below < p.inflate_above);
        assert!(p.step_mib > 0);
        assert!(p.guest_floor_mib > 0);
    }
}
