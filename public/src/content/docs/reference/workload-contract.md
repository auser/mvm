---
title: Workload Contract
description: What a workload binary must look like to be runnable as a production mvm guest entrypoint, and what it can expect at run time.
---

A production mvm guest exposes exactly **one** runnable workload: the
binary named by `/etc/mvm/entrypoint`. The host invokes it via the
`RunEntrypoint` vsock request; the guest agent validates it once at boot,
holds a file descriptor to it for the agent's lifetime, and refuses every
`RunEntrypoint` if validation ever failed.

This page is the contract: where the binary must live, what it must look
like on disk, what it can read and write at run time, and how the agent
reports what happened.

> This is the **production** path. For dev-mode arbitrary execution (the
> `dev-shell` Cargo feature, used by the dev-mode `exec` command), see
> [Sandboxed Exec](/guides/exec/). The dev path's handler is
> physically absent from production guest binaries (W4.3).

## Where the binary lives

| Requirement      | Value                                                                |
|------------------|----------------------------------------------------------------------|
| Marker file      | `/etc/mvm/entrypoint` -- text file, absolute path, trailing newline ok |
| Resolved prefix  | Must canonicalize under `/usr/lib/mvm/wrappers/`                     |
| Filesystem       | Must be on the same filesystem as `/usr` (the verity-sealed rootfs)  |
| File type        | Regular file (no symlink to elsewhere, no directory)                  |
| Ownership        | `uid=0 gid=0`                                                         |
| Mode             | Exactly `0755`                                                        |
| setuid / setgid  | Refused (`04000` or `02000` bits set)                                 |

The marker's contents are read once, trimmed, and `realpath`-resolved.
A relative path, a missing file, a path that resolves outside the allowed
prefix, or a path on a different filesystem all fail closed --
`RunEntrypoint` returns `EntrypointInvalid` for the lifetime of the
agent. Restart the guest to retry validation.

The "same filesystem as `/usr`" check is what binds the workload to the
verity-sealed rootfs. A wrapper materialized into a writable mount (e.g.
an `--add-dir` overlay) lives on a different `dev_t` and is rejected.

## Per-call lifecycle

When the host issues `RunEntrypoint`, the agent spawns the validated
wrapper with:

| Channel | Direction | Behavior                                                          |
|---------|-----------|-------------------------------------------------------------------|
| stdin   | host -> guest | Request payload bytes piped in, then closed                   |
| stdout  | guest -> host | Captured, surfaced in the response                            |
| stderr  | guest -> host | Captured, surfaced in the response                            |
| fd 3    | guest -> host | Framed control records (see below)                            |
| env     | --        | Cleared; no host env leaks in                                     |
| cwd     | --        | Set by the caller in the request                                  |
| pgid    | --        | Wrapper is its own process-group leader; signals reach all descendants |
| core    | --        | `RLIMIT_CORE=0` is inherited; a crash does not write a core file  |

The wrapper is invoked through `/proc/self/fd/<n>` on Linux, where `<n>`
is the validation fd held open by the agent. That defeats TOCTOU between
validation and spawn -- replacing the file on disk after boot does not
change which inode runs.

## Caps (v1)

| Cap              | Value     | On breach                                                         |
|------------------|-----------|-------------------------------------------------------------------|
| stdin payload    | 1 MiB     | Wrapper is **not spawned**; outcome is `PayloadCap(Stdin)`        |
| stdout captured  | 1 MiB     | Wrapper killed (SIGTERM, then SIGKILL after 2 s); `PayloadCap(Stdout)` |
| stderr captured  | 1 MiB     | Wrapper killed; `PayloadCap(Stderr)`                              |
| fd 3 captured    | 1 MiB     | Further records silently dropped; wrapper not killed              |
| Per-frame header | 64 KiB    | Channel marked corrupt; no further records read; wrapper not killed |
| Wall-clock timeout | caller-set | Wrapper killed; outcome is `Timeout`                            |

Caps are an upper bound -- the wrapper sees its writes succeed up to the
cap and then either gets killed (stdout/stderr) or has its later writes
dropped (fd 3). fd 3 is for structured records the host correlates by
kind, so dropping records on overflow is preferable to killing the
wrapper.

## Outcomes

A `RunEntrypoint` call's response is a **stream** of `EntrypointEvent`
frames sent back over vsock, terminated by either `Exit { code }` or
`Error { kind, message }`:

| Event     | Direction     | Meaning                                                       |
|-----------|---------------|---------------------------------------------------------------|
| `Stdout`  | guest -> host | Bytes from the wrapper's stdout. v1 sends one buffered chunk. |
| `Stderr`  | guest -> host | Bytes from the wrapper's stderr. v1 sends one buffered chunk. |
| `Control` | guest -> host | One framed fd-3 record (see [fd 3 control records](#fd-3-control-records)). |
| `Exit`    | terminal      | Wrapper exited normally with the given code.                  |
| `Error`   | terminal      | Agent-side condition prevented or interrupted the call.       |

When `Error` is the terminal frame, its `kind` is one of seven
`RunEntrypointError` variants:

| Variant              | Trigger                                                              |
|----------------------|----------------------------------------------------------------------|
| `PayloadCap`         | stdin / stdout / stderr exceeded the per-stream cap                  |
| `Timeout`            | wall-clock deadline exceeded; agent killed the process group         |
| `Busy`               | another `RunEntrypoint` was already in flight on this VM (M12: agents serialize per VM; concurrency comes from pool growth) |
| `WrapperCrashed`     | wrapper exited via signal (segfault, OOM kill, etc.)                 |
| `EntrypointInvalid`  | boot-time validation of `/etc/mvm/entrypoint` failed; reported per call for the agent's lifetime |
| `SessionKilled`      | the session backing the call was killed mid-flight (`mvmctl session kill`); synthesized host-side from a transport drop coincident with a `Killed` session record |
| `InternalError`      | agent-internal failure (file I/O, vsock framing, IPC plumbing); the human-readable detail is in `message` |

Streams preceding the terminal frame are still delivered -- a partial
stdout buffer from a timed-out or capped wrapper still reaches the
host.

> The agent itself never emits `SessionKilled` -- by the time the
> session is gone, the VM is torn down with it. The host synthesizes
> the error after detecting the transport drop.

## fd 3 control records

fd 3 is a one-way pipe from wrapper to host for **structured** records
that the host correlates by `kind`. It is independent of stderr -- a
wrapper writing to fd 3 does not pollute the user-visible stderr stream.

Frame layout (little-endian):

```text
header_len:  u32   (4 bytes; <= 64 KiB)
header_json: bytes (header_len bytes; UTF-8 JSON)
payload_len: u32   (4 bytes)
payload:     bytes (payload_len bytes; opaque)
```

A wrapper writing one record, then exiting:

```sh
#!/bin/sh
# header `{"kind":"ok"}` (13 bytes), empty payload
printf '\015\0\0\0' >&3
printf '{"kind":"ok"}' >&3
printf '\0\0\0\0' >&3
exit 0
```

Rules the wrapper should expect:

- **Partial frames at EOF are dropped silently.** If you flush a header
  prefix and exit before writing the payload, that frame is gone --
  the host receives no record for it. Don't rely on partial frames.
- **An oversized header (>64 KiB) corrupts the channel.** The drain
  stops after that frame; later valid frames are dropped.
- **Records are streamed, not transactional.** The host sees records in
  the order written and is allowed to act on them while the wrapper is
  still running.
- **fd 3 is best-effort.** A breached fd 3 cap drops records but does
  not kill the wrapper -- a wrapper that depends on every record
  arriving is mis-using the channel.

## Session-level VM reuse

A session (`mvmctl session start`) keeps a guest VM alive across many
`RunEntrypoint` calls. The contract documented above is **per call**,
not per VM:

- The wrapper is spawned **fresh** for every `RunEntrypoint`. No
  in-process state survives between calls -- environment, working
  directory, open files, and the process tree are all re-established.
- Only **one** `RunEntrypoint` may be in flight on a given VM at a
  time. A second concurrent request returns `Busy`.
- Concurrency at the workload layer comes from **growing the VM pool**
  (each VM runs its own wrappers serially), not from making the
  wrapper reentrant.
- A session ending (idle timeout, explicit `session kill`, or VM
  crash) surfaces to any in-flight call as `SessionKilled`.

A wrapper that tolerates being spawned and exited many times within
the same VM works correctly under sessions. A wrapper that assumes
long-lived in-process state across calls does not -- persist that
state to disk (the rootfs is read-only; use a writable overlay if you
need cross-call persistence) or accept that it is per-call.

## How this differs from the dev-mode `exec` path

The production `RunEntrypoint` contract and `mvmctl exec` solve different
problems and live on different code paths:

| Feature                  | Production `RunEntrypoint`     | `mvmctl exec` (dev-shell)               |
|--------------------------|--------------------------------|------------------------------------------|
| Compiled into prod build | Yes                            | No (Cargo feature gated; absent in prod) |
| Allowed binary           | Single, validated at boot      | Any binary the caller names              |
| Validation               | uid/gid/mode/prefix/fs checks  | None beyond standard execve semantics    |
| Use case                 | Long-lived workload            | One-shot dev / agent task                |

If you're shipping a production guest, `RunEntrypoint` is the contract.
If you're building locally and want to throw arbitrary commands into a
fresh VM, [Sandboxed Exec](/guides/exec/) is the workflow.

## Authoring a wrapper: checklist

- [ ] Binary lives at `/usr/lib/mvm/wrappers/<name>` in the rootfs flake.
- [ ] Built into the verity-sealed rootfs, not into a writable overlay.
- [ ] `chown 0:0` and `chmod 0755`. No setuid / setgid bits.
- [ ] `/etc/mvm/entrypoint` contains the absolute path to it, plus a
      trailing newline.
- [ ] Reads request input from stdin and emits results on stdout.
- [ ] Reserves stderr for human-readable diagnostics.
- [ ] Uses fd 3 only for structured control records; tolerates dropped
      records on overflow.
- [ ] Exits non-zero on failure; relies on the host to surface the
      classification (`Timeout`, `PayloadCap`, `Busy`,
      `WrapperCrashed`, `SessionKilled`).
- [ ] Tested under the v1 caps (1 MiB / stream, caller-set timeout).

## See also

- [Guest Agent](/reference/guest-agent/) -- the agent process that
  hosts `RunEntrypoint` and the rest of the vsock protocol.
- [Sandboxed Exec](/guides/exec/) -- the dev-mode arbitrary-command
  path; not the production contract.
- [Matryoshka Model](/security/matryoshka/) -- the five trust layers
  and the seven CI-enforced claims, including the verified-rootfs and
  no-`do_exec`-in-production constraints this contract leans on.
