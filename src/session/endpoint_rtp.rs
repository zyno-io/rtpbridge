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
use crate::control::protocol::{EndpointDirection, EndpointId, EndpointState};
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
    /// Last outbound RTP timestamp (for RTCP SR)
    pub last_rtp_timestamp: u32,

    /// SRTP context for outgoing packets (None = plain RTP)
    srtp_tx: Option<SrtpContext>,
    /// SRTP context for incoming packets
    srtp_rx: Option<SrtpContext>,

    /// New SRTP RX context during rekey transition period
    srtp_rx_new: Option<SrtpContext>,
    /// When to force-switch to the new SRTP RX context
    rekey_switchover: Option<Instant>,

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
            srtp_tx: None,
            srtp_rx: None,
            srtp_rx_new: None,
            rekey_switchover: None,
            srtcp_tx: None,
            srtcp_rx: None,
            srtcp_rx_new: None,
            rtcp_mux: false,
            addr_learn_window_secs: 5,
            created_at: Instant::now(),
            addr_locked: false,
            cancel_token: CancellationToken::new(),
            recv_tasks: Vec::new(),
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
            } else {
                // Rekey: set as pending RX with dual-context switchover
                self.srtp_rx_new = Some(
                    SrtpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTP RX rekey failed: {e}"))?,
                );
                self.srtcp_rx_new = Some(
                    SrtcpContext::from_sdes_key(&crypto.key_b64)
                        .map_err(|e| anyhow::anyhow!("SRTCP RX rekey failed: {e}"))?,
                );
                self.rekey_switchover = Some(Instant::now() + Duration::from_secs(5));
                debug!(endpoint_id = %self.id, "SRTP RX rekey: dual-context transition started (5s)");
            }
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

    /// Write an RTP packet out through this endpoint
    pub async fn write_rtp(&mut self, packet: &RoutedRtpPacket) -> anyhow::Result<()> {
        let remote = self
            .remote_rtp_addr
            .ok_or_else(|| anyhow::anyhow!("No remote RTP address"))?;

        // Don't send until we've received at least one inbound packet.
        // The SDP address may be a private IP behind NAT; sending to it
        // before symmetric RTP has learned the real address can poison
        // ARP/MAC tables on multi-homed hosts (e.g., Multus + CNI).
        if self.remote_ssrc.is_none() {
            return Ok(());
        }

        // Use our own SSRC, sequence number, but preserve PT and timestamp
        let data = RtpHeader::build(
            packet.payload_type,
            self.seq_no,
            packet.timestamp,
            self.our_ssrc,
            packet.marker,
            &packet.payload,
        );

        self.seq_no = self.seq_no.wrapping_add(1);
        self.last_rtp_timestamp = packet.timestamp;
        self.stats.record_outbound(packet.payload.len());
        self.rtcp_stats.record_sent(packet.payload.len());

        // SRTP encrypt if enabled
        let data = if let Some(ref mut ctx) = self.srtp_tx {
            ctx.protect(&data)?
        } else {
            data
        };

        self.rtp_socket.send_to(&data, remote).await?;
        Ok(())
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
}
