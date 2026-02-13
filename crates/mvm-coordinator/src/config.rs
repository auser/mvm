use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Top-level coordinator configuration loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct CoordinatorConfig {
    pub coordinator: CoordinatorGlobal,
    #[serde(default)]
    pub nodes: Vec<NodeEntry>,
    #[serde(default)]
    pub routes: Vec<RouteEntry>,
}

/// Global coordinator settings.
#[derive(Debug, Clone, Deserialize)]
pub struct CoordinatorGlobal {
    /// Default idle timeout before sleeping a gateway (seconds).
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Max time to wait for a gateway to wake (seconds).
    #[serde(default = "default_wake_timeout")]
    pub wake_timeout_secs: u64,
    /// Background health check interval (seconds).
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u64,
    /// Max concurrent connections per tenant.
    #[serde(default = "default_max_connections")]
    pub max_connections_per_tenant: u32,
}

/// An agent node the coordinator can talk to.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeEntry {
    /// QUIC address of the agent (host:port).
    pub address: SocketAddr,
    /// Human-readable name for this node.
    #[serde(default)]
    pub name: String,
}

/// A route mapping an inbound listener to a tenant's gateway pool on a node.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteEntry {
    pub tenant_id: String,
    pub pool_id: String,
    /// Listen address for this route (e.g. "0.0.0.0:8443").
    pub listen: SocketAddr,
    /// Agent node address to forward to.
    pub node: SocketAddr,
    /// Per-route idle timeout override (seconds).
    pub idle_timeout_secs: Option<u64>,
}

fn default_idle_timeout() -> u64 {
    300
}
fn default_wake_timeout() -> u64 {
    10
}
fn default_health_interval() -> u64 {
    30
}
fn default_max_connections() -> u32 {
    1000
}

impl CoordinatorConfig {
    /// Load coordinator config from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read coordinator config: {}", path.display()))?;
        Self::parse(&content)
    }

    /// Parse coordinator config from a TOML string.
    pub fn parse(s: &str) -> Result<Self> {
        let config: Self =
            toml::from_str(s).with_context(|| "Failed to parse coordinator config TOML")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.routes.is_empty() {
            anyhow::bail!("Coordinator config must have at least one [[routes]] entry");
        }

        // Check for duplicate listen addresses
        let mut seen_addrs = std::collections::HashSet::new();
        for route in &self.routes {
            if !seen_addrs.insert(route.listen) {
                anyhow::bail!("Duplicate listen address {} in routes config", route.listen);
            }
            // Verify the route's node exists in the nodes list (if nodes are specified)
            if !self.nodes.is_empty() && !self.nodes.iter().any(|n| n.address == route.node) {
                anyhow::bail!(
                    "Route for tenant '{}' references unknown node {}. Add it to [[nodes]].",
                    route.tenant_id,
                    route.node
                );
            }
        }
        Ok(())
    }
}

impl RouteEntry {
    /// Effective idle timeout: per-route override or global default.
    pub fn idle_timeout(&self, global: &CoordinatorGlobal) -> u64 {
        self.idle_timeout_secs.unwrap_or(global.idle_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
[coordinator]

[[nodes]]
address = "127.0.0.1:4433"
name = "node-1"

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
"#;
        let config = CoordinatorConfig::parse(toml).unwrap();
        assert_eq!(config.coordinator.idle_timeout_secs, 300);
        assert_eq!(config.coordinator.wake_timeout_secs, 10);
        assert_eq!(config.coordinator.health_interval_secs, 30);
        assert_eq!(config.nodes.len(), 1);
        assert_eq!(config.nodes[0].name, "node-1");
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].tenant_id, "alice");
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
[coordinator]
idle_timeout_secs = 600
wake_timeout_secs = 15
health_interval_secs = 60
max_connections_per_tenant = 500

[[nodes]]
address = "127.0.0.1:4433"
name = "node-1"

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
idle_timeout_secs = 900

[[routes]]
tenant_id = "bob"
pool_id = "gateways"
listen = "0.0.0.0:8444"
node = "127.0.0.1:4433"
"#;
        let config = CoordinatorConfig::parse(toml).unwrap();
        assert_eq!(config.coordinator.idle_timeout_secs, 600);
        assert_eq!(config.coordinator.max_connections_per_tenant, 500);
        assert_eq!(config.routes.len(), 2);
        assert_eq!(config.routes[0].idle_timeout_secs, Some(900));
        assert_eq!(config.routes[1].idle_timeout_secs, None);
    }

    #[test]
    fn test_route_idle_timeout_override() {
        let toml = r#"
[coordinator]
idle_timeout_secs = 300

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
idle_timeout_secs = 900
"#;
        let config = CoordinatorConfig::parse(toml).unwrap();
        let effective = config.routes[0].idle_timeout(&config.coordinator);
        assert_eq!(effective, 900);
    }

    #[test]
    fn test_route_idle_timeout_default() {
        let toml = r#"
[coordinator]
idle_timeout_secs = 300

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
"#;
        let config = CoordinatorConfig::parse(toml).unwrap();
        let effective = config.routes[0].idle_timeout(&config.coordinator);
        assert_eq!(effective, 300);
    }

    #[test]
    fn test_reject_empty_routes() {
        let toml = r#"
[coordinator]

[[nodes]]
address = "127.0.0.1:4433"
"#;
        let result = CoordinatorConfig::parse(toml);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("at least one"));
    }

    #[test]
    fn test_reject_duplicate_listen() {
        let toml = r#"
[coordinator]

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"

[[routes]]
tenant_id = "bob"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
"#;
        let result = CoordinatorConfig::parse(toml);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("Duplicate listen"));
    }

    #[test]
    fn test_reject_unknown_node() {
        let toml = r#"
[coordinator]

[[nodes]]
address = "127.0.0.1:4433"

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "10.0.0.1:4433"
"#;
        let result = CoordinatorConfig::parse(toml);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("unknown node"));
    }

    #[test]
    fn test_no_nodes_skips_validation() {
        let toml = r#"
[coordinator]

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"
"#;
        // When no [[nodes]] section, route node validation is skipped
        let config = CoordinatorConfig::parse(toml).unwrap();
        assert!(config.nodes.is_empty());
        assert_eq!(config.routes.len(), 1);
    }

    #[test]
    fn test_defaults() {
        assert_eq!(default_idle_timeout(), 300);
        assert_eq!(default_wake_timeout(), 10);
        assert_eq!(default_health_interval(), 30);
        assert_eq!(default_max_connections(), 1000);
    }
}
