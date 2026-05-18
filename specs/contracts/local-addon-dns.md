# Contract: local addon DNS resolver and TCP-vsock bridge

## Status

**Proposed.** This is an mvm-local developer experience primitive, not
a distributed service mesh.

The in-guest binaries make `db.addon.local:5432`-style addressing work
for a microVM that declares local development addons such as databases,
KV stores, queues, or object-store emulators:

- **`mvm-addon-dns`** resolves configured `*.addon.local` names.
- **`mvm-addon-vsock-bridge`** fronts configured loopback addresses and
  forwards accepted TCP connections to a local host addon proxy over
  vsock.

The goal is ergonomic local addon access inside one developer microVM.
Tenant-aware service discovery, distributed routing, iroh/QUIC,
capability tokens, endpoint identities, and inter-VM policy belong in
`mvmd`.

## Boundary

`mvm` owns the substrate needed inside one locally running microVM:

- loopback-only DNS for declared local addons
- loopback TCP listeners inside the guest
- guest-to-host forwarding over vsock
- config-disk parsing for local addon records

`mvmd` owns the distributed mesh:

- tenant and workspace policy
- cross-VM or cross-host service discovery
- iroh/QUIC transport
- capability tokens and endpoint identity
- host fleet orchestration

The crates in this contract must not grow distributed control-plane
behavior. If a field or code path needs tenant identity, remote endpoint
identity, cryptographic routing, or service authorization, it belongs in
`mvmd`.

## Data Path

```text
consumer app connects to db.addon.local:5432
  -> mvm-addon-dns resolves db.addon.local to a configured loopback IP
  -> mvm-addon-vsock-bridge accepts on that loopback IP and port
  -> bridge opens a vsock stream to the host addon proxy
  -> host addon proxy connects to the local addon process
```

No TAP is required for this path. The guest only emits local TCP traffic
to loopback plus vsock traffic to the host.

## Crate: `mvm-addon-dns`

Thin wrapper over `hickory-dns`.

- **Listen scope:** `127.0.0.1:53` and `::1:53` only. Never listen on a
  public interface or TAP.
- **Authority scope:** authoritative for `*.addon.local` only. Forward
  all other queries to the guest's upstream resolver chain.
- **Zone source:** load records from the config disk's
  `addon_dns_zone` array.
- **Reload model:** SIGHUP reloads the zone without dropping in-flight
  queries.
- **No-op mode:** an absent or empty zone file means the binary idles
  under supervision and opens no DNS service beyond what is explicitly
  wired.

v1 zone shape:

```json
[
  {"hostname": "db.addon.local", "address": "127.0.255.1"},
  {"hostname": "cache.addon.local", "address": "127.0.255.2"}
]
```

## Crate: `mvm-addon-vsock-bridge`

Per-connection TCP-vsock bridge for local addons.

- **Loopback bindings:** load `addon_loopback_bindings` from the config
  disk.
- **Listen behavior:** bind explicit loopback listeners for declared
  addon endpoints.
- **Per-connection header:** write a length-prefixed JSON header to the
  host addon proxy before any application bytes.
- **Proxy behavior:** after the header, proxy bytes both ways and
  preserve half-close behavior.
- **No secrets:** the guest-side bridge does not receive, store, log, or
  forward credentials, capability tokens, endpoint private keys, or
  tenant policy material.

v1 binding shape:

```json
[
  {"peer": "db", "loopback_ip": "127.0.255.1", "vsock_port": 5253},
  {"peer": "cache", "loopback_ip": "127.0.255.2", "vsock_port": 5254}
]
```

v1 peer header:

```json
{
  "version": 1,
  "peer": "db"
}
```

The header is encoded as `4-byte big-endian length || UTF-8 JSON`.

## Config-Disk Schema

```json
{
  "addon_dns_zone": [],
  "addon_loopback_bindings": []
}
```

Both fields are optional. Missing or empty fields put the corresponding
binary into no-op mode.

## Validation

- Boot a VM with no declared local addons: both binaries are no-ops and
  existing networking is unchanged.
- Boot a VM with a `db` addon: `dig db.addon.local @127.0.0.1` returns
  the configured loopback IP.
- Connecting to the configured loopback IP and port opens a vsock stream
  to the host addon proxy with the expected v1 peer header.
- Malformed zone and binding JSON fails closed with a parse error.
- Oversized or truncated peer headers are rejected.
- The crates do not depend on iroh or any distributed mesh runtime.
