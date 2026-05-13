"""mvm — Python SDK for the mvm workload toolchain.

Plan 60 Phase 5 — only the lower-layer IR types are wired today
(generated from `schema/workload-ir-v0.json` via `cargo xtask
gen-stubs`). The decorator surface (`@mvm.func`, `@mvm.app`) and the
host-side invoke transport (`_remote.py`, `_subprocess.py`) land in a
follow-on slice.
"""

from mvm._ir import Workload

__all__ = ["Workload"]
