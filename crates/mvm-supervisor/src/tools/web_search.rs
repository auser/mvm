//! `mvm.web_search` — provider-fronted web search through a
//! per-tenant provider allowlist.
//!
//! Plan 60 Phase 7. The agent passes a query string + (optionally) a
//! preferred provider; the supervisor:
//!
//! 1. Validates the query — non-empty, length-capped, no embedded
//!    control characters that would confuse upstream APIs.
//! 2. Resolves the provider — caller-supplied name (`"brave"`,
//!    `"google"`, …) or the tool's `default_provider` if absent.
//! 3. Checks the provider against the tool's allowlist. An empty
//!    allowlist means *no provider is reachable* (fail-closed
//!    default; operators must opt in per provider).
//! 4. Delegates to an injected [`SearchProvider`] so the
//!    policy/validation layers are testable without an upstream
//!    HTTP client. The default provider is [`NoopSearchProvider`],
//!    which always returns [`SearchError::Unwired`]; real Brave /
//!    Google / DuckDuckGo impls land in follow-up slices.
//!
//! ## Why provider-name allowlists instead of host allowlists
//!
//! `mvm.web_fetch` constrains the *destination host* (the agent
//! decides where to look). `mvm.web_search` constrains the
//! *provider service* — the agent has no say over which upstream
//! the provider impl ultimately calls. Pinning by provider name is
//! the granularity that matches the security model: an operator
//! grants "Brave can search; not Google, not Bing." A future slice
//! may add per-provider host pinning so a wedged DNS doesn't let a
//! provider impl reach the wrong endpoint.
//!
//! ## What this tool is NOT
//!
//! - Not a fetch tool — see `mvm.web_fetch`.
//! - Not a "let the agent pick any API key" surface — provider
//!   credentials are supervisor-owned. The agent doesn't see them.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{HostMediatedTool, ToolInvokeError};

pub const TOOL_NAME: &str = "mvm.web_search";

/// Default `max_results` when the caller doesn't specify. Bounded
/// at the upper end by [`MAX_ALLOWED_RESULTS`].
pub const DEFAULT_MAX_RESULTS: u32 = 10;

/// Hard upper bound on `max_results`. Caller-supplied values above
/// this are clamped (not errored) so older clients still make
/// progress.
pub const MAX_ALLOWED_RESULTS: u32 = 50;

/// Query-length cap. Defense in depth: most search APIs reject
/// queries longer than ~512 chars anyway, but pre-rejecting here
/// keeps the audit chain from logging giant strings.
pub const MAX_QUERY_LEN: usize = 1024;

/// Pluggable upstream search adapter. Production impls (`BraveProvider`,
/// `GoogleProvider`, …) wrap their respective HTTP clients;
/// [`NoopSearchProvider`] is the substrate default and returns
/// [`SearchError::Unwired`] so the tool ships fail-closed.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Provider name as exposed to the agent. Must be lowercase,
    /// stable, and unique within a [`WebSearchTool`]'s allowlist.
    fn name(&self) -> &'static str;

    /// Run a search. `max_results` is the post-clamp upper bound;
    /// the impl is free to return fewer.
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>, SearchError>;
}

/// One result row in the response array.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("upstream returned no results")]
    Empty,
    #[error("upstream API error: {0}")]
    Upstream(String),
    #[error("upstream rate-limited")]
    RateLimited,
    #[error("provider not wired (NoopSearchProvider)")]
    Unwired,
}

/// Default fail-closed provider — refuses every call with
/// [`SearchError::Unwired`].
pub struct NoopSearchProvider;

#[async_trait]
impl SearchProvider for NoopSearchProvider {
    fn name(&self) -> &'static str {
        "noop"
    }
    async fn search(&self, _query: &str, _max: u32) -> Result<Vec<SearchHit>, SearchError> {
        Err(SearchError::Unwired)
    }
}

/// One agent-callable search surface, scoped to an allowlist of
/// provider names.
pub struct WebSearchTool {
    /// Provider names the tool is permitted to call. Empty =
    /// nothing reachable (fail-closed).
    allowed_providers: BTreeSet<String>,
    /// Provider used when the agent doesn't specify one. Must
    /// appear in `allowed_providers` to be reachable; if it
    /// doesn't, the search fails closed with a clear message.
    default_provider: String,
    /// Concrete provider impls keyed by `provider.name()`. Built
    /// via [`Self::register_provider`].
    providers: std::collections::BTreeMap<String, Arc<dyn SearchProvider>>,
}

impl WebSearchTool {
    /// Build with an empty allowlist + no providers wired. Every
    /// invoke returns `Upstream` with a clear "no providers
    /// configured" message. Useful as the default until per-tenant
    /// config lands.
    pub fn fail_closed() -> Self {
        Self {
            allowed_providers: BTreeSet::new(),
            default_provider: "noop".to_string(),
            providers: std::collections::BTreeMap::new(),
        }
    }

    /// Build with an explicit provider allowlist + default. The
    /// default is allowed to be a name that isn't registered yet;
    /// a search with the unregistered default fails closed when
    /// invoked (so config drift is loud, not silent).
    pub fn with_allowlist(
        allowed: impl IntoIterator<Item = String>,
        default_provider: String,
    ) -> Self {
        Self {
            allowed_providers: allowed.into_iter().collect(),
            default_provider,
            providers: std::collections::BTreeMap::new(),
        }
    }

    /// Plug a concrete provider impl. Replaces any previous entry
    /// with the same `name()` — useful in tests for swapping a
    /// stub. The provider's name is NOT automatically added to the
    /// allowlist; operators control that surface separately so
    /// "registered" and "permitted" stay distinct concepts.
    pub fn register_provider(mut self, provider: Arc<dyn SearchProvider>) -> Self {
        self.providers.insert(provider.name().to_string(), provider);
        self
    }

    /// Read-only view of the allowlist for diagnostic surfaces.
    pub fn allowlist(&self) -> Vec<&str> {
        self.allowed_providers.iter().map(String::as_str).collect()
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::fail_closed()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchParams {
    pub query: String,
    /// Optional provider override. When absent, the tool's
    /// `default_provider` is used. Must appear in the allowlist.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default = "default_max_results")]
    pub max_results: u32,
}

fn default_max_results() -> u32 {
    DEFAULT_MAX_RESULTS
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSearchResult {
    pub provider: String,
    pub query: String,
    pub hits: Vec<SearchHit>,
}

#[async_trait]
impl HostMediatedTool for WebSearchTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    async fn invoke(
        &self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolInvokeError> {
        let parsed: WebSearchParams =
            serde_json::from_value(params).map_err(|e| ToolInvokeError::InvalidParams {
                tool: TOOL_NAME.to_string(),
                message: e.to_string(),
            })?;

        let query = parsed.query.trim();
        if query.is_empty() {
            return Err(ToolInvokeError::InvalidParams {
                tool: TOOL_NAME.to_string(),
                message: "query must be non-empty".to_string(),
            });
        }
        if query.chars().any(|c| c.is_control()) {
            return Err(ToolInvokeError::InvalidParams {
                tool: TOOL_NAME.to_string(),
                message: "query contains control characters".to_string(),
            });
        }
        if query.len() > MAX_QUERY_LEN {
            return Err(ToolInvokeError::InvalidParams {
                tool: TOOL_NAME.to_string(),
                message: format!("query length {} exceeds max {MAX_QUERY_LEN}", query.len()),
            });
        }

        let provider_name = parsed.provider.as_deref().unwrap_or(&self.default_provider);

        if !self.allowed_providers.contains(provider_name) {
            return Err(ToolInvokeError::Upstream {
                tool: TOOL_NAME.to_string(),
                message: format!(
                    "provider {provider_name:?} not on per-tenant allowlist (allowed: {allowed:?})",
                    allowed = self.allowlist()
                ),
            });
        }

        let provider =
            self.providers
                .get(provider_name)
                .ok_or_else(|| ToolInvokeError::Upstream {
                    tool: TOOL_NAME.to_string(),
                    message: format!(
                        "provider {provider_name:?} is allowed but no impl is registered \
                     (config drift between allowlist and provider table)"
                    ),
                })?;

        let max = parsed.max_results.min(MAX_ALLOWED_RESULTS);
        let hits = provider
            .search(query, max)
            .await
            .map_err(|e| ToolInvokeError::Upstream {
                tool: TOOL_NAME.to_string(),
                message: e.to_string(),
            })?;

        serde_json::to_value(WebSearchResult {
            provider: provider_name.to_string(),
            query: query.to_string(),
            hits,
        })
        .map_err(|e| ToolInvokeError::Upstream {
            tool: TOOL_NAME.to_string(),
            message: format!("serializing result: {e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubProvider {
        name: &'static str,
        calls: std::sync::Mutex<Vec<(String, u32)>>,
        response: Vec<SearchHit>,
    }

    impl StubProvider {
        fn new(name: &'static str, response: Vec<SearchHit>) -> Self {
            Self {
                name,
                calls: std::sync::Mutex::new(Vec::new()),
                response,
            }
        }
        fn calls(&self) -> Vec<(String, u32)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SearchProvider for StubProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn search(&self, query: &str, max: u32) -> Result<Vec<SearchHit>, SearchError> {
            self.calls.lock().unwrap().push((query.to_string(), max));
            Ok(self.response.clone())
        }
    }

    fn hits_sample() -> Vec<SearchHit> {
        vec![
            SearchHit {
                title: "Rust".into(),
                url: "https://www.rust-lang.org".into(),
                snippet: "systems language".into(),
            },
            SearchHit {
                title: "Cargo".into(),
                url: "https://crates.io".into(),
                snippet: "package registry".into(),
            },
        ]
    }

    fn tool_with_stub(
        allow: &[&str],
        default: &str,
        provider_name: &'static str,
    ) -> (WebSearchTool, Arc<StubProvider>) {
        let stub = Arc::new(StubProvider::new(provider_name, hits_sample()));
        let tool =
            WebSearchTool::with_allowlist(allow.iter().map(|s| s.to_string()), default.to_string())
                .register_provider(stub.clone());
        (tool, stub)
    }

    // ──────────────────────────────────────────────────────────────
    // Policy: provider allowlist
    // ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn allowlist_blocks_unconfigured_provider() {
        // Exit-test target per plan 60 Phase 7:
        // `mvm_supervisor::tools::web_search::tests::allowlist_blocks_unconfigured_provider`.
        let tool = WebSearchTool::with_allowlist(["brave".to_string()], "brave".to_string());
        let err = tool
            .invoke(serde_json::json!({
                "query": "rust async",
                "provider": "google"
            }))
            .await
            .unwrap_err();
        match err {
            ToolInvokeError::Upstream { tool, message } => {
                assert_eq!(tool, TOOL_NAME);
                assert!(message.contains("google"), "{message}");
                assert!(message.contains("not on per-tenant allowlist"), "{message}");
                // The allowed providers show up so an operator can
                // see "ah, only brave is on".
                assert!(message.contains("brave"), "{message}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_allowlist_denies_every_provider() {
        let tool = WebSearchTool::fail_closed();
        let err = tool
            .invoke(serde_json::json!({ "query": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolInvokeError::Upstream { .. }));
    }

    #[tokio::test]
    async fn allowlisted_provider_falls_through_to_impl() {
        let (tool, stub) = tool_with_stub(&["brave"], "brave", "brave");
        let out = tool
            .invoke(serde_json::json!({ "query": "rust async" }))
            .await
            .unwrap();
        let parsed: WebSearchResult = serde_json::from_value(out).unwrap();
        assert_eq!(parsed.provider, "brave");
        assert_eq!(parsed.query, "rust async");
        assert_eq!(parsed.hits.len(), 2);
        // Provider saw the trimmed query + clamped max.
        let calls = stub.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "rust async");
        assert_eq!(calls[0].1, DEFAULT_MAX_RESULTS);
    }

    #[tokio::test]
    async fn explicit_provider_overrides_default() {
        let (tool, _stub) = tool_with_stub(&["brave", "google"], "brave", "google");
        // The default is "brave" but the agent asks for "google";
        // since "google" is on the allowlist and a Google provider
        // is registered, the explicit choice wins.
        let out = tool
            .invoke(serde_json::json!({
                "query": "rust",
                "provider": "google"
            }))
            .await
            .unwrap();
        let parsed: WebSearchResult = serde_json::from_value(out).unwrap();
        assert_eq!(parsed.provider, "google");
    }

    #[tokio::test]
    async fn allowlisted_but_unregistered_provider_fails_loudly() {
        // Operator put "brave" on the allowlist but the supervisor
        // didn't register a provider impl for it. Caller gets a
        // clear "config drift" message rather than a silent fall-
        // through.
        let tool = WebSearchTool::with_allowlist(["brave".to_string()], "brave".to_string());
        let err = tool
            .invoke(serde_json::json!({ "query": "x" }))
            .await
            .unwrap_err();
        match err {
            ToolInvokeError::Upstream { message, .. } => {
                assert!(message.contains("config drift"), "{message}");
                assert!(message.contains("brave"), "{message}");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    // ──────────────────────────────────────────────────────────────
    // Query validation
    // ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rejects_empty_query() {
        let (tool, _) = tool_with_stub(&["brave"], "brave", "brave");
        let err = tool
            .invoke(serde_json::json!({ "query": "" }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, ToolInvokeError::InvalidParams { .. }));
        assert!(msg.contains("non-empty"), "got: {msg}");
    }

    #[tokio::test]
    async fn rejects_whitespace_only_query() {
        let (tool, _) = tool_with_stub(&["brave"], "brave", "brave");
        let err = tool
            .invoke(serde_json::json!({ "query": "   \t  " }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolInvokeError::InvalidParams { .. }));
    }

    #[tokio::test]
    async fn rejects_query_with_control_characters() {
        // A control byte in the query would confuse upstream APIs
        // (or worse, smuggle through TLS). Pre-reject so the audit
        // chain doesn't log it either.
        let (tool, _) = tool_with_stub(&["brave"], "brave", "brave");
        let err = tool
            .invoke(serde_json::json!({ "query": "hello\nworld" }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("control characters"), "got: {msg}");
    }

    #[tokio::test]
    async fn rejects_overlong_query() {
        let (tool, _) = tool_with_stub(&["brave"], "brave", "brave");
        let query = "x".repeat(MAX_QUERY_LEN + 1);
        let err = tool
            .invoke(serde_json::json!({ "query": query }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exceeds max"), "got: {msg}");
    }

    #[tokio::test]
    async fn rejects_unknown_field() {
        let (tool, _) = tool_with_stub(&["brave"], "brave", "brave");
        let err = tool
            .invoke(serde_json::json!({
                "query": "x",
                "extra": 1
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolInvokeError::InvalidParams { .. }));
    }

    // ──────────────────────────────────────────────────────────────
    // max_results plumbing
    // ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn caller_max_results_clamped_to_max_allowed() {
        let (tool, stub) = tool_with_stub(&["brave"], "brave", "brave");
        tool.invoke(serde_json::json!({
            "query": "x",
            "max_results": 9999_u32
        }))
        .await
        .unwrap();
        assert_eq!(stub.calls()[0].1, MAX_ALLOWED_RESULTS);
    }

    #[tokio::test]
    async fn custom_max_results_passes_through() {
        let (tool, stub) = tool_with_stub(&["brave"], "brave", "brave");
        tool.invoke(serde_json::json!({
            "query": "x",
            "max_results": 3_u32
        }))
        .await
        .unwrap();
        assert_eq!(stub.calls()[0].1, 3);
    }

    // ──────────────────────────────────────────────────────────────
    // Trait surface
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn tool_name_is_canonical_mvm_prefix() {
        let tool = WebSearchTool::default();
        assert_eq!(tool.name(), TOOL_NAME);
        assert!(TOOL_NAME.starts_with("mvm."));
    }

    #[test]
    fn noop_provider_is_named_noop() {
        let p = NoopSearchProvider;
        assert_eq!(p.name(), "noop");
    }
}
