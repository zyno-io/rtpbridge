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
    /// Playout buffer: packets dropped because they arrived after their play slot.
    pub playout_late_drops: Counter,
    /// Playout buffer: frames dropped to bound latency when a producer ran ahead.
    pub playout_overflow_drops: Counter,
    /// Playout buffer: silence frames synthesized to ride out a clockless-source underflow.
    pub playout_underflow_fills: Counter,
    /// Events dropped due to channel backpressure (client too slow).
    pub events_dropped: Counter,
    /// Inbound packets that a WebRTC endpoint's `handle_receive` rejected
    /// (str0m parse / auth / DTLS errors). Counts every drop; a per-endpoint
    /// WARN is emitted on first occurrence only.
    pub webrtc_packet_errors: Counter,
    /// WebRTC endpoints that stayed in `Connecting` past the watchdog
    /// threshold without reaching `Connected`. One increment per stuck
    /// endpoint (the watchdog only fires once per Connecting period).
    pub webrtc_connecting_stuck: Counter,
    /// ICE-restart requests rejected because the endpoint already had an
    /// unanswered pending offer. str0m keeps only one pending offer, so
    /// overwriting it would let a later answer apply against the wrong offer
    /// (credential divergence → silent media blackhole). A non-zero value
    /// means a caller issued overlapping ICE restarts.
    pub webrtc_ice_restart_conflicts: Counter,
    /// WebRTC per-endpoint UDP recv task reached its receive loop. The
    /// receive loop is the ONLY thing that pulls inbound ICE/STUN/SRTP off an
    /// endpoint's socket, so this is the heartbeat of the media datapath.
    pub webrtc_recv_task_started: Counter,
    /// WebRTC recv loop exited COOPERATIVELY (cancellation observed in the
    /// `select!`, session-channel close, or UDP error). Does NOT count
    /// `Drop`-driven teardown, which aborts the task — so this sits well below
    /// `_started`, it does not mirror it. A spike means tasks ending abnormally
    /// (e.g. `udp_error`).
    pub webrtc_recv_task_exited: Counter,
    /// WebRTC endpoints whose recv task had already finished while the endpoint
    /// was still active in the session. This covers panics, UDP-error exits,
    /// session-channel closes, and unexpected cooperative exits that leave a live
    /// endpoint with no socket reader.
    pub webrtc_recv_task_dead: Counter,
    /// WebRTC endpoints whose recv task never reached its receive loop within the
    /// grace window. The session liveness sweep observes this AFTER creation (it
    /// does not block/abort creation). A non-zero value proves the never-started
    /// receive-task variant; a media blackhole where the task starts but never
    /// gets socket readiness must be diagnosed with the live probe/runbook.
    /// See docs/WEBRTC_RECV_TASK_WEDGE.md.
    pub webrtc_recv_task_start_timeout: Counter,
    /// Inbound WebRTC packets dropped because the session packet channel was
    /// full (would have blocked `send`). Switching to `try_send` keeps a full
    /// channel from PARKING the UDP reader; sustained non-zero means the session
    /// task is behind, not that the datapath is wedged.
    pub webrtc_recv_overflow: Counter,
    /// WebRTC UDP datagrams successfully handed to the OS socket.
    ///
    /// This is the send-result counterpart to endpoint outbound RTP counters,
    /// which are incremented when media is written into str0m. A packet can be
    /// counted as outbound RTP but still fail at the UDP send step.
    pub webrtc_udp_send_ok: Counter,
    /// WebRTC UDP datagrams dropped because `try_send_to` failed or str0m chose a
    /// local candidate address that no bound endpoint socket owns.
    pub webrtc_udp_send_dropped: Counter,

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
        let playout_late_drops = Counter::default();
        let playout_overflow_drops = Counter::default();
        let playout_underflow_fills = Counter::default();
        let events_dropped = Counter::default();
        let webrtc_packet_errors = Counter::default();
        let webrtc_connecting_stuck = Counter::default();
        let webrtc_ice_restart_conflicts = Counter::default();
        let webrtc_recv_task_started = Counter::default();
        let webrtc_recv_task_exited = Counter::default();
        let webrtc_recv_task_dead = Counter::default();
        let webrtc_recv_task_start_timeout = Counter::default();
        let webrtc_recv_overflow = Counter::default();
        let webrtc_udp_send_ok = Counter::default();
        let webrtc_udp_send_dropped = Counter::default();

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
            "rtpbridge_playout_late_drops",
            "Playout packets dropped for arriving after their play slot",
            playout_late_drops.clone(),
        );
        registry.register(
            "rtpbridge_playout_overflow_drops",
            "Playout frames dropped to bound latency when a producer ran ahead",
            playout_overflow_drops.clone(),
        );
        registry.register(
            "rtpbridge_playout_underflow_fills",
            "Playout silence frames synthesized to ride out clockless-source underflow",
            playout_underflow_fills.clone(),
        );
        registry.register(
            "rtpbridge_events_dropped",
            "Events dropped due to channel backpressure",
            events_dropped.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_packet_errors",
            "Inbound packets a WebRTC endpoint's str0m handle_receive rejected",
            webrtc_packet_errors.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_connecting_stuck",
            "WebRTC endpoints that stayed in Connecting past the watchdog threshold",
            webrtc_connecting_stuck.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_ice_restart_conflicts",
            "ICE-restart requests rejected because an unanswered offer was already pending",
            webrtc_ice_restart_conflicts.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_recv_task_started",
            "WebRTC recv tasks that reached their receive loop",
            webrtc_recv_task_started.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_recv_task_exited",
            "WebRTC recv tasks that exited their receive loop",
            webrtc_recv_task_exited.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_recv_task_dead",
            "WebRTC recv tasks found finished while their endpoint was still active",
            webrtc_recv_task_dead.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_recv_task_start_timeout",
            "WebRTC endpoints whose recv task never reached its receive loop within the grace window",
            webrtc_recv_task_start_timeout.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_recv_overflow",
            "Inbound WebRTC packets dropped because the session packet channel was full",
            webrtc_recv_overflow.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_udp_send_ok",
            "WebRTC UDP datagrams successfully handed to the OS socket",
            webrtc_udp_send_ok.clone(),
        );
        registry.register(
            "rtpbridge_webrtc_udp_send_dropped",
            "WebRTC UDP datagrams dropped at the socket send step",
            webrtc_udp_send_dropped.clone(),
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
            playout_late_drops,
            playout_overflow_drops,
            playout_underflow_fills,
            events_dropped,
            webrtc_packet_errors,
            webrtc_connecting_stuck,
            webrtc_ice_restart_conflicts,
            webrtc_recv_task_started,
            webrtc_recv_task_exited,
            webrtc_recv_task_dead,
            webrtc_recv_task_start_timeout,
            webrtc_recv_overflow,
            webrtc_udp_send_ok,
            webrtc_udp_send_dropped,
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
        assert!(output.contains("rtpbridge_webrtc_recv_task_started_total"));
        assert!(output.contains("rtpbridge_webrtc_recv_task_exited_total"));
        assert!(output.contains("rtpbridge_webrtc_recv_task_dead_total"));
        assert!(output.contains("rtpbridge_webrtc_recv_task_start_timeout_total"));
        assert!(output.contains("rtpbridge_webrtc_recv_overflow_total"));
        assert!(output.contains("rtpbridge_webrtc_udp_send_ok_total"));
        assert!(output.contains("rtpbridge_webrtc_udp_send_dropped_total"));
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
