#!/usr/bin/env bash
#
# One-shot rewrite of `mvm_core::audit::emit(KIND, vm, detail)` and
# `mvm_core::audit::event(KIND).…emit()` call sites to the
# `mvm_core::audit_emit!` macro. Companion to
# `xtask check-audit-positional`, which enforces that no positional
# emit survives the sweep.
#
# Handles the four common shapes (single- and multi-line):
#
#   emit(KIND, None, None)
#       → audit_emit!(KIND)
#
#   emit(KIND, Some(&vm), None)
#       → audit_emit!(KIND, vm: vm)
#
#   emit(KIND, None, Some(&format!("…")))
#       → audit_emit!(KIND, "…")
#
#   emit(KIND, Some(&vm), Some(&format!("…")))
#       → audit_emit!(KIND, vm: vm, "…")
#
# Shapes the script intentionally skips:
#   * `Some(literal_str)` (rare; cleaner by hand)
#   * `Some(detail_var)` where detail isn't `format!()` (would need
#     wrapping in `"{detail_var}"` which is conservative-rewriting
#     territory)
#   * Nested parens inside the `format!(…)` arg
#
# The xtask lint catches whatever this misses.

set -euo pipefail

cd "$(dirname "$0")/.."

mapfile -t files < <(
  grep -rl --include='*.rs' \
    -E 'mvm_core::audit::(emit|emit_to|event)\(' \
    crates/
)

if (( ${#files[@]} == 0 )); then
  echo "migrate-audit-emit: no positional emit call sites found"
  exit 0
fi

echo "migrate-audit-emit: rewriting ${#files[@]} file(s)"

for f in "${files[@]}"; do
  # Whole-file slurp mode (-0777) so multi-line `emit(\n ..., \n ..., \n)`
  # forms collapse. The script is idempotent — running it twice is a
  # no-op (subsequent passes find no `emit(` left to rewrite).
  perl -i -0777 -pe '
    # --- 4-arg arm: emit(KIND, Some(&?vm), Some(&format!("..." [, args]))) ---
    s{
      mvm_core::audit::emit\(
        \s* mvm_core::audit::LocalAuditKind::(\w+) \s*,
        \s* Some\(\s*&?\s*([\w\.]+)\s*\) \s*,
        \s* Some\(\s*&\s*format!\(\s*(".*?")\s*(,[^)]*)?\s*\)\s*\) \s*,?
        \s*
      \);?
    }{
      my $args = defined $4 ? "$3 $4" : $3;
      "mvm_core::audit_emit!($1, vm: $2, $args);"
    }gxse;

    # --- 3-arg arm with format-detail: emit(KIND, None, Some(&format!(…))) ---
    s{
      mvm_core::audit::emit\(
        \s* mvm_core::audit::LocalAuditKind::(\w+) \s*,
        \s* None \s*,
        \s* Some\(\s*&\s*format!\(\s*(".*?")\s*(,[^)]*)?\s*\)\s*\) \s*,?
        \s*
      \);?
    }{
      my $args = defined $3 ? "$2 $3" : $2;
      "mvm_core::audit_emit!($1, $args);"
    }gxse;

    # --- 3-arg arm with vm only: emit(KIND, Some(&vm), None) ---
    s{
      mvm_core::audit::emit\(
        \s* mvm_core::audit::LocalAuditKind::(\w+) \s*,
        \s* Some\(\s*&\s*(\w+)\s*\) \s*,
        \s* None \s*,?
        \s*
      \);?
    }{mvm_core::audit_emit!($1, vm: &$2);}gxs;

    # --- bare arm: emit(KIND, None, None) ---
    s{
      mvm_core::audit::emit\(
        \s* mvm_core::audit::LocalAuditKind::(\w+) \s*,
        \s* None \s*,
        \s* None \s*,?
        \s*
      \);?
    }{mvm_core::audit_emit!($1);}gxs;

    # --- vm + detail (broader vm patterns): vm can be `&ident`,
    # `ident`, `&self.field`, etc.; detail can be any expression
    # that fits on one regex match (no nested parens). The detail
    # text is wrapped in `"{detail}"` so format!() can do inline
    # substitution from the local binding (works for plain idents,
    # field access, literal string-refs). When detail is a more
    # complex expression (e.g. `if cond { a } else { b }`), this
    # will not fit cleanly and the lint flags it for manual review.
    s{
      mvm_core::audit::emit\(
        \s* mvm_core::audit::LocalAuditKind::(\w+) \s*,
        \s* Some\(\s*&?\s*([\w\.]+)\s*\) \s*,
        \s* Some\(\s*&?\s*([\w\.]+)\s*\) \s*,?
        \s*
      \);?
    }{mvm_core::audit_emit!($1, vm: $2, "{$3}");}gxs;

    # --- detail only (no vm) where detail is an ident or field ---
    s{
      mvm_core::audit::emit\(
        \s* mvm_core::audit::LocalAuditKind::(\w+) \s*,
        \s* None \s*,
        \s* Some\(\s*&?\s*([\w\.]+)\s*\) \s*,?
        \s*
      \);?
    }{mvm_core::audit_emit!($1, "{$2}");}gxs;

    # --- vm only (broader): emit(KIND, Some(&?ident_or_field), None) ---
    s{
      mvm_core::audit::emit\(
        \s* mvm_core::audit::LocalAuditKind::(\w+) \s*,
        \s* Some\(\s*&?\s*([\w\.]+)\s*\) \s*,
        \s* None \s*,?
        \s*
      \);?
    }{mvm_core::audit_emit!($1, vm: $2);}gxs;
  ' "$f"
done

echo "migrate-audit-emit: done. Run xtask check-audit-positional to find anything left."
