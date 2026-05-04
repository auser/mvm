//! Validate the `env` parameter of `tools/call run` against the
//! installed manifest-keyed slot registry (plan 38).
//!
//! Stdio-only: this is the bridge between the wire protocol (which
//! accepts arbitrary strings) and the local slot artifacts on disk.
//! Plan 33's hosted variant in mvmd implements its own validator
//! that resolves `tenant/pool/manifest@revision`.

use anyhow::Result;

/// Returns the list of slot identifiers known to mvmctl —
/// 64-char-hex slot hashes from the manifest-driven registry, plus
/// any optional `name` aliases set in their `manifest.json`.
/// Equivalent to `mvmctl manifest ls`.
pub fn known_envs() -> Result<Vec<String>> {
    let slots = mvm_runtime::vm::template::lifecycle::template_list_slots()?;
    let mut envs = Vec::with_capacity(slots.len() * 2);
    for s in slots {
        envs.push(s.slot_hash);
        if let Some(name) = s.name {
            envs.push(name);
        }
    }
    Ok(envs)
}

/// Check that `env` is a built-in preset, a known slot hash, or a
/// known slot's display `name`. Returns `Ok(())` if so; otherwise an
/// error whose message lists the available envs so the LLM can
/// recover by re-issuing with a valid value.
///
/// Built-in presets (`shell`, `bash`, `python`, `node`) are accepted
/// unconditionally — the dispatcher path resolves them to the
/// appropriate built-in slot when they're requested.
pub fn validate_env(env: &str) -> Result<()> {
    const BUILTIN_PRESETS: &[&str] = &["shell", "bash", "python", "node"];
    if BUILTIN_PRESETS.contains(&env) {
        return Ok(());
    }
    let envs = known_envs()?;
    if envs.iter().any(|e| e == env) {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "env '{env}' is not a built-in preset or a known slot. \
         Built-in: [{}]. Built slots: [{}]. \
         Build new ones via `mvmctl init <DIR> && mvmctl build <DIR>`.",
        BUILTIN_PRESETS.join(", "),
        envs.join(", ")
    ))
}
