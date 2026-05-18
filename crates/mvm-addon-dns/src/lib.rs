//! In-guest DNS resolver for configured local addon hostnames.
//!
//! Thin wrapper over `hickory-dns`. Listens on `127.0.0.1:53` and
//! `::1:53` only; authoritative only for exact hostnames configured
//! in the per-instance zone; forwards everything else upstream. The
//! zone is loaded from the config disk's `addon_dns_zone` (see
//! `mvm/specs/contracts/local-addon-dns.md`).
//!
//! This crate intentionally contains no distributed mesh logic.

#![warn(missing_docs)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::Path;

/// One A-record entry the resolver serves. The config-disk zone is a
/// JSON array of these (see contract spec).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneRecord {
    /// Fully-qualified hostname (e.g. `db.dev.internal`).
    pub hostname: String,
    /// IPv4 address the resolver returns for `A` queries against
    /// `hostname`.
    pub address: Ipv4Addr,
}

/// Parse the config disk's `addon_dns_zone` JSON file into a list of
/// records. The on-disk format is the JSON shape spec'd in
/// `mvm/specs/contracts/local-addon-dns.md`:
///
/// ```jsonc
/// [
///   {"hostname": "db.dev.internal", "address": "10.255.0.1"},
///   {"hostname": "cache.dev.internal", "address": "10.255.0.2"}
/// ]
/// ```
pub fn load_zone(path: &Path) -> Result<Vec<ZoneRecord>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("could not read zone file at {}", path.display()))?;
    if body.trim().is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(&body).with_context(|| {
        format!(
            "could not parse zone file at {} as a JSON array of {{hostname, address}} entries",
            path.display()
        )
    })
}

/// In-process zone state. Owned by the resolver loop; refreshed on
/// SIGHUP. Methods are intentionally read-only at this layer — zone
/// updates flow through `load_zone` + `Zone::set_records`.
pub struct Zone {
    records: Vec<ZoneRecord>,
}

impl Zone {
    /// Build a `Zone` from a parsed record list.
    pub fn new(records: Vec<ZoneRecord>) -> Self {
        Self { records }
    }

    /// Replace the in-memory zone wholesale. Caller responsibility:
    /// take a write lock if the resolver is reading concurrently.
    pub fn set_records(&mut self, records: Vec<ZoneRecord>) {
        self.records = records;
    }

    /// Look up an A record. Case-insensitive on the hostname.
    /// Returns the first matching record; the contract spec
    /// guarantees at most one entry per hostname per instance.
    pub fn lookup(&self, hostname: &str) -> Option<&ZoneRecord> {
        let hostname = normalize_hostname(hostname);
        self.records
            .iter()
            .find(|r| normalize_hostname(&r.hostname).eq_ignore_ascii_case(hostname))
    }

    /// Whether the zone is authoritative for `hostname`. Authority
    /// is intentionally limited to exact configured records so local
    /// addon DNS can mirror production hostnames without hijacking a
    /// whole domain or suffix.
    pub fn is_authoritative_for(&self, hostname: &str) -> bool {
        self.lookup(hostname).is_some()
    }

    /// Number of records currently loaded. Useful for "no-op when
    /// zone is empty" diagnostics in the binary.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the zone has zero records loaded (and thus the
    /// resolver should idle as a no-op).
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

fn normalize_hostname(hostname: &str) -> &str {
    hostname.trim_end_matches('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_zone_parses_minimal_records() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("zone.json");
        std::fs::write(
            &path,
            r#"[
              {"hostname": "db.dev.internal", "address": "10.255.0.1"},
              {"hostname": "cache.dev.internal", "address": "10.255.0.2"}
            ]"#,
        )
        .unwrap();
        let records = load_zone(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].hostname, "db.dev.internal");
        assert_eq!(records[0].address, Ipv4Addr::new(10, 255, 0, 1));
    }

    #[test]
    fn load_zone_accepts_empty_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("zone.json");
        std::fs::write(&path, "").unwrap();
        assert!(load_zone(&path).unwrap().is_empty());
    }

    #[test]
    fn zone_lookup_is_case_insensitive() {
        let zone = Zone::new(vec![ZoneRecord {
            hostname: "db.dev.internal".to_string(),
            address: Ipv4Addr::new(10, 255, 0, 1),
        }]);
        assert!(zone.lookup("db.dev.internal").is_some());
        assert!(zone.lookup("DB.DEV.INTERNAL").is_some());
        assert!(zone.lookup("missing.dev.internal").is_none());
    }

    #[test]
    fn is_authoritative_for_only_recognizes_configured_names() {
        let zone = Zone::new(vec![ZoneRecord {
            hostname: "db.dev.internal".to_string(),
            address: Ipv4Addr::new(10, 255, 0, 1),
        }]);
        assert!(zone.is_authoritative_for("db.dev.internal"));
        assert!(zone.is_authoritative_for("db.dev.internal."));
        assert!(zone.is_authoritative_for("DB.DEV.INTERNAL"));
        assert!(!zone.is_authoritative_for("cache.dev.internal"));
        assert!(!zone.is_authoritative_for("dev.internal"));
        assert!(!zone.is_authoritative_for("example.com"));
        assert!(!zone.is_authoritative_for("evil.db.dev.internal.attacker.com"));
    }

    #[test]
    fn zone_set_records_replaces_state() {
        let mut zone = Zone::new(vec![ZoneRecord {
            hostname: "old.dev.internal".to_string(),
            address: Ipv4Addr::new(10, 255, 0, 1),
        }]);
        assert_eq!(zone.len(), 1);
        zone.set_records(vec![]);
        assert_eq!(zone.len(), 0);
        assert!(zone.is_empty());
    }
}
