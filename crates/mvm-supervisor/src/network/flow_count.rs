//! Plan 113 — flow-count-metrics observer; implementation lands in Task 2.

#![allow(dead_code)] // Task 2 fills in per-tenant counters + Prometheus output.

use crate::gateway_bridge::FlowEvent;
use crate::network::{Observer, RequiredCapabilities};
use std::sync::Arc;

pub(crate) struct FlowCountMetrics;

impl FlowCountMetrics {
    /// Returns the observer wrapped in `Arc<dyn Observer>` for direct
    /// insertion into a `Pipeline`. Named `into_arc` (not `new`) to
    /// avoid clippy's `new_ret_no_self` — the constructor returns
    /// `Arc<dyn Observer>`, not `Self`.
    pub(crate) fn into_arc() -> Arc<dyn Observer> {
        Arc::new(FlowCountMetrics)
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
    fn on_flow_event(&self, _: &FlowEvent) {}
}
