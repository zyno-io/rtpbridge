use std::sync::Mutex;

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// Prometheus metrics for rtpbridge.
///
/// All metric fields are cheap to clone (atomic internals), so callers can
/// `Arc<Metrics>` and hand out clones freely.
#[derive(Debug)]
pub struct Metrics {
    /// Total sessions created.
    pub sessions_total: Counter,
    /// Currently active sessions.
    pub sessions_active: Gauge,
    /// Total endpoints created.
    pub endpoints_total: Counter,
    /// Currently active endpoints.
    pub endpoints_active: Gauge,
    /// Total packets routed between endpoints.
    pub packets_routed: Counter,
    /// Total packets recorded to PCAP.
    pub packets_recorded: Counter,
    /// Currently active recordings.
    pub recordings_active: Gauge,
    /// SRTP authentication / replay errors.
    pub srtp_errors: Counter,
    /// DTMF events detected.
    pub dtmf_events: Counter,
    /// Transcode errors (decode or encode failures).
    pub transcode_errors: Counter,
    /// Events dropped due to channel backpressure (client too slow).
    pub events_dropped: Counter,

    /// The Prometheus registry.  Wrapped in a `Mutex` because
    /// `encode()` requires `&Registry` but we need interior mutability
    /// for the registry reference behind `Arc<Metrics>`.
    registry: Mutex<Registry>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// Create a new `Metrics` instance with all counters and gauges
    /// registered under the `rtpbridge_` prefix.
    ///
    /// Counter names omit the `_total` suffix because `prometheus-client`
    /// appends it automatically in the OpenMetrics text exposition format.
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let sessions_total = Counter::default();
        let sessions_active = Gauge::default();
        let endpoints_total = Counter::default();
        let endpoints_active = Gauge::default();
        let packets_routed = Counter::default();
        let packets_recorded = Counter::default();
        let recordings_active = Gauge::default();
        let srtp_errors = Counter::default();
        let dtmf_events = Counter::default();
        let transcode_errors = Counter::default();
        let events_dropped = Counter::default();

        // Note: prometheus-client automatically appends `_total` to counter
        // names in the encoded output, so we register without that suffix.
        registry.register(
            "rtpbridge_sessions",
            "Total sessions created",
            sessions_total.clone(),
        );
        registry.register(
            "rtpbridge_sessions_active",
            "Currently active sessions",
            sessions_active.clone(),
        );
        registry.register(
            "rtpbridge_endpoints",
            "Total endpoints created",
            endpoints_total.clone(),
        );
        registry.register(
            "rtpbridge_endpoints_active",
            "Currently active endpoints",
            endpoints_active.clone(),
        );
        registry.register(
            "rtpbridge_packets_routed",
            "Total packets routed between endpoints",
            packets_routed.clone(),
        );
        registry.register(
            "rtpbridge_packets_recorded",
            "Total packets recorded to PCAP",
            packets_recorded.clone(),
        );
        registry.register(
            "rtpbridge_recordings_active",
            "Currently active recordings",
            recordings_active.clone(),
        );
        registry.register(
            "rtpbridge_srtp_errors",
            "SRTP authentication or replay errors",
            srtp_errors.clone(),
        );
        registry.register(
            "rtpbridge_dtmf_events",
            "DTMF events detected",
            dtmf_events.clone(),
        );
        registry.register(
            "rtpbridge_transcode_errors",
            "Transcode errors (decode or encode failures)",
            transcode_errors.clone(),
        );
        registry.register(
            "rtpbridge_events_dropped",
            "Events dropped due to channel backpressure",
            events_dropped.clone(),
        );
        Self {
            sessions_total,
            sessions_active,
            endpoints_total,
            endpoints_active,
            packets_routed,
            packets_recorded,
            recordings_active,
            srtp_errors,
            dtmf_events,
            transcode_errors,
            events_dropped,
            registry: Mutex::new(registry),
        }
    }

    /// Encode all registered metrics in Prometheus text exposition format.
    pub fn encode(&self) -> anyhow::Result<String> {
        let mut buf = String::new();
        // Recover from poisoned lock — the registry data is still valid even if
        // a prior holder panicked. This prevents permanent metrics outage.
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        encode(&mut buf, &registry).map_err(|e| anyhow::anyhow!("metrics encoding failed: {e}"))?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_metrics_encode_empty() {
        let m = Metrics::new();
        let output = m.encode().unwrap();
        // Counters get `_total` suffix automatically; gauges do not.
        assert!(output.contains("rtpbridge_sessions_total"));
        assert!(output.contains("rtpbridge_sessions_active"));
        assert!(output.contains("rtpbridge_endpoints_total"));
        assert!(output.contains("rtpbridge_packets_routed_total"));
        assert!(output.contains("rtpbridge_srtp_errors_total"));
        assert!(output.contains("rtpbridge_dtmf_events_total"));
        assert!(output.contains("rtpbridge_transcode_errors_total"));
        assert!(output.contains("rtpbridge_events_dropped_total"));
    }

    #[test]
    fn test_counter_increment() {
        let m = Metrics::new();
        m.sessions_total.inc();
        m.sessions_total.inc();
        m.packets_routed.inc();
        let output = m.encode().unwrap();
        assert!(output.contains("rtpbridge_sessions_total 2"));
        assert!(output.contains("rtpbridge_packets_routed_total 1"));
    }

    #[tokio::test]
    async fn test_concurrent_counter_increments() {
        use std::sync::Arc;

        let m = Arc::new(Metrics::new());
        let mut handles = Vec::new();

        // Spawn 10 tasks each incrementing counters 100 times
        for _ in 0..10 {
            let m = Arc::clone(&m);
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    m.sessions_total.inc();
                    m.packets_routed.inc();
                    m.endpoints_total.inc();
                    m.sessions_active.inc();
                    tokio::task::yield_now().await; // force interleaving
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let output = m.encode().unwrap();
        // 10 tasks * 100 increments = 1000 total
        assert!(
            output.contains("rtpbridge_sessions_total 1000"),
            "sessions_total should be 1000 after concurrent increments: {}",
            output
        );
        assert!(
            output.contains("rtpbridge_packets_routed_total 1000"),
            "packets_routed should be 1000"
        );
        assert!(
            output.contains("rtpbridge_endpoints_total 1000"),
            "endpoints_total should be 1000"
        );
        assert!(
            output.contains("rtpbridge_sessions_active 1000"),
            "sessions_active gauge should be 1000"
        );
    }

    #[tokio::test]
    async fn test_concurrent_gauge_inc_dec() {
        use std::sync::Arc;

        let m = Arc::new(Metrics::new());
        let mut handles = Vec::new();

        // Spawn tasks that increment and then decrement the gauge
        for _ in 0..10 {
            let m = Arc::clone(&m);
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    m.sessions_active.inc();
                }
                for _ in 0..100 {
                    m.sessions_active.dec();
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let output = m.encode().unwrap();
        // Net change should be 0 (each task inc 100, dec 100)
        assert!(
            output.contains("rtpbridge_sessions_active 0"),
            "sessions_active should be 0 after balanced inc/dec: {}",
            output
        );
    }

    #[test]
    fn test_gauge_inc_dec() {
        let m = Metrics::new();
        m.sessions_active.inc();
        m.sessions_active.inc();
        m.sessions_active.dec();
        let output = m.encode().unwrap();
        assert!(output.contains("rtpbridge_sessions_active 1"));
    }
}
