//! Plan 113 / ADR-064 — flow-count-metrics observer.
//!
//! Per-tenant flow counters surfaced via mvm-cli's existing
//! --metrics-port Prometheus endpoint (mvm-cli/src/metrics_server.rs).
//! Three counter families keyed on tenant:
//!
//!   mvm_flow_opened_total{tenant="..."}
//!   mvm_flow_closed_total{tenant="..."}
//!   mvm_flow_close_reason_total{tenant="...",reason="..."}
//!
//! Wire-up to the CLI metrics endpoint is Task 7 (per-VM scrape file
//! written under `~/.mvm/audit/metrics-<vm>-flow-count.prom`).
//!
//! The mvm-supervisor::gateway_bridge::FlowEvent does NOT carry a
//! tenant string per event — the supervisor is single-VM single-tenant
//! by construction (ADR-002 "one guest = one workload"). The tenant is
//! established at supervisor startup via BridgeConfig.plan.tenant; this
//! observer reads MVM_TENANT once at Arc-construction time, which is
//! one of the four canonical tenant sources from ADR-064 §Decision 9.

#![allow(dead_code)] // Task 7 wires the scrape-file path; until then
// prometheus_format / scrape_file_path are only
// exercised by tests.

use crate::gateway_bridge::{FlowEvent, FlowEventKind};
use crate::network::{Observer, RequiredCapabilities};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct FlowCountMetrics {
    tenant: String,
    opened: AtomicU64,
    closed: AtomicU64,
    closed_by_reason: Mutex<std::collections::BTreeMap<String, u64>>,
}

impl FlowCountMetrics {
    /// Constructor used by `ObserverAllowlist::resolve`. The allowlist
    /// closure signature is `Fn() -> Arc<dyn Observer>` (no args), so
    /// the tenant is read from `MVM_TENANT` at construction time.
    /// Renamed from `new` to `into_arc` per Task 1's `clippy::new_ret_no_self`
    /// resolution.
    pub fn into_arc() -> Arc<dyn Observer> {
        let tenant = std::env::var("MVM_TENANT").unwrap_or_else(|_| "local".to_string());
        Arc::new(Self {
            tenant,
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    pub fn opened(&self) -> u64 {
        self.opened.load(Ordering::SeqCst)
    }

    pub fn closed(&self) -> u64 {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn closed_by_reason_snapshot(&self) -> std::collections::BTreeMap<String, u64> {
        self.closed_by_reason
            .lock()
            .expect("flow-count-metrics mutex poisoned")
            .clone()
    }

    /// Prometheus text format for the three counter families.
    /// Mounted by mvm-cli's /metrics handler in Task 7 via the per-VM
    /// scrape file at `~/.mvm/audit/metrics-<vm>-flow-count.prom`.
    pub fn prometheus_format(&self) -> String {
        let tenant = &self.tenant;
        let opened = self.opened.load(Ordering::SeqCst);
        let closed = self.closed.load(Ordering::SeqCst);
        let reasons = self.closed_by_reason_snapshot();
        let mut out = String::new();
        out.push_str(
            "# HELP mvm_flow_opened_total Total flows observed opened per tenant\n\
             # TYPE mvm_flow_opened_total counter\n",
        );
        out.push_str(&format!(
            "mvm_flow_opened_total{{tenant=\"{tenant}\"}} {opened}\n"
        ));
        out.push_str(
            "# HELP mvm_flow_closed_total Total flows observed closed per tenant\n\
             # TYPE mvm_flow_closed_total counter\n",
        );
        out.push_str(&format!(
            "mvm_flow_closed_total{{tenant=\"{tenant}\"}} {closed}\n"
        ));
        out.push_str(
            "# HELP mvm_flow_close_reason_total Per-reason flow-closed counters\n\
             # TYPE mvm_flow_close_reason_total counter\n",
        );
        for (reason, n) in reasons {
            out.push_str(&format!(
                "mvm_flow_close_reason_total{{tenant=\"{tenant}\",reason=\"{reason}\"}} {n}\n"
            ));
        }
        out
    }
}

impl Observer for FlowCountMetrics {
    fn name(&self) -> &'static str {
        "flow-count-metrics"
    }

    fn required_capabilities(&self) -> RequiredCapabilities {
        RequiredCapabilities {
            flow_events: true,
            payload_tap: false,
        }
    }

    fn on_flow_event(&self, event: &FlowEvent) {
        match &event.kind {
            FlowEventKind::Opened => {
                self.opened.fetch_add(1, Ordering::SeqCst);
            }
            FlowEventKind::Closed { reason } => {
                self.closed.fetch_add(1, Ordering::SeqCst);
                let mut g = self
                    .closed_by_reason
                    .lock()
                    .expect("flow-count-metrics mutex poisoned");
                let key = reason.as_str().to_string();
                *g.entry(key).or_insert(0) += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{FlowCloseReason, FlowDirection};

    // Tests construct FlowCountMetrics directly via struct literal so
    // they have access to the concrete fields; FlowCountMetrics::into_arc()
    // returns Arc<dyn Observer> which doesn't expose internals.

    fn opened_evt() -> FlowEvent {
        FlowEvent {
            flow_id: "vm-a-egress-1".to_string(),
            direction: FlowDirection::Egress,
            kind: FlowEventKind::Opened,
        }
    }

    fn closed_evt(reason: FlowCloseReason) -> FlowEvent {
        FlowEvent {
            flow_id: "vm-a-egress-1".to_string(),
            direction: FlowDirection::Egress,
            kind: FlowEventKind::Closed { reason },
        }
    }

    #[test]
    fn opened_counter_increments() {
        let m = FlowCountMetrics {
            tenant: "test-opens".into(),
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        };
        m.on_flow_event(&opened_evt());
        m.on_flow_event(&opened_evt());
        assert_eq!(m.opened(), 2);
    }

    #[test]
    fn closed_counter_and_reason_split() {
        let m = FlowCountMetrics {
            tenant: "test-closes".into(),
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        };
        m.on_flow_event(&closed_evt(FlowCloseReason::Eof));
        m.on_flow_event(&closed_evt(FlowCloseReason::PolicyDropped));
        m.on_flow_event(&closed_evt(FlowCloseReason::Eof));
        assert_eq!(m.closed(), 3);
        let snap = m.closed_by_reason_snapshot();
        assert_eq!(snap.get("eof").copied(), Some(2));
        assert_eq!(snap.get("policy_dropped").copied(), Some(1));
    }

    #[test]
    fn prometheus_format_emits_expected_lines() {
        let m = FlowCountMetrics {
            tenant: "acme".into(),
            opened: AtomicU64::new(5),
            closed: AtomicU64::new(3),
            closed_by_reason: Mutex::new({
                let mut m = std::collections::BTreeMap::new();
                m.insert("eof".to_string(), 2u64);
                m.insert("policy_dropped".to_string(), 1u64);
                m
            }),
        };
        let prom = m.prometheus_format();
        assert!(
            prom.contains("mvm_flow_opened_total{tenant=\"acme\"} 5"),
            "prom was: {prom}"
        );
        assert!(
            prom.contains("mvm_flow_closed_total{tenant=\"acme\"} 3"),
            "prom was: {prom}"
        );
        assert!(
            prom.contains("mvm_flow_close_reason_total{tenant=\"acme\",reason=\"eof\"} 2"),
            "prom was: {prom}"
        );
        assert!(
            prom.contains(
                "mvm_flow_close_reason_total{tenant=\"acme\",reason=\"policy_dropped\"} 1"
            ),
            "prom was: {prom}"
        );
    }

    #[test]
    fn required_capabilities_no_payload_tap() {
        let m = FlowCountMetrics {
            tenant: "caps-test".into(),
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            closed_by_reason: Mutex::new(std::collections::BTreeMap::new()),
        };
        let req = m.required_capabilities();
        assert!(req.flow_events);
        assert!(!req.payload_tap);
    }
}
