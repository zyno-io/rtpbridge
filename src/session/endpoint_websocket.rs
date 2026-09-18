use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Instant;

use crate::control::ws_io::{NetworkTasks, Writer};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};
use uuid::Uuid;

use super::endpoint::{EndpointConfig, InboundPacket, RoutedRtpPacket};
use super::media_session::SessionCommand;
use super::stats::EndpointStats;
use crate::control::protocol::{
    EndpointDirection, EndpointDirectionUpdate, EndpointId, EndpointState,
};
use crate::control::transport::ServerWebSocket;

/// The concrete WebSocket type handed to a WS audio endpoint after the
/// HTTP upgrade completes on the control/HTTP listener.
pub type AudioWsStream = ServerWebSocket;

/// L16 payload type used internally (matches bridge endpoints).
const L16_PT: u8 = 127;
/// Bounded outbound queue (session -> IO task), in 20 ms frames (~2 s).
const OUTBOUND_QUEUE_FRAMES: usize = 100;

/// A WebSocket audio-streaming endpoint.
///
/// Internally this is an L16 endpoint (PT 127) at the wire `sample_rate`, like
/// [`super::endpoint_bridge::BridgeEndpoint`] (which is pinned to 48 kHz), but its transport is a
/// WebSocket binary stream rather than an in-process channel, and it synthesizes its own monotonic
/// inbound RTP timeline (Bridge's `ts=0` would freeze the wire timestamp at any downstream RTP
/// endpoint — see `RtpEndpoint::advance_outbound_timeline`). Running internally at the wire rate
/// means the session's per-edge resampling converts to peers directly (e.g. 16 kHz↔8 kHz) instead
/// of detouring every frame through 48 kHz.
///
/// The peer dials in to `/audio/<connect_token>`; the audio socket is then handed to this
/// endpoint via [`WebSocketEndpoint::attach_io`], which spawns a single IO task that pumps
/// audio in both directions, reframing the wire stream into 20 ms L16 packets (no resampling —
/// the internal rate is the wire rate).
pub struct WebSocketEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,

    /// Wire sample rate (8000 / 16000 / 48000). Mono, 16-bit LE.
    pub sample_rate: u32,
    /// Number of 20 ms frames coalesced into one outbound WS message. 0 = passthrough.
    flush_frames: usize,
    /// Single-use token the peer presents on the audio WS to bind to this endpoint.
    pub connect_token: Uuid,
    /// Baseline direction used in `auto` mode.
    auto_direction: EndpointDirection,

    /// Session -> IO task: native-rate L16 frames awaiting WS transmission.
    outbound_tx: mpsc::Sender<Vec<u8>>,
    /// Taken by `attach_io` when the audio socket connects.
    outbound_rx: Option<mpsc::Receiver<Vec<u8>>>,

    cancel: CancellationToken,
    io_task: Option<tokio::task::JoinHandle<()>>,
    connected: bool,
}

impl WebSocketEndpoint {
    /// Create a WS audio endpoint. Starts in `Connecting` (not routed) until the
    /// audio socket attaches via [`attach_io`](Self::attach_io).
    pub fn new(
        id: EndpointId,
        direction: EndpointDirection,
        sample_rate: u32,
        flush_ms: u32,
    ) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_QUEUE_FRAMES);
        Self {
            id,
            config: EndpointConfig { direction },
            state: EndpointState::Connecting,
            stats: EndpointStats::new(),
            sample_rate,
            flush_frames: (flush_ms / 20) as usize,
            connect_token: Uuid::new_v4(),
            auto_direction: direction,
            outbound_tx,
            outbound_rx: Some(outbound_rx),
            cancel: CancellationToken::new(),
            io_task: None,
            connected: false,
        }
    }

    /// Whether the audio socket has attached.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Codec label for stats/info, e.g. `"L16/8000"`.
    pub fn codec_label(&self) -> String {
        format!("L16/{}", self.sample_rate)
    }

    /// Apply a direction override (`auto` reverts to the creation direction).
    pub fn set_direction_override(&mut self, update: EndpointDirectionUpdate) {
        self.config.direction = update.as_direction().unwrap_or(self.auto_direction);
    }

    /// Queue an outbound frame for the WS peer. The routed payload is already
    /// native-rate L16 (the WS endpoint's send codec), so the IO task only
    /// reframes/coalesces for the wire. Returns `Ok(None)` — nothing is written
    /// to the wire here (no recording tap), matching bridge semantics. Drops
    /// newest on full.
    pub fn write_rtp(&mut self, packet: &RoutedRtpPacket) -> anyhow::Result<Option<Vec<u8>>> {
        match self.outbound_tx.try_send(packet.payload.clone()) {
            Ok(()) => self.stats.record_outbound(packet.payload.len()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                trace!(ws_id = %self.id, "ws outbound frame dropped (backpressure)");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                trace!(ws_id = %self.id, "ws outbound channel closed");
            }
        }
        Ok(None)
    }

    /// Wrap one native-rate L16 frame as a raw inbound packet. The synthesized monotonic
    /// timeline (seq/ts/ssrc, silence-fill, talkspurt marker) is owned by this source's
    /// `playout::SynthClock`, which paces the frames onto the shared grid — so the
    /// seq/ts/ssrc here are placeholders the buffer overwrites. Records inbound stats.
    pub fn wrap_inbound(&mut self, payload: Vec<u8>) -> RoutedRtpPacket {
        self.stats.record_inbound(payload.len());
        RoutedRtpPacket {
            source_endpoint_id: self.id,
            payload_type: L16_PT,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0,
            marker: false,
            payload,
        }
    }

    /// Bind a freshly-upgraded audio WebSocket to this endpoint, spawning the IO task.
    /// Transitions the endpoint to `Connected` (caller must `rebuild_routing`).
    pub fn attach_io(
        &mut self,
        ws: AudioWsStream,
        packet_tx: mpsc::Sender<InboundPacket>,
        cmd_tx: mpsc::Sender<SessionCommand>,
        permit: OwnedSemaphorePermit,
    ) -> anyhow::Result<()> {
        if self.connected {
            anyhow::bail!("websocket endpoint already has an audio connection");
        }
        let outbound_rx = self
            .outbound_rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("websocket endpoint outbound channel already taken"))?;

        let completion = cmd_tx
            .try_reserve_owned()
            .map_err(|_| anyhow::anyhow!("WS_BUSY"))?;
        self.connected = true;
        self.state = EndpointState::Connected;

        let handle = tokio::spawn(ws_io_task(
            ws,
            outbound_rx,
            packet_tx,
            completion,
            self.id,
            self.sample_rate,
            self.flush_frames,
            self.cancel.clone(),
            permit,
        ));
        self.io_task = Some(handle);
        Ok(())
    }

    /// Cancel and await the IO task (used on explicit teardown / transfer paths).
    pub async fn stop_io_task(&mut self) {
        self.cancel.cancel();
        if let Some(mut handle) = self.io_task.take() {
            let stopped = tokio::time::timeout(Duration::from_secs(1), &mut handle).await;
            if stopped.is_err() {
                handle.abort();
            }
        }
        self.cancel = CancellationToken::new();
    }
}

impl Drop for WebSocketEndpoint {
    fn drop(&mut self) {
        // Cooperative cancel + hard abort, mirroring RtpEndpoint::drop, so the IO
        // task can't outlive the endpoint when the session clears endpoints directly.
        self.cancel.cancel();
        if let Some(handle) = self.io_task.take() {
            handle.abort();
        }
    }
}

impl std::fmt::Debug for WebSocketEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketEndpoint")
            .field("id", &self.id)
            .field("sample_rate", &self.sample_rate)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

/// Decode L16 LE bytes to i16 samples (drops a trailing odd byte if present).
#[cfg(test)]
fn l16_to_samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Encode i16 samples to L16 LE bytes.
#[cfg(test)]
fn samples_to_l16(samples: &[i16], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

struct Disconnect {
    completion: Option<mpsc::OwnedPermit<SessionCommand>>,
    endpoint_id: EndpointId,
}
impl Drop for Disconnect {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            completion.send(SessionCommand::WebSocketDisconnected {
                endpoint_id: self.endpoint_id,
            });
        }
    }
}

/// Socket input and output are independent. Accepted PCM is paced to the media
/// grid with a finite byte budget; overload closes the endpoint explicitly.
#[allow(clippy::too_many_arguments)]
async fn ws_io_task(
    ws: AudioWsStream,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    packet_tx: mpsc::Sender<InboundPacket>,
    completion: mpsc::OwnedPermit<SessionCommand>,
    endpoint_id: EndpointId,
    sample_rate: u32,
    flush_frames: usize,
    cancel: CancellationToken,
    _permit: OwnedSemaphorePermit,
) {
    let _disconnect = Disconnect {
        completion: Some(completion),
        endpoint_id,
    };
    let (sink, mut ws_rx) = ws.split();
    let (writer, writer_task) = Writer::spawn(sink, cancel.clone());
    let output_writer = writer.clone();
    let output_cancel = cancel.clone();
    let frame_bytes = (sample_rate / 50) as usize * 2;
    let flush_bytes = flush_frames * frame_bytes;
    let output_task = tokio::spawn(async move {
        let mut coalesce = Vec::new();
        loop {
            let frame = tokio::select! {
                _ = output_cancel.cancelled() => break,
                frame = outbound_rx.recv() => match frame { Some(frame) => frame, None => break },
            };
            if flush_bytes == 0 {
                let sent = output_writer.send(Message::Binary(frame.into())).await;
                if sent.is_err() {
                    break;
                }
            } else {
                if frame.len() > 256 * 1024 || coalesce.len() + frame.len() > 256 * 1024 {
                    break;
                }
                coalesce.extend_from_slice(&frame);
                if coalesce.len() >= flush_bytes {
                    let frame = std::mem::take(&mut coalesce);
                    let sent = output_writer.send(Message::Binary(frame.into())).await;
                    if sent.is_err() {
                        break;
                    }
                }
            }
        }
        if !coalesce.is_empty() {
            let _ = output_writer.send(Message::Binary(coalesce.into())).await;
        }
        output_cancel.cancel();
    });
    let _network = NetworkTasks {
        cancel: cancel.clone(),
        tasks: vec![writer_task, output_task],
    };
    let mut incoming = std::collections::VecDeque::<u8>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut pending: Option<(Vec<u8>, tokio::time::Instant)> = None;
    loop {
        let deadline = pending
            .as_ref()
            .map(|(_, deadline)| *deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86400));
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep_until(deadline), if pending.is_some() => break,
            _ = ping.tick(), if pending.is_none() => {
                let nonce = rand::random::<u64>().to_be_bytes().to_vec();
                let sent = writer.send(Message::Ping(nonce.clone().into())).await;
                if sent.is_err() { break; }
                pending = Some((nonce, tokio::time::Instant::now() + Duration::from_secs(10)));
            }
            _ = tick.tick(), if incoming.len() >= frame_bytes => {
                match packet_tx.try_reserve() {
                    Ok(permit) => {
                        let data = incoming.drain(..frame_bytes).collect();
                        permit.send(InboundPacket { endpoint_id,
                            source: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                            data, recv_at: Instant::now(), is_rtcp: false, local: None,
                        });
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                    Err(_) => {} // Keep accepted bytes until the next tick.
                }
            }
            message = ws_rx.next() => match message {
                Some(Ok(Message::Binary(data))) => {
                    if data.len() > 256 * 1024 - incoming.len() {
                        debug!(%endpoint_id, "WS_AUDIO_BACKPRESSURE");
                        break;
                    }
                    incoming.extend(data.iter().copied());
                }
                Some(Ok(Message::Ping(data))) => {
                    let sent = writer.send(Message::Pong(data)).await;
                    if sent.is_err() { break; }
                }
                Some(Ok(Message::Pong(data))) => {
                    if pending.as_ref().is_some_and(|(nonce, _)| nonce.as_slice() == data.as_ref()) { pending = None; }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
    let _ = writer.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::transport::{BoxedServerIo, PrefixedIo};
    use futures_util::SinkExt;
    use std::sync::Arc;
    use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};

    #[tokio::test(start_paused = true)]
    async fn maximum_burst_survives_backpressure_and_keeps_20ms_pacing() {
        let (socket, peer) = tokio::io::duplex(512 * 1024);
        let io = PrefixedIo::new(Vec::new(), Box::new(socket) as BoxedServerIo);
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        let mut peer = WebSocketStream::from_raw_socket(peer, Role::Client, None).await;
        let mut endpoint = ep(8000, 0);
        let (packets, mut received) = mpsc::channel(1);
        let (commands, _commands_rx) = mpsc::channel(1);
        let budget = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&budget).try_acquire_owned().unwrap();
        endpoint.attach_io(ws, packets, commands, permit).unwrap();
        let mut expected: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        let sent = peer.send(Message::Binary(expected.clone().into())).await;
        sent.unwrap();
        tokio::task::yield_now().await;
        // Fill the media queue and force several unsuccessful drain attempts.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut actual = Vec::new();
        let mut previous = tokio::time::Instant::now();
        for index in 0..expected.len() / 320 {
            let packet = received.recv().await;
            let packet = packet.expect("accepted burst must remain connected");
            let now = tokio::time::Instant::now();
            if index >= 2 {
                assert!(now - previous >= Duration::from_millis(20));
            }
            previous = now;
            actual.extend(packet.data);
        }
        let padding = vec![0; 320 - expected.len() % 320];
        expected.extend_from_slice(&padding);
        let sent = peer.send(Message::Binary(padding.into())).await;
        sent.unwrap();
        let packet = received.recv().await;
        actual.extend(packet.unwrap().data);
        assert_eq!(actual, expected);
        endpoint.stop_io_task().await;
        assert_eq!(budget.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_pong_disconnects_once_and_releases_connection_admission() {
        let (socket, _silent_peer) = tokio::io::duplex(4096);
        let io = PrefixedIo::new(Vec::new(), Box::new(socket) as BoxedServerIo);
        let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
        let mut endpoint = ep(8000, 0);
        let (packets, _received) = mpsc::channel(1);
        let (commands, mut commands_rx) = mpsc::channel(1);
        let budget = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::clone(&budget).try_acquire_owned().unwrap();
        endpoint.attach_io(ws, packets, commands, permit).unwrap();
        let disconnected = tokio::time::timeout(Duration::from_secs(41), commands_rx.recv()).await;
        assert!(
            matches!(disconnected.unwrap(), Some(SessionCommand::WebSocketDisconnected { endpoint_id }) if endpoint_id == endpoint.id)
        );
        endpoint.stop_io_task().await;
        assert_eq!(budget.available_permits(), 1);
        assert!(commands_rx.try_recv().is_err());
    }

    fn ep(sample_rate: u32, flush_ms: u32) -> WebSocketEndpoint {
        WebSocketEndpoint::new(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            sample_rate,
            flush_ms,
        )
    }

    #[test]
    fn new_starts_connecting_and_not_attached() {
        let e = ep(8000, 0);
        assert_eq!(e.state, EndpointState::Connecting);
        assert!(!e.is_connected());
        assert_eq!(e.sample_rate, 8000);
        assert_eq!(e.codec_label(), "L16/8000");
        assert!(e.outbound_rx.is_some());
    }

    #[test]
    fn flush_ms_converts_to_frames() {
        assert_eq!(ep(8000, 0).flush_frames, 0);
        assert_eq!(ep(8000, 20).flush_frames, 1);
        assert_eq!(ep(8000, 100).flush_frames, 5);
    }

    #[test]
    fn write_rtp_records_and_never_blocks_on_full() {
        let mut e = ep(8000, 0);
        let pkt = RoutedRtpPacket {
            source_endpoint_id: EndpointId::new_v4(),
            payload_type: 127,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0,
            marker: false,
            payload: vec![0u8; 320], // one 20 ms L16 frame at 8 kHz (160 samples)
        };
        // Queue capacity is bounded; pushing far more than capacity must not panic
        // or block (drop-newest on full).
        for _ in 0..(OUTBOUND_QUEUE_FRAMES + 50) {
            assert!(e.write_rtp(&pkt).unwrap().is_none());
        }
        // At least `capacity` frames were accepted and counted.
        assert!(e.stats.outbound_packets >= OUTBOUND_QUEUE_FRAMES as u64);
    }

    #[test]
    fn wrap_inbound_records_stats_and_uses_placeholder_timeline() {
        let mut e = ep(8000, 0);
        let p0 = e.wrap_inbound(vec![0u8; 320]);
        let p1 = e.wrap_inbound(vec![0u8; 320]);
        assert_eq!(p0.payload_type, 127);
        assert_eq!(p0.source_endpoint_id, e.id);
        // The SynthClock owns the real timeline; these are placeholders (0/0/0).
        assert_eq!((p1.sequence_number, p1.timestamp, p1.ssrc), (0, 0, 0));
        assert_eq!(e.stats.inbound_packets, 2);
    }

    #[test]
    fn set_direction_override_auto_reverts() {
        let mut e = ep(8000, 0);
        e.set_direction_override(EndpointDirectionUpdate::RecvOnly);
        assert_eq!(e.config.direction, EndpointDirection::RecvOnly);
        e.set_direction_override(EndpointDirectionUpdate::Auto);
        assert_eq!(e.config.direction, EndpointDirection::SendRecv);
    }

    #[test]
    fn l16_roundtrip_and_odd_byte() {
        let samples = vec![0i16, 1, -1, 32767, -32768];
        let mut bytes = Vec::new();
        samples_to_l16(&samples, &mut bytes);
        assert_eq!(bytes.len(), samples.len() * 2);
        assert_eq!(l16_to_samples(&bytes), samples);
        // Odd trailing byte is dropped by chunks_exact.
        bytes.push(0x7f);
        assert_eq!(l16_to_samples(&bytes), samples);
    }
}
