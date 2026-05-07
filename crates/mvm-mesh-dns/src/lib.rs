//! In-guest DNS resolver for `*.mesh.local` (ADR-0018 / ADR-0020).
//!
//! Thin wrapper over `hickory-dns`. Listens on `127.0.0.1:53` and
//! `::1:53` only; authoritative for `*.mesh.local`; forwards
//! everything else upstream. Per-instance zone loaded from the
//! config disk's `mesh_dns_zone` (see
//! `mvm/specs/contracts/in-guest-mesh-dns.md`).
//!
//! The resolver is **iroh-free** by design — the cryptographic data
//! plane lives on the host in mvmd (per ADR-0020 §Hard constraint).
//! `cargo tree -p mvm-mesh-dns` MUST NOT include any `iroh*` crate.

#![warn(missing_docs)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::Path;

/// One A-record entry the resolver serves. The config-disk zone is a
/// JSON array of these (see contract spec).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneRecord {
    /// Fully-qualified hostname (e.g. `db.mesh.local`).
    pub hostname: String,
    /// IPv4 address the resolver returns for `A` queries against
    /// `hostname`.
    pub address: Ipv4Addr,
}

/// Parse the config disk's `mesh_dns_zone` JSON file into a list of
/// records. The on-disk format is the JSON shape spec'd in
/// `mvm/specs/contracts/in-guest-mesh-dns.md`:
///
/// ```jsonc
/// [
///   {"hostname": "db.mesh.local", "address": "10.255.0.1"},
///   {"hostname": "cache.mesh.local", "address": "10.255.0.2"}
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
        self.records
            .iter()
            .find(|r| r.hostname.eq_ignore_ascii_case(hostname))
    }

    /// Whether the zone is authoritative for `hostname` — i.e. it
    /// ends in `.mesh.local`. The resolver uses this to decide
    /// between answering authoritatively and forwarding upstream.
    pub fn is_authoritative_for(&self, hostname: &str) -> bool {
        let h = hostname.trim_end_matches('.');
        h.ends_with(".mesh.local") || h == "mesh.local"
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
              {"hostname": "db.mesh.local", "address": "10.255.0.1"},
              {"hostname": "cache.mesh.local", "address": "10.255.0.2"}
            ]"#,
        )
        .unwrap();
        let records = load_zone(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].hostname, "db.mesh.local");
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
            hostname: "db.mesh.local".to_string(),
            address: Ipv4Addr::new(10, 255, 0, 1),
        }]);
        assert!(zone.lookup("db.mesh.local").is_some());
        assert!(zone.lookup("DB.MESH.LOCAL").is_some());
        assert!(zone.lookup("missing.mesh.local").is_none());
    }

    #[test]
    fn is_authoritative_for_only_recognizes_mesh_local() {
        let zone = Zone::new(vec![]);
        assert!(zone.is_authoritative_for("db.mesh.local"));
        assert!(zone.is_authoritative_for("db.mesh.local."));
        assert!(zone.is_authoritative_for("mesh.local"));
        assert!(!zone.is_authoritative_for("example.com"));
        assert!(!zone.is_authoritative_for("local"));
        // Defensive: a hostname containing "mesh.local" mid-string is
        // NOT authoritative — only suffix match.
        assert!(!zone.is_authoritative_for("evil.mesh.local.attacker.com"));
    }

    #[test]
    fn zone_set_records_replaces_state() {
        let mut zone = Zone::new(vec![ZoneRecord {
            hostname: "old.mesh.local".to_string(),
            address: Ipv4Addr::new(10, 255, 0, 1),
        }]);
        assert_eq!(zone.len(), 1);
        zone.set_records(vec![]);
        assert_eq!(zone.len(), 0);
        assert!(zone.is_empty());
    }
}
