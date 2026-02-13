use std::collections::HashMap;
use std::net::SocketAddr;

use super::config::CoordinatorConfig;

/// Resolved route for a tenant's gateway pool.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub tenant_id: String,
    pub pool_id: String,
    pub node: SocketAddr,
    pub idle_timeout_secs: u64,
}

/// Lookup table: listen address -> tenant route.
///
/// In port-based mode, each tenant gets its own listen port. The coordinator
/// runs one TCP listener per route and uses the listener's address to determine
/// which tenant the connection belongs to.
#[derive(Debug)]
pub struct RouteTable {
    by_listen_addr: HashMap<SocketAddr, ResolvedRoute>,
}

impl RouteTable {
    /// Build a route table from coordinator config.
    pub fn from_config(config: &CoordinatorConfig) -> Self {
        let mut by_listen_addr = HashMap::new();
        for route in &config.routes {
            let resolved = ResolvedRoute {
                tenant_id: route.tenant_id.clone(),
                pool_id: route.pool_id.clone(),
                node: route.node,
                idle_timeout_secs: route.idle_timeout(&config.coordinator),
            };
            by_listen_addr.insert(route.listen, resolved);
        }
        Self { by_listen_addr }
    }

    /// Look up a route by the listen address that accepted the connection.
    pub fn lookup(&self, listen_addr: &SocketAddr) -> Option<&ResolvedRoute> {
        self.by_listen_addr.get(listen_addr)
    }

    /// All unique listen addresses that need TCP listeners.
    pub fn listen_addrs(&self) -> Vec<SocketAddr> {
        self.by_listen_addr.keys().copied().collect()
    }

    /// All routes in the table.
    pub fn routes(&self) -> impl Iterator<Item = (&SocketAddr, &ResolvedRoute)> {
        self.by_listen_addr.iter()
    }

    /// Number of routes.
    pub fn len(&self) -> usize {
        self.by_listen_addr.len()
    }

    /// Whether the route table is empty.
    pub fn is_empty(&self) -> bool {
        self.by_listen_addr.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;

    fn test_config() -> CoordinatorConfig {
        let toml = r#"
[coordinator]
idle_timeout_secs = 300

[[nodes]]
address = "127.0.0.1:4433"
name = "node-1"

[[routes]]
tenant_id = "alice"
pool_id = "gateways"
listen = "0.0.0.0:8443"
node = "127.0.0.1:4433"

[[routes]]
tenant_id = "bob"
pool_id = "gateways"
listen = "0.0.0.0:8444"
node = "127.0.0.1:4433"
idle_timeout_secs = 600
"#;
        CoordinatorConfig::parse(toml).unwrap()
    }

    #[test]
    fn test_route_table_from_config() {
        let config = test_config();
        let table = RouteTable::from_config(&config);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_lookup_by_listen_addr() {
        let config = test_config();
        let table = RouteTable::from_config(&config);

        let addr: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let route = table.lookup(&addr).unwrap();
        assert_eq!(route.tenant_id, "alice");
        assert_eq!(route.pool_id, "gateways");
        assert_eq!(route.idle_timeout_secs, 300); // global default
    }

    #[test]
    fn test_lookup_with_override() {
        let config = test_config();
        let table = RouteTable::from_config(&config);

        let addr: SocketAddr = "0.0.0.0:8444".parse().unwrap();
        let route = table.lookup(&addr).unwrap();
        assert_eq!(route.tenant_id, "bob");
        assert_eq!(route.idle_timeout_secs, 600); // per-route override
    }

    #[test]
    fn test_lookup_missing() {
        let config = test_config();
        let table = RouteTable::from_config(&config);

        let addr: SocketAddr = "0.0.0.0:9999".parse().unwrap();
        assert!(table.lookup(&addr).is_none());
    }

    #[test]
    fn test_listen_addrs() {
        let config = test_config();
        let table = RouteTable::from_config(&config);

        let mut addrs = table.listen_addrs();
        addrs.sort();
        assert_eq!(addrs.len(), 2);
    }
}
