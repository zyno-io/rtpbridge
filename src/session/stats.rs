use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Wire-level inbound datagram counters for endpoints backed by a UDP socket
/// (WebRTC, plain RTP). Counts EVERY datagram the socket delivers — STUN/ICE
/// bindings, DTLS, RTCP, RTP, and malformed junk — BEFORE any demux or parse.
///
/// Contrast with [`EndpointStats`] `inbound_*`, which count only validated RTP
/// media (post-demux for WebRTC, post-parse/decrypt for RTP). The gap between
/// the two is the signal for "remote path is alive but sending no media" (raw
/// climbing, media flat); both flat is a dead path — the remote-network-failure
/// signal. STUN consent keepalives (RFC 7675) and RTCP keep arriving even
/// during media silence, so this counter moves whenever the peer's network path
/// is up, independent of whether it is producing media.
///
/// Incremented from the per-endpoint recv task and read from the session task,
/// so the fields are atomic and the struct is shared via `Arc`.
#[derive(Debug, Default)]
pub struct RawRecvCounters {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl RawRecvCounters {
    /// Record one received datagram of `bytes` length (called once per
    /// successful `recv_from`, before any demux/parse).
    pub fn record(&self, bytes: usize) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn packets(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Per-endpoint statistics
#[derive(Debug, Clone)]
pub struct EndpointStats {
    pub inbound_packets: u64,
    pub inbound_bytes: u64,
    pub outbound_packets: u64,
    pub outbound_bytes: u64,
    pub last_received: Option<Instant>,
    pub created_at: Instant,
}

impl Default for EndpointStats {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointStats {
    pub fn new() -> Self {
        Self {
            inbound_packets: 0,
            inbound_bytes: 0,
            outbound_packets: 0,
            outbound_bytes: 0,
            last_received: None,
            created_at: Instant::now(),
        }
    }

    pub fn record_inbound(&mut self, bytes: usize) {
        self.inbound_packets += 1;
        self.inbound_bytes += bytes as u64;
        self.last_received = Some(Instant::now());
    }

    pub fn record_outbound(&mut self, bytes: usize) {
        self.outbound_packets += 1;
        self.outbound_bytes += bytes as u64;
    }

    /// Milliseconds since last received packet, or None if never received
    pub fn ms_since_last_received(&self) -> Option<u64> {
        self.last_received.map(|t| t.elapsed().as_millis() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn new_stats_are_zeroed() {
        let stats = EndpointStats::new();
        assert_eq!(stats.inbound_packets, 0);
        assert_eq!(stats.inbound_bytes, 0);
        assert_eq!(stats.outbound_packets, 0);
        assert_eq!(stats.outbound_bytes, 0);
        assert!(stats.last_received.is_none());
    }

    #[test]
    fn default_matches_new() {
        let a = EndpointStats::new();
        let b = EndpointStats::default();
        assert_eq!(a.inbound_packets, b.inbound_packets);
        assert_eq!(a.inbound_bytes, b.inbound_bytes);
        assert_eq!(a.outbound_packets, b.outbound_packets);
        assert_eq!(a.outbound_bytes, b.outbound_bytes);
        assert!(a.last_received.is_none());
        assert!(b.last_received.is_none());
    }

    // --- record_inbound tests ---

    #[test]
    fn record_inbound_increments_packets_and_bytes() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(160);
        assert_eq!(stats.inbound_packets, 1);
        assert_eq!(stats.inbound_bytes, 160);
    }

    #[test]
    fn record_inbound_accumulates_over_multiple_calls() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(100);
        stats.record_inbound(200);
        stats.record_inbound(300);
        assert_eq!(stats.inbound_packets, 3);
        assert_eq!(stats.inbound_bytes, 600);
    }

    #[test]
    fn record_inbound_sets_last_received() {
        let mut stats = EndpointStats::new();
        assert!(stats.last_received.is_none());
        stats.record_inbound(10);
        assert!(stats.last_received.is_some());
    }

    #[test]
    fn record_inbound_does_not_affect_outbound() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(500);
        assert_eq!(stats.outbound_packets, 0);
        assert_eq!(stats.outbound_bytes, 0);
    }

    // --- record_outbound tests ---

    #[test]
    fn record_outbound_increments_packets_and_bytes() {
        let mut stats = EndpointStats::new();
        stats.record_outbound(320);
        assert_eq!(stats.outbound_packets, 1);
        assert_eq!(stats.outbound_bytes, 320);
    }

    #[test]
    fn record_outbound_accumulates_over_multiple_calls() {
        let mut stats = EndpointStats::new();
        stats.record_outbound(50);
        stats.record_outbound(75);
        assert_eq!(stats.outbound_packets, 2);
        assert_eq!(stats.outbound_bytes, 125);
    }

    #[test]
    fn record_outbound_does_not_affect_inbound_or_last_received() {
        let mut stats = EndpointStats::new();
        stats.record_outbound(100);
        assert_eq!(stats.inbound_packets, 0);
        assert_eq!(stats.inbound_bytes, 0);
        assert!(stats.last_received.is_none());
    }

    // --- ms_since_last_received tests ---

    #[test]
    fn ms_since_last_received_none_before_any_packets() {
        let stats = EndpointStats::new();
        assert!(stats.ms_since_last_received().is_none());
    }

    #[test]
    fn ms_since_last_received_returns_some_after_inbound() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(10);
        let ms = stats.ms_since_last_received();
        assert!(ms.is_some());
        // Should be very recent (within 1 second realistically)
        assert!(ms.unwrap() < 1000);
    }

    #[test]
    fn ms_since_last_received_grows_over_time() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(10);
        // Sleep briefly so elapsed time is measurable
        thread::sleep(Duration::from_millis(20));
        let ms = stats.ms_since_last_received().unwrap();
        assert!(ms >= 15, "expected at least 15ms elapsed, got {ms}");
    }

    #[test]
    fn ms_since_last_received_updates_on_subsequent_inbound() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(10);
        thread::sleep(Duration::from_millis(30));
        // Record another inbound -- last_received should reset
        stats.record_inbound(20);
        let ms = stats.ms_since_last_received().unwrap();
        // Should be very recent since we just recorded
        assert!(ms < 20, "expected <20ms after fresh inbound, got {ms}");
    }

    // --- Edge cases ---

    #[test]
    fn record_inbound_zero_length_payload() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(0);
        assert_eq!(stats.inbound_packets, 1);
        assert_eq!(stats.inbound_bytes, 0);
        assert!(stats.last_received.is_some());
    }

    #[test]
    fn record_outbound_zero_length_payload() {
        let mut stats = EndpointStats::new();
        stats.record_outbound(0);
        assert_eq!(stats.outbound_packets, 1);
        assert_eq!(stats.outbound_bytes, 0);
    }

    #[test]
    fn record_inbound_very_large_payload() {
        let mut stats = EndpointStats::new();
        // usize::MAX on 64-bit is 2^64-1; as u64 it should fit
        let large: usize = 1_000_000_000;
        stats.record_inbound(large);
        assert_eq!(stats.inbound_packets, 1);
        assert_eq!(stats.inbound_bytes, large as u64);
    }

    #[test]
    fn record_outbound_very_large_payload() {
        let mut stats = EndpointStats::new();
        let large: usize = 1_000_000_000;
        stats.record_outbound(large);
        assert_eq!(stats.outbound_packets, 1);
        assert_eq!(stats.outbound_bytes, large as u64);
    }

    #[test]
    fn many_small_packets_accumulate_correctly() {
        let mut stats = EndpointStats::new();
        for _ in 0..10_000 {
            stats.record_inbound(1);
        }
        assert_eq!(stats.inbound_packets, 10_000);
        assert_eq!(stats.inbound_bytes, 10_000);
    }

    #[test]
    fn mixed_inbound_and_outbound_tracked_independently() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(100);
        stats.record_outbound(200);
        stats.record_inbound(300);
        stats.record_outbound(400);
        assert_eq!(stats.inbound_packets, 2);
        assert_eq!(stats.inbound_bytes, 400);
        assert_eq!(stats.outbound_packets, 2);
        assert_eq!(stats.outbound_bytes, 600);
    }

    #[test]
    fn clone_preserves_stats() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(42);
        stats.record_outbound(99);
        let cloned = stats.clone();
        assert_eq!(cloned.inbound_packets, 1);
        assert_eq!(cloned.inbound_bytes, 42);
        assert_eq!(cloned.outbound_packets, 1);
        assert_eq!(cloned.outbound_bytes, 99);
        assert!(cloned.last_received.is_some());
    }

    #[test]
    fn clone_is_independent() {
        let mut stats = EndpointStats::new();
        stats.record_inbound(10);
        let mut cloned = stats.clone();
        cloned.record_inbound(20);
        // Original should be unaffected
        assert_eq!(stats.inbound_packets, 1);
        assert_eq!(stats.inbound_bytes, 10);
        // Clone should have both
        assert_eq!(cloned.inbound_packets, 2);
        assert_eq!(cloned.inbound_bytes, 30);
    }

    // --- RawRecvCounters tests ---

    #[test]
    fn raw_recv_counters_default_is_zero() {
        let raw = RawRecvCounters::default();
        assert_eq!(raw.packets(), 0);
        assert_eq!(raw.bytes(), 0);
    }

    #[test]
    fn raw_recv_record_accumulates_packets_and_bytes() {
        let raw = RawRecvCounters::default();
        raw.record(20); // e.g. a STUN binding
        raw.record(160); // an RTP datagram
        raw.record(0); // a zero-length datagram still counts as one packet
        assert_eq!(raw.packets(), 3);
        assert_eq!(raw.bytes(), 180);
    }

    #[test]
    fn raw_recv_counters_shared_across_arc_clones() {
        use std::sync::Arc;
        // The recv task holds one Arc clone and the session task another; both
        // must observe the same underlying counter.
        let a = Arc::new(RawRecvCounters::default());
        let b = Arc::clone(&a);
        a.record(100);
        b.record(50);
        assert_eq!(a.packets(), 2);
        assert_eq!(a.bytes(), 150);
        assert_eq!(b.packets(), 2);
        assert_eq!(b.bytes(), 150);
    }

    #[test]
    fn raw_recv_counters_accumulate_under_concurrency() {
        use std::sync::Arc;
        let raw = Arc::new(RawRecvCounters::default());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let r = Arc::clone(&raw);
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    r.record(10);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(raw.packets(), 8000);
        assert_eq!(raw.bytes(), 80_000);
    }
}
