use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_tungstenite::WebSocketStream;
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

/// The concrete WebSocket type handed to a WS audio endpoint after the
/// HTTP upgrade completes on the control/HTTP listener.
pub type AudioWsStream = WebSocketStream<TcpStream>;

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

        self.connected = true;
        self.state = EndpointState::Connected;

        let handle = tokio::spawn(ws_io_task(
            ws,
            outbound_rx,
            packet_tx,
            cmd_tx,
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
        if let Some(handle) = self.io_task.take() {
            let _ = handle.await;
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
fn l16_to_samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Encode i16 samples to L16 LE bytes.
fn samples_to_l16(samples: &[i16], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

/// Single combined IO task: pumps audio both directions over one WS, handles
/// Ping/Pong/Close, reframes the wire stream into exact 20 ms L16 packets at the
/// wire `sample_rate` (no resampling — the internal rate is the wire rate), and
/// coalesces outbound frames per `flush_frames` (0 = passthrough). Owns the
/// connection-limit permit for the socket's lifetime.
#[allow(clippy::too_many_arguments)]
async fn ws_io_task(
    ws: AudioWsStream,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    packet_tx: mpsc::Sender<InboundPacket>,
    cmd_tx: mpsc::Sender<SessionCommand>,
    endpoint_id: EndpointId,
    sample_rate: u32,
    flush_frames: usize,
    cancel: CancellationToken,
    _permit: OwnedSemaphorePermit,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let null_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

    // 20 ms of audio at the wire rate (samples / bytes). The session's internal
    // L16 format for this endpoint is the wire rate itself, so the IO task does
    // no resampling — it only reframes into exact 20 ms chunks.
    let frame_native = (sample_rate / 50) as usize;
    let flush_bytes = flush_frames * frame_native * 2;

    // Inbound: reassemble wire bytes into 20 ms L16 frames.
    let mut in_samples: Vec<i16> = Vec::with_capacity(frame_native * 2);
    let mut in_byte_rem: Vec<u8> = Vec::new(); // trailing odd byte across reads

    // Outbound coalescing buffers.
    let mut out_coalesce: Vec<u8> = Vec::new();
    let mut native_buf: Vec<i16> = Vec::with_capacity(frame_native + 8);
    let mut frame_bytes: Vec<u8> = Vec::with_capacity(frame_native * 2);

    'io: loop {
        tokio::select! {
            _ = cancel.cancelled() => break 'io,

            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    // Reassemble samples, carrying any odd leftover byte.
                    let mut bytes = std::mem::take(&mut in_byte_rem);
                    bytes.extend_from_slice(&data[..]);
                    let usable = bytes.len() - (bytes.len() % 2);
                    for c in bytes[..usable].chunks_exact(2) {
                        in_samples.push(i16::from_le_bytes([c[0], c[1]]));
                    }
                    if usable < bytes.len() {
                        in_byte_rem.push(bytes[usable]);
                    }
                    // Emit exact 20 ms L16 frames at the wire/native rate.
                    while in_samples.len() >= frame_native {
                        let frame: Vec<i16> = in_samples.drain(..frame_native).collect();
                        let mut payload = Vec::with_capacity(frame_native * 2);
                        samples_to_l16(&frame, &mut payload);
                        let pkt = InboundPacket {
                            endpoint_id,
                            source: null_addr,
                            data: payload,
                            is_rtcp: false,
                            local: None,
                        };
                        if packet_tx.send(pkt).await.is_err() {
                            break 'io; // session gone
                        }
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    if ws_tx.send(Message::Pong(p)).await.is_err() {
                        break 'io;
                    }
                }
                Some(Ok(Message::Close(_))) => break 'io,
                Some(Ok(_)) => {} // Text / Pong / Frame — ignore
                Some(Err(e)) => {
                    debug!(endpoint_id = %endpoint_id, error = %e, "ws audio read error");
                    break 'io;
                }
                None => break 'io,
            },

            frame = outbound_rx.recv() => match frame {
                Some(l16_frame) => {
                    // Already native-rate L16; just guarantee an exact 20 ms frame.
                    native_buf.clear();
                    native_buf.extend(l16_to_samples(&l16_frame));
                    native_buf.resize(frame_native, 0);
                    samples_to_l16(&native_buf, &mut frame_bytes);
                    if flush_bytes == 0 {
                        if ws_tx
                            .send(Message::Binary(frame_bytes.clone().into()))
                            .await
                            .is_err()
                        {
                            break 'io;
                        }
                    } else {
                        out_coalesce.extend_from_slice(&frame_bytes);
                        if out_coalesce.len() >= flush_bytes {
                            let msg = std::mem::take(&mut out_coalesce);
                            if ws_tx.send(Message::Binary(msg.into())).await.is_err() {
                                break 'io;
                            }
                        }
                    }
                }
                None => break 'io, // endpoint removed (outbound_tx dropped)
            },
        }
    }

    // Flush any partial coalesce buffer, then close cleanly.
    if !out_coalesce.is_empty() {
        let _ = ws_tx.send(Message::Binary(out_coalesce.into())).await;
    }
    let _ = ws_tx.send(Message::Close(None)).await;
    // Awaited (not try_send): a dropped disconnect would leave the endpoint stuck
    // `Connected` and routed forever. Resolves to Err harmlessly if the session is gone.
    let _ = cmd_tx
        .send(SessionCommand::WebSocketDisconnected { endpoint_id })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

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
