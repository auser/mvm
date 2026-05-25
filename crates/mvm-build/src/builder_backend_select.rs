//! Plan 97 Phase C — builder-runtime backend selection.
//!
//! Picks between [`libkrun_builder::LibkrunBuilderVm`] (default) and
//! [`vz_builder::VzBuilderVm`] (opt-in via `MVM_BUILDER_BACKEND=vz`).
//! Returns `Box<dyn BuilderVm>` so callers do not need to switch on
//! the concrete type — both drivers implement
//! [`builder_vm::BuilderVm`] with byte-identical artifact contracts
//! (`finalize_flake_job` / `finalize_install_job` from PRs #436/#437
//! produce the same [`builder_vm::BuilderArtifacts`] shape regardless
//! of which hypervisor booted the guest).
//!
//! ## Env-var dispatch
//!
//! - `MVM_BUILDER_BACKEND` unset → libkrun (the default)
//! - `MVM_BUILDER_BACKEND=libkrun` → libkrun (explicit)
//! - `MVM_BUILDER_BACKEND=vz` → Vz
//! - Any other value → libkrun, with a `tracing::warn!` so a typo is
//!   visible without aborting the build.
//!
//! Case-insensitive on both halves: `Vz`, `VZ`, `vz` all parse the
//! same; surrounding whitespace is trimmed.

use crate::builder_vm::BuilderVm;
use crate::libkrun_builder::LibkrunBuilderVm;
use crate::vz_builder::VzBuilderVm;

/// Env-var name the dispatch consults. Surfaced as a constant so
/// `mvmctl doctor` can reference it without re-deriving the string.
pub const MVM_BUILDER_BACKEND_ENV: &str = "MVM_BUILDER_BACKEND";

/// Recognised choices for [`MVM_BUILDER_BACKEND_ENV`]. Kept as a
/// tagged enum so a future addition (e.g. Firecracker-builder on
/// Linux) is a `match` exhaustiveness check rather than a string
/// drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderBackendChoice {
    /// libkrun-backed builder VM. Default when the env var is unset
    /// or holds a value we don't recognise.
    Libkrun,
    /// Vz-backed builder VM. Opt-in via `MVM_BUILDER_BACKEND=vz`.
    Vz,
}

impl BuilderBackendChoice {
    /// Human-readable name suitable for log + error messages.
    pub fn name(self) -> &'static str {
        match self {
            BuilderBackendChoice::Libkrun => "libkrun",
            BuilderBackendChoice::Vz => "vz",
        }
    }
}

/// Resolve the env var to a [`BuilderBackendChoice`]. Pure function
/// so unit tests can exercise the parsing without spawning a VM.
/// Unset and unrecognised values both resolve to
/// [`BuilderBackendChoice::Libkrun`] — the latter logs a warning
/// via `tracing::warn!` so a typo is visible without aborting the
/// build.
pub fn resolve_choice() -> BuilderBackendChoice {
    let Some(raw) = std::env::var_os(MVM_BUILDER_BACKEND_ENV) else {
        return BuilderBackendChoice::Libkrun;
    };
    let s = raw.to_string_lossy();
    let trimmed = s.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "" | "libkrun" => BuilderBackendChoice::Libkrun,
        "vz" => BuilderBackendChoice::Vz,
        other => {
            tracing::warn!(
                value = %other,
                "{MVM_BUILDER_BACKEND_ENV} value not recognised; falling back to libkrun"
            );
            BuilderBackendChoice::Libkrun
        }
    }
}

/// Construct the builder driver the env var selects. Returns a
/// boxed trait object so callers don't have to enumerate concrete
/// types at the call site.
///
/// Both drivers construct via `::default()` — neither does I/O at
/// construction time. The first I/O happens inside `run_build`
/// (image lookup, lock acquire, supervisor spawn).
pub fn resolve_builder_backend() -> Box<dyn BuilderVm> {
    match resolve_choice() {
        BuilderBackendChoice::Libkrun => Box::new(LibkrunBuilderVm::default()),
        BuilderBackendChoice::Vz => Box::new(VzBuilderVm::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    /// Process-wide lock for env mutation. Same pattern the
    /// `builder_vm_timeout` tests use; serialises tests so concurrent
    /// threads don't observe each other's writes.
    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn with_env<F: FnOnce() -> R, R>(value: Option<&str>, f: F) -> R {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os(MVM_BUILDER_BACKEND_ENV);
        // SAFETY: tests serialise env mutation via ENV_LOCK.
        unsafe {
            match value {
                Some(v) => std::env::set_var(MVM_BUILDER_BACKEND_ENV, v),
                None => std::env::remove_var(MVM_BUILDER_BACKEND_ENV),
            }
        }
        let result = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(MVM_BUILDER_BACKEND_ENV, v),
                None => std::env::remove_var(MVM_BUILDER_BACKEND_ENV),
            }
        }
        result
    }

    #[test]
    fn resolve_choice_unset_picks_libkrun() {
        with_env(None, || {
            assert_eq!(resolve_choice(), BuilderBackendChoice::Libkrun);
        });
    }

    #[test]
    fn resolve_choice_explicit_libkrun_picks_libkrun() {
        with_env(Some("libkrun"), || {
            assert_eq!(resolve_choice(), BuilderBackendChoice::Libkrun);
        });
    }

    #[test]
    fn resolve_choice_lowercase_vz_picks_vz() {
        with_env(Some("vz"), || {
            assert_eq!(resolve_choice(), BuilderBackendChoice::Vz);
        });
    }

    #[test]
    fn resolve_choice_uppercase_vz_picks_vz() {
        // Case-insensitive matters because operators set this in
        // shell rc files and the convention varies. `Vz` is the
        // crate name; `VZ` is the entitlement string. Both should
        // work.
        with_env(Some("VZ"), || {
            assert_eq!(resolve_choice(), BuilderBackendChoice::Vz);
        });
    }

    #[test]
    fn resolve_choice_surrounding_whitespace_is_trimmed() {
        with_env(Some("  vz  "), || {
            assert_eq!(resolve_choice(), BuilderBackendChoice::Vz);
        });
    }

    #[test]
    fn resolve_choice_unrecognised_value_falls_back_to_libkrun() {
        // Typo / future backend / accidental value all surface as
        // libkrun. The tracing::warn! is observed manually in
        // production logs.
        with_env(Some("firecracker"), || {
            assert_eq!(resolve_choice(), BuilderBackendChoice::Libkrun);
        });
    }

    #[test]
    fn resolve_choice_empty_string_picks_libkrun() {
        // `MVM_BUILDER_BACKEND=` shows up in tooling that exports
        // every shell var unconditionally; treat as unset.
        with_env(Some(""), || {
            assert_eq!(resolve_choice(), BuilderBackendChoice::Libkrun);
        });
    }

    #[test]
    fn backend_choice_name_round_trips() {
        assert_eq!(BuilderBackendChoice::Libkrun.name(), "libkrun");
        assert_eq!(BuilderBackendChoice::Vz.name(), "vz");
    }

    #[test]
    fn resolve_builder_backend_constructs_libkrun_by_default() {
        // resolve_builder_backend doesn't expose the concrete type,
        // so we can only assert the call returns *some* driver.
        // The other tests cover the choice mapping; this one pins
        // the wiring through the dispatch.
        with_env(None, || {
            let _backend = resolve_builder_backend();
        });
    }

    #[test]
    fn resolve_builder_backend_constructs_vz_when_env_picks_vz() {
        with_env(Some("vz"), || {
            let _backend = resolve_builder_backend();
        });
    }
}
