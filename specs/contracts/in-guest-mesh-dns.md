# Contract: in-guest mesh DNS resolver and TCP↔vsock bridge

## Status

**Proposed — paired contract for mvmforge ADR-0018 / ADR-0020** (drafted 2026-05-07).

The two new in-guest binaries that make `db.mesh.local:5432`-style
addressing work for consumers without any iroh code in the guest:
**`mvm-mesh-dns`** (hickory-dns wrapper resolving `*.mesh.local`)
and **`mvm-mesh-vsock-bridge`** (TCP↔vsock bridge that fronts the
loopback IPs the resolver hands out and forwards to mvmd-agent on
the host).

Both are iroh-free, supervised by the existing minimal-init respawn
loop alongside `mvm-guest-agent`. Their only inputs come from the
config disk (per
[`mvmd/specs/contracts/addon-runtime.md`](https://github.com/tinylabs/mvmd/blob/main/specs/contracts/addon-runtime.md)).

## Audience

mvm maintainers building `crates/mvm-mesh-dns/` and
`crates/mvm-mesh-vsock-bridge/`. mvmd maintainers populating the
config-disk fields these binaries consume.

## Background

ADR-0018 §Cross-host scope v1 commits to **vsock-only host↔guest
data path** — the existing mvm-guest-agent vsock-narrow-surface
contract is preserved. Adding TAP interfaces would expose the host
kernel's network stack to guest-controlled L2/L3 packets, widening
the attack surface meaningfully. Instead the mesh data path runs:

```
consumer app TCP connect to db.mesh.local:5432
  → mvm-mesh-dns (in-guest, 127.0.0.1:53) resolves to 10.255.0.1
  → mvm-mesh-vsock-bridge (in-guest) accepts on 10.255.0.1:5432
  → opens vsock stream to mvmd-agent (host) with peer header
  → mvmd-agent dials iroh-QUIC to addon's host's mvmd-agent
  → remote mvmd-agent forwards via vsock PortForward to addon guest
```

No iroh in mvm. No TAP. Pure Linux networking + vsock between guest
and host.

## Contract

### Crate: `mvm-mesh-dns`

Thin wrapper over `hickory-dns` (formerly `trust-dns`; mature,
audited, used by Cloudflare's Pingora). Behavior:

- **Listen scope**: `127.0.0.1:53` and `::1:53` only. Never on a
  public interface, never on a TAP. Not a recursive resolver for
  untrusted clients.
- **Authority scope**: authoritative for `*.mesh.local` only;
  forwards everything else (`*.com`, IP literals, etc.) to the
  upstream resolver chain inherited from the guest's existing
  networking (typically `8.8.8.8` or whatever the substrate
  configures).
- **Zone source**: per-instance zone records loaded at boot from
  the config disk's `mesh_dns_zone` array (see
  [addon-runtime.md](https://github.com/tinylabs/mvmd/blob/main/specs/contracts/addon-runtime.md)).
  v1 emits A records only:

  ```jsonc
  [
    {"hostname": "db.mesh.local", "address": "10.255.0.1"},
    {"hostname": "cache.mesh.local", "address": "10.255.0.2"}
  ]
  ```

  v2-reserved fields (SRV records, TXT-record cert hints, TTL
  overrides, hot-rotation deltas) are accepted but unused at v1.
- **Reload model**: SIGHUP-on-config-disk-change. mvmd signals
  via the existing guest-agent vsock RPC when the zone updates
  (e.g. on credential rotation that re-derives a hostname, or on
  addon respawn at a new endpoint).
- **Supervisor**: runs as an unprivileged user (e.g. `_mvm-dns`),
  supervised by the existing minimal-init respawn loop.
- **Implementation crate**: `crates/mvm-mesh-dns/` — thin wrapper
  over hickory-dns, config-disk loader, SIGHUP handler. Target
  size: ~200 LOC + tests. iroh-free.

### Crate: `mvm-mesh-vsock-bridge`

Per-connection TCP↔vsock bridge. Behavior:

- **Loopback bindings**: at boot, reads `mesh_loopback_bindings`
  from the config disk:

  ```jsonc
  [
    {"peer": "db.mesh.local", "loopback_ip": "10.255.0.1", "vsock_port": 5253},
    {"peer": "cache.mesh.local", "loopback_ip": "10.255.0.2", "vsock_port": 5254}
  ]
  ```

  For each entry, binds a TCP listener on
  `<loopback_ip>:<original_port>` (the original port comes from the
  consumer's connection — see "Listen behavior" below). The
  loopback IPs are aliases on `lo` (no TAP, no host-visible
  interface).
- **Listen behavior**: the bridge intercepts TCP connections to any
  address in the configured loopback range. v1 binds an explicit
  listener per `(loopback_ip, port)` discovered from the consumer's
  declared addon ports; future versions may install a transparent-
  proxy iptables redirect to capture arbitrary ports.
- **Per-connection peer header**: on accept, opens a fresh vsock
  stream to mvmd-agent's per-instance vsock listener (port from the
  config disk's `mesh_loopback_bindings[].vsock_port`) and writes
  the header **before** any application bytes:

  ```jsonc
  {
    "version": 1,
    "peer": "db",                  // bare alias / name (no .mesh.local suffix)
    "consumer_endpoint_id": "..."  // public Ed25519 (hex), supplied by config disk
  }
  ```

  Header is a length-prefixed JSON blob (4-byte big-endian length +
  UTF-8 JSON; matches the existing mvm-core wire conventions).
- **Bidirectional proxying**: after the header is acknowledged
  (mvmd-agent sends a 1-byte ACK), the bridge proxies bytes both
  ways until either side closes. Half-close semantics preserved.
- **Capability tokens**: NEVER on the guest side. The bridge does
  not see, store, or transmit capability tokens. mvmd-agent on the
  host attaches the token to the iroh handshake transparently.
- **Implementation crate**: `crates/mvm-mesh-vsock-bridge/` — small
  Rust binary over libc vsock + tokio. Target size: ~100 LOC + tests.
  iroh-free.

### Config-disk schema (consumed)

The config disk JSON (mounted on tmpfs in the consumer's namespace
only) carries the fields above plus
`crates/mvm-core/src/config.rs`'s formal schema:

```jsonc
{
  "mesh_dns_zone": [...],            // mvm-mesh-dns reads this
  "mesh_loopback_bindings": [...]    // mvm-mesh-vsock-bridge reads this
}
```

When either field is empty/absent, the corresponding binary becomes
a no-op (idle, supervised, no listeners). This keeps the `mkGuest`
contract simple: bake both binaries unconditionally; emit/skip the
config-disk fields based on whether the workload declares addons.

### Init / startup ordering

Existing minimal-init's service-respawn loop launches both
binaries in parallel after `04-etc-and-users.sh` (which configures
`/etc/resolv.conf` to list `127.0.0.1` first when the mesh is
enabled). No ordering constraint between `mvm-mesh-dns` and
`mvm-mesh-vsock-bridge`; the consumer's app code runs after both
are listening (init waits on a small readiness check before
launching the workload's entrypoint).

### What this contract does NOT cover

- **iroh, QUIC, Ed25519 endpoint identities, capability tokens** —
  all on the host side. See
  [`mvmd/specs/contracts/workload-mesh.md`](https://github.com/tinylabs/mvmd/blob/main/specs/contracts/workload-mesh.md).
- **Inter-tenant authorization** — enforced at the coordinator
  + mvmd-agent boundary, not by the in-guest binaries.
- **Persistence** — the config disk is tmpfs; the in-guest
  resolver and bridge hold no state across reboots.

## Validation

- Boot a consumer with no `addons[]` declared: both binaries start
  but are no-ops; the existing networking is unchanged.
- Boot a consumer with one addon: `dig db.mesh.local @127.0.0.1`
  returns `10.255.0.1`; `dig example.com @127.0.0.1` forwards to
  upstream; `nc 10.255.0.1 5432` opens a connection (terminated by
  mvmd-agent on the host with the per-connection peer header).
- SIGHUP after config-disk update reloads the zone without dropping
  in-flight DNS queries.
- Host kernel network stack receives no guest-sourced packets
  (verified by host-side `tcpdump` showing only vsock + iroh-QUIC
  traffic on the relevant interfaces).
- The two binaries link no iroh-QUIC code (`cargo tree -p mvm-mesh-dns`
  and `-p mvm-mesh-vsock-bridge` must NOT include `iroh*` crates).
- Bridge handles half-close cleanly: `nc 10.255.0.1 5432` followed
  by Ctrl-D (consumer-side close) leaves the addon-side write side
  open until the addon closes.

## Related

- [`mvmd/specs/contracts/addon-runtime.md`](https://github.com/tinylabs/mvmd/blob/main/specs/contracts/addon-runtime.md)
  — the config-disk schema this contract consumes.
- [`mvmd/specs/contracts/workload-mesh.md`](https://github.com/tinylabs/mvmd/blob/main/specs/contracts/workload-mesh.md)
  — the iroh-QUIC + capability-token data plane that picks up
  where the vsock-bridge hands off.
- [`mvmforge/specs/adrs/0018-composable-addons.md`](https://github.com/tinylabs/mvmforge/blob/main/specs/adrs/0018-composable-addons.md)
  — the user-facing decision this stack implements.
