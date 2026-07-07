use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
#[derive(Debug)]
pub struct RawRecvCounters {
    packets: AtomicU64,
    bytes: AtomicU64,
    recv_loop_gap_ms: AtomicU64,
    max_recv_loop_gap_ms: AtomicU64,
    enqueue_wait_ms: AtomicU64,
    max_enqueue_wait_ms: AtomicU64,
    dequeue_delay_ms: AtomicU64,
    max_dequeue_delay_ms: AtomicU64,
    channel_capacity: AtomicU64,
    min_channel_capacity: AtomicU64,
    channel_overflows: AtomicU64,
    raw_rtp_packets: AtomicU64,
    raw_rtp_bytes: AtomicU64,
    raw_rtp_packets_lost: AtomicU64,
    raw_rtp_sequence_gaps: AtomicU64,
    raw_rtp_max_sequence_gap: AtomicU64,
    raw_rtp_duplicate_packets: AtomicU64,
    raw_rtp_out_of_order_packets: AtomicU64,
    raw_rtp_sequence_resets: AtomicU64,
    raw_rtp_last_sequence: AtomicU64,
    raw_rtp_last_ssrc: AtomicU64,
    raw_rtp_initialized: AtomicBool,
}

impl Default for RawRecvCounters {
    fn default() -> Self {
        Self {
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            recv_loop_gap_ms: AtomicU64::new(0),
            max_recv_loop_gap_ms: AtomicU64::new(0),
            enqueue_wait_ms: AtomicU64::new(0),
            max_enqueue_wait_ms: AtomicU64::new(0),
            dequeue_delay_ms: AtomicU64::new(0),
            max_dequeue_delay_ms: AtomicU64::new(0),
            channel_capacity: AtomicU64::new(0),
            min_channel_capacity: AtomicU64::new(u64::MAX),
            channel_overflows: AtomicU64::new(0),
            raw_rtp_packets: AtomicU64::new(0),
            raw_rtp_bytes: AtomicU64::new(0),
            raw_rtp_packets_lost: AtomicU64::new(0),
            raw_rtp_sequence_gaps: AtomicU64::new(0),
            raw_rtp_max_sequence_gap: AtomicU64::new(0),
            raw_rtp_duplicate_packets: AtomicU64::new(0),
            raw_rtp_out_of_order_packets: AtomicU64::new(0),
            raw_rtp_sequence_resets: AtomicU64::new(0),
            raw_rtp_last_sequence: AtomicU64::new(0),
            raw_rtp_last_ssrc: AtomicU64::new(0),
            raw_rtp_initialized: AtomicBool::new(false),
        }
    }
}

impl RawRecvCounters {
    /// Record one received datagram of `bytes` length (called once per
    /// successful `recv_from`, before any demux/parse).
    pub fn record(&self, bytes: usize) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Record receive-loop diagnostics for a socket-backed endpoint.
    ///
    /// `recv_gap` is the time since this recv task last pulled a datagram from
    /// the OS socket. `channel_capacity` is the session packet channel's
    /// remaining capacity just before enqueue/drop.
    pub fn record_recv_diagnostics(&self, recv_gap: Option<Duration>, channel_capacity: usize) {
        if let Some(gap) = recv_gap {
            let gap_ms = duration_ms(gap);
            self.recv_loop_gap_ms.store(gap_ms, Ordering::Relaxed);
            update_max(&self.max_recv_loop_gap_ms, gap_ms);
        }
        let capacity = channel_capacity as u64;
        self.channel_capacity.store(capacity, Ordering::Relaxed);
        update_min(&self.min_channel_capacity, capacity);
    }

    /// Record how long the recv task waited for capacity in the session packet
    /// channel. For non-blocking paths this is recorded as zero.
    pub fn record_enqueue_wait(&self, wait: Duration) {
        let wait_ms = duration_ms(wait);
        self.enqueue_wait_ms.store(wait_ms, Ordering::Relaxed);
        update_max(&self.max_enqueue_wait_ms, wait_ms);
    }

    /// Record how long a packet sat in the session packet channel before the
    /// media session task dequeued it.
    pub fn record_dequeue_delay(&self, delay: Duration) {
        let delay_ms = duration_ms(delay);
        self.dequeue_delay_ms.store(delay_ms, Ordering::Relaxed);
        update_max(&self.max_dequeue_delay_ms, delay_ms);
    }

    pub fn record_channel_overflow(&self) {
        self.channel_overflows.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an RTP-looking datagram before WebRTC demux/decrypt.
    ///
    /// SRTP leaves the RTP fixed header in clear text, so this lets the recv task
    /// distinguish "the packet was absent at bridge ingress" from "the packet
    /// reached the bridge socket but did not emerge as a str0m RTP event".
    pub fn record_raw_rtp_datagram(&self, data: &[u8]) {
        let Some((seq, ssrc)) = parse_raw_rtp_header(data) else {
            return;
        };

        self.raw_rtp_packets.fetch_add(1, Ordering::Relaxed);
        self.raw_rtp_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        let seq = u64::from(seq);
        let ssrc = u64::from(ssrc);
        if !self.raw_rtp_initialized.swap(true, Ordering::Relaxed) {
            self.raw_rtp_last_sequence.store(seq, Ordering::Relaxed);
            self.raw_rtp_last_ssrc.store(ssrc, Ordering::Relaxed);
            return;
        }

        let last_ssrc = self.raw_rtp_last_ssrc.load(Ordering::Relaxed);
        if last_ssrc != ssrc {
            self.raw_rtp_sequence_resets.fetch_add(1, Ordering::Relaxed);
            self.raw_rtp_last_sequence.store(seq, Ordering::Relaxed);
            self.raw_rtp_last_ssrc.store(ssrc, Ordering::Relaxed);
            return;
        }

        let last_seq = self.raw_rtp_last_sequence.load(Ordering::Relaxed) as u16;
        let seq16 = seq as u16;
        let delta = seq16.wrapping_sub(last_seq);
        match delta {
            0 => {
                self.raw_rtp_duplicate_packets
                    .fetch_add(1, Ordering::Relaxed);
            }
            1 => {
                self.raw_rtp_last_sequence.store(seq, Ordering::Relaxed);
            }
            2..=0x7fff => {
                let missing = u64::from(delta - 1);
                self.raw_rtp_packets_lost
                    .fetch_add(missing, Ordering::Relaxed);
                self.raw_rtp_sequence_gaps.fetch_add(1, Ordering::Relaxed);
                update_max(&self.raw_rtp_max_sequence_gap, missing);
                self.raw_rtp_last_sequence.store(seq, Ordering::Relaxed);
            }
            _ => {
                self.raw_rtp_out_of_order_packets
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn packets(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn recv_loop_gap_ms(&self) -> u64 {
        self.recv_loop_gap_ms.load(Ordering::Relaxed)
    }

    pub fn max_recv_loop_gap_ms(&self) -> u64 {
        self.max_recv_loop_gap_ms.load(Ordering::Relaxed)
    }

    pub fn enqueue_wait_ms(&self) -> u64 {
        self.enqueue_wait_ms.load(Ordering::Relaxed)
    }

    pub fn max_enqueue_wait_ms(&self) -> u64 {
        self.max_enqueue_wait_ms.load(Ordering::Relaxed)
    }

    pub fn dequeue_delay_ms(&self) -> u64 {
        self.dequeue_delay_ms.load(Ordering::Relaxed)
    }

    pub fn max_dequeue_delay_ms(&self) -> u64 {
        self.max_dequeue_delay_ms.load(Ordering::Relaxed)
    }

    pub fn channel_capacity(&self) -> u64 {
        self.channel_capacity.load(Ordering::Relaxed)
    }

    pub fn min_channel_capacity(&self) -> Option<u64> {
        let value = self.min_channel_capacity.load(Ordering::Relaxed);
        if value == u64::MAX { None } else { Some(value) }
    }

    pub fn channel_overflows(&self) -> u64 {
        self.channel_overflows.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_packets(&self) -> u64 {
        self.raw_rtp_packets.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_bytes(&self) -> u64 {
        self.raw_rtp_bytes.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_packets_lost(&self) -> u64 {
        self.raw_rtp_packets_lost.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_sequence_gaps(&self) -> u64 {
        self.raw_rtp_sequence_gaps.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_max_sequence_gap(&self) -> u64 {
        self.raw_rtp_max_sequence_gap.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_duplicate_packets(&self) -> u64 {
        self.raw_rtp_duplicate_packets.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_out_of_order_packets(&self) -> u64 {
        self.raw_rtp_out_of_order_packets.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_sequence_resets(&self) -> u64 {
        self.raw_rtp_sequence_resets.load(Ordering::Relaxed)
    }

    pub fn raw_rtp_last_sequence(&self) -> Option<u64> {
        self.raw_rtp_initialized
            .load(Ordering::Relaxed)
            .then(|| self.raw_rtp_last_sequence.load(Ordering::Relaxed))
    }

    pub fn raw_rtp_last_ssrc(&self) -> Option<u64> {
        self.raw_rtp_initialized
            .load(Ordering::Relaxed)
            .then(|| self.raw_rtp_last_ssrc.load(Ordering::Relaxed))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn parse_raw_rtp_header(data: &[u8]) -> Option<(u16, u32)> {
    if data.len() < 12 || (data[0] & 0xc0) != 0x80 {
        return None;
    }

    // RTCP packet types occupy the second byte in the 192..=223 range. RTP's
    // marker bit can set the high bit, so values above 223 are still valid RTP.
    if (192..=223).contains(&data[1]) {
        return None;
    }

    let csrc_count = usize::from(data[0] & 0x0f);
    if data.len() < 12 + csrc_count * 4 {
        return None;
    }

    Some((
        u16::from_be_bytes([data[2], data[3]]),
        u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
    ))
}

fn update_max(value: &AtomicU64, candidate: u64) {
    let mut current = value.load(Ordering::Relaxed);
    while candidate > current {
        match value.compare_exchange_weak(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

fn update_min(value: &AtomicU64, candidate: u64) {
    let mut current = value.load(Ordering::Relaxed);
    while candidate < current {
        match value.compare_exchange_weak(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(next) => current = next,
        }
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
    fn raw_recv_diagnostics_track_latest_and_extremes() {
        let raw = RawRecvCounters::default();
        assert_eq!(raw.min_channel_capacity(), None);

        raw.record_recv_diagnostics(Some(Duration::from_millis(25)), 128);
        raw.record_recv_diagnostics(Some(Duration::from_millis(10)), 64);
        raw.record_enqueue_wait(Duration::from_millis(3));
        raw.record_enqueue_wait(Duration::from_millis(12));
        raw.record_dequeue_delay(Duration::from_millis(30));
        raw.record_dequeue_delay(Duration::from_millis(5));
        raw.record_channel_overflow();

        assert_eq!(raw.recv_loop_gap_ms(), 10);
        assert_eq!(raw.max_recv_loop_gap_ms(), 25);
        assert_eq!(raw.enqueue_wait_ms(), 12);
        assert_eq!(raw.max_enqueue_wait_ms(), 12);
        assert_eq!(raw.dequeue_delay_ms(), 5);
        assert_eq!(raw.max_dequeue_delay_ms(), 30);
        assert_eq!(raw.channel_capacity(), 64);
        assert_eq!(raw.min_channel_capacity(), Some(64));
        assert_eq!(raw.channel_overflows(), 1);
    }

    #[test]
    fn raw_recv_rtp_sequence_tracking_classifies_gaps_and_reordering() {
        let raw = RawRecvCounters::default();
        raw.record_raw_rtp_datagram(&stun_like_packet());
        raw.record_raw_rtp_datagram(&rtcp_like_packet());

        raw.record_raw_rtp_datagram(&rtp_packet(10, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(11, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(15, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(15, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(14, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(1, 0x0506_0708));

        assert_eq!(raw.raw_rtp_packets(), 6);
        assert_eq!(raw.raw_rtp_bytes(), 72);
        assert_eq!(raw.raw_rtp_packets_lost(), 3);
        assert_eq!(raw.raw_rtp_sequence_gaps(), 1);
        assert_eq!(raw.raw_rtp_max_sequence_gap(), 3);
        assert_eq!(raw.raw_rtp_duplicate_packets(), 1);
        assert_eq!(raw.raw_rtp_out_of_order_packets(), 1);
        assert_eq!(raw.raw_rtp_sequence_resets(), 1);
        assert_eq!(raw.raw_rtp_last_sequence(), Some(1));
        assert_eq!(raw.raw_rtp_last_ssrc(), Some(0x0506_0708));
    }

    #[test]
    fn raw_recv_rtp_sequence_tracking_handles_rollover() {
        let raw = RawRecvCounters::default();
        raw.record_raw_rtp_datagram(&rtp_packet(u16::MAX - 1, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(u16::MAX, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(0, 0x0102_0304));
        raw.record_raw_rtp_datagram(&rtp_packet(2, 0x0102_0304));

        assert_eq!(raw.raw_rtp_packets(), 4);
        assert_eq!(raw.raw_rtp_packets_lost(), 1);
        assert_eq!(raw.raw_rtp_sequence_gaps(), 1);
        assert_eq!(raw.raw_rtp_last_sequence(), Some(2));
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

    fn rtp_packet(seq: u16, ssrc: u32) -> Vec<u8> {
        let mut packet = vec![0u8; 12];
        packet[0] = 0x80;
        packet[1] = 111;
        packet[2..4].copy_from_slice(&seq.to_be_bytes());
        packet[4..8].copy_from_slice(&1234u32.to_be_bytes());
        packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
        packet
    }

    fn stun_like_packet() -> Vec<u8> {
        vec![0x00, 0x01, 0x00, 0x00]
    }

    fn rtcp_like_packet() -> Vec<u8> {
        vec![0x80, 200, 0x00, 0x06, 0, 0, 0, 1, 0, 0, 0, 2]
    }
}
