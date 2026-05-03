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
use super::stats::EndpointStats;
use crate::control::protocol::{
    EndpointDirection, EndpointDirectionUpdate, EndpointId, EndpointState,
};
use crate::media::rtcp::{self, RtcpStats};
use crate::media::rtp::RtpHeader;
use crate::media::sdp::{self, SdpCodec, SdpCrypto};
use crate::media::srtp::{SrtcpContext, SrtpContext};
use crate::net::socket_pool::SocketPair;

/// A plain RTP (optionally SRTP) endpoint
pub struct RtpEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,

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
            rtcp_stats: RtcpStats::new(),
            last_rtcp_sent: Instant::now(),
            seq_no: rand::random(),
            last_rtp_timestamp: 0,
            last_outbound_ts: None,
            last_source_id: None,
            last_source_ts: None,
            learned_step: None,
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

    /// Start recv tasks for RTP and RTCP sockets
    pub fn start_recv_tasks(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        // RTP recv task
        let rtp_socket = Arc::clone(&self.rtp_socket);
        let endpoint_id = self.id;
        let tx = packet_tx.clone();
        let token = self.cancel_token.clone();
        self.recv_tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let exit_reason;
            loop {
                tokio::select! {
                    result = rtp_socket.recv_from(&mut buf) => {
                        match result {
                            Ok((n, source)) => {
                                let packet = InboundPacket {
                                    endpoint_id,
                                    source,
                                    data: buf[..n].to_vec(),
                                    is_rtcp: false,
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
        self.recv_tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let exit_reason;
            loop {
                tokio::select! {
                    result = rtcp_socket.recv_from(&mut buf) => {
                        match result {
                            Ok((n, source)) => {
                                let packet = InboundPacket {
                                    endpoint_id,
                                    source,
                                    data: buf[..n].to_vec(),
                                    is_rtcp: true,
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
            .and_then(map_remote_direction_to_local)
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

        // Use the codecs from the offer (intersect with what we support)
        endpoint.codecs = parsed.codecs.clone();
        endpoint.telephone_event_pt = parsed.telephone_event_pt;

        // Pick first codec as our send codec, and learn the receive clock rate
        endpoint.send_codec = parsed
            .codecs
            .iter()
            .find(|c| c.name != "telephone-event")
            .cloned();
        endpoint.recv_clock_rate = endpoint
            .send_codec
            .as_ref()
            .map(|c| c.clock_rate)
            .unwrap_or(8000);

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

        // Generate SDP answer
        let answer_codecs: Vec<&SdpCodec> = endpoint.codecs.iter().collect();
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
        endpoint.telephone_event_pt = codecs
            .iter()
            .find(|c| c.name == "telephone-event")
            .map(|c| c.pt);

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
    pub fn accept_answer(&mut self, answer_sdp: &str) -> anyhow::Result<()> {
        let parsed = sdp::parse_sdp(answer_sdp);

        if let Some(dir) = parsed
            .direction
            .as_deref()
            .and_then(map_remote_direction_to_local)
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

        // Apply remote SDP direction by default unless an explicit manual
        // override is active via endpoint.update_direction.
        if let Some(dir) = parsed
            .direction
            .as_deref()
            .and_then(map_remote_direction_to_local)
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

        // Learn remote SSRC from first packet
        if self.remote_ssrc.is_none() {
            self.remote_ssrc = Some(header.ssrc);
            debug!(endpoint_id = %self.id, ssrc = header.ssrc, "learned remote SSRC");
        }

        // Symmetric RTP: learn/update remote address from inbound media
        if !self.addr_locked {
            if self.created_at.elapsed() > Duration::from_secs(self.addr_learn_window_secs) {
                // Learning window expired — lock the address
                self.addr_locked = true;
                debug!(endpoint_id = %self.id, addr = ?self.remote_rtp_addr, "address locked after learning window");
            } else if self.remote_rtp_addr != Some(source) {
                if let Some(old) = self.remote_rtp_addr {
                    tracing::info!(
                        endpoint_id = %self.id,
                        sdp_addr = %old,
                        actual_addr = %source,
                        "symmetric RTP: updating remote address (SDP mismatch, likely NAT)"
                    );
                } else {
                    debug!(endpoint_id = %self.id, addr = %source, "learned remote address from first packet");
                }
                self.remote_rtp_addr = Some(source);
                if self.rtcp_mux {
                    self.remote_rtcp_addr = Some(source);
                } else {
                    self.remote_rtcp_addr = rtcp_addr_from_rtp(source);
                }
            }
        }

        // Update stats
        self.stats.record_inbound(payload.len());
        self.rtcp_stats.record_received(
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

    /// Write an RTP packet out through this endpoint.
    ///
    /// Returns `Ok(Some(unencrypted_bytes))` with the wire-format RTP packet that
    /// was sent (pre-SRTP-encryption) so the caller can feed it to a recording
    /// tap. Returns `Ok(None)` when the packet was skipped (no remote SSRC yet).
    /// Errors out only on hard send failures.
    pub async fn write_rtp(&mut self, packet: &RoutedRtpPacket) -> anyhow::Result<Option<Vec<u8>>> {
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
        let (outbound_ts, marker) = self.advance_outbound_timeline(
            packet.source_endpoint_id,
            packet.timestamp,
            packet.marker,
        );

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
        if let Some(ref mut ctx) = self.srtp_tx {
            let encrypted = ctx.protect(&unencrypted)?;
            self.rtp_socket.send_to(&encrypted, remote).await?;
        } else {
            self.rtp_socket.send_to(&unencrypted, remote).await?;
        }
        Ok(Some(unencrypted))
    }

    /// Send RTCP SR+RR if enough time has elapsed (every 5 seconds)
    /// Send RTCP SR+RR if enough time has elapsed. Returns the raw RTCP bytes
    /// if a packet was sent (for recording tap), or None if skipped.
    pub async fn maybe_send_rtcp(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
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

        self.rtcp_socket.send_to(&send_data, remote_rtcp).await?;
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

fn map_remote_direction_to_local(remote_dir: &str) -> Option<EndpointDirection> {
    match remote_dir {
        "sendrecv" => Some(EndpointDirection::SendRecv),
        "recvonly" => Some(EndpointDirection::SendOnly), // remote receives -> we send
        "sendonly" => Some(EndpointDirection::RecvOnly), // remote sends -> we receive
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
mod tests {
    use super::*;

    // ── is_rtcp_mux_packet tests ────────────────────────────────────

    #[test]
    fn test_rtcp_mux_detects_sr() {
        // Sender Report: PT = 200 (0xC8)
        let pkt = [0x80, 200u8, 0x00, 0x06];
        assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_detects_rr() {
        // Receiver Report: PT = 201 (0xC9)
        let pkt = [0x80, 201u8, 0x00, 0x01];
        assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_detects_sdes() {
        let pkt = [0x80, 202u8, 0x00, 0x02];
        assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_detects_bye() {
        let pkt = [0x80, 203u8, 0x00, 0x01];
        assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_detects_app() {
        let pkt = [0x80, 204u8, 0x00, 0x03];
        assert!(RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_rejects_extended_types() {
        // Extended RTCP types (205-213) are excluded from demux to avoid collision
        // with RTP packets that have marker bit set + PT 77-85.
        // These are handled by the RTCP parser after initial classification.
        for pt in 205..=213u8 {
            let pkt = [0x80, pt, 0x00, 0x01];
            assert!(
                !RtpEndpoint::is_rtcp_mux_packet(&pkt),
                "PT {pt} should NOT be detected as RTCP in demux (extended range excluded)"
            );
        }
    }

    #[test]
    fn test_rtcp_mux_rejects_rtp_pcmu() {
        // RTP PCMU: PT = 0, with marker bit clear → byte 1 = 0x00
        let pkt = [0x80, 0x00, 0x00, 0x01];
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_rejects_rtp_pcmu_with_marker() {
        // RTP PCMU with marker bit set: byte 1 = 0x80 | 0 = 0x80 (128)
        let pkt = [0x80, 0x80, 0x00, 0x01];
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_rejects_rtp_opus_111() {
        // RTP Opus PT=111: byte 1 = 111 (0x6F), no marker
        let pkt = [0x80, 111, 0x00, 0x01];
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_rejects_rtp_opus_111_marker() {
        // RTP Opus PT=111 with marker: byte 1 = 0x80 | 111 = 0xEF (239)
        let pkt = [0x80, 0xEF, 0x00, 0x01];
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_rejects_rtp_g722() {
        // RTP G.722: PT = 9
        let pkt = [0x80, 9, 0x00, 0x01];
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&pkt));
    }

    #[test]
    fn test_rtcp_mux_rejects_too_short() {
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&[]));
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&[0x80]));
    }

    #[test]
    fn test_rtcp_mux_boundary_values() {
        // 199 should NOT match (just below RTCP range)
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&[0x80, 199]));
        // 200 should match (first RTCP PT — SR)
        assert!(RtpEndpoint::is_rtcp_mux_packet(&[0x80, 200]));
        // 204 should match (last safe RTCP PT — APP)
        assert!(RtpEndpoint::is_rtcp_mux_packet(&[0x80, 204]));
        // 205 should NOT match (excluded to avoid marker-bit collision)
        assert!(!RtpEndpoint::is_rtcp_mux_packet(&[0x80, 205]));
    }

    // ── rtcp_mux address negotiation tests ──────────────────────────

    fn make_sdp_with_mux(port: u16, rtcp_mux: bool) -> String {
        let mut sdp = format!(
            "v=0\r\n\
             o=- 123 1 IN IP4 10.0.0.1\r\n\
             s=-\r\n\
             c=IN IP4 10.0.0.1\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=sendrecv\r\n"
        );
        if rtcp_mux {
            sdp.push_str("a=rtcp-mux\r\n");
        }
        sdp
    }

    #[tokio::test]
    async fn test_from_offer_with_rtcp_mux_sets_same_port() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51000, 51100)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);

        let sdp = make_sdp_with_mux(20000, true);
        let (ep, _answer) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            &sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        assert!(ep.rtcp_mux);
        assert_eq!(
            ep.remote_rtp_addr.unwrap().port(),
            ep.remote_rtcp_addr.unwrap().port(),
            "with rtcp-mux, RTCP addr should equal RTP addr"
        );
        assert_eq!(ep.remote_rtcp_addr.unwrap().port(), 20000);
    }

    #[tokio::test]
    async fn test_from_offer_without_rtcp_mux_uses_port_plus_one() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51100, 51200)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);

        let sdp = make_sdp_with_mux(20000, false);
        let (ep, _answer) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            &sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        assert!(!ep.rtcp_mux);
        assert_eq!(ep.remote_rtp_addr.unwrap().port(), 20000);
        assert_eq!(
            ep.remote_rtcp_addr.unwrap().port(),
            20001,
            "without rtcp-mux, RTCP port should be RTP port + 1"
        );
    }

    #[tokio::test]
    async fn test_from_offer_applies_initial_remote_direction_in_auto_mode() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41000, 41100)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);

        let sdp = make_sdp_with_mux(20000, true).replace("a=sendrecv", "a=recvonly");
        let (ep, _answer) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            &sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        assert_eq!(
            ep.config.direction,
            EndpointDirection::SendOnly,
            "initial remote recvonly should map to local sendonly"
        );
    }

    #[tokio::test]
    async fn test_accept_answer_with_rtcp_mux_updates_addr() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51200, 51300)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        let answer = make_sdp_with_mux(30000, true);
        ep.accept_answer(&answer).unwrap();

        assert!(ep.rtcp_mux);
        assert_eq!(ep.remote_rtp_addr.unwrap().port(), 30000);
        assert_eq!(
            ep.remote_rtcp_addr.unwrap().port(),
            30000,
            "accept_answer with rtcp-mux should set RTCP addr = RTP addr"
        );
    }

    #[tokio::test]
    async fn test_accept_answer_without_rtcp_mux_uses_port_plus_one() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51300, 51400)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        let answer = make_sdp_with_mux(30000, false);
        ep.accept_answer(&answer).unwrap();

        assert!(!ep.rtcp_mux);
        assert_eq!(ep.remote_rtcp_addr.unwrap().port(), 30001);
    }

    #[tokio::test]
    async fn test_accept_answer_applies_initial_remote_direction_in_auto_mode() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41100, 41200)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let codecs = vec![
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ];

        let (mut ep, _offer) = RtpEndpoint::create_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            pair,
            "127.0.0.1".parse().unwrap(),
            &codecs,
            false,
            tx,
        )
        .unwrap();

        let answer = make_sdp_with_mux(30000, true).replace("a=sendrecv", "a=sendonly");
        ep.accept_answer(&answer).unwrap();

        assert_eq!(
            ep.config.direction,
            EndpointDirection::RecvOnly,
            "initial remote sendonly should map to local recvonly"
        );
    }

    #[tokio::test]
    async fn test_update_remote_sdp_updates_addr_with_rtcp_mux() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51400, 51500)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        // First set an initial address via accept_answer
        let initial = make_sdp_with_mux(30000, true);
        ep.accept_answer(&initial).unwrap();
        assert_eq!(ep.remote_rtp_addr.unwrap().port(), 30000);

        // Re-INVITE with a different port — update_remote_sdp should pick it up
        let reinvite = make_sdp_with_mux(40000, true);
        ep.update_remote_sdp(&reinvite).unwrap();

        assert_eq!(ep.remote_rtp_addr.unwrap().port(), 40000);
        assert_eq!(ep.remote_rtcp_addr.unwrap().port(), 40000);
    }

    #[tokio::test]
    async fn test_update_remote_sdp_preserves_codecs() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51500, 51600)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        // Seed the offered codec set so accept_answer's intersect-with-offer logic
        // actually populates self.codecs (it would otherwise reject everything).
        ep.codecs = vec![
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ];

        // Initial answer: PCMU only
        let initial = make_sdp_with_mux(30000, true);
        ep.accept_answer(&initial).unwrap();
        let codecs_before = ep.codecs.clone();
        let send_codec_before = ep.send_codec.clone();
        assert!(
            !codecs_before.is_empty(),
            "initial accept_answer should populate codecs"
        );

        // Re-INVITE with a different codec list (PCMA added, different PT)
        let reinvite = "\
            v=0\r\n\
            o=- 123 2 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 40000 RTP/AVP 8 0 101\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n\
            a=rtcp-mux\r\n";

        ep.update_remote_sdp(reinvite).unwrap();

        // Address updated
        assert_eq!(ep.remote_rtp_addr.unwrap().port(), 40000);
        // Codecs unchanged — this is the whole point of update_remote_sdp vs accept_answer
        assert_eq!(
            ep.codecs, codecs_before,
            "update_remote_sdp must NOT modify codec list"
        );
        assert_eq!(
            ep.send_codec, send_codec_before,
            "update_remote_sdp must NOT modify send_codec"
        );
    }

    #[tokio::test]
    async fn test_update_remote_sdp_resets_addr_lock() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51600, 51700)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        // Initial answer and manually lock the address
        let initial = make_sdp_with_mux(30000, true);
        ep.accept_answer(&initial).unwrap();
        ep.addr_locked = true;

        let reinvite = make_sdp_with_mux(40000, true);
        ep.update_remote_sdp(&reinvite).unwrap();

        assert!(
            !ep.addr_locked,
            "update_remote_sdp should reset addr lock so new NAT bindings can be learned"
        );
    }

    #[tokio::test]
    async fn test_update_remote_sdp_applies_direction_in_auto_mode() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41200, 41300)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        let reinvite_sendonly = "\
            v=0\r\n\
            o=- 123 2 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 40000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendonly\r\n\
            a=rtcp-mux\r\n";

        ep.update_remote_sdp(reinvite_sendonly).unwrap();
        assert_eq!(
            ep.config.direction,
            EndpointDirection::RecvOnly,
            "remote sendonly should map to local recvonly in auto mode"
        );
    }

    #[tokio::test]
    async fn test_update_remote_sdp_manual_override_takes_priority_until_auto() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41300, 41400)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        ep.set_direction_override(EndpointDirectionUpdate::SendOnly);
        assert_eq!(ep.config.direction, EndpointDirection::SendOnly);

        let reinvite_sendonly = "\
            v=0\r\n\
            o=- 123 2 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 40000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendonly\r\n\
            a=rtcp-mux\r\n";

        ep.update_remote_sdp(reinvite_sendonly).unwrap();
        assert_eq!(
            ep.config.direction,
            EndpointDirection::SendOnly,
            "manual override must win over remote SDP direction"
        );

        ep.set_direction_override(EndpointDirectionUpdate::Auto);
        assert_eq!(
            ep.config.direction,
            EndpointDirection::RecvOnly,
            "switching back to auto should apply last remote SDP direction"
        );
    }

    #[tokio::test]
    async fn test_update_remote_sdp_maps_inactive_direction() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 41400, 41500)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        let reinvite_inactive = "\
            v=0\r\n\
            o=- 123 2 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 40000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=inactive\r\n\
            a=rtcp-mux\r\n";

        ep.update_remote_sdp(reinvite_inactive).unwrap();
        assert_eq!(ep.config.direction, EndpointDirection::Inactive);
    }

    /// Regression: when a bridged peer (e.g. Grandstream GXP21xx) sends a
    /// hold/unhold re-INVITE, its offer may list codecs in a different order
    /// than the original offer. The 200 OK answer we generate must keep the
    /// originally-negotiated `send_codec` listed first — otherwise the phone
    /// will switch its outbound codec to whatever PT we list first while the
    /// rtpbridge endpoint keeps sending the originally-negotiated codec,
    /// producing one-way audio after un-hold.
    #[tokio::test]
    async fn test_update_remote_sdp_answer_lists_send_codec_first() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51900, 52000)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);

        // Original offer lists G722 first — endpoint negotiates G722 as send_codec.
        let original_offer = "\
            v=0\r\n\
            o=- 123 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 30000 RTP/AVP 9 0 101\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";
        let (mut ep, _initial_answer) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            original_offer,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();
        assert_eq!(
            ep.send_codec.as_ref().map(|c| c.name),
            Some("G722"),
            "send_codec should be G722 from initial offer"
        );

        // Re-INVITE re-orders the codec list: PCMU first now.
        let reinvite = "\
            v=0\r\n\
            o=- 123 2 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 40000 RTP/AVP 0 9 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";

        let answer = ep.update_remote_sdp(reinvite).unwrap();

        // Our answer's m= line must list G722's PT (9) first.
        let m_line = answer
            .lines()
            .find(|l| l.starts_with("m=audio"))
            .expect("answer should have m=audio line");
        let pts: Vec<&str> = m_line.split_whitespace().skip(3).collect();
        assert_eq!(
            pts.first().copied(),
            Some("9"),
            "answer's first PT must be G722 (9), not the offerer's preferred PCMU. \
             Full m= line: {m_line}"
        );

        // send_codec must remain unchanged (this is the key invariant of update_remote_sdp).
        assert_eq!(
            ep.send_codec.as_ref().map(|c| c.name),
            Some("G722"),
            "send_codec must remain G722 across re-INVITE"
        );
    }

    /// Helper: SDP with SRTP crypto line.
    fn make_savp_sdp(port: u16, key_b64: &str) -> String {
        format!(
            "v=0\r\n\
             o=- 123 1 IN IP4 10.0.0.1\r\n\
             s=-\r\n\
             c=IN IP4 10.0.0.1\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/SAVP 0 101\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=rtpmap:101 telephone-event/8000\r\n\
             a=sendrecv\r\n\
             a=rtcp-mux\r\n\
             a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:{key_b64}\r\n"
        )
    }

    /// A hold/unhold re-INVITE that carries the same SRTP crypto line as the initial
    /// answer must NOT trigger a rekey: resetting the SRTP RX context discards the
    /// running rollover counter and produces garbled/static audio for ~5s while the
    /// dual-context transition waits out. Phones like the Grandstream GXP2130 reuse
    /// the same key across re-INVITEs, so this is the common path.
    #[tokio::test]
    async fn test_update_remote_sdp_same_srtp_key_skips_rekey() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51700, 51800)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        // Seed the offered codec set so accept_answer's intersect-with-offer logic
        // populates codecs/srtp_rx.
        ep.codecs = vec![
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ];

        let key = "E4peZWnTvtquGbT3QN3ZJOM8i0Q2zNLc55bTN2VW";
        ep.accept_answer(&make_savp_sdp(30000, key)).unwrap();
        assert!(
            ep.srtp_rx.is_some(),
            "initial accept_answer should install SRTP RX"
        );
        assert!(ep.srtp_rx_new.is_none(), "no rekey yet");

        // Simulate that we had already learned a remote SSRC during the call
        ep.remote_ssrc = Some(0xDEAD_BEEF);

        // Re-INVITE with the SAME crypto key
        ep.update_remote_sdp(&make_savp_sdp(40000, key)).unwrap();

        assert!(ep.srtp_rx.is_some(), "SRTP RX context must be preserved");
        assert!(
            ep.srtp_rx_new.is_none(),
            "same-key re-INVITE must NOT start a dual-context transition (would wipe ROC)"
        );
        assert!(
            ep.rekey_switchover.is_none(),
            "no rekey switchover should be scheduled for a same-key re-INVITE"
        );
        assert_eq!(
            ep.remote_rtp_addr.unwrap().port(),
            40000,
            "address still updates"
        );

        // SSRC must be forgotten so it's relearned from the next inbound packet —
        // phones often switch SSRC across hold/unhold (always after RTCP BYE).
        assert!(
            ep.remote_ssrc.is_none(),
            "remote_ssrc must be cleared so it's relearned from the next packet"
        );
    }

    /// When the re-INVITE carries a genuinely different crypto key, update_remote_sdp
    /// must kick off the 5-second dual-context transition so in-flight packets on the
    /// old key decrypt until the remote switches over.
    #[tokio::test]
    async fn test_update_remote_sdp_different_srtp_key_triggers_rekey() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 51800, 51900)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);

        ep.codecs = vec![
            crate::media::sdp::CODEC_PCMU,
            crate::media::sdp::CODEC_TELEPHONE_EVENT,
        ];

        let key1 = "E4peZWnTvtquGbT3QN3ZJOM8i0Q2zNLc55bTN2VW";
        let key2 = "yyuehWOy8nibRn+adLgDP5fiaZ61fEw7RuxBimMr";

        ep.accept_answer(&make_savp_sdp(30000, key1)).unwrap();
        assert_eq!(ep.srtp_rx_key_b64.as_deref(), Some(key1));

        ep.update_remote_sdp(&make_savp_sdp(30000, key2)).unwrap();

        assert!(
            ep.srtp_rx_new.is_some(),
            "different-key re-INVITE must start a dual-context transition"
        );
        assert!(
            ep.rekey_switchover.is_some(),
            "rekey switchover deadline must be set"
        );
        assert_eq!(
            ep.srtp_rx_key_b64.as_deref(),
            Some(key2),
            "key tracker must be updated to the new key"
        );
    }

    /// Allocate a real socket pair and return a fresh PCMU endpoint suitable
    /// for testing the timestamp-continuity logic in isolation.
    async fn mk_ts_endpoint(start: u16, end: u16) -> RtpEndpoint {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), start, end)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);
        ep.send_codec = Some(crate::media::sdp::CODEC_PCMU); // 8000Hz → 160 step
        ep
    }

    #[tokio::test]
    async fn test_ts_first_packet_seeds_from_source() {
        let mut ep = mk_ts_endpoint(52000, 52100).await;
        let src = EndpointId::new_v4();

        let (ts, marker) = ep.advance_outbound_timeline(src, 12_345, false);
        assert_eq!(ts, 12_345, "first packet should seed from source TS");
        assert!(!marker, "first packet's marker passes through (false here)");
        assert_eq!(ep.last_outbound_ts, Some(12_345));
        assert_eq!(ep.last_source_id, Some(src));
        assert_eq!(ep.last_source_ts, Some(12_345));
    }

    #[tokio::test]
    async fn test_ts_same_source_uses_source_delta() {
        let mut ep = mk_ts_endpoint(52100, 52200).await;
        let src = EndpointId::new_v4();

        let (ts1, _) = ep.advance_outbound_timeline(src, 1000, false);
        let (ts2, m2) = ep.advance_outbound_timeline(src, 1160, false);
        let (ts3, m3) = ep.advance_outbound_timeline(src, 1320, false);

        assert_eq!(ts1, 1000, "seed");
        assert_eq!(ts2, 1160, "advance by source delta of 160");
        assert_eq!(ts3, 1320, "advance by source delta of 160");
        assert!(!m2 && !m3, "no marker override on same-source steady flow");
        assert_eq!(
            ep.learned_step,
            Some(160),
            "learned step adopts the source pacing"
        );
    }

    #[tokio::test]
    async fn test_ts_source_change_bumps_one_frame_and_marks() {
        let mut ep = mk_ts_endpoint(52200, 52300).await;
        let src_a = EndpointId::new_v4();
        let src_b = EndpointId::new_v4();

        // Establish a baseline on source A.
        ep.advance_outbound_timeline(src_a, 1000, false);
        ep.advance_outbound_timeline(src_a, 1160, false);
        let last_outbound = ep.last_outbound_ts.unwrap();

        // Source B's timestamps live in a totally different domain.
        let (ts, marker) = ep.advance_outbound_timeline(src_b, 9_999_000, false);

        assert_eq!(
            ts,
            last_outbound.wrapping_add(160),
            "source change must bump by one frame, not jump to source B's TS"
        );
        assert!(
            marker,
            "source change must set marker so the receiver re-anchors"
        );
        assert_eq!(ep.last_source_id, Some(src_b));
        assert_eq!(ep.last_source_ts, Some(9_999_000));
    }

    #[tokio::test]
    async fn test_ts_mixer_passthrough_transition_preserves_continuity() {
        // Reproduces the bug: mixer (source=nil) feeds during hold-music, then
        // we transition back to passthrough from a real source after un-hold.
        // Without destination-owned timeline, the wire timestamp domain jumps.
        let mut ep = mk_ts_endpoint(52300, 52400).await;
        let real_src = EndpointId::new_v4();
        let mixer = EndpointId::nil();

        // Real source pre-hold.
        ep.advance_outbound_timeline(real_src, 5_000, false);
        ep.advance_outbound_timeline(real_src, 5_160, false);

        // Hold: mixer takes over with its own monotonic clock.
        let (mix1, m1) = ep.advance_outbound_timeline(mixer, 9_000_000, false);
        let (mix2, m2) = ep.advance_outbound_timeline(mixer, 9_000_160, false);
        assert_eq!(
            mix1,
            5_160 + 160,
            "mixer first packet bumps by one frame off real source"
        );
        assert!(m1, "transition into mixer sets marker");
        assert_eq!(
            mix2,
            mix1 + 160,
            "subsequent mixer packets follow source delta"
        );
        assert!(!m2, "steady mixer flow does not set marker");

        // Un-hold: real source resumes — and its TS may be wildly different.
        let (resume, m3) = ep.advance_outbound_timeline(real_src, 5_320, false);
        assert_eq!(
            resume,
            mix2 + 160,
            "mixer→passthrough must continue the destination's timeline"
        );
        assert!(
            m3,
            "mixer→passthrough sets marker so jitter buffer re-anchors"
        );
    }

    #[tokio::test]
    async fn test_ts_duplicate_holds_outbound() {
        let mut ep = mk_ts_endpoint(52400, 52500).await;
        let src = EndpointId::new_v4();

        ep.advance_outbound_timeline(src, 1000, false);
        let (ts2, _) = ep.advance_outbound_timeline(src, 1160, false);
        // Same source-TS again — duplicate / retransmit.
        let (ts3, m3) = ep.advance_outbound_timeline(src, 1160, false);

        assert_eq!(
            ts3, ts2,
            "duplicate source TS must not advance the wire timeline"
        );
        assert!(!m3, "duplicate is not a new talk-spurt");
    }

    #[tokio::test]
    async fn test_ts_huge_same_source_jump_clamps_and_marks() {
        let mut ep = mk_ts_endpoint(52500, 52600).await;
        let src = EndpointId::new_v4();

        ep.advance_outbound_timeline(src, 1000, false);
        ep.advance_outbound_timeline(src, 1160, false);
        let last_out = ep.last_outbound_ts.unwrap();

        // Source TS jumps by ~10s (50_000 samples at 8kHz) — way beyond
        // 10×160. Should collapse to one frame and set marker.
        let (ts, marker) = ep.advance_outbound_timeline(src, 1160 + 50_000, false);
        assert_eq!(
            ts,
            last_out + 160,
            "huge in-source jump clamps to one frame"
        );
        assert!(marker, "in-source discontinuity sets marker");
    }

    #[tokio::test]
    async fn test_ts_learned_step_used_for_source_change() {
        let mut ep = mk_ts_endpoint(52600, 52700).await;
        let src_a = EndpointId::new_v4();
        let src_b = EndpointId::new_v4();

        // Source A paces at 320 (40ms ptime). Two packets establish the learned step.
        ep.advance_outbound_timeline(src_a, 1000, false);
        ep.advance_outbound_timeline(src_a, 1320, false);
        assert_eq!(ep.learned_step, Some(320), "learned step adopts 40ms ptime");
        let last_out = ep.last_outbound_ts.unwrap();

        // Source change should now bump by the learned step (320), not the
        // codec's nominal 20ms step (160).
        let (ts, _) = ep.advance_outbound_timeline(src_b, 7777, false);
        assert_eq!(
            ts,
            last_out + 320,
            "source-change bump should use learned step when available"
        );
    }

    #[tokio::test]
    async fn test_ts_no_send_codec_falls_back_to_pcmu_step() {
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52700, 52800)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let mut ep = RtpEndpoint::new(EndpointId::new_v4(), EndpointDirection::SendRecv, pair);
        // Intentionally leave send_codec as None.
        let src_a = EndpointId::new_v4();
        let src_b = EndpointId::new_v4();
        ep.advance_outbound_timeline(src_a, 1000, false);
        let (ts, _) = ep.advance_outbound_timeline(src_b, 50_000, false);
        assert_eq!(ts, 1000 + 160, "fallback step is 160 (8kHz/50)");
    }
}
