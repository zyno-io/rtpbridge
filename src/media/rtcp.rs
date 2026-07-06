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
    // SSRC of the source whose SR populated last_sr_ntp/last_sr_received. The
    // LSR/DLSR we echo in an RR block are only meaningful for that SSRC, so
    // build_sr_rr emits them only when reporting on this same source — keeping
    // SR timing correct across SSRC changes and reordered/early SRs without
    // depending on RTP-vs-RTCP arrival ordering.
    last_sr_ssrc: Option<u32>,

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

    // Source SSRC of the current stream generation. A mid-call SSRC change
    // (a hold/re-INVITE that restarts the RTP stream with a fresh SSRC and
    // sequence base) re-baselines the sequence tracking above, so cumulative
    // loss doesn't balloon toward 100% from a stale baseline.
    current_ssrc: Option<u32>,
    // Loss folded in from prior SSRC generations, carried across re-baselines
    // so cumulative_lost() reflects whole-leg loss without cross-stream inflation.
    lost_accumulated: u32,
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
            last_sr_ssrc: None,
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
            current_ssrc: None,
            lost_accumulated: 0,
        }
    }

    /// Record an inbound RTP packet for stats (RFC 3550 A.3 sequence tracking).
    ///
    /// `ssrc` is the source SSRC of the packet. When it changes mid-stream
    /// (a hold/re-INVITE that restarts RTP with a fresh SSRC and sequence
    /// base), the prior generation's loss is folded into `lost_accumulated`
    /// and the per-generation sequence/jitter state is re-baselined. Without
    /// this, the stale baseline makes `expected_packets()` diverge wildly from
    /// `packets_received` and cumulative loss balloons toward 100%.
    pub fn record_received(
        &mut self,
        ssrc: u32,
        seq: u16,
        timestamp: u32,
        payload_len: usize,
        clock_rate: u32,
    ) {
        if self.current_ssrc != Some(ssrc) {
            if self.current_ssrc.is_some() {
                // Fold the ending generation's loss into the accumulator before
                // the reset, so whole-leg loss is preserved across the change.
                self.lost_accumulated = self
                    .lost_accumulated
                    .saturating_add(self.current_gen_lost());
            }
            self.current_ssrc = Some(ssrc);
            // Re-baseline sequence and jitter tracking for the new generation.
            self.seq_initialized = false;
            self.packets_received = 0;
            self.octets_received = 0;
            self.expected_prior = 0;
            self.received_prior = 0;
            // Jitter is per-source: clear both the smoothed value and the
            // transit baseline so the new generation starts fresh rather than
            // inheriting (and slowly decaying) the old stream's jitter.
            self.last_transit = 0;
            self.jitter = 0;
            // last_sr_* is NOT cleared here: it's keyed by last_sr_ssrc and
            // gated in build_sr_rr, so the old source's SR timing can never
            // leak into the new SSRC's report block regardless of ordering.
        }

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
                // RFC 3550 A.8: J += (|D| - J) / 16. Must be signed — when |D| < J the
                // term is negative and pulls J downward. Doing this with u32 wrapping_sub
                // wraps to ~UINT32_MAX/16 and J diverges to ~UINT32_MAX every time |D|
                // dips below J.
                let delta = (d as i64 - self.jitter as i64) >> 4;
                self.jitter = (self.jitter as i64 + delta).max(0) as u32;
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

    /// Lost packets within the current SSRC generation (RFC 3550 A.3).
    fn current_gen_lost(&self) -> u32 {
        self.expected_packets()
            .saturating_sub(self.packets_received)
    }

    /// Cumulative packets lost across the whole leg, including loss carried
    /// forward from prior SSRC generations (see `record_received`).
    pub fn cumulative_lost(&self) -> u32 {
        self.lost_accumulated
            .saturating_add(self.current_gen_lost())
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
        self.last_sr_ssrc = Some(sr.ssrc);
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
    // RFC 3550: the report block's cumulative-lost is scoped to the SSRC in
    // this block (the current generation), not the whole-leg total that the
    // session stats event reports via `cumulative_lost()`.
    let cum_lost = stats.current_gen_lost() & 0x00FFFFFF;
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
    // Last SR / Delay since last SR — only meaningful when our most recent SR
    // came from the SSRC we're reporting on. After an SSRC change (or a
    // straggler/early SR from a different source) the stored timing belongs to
    // another source, so we emit 0 ("no SR received yet from this source")
    // rather than a mismatched LSR that would make the peer compute a bogus RTT.
    let (last_sr, dlsr) = if stats.last_sr_ssrc == Some(remote_ssrc) {
        let dlsr = stats
            .last_sr_received
            .map(|t| {
                let elapsed = t.elapsed();
                ((elapsed.as_secs().min(0xFFFF) as u32) << 16)
                    | (elapsed.subsec_micros() as u64 * 65536 / 1_000_000) as u32
            })
            .unwrap_or(0);
        (stats.last_sr_ntp, dlsr)
    } else {
        (0, 0)
    };
    buf.extend_from_slice(&last_sr.to_be_bytes());
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
#[path = "rtcp_tests.rs"]
mod tests;
