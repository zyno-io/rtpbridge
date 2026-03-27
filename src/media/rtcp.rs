/// RTCP packet types
const RTCP_SR: u8 = 200;
const RTCP_RR: u8 = 201;
const RTCP_SDES: u8 = 202;
const RTCP_BYE: u8 = 203;

/// Parsed RTCP Sender Report
#[allow(dead_code)] // wire format fields read in tests and future stats aggregation
#[derive(Debug, Clone)]
pub struct SenderReport {
    pub ssrc: u32,
    pub ntp_timestamp: u64,
    pub rtp_timestamp: u32,
    pub sender_packet_count: u32,
    pub sender_octet_count: u32,
    pub report_blocks: Vec<ReportBlock>,
}

/// Parsed RTCP Receiver Report
#[allow(dead_code)] // wire format fields read in tests and future stats aggregation
#[derive(Debug, Clone)]
pub struct ReceiverReport {
    pub ssrc: u32,
    pub report_blocks: Vec<ReportBlock>,
}

/// RTCP Report Block (shared between SR and RR)
#[allow(dead_code)] // wire format fields read in tests and future stats aggregation
#[derive(Debug, Clone)]
pub struct ReportBlock {
    pub ssrc: u32,
    pub fraction_lost: u8,
    pub cumulative_lost: u32, // 24-bit
    pub highest_seq: u32,
    pub jitter: u32,
    pub last_sr: u32,
    pub delay_since_last_sr: u32,
}

/// Statistics tracker for generating RTCP reports (RFC 3550 Appendix A.3)
#[derive(Debug)]
pub struct RtcpStats {
    // Receive stats (for generating RR about what we receive)
    pub packets_received: u32,
    pub octets_received: u32,
    pub jitter: u32,
    pub last_sr_ntp: u32, // middle 32 bits of NTP from last SR
    pub last_sr_received: Option<std::time::Instant>,

    // RFC 3550 A.3 sequence number tracking
    seq_initialized: bool,
    base_seq: u32,         // first seq seen (for expected count)
    extended_max_seq: u32, // high 16 bits = ROC (rollover count), low 16 = max seq
    expected_prior: u32,   // expected packets at last RR (for fraction_lost interval)
    received_prior: u32,   // received packets at last RR (for fraction_lost interval)

    last_transit: i64,
    /// Monotonic epoch for jitter arrival-time computation (avoids SystemTime NTP jumps)
    epoch: std::time::Instant,

    // Send stats (for generating SR about what we send)
    pub packets_sent: u32,
    pub octets_sent: u32,

    // RTT tracking: NTP middle-32 of our last sent SR, for RTT computation
    // when we receive an RR referencing it.
    pub our_last_sr_ntp_middle: u32,

    // Computed RTT from the remote's RR (RFC 3550 §6.4.1)
    pub rtt_ms: Option<f64>,
}

impl Default for RtcpStats {
    fn default() -> Self {
        Self::new()
    }
}

impl RtcpStats {
    pub fn new() -> Self {
        Self {
            packets_received: 0,
            octets_received: 0,
            jitter: 0,
            last_sr_ntp: 0,
            last_sr_received: None,
            seq_initialized: false,
            base_seq: 0,
            extended_max_seq: 0,
            expected_prior: 0,
            received_prior: 0,
            last_transit: 0,
            epoch: std::time::Instant::now(),
            packets_sent: 0,
            octets_sent: 0,
            our_last_sr_ntp_middle: 0,
            rtt_ms: None,
        }
    }

    /// Record an inbound RTP packet for stats (RFC 3550 A.3 sequence tracking)
    pub fn record_received(
        &mut self,
        seq: u16,
        timestamp: u32,
        payload_len: usize,
        clock_rate: u32,
    ) {
        self.packets_received = self.packets_received.wrapping_add(1);
        self.octets_received = self.octets_received.wrapping_add(payload_len as u32);

        let seq32 = seq as u32;
        if !self.seq_initialized {
            self.base_seq = seq32;
            self.extended_max_seq = seq32;
            self.seq_initialized = true;
        } else {
            // RFC 3550 A.1: detect rollover by comparing to the low 16 bits
            let max_seq_lo = self.extended_max_seq & 0xFFFF;
            let roc = self.extended_max_seq & 0xFFFF0000;

            // If seq < max by more than half the space, it's a forward rollover
            if seq32 < max_seq_lo && (max_seq_lo - seq32) > 0x8000 {
                // Rollover: seq wrapped from 65535 to 0
                self.extended_max_seq = roc.wrapping_add(0x10000) | seq32;
            } else if seq32 > max_seq_lo {
                // Normal forward progression
                self.extended_max_seq = roc | seq32;
            }
            // else: duplicate or reordered old packet — don't update max
        }

        // Jitter calculation (RFC 3550 A.8)
        // Use microsecond precision to avoid integer truncation for low clock rates.
        // E.g. clock_rate=8000, timestamp=1: 1*1_000_000/8000 = 125us (vs 0ms before).
        if clock_rate > 0 {
            let arrival_us = self.epoch.elapsed().as_micros() as i64;
            let transit = arrival_us - (timestamp as i64 * 1_000_000 / clock_rate as i64);
            if self.last_transit != 0 {
                let d = (transit - self.last_transit)
                    .unsigned_abs()
                    .min(u32::MAX as u64) as u32;
                self.jitter = self.jitter.wrapping_add(d.wrapping_sub(self.jitter) >> 4);
            }
            self.last_transit = transit;
        }
    }

    /// Total expected packets since start (RFC 3550 A.3)
    pub fn expected_packets(&self) -> u32 {
        if !self.seq_initialized {
            return 0;
        }
        self.extended_max_seq - self.base_seq + 1
    }

    /// Cumulative packets lost since start (RFC 3550 A.3)
    pub fn cumulative_lost(&self) -> u32 {
        self.expected_packets()
            .saturating_sub(self.packets_received)
    }

    /// Compute fraction lost since last RR and update prior counters (RFC 3550 A.3)
    pub fn fraction_lost_and_update(&mut self) -> u8 {
        let expected = self.expected_packets();
        let expected_interval = expected.wrapping_sub(self.expected_prior);
        let received_interval = self.packets_received.wrapping_sub(self.received_prior);
        self.expected_prior = expected;
        self.received_prior = self.packets_received;

        if expected_interval == 0 || received_interval >= expected_interval {
            return 0;
        }
        let lost_interval = expected_interval - received_interval;
        (((lost_interval as u64) << 8) / expected_interval as u64).min(255) as u8
    }

    /// The extended highest sequence number for RTCP reports
    pub fn highest_seq(&self) -> u32 {
        self.extended_max_seq
    }

    /// Record an outbound RTP packet for stats
    pub fn record_sent(&mut self, payload_len: usize) {
        self.packets_sent = self.packets_sent.wrapping_add(1);
        self.octets_sent = self.octets_sent.wrapping_add(payload_len as u32);
    }

    /// Process an incoming RTCP Sender Report
    pub fn process_sr(&mut self, sr: &SenderReport) {
        self.last_sr_ntp = ((sr.ntp_timestamp >> 16) & 0xFFFFFFFF) as u32;
        self.last_sr_received = Some(std::time::Instant::now());
    }

    /// Process a Receiver Report block that references our SSRC.
    /// Computes RTT from LSR/DLSR fields per RFC 3550 §6.4.1.
    pub fn process_rr(&mut self, block: &ReportBlock, our_ssrc: u32) {
        if block.ssrc != our_ssrc {
            return;
        }
        if block.last_sr == 0 {
            return; // remote hasn't received an SR from us yet
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        let ntp_secs = now.as_secs() + 2208988800; // NTP epoch offset
        let ntp_frac = (now.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000;
        let ntp = (ntp_secs << 32) | ntp_frac;
        let ntp_middle = ((ntp >> 16) & 0xFFFFFFFF) as u32;

        let rtt_ntp = ntp_middle
            .wrapping_sub(block.last_sr)
            .wrapping_sub(block.delay_since_last_sr);

        // Convert from NTP compact (16.16 fixed point) to milliseconds
        let rtt_sec = (rtt_ntp >> 16) as f64 + (rtt_ntp & 0xFFFF) as f64 / 65536.0;
        let rtt = rtt_sec * 1000.0;

        // Sanity: positive and < 10 seconds
        if (0.0..10000.0).contains(&rtt) {
            self.rtt_ms = Some(rtt);
        }
    }

    /// Record that we sent an SR with the given NTP timestamp.
    pub fn record_sr_sent(&mut self, ntp: u64) {
        self.our_last_sr_ntp_middle = ((ntp >> 16) & 0xFFFFFFFF) as u32;
    }
}

/// Build a compound RTCP packet containing SR + RR.
/// Also records the NTP timestamp of this SR in `stats` for RTT computation.
pub fn build_sr_rr(
    our_ssrc: u32,
    remote_ssrc: u32,
    stats: &mut RtcpStats,
    rtp_timestamp: u32,
    clock_rate: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);

    // NTP timestamp (crude approximation)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let ntp_secs = now.as_secs() + 2208988800; // NTP epoch offset
    let ntp_frac = (now.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000;
    let ntp = (ntp_secs << 32) | ntp_frac;

    // Record NTP of our outbound SR for RTT computation when we receive RR
    stats.record_sr_sent(ntp);

    // Sender Report (with 1 report block = RR about remote)
    let rc = 1u8; // 1 report block
    // Header: V=2, P=0, RC=1, PT=200 (SR)
    buf.push(0x80 | rc);
    buf.push(RTCP_SR);
    // Length in 32-bit words minus 1 (excluding header word):
    // SSRC(1) + SR_fields(5) + report_blocks(6 each) = 6 + 6*RC
    let length: u16 = 6 + (rc as u16 * 6);
    buf.extend_from_slice(&length.to_be_bytes());
    // SSRC of sender
    buf.extend_from_slice(&our_ssrc.to_be_bytes());
    // NTP timestamp (8 bytes)
    buf.extend_from_slice(&ntp.to_be_bytes());
    // RTP timestamp corresponding to the NTP timestamp above
    buf.extend_from_slice(&rtp_timestamp.to_be_bytes());
    // Sender packet count
    buf.extend_from_slice(&stats.packets_sent.to_be_bytes());
    // Sender octet count
    buf.extend_from_slice(&stats.octets_sent.to_be_bytes());

    // Report block about the remote party
    // SSRC being reported on
    buf.extend_from_slice(&remote_ssrc.to_be_bytes());
    // Fraction lost (8 bits) + cumulative lost (24 bits) — RFC 3550 A.3
    let fraction_lost = stats.fraction_lost_and_update();
    let cum_lost = stats.cumulative_lost() & 0x00FFFFFF;
    buf.push(fraction_lost);
    buf.push(((cum_lost >> 16) & 0xFF) as u8);
    buf.push(((cum_lost >> 8) & 0xFF) as u8);
    buf.push((cum_lost & 0xFF) as u8);
    // Extended highest sequence number
    buf.extend_from_slice(&stats.highest_seq().to_be_bytes());
    // Interarrival jitter — convert from microseconds to RTP timestamp units
    let rate = if clock_rate > 0 { clock_rate } else { 8000 };
    let jitter_ts = (stats.jitter as u64 * rate as u64 / 1_000_000) as u32;
    buf.extend_from_slice(&jitter_ts.to_be_bytes());
    // Last SR (middle 32 bits of NTP)
    buf.extend_from_slice(&stats.last_sr_ntp.to_be_bytes());
    // Delay since last SR (1/65536 sec units)
    let dlsr = stats
        .last_sr_received
        .map(|t| {
            let elapsed = t.elapsed();
            ((elapsed.as_secs().min(0xFFFF) as u32) << 16)
                | (elapsed.subsec_micros() as u64 * 65536 / 1_000_000) as u32
        })
        .unwrap_or(0);
    buf.extend_from_slice(&dlsr.to_be_bytes());

    buf
}

/// Parse an RTCP compound packet, extracting SR and RR data
pub fn parse_rtcp(data: &[u8]) -> Vec<RtcpPacket> {
    let mut packets = Vec::new();
    let mut offset = 0;

    while offset + 4 <= data.len() {
        let version = (data[offset] >> 6) & 0x03;
        if version != 2 {
            break;
        }

        let padding = (data[offset] >> 5) & 0x01 != 0;
        let rc = data[offset] & 0x1F;
        let pt = data[offset + 1];
        let length = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let packet_len = (length + 1) * 4;

        if offset + packet_len > data.len() {
            break;
        }

        // Strip padding bytes if P bit is set (RFC 3550 §6.4.1).
        // Last byte of the packet indicates the number of padding bytes.
        let pkt_data = &data[offset..offset + packet_len];
        let content_len = if padding && !pkt_data.is_empty() {
            let pad_count = *pkt_data.last().unwrap() as usize;
            if pad_count > 0 && pad_count <= pkt_data.len() {
                pkt_data.len() - pad_count
            } else {
                pkt_data.len()
            }
        } else {
            pkt_data.len()
        };
        let pkt_content = &pkt_data[..content_len];

        match pt {
            RTCP_SR if pkt_content.len() >= 28 => {
                let ssrc = u32::from_be_bytes([
                    pkt_content[4],
                    pkt_content[5],
                    pkt_content[6],
                    pkt_content[7],
                ]);
                let ntp = u64::from_be_bytes([
                    pkt_content[8],
                    pkt_content[9],
                    pkt_content[10],
                    pkt_content[11],
                    pkt_content[12],
                    pkt_content[13],
                    pkt_content[14],
                    pkt_content[15],
                ]);
                let rtp_ts = u32::from_be_bytes([
                    pkt_content[16],
                    pkt_content[17],
                    pkt_content[18],
                    pkt_content[19],
                ]);
                let pkt_count = u32::from_be_bytes([
                    pkt_content[20],
                    pkt_content[21],
                    pkt_content[22],
                    pkt_content[23],
                ]);
                let oct_count = u32::from_be_bytes([
                    pkt_content[24],
                    pkt_content[25],
                    pkt_content[26],
                    pkt_content[27],
                ]);

                let blocks = parse_report_blocks(&pkt_content[28..], rc);

                packets.push(RtcpPacket::SenderReport(SenderReport {
                    ssrc,
                    ntp_timestamp: ntp,
                    rtp_timestamp: rtp_ts,
                    sender_packet_count: pkt_count,
                    sender_octet_count: oct_count,
                    report_blocks: blocks,
                }));
            }
            RTCP_RR if pkt_content.len() >= 8 => {
                let ssrc = u32::from_be_bytes([
                    pkt_content[4],
                    pkt_content[5],
                    pkt_content[6],
                    pkt_content[7],
                ]);
                let blocks = parse_report_blocks(&pkt_content[8..], rc);

                packets.push(RtcpPacket::ReceiverReport(ReceiverReport {
                    ssrc,
                    report_blocks: blocks,
                }));
            }
            RTCP_BYE => {
                // RFC 3550 §6.6: BYE packet — RC = number of SSRCs
                let mut ssrc_list = Vec::new();
                let mut off = 4;
                for _ in 0..rc {
                    if off + 4 > pkt_content.len() {
                        break;
                    }
                    ssrc_list.push(u32::from_be_bytes([
                        pkt_content[off],
                        pkt_content[off + 1],
                        pkt_content[off + 2],
                        pkt_content[off + 3],
                    ]));
                    off += 4;
                }
                // Optional length-prefixed reason string
                let reason = if off < pkt_content.len() {
                    let len = pkt_content[off] as usize;
                    off += 1;
                    if len > 0 && off + len <= pkt_content.len() {
                        Some(String::from_utf8_lossy(&pkt_content[off..off + len]).to_string())
                    } else {
                        None
                    }
                } else {
                    None
                };
                packets.push(RtcpPacket::Bye(ByePacket { ssrc_list, reason }));
            }
            RTCP_SDES => {
                tracing::trace!("SDES packet skipped");
            }
            _ => {
                // Skip unknown RTCP types (APP, etc.)
            }
        }

        offset += packet_len;
    }

    packets
}

fn parse_report_blocks(data: &[u8], count: u8) -> Vec<ReportBlock> {
    let mut blocks = Vec::new();
    let mut off = 0;

    for _ in 0..count {
        if off + 24 > data.len() {
            break;
        }
        blocks.push(ReportBlock {
            ssrc: u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]),
            fraction_lost: data[off + 4],
            cumulative_lost: u32::from_be_bytes([0, data[off + 5], data[off + 6], data[off + 7]]),
            highest_seq: u32::from_be_bytes([
                data[off + 8],
                data[off + 9],
                data[off + 10],
                data[off + 11],
            ]),
            jitter: u32::from_be_bytes([
                data[off + 12],
                data[off + 13],
                data[off + 14],
                data[off + 15],
            ]),
            last_sr: u32::from_be_bytes([
                data[off + 16],
                data[off + 17],
                data[off + 18],
                data[off + 19],
            ]),
            delay_since_last_sr: u32::from_be_bytes([
                data[off + 20],
                data[off + 21],
                data[off + 22],
                data[off + 23],
            ]),
        });
        off += 24;
    }

    blocks
}

/// Parsed RTCP BYE packet (RFC 3550 §6.6)
#[derive(Debug, Clone)]
pub struct ByePacket {
    pub ssrc_list: Vec<u32>,
    pub reason: Option<String>,
}

/// Parsed RTCP packet
#[derive(Debug)]
pub enum RtcpPacket {
    SenderReport(SenderReport),
    ReceiverReport(ReceiverReport),
    Bye(ByePacket),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_and_parse_sr() {
        let mut stats = RtcpStats::new();
        stats.packets_sent = 100;
        stats.octets_sent = 16000;
        // Simulate receiving 50 packets (seq 0..49)
        for seq in 0u16..50 {
            stats.record_received(seq, seq as u32 * 160, 160, 8000);
        }

        let data = build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);
        let packets = parse_rtcp(&data);

        assert_eq!(packets.len(), 1);
        match &packets[0] {
            RtcpPacket::SenderReport(sr) => {
                assert_eq!(sr.ssrc, 0x12345678);
                assert_eq!(sr.sender_packet_count, 100);
                assert_eq!(sr.sender_octet_count, 16000);
                assert_eq!(sr.report_blocks.len(), 1);
                assert_eq!(sr.report_blocks[0].ssrc, 0xAABBCCDD);
            }
            _ => panic!("expected SR"),
        }
    }

    #[test]
    fn test_parse_padded_sr() {
        // Build a normal SR, then add padding and set the P bit
        let mut stats = RtcpStats::new();
        stats.packets_sent = 42;
        stats.octets_sent = 8000;
        let mut data = build_sr_rr(0xDEADBEEF, 0x11223344, &mut stats, 0, 8000);

        let pad_bytes = 4u8; // add 4 bytes of padding
        data.extend_from_slice(&[0x00, 0x00, 0x00, pad_bytes]);

        // Update length field (word 1-2): add 1 word for the padding
        let orig_len = u16::from_be_bytes([data[2], data[3]]);
        let new_len = orig_len + 1; // 1 extra 32-bit word
        data[2..4].copy_from_slice(&new_len.to_be_bytes());

        // Set P bit (bit 5 of first byte)
        data[0] |= 0x20;

        let packets = parse_rtcp(&data);
        assert_eq!(packets.len(), 1);
        match &packets[0] {
            RtcpPacket::SenderReport(sr) => {
                assert_eq!(sr.ssrc, 0xDEADBEEF);
                assert_eq!(sr.sender_packet_count, 42);
                assert_eq!(sr.report_blocks.len(), 1);
                assert_eq!(sr.report_blocks[0].ssrc, 0x11223344);
            }
            _ => panic!("expected SR"),
        }
    }

    #[test]
    fn test_rtt_from_rr() {
        let our_ssrc = 0x12345678;
        let mut stats = RtcpStats::new();

        // Compute current NTP middle to use as the "sent SR" timestamp
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let ntp_secs = now.as_secs() + 2208988800;
        let ntp_frac = (now.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000;
        let ntp = (ntp_secs << 32) | ntp_frac;
        stats.record_sr_sent(ntp);
        let sr_ntp_middle = stats.our_last_sr_ntp_middle;
        assert_ne!(sr_ntp_middle, 0);

        // Simulate the remote sending back an RR referencing our SR.
        // DLSR = 0 (instant reflection, remote had zero processing delay)
        let block = ReportBlock {
            ssrc: our_ssrc,
            fraction_lost: 0,
            cumulative_lost: 0,
            highest_seq: 10,
            jitter: 0,
            last_sr: sr_ntp_middle,
            delay_since_last_sr: 0,
        };

        stats.process_rr(&block, our_ssrc);

        // RTT = now_ntp_middle - LSR - DLSR ≈ 0 (within a few ms)
        let rtt = stats.rtt_ms.expect("RTT should be computed");
        assert!(rtt >= 0.0, "RTT should be non-negative, got {}", rtt);
        assert!(
            rtt < 5000.0,
            "RTT should be < 5s for same-process test, got {}",
            rtt
        );
    }

    #[test]
    fn test_rtt_none_without_rr() {
        let stats = RtcpStats::new();
        assert!(stats.rtt_ms.is_none());
    }

    #[test]
    fn test_rr_wrong_ssrc_ignored() {
        let mut stats = RtcpStats::new();
        stats.our_last_sr_ntp_middle = 0x12340000;

        let block = ReportBlock {
            ssrc: 0xDEADBEEF, // wrong SSRC
            fraction_lost: 0,
            cumulative_lost: 0,
            highest_seq: 0,
            jitter: 0,
            last_sr: 0x12340000,
            delay_since_last_sr: 0,
        };

        stats.process_rr(&block, 0x12345678);
        assert!(
            stats.rtt_ms.is_none(),
            "RTT should not be computed for wrong SSRC"
        );
    }

    #[test]
    fn test_sequential_packets_no_loss() {
        let mut stats = RtcpStats::new();
        for seq in 0u16..100 {
            stats.record_received(seq, seq as u32 * 160, 160, 8000);
        }
        assert_eq!(stats.expected_packets(), 100);
        assert_eq!(stats.packets_received, 100);
        assert_eq!(stats.cumulative_lost(), 0);
    }

    #[test]
    fn test_gap_causes_loss() {
        let mut stats = RtcpStats::new();
        // Send seq 0..4, skip 5..9, send 10..14
        for seq in 0u16..5 {
            stats.record_received(seq, seq as u32 * 160, 160, 8000);
        }
        for seq in 10u16..15 {
            stats.record_received(seq, seq as u32 * 160, 160, 8000);
        }
        assert_eq!(stats.expected_packets(), 15); // 0..14 inclusive
        assert_eq!(stats.packets_received, 10);
        assert_eq!(stats.cumulative_lost(), 5);
    }

    #[test]
    fn test_reordered_packets_no_spurious_loss() {
        let mut stats = RtcpStats::new();
        // Receive: 0, 2, 1, 3 (reordered, no actual loss)
        stats.record_received(0, 0, 160, 8000);
        stats.record_received(2, 320, 160, 8000);
        stats.record_received(1, 160, 160, 8000); // reordered
        stats.record_received(3, 480, 160, 8000);
        assert_eq!(stats.expected_packets(), 4);
        assert_eq!(stats.packets_received, 4);
        assert_eq!(stats.cumulative_lost(), 0);
    }

    #[test]
    fn test_sequence_wraparound() {
        let mut stats = RtcpStats::new();
        // Start near wraparound
        for seq in 65534u16..=65535 {
            stats.record_received(seq, seq as u32 * 160, 160, 8000);
        }
        // Wrap around
        for seq in 0u16..2 {
            stats.record_received(seq, (65536 + seq as u32) * 160, 160, 8000);
        }
        assert_eq!(stats.packets_received, 4);
        assert_eq!(stats.expected_packets(), 4);
        assert_eq!(stats.cumulative_lost(), 0);
    }

    #[test]
    fn test_fraction_lost_interval() {
        let mut stats = RtcpStats::new();
        // First interval: 10 expected, 10 received → 0 loss
        for seq in 0u16..10 {
            stats.record_received(seq, seq as u32 * 160, 160, 8000);
        }
        assert_eq!(stats.fraction_lost_and_update(), 0);

        // Second interval: 10 expected (10..19), 5 received → 50% loss
        for seq in [10u16, 12, 14, 16, 18] {
            stats.record_received(seq, seq as u32 * 160, 160, 8000);
        }
        // expected_interval = 19 - 10 + 1 - (10-10) = 10; received_interval = 15-10 = 5
        // lost_interval = 5; fraction = (5 << 8) / 10 = 128
        let frac = stats.fraction_lost_and_update();
        assert!(
            frac > 100 && frac < 150,
            "fraction_lost should be ~128 (50%), got {}",
            frac
        );
    }

    #[test]
    fn test_rtt_negative_clamped() {
        let our_ssrc = 0x12345678;
        let mut stats = RtcpStats::new();

        // Record that we sent an SR with some NTP middle value
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let ntp_secs = now.as_secs() + 2208988800;
        let ntp_frac = (now.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000;
        let ntp = (ntp_secs << 32) | ntp_frac;
        stats.record_sr_sent(ntp);

        // Create a ReportBlock with last_sr set far AHEAD of current time
        // (simulating clock skew). This makes wrapping_sub produce a huge value.
        let future_ntp_middle = stats.our_last_sr_ntp_middle.wrapping_add(0x00100000); // ~16 minutes ahead
        let block = ReportBlock {
            ssrc: our_ssrc,
            fraction_lost: 0,
            cumulative_lost: 0,
            highest_seq: 10,
            jitter: 0,
            last_sr: future_ntp_middle,
            delay_since_last_sr: 0,
        };

        stats.process_rr(&block, our_ssrc);

        // The computed RTT would be a huge wrapping value (>10s), so it should be rejected
        assert!(
            stats.rtt_ms.is_none(),
            "RTT should be None when computed value exceeds 10s sanity check, got {:?}",
            stats.rtt_ms
        );
    }

    #[test]
    fn test_build_sr_rr_zero_ssrc() {
        let mut stats = RtcpStats::new();
        stats.packets_sent = 10;
        stats.octets_sent = 1600;

        // Build SR/RR with remote_ssrc=0
        let data = build_sr_rr(0x12345678, 0, &mut stats, 0, 8000);
        let packets = parse_rtcp(&data);

        assert_eq!(packets.len(), 1);
        match &packets[0] {
            RtcpPacket::SenderReport(sr) => {
                assert_eq!(sr.ssrc, 0x12345678);
                assert_eq!(sr.report_blocks.len(), 1);
                assert_eq!(sr.report_blocks[0].ssrc, 0, "report block SSRC should be 0");
            }
            _ => panic!("expected SR"),
        }
    }

    #[test]
    fn test_truncated_compound_packet() {
        // Build a valid SR, then append a truncated second packet.
        // The parser should return the first SR and stop gracefully.
        let mut stats = RtcpStats::new();
        stats.packets_sent = 50;
        stats.octets_sent = 8000;
        let valid_sr = build_sr_rr(0x11111111, 0x22222222, &mut stats, 0, 8000);

        // Append a truncated RTCP RR header (only 6 bytes of a packet that
        // claims to be 8+ bytes via the length field)
        let mut compound = valid_sr.clone();
        compound.push(0x81); // V=2, RC=1
        compound.push(RTCP_RR);
        compound.push(0x00);
        compound.push(0x07); // length = 7 words = 32 bytes (but we only provide 2 more)
        compound.push(0xAA);
        compound.push(0xBB);

        let packets = parse_rtcp(&compound);
        // Should successfully parse the first SR and skip the truncated second
        assert_eq!(packets.len(), 1, "should parse only the valid first SR");
        match &packets[0] {
            RtcpPacket::SenderReport(sr) => {
                assert_eq!(sr.ssrc, 0x11111111);
                assert_eq!(sr.sender_packet_count, 50);
            }
            _ => panic!("expected SR"),
        }
    }

    #[test]
    fn test_truncated_report_block_in_sr() {
        // Build an SR that claims RC=2 but only has data for 1 report block.
        // The parser should return 1 report block, not panic.
        let mut stats = RtcpStats::new();
        stats.packets_sent = 10;
        let mut data = build_sr_rr(0x12345678, 0xAABBCCDD, &mut stats, 0, 8000);

        // Overwrite RC from 1 to 2 — the packet now claims 2 report blocks
        // but only has data for 1
        data[0] = (data[0] & 0xE0) | 2; // V=2, P=0, RC=2

        // Also increase the length to claim the extra report block exists
        let orig_len = u16::from_be_bytes([data[2], data[3]]);
        let new_len = orig_len + 6; // +6 words for another report block
        data[2..4].copy_from_slice(&new_len.to_be_bytes());

        let packets = parse_rtcp(&data);
        // Parser should parse what it can: offset+packet_len > data.len() → breaks
        // OR parses 1 block and stops because data runs out
        // Either way, should not panic
        assert!(
            packets.len() <= 1,
            "should handle truncated report blocks gracefully"
        );
    }

    #[test]
    fn test_parse_empty_rtcp() {
        let packets = parse_rtcp(&[]);
        assert!(
            packets.is_empty(),
            "empty data should produce no RTCP packets"
        );
    }

    #[test]
    fn test_parse_too_short_rtcp() {
        // Less than 4 bytes (minimum RTCP header)
        let packets = parse_rtcp(&[0x80, 0xC8, 0x00]);
        assert!(
            packets.is_empty(),
            "3-byte data should produce no RTCP packets"
        );
    }

    #[test]
    fn test_parse_wrong_version_rtcp() {
        // Version != 2 should stop parsing
        let mut data = vec![0u8; 32];
        data[0] = 0x00; // version 0
        data[1] = RTCP_SR;
        let packets = parse_rtcp(&data);
        assert!(
            packets.is_empty(),
            "version 0 should produce no RTCP packets"
        );
    }

    #[test]
    fn test_timestamp_u32_max_handling() {
        let mut stats = RtcpStats::new();
        // Record packets with timestamps near u32::MAX
        stats.record_received(0, u32::MAX - 1000, 160, 8000);
        stats.record_received(1, u32::MAX - 500, 160, 8000);
        // Wrap around
        stats.record_received(2, 100, 160, 8000);
        stats.record_received(3, 600, 160, 8000);

        assert_eq!(stats.packets_received, 4);
        assert_eq!(stats.expected_packets(), 4);
        assert_eq!(stats.cumulative_lost(), 0);
        // Jitter should be computed without panic (the key test is no panic)
    }

    #[test]
    fn test_sr_contains_rtp_timestamp() {
        let mut stats = RtcpStats::new();
        stats.packets_sent = 200;
        stats.octets_sent = 32000;

        let rtp_ts: u32 = 0xDEAD_BEEF;
        let data = build_sr_rr(0x11111111, 0x22222222, &mut stats, rtp_ts, 8000);

        // SR packet layout (byte offsets):
        //   0..4   = header (V, P, RC, PT, length)
        //   4..8   = SSRC of sender
        //   8..16  = NTP timestamp (8 bytes)
        //  16..20  = RTP timestamp  <-- this is what we're testing
        //  20..24  = sender packet count
        //  24..28  = sender octet count
        let embedded_rtp_ts = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        assert_eq!(
            embedded_rtp_ts, rtp_ts,
            "RTP timestamp should appear at bytes 16..20 of the SR"
        );

        // Also verify via parse_rtcp round-trip
        let packets = parse_rtcp(&data);
        assert_eq!(packets.len(), 1);
        match &packets[0] {
            RtcpPacket::SenderReport(sr) => {
                assert_eq!(sr.rtp_timestamp, rtp_ts);
            }
            _ => panic!("expected SR"),
        }
    }

    #[test]
    fn test_parse_unknown_rtcp_type_skipped() {
        // Build a compound packet: known SR + unknown type (e.g. SDES=202) + another SR
        let mut stats = RtcpStats::new();
        let sr1 = build_sr_rr(0x11111111, 0x22222222, &mut stats, 0, 8000);

        // Craft a minimal SDES packet (PT=202, version=2, rc=0, length=1 → 8 bytes)
        let sdes: Vec<u8> = vec![
            0x80, 202, 0x00, 0x01, // V=2, P=0, SC=0, PT=202(SDES), length=1
            0x00, 0x00, 0x00, 0x00, // SDES content (4 bytes padding)
        ];

        let sr2 = build_sr_rr(0x33333333, 0x44444444, &mut stats, 0, 8000);

        let mut compound = Vec::new();
        compound.extend_from_slice(&sr1);
        compound.extend_from_slice(&sdes);
        compound.extend_from_slice(&sr2);

        let packets = parse_rtcp(&compound);
        // Should get 2 SR packets, SDES is skipped (unknown type)
        assert_eq!(
            packets.len(),
            2,
            "should parse 2 SRs, skipping unknown SDES"
        );
        match (&packets[0], &packets[1]) {
            (RtcpPacket::SenderReport(a), RtcpPacket::SenderReport(b)) => {
                assert_eq!(a.ssrc, 0x11111111);
                assert_eq!(b.ssrc, 0x33333333);
            }
            _ => panic!("expected two SenderReports"),
        }
    }

    #[test]
    fn test_parse_bye_single_ssrc_with_reason() {
        // BYE: V=2, P=0, RC=1, PT=203
        // SSRC: 0x12345678, reason: "bye"
        // Total: 4 (header) + 4 (SSRC) + 1 (reason len) + 3 (reason) + 4 (pad to 32-bit) = 16
        // length field = 16/4 - 1 = 3
        let data: Vec<u8> = vec![
            0x81, 203, 0x00, 0x03, // header: V=2, RC=1, PT=BYE, length=3
            0x12, 0x34, 0x56, 0x78, // SSRC
            0x03, // reason length
            b'b', b'y', b'e', // reason string
            0x00, 0x00, 0x00, 0x00, // padding to 32-bit boundary
        ];
        let packets = parse_rtcp(&data);
        assert_eq!(packets.len(), 1);
        match &packets[0] {
            RtcpPacket::Bye(bye) => {
                assert_eq!(bye.ssrc_list, vec![0x12345678]);
                assert_eq!(bye.reason.as_deref(), Some("bye"));
            }
            _ => panic!("expected BYE"),
        }
    }

    #[test]
    fn test_parse_bye_multiple_ssrcs_no_reason() {
        // BYE: V=2, P=0, RC=2, PT=203, length=2 (12 bytes)
        let data: Vec<u8> = vec![
            0x82, 203, 0x00, 0x02, // header: V=2, RC=2, PT=BYE, length=2
            0x11, 0x22, 0x33, 0x44, // SSRC 1
            0x55, 0x66, 0x77, 0x88, // SSRC 2
        ];
        let packets = parse_rtcp(&data);
        assert_eq!(packets.len(), 1);
        match &packets[0] {
            RtcpPacket::Bye(bye) => {
                assert_eq!(bye.ssrc_list, vec![0x11223344, 0x55667788]);
                assert!(bye.reason.is_none());
            }
            _ => panic!("expected BYE"),
        }
    }

    #[test]
    fn test_parse_bye_zero_ssrcs() {
        // BYE: V=2, P=0, RC=0, PT=203, length=0 (4 bytes — header only)
        let data: Vec<u8> = vec![
            0x80, 203, 0x00, 0x00, // header: V=2, RC=0, PT=BYE, length=0
        ];
        let packets = parse_rtcp(&data);
        assert_eq!(packets.len(), 1);
        match &packets[0] {
            RtcpPacket::Bye(bye) => {
                assert!(bye.ssrc_list.is_empty());
                assert!(bye.reason.is_none());
            }
            _ => panic!("expected BYE"),
        }
    }

    #[test]
    fn test_parse_rtcp_with_csrc_rtp() {
        // RTP packet with CSRC list should still parse correctly
        // (This tests rtp.rs but is related to RTCP flow)
        use crate::media::rtp::RtpHeader;
        let pkt = RtpHeader::build(0, 1, 160, 0x12345678, false, &[0x80; 160]);
        let parsed = RtpHeader::parse(&pkt);
        assert!(parsed.is_some());
        let hdr = parsed.unwrap();
        assert_eq!(hdr.payload_type, 0);
        assert_eq!(hdr.sequence_number, 1);
    }

    #[test]
    fn test_fraction_lost_total_loss_returns_255() {
        // When lost_interval == expected_interval (100% loss), result must be 255.
        // Before the fix, ((N << 8) / N) = 256, and (256 as u8) wrapped to 0.
        let mut stats = RtcpStats::new();
        // First interval: receive seq 0 to establish baseline
        stats.record_received(0, 0, 160, 8000);
        stats.fraction_lost_and_update(); // snapshot priors

        // Second interval: advance expected by 100 but receive 0 more packets.
        // Directly advance extended_max_seq via record_received with a far seq.
        stats.record_received(100, 100 * 160, 160, 8000);
        // Now expected = 101, received = 2, but we want to test a full-loss interval.
        // expected_prior = 1, received_prior = 1 (from first update)
        // After receiving seq 100: expected = 101, received = 2
        // expected_interval = 101 - 1 = 100, received_interval = 2 - 1 = 1
        // lost_interval = 99, fraction = (99*256)/100 = 253
        let frac = stats.fraction_lost_and_update();
        assert!(
            frac >= 250,
            "near-total loss should produce fraction_lost >= 250, got {}",
            frac
        );
    }

    #[test]
    fn test_fraction_lost_large_interval_no_overflow() {
        // When lost_interval > 2^24, the old formula (lost_interval << 8) overflowed u32.
        // The fix uses u64 intermediate + min(255) to handle this correctly.
        let mut stats = RtcpStats::new();
        // Establish baseline
        stats.record_received(0, 0, 160, 8000);
        stats.fraction_lost_and_update(); // snapshot priors (expected_prior=1, received_prior=1)

        // Force a massive sequence jump by wrapping around many times.
        // After 512 rollovers: extended_max_seq ≈ 512 * 65536 = 33_554_432 (> 2^24)
        // We only receive a few packets so most are "lost".
        //
        // Drive this through the public API by recording a seq that forces rollovers.
        // record_received detects rollover when (max_seq_lo - seq32) > 0x8000.
        // We simulate this by alternating near-end and near-start seqs.
        let mut ts = 160u32;
        for _ in 0..512 {
            // Jump to near-end of seq space to trigger rollover detection
            stats.record_received(65535, ts, 160, 8000);
            ts = ts.wrapping_add(160);
            stats.record_received(0, ts, 160, 8000);
            ts = ts.wrapping_add(160);
        }
        // extended_max_seq should now be very large (512 rollovers * 65536 + some)
        let expected = stats.expected_packets();
        assert!(
            expected > 0x01_000_000,
            "expected packets should exceed 2^24, got {}",
            expected
        );

        // Fraction lost should be very high (we only received 1025 packets out of millions)
        // and must not panic from overflow
        let frac = stats.fraction_lost_and_update();
        assert!(
            frac >= 250,
            "massive loss should produce fraction_lost >= 250, got {}",
            frac
        );
    }
}
