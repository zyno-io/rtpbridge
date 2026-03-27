use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use super::endpoint::{EndpointConfig, InboundPacket, RoutedRtpPacket};
use super::stats::EndpointStats;
use crate::control::protocol::{EndpointDirection, EndpointId, EndpointState};

/// A WebRTC endpoint backed by str0m
pub struct WebRtcEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,
    pub rtc: Rtc,
    pub socket: Arc<UdpSocket>,
    pub local_addr: SocketAddr,
    /// Mid for the audio media line (set after SDP negotiation)
    pub audio_mid: Option<Mid>,
    /// Pending offer (when we created an offer, waiting for answer)
    pub pending_offer: Option<SdpPendingOffer>,
    /// Handle to the recv task (aborted on drop)
    recv_task: Option<tokio::task::JoinHandle<()>>,
    /// Cancellation token for cooperative recv task shutdown (cloned in start_recv_task, cancelled in drop)
    #[allow(dead_code)]
    cancel_token: CancellationToken,
}

impl WebRtcEndpoint {
    /// Create a new WebRTC endpoint with its own UDP socket
    async fn new_with_socket(
        id: EndpointId,
        config: EndpointConfig,
        bind_addr: SocketAddr,
    ) -> anyhow::Result<Self> {
        // WebRTC endpoints use OS-assigned ephemeral ports (not rtp_port_range).
        // ICE negotiates connectivity dynamically, so fixed port ranges don't apply.
        let socket = UdpSocket::bind(bind_addr).await?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);

        let rtc = RtcConfig::new()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .build(Instant::now());

        Ok(Self {
            id,
            config,
            state: EndpointState::New,
            stats: EndpointStats::new(),
            rtc,
            socket,
            local_addr,
            audio_mid: None,
            pending_offer: None,
            recv_task: None,
            cancel_token: CancellationToken::new(),
        })
    }

    /// Start the recv task that reads UDP packets and sends them to the session
    pub fn start_recv_task(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        let socket = Arc::clone(&self.socket);
        let endpoint_id = self.id;
        let token = self.cancel_token.clone();

        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                tokio::select! {
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((n, source)) => {
                                let packet = InboundPacket {
                                    endpoint_id,
                                    source,
                                    data: buf[..n].to_vec(),
                                    is_rtcp: false,
                                };
                                if packet_tx.send(packet).await.is_err() {
                                    break; // Session dropped
                                }
                            }
                            Err(e) => {
                                warn!(endpoint_id = %endpoint_id, error = %e, "UDP recv error");
                                break;
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        break;
                    }
                }
            }
        });

        self.recv_task = Some(handle);
    }

    /// Create from a remote SDP offer, returning the SDP answer string
    pub async fn from_offer(
        id: EndpointId,
        direction: EndpointDirection,
        offer_sdp: &str,
        bind_addr: SocketAddr,
        packet_tx: mpsc::Sender<InboundPacket>,
    ) -> anyhow::Result<(Self, String)> {
        let config = EndpointConfig { direction };
        let mut endpoint = Self::new_with_socket(id, config, bind_addr).await?;

        // Add local ICE candidate
        let candidate = Candidate::host(endpoint.local_addr, "udp")?;
        endpoint.rtc.add_local_candidate(candidate);

        // Parse SDP offer (try raw SDP string first, then JSON)
        let offer = SdpOffer::from_sdp_string(offer_sdp).or_else(|_| {
            serde_json::from_str::<SdpOffer>(offer_sdp)
                .map_err(|e| anyhow::anyhow!("Failed to parse SDP offer: {e}"))
        })?;

        let answer = endpoint.rtc.sdp_api().accept_offer(offer)?;
        let answer_str = answer.to_sdp_string();

        endpoint.state = EndpointState::Connecting;
        endpoint.start_recv_task(packet_tx);

        Ok((endpoint, answer_str))
    }

    /// Create an SDP offer for a new outgoing endpoint
    pub async fn create_offer(
        id: EndpointId,
        direction: EndpointDirection,
        bind_addr: SocketAddr,
        packet_tx: mpsc::Sender<InboundPacket>,
    ) -> anyhow::Result<(Self, String)> {
        let config = EndpointConfig { direction };
        let mut endpoint = Self::new_with_socket(id, config, bind_addr).await?;

        // Add local ICE candidate
        let candidate = Candidate::host(endpoint.local_addr, "udp")?;
        endpoint.rtc.add_local_candidate(candidate);

        // SDP direction is always sendrecv — the mixing direction is enforced
        // by the routing table, not the transport layer. This ensures str0m
        // creates both RX and TX streams regardless of mixing direction.
        let mut api = endpoint.rtc.sdp_api();
        let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = api
            .apply()
            .ok_or_else(|| anyhow::anyhow!("Failed to create SDP offer"))?;

        let offer_str = offer.to_sdp_string();
        endpoint.pending_offer = Some(pending);
        // For the offer creator, Event::MediaAdded doesn't fire — set mid directly
        endpoint.audio_mid = Some(mid);

        endpoint.state = EndpointState::Connecting;
        endpoint.start_recv_task(packet_tx);

        Ok((endpoint, offer_str))
    }

    /// Accept a remote SDP answer (after we created an offer)
    pub fn accept_answer(&mut self, answer_sdp: &str) -> anyhow::Result<()> {
        let pending = self
            .pending_offer
            .take()
            .ok_or_else(|| anyhow::anyhow!("No pending offer to accept answer for"))?;

        let answer = SdpAnswer::from_sdp_string(answer_sdp).or_else(|_| {
            serde_json::from_str::<SdpAnswer>(answer_sdp)
                .map_err(|e| anyhow::anyhow!("Failed to parse SDP answer: {e}"))
        })?;

        self.rtc.sdp_api().accept_answer(pending, answer)?;

        Ok(())
    }

    /// Perform an ICE restart, returning a new SDP offer
    pub fn ice_restart(&mut self) -> anyhow::Result<String> {
        let mut api = self.rtc.sdp_api();
        let _creds = api.ice_restart(true); // keep local candidates
        let (offer, pending) = api
            .apply()
            .ok_or_else(|| anyhow::anyhow!("Failed to create ICE restart offer"))?;

        let offer_str = offer.to_sdp_string();
        self.pending_offer = Some(pending);
        Ok(offer_str)
    }

    /// Feed a received UDP packet into the str0m state machine
    pub fn handle_receive(
        &mut self,
        source: SocketAddr,
        data: &[u8],
        now: Instant,
    ) -> anyhow::Result<()> {
        let receive = Receive::new(Protocol::Udp, source, self.local_addr, data)?;
        let input = Input::Receive(now, receive);
        self.rtc.handle_input(input)?;
        Ok(())
    }

    /// Handle a timeout
    pub fn handle_timeout(&mut self, now: Instant) -> anyhow::Result<()> {
        self.rtc.handle_input(Input::Timeout(now))?;
        Ok(())
    }

    /// Poll str0m for output, returning events and transmits.
    /// Returns the next timeout.
    pub fn poll_output(&mut self) -> anyhow::Result<(Vec<WebRtcEvent>, Instant)> {
        let mut events = Vec::new();

        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(when) => {
                    return Ok((events, when));
                }
                Output::Transmit(transmit) => {
                    // Use non-blocking send to avoid spawning a task per packet.
                    // UDP sends almost never block; if the socket isn't ready we
                    // drop the packet (acceptable for real-time media).
                    if let Err(e) = self
                        .socket
                        .try_send_to(&transmit.contents, transmit.destination)
                    {
                        trace!(error = %e, "UDP send dropped (would block)");
                    }
                }
                Output::Event(event) => match event {
                    Event::Connected => {
                        debug!(endpoint_id = %self.id, "WebRTC connected");
                        let old = self.state;
                        self.state = EndpointState::Connected;
                        events.push(WebRtcEvent::StateChanged {
                            old,
                            new: self.state,
                        });
                    }
                    Event::IceConnectionStateChange(ice_state) => {
                        debug!(endpoint_id = %self.id, ?ice_state, "ICE state change");
                        match ice_state {
                            IceConnectionState::Connected | IceConnectionState::Completed => {
                                if self.state != EndpointState::Connected {
                                    let old = self.state;
                                    self.state = EndpointState::Connected;
                                    events.push(WebRtcEvent::StateChanged {
                                        old,
                                        new: self.state,
                                    });
                                }
                            }
                            IceConnectionState::Disconnected => {
                                if self.state != EndpointState::Disconnected {
                                    let old = self.state;
                                    self.state = EndpointState::Disconnected;
                                    events.push(WebRtcEvent::StateChanged {
                                        old,
                                        new: self.state,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::RtpPacket(pkt) => {
                        self.stats.record_inbound(pkt.payload.len());
                        events.push(WebRtcEvent::RtpPacket(RoutedRtpPacket {
                            source_endpoint_id: self.id,
                            payload_type: *pkt.header.payload_type,
                            sequence_number: pkt.header.sequence_number,
                            timestamp: pkt.header.timestamp,
                            ssrc: *pkt.header.ssrc,
                            marker: pkt.header.marker,
                            payload: pkt.payload,
                        }));
                    }
                    Event::MediaAdded(media) => {
                        debug!(endpoint_id = %self.id, mid = %media.mid, kind = ?media.kind, "media added");
                        if media.kind == MediaKind::Audio {
                            self.audio_mid = Some(media.mid);
                        }
                    }
                    _ => {
                        trace!(endpoint_id = %self.id, "unhandled str0m event");
                    }
                },
            }
        }
    }

    /// Write an RTP packet out through this endpoint
    pub fn write_rtp(&mut self, packet: &RoutedRtpPacket) -> anyhow::Result<()> {
        let mid = self
            .audio_mid
            .ok_or_else(|| anyhow::anyhow!("No audio mid negotiated"))?;

        let pt = packet.payload_type.into();
        let seq_no: str0m::rtp::SeqNo = (packet.sequence_number as u64).into();

        let mut api = self.rtc.direct_api();
        let stream_tx = api
            .stream_tx_by_mid(mid, None)
            .ok_or_else(|| anyhow::anyhow!("No TX stream for mid {mid}"))?;

        // Clone needed: str0m takes ownership, but the same packet may route to multiple destinations
        stream_tx.write_rtp(
            pt,
            seq_no,
            packet.timestamp,
            Instant::now(),
            packet.marker,
            str0m::rtp::ExtensionValues::default(),
            false,
            packet.payload.clone(),
        )?;

        self.stats.record_outbound(packet.payload.len());
        Ok(())
    }

    /// Stop recv tasks for transfer. Cancels the token, awaits the task,
    /// and creates a fresh CancellationToken for restart.
    pub async fn stop_recv_tasks(&mut self) {
        self.cancel_token.cancel();
        if let Some(handle) = self.recv_task.take() {
            let _ = handle.await;
        }
        self.cancel_token = CancellationToken::new();
    }

    /// Restart recv tasks with a new packet_tx (after transfer to a new session).
    pub fn restart_recv_tasks(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        self.start_recv_task(packet_tx);
    }
}

impl Drop for WebRtcEndpoint {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        if let Some(handle) = self.recv_task.take() {
            handle.abort();
        }
    }
}

/// Events produced by a WebRTC endpoint
#[derive(Debug)]
pub enum WebRtcEvent {
    StateChanged {
        old: EndpointState,
        new: EndpointState,
    },
    RtpPacket(RoutedRtpPacket),
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::change::{SdpAnswer, SdpOffer};
    use str0m::media::{Direction, MediaKind};

    /// Diagnostic: verify str0m RTP mode media flow between two instances.
    /// Server (ICE lite) creates offer → client accepts → ICE → server writes RTP → client receives.
    #[test]
    fn test_str0m_rtp_mode_media_exchange() {
        let server_addr: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let client_addr: std::net::SocketAddr = "127.0.0.1:40001".parse().unwrap();

        // Server: ICE lite + RTP mode (matches rtpbridge config)
        let mut server = RtcConfig::new()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .build(Instant::now());
        server.add_local_candidate(Candidate::host(server_addr, "udp").unwrap());

        let mut api = server.sdp_api();
        let offer_mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = api.apply().unwrap();
        let offer_str = offer.to_sdp_string();
        // For the offer creator, MediaAdded doesn't fire — mid is known from add_media
        let server_mid: Option<Mid> = Some(offer_mid);

        // Client: RTP mode, not ICE lite
        let mut client = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
        client.add_local_candidate(Candidate::host(client_addr, "udp").unwrap());

        let answer = client
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&offer_str).unwrap())
            .unwrap();
        let answer_str = answer.to_sdp_string();

        server
            .sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(&answer_str).unwrap())
            .unwrap();

        // Drive ICE: exchange STUN packets until connected
        let mut s2c: Vec<Vec<u8>> = Vec::new();
        let mut c2s: Vec<Vec<u8>> = Vec::new();
        let start = Instant::now();
        let mut ice_connected = false;
        let mut client_rtp_count = 0u32;
        let mut wrote_rtp = false;
        let mut write_errors: Vec<String> = Vec::new();

        for i in 0..500 {
            let now = start + std::time::Duration::from_millis(i * 10);

            // Drive server
            loop {
                match server.poll_output() {
                    Ok(Output::Transmit(t)) => s2c.push(t.contents.to_vec()),
                    Ok(Output::Event(e)) => {
                        if let Event::IceConnectionStateChange(IceConnectionState::Connected)
                        | Event::IceConnectionStateChange(IceConnectionState::Completed)
                        | Event::Connected = &e
                        {
                            ice_connected = true;
                        }
                        // MediaAdded doesn't fire for the offer creator;
                        // server_mid is set from add_media() above.
                    }
                    Ok(Output::Timeout(_)) => {
                        server.handle_input(Input::Timeout(now)).ok();
                        break;
                    }
                    Err(_) => break,
                }
            }

            // Drive client
            loop {
                match client.poll_output() {
                    Ok(Output::Transmit(t)) => c2s.push(t.contents.to_vec()),
                    Ok(Output::Event(e)) => {
                        if let Event::RtpPacket(_) = &e {
                            client_rtp_count += 1;
                        }
                    }
                    Ok(Output::Timeout(_)) => {
                        client.handle_input(Input::Timeout(now)).ok();
                        break;
                    }
                    Err(_) => break,
                }
            }

            // Deliver packets s2c
            for data in s2c.drain(..) {
                if let Ok(r) = Receive::new(Protocol::Udp, server_addr, client_addr, &data) {
                    client.handle_input(Input::Receive(now, r)).ok();
                }
            }
            // Deliver packets c2s
            for data in c2s.drain(..) {
                if let Ok(r) = Receive::new(Protocol::Udp, client_addr, server_addr, &data) {
                    server.handle_input(Input::Receive(now, r)).ok();
                }
            }

            // After ICE connects, write RTP from server to client
            if ice_connected && !wrote_rtp && server_mid.is_some() && i > 100 {
                wrote_rtp = true;
                let mid = server_mid.unwrap();
                for seq in 0..10u64 {
                    let mut api = server.direct_api();
                    match api.stream_tx_by_mid(mid, None) {
                        Some(stream) => {
                            let result = stream.write_rtp(
                                111.into(),
                                seq.into(),
                                (seq as u32) * 960,
                                now,
                                seq == 0,
                                str0m::rtp::ExtensionValues::default(),
                                false,
                                vec![0x80u8; 160],
                            );
                            if let Err(e) = result {
                                write_errors.push(format!("seq {seq}: {e}"));
                            }
                        }
                        None => {
                            write_errors.push(format!("seq {seq}: no stream_tx for mid {mid}"));
                        }
                    }
                }
            }
        }

        eprintln!(
            "str0m diag: ice_connected={ice_connected}, server_mid={server_mid:?}, \
             wrote_rtp={wrote_rtp}, client_rtp_count={client_rtp_count}, \
             write_errors={write_errors:?}"
        );

        assert!(ice_connected, "ICE should connect");
        assert!(server_mid.is_some(), "server should have audio mid");
        assert!(wrote_rtp, "should have attempted write_rtp");

        if !write_errors.is_empty() {
            eprintln!("write_rtp errors (explains why client got 0 RTP): {write_errors:?}");
        }

        assert!(
            client_rtp_count > 0,
            "client should receive RTP packets written by server via str0m"
        );
    }

    /// Regression: create_offer with RecvOnly mixing direction must produce
    /// sendrecv SDP and allow write_rtp (TX stream must exist for mix delivery).
    #[tokio::test]
    async fn test_create_offer_recvonly_produces_sendrecv_sdp() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let (ep, offer_sdp) =
            WebRtcEndpoint::create_offer(id, EndpointDirection::RecvOnly, bind_addr, tx)
                .await
                .expect("create_offer should succeed");

        // SDP must be sendrecv — mixing direction is routing-table-only
        assert!(
            offer_sdp.contains("a=sendrecv"),
            "RecvOnly endpoint SDP must contain sendrecv, got:\n{offer_sdp}"
        );
        assert!(
            !offer_sdp.contains("a=recvonly"),
            "RecvOnly endpoint SDP must NOT contain recvonly"
        );

        // The endpoint's mixing direction is still RecvOnly
        assert_eq!(ep.config.direction, EndpointDirection::RecvOnly);

        // audio_mid must be set (TX stream exists)
        assert!(ep.audio_mid.is_some(), "audio_mid must be set");
    }

    /// Regression: create_offer with SendOnly mixing direction must also
    /// produce sendrecv SDP (so the remote peer sends RTP that we can receive,
    /// even though routing won't forward it to other endpoints).
    #[tokio::test]
    async fn test_create_offer_sendonly_produces_sendrecv_sdp() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let (_ep, offer_sdp) =
            WebRtcEndpoint::create_offer(id, EndpointDirection::SendOnly, bind_addr, tx)
                .await
                .expect("create_offer should succeed");

        assert!(
            offer_sdp.contains("a=sendrecv"),
            "SendOnly endpoint SDP must contain sendrecv, got:\n{offer_sdp}"
        );
        assert!(
            !offer_sdp.contains("a=sendonly"),
            "SendOnly endpoint SDP must NOT contain sendonly"
        );
    }

    /// Regression: full end-to-end test that a RecvOnly mixing endpoint can
    /// deliver RTP to the remote peer (spy/listen scenario).
    #[test]
    fn test_recvonly_endpoint_delivers_rtp_to_client() {
        let server_addr: std::net::SocketAddr = "127.0.0.1:40010".parse().unwrap();
        let client_addr: std::net::SocketAddr = "127.0.0.1:40011".parse().unwrap();

        let mut server = RtcConfig::new()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .build(Instant::now());
        server.add_local_candidate(Candidate::host(server_addr, "udp").unwrap());

        // After fix: create_offer always uses SendRecv for str0m
        let mut api = server.sdp_api();
        let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = api.apply().unwrap();
        let offer_str = offer.to_sdp_string();

        let mut client = RtcConfig::new().set_rtp_mode(true).build(Instant::now());
        client.add_local_candidate(Candidate::host(client_addr, "udp").unwrap());
        let answer = client
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(&offer_str).unwrap())
            .unwrap();
        server
            .sdp_api()
            .accept_answer(
                pending,
                SdpAnswer::from_sdp_string(&answer.to_sdp_string()).unwrap(),
            )
            .unwrap();

        // Drive ICE + deliver RTP from server to client
        let mut s2c: Vec<Vec<u8>> = Vec::new();
        let mut c2s: Vec<Vec<u8>> = Vec::new();
        let start = Instant::now();
        let mut ice_connected = false;
        let mut client_rtp_count = 0u32;
        let mut wrote_rtp = false;

        for i in 0..500 {
            let now = start + std::time::Duration::from_millis(i * 10);

            loop {
                match server.poll_output() {
                    Ok(Output::Transmit(t)) => s2c.push(t.contents.to_vec()),
                    Ok(Output::Event(e)) => {
                        if matches!(
                            &e,
                            Event::IceConnectionStateChange(IceConnectionState::Connected)
                                | Event::IceConnectionStateChange(IceConnectionState::Completed)
                                | Event::Connected
                        ) {
                            ice_connected = true;
                        }
                    }
                    Ok(Output::Timeout(_)) => {
                        server.handle_input(Input::Timeout(now)).ok();
                        break;
                    }
                    Err(_) => break,
                }
            }

            loop {
                match client.poll_output() {
                    Ok(Output::Transmit(t)) => c2s.push(t.contents.to_vec()),
                    Ok(Output::Event(e)) => {
                        if matches!(&e, Event::RtpPacket(_)) {
                            client_rtp_count += 1;
                        }
                    }
                    Ok(Output::Timeout(_)) => {
                        client.handle_input(Input::Timeout(now)).ok();
                        break;
                    }
                    Err(_) => break,
                }
            }

            for data in s2c.drain(..) {
                if let Ok(r) = Receive::new(Protocol::Udp, server_addr, client_addr, &data) {
                    client.handle_input(Input::Receive(now, r)).ok();
                }
            }
            for data in c2s.drain(..) {
                if let Ok(r) = Receive::new(Protocol::Udp, client_addr, server_addr, &data) {
                    server.handle_input(Input::Receive(now, r)).ok();
                }
            }

            if ice_connected && !wrote_rtp && i > 100 {
                wrote_rtp = true;
                for seq in 0..10u64 {
                    let mut api = server.direct_api();
                    let stream = api
                        .stream_tx_by_mid(mid, None)
                        .expect("TX stream must exist for recvonly mixing endpoint");
                    stream
                        .write_rtp(
                            111.into(),
                            seq.into(),
                            (seq as u32) * 960,
                            now,
                            seq == 0,
                            str0m::rtp::ExtensionValues::default(),
                            false,
                            vec![0x80u8; 160],
                        )
                        .expect("write_rtp must succeed");
                }
            }
        }

        assert!(ice_connected, "ICE should connect");
        assert!(wrote_rtp, "should have written RTP");
        assert!(
            client_rtp_count > 0,
            "spy phone must receive RTP from the session mix"
        );
    }
}
