use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Derive the RTCP address from an RTP address (same IP, port + 1).
/// Returns None if the RTP port is 65535 (overflow).
fn rtcp_addr_from_rtp(rtp_addr: SocketAddr) -> Option<SocketAddr> {
    rtp_addr
        .port()
        .checked_add(1)
        .map(|p| SocketAddr::new(rtp_addr.ip(), p))
}

use super::endpoint::{EndpointConfig, InboundPacket, RoutedRtpPacket};
use super::stats::{EndpointStats, RawRecvCounters};
use crate::control::protocol::{
    EndpointDirection, EndpointDirectionUpdate, EndpointId, EndpointState,
};
use crate::media::rtcp::{self, RtcpStats};
use crate::media::rtp::RtpHeader;
use crate::media::sdp::{self, SdpCodec, SdpCrypto};
use crate::media::srtp::{SrtcpContext, SrtpContext};
use crate::metrics::Metrics;
use crate::net::socket_pool::SocketPair;

/// A plain RTP (optionally SRTP) endpoint
pub struct RtpEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,
    /// Wire-level inbound datagram counters: ALL datagrams the RTP and RTCP
    /// sockets deliver, before parse/decrypt. Shared with the recv tasks, which
    /// increment it. Diverging from `stats.inbound_*` (validated RTP media
    /// only) while this climbs means the peer's path is alive but producing no
    /// media — for plain RTP, RTCP keepalives keep this moving during silence.
    pub raw_recv: Arc<RawRecvCounters>,

    pub rtp_socket: Arc<UdpSocket>,
    pub rtcp_socket: Arc<UdpSocket>,
    pub local_rtp_addr: SocketAddr,
    pub remote_rtp_addr: Option<SocketAddr>,
    pub remote_rtcp_addr: Option<SocketAddr>,

    /// Our SSRC for outgoing RTP
    pub our_ssrc: u32,
    /// Remote SSRC (learned from first received packet)
    pub remote_ssrc: Option<u32>,

    /// Negotiated codecs
    pub codecs: Vec<SdpCodec>,
    /// The codec we're currently sending
    pub send_codec: Option<SdpCodec>,
    /// Clock rate of the inbound (receive) audio codec, for RTCP jitter calculation
    recv_clock_rate: u32,
    /// Telephone-event payload type (for DTMF)
    pub telephone_event_pt: Option<u8>,
    /// Negotiated telephone-event (RFC 4733) clock. DTMF event durations and
    /// timestamps are expressed in this clock, independent of the audio codec
    /// clock (`send_codec`). Defaults to the 8000 SIP convention.
    pub telephone_event_clock_rate: u32,

    /// RTCP statistics
    pub rtcp_stats: RtcpStats,
    /// Last time we sent RTCP
    pub last_rtcp_sent: Instant,

    /// Outgoing sequence number
    pub seq_no: u16,
    /// Last outbound RTP timestamp (for RTCP SR). This is the wire timestamp,
    /// owned by this destination — not a copy of a source packet's timestamp.
    pub last_rtp_timestamp: u32,

    /// Destination-owned outbound RTP timestamp timeline. Set after the first
    /// successful write_rtp so subsequent packets can advance from a known
    /// reference instead of inheriting whatever the source last sent.
    last_outbound_ts: Option<u32>,
    /// Source endpoint of the last packet we wrote. Used to detect source
    /// changes (mixer↔passthrough, file↔normal, source A↔source B) so we
    /// can preserve the destination's RTP timeline across the switch.
    last_source_id: Option<EndpointId>,
    /// Source's RTP timestamp at the last packet we wrote (matched to
    /// `last_source_id`). Lets us compute a delta within the same source.
    last_source_ts: Option<u32>,
    /// Smoothed packet-duration estimate in RTP timestamp units, learned
    /// from recent same-source deltas. Used as the bump on source changes
    /// or discontinuity clamps. Falls back to `clock_rate / 50` (20ms) if
    /// no sane samples have been seen yet.
    learned_step: Option<u32>,
    /// Constant wire timestamp for the in-progress RFC 4733 telephone-event
    /// (DTMF) event. All packets of one event share a single timestamp, so we
    /// anchor it once on the marker packet and hold it across the redundant
    /// and continuation packets — even while normal audio is interleaved on
    /// the same SSRC. Re-anchored on each new event (next marker packet).
    dtmf_wire_ts: Option<u32>,

    /// SRTP context for outgoing packets (None = plain RTP)
    srtp_tx: Option<SrtpContext>,
    /// SRTP context for incoming packets
    srtp_rx: Option<SrtpContext>,

    /// New SRTP RX context during rekey transition period
    srtp_rx_new: Option<SrtpContext>,
    /// When to force-switch to the new SRTP RX context
    rekey_switchover: Option<Instant>,
    /// Base64 SDES key currently installed in srtp_rx — used to skip spurious
    /// rekeys when a re-INVITE echoes back the same key (otherwise a fresh
    /// context with ROC=0 would overwrite the live one with valid ROC state).
    srtp_rx_key_b64: Option<String>,
    /// Base64 SDES key currently installed in srtp_tx — preserved so we can
    /// re-emit the same key in a re-INVITE answer (peer is still using it to
    /// decrypt our packets, so the answer must repeat it).
    srtp_tx_key_b64: Option<String>,

    /// SRTCP context for outgoing RTCP (None = plain RTCP)
    srtcp_tx: Option<SrtcpContext>,
    /// SRTCP context for incoming RTCP
    srtcp_rx: Option<SrtcpContext>,
    /// New SRTCP RX context during rekey transition period
    srtcp_rx_new: Option<SrtcpContext>,

    /// Whether rtcp-mux was negotiated (RTCP on same port as RTP)
    pub rtcp_mux: bool,

    /// Symmetric RTP: time window for address learning (seconds)
    addr_learn_window_secs: u64,
    /// When the endpoint was created (for address learning window)
    created_at: Instant,
    /// Whether the remote address has been locked (learning window expired)
    addr_locked: bool,

    /// Direction control mode:
    /// - Auto: follow SDP direction from re-INVITEs (endpoint.rtp.reinvite)
    /// - Manual: explicit override from endpoint.update_direction
    direction_auto: bool,
    /// Most recently advertised remote SDP direction mapped into local direction.
    last_remote_direction: Option<EndpointDirection>,

    /// Cancellation token for cooperative recv task shutdown
    cancel_token: CancellationToken,
    /// Recv task handles
    recv_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl RtpEndpoint {
    pub fn new(id: EndpointId, direction: EndpointDirection, socket_pair: SocketPair) -> Self {
        Self {
            id,
            config: EndpointConfig { direction },
            state: EndpointState::New,
            stats: EndpointStats::new(),
            raw_recv: Arc::new(RawRecvCounters::default()),
            rtp_socket: Arc::new(socket_pair.rtp_socket),
            rtcp_socket: Arc::new(socket_pair.rtcp_socket),
            local_rtp_addr: socket_pair.rtp_addr,

            remote_rtp_addr: None,
            remote_rtcp_addr: None,
            our_ssrc: rand::random(),
            remote_ssrc: None,
            codecs: Vec::new(),
            send_codec: None,
            recv_clock_rate: 8000,
            telephone_event_pt: None,
            telephone_event_clock_rate: 8000,
            rtcp_stats: RtcpStats::new(),
            last_rtcp_sent: Instant::now(),
            seq_no: rand::random(),
            last_rtp_timestamp: 0,
            last_outbound_ts: None,
            last_source_id: None,
            last_source_ts: None,
            learned_step: None,
            dtmf_wire_ts: None,
            srtp_tx: None,
            srtp_rx: None,
            srtp_rx_new: None,
            rekey_switchover: None,
            srtp_rx_key_b64: None,
            srtp_tx_key_b64: None,
            srtcp_tx: None,
            srtcp_rx: None,
            srtcp_rx_new: None,
            rtcp_mux: false,
            addr_learn_window_secs: 5,
            created_at: Instant::now(),
            addr_locked: false,
            // Auto mode tracks remote SDP direction. An explicit non-default
            // direction at creation is treated as a manual override, so the
            // user's choice survives the initial offer/answer exchange.
            direction_auto: matches!(direction, EndpointDirection::SendRecv),
            last_remote_direction: None,
            cancel_token: CancellationToken::new(),
            recv_tasks: Vec::new(),
        }
    }

    pub fn set_direction_override(&mut self, update: EndpointDirectionUpdate) {
        if let Some(dir) = update.as_direction() {
            self.config.direction = dir;
            self.direction_auto = false;
        } else {
            self.direction_auto = true;
            if let Some(dir) = self.last_remote_direction {
                self.config.direction = dir;
            }
        }
    }

    /// Rotate the outbound SSRC and reset the outbound RTP timeline.
    ///
    /// Used on hold→unhold transitions to give the receiver a fresh stream to
    /// re-anchor its jitter buffer against. Per RFC 3550, a new SSRC SHOULD
    /// start with random seq/timestamp bases, so the peer treats it as an
    /// independent stream rather than a continuation of the prior timeline.
    /// `last_source_id`/`last_source_ts` are also cleared so the next outbound
    /// packet seeds from the new random anchor with a marker bit set (via
    /// `advance_outbound_timeline`'s "first packet" arm).
    pub fn bump_outbound_ssrc(&mut self) {
        let prev = self.our_ssrc;
        self.our_ssrc = rand::random();
        self.seq_no = rand::random();
        let new_ts: u32 = rand::random();
        self.last_outbound_ts = None;
        self.last_source_id = None;
        self.last_source_ts = None;
        self.dtmf_wire_ts = None;
        self.last_rtp_timestamp = new_ts;

        // A new SSRC starts a fresh SRTP cryptographic context on the wire: the
        // peer (re-)initialises its per-SSRC rollover counter (ROC) at 0 when it
        // first sees the new SSRC. With per-SSRC TX state (RFC 3711 §3.2.1) our
        // new SSRC already starts at ROC 0 on its own; we still clear the TX map
        // here so the retired SSRC's now-unused state doesn't accumulate across
        // repeated rotations. Mirrors the RX-side `reset_sequence_state()` done
        // in `update_remote_sdp`.
        //
        // (Historically this was load-bearing: when ROC was keyed globally, the
        // stale `highest_seq` could spuriously bump ROC on the new SSRC's first
        // packet so the auth tag — which covers the ROC — failed at the peer,
        // making EVERY outbound SRTP packet drop. That was the one-way-audio seen
        // after a hold long enough to drive the direction back through
        // recvonly→sendrecv. Per-SSRC state removes that failure mode; this reset
        // is now hygiene, not correctness.)
        //
        // SRTCP TX index is intentionally NOT reset: the index travels on the
        // wire and the IV is keyed by (ssrc, wire-index), so a new SSRC stays
        // decryptable, and keeping the index monotonic avoids tripping the
        // peer's SRTCP replay window.
        if let Some(ref mut tx) = self.srtp_tx {
            tx.reset_sequence_state();
        }

        debug!(
            endpoint_id = %self.id,
            prev_ssrc = prev,
            new_ssrc = self.our_ssrc,
            "RTP outbound SSRC rotated"
        );
    }

    /// Start recv tasks for RTP and RTCP sockets
    pub fn start_recv_tasks(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        // RTP recv task
        let rtp_socket = Arc::clone(&self.rtp_socket);
        let endpoint_id = self.id;
        let tx = packet_tx.clone();
        let token = self.cancel_token.clone();
        let raw_recv = Arc::clone(&self.raw_recv);
        self.recv_tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let exit_reason;
            loop {
                tokio::select! {
                    result = rtp_socket.recv_from(&mut buf) => {
                        match result {
                            Ok((n, source)) => {
                                // Wire-level count: every datagram on the RTP
                                // socket, before parse/decrypt drops invalid or
                                // non-RTP traffic.
                                raw_recv.record(n);
                                let packet = InboundPacket {
                                    endpoint_id,
                                    source,
                                    data: buf[..n].to_vec(),
                                    is_rtcp: false,
                                    local: None,
                                };
                                if tx.send(packet).await.is_err() {
                                    exit_reason = "session channel closed";
                                    break;
                                }
                            }
                            Err(e) => {
                                // ECONNREFUSED can arrive on Linux when a previous
                                // send_to triggered ICMP port-unreachable.  This is
                                // transient and must not kill the recv task.
                                debug!(endpoint_id = %endpoint_id, error = %e, "RTP recv transient error, continuing");
                                continue;
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        exit_reason = "cancelled";
                        break;
                    }
                }
            }
            let refs = Arc::strong_count(&rtp_socket);
            info!(endpoint_id = %endpoint_id, reason = exit_reason, arc_refs = refs, "RTP recv task exiting");
        }));

        // RTCP recv task
        let rtcp_socket = Arc::clone(&self.rtcp_socket);
        let endpoint_id = self.id;
        let token = self.cancel_token.clone();
        let raw_recv = Arc::clone(&self.raw_recv);
        self.recv_tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let exit_reason;
            loop {
                tokio::select! {
                    result = rtcp_socket.recv_from(&mut buf) => {
                        match result {
                            Ok((n, source)) => {
                                // Count RTCP datagrams into the same wire-level
                                // tally: for plain RTP these are the peer's
                                // liveness keepalives during media silence.
                                raw_recv.record(n);
                                let packet = InboundPacket {
                                    endpoint_id,
                                    source,
                                    data: buf[..n].to_vec(),
                                    is_rtcp: true,
                                    local: None,
                                };
                                if packet_tx.send(packet).await.is_err() {
                                    exit_reason = "session channel closed";
                                    break;
                                }
                            }
                            Err(e) => {
                                debug!(endpoint_id = %endpoint_id, error = %e, "RTCP recv transient error, continuing");
                                continue;
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        exit_reason = "cancelled";
                        break;
                    }
                }
            }
            debug!(endpoint_id = %endpoint_id, reason = exit_reason, "RTCP recv task exiting");
        }));
    }

    /// Create from a remote SDP offer, returning our SDP answer.
    /// `socket_pair` is consumed here. On SRTP init failure, the sockets are dropped
    /// (ports released back to OS). The SocketPool wraps its counter so these ports
    /// will be available for future allocations.
    pub fn from_offer(
        id: EndpointId,
        direction: EndpointDirection,
        offer_sdp: &str,
        socket_pair: SocketPair,
        bind_ip: std::net::IpAddr,
        packet_tx: mpsc::Sender<InboundPacket>,
    ) -> anyhow::Result<(Self, String)> {
        let parsed = sdp::parse_sdp(offer_sdp);

        let mut endpoint = Self::new(id, direction, socket_pair);

        if let Some(dir) = parsed
            .direction
            .as_deref()
            .and_then(endpoint_direction_from_sdp)
        {
            endpoint.last_remote_direction = Some(dir);
            if endpoint.direction_auto {
                endpoint.config.direction = dir;
            }
        }

        // Set remote address from SDP
        endpoint.remote_rtp_addr = parsed.remote_addr;
        endpoint.rtcp_mux = parsed.rtcp_mux;
        if let Some(addr) = parsed.remote_addr {
            if parsed.rtcp_mux {
                endpoint.remote_rtcp_addr = Some(addr);
            } else {
                endpoint.remote_rtcp_addr = rtcp_addr_from_rtp(addr);
            }
        }

        endpoint.telephone_event_pt = parsed.telephone_event_pt;
        endpoint.telephone_event_clock_rate = parsed.telephone_event_clock_rate.unwrap_or(8000);

        // Pick the highest-quality offered codec as our send codec (rather than
        // the offerer's first-listed preference), and learn the receive clock rate.
        endpoint.send_codec = crate::media::sdp::select_answer_codec(&parsed.codecs).cloned();
        endpoint.recv_clock_rate = endpoint
            .send_codec
            .as_ref()
            .map(|c| c.clock_rate)
            .unwrap_or(8000);

        // Commit to a single media codec: keep only the selected codec plus the
        // offered telephone-event. This endpoint decodes ALL inbound media as
        // send_codec (see endpoint_audio_codec) — advertising other audio codecs
        // the peer could then send would cause them to be misdecoded. Trimming the
        // set here makes every answer we generate (initial and re-INVITE) advertise
        // exactly the selected codec.
        endpoint.codecs = endpoint
            .send_codec
            .iter()
            .cloned()
            .chain(
                parsed
                    .codecs
                    .iter()
                    .filter(|c| c.name == "telephone-event")
                    .cloned(),
            )
            .collect();

        // Set up SRTP/SRTCP if crypto was offered.
        // RX uses the offerer's key; TX uses an independently generated key
        // to prevent keystream reuse between directions.
        let answer_crypto = if let Some(ref crypto) = parsed.crypto {
            endpoint.srtp_rx = Some(
                SrtpContext::from_sdes_key(&crypto.key_b64)
                    .map_err(|e| anyhow::anyhow!("SRTP RX init failed: {e}"))?,
            );
            endpoint.srtcp_rx = Some(
                SrtcpContext::from_sdes_key(&crypto.key_b64)
                    .map_err(|e| anyhow::anyhow!("SRTCP RX init failed: {e}"))?,
            );
            endpoint.srtp_rx_key_b64 = Some(crypto.key_b64.clone());

            // Generate independent TX key
            let mut answer_key_bytes = [0u8; 30];
            for b in answer_key_bytes.iter_mut() {
                *b = rand::random();
            }
            let answer_key_b64 = crate::media::srtp::base64_encode(&answer_key_bytes);

            endpoint.srtp_tx = Some(
                SrtpContext::from_sdes_key(&answer_key_b64)
                    .map_err(|e| anyhow::anyhow!("SRTP TX init failed: {e}"))?,
            );
            endpoint.srtcp_tx = Some(
                SrtcpContext::from_sdes_key(&answer_key_b64)
                    .map_err(|e| anyhow::anyhow!("SRTCP TX init failed: {e}"))?,
            );
            endpoint.srtp_tx_key_b64 = Some(answer_key_b64.clone());

            Some(SdpCrypto {
                tag: crypto.tag,
                suite: crypto.suite.clone(),
                key_b64: answer_key_b64,
            })
        } else {
            None
        };

        // Generate SDP answer from the trimmed codec set (selected codec +
        // telephone-event), send_codec first so the offerer transmits the codec
        // we selected (RFC 3264 — the answerer's first listed PT is what the
        // offerer's device sends), matching our send_codec and recv_clock_rate.
        let mut answer_codecs: Vec<&SdpCodec> = Vec::new();
        if let Some(ref send) = endpoint.send_codec {
            answer_codecs.push(send);
        }
        for c in endpoint.codecs.iter() {
            if matches!(endpoint.send_codec, Some(ref send) if send.pt == c.pt) {
                continue;
            }
            answer_codecs.push(c);
        }
        let answer = sdp::generate_sdp_answer(
            SocketAddr::new(bind_ip, endpoint.local_rtp_addr.port()),
            endpoint.local_rtp_addr.port(),
            &answer_codecs,
            answer_crypto.as_ref(),
            id.as_u128() as u64,
        );

        endpoint.state = EndpointState::Connected;
        endpoint.start_recv_tasks(packet_tx);

        Ok((endpoint, answer))
    }

    /// Create an SDP offer
    pub fn create_offer(
        id: EndpointId,
        direction: EndpointDirection,
        socket_pair: SocketPair,
        bind_ip: std::net::IpAddr,
        codecs: &[SdpCodec],
        srtp: bool,
        packet_tx: mpsc::Sender<InboundPacket>,
    ) -> anyhow::Result<(Self, String)> {
        let mut endpoint = Self::new(id, direction, socket_pair);
        endpoint.codecs = codecs.to_vec();
        endpoint.send_codec = codecs.iter().find(|c| c.name != "telephone-event").cloned();
        let te_codec = codecs.iter().find(|c| c.name == "telephone-event");
        endpoint.telephone_event_pt = te_codec.map(|c| c.pt);
        // Provisional until the answer; we advertise telephone-event at this
        // clock (8000 for a default offer) and finalize in accept_answer.
        endpoint.telephone_event_clock_rate = te_codec.map_or(8000, |c| c.clock_rate);

        // Generate crypto if SRTP requested
        let crypto = if srtp {
            // Generate a random 30-byte key (128-bit master key + 112-bit salt)
            let mut key_bytes = [0u8; 30];
            for b in key_bytes.iter_mut() {
                *b = rand::random();
            }
            let b64_encoded = base64_encode(&key_bytes);
            // Init TX contexts with the local key. RX contexts stay None until
            // accept_answer provides the remote peer's independent key.
            endpoint.srtp_tx = Some(
                SrtpContext::from_sdes_key(&b64_encoded)
                    .map_err(|e| anyhow::anyhow!("SRTP TX init failed: {e}"))?,
            );
            endpoint.srtcp_tx = Some(
                SrtcpContext::from_sdes_key(&b64_encoded)
                    .map_err(|e| anyhow::anyhow!("SRTCP TX init failed: {e}"))?,
            );
            endpoint.srtp_tx_key_b64 = Some(b64_encoded.clone());
            Some(SdpCrypto {
                tag: 1,
                suite: "AES_CM_128_HMAC_SHA1_80".to_string(),
                key_b64: b64_encoded,
            })
        } else {
            None
        };

        let offer_codecs: Vec<&SdpCodec> = endpoint.codecs.iter().collect();
        let offer = sdp::generate_sdp_offer(
            SocketAddr::new(bind_ip, endpoint.local_rtp_addr.port()),
            endpoint.local_rtp_addr.port(),
            &offer_codecs,
            crypto.as_ref(),
            id.as_u128() as u64,
        );

        endpoint.state = EndpointState::Connecting;
        endpoint.start_recv_tasks(packet_tx);

        Ok((endpoint, offer))
    }

    /// Accept a remote SDP answer
    /// Reject a remote SDP whose connection address family differs from this
    /// endpoint's already-bound local RTP socket family. The socket is bound to a
    /// single family for its lifetime (we don't migrate sockets), so sending to
    /// the other family would fail at runtime. Returns `Ok(())` when the remote
    /// has no address or matches the bound family.
    fn reject_family_change(&self, remote: Option<SocketAddr>) -> anyhow::Result<()> {
        if let Some(remote) = remote {
            let bound_is_v6 = self.local_rtp_addr.is_ipv6();
            if remote.is_ipv6() != bound_is_v6 {
                anyhow::bail!(
                    "remote SDP address family ({}) does not match this endpoint's bound \
                     media family ({}); socket migration across address families is not supported",
                    if remote.is_ipv6() { "IPv6" } else { "IPv4" },
                    if bound_is_v6 { "IPv6" } else { "IPv4" },
                );
            }
        }
        Ok(())
    }

    pub fn accept_answer(&mut self, answer_sdp: &str) -> anyhow::Result<()> {
        let parsed = sdp::parse_sdp(answer_sdp);

        // Reject an address-family change before mutating any state: our RTP
        // socket is already bound to one family, and we don't migrate sockets, so
        // a v4↔v6 flip would leave us unable to reach the remote. Bail here (no
        // partial mutation) rather than half-applying the SDP.
        self.reject_family_change(parsed.remote_addr)?;

        if let Some(dir) = parsed
            .direction
            .as_deref()
            .and_then(endpoint_direction_from_sdp)
        {
            self.last_remote_direction = Some(dir);
            if self.direction_auto {
                self.config.direction = dir;
            }
        }

        self.remote_rtp_addr = parsed.remote_addr;
        if parsed.rtcp_mux {
            self.rtcp_mux = true;
        }
        if let Some(addr) = parsed.remote_addr {
            if self.rtcp_mux {
                self.remote_rtcp_addr = Some(addr);
            } else {
                self.remote_rtcp_addr = rtcp_addr_from_rtp(addr);
            }
        }

        // Update codecs from answer — only accept codecs that were in our offer.
        // self.codecs contains the offered set (set during from_offer or create_offer).
        if !parsed.codecs.is_empty() {
            let offered: std::collections::HashSet<String> = self
                .codecs
                .iter()
                .map(|c| c.name.to_ascii_uppercase())
                .collect();
            let valid: Vec<_> = parsed
                .codecs
                .iter()
                .filter(|c| offered.contains(&c.name.to_ascii_uppercase()))
                .cloned()
                .collect();
            if valid.is_empty() && !offered.is_empty() {
                anyhow::bail!("SDP answer contains no codecs from the original offer");
            }
            self.codecs = valid;
            self.send_codec = self
                .codecs
                .iter()
                .find(|c| c.name != "telephone-event")
                .cloned();
        }

        self.telephone_event_pt = parsed.telephone_event_pt;
        // Finalize the negotiated DTMF clock from the answer (defaulting to the
        // 8000 SIP convention if the peer answered without a telephone-event).
        self.telephone_event_clock_rate = parsed.telephone_event_clock_rate.unwrap_or(8000);
        // Update receive clock rate from the answer's codec
        if let Some(ref sc) = self.send_codec {
            self.recv_clock_rate = sc.clock_rate;
        }

        // Set up SRTP/SRTCP from answer crypto if present.
        // RX contexts use the answer's key (remote peer's encrypt key).
        if let Some(ref crypto) = parsed.crypto {
            if self.srtp_rx.is_none() {
                // Initial setup: set RX directly
                self.srtp_rx = Some(
                    SrtpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTP RX init failed: {e}"))?,
                );
                self.srtcp_rx = Some(
                    SrtcpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTCP RX init failed: {e}"))?,
                );
                self.srtp_rx_key_b64 = Some(crypto.key_b64.clone());
            } else if self.srtp_rx_key_b64.as_deref() != Some(crypto.key_b64.as_str()) {
                // Rekey: key actually changed, set as pending RX with dual-context switchover
                self.srtp_rx_new = Some(
                    SrtpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTP RX rekey failed: {e}"))?,
                );
                self.srtcp_rx_new = Some(
                    SrtcpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTCP RX rekey failed: {e}"))?,
                );
                self.rekey_switchover = Some(Instant::now() + Duration::from_secs(5));
                self.srtp_rx_key_b64 = Some(crypto.key_b64.clone());
                debug!(endpoint_id = %self.id, "SRTP RX rekey: dual-context transition started (5s)");
            }
            // else: same key as before — don't touch the live SRTP context (would wipe ROC state)

            if self.srtp_tx.is_none() {
                // Edge case: offer had no SRTP but answer provides crypto.
                // Generate an independent TX key to avoid keystream reuse.
                let mut tx_key_bytes = [0u8; 30];
                for b in tx_key_bytes.iter_mut() {
                    *b = rand::random();
                }
                let tx_key_b64 = base64_encode(&tx_key_bytes);
                self.srtp_tx = Some(
                    SrtpContext::from_sdes_key(&tx_key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTP TX init failed: {e}"))?,
                );
                self.srtcp_tx = Some(
                    SrtcpContext::from_sdes_key(&tx_key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTCP TX init failed: {e}"))?,
                );
                self.srtp_tx_key_b64 = Some(tx_key_b64);
                warn!(endpoint_id = %self.id,
                    "SRTP TX key generated in accept_answer fallback — \
                     remote peer may not know this key since it was not in the original offer");
            }
        }

        if self.remote_rtp_addr.is_none() {
            anyhow::bail!("SDP answer has no connection address");
        }

        self.state = EndpointState::Connected;

        // Re-anchor the symmetric-RTP learning window to answer time. As the
        // offerer we don't know the peer's address (or get any media) until the
        // answer arrives, which for a ringing phone can be many seconds after
        // this endpoint was created — long enough that a window anchored at
        // creation would already be closed, locking us to the (often private,
        // NAT'd) SDP address and never latching the real source. `update_remote_sdp`
        // resets the window for the same reason on re-INVITE; this also lets a
        // post-rekey answer (which overwrites `remote_rtp_addr` from SDP above)
        // re-latch the live source.
        self.reset_addr_lock();

        Ok(())
    }

    /// Like `accept_answer` but only updates address and SRTP state — codecs
    /// are intentionally left untouched.  Used for re-INVITE hold/unhold where
    /// the phone may advertise a different codec list or payload-type mapping
    /// that would corrupt the endpoint's send_codec if blindly accepted.
    ///
    /// Returns an answer SDP describing the endpoint's current state. The
    /// codec list is reordered so that `send_codec` is listed first — this is
    /// important for phones (e.g. Grandstream GXP21xx) that re-derive their
    /// outbound codec from the first PT in the answer; without reordering they
    /// would switch to the offerer's preferred codec while we keep sending the
    /// originally-negotiated one, causing one-way audio after hold/unhold.
    pub fn update_remote_sdp(&mut self, sdp: &str) -> anyhow::Result<String> {
        let parsed = sdp::parse_sdp(sdp);

        // Reject a re-INVITE/re-negotiation that flips the address family before
        // mutating any state — the bound socket can't reach the other family and
        // we don't migrate sockets. Bail cleanly instead of half-applying.
        self.reject_family_change(parsed.remote_addr)?;

        // Apply remote SDP direction by default unless an explicit manual
        // override is active via endpoint.update_direction.
        if let Some(dir) = parsed
            .direction
            .as_deref()
            .and_then(endpoint_direction_from_sdp)
        {
            self.last_remote_direction = Some(dir);
            if self.direction_auto {
                self.config.direction = dir;
            }
        }

        // Update remote address
        self.remote_rtp_addr = parsed.remote_addr;
        if parsed.rtcp_mux {
            self.rtcp_mux = true;
        }
        if let Some(addr) = parsed.remote_addr {
            if self.rtcp_mux {
                self.remote_rtcp_addr = Some(addr);
            } else {
                self.remote_rtcp_addr = rtcp_addr_from_rtp(addr);
            }
        }

        // Handle SRTP rekey if crypto changed
        if let Some(ref crypto) = parsed.crypto {
            if self.srtp_rx.is_none() {
                self.srtp_rx = Some(
                    SrtpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTP RX init failed: {e}"))?,
                );
                self.srtcp_rx = Some(
                    SrtcpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTCP RX init failed: {e}"))?,
                );
                self.srtp_rx_key_b64 = Some(crypto.key_b64.clone());
            } else if self.srtp_rx_key_b64.as_deref() != Some(crypto.key_b64.as_str()) {
                self.srtp_rx_new = Some(
                    SrtpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTP RX rekey failed: {e}"))?,
                );
                self.srtcp_rx_new = Some(
                    SrtcpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTCP RX rekey failed: {e}"))?,
                );
                self.rekey_switchover = Some(Instant::now() + Duration::from_secs(5));
                self.srtp_rx_key_b64 = Some(crypto.key_b64.clone());
                debug!(endpoint_id = %self.id, "SRTP RX rekey via update_remote_sdp: dual-context transition started (5s)");
            } else {
                // Same key — keep the derived session keys, but reset the RX sequence /
                // replay window. Phones (e.g. Grandstream GXP21xx) commonly send RTCP BYE
                // on hold and resume with a new SSRC + reset RTP seq number on unhold.
                // The old replay_window/highest_seq would reject those low-seq packets as
                // "too old", silently dropping every post-resume packet until the seq #
                // climbed back above the window. The cipher_key/auth_key derivation stays
                // intact so we can still decrypt — only the per-stream tracking is reset.
                if let Some(ref mut rx) = self.srtp_rx {
                    rx.reset_sequence_state();
                }
                if let Some(ref mut rx) = self.srtcp_rx {
                    rx.reset_recv_state();
                }
                debug!(
                    endpoint_id = %self.id,
                    "SRTP RX same-key re-INVITE: reset sequence/replay state for restarted peer stream"
                );
            }

            if self.srtp_tx.is_none() {
                let mut tx_key_bytes = [0u8; 30];
                for b in tx_key_bytes.iter_mut() {
                    *b = rand::random();
                }
                let tx_key_b64 = base64_encode(&tx_key_bytes);
                self.srtp_tx = Some(
                    SrtpContext::from_sdes_key(&tx_key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTP TX init failed: {e}"))?,
                );
                self.srtcp_tx = Some(
                    SrtcpContext::from_sdes_key(&tx_key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTCP TX init failed: {e}"))?,
                );
                self.srtp_tx_key_b64 = Some(tx_key_b64);
                warn!(endpoint_id = %self.id,
                    "SRTP TX key generated in update_remote_sdp fallback — \
                     remote peer may not know this key since it was not in the original offer");
            }
        }

        // Reset address lock so symmetric RTP can learn the new source
        self.reset_addr_lock();

        // Forget the remote SSRC so it's relearned from the next inbound packet.
        // Phones often switch SSRC across hold/unhold (and always do after RTCP BYE),
        // so trusting the old SSRC for outbound RTCP reports would be wrong. The
        // remote_ssrc.is_none() gate in write_rtp() also doubles as a NAT-safety
        // check: we won't send outbound media to the new address until we've
        // confirmed the peer is actually there by receiving a packet from them.
        self.remote_ssrc = None;

        // Build the answer SDP. send_codec goes first so the peer sends us the
        // codec we're already sending (RFC 3264 — answerer's first listed PT
        // is what the offerer's device transmits).
        let bind_ip = self.local_rtp_addr.ip();
        let mut answer_codecs: Vec<&SdpCodec> = Vec::new();
        if let Some(ref send) = self.send_codec {
            answer_codecs.push(send);
        }
        for c in self.codecs.iter() {
            if matches!(self.send_codec, Some(ref send) if send.pt == c.pt) {
                continue;
            }
            answer_codecs.push(c);
        }

        // If SRTP is active, repeat our TX key in the answer (the peer is
        // still using it to decrypt our outbound packets). Mirror tag/suite
        // from the offer if present; otherwise use the codebase default.
        let answer_crypto = self.srtp_tx_key_b64.as_ref().map(|key| {
            let (tag, suite) = parsed
                .crypto
                .as_ref()
                .map(|c| (c.tag, c.suite.clone()))
                .unwrap_or_else(|| (1, "AES_CM_128_HMAC_SHA1_80".to_string()));
            SdpCrypto {
                tag,
                suite,
                key_b64: key.clone(),
            }
        });

        let answer = sdp::generate_sdp_answer(
            SocketAddr::new(bind_ip, self.local_rtp_addr.port()),
            self.local_rtp_addr.port(),
            &answer_codecs,
            answer_crypto.as_ref(),
            self.id.as_u128() as u64,
        );

        Ok(answer)
    }

    /// Whether this endpoint has SRTP decryption configured.
    pub fn has_srtp(&self) -> bool {
        self.srtp_rx.is_some() || self.srtp_rx_new.is_some()
    }

    /// Point outbound RTP/RTCP at `source` (symmetric RTP). The RTCP address
    /// follows the mux mode: the same address under `rtcp-mux`, otherwise the
    /// RTP port + 1.
    fn latch_remote_addr(&mut self, source: SocketAddr) {
        self.remote_rtp_addr = Some(source);
        self.remote_rtcp_addr = if self.rtcp_mux {
            Some(source)
        } else {
            rtcp_addr_from_rtp(source)
        };
    }

    /// Process an inbound RTP packet (SRTP decrypt if enabled)
    pub fn handle_rtp(&mut self, data: &[u8], source: SocketAddr) -> Option<RoutedRtpPacket> {
        // Check rekey switchover deadline
        self.check_rekey_switchover();

        // SRTP decrypt if enabled
        let decrypted;
        let data = if self.srtp_rx.is_some() || self.srtp_rx_new.is_some() {
            // During rekey, try the new context first, then fall back to old
            if let Some(ref mut new_ctx) = self.srtp_rx_new {
                match new_ctx.unprotect(data) {
                    Ok(d) => {
                        // New key works — promote it and clear transition state
                        debug!(endpoint_id = %self.id, "SRTP rekey: new key succeeded, promoting");
                        self.srtp_rx = self.srtp_rx_new.take();
                        self.rekey_switchover = None;
                        decrypted = d;
                        &decrypted
                    }
                    Err(_) => {
                        // New key failed — try old key
                        if let Some(ref mut old_ctx) = self.srtp_rx {
                            match old_ctx.unprotect(data) {
                                Ok(d) => {
                                    decrypted = d;
                                    &decrypted
                                }
                                Err(e) => {
                                    debug!(endpoint_id = %self.id, error = %e, "SRTP decrypt failed (both keys)");
                                    return None;
                                }
                            }
                        } else {
                            debug!(endpoint_id = %self.id, "SRTP decrypt failed: no old key");
                            return None;
                        }
                    }
                }
            } else if let Some(ref mut ctx) = self.srtp_rx {
                match ctx.unprotect(data) {
                    Ok(d) => {
                        decrypted = d;
                        &decrypted
                    }
                    Err(e) => {
                        debug!(endpoint_id = %self.id, error = %e, "SRTP decrypt failed");
                        return None;
                    }
                }
            } else {
                data
            }
        } else {
            data
        };

        let header = RtpHeader::parse(data)?;
        let payload = header.payload(data);

        // Learn remote SSRC from the first packet. That same first (already
        // authenticated, for SRTP) packet also latches the real source address
        // unconditionally — even if the symmetric-RTP learning window has already
        // elapsed. This covers an offerer leg whose callee rings longer than the
        // window before answering, so media only starts well after the endpoint
        // was created: the negotiated SDP address is just a placeholder (often an
        // unroutable private NAT address) until we actually hear from the peer.
        // SRTP packets are authenticated/decrypted above and plain RTP already
        // trusts the first packet for the SSRC, so this does not widen the
        // spoofing surface. Later NAT rebinds are handled by the windowed path.
        if self.remote_ssrc.is_none() {
            self.remote_ssrc = Some(header.ssrc);
            debug!(endpoint_id = %self.id, ssrc = header.ssrc, "learned remote SSRC");

            if self.remote_rtp_addr != Some(source) {
                if let Some(old) = self.remote_rtp_addr {
                    tracing::info!(
                        endpoint_id = %self.id,
                        sdp_addr = %old,
                        actual_addr = %source,
                        "symmetric RTP: latched remote address from first packet (SDP mismatch, likely NAT)"
                    );
                } else {
                    debug!(endpoint_id = %self.id, addr = %source, "learned remote address from first packet");
                }
                self.latch_remote_addr(source);
            }
        } else if self.remote_ssrc != Some(header.ssrc) {
            // SSRC changed mid-call (e.g. a hold/re-INVITE that restarts the
            // RTP stream with a fresh SSRC). Track the new source so the RTCP
            // SR/RR we emit references it — `rtcp_stats` already re-baselines
            // its sequence/loss/jitter on the same change. We do NOT re-latch
            // the address here; a NAT rebind is handled by the windowed
            // symmetric-RTP path below.
            debug!(
                endpoint_id = %self.id,
                old_ssrc = ?self.remote_ssrc,
                new_ssrc = header.ssrc,
                "remote SSRC changed mid-call; tracking new source"
            );
            self.remote_ssrc = Some(header.ssrc);
        }

        // Symmetric RTP: keep tracking address changes within the learning
        // window (a NAT rebind during call setup), then lock once it elapses.
        if !self.addr_locked {
            if self.created_at.elapsed() > Duration::from_secs(self.addr_learn_window_secs) {
                // Learning window expired — lock the address
                self.addr_locked = true;
                debug!(endpoint_id = %self.id, addr = ?self.remote_rtp_addr, "address locked after learning window");
            } else if self.remote_rtp_addr != Some(source) {
                tracing::info!(
                    endpoint_id = %self.id,
                    sdp_addr = ?self.remote_rtp_addr,
                    actual_addr = %source,
                    "symmetric RTP: updating remote address (SDP mismatch, likely NAT)"
                );
                self.latch_remote_addr(source);
            }
        }

        // Update stats
        self.stats.record_inbound(payload.len());
        self.rtcp_stats.record_received(
            header.ssrc,
            header.sequence_number,
            header.timestamp,
            payload.len(),
            self.recv_clock_rate,
        );

        Some(RoutedRtpPacket {
            source_endpoint_id: self.id,
            payload_type: header.payload_type,
            sequence_number: header.sequence_number,
            timestamp: header.timestamp,
            ssrc: header.ssrc,
            marker: header.marker,
            payload: payload.to_vec(),
        })
    }

    /// Process an inbound RTCP packet (SRTCP decrypt if enabled).
    /// Returns (ByePacket if BYE received, decrypted RTCP bytes for recording).
    pub fn handle_rtcp(&mut self, data: &[u8]) -> (Option<rtcp::ByePacket>, Option<Vec<u8>>) {
        let plain = if self.srtcp_rx.is_some() || self.srtcp_rx_new.is_some() {
            // During rekey, try the new context first, then fall back to old
            if let Some(ref mut new_ctx) = self.srtcp_rx_new {
                match new_ctx.unprotect_rtcp(data) {
                    Ok(d) => {
                        debug!(endpoint_id = %self.id, "SRTCP rekey: new key succeeded, promoting");
                        self.srtcp_rx = self.srtcp_rx_new.take();
                        d
                    }
                    Err(_) => {
                        if let Some(ref mut ctx) = self.srtcp_rx {
                            match ctx.unprotect_rtcp(data) {
                                Ok(d) => d,
                                Err(e) => {
                                    debug!(endpoint_id = %self.id, error = %e, "SRTCP decrypt failed (both keys)");
                                    return (None, None);
                                }
                            }
                        } else {
                            return (None, None);
                        }
                    }
                }
            } else if let Some(ref mut ctx) = self.srtcp_rx {
                match ctx.unprotect_rtcp(data) {
                    Ok(d) => d,
                    Err(e) => {
                        debug!(endpoint_id = %self.id, error = %e, "SRTCP decrypt failed");
                        return (None, None);
                    }
                }
            } else {
                data.to_vec()
            }
        } else {
            data.to_vec()
        };
        let packets = rtcp::parse_rtcp(&plain);
        let mut bye_result = None;
        for pkt in packets {
            match pkt {
                rtcp::RtcpPacket::SenderReport(sr) => {
                    // Process report blocks in the SR (they function as RR blocks)
                    for block in &sr.report_blocks {
                        self.rtcp_stats.process_rr(block, self.our_ssrc);
                    }
                    self.rtcp_stats.process_sr(&sr);
                }
                rtcp::RtcpPacket::ReceiverReport(rr) => {
                    for block in &rr.report_blocks {
                        self.rtcp_stats.process_rr(block, self.our_ssrc);
                    }
                }
                rtcp::RtcpPacket::Bye(bye) => {
                    tracing::info!(
                        endpoint_id = %self.id,
                        ssrc_count = bye.ssrc_list.len(),
                        reason = ?bye.reason,
                        "RTCP BYE received"
                    );
                    bye_result = Some(bye);
                }
            }
        }
        (bye_result, Some(plain))
    }

    /// Advance the destination-owned RTP timestamp timeline for one outbound
    /// packet and return `(wire_timestamp, marker_bit)`.
    ///
    /// The endpoint owns its outbound RTP timeline rather than copying the
    /// source packet's timestamp through. With the same SSRC, any timestamp
    /// jump (mixer→passthrough after hold-music removal, passthrough →
    /// passthrough after a source switch, file→normal forwarding, etc.)
    /// corrupts receiver jitter buffers and produces static or silence on
    /// the held leg after un-hold. Anchoring to our own previous output and
    /// only borrowing the *delta* from the source keeps the wire timeline
    /// monotonic and continuous across all of those transitions.
    ///
    /// - Same source: advance by the source's delta, but clamp impossibly
    ///   large jumps (>10 packet durations) to one frame and set the marker
    ///   bit so the receiver re-anchors. Update the smoothed packet-duration
    ///   estimate from sane deltas so source changes use a realistic step
    ///   even for non-20ms ptimes.
    /// - Different source (mixer↔passthrough, A↔B): advance by one packet
    ///   duration and force the marker bit so the receiver treats it as a
    ///   new talk-spurt instead of a lost-packet gap.
    /// - First packet ever: seed the timeline from the source so the
    ///   receiver doesn't see an arbitrary starting timestamp.
    fn advance_outbound_timeline(
        &mut self,
        source_id: EndpointId,
        source_ts: u32,
        source_marker: bool,
    ) -> (u32, bool) {
        let nominal_step = self
            .send_codec
            .as_ref()
            .map(|c| c.clock_rate / 50)
            .unwrap_or(160);
        let bump = self.learned_step.unwrap_or(nominal_step);

        let (outbound_ts, marker_override) = match (
            self.last_outbound_ts,
            self.last_source_id,
            self.last_source_ts,
        ) {
            (Some(last_out), Some(last_src), Some(last_src_ts)) if last_src == source_id => {
                let delta = source_ts.wrapping_sub(last_src_ts);
                let max_safe = bump.saturating_mul(10);
                if delta == 0 {
                    // Duplicate / same-TS retransmit — don't advance the wire.
                    (last_out, false)
                } else if delta > max_safe {
                    // Within-source discontinuity (codec quirk, clock skip):
                    // collapse the gap and signal a new talk-spurt.
                    (last_out.wrapping_add(bump), true)
                } else {
                    if delta <= bump.saturating_mul(2) {
                        self.learned_step = Some(delta);
                    }
                    (last_out.wrapping_add(delta), false)
                }
            }
            (Some(last_out), _, _) => {
                // Source changed (mixer↔passthrough, file↔normal, A↔B).
                (last_out.wrapping_add(bump), true)
            }
            _ => (source_ts, source_marker),
        };

        self.last_outbound_ts = Some(outbound_ts);
        self.last_source_id = Some(source_id);
        self.last_source_ts = Some(source_ts);
        (outbound_ts, source_marker || marker_override)
    }

    /// Wire timestamp for an outbound RFC 4733 telephone-event (DTMF) packet.
    ///
    /// Every packet of one event shares a single RTP timestamp. We anchor it
    /// once on the marker packet (to the next frame past our last output, so it
    /// sits within the shared-SSRC timeline) and hold it across the continuation
    /// and redundant end packets.
    ///
    /// The anchor also *advances* `last_outbound_ts` by one frame, claiming that
    /// slot on the wire timeline. This is deliberate and serves two ends:
    /// - back-to-back digits during silence get distinct timestamps (without it,
    ///   both would re-anchor to the same `last_outbound_ts + bump`, and a
    ///   receiver would dedup the second as a redundant copy of the first);
    /// - a DTMF-first endpoint seeds the timeline, so the following audio packet
    ///   advances from the anchor instead of jumping to the source timestamp.
    ///
    /// We do NOT touch `last_source_id`/`last_source_ts`, so interleaved audio
    /// still sees the same source and never gets a spurious talk-spurt marker —
    /// the property that fixes the "one digit arrives as many events" bug. The
    /// cost is a single one-frame gap in the audio timeline per digit, which is
    /// exactly the slot the DTMF event occupies and is benign for jitter buffers.
    fn dtmf_outbound_ts(&mut self, marker: bool, fallback_ts: u32) -> u32 {
        // Continuation / redundant end packets reuse the held event anchor.
        if !marker && let Some(ts) = self.dtmf_wire_ts {
            return ts;
        }
        // Marker packet (or the first packet ever): (re-)anchor. Advance by one
        // 20ms frame in the *telephone-event* clock — DTMF timestamps live on the
        // negotiated telephone-event timeline, not the audio codec clock (these
        // coincide at 8000 for a typical SIP phone, but not when media is Opus).
        let bump = self.telephone_event_clock_rate / 50;
        let anchor = self
            .last_outbound_ts
            .map_or(fallback_ts, |t| t.wrapping_add(bump));
        self.dtmf_wire_ts = Some(anchor);
        self.last_outbound_ts = Some(anchor);
        anchor
    }

    /// Write an RTP packet out through this endpoint.
    ///
    /// Returns `Ok(Some(unencrypted_bytes))` with the wire-format RTP packet that
    /// was sent (pre-SRTP-encryption) so the caller can feed it to a recording
    /// tap. Returns `Ok(None)` when the packet was skipped (no remote SSRC yet).
    /// Errors out only on hard send failures.
    pub async fn write_rtp(
        &mut self,
        packet: &RoutedRtpPacket,
        metrics: &Metrics,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let remote = self
            .remote_rtp_addr
            .ok_or_else(|| anyhow::anyhow!("No remote RTP address"))?;

        // Don't send until we've received at least one inbound packet.
        // The SDP address may be a private IP behind NAT; sending to it
        // before symmetric RTP has learned the real address can poison
        // ARP/MAC tables on multi-homed hosts (e.g., Multus + CNI).
        if self.remote_ssrc.is_none() {
            return Ok(None);
        }

        // Anchor the wire timestamp to our own previous output. See
        // `advance_outbound_timeline` for the full rationale.
        //
        // Telephone-event (RFC 4733 DTMF) packets are the exception: every
        // packet of one event shares a single RTP timestamp with the marker
        // set only on the first. Running them through the audio timeline would
        // treat each audio↔DTMF source switch as a new talk-spurt — forcing
        // the marker and bumping the timestamp on every interleaved DTMF
        // packet, so one injected digit reaches the far end as many events.
        // Instead, anchor the event's timestamp once (to the next audio frame)
        // and hold it constant. See `dtmf_outbound_ts`.
        let (outbound_ts, marker) = if Some(packet.payload_type) == self.telephone_event_pt {
            (
                self.dtmf_outbound_ts(packet.marker, packet.timestamp),
                packet.marker,
            )
        } else {
            self.advance_outbound_timeline(
                packet.source_endpoint_id,
                packet.timestamp,
                packet.marker,
            )
        };

        let unencrypted = RtpHeader::build(
            packet.payload_type,
            self.seq_no,
            outbound_ts,
            self.our_ssrc,
            marker,
            &packet.payload,
        );

        self.seq_no = self.seq_no.wrapping_add(1);
        self.last_rtp_timestamp = outbound_ts;
        self.stats.record_outbound(packet.payload.len());
        self.rtcp_stats.record_sent(packet.payload.len());

        // SRTP encrypt if enabled. We send from whichever buffer is appropriate
        // for the wire and return the unencrypted bytes for the recording tap.
        // (No clone in the SRTP-off path: we send `&unencrypted` and then move it
        // into the return value.)
        let send_result = if let Some(ref mut ctx) = self.srtp_tx {
            let encrypted = ctx.protect(&unencrypted)?;
            self.rtp_socket.send_to(&encrypted, remote).await
        } else {
            self.rtp_socket.send_to(&unencrypted, remote).await
        };
        match send_result {
            Ok(_) => metrics.record_udp_send_ok("rtp", "rtp", remote),
            Err(e) => {
                metrics.record_udp_send_error("rtp", "rtp", remote);
                return Err(e.into());
            }
        }
        Ok(Some(unencrypted))
    }

    /// Send RTCP SR+RR if enough time has elapsed (every 5 seconds)
    /// Send RTCP SR+RR if enough time has elapsed. Returns the raw RTCP bytes
    /// if a packet was sent (for recording tap), or None if skipped.
    pub async fn maybe_send_rtcp(&mut self, metrics: &Metrics) -> anyhow::Result<Option<Vec<u8>>> {
        if self.last_rtcp_sent.elapsed() < Duration::from_secs(5) {
            return Ok(None);
        }

        let remote_rtcp = match self.remote_rtcp_addr {
            Some(addr) => addr,
            None => return Ok(None),
        };

        let remote_ssrc = match self.remote_ssrc {
            Some(ssrc) => ssrc,
            None => return Ok(None), // no remote SSRC learned yet, skip RTCP
        };
        let clock_rate = self
            .send_codec
            .as_ref()
            .map(|c| c.clock_rate)
            .unwrap_or(8000);
        let rtcp_data = rtcp::build_sr_rr(
            self.our_ssrc,
            remote_ssrc,
            &mut self.rtcp_stats,
            self.last_rtp_timestamp,
            clock_rate,
        );

        // SRTCP encrypt if enabled
        let send_data = if let Some(ref mut ctx) = self.srtcp_tx {
            ctx.protect_rtcp(&rtcp_data)?
        } else {
            rtcp_data.clone()
        };

        match self.rtcp_socket.send_to(&send_data, remote_rtcp).await {
            Ok(_) => metrics.record_udp_send_ok("rtp", "rtcp", remote_rtcp),
            Err(e) => {
                metrics.record_udp_send_error("rtp", "rtcp", remote_rtcp);
                return Err(e.into());
            }
        }
        self.last_rtcp_sent = Instant::now();

        // Return plain RTCP for recording tap (post-decryption)
        Ok(Some(rtcp_data))
    }

    /// Reset the symmetric RTP address learning window.
    /// Called after direction changes (e.g. hold/unhold) where the phone
    /// may resume from a new NAT binding.
    pub fn reset_addr_lock(&mut self) {
        self.addr_locked = false;
        self.created_at = Instant::now();
    }

    /// Generate a new SRTP TX key, replacing TX immediately. Returns the new
    /// SDP offer with the new crypto line. The remote peer's new RX key will
    /// arrive via `accept_answer()`, which sets up the dual-context transition.
    pub fn srtp_rekey(&mut self) -> anyhow::Result<String> {
        if self.srtp_tx.is_none() {
            anyhow::bail!("SRTP is not active on this endpoint");
        }

        // Generate new 30-byte random key for TX only
        let mut key_bytes = [0u8; 30];
        for b in key_bytes.iter_mut() {
            *b = rand::random();
        }
        let b64_encoded = base64_encode(&key_bytes);

        // Create new TX context — replace immediately
        self.srtp_tx = Some(
            SrtpContext::from_sdes_key(&b64_encoded)
                .map_err(|e| anyhow::anyhow!("SRTP TX rekey failed: {e}"))?,
        );
        self.srtcp_tx = Some(
            SrtcpContext::from_sdes_key(&b64_encoded)
                .map_err(|e| anyhow::anyhow!("SRTCP TX rekey failed: {e}"))?,
        );
        self.srtp_tx_key_b64 = Some(b64_encoded.clone());

        // RX contexts are NOT updated here. The remote peer will provide their
        // new key in their SDP answer, and accept_answer() will set up the
        // dual-context RX transition at that point.

        // Build new SDP offer with the new crypto line
        let crypto = SdpCrypto {
            tag: 1,
            suite: "AES_CM_128_HMAC_SHA1_80".to_string(),
            key_b64: b64_encoded,
        };

        let bind_ip = self.local_rtp_addr.ip();
        let offer_codecs: Vec<&SdpCodec> = self.codecs.iter().collect();
        let sdp = sdp::generate_sdp_offer(
            SocketAddr::new(bind_ip, self.local_rtp_addr.port()),
            self.local_rtp_addr.port(),
            &offer_codecs,
            Some(&crypto),
            self.id.as_u128() as u64,
        );

        // Re-open the symmetric RTP address learning window.
        // After rekey, the remote peer may change its NAT binding, so we need
        // to re-learn the source address from inbound packets.
        self.addr_locked = false;
        self.created_at = Instant::now();

        debug!(endpoint_id = %self.id, "SRTP TX rekeyed — awaiting answer for RX update");
        Ok(sdp)
    }

    /// If the rekey switchover deadline has passed, force-promote the new RX context.
    fn check_rekey_switchover(&mut self) {
        if let Some(deadline) = self.rekey_switchover
            && Instant::now() >= deadline
        {
            if self.srtp_rx_new.is_some() {
                debug!(endpoint_id = %self.id, "SRTP rekey: switchover deadline reached, forcing new key");
                self.srtp_rx = self.srtp_rx_new.take();
            }
            if self.srtcp_rx_new.is_some() {
                self.srtcp_rx = self.srtcp_rx_new.take();
            }
            self.rekey_switchover = None;
        }
    }

    /// Check if a packet on the RTP socket is actually RTCP (for rtcp-mux demux).
    ///
    /// Only PT 200-204 (SR/RR/SDES/BYE/APP) are detected. Extended RTCP types
    /// 205-213 (RTPFB, PSFB, XR, etc.) are intentionally excluded because their
    /// byte values overlap with legitimate RTP packets that have the marker bit
    /// set (RTP PT 77-85 | 0x80 = 205-213). This is a deliberate tradeoff per
    /// RFC 5761 §4: those RTCP packets will be treated as malformed RTP and
    /// discarded, which is harmless since they are optional feedback mechanisms
    /// not critical to media flow.
    pub fn is_rtcp_mux_packet(data: &[u8]) -> bool {
        if data.len() >= 2 {
            let pt = data[1];
            return (200..=204).contains(&pt);
        }
        false
    }

    /// Stop recv tasks for transfer. Cancels the token, awaits all tasks,
    /// and creates a fresh CancellationToken for restart.
    pub async fn stop_recv_tasks(&mut self) {
        self.cancel_token.cancel();
        for handle in self.recv_tasks.drain(..) {
            let _ = handle.await;
        }
        self.cancel_token = CancellationToken::new();
    }

    /// Restart recv tasks with a new packet_tx (after transfer to a new session).
    pub fn restart_recv_tasks(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        self.start_recv_tasks(packet_tx);
    }
}

/// Parse a peer's SDP `a=` direction attribute into an `EndpointDirection`.
///
/// `EndpointDirection` is expressed from the peer's perspective (see its doc in
/// `protocol.rs`), which is exactly what the SDP attribute already encodes, so
/// this is a direct mapping — NOT a mirror. e.g. a peer offering `a=sendonly`
/// (the peer sends, won't receive) becomes `SendOnly`, which `routing.rs` treats
/// as a source that rtpbridge does not transmit to.
fn endpoint_direction_from_sdp(remote_dir: &str) -> Option<EndpointDirection> {
    match remote_dir {
        "sendrecv" => Some(EndpointDirection::SendRecv),
        "recvonly" => Some(EndpointDirection::RecvOnly),
        "sendonly" => Some(EndpointDirection::SendOnly),
        "inactive" => Some(EndpointDirection::Inactive),
        _ => None,
    }
}

/// Cancels then aborts recv tasks on drop, ensuring cleanup when endpoints are removed
/// from a session (HashMap::remove drops the value, triggering this) or when the session
/// itself ends. The cancellation token gives tasks a cooperative exit path; the abort
/// serves as a safety net.
impl Drop for RtpEndpoint {
    fn drop(&mut self) {
        let rtp_refs = Arc::strong_count(&self.rtp_socket);
        warn!(
            endpoint_id = %self.id,
            local_port = self.local_rtp_addr.port(),
            rtp_arc_refs = rtp_refs,
            "RtpEndpoint dropping"
        );
        self.cancel_token.cancel();
        for handle in self.recv_tasks.drain(..) {
            handle.abort();
        }
    }
}

pub(crate) fn base64_encode(data: &[u8]) -> String {
    crate::media::srtp::base64_encode(data)
}

#[cfg(test)]
#[path = "endpoint_rtp_tests.rs"]
mod tests;
