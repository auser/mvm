"""Plan 120 Task 5 — the one-call `Sandbox.exec(...)` DX headline.

`exec` is the dev-tier one-shot: run argv in the sandbox, collect
stdout/stderr/exit. It refuses prod templates via `SandboxDevOnly`
*before* any vsock traffic (ADR-002 §W4.3, security claim 4) — there
is no prod exec path.
"""

from __future__ import annotations

import os

import pytest

import mvm
import mvm._sandbox as s


@pytest.fixture(autouse=True)
def _isolate():
    """Clean recording + mode env around each test."""
    mvm.reset_recording()
    os.environ.pop("MVM_SDK_MODE", None)
    os.environ.pop("MVM_CLI_BIN", None)
    yield
    mvm.reset_recording()
    os.environ.pop("MVM_SDK_MODE", None)
    os.environ.pop("MVM_CLI_BIN", None)


# ── always-on: claim 4 — exec never reaches a prod container ──────────


def test_exec_refuses_prod_template_before_any_subprocess(monkeypatch) -> None:
    """A prod-mode transport must make `exec` raise `SandboxDevOnly`
    *before* spawning any `mvmctl` subprocess (security claim 4)."""

    def _fail(*_a, **_k):
        pytest.fail("exec() spawned a subprocess on a prod template")

    monkeypatch.setattr(s.subprocess, "run", _fail)

    sb = s.Sandbox(
        "wid",
        live=s._LiveTransport(
            mvm_cli_bin="/bin/false", vm_id="sb-prod", build_mode="prod"
        ),
    )
    with pytest.raises(mvm.SandboxDevOnly, match="dev-mode template"):
        sb.exec("python", "-c", "print(1)")


def test_exec_in_record_mode_raises_not_a_recordable_op() -> None:
    """`exec` is a live one-shot; in record mode (no `_live`) it refuses
    rather than silently recording nothing."""
    sb = mvm.Sandbox.create(image="python:slim")  # record mode (default)
    with pytest.raises(mvm.SandboxModeError, match="live-transport one-shot"):
        sb.exec("python", "-c", "print(1)")


def test_create_accepts_image_alias_and_rejects_both_or_neither() -> None:
    """`image=` is an alias for the positional `template`; exactly one is
    required (the quickstart reads `Sandbox.create(image=...)`)."""
    sb = mvm.Sandbox.create(image="python:slim")
    assert sb.workload_id == "python:slim"
    mvm.reset_recording()
    with pytest.raises(ValueError, match="exactly one"):
        mvm.Sandbox.create("a", image="b")
    with pytest.raises(ValueError, match="exactly one"):
        mvm.Sandbox.create()


# ── gated: the real one-shot against a booted dev-tier sandbox ────────


@pytest.mark.skipif(
    os.environ.get("MVM_E2E_SMOKE") != "1",
    reason="boots a real microVM; set MVM_E2E_SMOKE=1 on a libkrun host",
)
def test_sandbox_exec_returns_stdout() -> None:
    # Live, dev-tier. The host's `mvmctl run --dev` normally sets these;
    # set them explicitly for a standalone gated run.
    os.environ["MVM_SDK_MODE"] = "live"
    os.environ.setdefault("MVM_CLI_BIN", os.environ.get("MVM_CLI_BIN", "mvmctl"))

    sb = mvm.Sandbox.create(image="python:slim")  # boots a dev-tier microVM
    try:
        r = sb.exec("python", "-c", "print(2+2)")
        assert r.exit_code == 0
        assert r.stdout.strip() == "4"
    finally:
        sb.shutdown()
