use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::audio_analysis;
use super::endpoint::{InboundPacket, RoutedRtpPacket};
use super::endpoint_enum::{
    Endpoint, endpoint_audio_codec, endpoint_last_rtp_timestamp, endpoint_rtp_clock_rate,
    endpoint_send_pt, endpoint_stream_descriptor,
};
use super::endpoint_file::FileEndpoint;
use super::endpoint_rtp::{RtpEndpoint, RtpMediaSecurity};
use super::endpoint_webrtc::{WebRtcEndpoint, WebRtcEvent, ice_state_str};
use super::endpoint_websocket::{AudioWsStream, WebSocketEndpoint};
use super::fax_tap;
use super::file_poll::FileRtpState;
use super::playout::{PlayoutBuffer, PlayoutKind, Policy};
use super::routing::RoutingTable;
use super::session_dtmf::{EndpointDtmf, PendingDtmfInjection};
use super::vad_tap;
use crate::control::protocol::*;
use crate::media::codec::AudioCodec;
use crate::media::dtmf::DtmfDetector;
use crate::media::fax::FaxDetector;
use crate::media::sdp;
use crate::media::transcode::TranscodePipeline;
use crate::media::vad::VadMonitor;

/// Wrapper around TranscodePipeline with LRU tracking
struct CachedTranscode {
    pipeline: TranscodePipeline,
    last_used: Instant,
}

use crate::net::socket_pool::{MediaBinding, MediaBindings};
use crate::recording::recorder::RecordingManager;

/// Bundle of endpoint state for cross-session transfer
pub struct EndpointTransferBundle {
    // Note: Debug is manually implemented below because Endpoint and AudioDecoder don't derive Debug
    pub endpoint: Endpoint,
    pub source_session_id: SessionId,
    pub dtmf_state: Option<EndpointDtmf>,
    pub sensitive_dtmf: bool,
    pub vad_monitor: Option<VadMonitor>,
    pub fax_detector: Option<FaxDetector>,
    pub analysis_decoder: Option<Box<dyn crate::media::codec::AudioDecoder>>,
    pub file_rtp_state: Option<FileRtpState>,
    pub url_source: Option<String>,
    pub media_timeout_was_emitted: bool,
}

impl std::fmt::Debug for EndpointTransferBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointTransferBundle")
            .field("source_session_id", &self.source_session_id)
            .finish_non_exhaustive()
    }
}

/// Commands sent to the media session task
#[derive(Debug)]
pub enum SessionCommand {
    Attach {
        event_tx: mpsc::Sender<Event>,
        critical_event_tx: mpsc::Sender<Event>,
        dropped_events: Arc<AtomicU64>,
    },
    Detach,
    Destroy,
    CreateFromOffer {
        reply: oneshot::Sender<anyhow::Result<(EndpointId, String)>>,
        sdp: String,
        direction: EndpointDirection,
        expected_type: Option<EndpointType>,
    },
    CreateOffer {
        reply: oneshot::Sender<anyhow::Result<(EndpointId, String)>>,
        direction: EndpointDirection,
        endpoint_type: EndpointType,
        srtp: bool,
        srtp_optional: bool,
        codecs: Option<Vec<String>>,
    },
    AcceptAnswer {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
        sdp: String,
        expected_type: Option<EndpointType>,
        /// For WebRTC ICE-restart answers: the offer generation this answer is
        /// for. `None` for the initial answer (no overlap risk). When `Some`,
        /// the session rejects the answer unless it matches the endpoint's
        /// current `offer_generation`.
        expected_generation: Option<u64>,
    },
    AcceptOffer {
        reply: oneshot::Sender<anyhow::Result<String>>,
        endpoint_id: EndpointId,
        sdp: String,
    },
    RemoveEndpoint {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
    },
    DtmfInject {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
        digit: char,
        duration_ms: u32,
        volume: u8,
    },
    DtmfSetSensitive {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
        enabled: bool,
    },
    RecordingStart {
        reply: oneshot::Sender<anyhow::Result<RecordingId>>,
        endpoint_id: Option<EndpointId>,
        file_path: String,
    },
    RecordingStop {
        reply: oneshot::Sender<anyhow::Result<(String, u64, u64, u64)>>,
        recording_id: RecordingId,
    },
    VadStart {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
        silence_interval_ms: u32,
        speech_threshold: f32,
    },
    VadStop {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
    },
    FaxDetectStart {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
    },
    FaxDetectStop {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
    },
    CreateWithFile {
        reply: oneshot::Sender<anyhow::Result<EndpointId>>,
        source: String,
        start_ms: u64,
        loop_count: Option<u32>,
        cache_ttl_secs: u32,
        timeout_ms: u32,
        shared: bool,
        headers: Option<std::collections::HashMap<String, String>>,
        gain_db: f32,
    },
    CreateWithTone {
        reply: oneshot::Sender<anyhow::Result<EndpointId>>,
        tone_type: super::endpoint_tone::ToneType,
        frequency: Option<f64>,
        duration_ms: Option<u64>,
    },
    FileReady {
        endpoint_id: EndpointId,
        result: anyhow::Result<std::path::PathBuf>,
        start_ms: u64,
        loop_count: Option<u32>,
        url: String,
    },
    FileSeek {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
        position_ms: u64,
    },
    FilePause {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
    },
    FileResume {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
    },
    IceRestart {
        reply: oneshot::Sender<anyhow::Result<(String, u64)>>,
        endpoint_id: EndpointId,
    },
    SrtpRekey {
        reply: oneshot::Sender<anyhow::Result<String>>,
        endpoint_id: EndpointId,
    },
    UpdateDirection {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
        direction: EndpointDirectionUpdate,
    },
    UpdateRemoteSdp {
        reply: oneshot::Sender<anyhow::Result<String>>,
        endpoint_id: EndpointId,
        sdp: String,
    },
    StatsSubscribe {
        reply: oneshot::Sender<anyhow::Result<()>>,
        interval_ms: u32,
        include_diagnostics: bool,
    },
    StatsUnsubscribe {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    GetInfo {
        reply: oneshot::Sender<SessionDetails>,
    },
    /// Extract an endpoint for transfer to another session
    ExtractEndpoint {
        reply: oneshot::Sender<anyhow::Result<EndpointTransferBundle>>,
        endpoint_id: EndpointId,
    },
    /// Insert a transferred endpoint into this session
    InsertEndpoint {
        reply: oneshot::Sender<Result<(), (anyhow::Error, EndpointTransferBundle)>>,
        bundle: Box<EndpointTransferBundle>,
    },
    /// Get a clone of the session's inbound packet channel sender
    GetPacketTx {
        reply: oneshot::Sender<mpsc::Sender<InboundPacket>>,
    },
    /// Insert a bridge endpoint into this session
    InsertBridgeEndpoint {
        reply: oneshot::Sender<anyhow::Result<EndpointId>>,
        bridge: super::endpoint_bridge::BridgeEndpoint,
    },
    /// Create a WebSocket audio endpoint (dial-in). Returns (endpoint_id, connect_token).
    CreateWebSocket {
        reply: oneshot::Sender<anyhow::Result<(EndpointId, uuid::Uuid)>>,
        direction: EndpointDirection,
        sample_rate: u32,
        flush_ms: u32,
    },
    /// Bind a dialed-in audio WebSocket to its endpoint. `ws`/`permit` are dropped
    /// (closing the socket, releasing the slot) if the endpoint is gone or already bound.
    AttachWebSocketAudio {
        reply: oneshot::Sender<anyhow::Result<()>>,
        endpoint_id: EndpointId,
        // Boxed: a WebSocketStream is large and would bloat every SessionCommand.
        ws: Box<AudioWsStream>,
        permit: tokio::sync::OwnedSemaphorePermit,
    },
    /// Notification from a WS audio IO task that its socket closed.
    WebSocketDisconnected {
        endpoint_id: EndpointId,
    },
}

/// Detailed session state returned by GetInfo
#[derive(Debug, serde::Serialize)]
pub struct SessionDetails {
    pub endpoints: Vec<EndpointInfo>,
    pub recordings: Vec<crate::control::protocol::RecordingInfo>,
    pub vad_active: Vec<EndpointId>,
    pub fax_detect_active: Vec<EndpointId>,
}

use tokio::sync::oneshot;

/// All mutable state for a media session, grouped to reduce parameter passing
/// and keep the select! loop thin.
struct SessionState {
    session_id: SessionId,
    media_bindings: Arc<MediaBindings>,
    media_dir: Option<std::path::PathBuf>,
    file_cache: Arc<crate::playback::file_cache::FileCache>,
    endpoint_count: Arc<std::sync::atomic::AtomicUsize>,
    max_endpoints: usize,
    metrics: Arc<crate::metrics::Metrics>,
    cmd_tx: mpsc::Sender<SessionCommand>,

    event_tx: Option<mpsc::Sender<Event>>,
    critical_event_tx: Option<mpsc::Sender<Event>>,
    dropped_events: Arc<AtomicU64>,
    endpoints: HashMap<EndpointId, Endpoint>,
    dtmf_state: HashMap<EndpointId, EndpointDtmf>,
    /// Endpoints whose DTMF values must be redacted from logs and omitted from
    /// packet recordings. The control event still carries the digit to the
    /// attached controller so it can complete the active gather.
    sensitive_dtmf_endpoints: HashSet<EndpointId>,
    routing: RoutingTable,
    recording_mgr: RecordingManager,
    vad_monitors: HashMap<EndpointId, VadMonitor>,
    stats_interval: Option<Duration>,
    stats_include_diagnostics: bool,
    last_stats_emit: Instant,
    file_rtp_states: HashMap<EndpointId, FileRtpState>,
    tone_rtp_states: HashMap<EndpointId, super::tone_poll::ToneRtpState>,
    transcode_cache: HashMap<(EndpointId, EndpointId), CachedTranscode>,
    url_sources: HashMap<EndpointId, String>,
    fax_detectors: HashMap<EndpointId, FaxDetector>,
    /// Per-endpoint audio decoders shared by VAD and fax detection. Inbound RTP
    /// is decoded to PCM once per packet (needed for G.722/Opus stateful
    /// decoding) and fed to whichever analysers are active.
    analysis_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>>,
    /// Tracks endpoints that have already emitted a media_timeout event.
    /// Cleared when the endpoint receives a packet again.
    media_timeout_emitted: std::collections::HashSet<EndpointId>,
    /// Last time check_media_timeouts was run (throttled to once per second)
    last_timeout_check: Instant,
    /// Pending DTMF injection (at most one at a time)
    dtmf_injection: Option<PendingDtmfInjection>,
    /// Shared file playback manager (cross-session shared decode)
    shared_playback: Arc<crate::playback::shared_playback::SharedPlaybackManager>,
    /// When the endpoint count dropped to zero (for empty session timeout)
    empty_since: Option<Instant>,
    /// Per-destination audio mixers for multi-party conferences (3+ endpoints).
    /// Active only for destinations receiving from 2+ sources.
    mixers: HashMap<EndpointId, super::mixer::DestinationMixer>,
    /// Per-source playout / re-pacing buffers. Clockless sources (WS/Bridge) get a
    /// Synth buffer; non-transparent RTP/WebRTC sources get a Tracked buffer. Transparent
    /// relay sources have no entry (see [`Policy::Bypass`]).
    playout_buffers: HashMap<EndpointId, PlayoutBuffer>,
    /// Engagement decision per source, recomputed on routing/tap changes.
    playout_policy: HashMap<EndpointId, Policy>,
    /// Next shared 20 ms playout-grid instant; `None` when no buffer has pending audio.
    mix_grid: Option<Instant>,
    /// Shared registry of pending WebSocket audio connect tokens. The session
    /// inserts a token per WS endpoint and removes it on endpoint/session teardown.
    ws_audio_registry: Arc<crate::control::ws_audio::WsAudioRegistry>,
}

/// A WS audio endpoint that never receives its dial-in within this window is
/// auto-removed, freeing its endpoint slot and pending connect token.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Event names that should be routed to the critical channel first.
const CRITICAL_EVENTS: &[&str] = &[
    "endpoint.state_changed",
    "endpoint.ice_state_changed",
    "recording.stopped",
    "endpoint.file.finished",
    "session.idle_timeout",
    "session.empty_timeout",
];

impl SessionState {
    /// Emit an event to the attached client, tracking drops.
    /// Critical events are routed to a priority channel first; if that fails,
    /// they fall through to the normal channel before being dropped.
    fn send_event(&self, name: &str, data: impl serde::Serialize) {
        debug!(event = name, "emitting event");
        if let Some(tx) = &self.event_tx {
            let event = Event::new(name, data);
            if CRITICAL_EVENTS.contains(&name)
                && let Some(critical_tx) = &self.critical_event_tx
            {
                match critical_tx.try_send(event) {
                    Ok(()) => return,
                    Err(mpsc::error::TrySendError::Full(event)) => {
                        // Critical channel full — fall through to normal channel
                        if tx.try_send(event).is_err() {
                            self.dropped_events.fetch_add(1, Ordering::Relaxed);
                            self.metrics.events_dropped.inc();
                            tracing::warn!(
                                event_name = name,
                                "critical event dropped: both channels full"
                            );
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.dropped_events.fetch_add(1, Ordering::Relaxed);
                        self.metrics.events_dropped.inc();
                        tracing::warn!(event_name = name, "event dropped: channel closed");
                    }
                }
                return;
            }
            if tx.try_send(event).is_err() {
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
                self.metrics.events_dropped.inc();
                tracing::warn!(event_name = name, "event dropped: channel full or closed");
            }
        }
    }

    /// Dispatch a command to the appropriate handler. Returns `false` if the
    /// session should be destroyed (Destroy command received).
    async fn handle_command(
        &mut self,
        cmd: SessionCommand,
        packet_tx: &mpsc::Sender<InboundPacket>,
    ) -> bool {
        match cmd {
            SessionCommand::Attach {
                event_tx: tx,
                critical_event_tx,
                dropped_events,
            } => {
                info!(session_id = %self.session_id, "control attached");
                self.event_tx = Some(tx);
                self.critical_event_tx = Some(critical_event_tx);
                self.dropped_events = dropped_events;
            }
            SessionCommand::Detach => {
                info!(session_id = %self.session_id, "control detached");
                self.event_tx = None;
                self.critical_event_tx = None;
            }
            SessionCommand::Destroy => {
                info!(session_id = %self.session_id, "destroying session");
                return false;
            }
            SessionCommand::CreateFromOffer {
                reply,
                sdp,
                direction,
                expected_type,
            } => {
                let result = self
                    .handle_create_from_offer(packet_tx, &sdp, direction, expected_type)
                    .await;
                if result.is_ok() {
                    self.metrics.endpoints_total.inc();
                    self.metrics.endpoints_active.inc();
                }
                let _ = reply.send(result);
            }
            SessionCommand::CreateOffer {
                reply,
                direction,
                endpoint_type,
                srtp,
                srtp_optional,
                codecs,
            } => {
                let result = self
                    .handle_create_offer(
                        packet_tx,
                        direction,
                        endpoint_type,
                        srtp,
                        srtp_optional,
                        codecs,
                    )
                    .await;
                if result.is_ok() {
                    self.metrics.endpoints_total.inc();
                    self.metrics.endpoints_active.inc();
                }
                let _ = reply.send(result);
            }
            SessionCommand::AcceptAnswer {
                reply,
                endpoint_id,
                sdp,
                expected_type,
                expected_generation,
            } => {
                let _ = reply.send(self.handle_accept_answer(
                    endpoint_id,
                    &sdp,
                    expected_type,
                    expected_generation,
                ));
            }
            SessionCommand::AcceptOffer {
                reply,
                endpoint_id,
                sdp,
            } => {
                let _ = reply.send(self.handle_accept_offer(endpoint_id, &sdp));
            }
            SessionCommand::RemoveEndpoint { reply, endpoint_id } => {
                let _ = reply.send(self.handle_remove_endpoint(endpoint_id).await);
            }
            SessionCommand::DtmfInject {
                reply,
                endpoint_id,
                digit,
                duration_ms,
                volume,
            } => {
                let _ =
                    reply.send(self.handle_dtmf_inject(&endpoint_id, digit, duration_ms, volume));
            }
            SessionCommand::DtmfSetSensitive {
                reply,
                endpoint_id,
                enabled,
            } => {
                let _ = reply.send(self.handle_dtmf_set_sensitive(endpoint_id, enabled));
            }
            SessionCommand::RecordingStart {
                reply,
                endpoint_id,
                file_path,
            } => {
                // Seed descriptors for already-negotiated endpoints so the new
                // recording is self-describing from byte 0 (start() replays the
                // cache). Without this, a recording started mid-call wouldn't carry
                // an endpoint's descriptor until its next media packet.
                let descs: Vec<(
                    EndpointId,
                    crate::recording::meta::StreamDescriptor,
                    Option<SocketAddr>,
                    Option<SocketAddr>,
                )> = self
                    .endpoints
                    .iter()
                    .filter_map(|(id, ep)| {
                        let (local, remote) = endpoint_media_addrs(ep);
                        endpoint_stream_descriptor(ep, local, remote)
                            .map(|d| (*id, d, local, remote))
                    })
                    .collect();
                for (id, d, local, remote) in &descs {
                    self.recording_mgr.note_descriptor(id, d, *local, *remote);
                }

                // Validate that the endpoint exists before starting a recording
                let result = if let Some(eid) = endpoint_id {
                    if self.endpoints.contains_key(&eid) {
                        self.recording_mgr.start(Some(eid), file_path).await
                    } else {
                        Err(anyhow::anyhow!("Endpoint not found"))
                    }
                } else {
                    self.recording_mgr.start(None, file_path).await
                };
                if result.is_ok() {
                    self.metrics.recordings_active.inc();
                }
                let _ = reply.send(result);
            }
            SessionCommand::RecordingStop {
                reply,
                recording_id,
            } => {
                let result = self.recording_mgr.stop(&recording_id);
                if result.is_ok() {
                    self.metrics.recordings_active.dec();
                }
                let _ = reply.send(result);
            }
            SessionCommand::VadStart {
                reply,
                endpoint_id,
                silence_interval_ms,
                speech_threshold,
            } => {
                let _ = reply.send(self.handle_vad_start(
                    endpoint_id,
                    silence_interval_ms,
                    speech_threshold,
                ));
            }
            SessionCommand::VadStop { reply, endpoint_id } => {
                let _ = reply.send(self.handle_vad_stop(endpoint_id));
            }
            SessionCommand::FaxDetectStart { reply, endpoint_id } => {
                let _ = reply.send(self.handle_fax_detect_start(endpoint_id));
            }
            SessionCommand::FaxDetectStop { reply, endpoint_id } => {
                let _ = reply.send(self.handle_fax_detect_stop(endpoint_id));
            }
            SessionCommand::CreateWithFile {
                reply,
                source,
                start_ms,
                loop_count,
                cache_ttl_secs,
                timeout_ms,
                shared,
                headers,
                gain_db,
            } => {
                let result = self
                    .handle_create_with_file(
                        &source,
                        start_ms,
                        loop_count,
                        cache_ttl_secs,
                        timeout_ms,
                        shared,
                        headers,
                        gain_db,
                    )
                    .await;
                if result.is_ok() {
                    self.metrics.endpoints_total.inc();
                    self.metrics.endpoints_active.inc();
                }
                let _ = reply.send(result);
            }
            SessionCommand::CreateWithTone {
                reply,
                tone_type,
                frequency,
                duration_ms,
            } => {
                let result = self.handle_create_tone(tone_type, frequency, duration_ms);
                if result.is_ok() {
                    self.metrics.endpoints_total.inc();
                    self.metrics.endpoints_active.inc();
                }
                let _ = reply.send(result);
            }
            SessionCommand::FileReady {
                endpoint_id,
                result,
                start_ms,
                loop_count,
                url,
            } => {
                self.handle_file_ready(endpoint_id, result, start_ms, loop_count, &url)
                    .await;
            }
            SessionCommand::FileSeek {
                reply,
                endpoint_id,
                position_ms,
            } => {
                let _ = reply.send(self.handle_file_seek(endpoint_id, position_ms));
            }
            SessionCommand::FilePause { reply, endpoint_id } => {
                let _ = reply.send(self.handle_file_pause(endpoint_id));
            }
            SessionCommand::FileResume { reply, endpoint_id } => {
                let _ = reply.send(self.handle_file_resume(endpoint_id));
            }
            SessionCommand::IceRestart { reply, endpoint_id } => {
                let _ = reply.send(self.handle_ice_restart(endpoint_id));
            }
            SessionCommand::SrtpRekey { reply, endpoint_id } => {
                let _ = reply.send(self.handle_srtp_rekey(endpoint_id));
            }
            SessionCommand::UpdateDirection {
                reply,
                endpoint_id,
                direction,
            } => {
                let _ = reply.send(self.handle_update_direction(endpoint_id, direction));
            }
            SessionCommand::UpdateRemoteSdp {
                reply,
                endpoint_id,
                sdp,
            } => {
                let _ = reply.send(self.handle_update_remote_sdp(endpoint_id, &sdp));
            }
            SessionCommand::StatsSubscribe {
                reply,
                interval_ms,
                include_diagnostics,
            } => {
                // First subscribe anchors the emit timeline to now, so the first
                // `stats` event lands one full interval out. A re-subscribe
                // (changing the interval) keeps the existing anchor: the new
                // interval is measured from the last actual emit, so the next
                // fire is `interval - last_stats_emit.elapsed()` away. If that's
                // already in the past, the emit gate in the run loop fires it
                // immediately on the next iteration.
                if self.stats_interval.is_none() {
                    self.last_stats_emit = Instant::now();
                }
                self.stats_interval = Some(Duration::from_millis(interval_ms as u64));
                self.stats_include_diagnostics = include_diagnostics;
                let _ = reply.send(Ok(()));
            }
            SessionCommand::StatsUnsubscribe { reply } => {
                self.stats_interval = None;
                self.stats_include_diagnostics = false;
                let _ = reply.send(Ok(()));
            }
            SessionCommand::GetInfo { reply } => {
                let _ = reply.send(self.get_info());
            }
            SessionCommand::ExtractEndpoint { reply, endpoint_id } => {
                let result = self.handle_extract_endpoint(endpoint_id).await;
                if result.is_ok() {
                    self.metrics.endpoints_active.dec();
                }
                let _ = reply.send(result);
            }
            SessionCommand::InsertEndpoint { reply, mut bundle } => {
                let result = self.handle_insert_endpoint(&mut bundle, packet_tx).await;
                match result {
                    Ok(()) => {
                        self.metrics.endpoints_total.inc();
                        self.metrics.endpoints_active.inc();
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err((e, *bundle)));
                    }
                }
            }
            SessionCommand::GetPacketTx { reply } => {
                let _ = reply.send(packet_tx.clone());
            }
            SessionCommand::InsertBridgeEndpoint { reply, bridge } => {
                let result = self.handle_insert_bridge_endpoint(bridge);
                if result.is_ok() {
                    self.metrics.endpoints_total.inc();
                    self.metrics.endpoints_active.inc();
                }
                let _ = reply.send(result);
            }
            SessionCommand::CreateWebSocket {
                reply,
                direction,
                sample_rate,
                flush_ms,
            } => {
                let result = self.handle_create_websocket(direction, sample_rate, flush_ms);
                if result.is_ok() {
                    self.metrics.endpoints_total.inc();
                    self.metrics.endpoints_active.inc();
                }
                let _ = reply.send(result);
            }
            SessionCommand::AttachWebSocketAudio {
                reply,
                endpoint_id,
                ws,
                permit,
            } => {
                let _ = reply.send(self.handle_attach_websocket_audio(
                    endpoint_id,
                    *ws,
                    permit,
                    packet_tx,
                ));
            }
            SessionCommand::WebSocketDisconnected { endpoint_id } => {
                self.handle_websocket_disconnected(endpoint_id);
            }
        }
        true
    }

    // ── Endpoint creation ───────────────────────────────────────────

    /// Choose the media binding for a plain-RTP endpoint that is answering a
    /// remote offer. Matches the remote SDP connection-address family exactly; if
    /// the remote announced a family we did not bind, returns an error rather than
    /// answering with an unreachable address of the other family. When the remote
    /// has no usable connection address, falls back to the primary binding.
    fn select_rtp_binding(&self, remote: Option<SocketAddr>) -> anyhow::Result<&MediaBinding> {
        match remote {
            Some(addr) => {
                let want_v6 = addr.is_ipv6();
                self.media_bindings.for_family(want_v6).ok_or_else(|| {
                    let fam = if want_v6 { "IPv6" } else { "IPv4" };
                    anyhow::anyhow!(
                        "remote SDP requests {fam} media but no {fam} address is bound (media_ip)"
                    )
                })
            }
            None => Ok(self.media_bindings.primary()),
        }
    }

    async fn handle_create_from_offer(
        &mut self,
        packet_tx: &mpsc::Sender<InboundPacket>,
        sdp_str: &str,
        direction: EndpointDirection,
        expected_type: Option<EndpointType>,
    ) -> anyhow::Result<(EndpointId, String)> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("MAX_ENDPOINTS_REACHED");
        }
        let id = EndpointId::new_v4();
        let parsed = sdp::parse_sdp(sdp_str);

        if let Some(expected) = expected_type {
            let is_match = matches!(
                (expected, parsed.is_webrtc),
                (EndpointType::Webrtc, true) | (EndpointType::Rtp, false)
            );
            if !is_match {
                anyhow::bail!(
                    "Offered SDP transport does not match method (expected {})",
                    match expected {
                        EndpointType::Webrtc => "webrtc",
                        EndpointType::Rtp => "rtp",
                    }
                );
            }
        }

        let (answer, te_pt) = if parsed.is_webrtc {
            // WebRTC offers a host candidate per bound family; ICE nominates one.
            let bind_addrs: Vec<SocketAddr> = self
                .media_bindings
                .ips()
                .map(|ip| SocketAddr::new(ip, 0))
                .collect();
            let (ep, answer) = WebRtcEndpoint::from_offer(
                id,
                direction,
                sdp_str,
                &bind_addrs,
                packet_tx.clone(),
                self.metrics.clone(),
            )
            .await?;
            info!(
                session_id = %self.session_id,
                endpoint_id = %id,
                endpoint_type = "webrtc",
                direction = ?direction,
                local_addr = %ep.local_addr,
                "endpoint created (from offer)"
            );
            self.endpoints.insert(id, Endpoint::WebRtc(Box::new(ep)));
            (answer, Some(101u8))
        } else {
            // Match the remote SDP's address family; reject if we didn't bind it.
            let binding = self.select_rtp_binding(parsed.remote_addr)?;
            let bind_ip = binding.ip;
            let pool = Arc::clone(&binding.pool);
            let pair = pool.allocate_pair().await?;
            let (ep, answer) =
                RtpEndpoint::from_offer(id, direction, sdp_str, pair, bind_ip, packet_tx.clone())?;
            let te = ep.telephone_event_pt;
            info!(
                session_id = %self.session_id,
                endpoint_id = %id,
                endpoint_type = "rtp",
                direction = ?direction,
                local_addr = %ep.local_rtp_addr,
                remote_addr = ?ep.remote_rtp_addr,
                codec = ?ep.send_codec.as_ref().map(|c| c.name),
                srtp = ep.has_srtp(),
                "endpoint created (from offer)"
            );
            self.endpoints.insert(id, Endpoint::Rtp(Box::new(ep)));
            (answer, te)
        };

        self.dtmf_state.insert(
            id,
            EndpointDtmf {
                detector: DtmfDetector::new(),
                te_pt,
            },
        );
        self.rebuild_routing();
        Ok((id, answer))
    }

    async fn handle_create_offer(
        &mut self,
        packet_tx: &mpsc::Sender<InboundPacket>,
        direction: EndpointDirection,
        endpoint_type: EndpointType,
        srtp: bool,
        srtp_optional: bool,
        codecs: Option<Vec<String>>,
    ) -> anyhow::Result<(EndpointId, String)> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("MAX_ENDPOINTS_REACHED");
        }
        let id = EndpointId::new_v4();

        let (offer, te_pt) = match endpoint_type {
            EndpointType::Webrtc => {
                // WebRTC offers a host candidate per bound family; ICE nominates one.
                let bind_addrs: Vec<SocketAddr> = self
                    .media_bindings
                    .ips()
                    .map(|ip| SocketAddr::new(ip, 0))
                    .collect();
                let (ep, offer) = WebRtcEndpoint::create_offer(
                    id,
                    direction,
                    &bind_addrs,
                    packet_tx.clone(),
                    self.metrics.clone(),
                )
                .await?;
                info!(
                    session_id = %self.session_id,
                    endpoint_id = %id,
                    endpoint_type = "webrtc",
                    direction = ?direction,
                    local_addr = %ep.local_addr,
                    "endpoint created (offer generated)"
                );
                self.endpoints.insert(id, Endpoint::WebRtc(Box::new(ep)));
                (offer, Some(101u8))
            }
            EndpointType::Rtp => {
                // No remote yet and a plain-RTP offer carries a single c= line —
                // advertise the primary (v4-preferred) binding.
                let binding = self.media_bindings.primary();
                let bind_ip = binding.ip;
                let pool = Arc::clone(&binding.pool);
                let pair = pool.allocate_pair().await?;
                // Advertise the caller's preferred codec order, or highest-
                // quality-first (Opus > G.722 > PCMU) when unspecified, so the
                // SIP answerer's default first-match selection stays as wideband
                // as it can instead of dropping to PCMU. See `offer_codec_list`.
                let offer_codecs = sdp::offer_codec_list(codecs.as_deref());
                let media_security = if srtp {
                    RtpMediaSecurity::Srtp
                } else if srtp_optional {
                    RtpMediaSecurity::OptionalSrtp
                } else {
                    RtpMediaSecurity::PlainRtp
                };
                let (ep, offer) = RtpEndpoint::create_offer(
                    id,
                    direction,
                    pair,
                    bind_ip,
                    &offer_codecs,
                    media_security,
                    packet_tx.clone(),
                )?;
                let te = ep.telephone_event_pt;
                let codec_names: Vec<&str> = ep.codecs.iter().map(|c| c.name).collect();
                info!(
                    session_id = %self.session_id,
                    endpoint_id = %id,
                    endpoint_type = "rtp",
                    direction = ?direction,
                    local_addr = %ep.local_rtp_addr,
                    codecs = ?codec_names,
                    srtp = srtp,
                    srtp_optional = srtp_optional,
                    "endpoint created (offer generated)"
                );
                self.endpoints.insert(id, Endpoint::Rtp(Box::new(ep)));
                (offer, te)
            }
        };

        self.dtmf_state.insert(
            id,
            EndpointDtmf {
                detector: DtmfDetector::new(),
                te_pt,
            },
        );
        self.rebuild_routing();
        Ok((id, offer))
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_create_with_file(
        &mut self,
        source: &str,
        start_ms: u64,
        loop_count: Option<u32>,
        cache_ttl_secs: u32,
        timeout_ms: u32,
        shared: bool,
        headers: Option<std::collections::HashMap<String, String>>,
        gain_db: f32,
    ) -> anyhow::Result<EndpointId> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("MAX_ENDPOINTS_REACHED");
        }

        let id = EndpointId::new_v4();

        if crate::playback::file_cache::is_url(source) {
            let mut ep = FileEndpoint::new_buffering(id, gain_db);
            ep.shared = shared;
            self.endpoints.insert(id, Endpoint::File(Box::new(ep)));
            self.rebuild_routing();
            info!(
                session_id = %self.session_id,
                endpoint_id = %id,
                endpoint_type = "file",
                source = %crate::control::logging::source_summary(source),
                shared = shared,
                gain_db = gain_db,
                "file endpoint created (downloading)"
            );

            let url = source.to_string();
            self.url_sources.insert(id, url.clone());
            let cmd_tx = self.cmd_tx.clone();
            let cache = Arc::clone(&self.file_cache);
            let ttl = cache_ttl_secs;
            let timeout = timeout_ms;
            tokio::spawn(async move {
                let result = cache
                    .get_or_download(&url, ttl, timeout, headers.as_ref())
                    .await;
                let download_ok = result.is_ok();
                if cmd_tx
                    .send(SessionCommand::FileReady {
                        endpoint_id: id,
                        result,
                        start_ms,
                        loop_count,
                        url: url.clone(),
                    })
                    .await
                    .is_err()
                {
                    // Session died before download completed. Release the cache
                    // ref that get_or_download() acquired, otherwise the entry
                    // stays at ref_count=1 forever and can never be evicted.
                    if download_ok {
                        cache.release(&url).await;
                    }
                }
            });

            Ok(id)
        } else {
            let media_dir = self.media_dir.as_ref().ok_or_else(|| {
                anyhow::anyhow!("Local file playback is disabled (no media_dir configured)")
            })?;
            let canonical_dir = media_dir.canonicalize().map_err(|e| {
                anyhow::anyhow!(
                    "media_dir '{}' is not accessible: {}",
                    media_dir.display(),
                    e
                )
            })?;
            let requested = std::path::Path::new(source);
            let canonical_path = requested
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("File path '{source}' is not accessible: {e}"))?;
            if !canonical_path.starts_with(&canonical_dir) {
                anyhow::bail!("File path is outside the allowed media directory");
            }

            if shared {
                let sub = self
                    .shared_playback
                    .subscribe(source, 8000, start_ms, loop_count)
                    .await?;
                let ep = FileEndpoint::new_shared(id, source, sub, gain_db);
                self.endpoints.insert(id, Endpoint::File(Box::new(ep)));
            } else {
                let ep = FileEndpoint::open(id, source, start_ms, loop_count, gain_db)?;
                self.endpoints.insert(id, Endpoint::File(Box::new(ep)));
            }
            self.rebuild_routing();
            info!(
                session_id = %self.session_id,
                endpoint_id = %id,
                endpoint_type = "file",
                source = %crate::control::logging::source_summary(source),
                shared = shared,
                start_ms = start_ms,
                loop_count = ?loop_count,
                gain_db = gain_db,
                "file endpoint created (local)"
            );
            Ok(id)
        }
    }

    // ── Endpoint modification ───────────────────────────────────────

    fn handle_accept_answer(
        &mut self,
        endpoint_id: EndpointId,
        sdp: &str,
        expected_type: Option<EndpointType>,
        expected_generation: Option<u64>,
    ) -> anyhow::Result<()> {
        let endpoint_type = self
            .endpoints
            .get(&endpoint_id)
            .map(Endpoint::kind_label)
            .unwrap_or("unknown");
        info!(
            session_id = %self.session_id,
            endpoint_id = %endpoint_id,
            endpoint_type = endpoint_type,
            expected_type = ?expected_type,
            sdp_len = sdp.len(),
            "endpoint accept_answer enter"
        );

        let mut state_change = None;
        let result = match self.endpoints.get_mut(&endpoint_id) {
            Some(ep) => {
                let old_state = ep.state();
                // Reject a stale WebRTC ICE-restart answer before touching the
                // endpoint: the offer it answers must still be the endpoint's
                // current pending offer. A mismatch means the offer was
                // superseded (or caller and session desynced across the control
                // link). Done up front so it covers both the typed
                // (endpoint.webrtc.accept_answer) and untyped paths, and so a
                // mismatch leaves the pending offer untouched. None (initial
                // answer) skips the check.
                if let (Some(g), Endpoint::WebRtc(wep)) = (expected_generation, &*ep)
                    && g != wep.offer_generation
                {
                    let current = wep.offer_generation;
                    warn!(
                        session_id = %self.session_id,
                        endpoint_id = %endpoint_id,
                        answer_generation = g,
                        current_generation = current,
                        "rejecting stale ICE-restart answer (generation mismatch)"
                    );
                    return Err(anyhow::anyhow!(
                        "stale ICE-restart answer: generation {g} != current pending offer generation {current}"
                    ));
                }
                let r = match (expected_type, &mut *ep) {
                    (Some(EndpointType::Webrtc), Endpoint::WebRtc(wep)) => wep.accept_answer(sdp),
                    (Some(EndpointType::Rtp), Endpoint::Rtp(rep)) => rep.accept_answer(sdp),
                    (Some(EndpointType::Webrtc), _) => {
                        Err(anyhow::anyhow!("Not a WebRTC endpoint"))
                    }
                    (Some(EndpointType::Rtp), _) => {
                        Err(anyhow::anyhow!("Not a plain RTP endpoint"))
                    }
                    (None, ep) => ep.accept_answer(sdp),
                };
                if r.is_ok() {
                    let new_state = ep.state();
                    if old_state != new_state {
                        state_change = Some((old_state, new_state));
                    }
                    // Refresh DTMF state in case the answer changed telephone-event PT
                    if let Some(ds) = self.dtmf_state.get_mut(&endpoint_id) {
                        ds.te_pt = ep.telephone_event_pt();
                    }
                }
                r
            }
            None => Err(anyhow::anyhow!("Endpoint not found")),
        };
        if let Some((old_state, new_state)) = state_change {
            // Log codec/address info now that answer is accepted
            let sdp_remote = sdp::parse_sdp(sdp).remote_addr;
            if let Some(ep) = self.endpoints.get(&endpoint_id) {
                match ep {
                    Endpoint::Rtp(rep) => {
                        info!(
                            session_id = %self.session_id,
                            endpoint_id = %endpoint_id,
                            local_addr = %rep.local_rtp_addr,
                            remote_addr = ?rep.remote_rtp_addr,
                            codec = ?rep.send_codec.as_ref().map(|c| c.name),
                            old_state = ?old_state,
                            new_state = ?new_state,
                            "endpoint answer accepted"
                        );
                    }
                    Endpoint::WebRtc(wep) => {
                        info!(
                            session_id = %self.session_id,
                            endpoint_id = %endpoint_id,
                            local_addr = %wep.local_addr,
                            sdp_remote_addr = ?sdp_remote,
                            old_state = ?old_state,
                            new_state = ?new_state,
                            "endpoint answer accepted"
                        );
                    }
                    _ => {
                        info!(
                            session_id = %self.session_id,
                            endpoint_id = %endpoint_id,
                            old_state = ?old_state,
                            new_state = ?new_state,
                            "endpoint answer accepted"
                        );
                    }
                }
            }
            self.send_event(
                "endpoint.state_changed",
                EndpointStateChangedData {
                    endpoint_id,
                    old_state,
                    new_state,
                },
            );
        }
        self.transcode_cache
            .retain(|(src, dst), _| *src != endpoint_id && *dst != endpoint_id);
        // Rebuild routing now that endpoint is Connected and has a remote address
        if state_change.is_some() {
            self.rebuild_routing();
        }
        match &result {
            Ok(()) => {
                let current_state = self.endpoints.get(&endpoint_id).map(|ep| ep.state());
                info!(
                    session_id = %self.session_id,
                    endpoint_id = %endpoint_id,
                    endpoint_type = endpoint_type,
                    state = ?current_state,
                    "endpoint accept_answer exit"
                );
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    endpoint_id = %endpoint_id,
                    endpoint_type = endpoint_type,
                    error = %e,
                    "endpoint accept_answer exit"
                );
            }
        }
        result
    }

    fn handle_accept_offer(
        &mut self,
        endpoint_id: EndpointId,
        sdp: &str,
    ) -> anyhow::Result<String> {
        let result = match self.endpoints.get_mut(&endpoint_id) {
            Some(ep) => ep.accept_offer(sdp),
            None => Err(anyhow::anyhow!("Endpoint not found")),
        };

        if result.is_ok() {
            let sdp_remote = sdp::parse_sdp(sdp).remote_addr;
            let local_addr = self.endpoints.get(&endpoint_id).and_then(|ep| match ep {
                Endpoint::WebRtc(w) => Some(w.local_addr),
                Endpoint::Rtp(r) => Some(r.local_rtp_addr),
                _ => None,
            });
            info!(
                session_id = %self.session_id,
                endpoint_id = %endpoint_id,
                local_addr = ?local_addr,
                sdp_remote_addr = ?sdp_remote,
                "endpoint offer accepted"
            );
        }

        result
    }

    fn handle_create_tone(
        &mut self,
        tone_type: super::endpoint_tone::ToneType,
        frequency: Option<f64>,
        duration_ms: Option<u64>,
    ) -> anyhow::Result<EndpointId> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("max endpoints reached");
        }
        let id = EndpointId::new_v4();
        let ep = super::endpoint_tone::ToneEndpoint::new(id, tone_type, frequency, duration_ms);
        self.endpoints.insert(id, Endpoint::Tone(Box::new(ep)));
        self.rebuild_routing();
        info!(
            session_id = %self.session_id,
            endpoint_id = %id,
            endpoint_type = "tone",
            tone_type = ?tone_type,
            frequency = ?frequency,
            duration_ms = ?duration_ms,
            "tone endpoint created"
        );
        Ok(id)
    }

    fn handle_create_websocket(
        &mut self,
        direction: EndpointDirection,
        sample_rate: u32,
        flush_ms: u32,
    ) -> anyhow::Result<(EndpointId, uuid::Uuid)> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("max endpoints reached");
        }
        let id = EndpointId::new_v4();
        let ep = WebSocketEndpoint::new(id, direction, sample_rate, flush_ms);
        let token = ep.connect_token;
        self.endpoints.insert(id, Endpoint::WebSocket(Box::new(ep)));
        // Register the single-use connect token so the dialed-in audio socket
        // can be routed back to this session and endpoint.
        self.ws_audio_registry.insert(
            token,
            crate::control::ws_audio::WsAudioTicket {
                cmd_tx: self.cmd_tx.clone(),
                session_id: self.session_id,
                endpoint_id: id,
            },
        );
        // Endpoint is Connecting (not yet routed) until the audio socket attaches.
        self.rebuild_routing();
        info!(
            session_id = %self.session_id,
            endpoint_id = %id,
            endpoint_type = "websocket",
            sample_rate,
            flush_ms,
            "websocket endpoint created (awaiting audio connection)"
        );
        Ok((id, token))
    }

    fn handle_attach_websocket_audio(
        &mut self,
        endpoint_id: EndpointId,
        ws: AudioWsStream,
        permit: tokio::sync::OwnedSemaphorePermit,
        packet_tx: &mpsc::Sender<InboundPacket>,
    ) -> anyhow::Result<()> {
        let cmd_tx = self.cmd_tx.clone();
        match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::WebSocket(wsep)) => {
                wsep.attach_io(ws, packet_tx.clone(), cmd_tx, permit)?;
            }
            Some(_) => anyhow::bail!("endpoint is not a websocket endpoint"),
            None => anyhow::bail!("Endpoint not found"),
        }
        // Now Connected — include it in routing and notify the controller.
        self.rebuild_routing();
        self.send_event("endpoint.ws.connected", WsConnectedData { endpoint_id });
        info!(
            session_id = %self.session_id,
            endpoint_id = %endpoint_id,
            "websocket audio connected"
        );
        Ok(())
    }

    /// Remove WS endpoints stuck in `Connecting` past the dial-in deadline,
    /// reclaiming their endpoint slot and connect token.
    async fn check_ws_connect_timeouts(&mut self) {
        let now = Instant::now();
        let expired: Vec<EndpointId> = self
            .endpoints
            .iter()
            .filter_map(|(id, ep)| match ep {
                Endpoint::WebSocket(w)
                    if w.state == EndpointState::Connecting
                        && now.duration_since(w.stats.created_at) >= WS_CONNECT_TIMEOUT =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        for id in expired {
            warn!(
                session_id = %self.session_id,
                endpoint_id = %id,
                "websocket endpoint dial-in timed out; removing"
            );
            self.send_event(
                "endpoint.ws.connect_timeout",
                WsDisconnectedData { endpoint_id: id },
            );
            let _ = self.handle_remove_endpoint(id).await;
        }
    }

    fn handle_websocket_disconnected(&mut self, endpoint_id: EndpointId) {
        // Only acts if the endpoint still exists (it may have been removed, which
        // is what triggered the IO task to exit in the first place).
        if let Some(Endpoint::WebSocket(wsep)) = self.endpoints.get_mut(&endpoint_id) {
            if wsep.state == EndpointState::Disconnected {
                return; // already handled
            }
            wsep.state = EndpointState::Disconnected;
            self.rebuild_routing();
            self.send_event(
                "endpoint.ws.disconnected",
                WsDisconnectedData { endpoint_id },
            );
            info!(
                session_id = %self.session_id,
                endpoint_id = %endpoint_id,
                "websocket audio disconnected"
            );
        }
    }

    async fn handle_remove_endpoint(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        if let Some(ep) = self.endpoints.get(&endpoint_id) {
            let ep_type = match ep {
                Endpoint::WebRtc(_) => "webrtc",
                Endpoint::Rtp(_) => "rtp",
                Endpoint::File(_) => "file",
                Endpoint::Tone(_) => "tone",
                Endpoint::Bridge(_) => "bridge",
                Endpoint::WebSocket(_) => "websocket",
            };
            info!(
                session_id = %self.session_id,
                endpoint_id = %endpoint_id,
                endpoint_type = ep_type,
                "endpoint removed"
            );
        }
        let mut removed = self.endpoints.remove(&endpoint_id);
        // Drop any pending WS audio connect token so it can't be claimed after removal.
        if let Some(Endpoint::WebSocket(ref wsep)) = removed {
            self.ws_audio_registry.remove(&wsep.connect_token);
        }
        // Explicitly clean up shared playback subscriber before general cleanup,
        // so the async ref_count decrement happens reliably (not via Drop spawn).
        if let Some(Endpoint::File(ref mut fep)) = removed
            && let Some(sub) = fep.shared_sub.take()
        {
            sub.cleanup().await;
        }
        // Auto-remove paired bridge endpoint in the other session
        if let Some(Endpoint::Bridge(ref bep)) = removed {
            let paired_id = bep.paired_endpoint_id;
            let paired_cmd_tx = bep.paired_cmd_tx.clone();
            // Fire-and-forget: send RemoveEndpoint to the paired session.
            // If the paired endpoint is already gone, the error is harmless.
            let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
            let _ = paired_cmd_tx.try_send(SessionCommand::RemoveEndpoint {
                reply: reply_tx,
                endpoint_id: paired_id,
            });
        }
        self.cleanup_endpoint_state(endpoint_id).await;
        let stopped_recs = self.recording_mgr.stop_endpoint_recordings(&endpoint_id);
        for info in &stopped_recs {
            self.metrics.recordings_active.dec();
            self.send_event(
                "recording.stopped",
                RecordingStoppedData {
                    recording_id: info.recording_id,
                    file_path: info.file_path.clone(),
                    duration_ms: info.duration_ms,
                    packets: info.packets,
                    dropped_packets: info.dropped_packets,
                    reason: "endpoint_removed".to_string(),
                },
            );
        }
        if removed.is_some() {
            self.rebuild_routing();
            self.metrics.endpoints_active.dec();
        }
        removed
            .map(|_| ())
            .ok_or_else(|| anyhow::anyhow!("Endpoint not found"))
    }

    // ── Endpoint transfer ───────────────────────────────────────────

    async fn handle_extract_endpoint(
        &mut self,
        endpoint_id: EndpointId,
    ) -> anyhow::Result<EndpointTransferBundle> {
        // Reject file/tone endpoints
        if matches!(
            self.endpoints.get(&endpoint_id),
            Some(Endpoint::File(_) | Endpoint::Tone(_) | Endpoint::WebSocket(_))
        ) {
            anyhow::bail!("File, tone, and websocket endpoints cannot be transferred");
        }

        let mut endpoint = self
            .endpoints
            .remove(&endpoint_id)
            .ok_or_else(|| anyhow::anyhow!("Endpoint not found"))?;

        let ep_type = match &endpoint {
            Endpoint::WebRtc(_) => "webrtc",
            Endpoint::Rtp(_) => "rtp",
            Endpoint::File(_) => "file",
            Endpoint::Tone(_) => "tone",
            Endpoint::Bridge(_) => "bridge",
            Endpoint::WebSocket(_) => "websocket",
        };
        info!(
            session_id = %self.session_id,
            endpoint_id = %endpoint_id,
            endpoint_type = ep_type,
            "endpoint extracted for transfer"
        );

        // Stop recv tasks before moving
        endpoint.stop_recv_tasks().await;

        // Extract ancillary state
        let dtmf_state = self.dtmf_state.remove(&endpoint_id);
        let sensitive_dtmf = self.sensitive_dtmf_endpoints.remove(&endpoint_id);
        let vad_monitor = self.vad_monitors.remove(&endpoint_id);
        let fax_detector = self.fax_detectors.remove(&endpoint_id);
        let analysis_decoder = self.analysis_decoders.remove(&endpoint_id);
        let file_rtp_state = self.file_rtp_states.remove(&endpoint_id);
        let url_source = self.url_sources.remove(&endpoint_id);
        let media_timeout_was_emitted = self.media_timeout_emitted.remove(&endpoint_id);
        // Drop the cached recording descriptor: the endpoint leaves this session,
        // and its synthetic address index may be recycled for a new endpoint.
        self.recording_mgr.forget_endpoint(&endpoint_id);

        // Clear pending DTMF injection targeting this endpoint
        if self
            .dtmf_injection
            .as_ref()
            .is_some_and(|inj| inj.endpoint_id == endpoint_id)
        {
            self.dtmf_injection = None;
        }

        // Stop recordings for this endpoint
        let stopped_recs = self.recording_mgr.stop_endpoint_recordings(&endpoint_id);
        for info in &stopped_recs {
            self.metrics.recordings_active.dec();
            self.send_event(
                "recording.stopped",
                RecordingStoppedData {
                    recording_id: info.recording_id,
                    file_path: info.file_path.clone(),
                    duration_ms: info.duration_ms,
                    packets: info.packets,
                    dropped_packets: info.dropped_packets,
                    reason: "endpoint_transferred".to_string(),
                },
            );
        }

        // Clear transcode cache entries
        self.transcode_cache
            .retain(|(src, dst), _| *src != endpoint_id && *dst != endpoint_id);

        // Rebuild routing
        self.rebuild_routing();

        // Emit transfer event
        self.send_event(
            "endpoint.transferred_out",
            EndpointTransferredOutData {
                endpoint_id,
                target_session_id: SessionId::nil(), // filled by handler
            },
        );

        Ok(EndpointTransferBundle {
            endpoint,
            source_session_id: self.session_id,
            dtmf_state,
            sensitive_dtmf,
            vad_monitor,
            fax_detector,
            analysis_decoder,
            file_rtp_state,
            url_source,
            media_timeout_was_emitted,
        })
    }

    async fn handle_insert_endpoint(
        &mut self,
        bundle: &mut EndpointTransferBundle,
        packet_tx: &mpsc::Sender<InboundPacket>,
    ) -> anyhow::Result<()> {
        // Check capacity
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("Maximum endpoints per session reached");
        }

        let endpoint_id = bundle.endpoint.id();
        let direction = bundle.endpoint.direction();
        let state = bundle.endpoint.state();
        let endpoint_type = match &bundle.endpoint {
            Endpoint::WebRtc(_) => "webrtc",
            Endpoint::Rtp(_) => "rtp",
            Endpoint::File(_) => "file",
            Endpoint::Tone(_) => "tone",
            Endpoint::Bridge(_) => "bridge",
            Endpoint::WebSocket(_) => "websocket",
        };

        info!(
            session_id = %self.session_id,
            endpoint_id = %endpoint_id,
            endpoint_type = endpoint_type,
            direction = ?direction,
            source_session_id = %bundle.source_session_id,
            "endpoint inserted (transferred in)"
        );

        // Restart recv tasks with this session's packet_tx
        bundle.endpoint.restart_recv_tasks(packet_tx.clone());

        // Take ownership of the endpoint and ancillary state from the bundle
        // We use std::mem::replace with a dummy that will be dropped
        let endpoint = std::mem::replace(
            &mut bundle.endpoint,
            Endpoint::File(Box::new(FileEndpoint::new_buffering(
                EndpointId::nil(),
                0.0,
            ))),
        );
        self.endpoints.insert(endpoint_id, endpoint);

        // Insert ancillary state
        if let Some(dtmf) = bundle.dtmf_state.take() {
            self.dtmf_state.insert(endpoint_id, dtmf);
        }
        if bundle.sensitive_dtmf {
            self.sensitive_dtmf_endpoints.insert(endpoint_id);
        }
        if let Some(vad) = bundle.vad_monitor.take() {
            self.vad_monitors.insert(endpoint_id, vad);
        }
        if let Some(fax) = bundle.fax_detector.take() {
            self.fax_detectors.insert(endpoint_id, fax);
        }
        if let Some(dec) = bundle.analysis_decoder.take() {
            self.analysis_decoders.insert(endpoint_id, dec);
        }
        if let Some(frs) = bundle.file_rtp_state.take() {
            self.file_rtp_states.insert(endpoint_id, frs);
        }
        if let Some(url) = bundle.url_source.take() {
            self.url_sources.insert(endpoint_id, url);
        }
        if bundle.media_timeout_was_emitted {
            self.media_timeout_emitted.insert(endpoint_id);
        }

        // Rebuild routing
        self.rebuild_routing();

        // Emit transfer event
        self.send_event(
            "endpoint.transferred_in",
            EndpointTransferredInData {
                endpoint_id,
                source_session_id: bundle.source_session_id,
                endpoint_type: endpoint_type.to_string(),
                direction,
                state,
            },
        );

        // Create DTMF detector if not transferred
        if !self.dtmf_state.contains_key(&endpoint_id) {
            self.dtmf_state.insert(
                endpoint_id,
                EndpointDtmf {
                    detector: DtmfDetector::new(),
                    te_pt: match &self.endpoints.get(&endpoint_id) {
                        Some(ep) => ep.telephone_event_pt(),
                        None => None,
                    },
                },
            );
        }

        Ok(())
    }

    fn handle_insert_bridge_endpoint(
        &mut self,
        bridge: super::endpoint_bridge::BridgeEndpoint,
    ) -> anyhow::Result<EndpointId> {
        // Check capacity
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("Maximum endpoints per session reached");
        }

        let endpoint_id = bridge.id;
        let paired_endpoint_id = bridge.paired_endpoint_id;
        let paired_session_id = bridge.paired_session_id;
        let direction = bridge.config.direction;
        self.endpoints
            .insert(endpoint_id, Endpoint::Bridge(Box::new(bridge)));

        // Create DTMF state entry (bridge endpoints don't have telephone-event PT)
        self.dtmf_state.insert(
            endpoint_id,
            EndpointDtmf {
                detector: DtmfDetector::new(),
                te_pt: None,
            },
        );

        // Rebuild routing
        self.rebuild_routing();

        info!(
            session_id = %self.session_id,
            endpoint_id = %endpoint_id,
            endpoint_type = "bridge",
            direction = ?direction,
            paired_endpoint_id = %paired_endpoint_id,
            paired_session_id = %paired_session_id,
            "bridge endpoint created"
        );

        Ok(endpoint_id)
    }

    // ── File endpoint commands ──────────────────────────────────────

    async fn handle_file_ready(
        &mut self,
        endpoint_id: EndpointId,
        result: anyhow::Result<std::path::PathBuf>,
        start_ms: u64,
        loop_count: Option<u32>,
        _url: &str,
    ) {
        let init_err = match result {
            Ok(path) => {
                if let Some(Endpoint::File(fep)) = self.endpoints.get_mut(&endpoint_id) {
                    let old_state = fep.state;
                    let path_str = path.to_string_lossy().to_string();
                    let is_shared = fep.shared;

                    let gain_db = fep.gain_db();

                    let init_result = if is_shared {
                        // Shared playback: subscribe to the shared decode task
                        // instead of initializing a local decoder.
                        match self
                            .shared_playback
                            .subscribe(&path_str, 8000, start_ms, loop_count)
                            .await
                        {
                            Ok(sub) => {
                                let new_ep =
                                    FileEndpoint::new_shared(endpoint_id, &path_str, sub, gain_db);
                                **fep = new_ep;
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        fep.initialize(&path_str, start_ms, loop_count)
                    };

                    match init_result {
                        Ok(()) => {
                            info!(
                                session_id = %self.session_id,
                                endpoint_id = %endpoint_id,
                                source = %crate::control::logging::source_summary(&path_str),
                                shared = is_shared,
                                "file endpoint playback started"
                            );
                            self.send_event(
                                "endpoint.state_changed",
                                EndpointStateChangedData {
                                    endpoint_id,
                                    old_state,
                                    new_state: EndpointState::Playing,
                                },
                            );
                            // Rebuild routing now that file endpoint is Playing
                            self.rebuild_routing();
                            None
                        }
                        Err(e) => Some(e),
                    }
                } else {
                    // Endpoint was removed during the download. cleanup_endpoint_state
                    // already released the cache ref via url_sources removal.
                    None
                }
            }
            Err(e) => {
                warn!(endpoint_id = %endpoint_id, error = %e, "file download failed");
                Some(e)
            }
        };

        if let Some(e) = init_err {
            warn!(endpoint_id = %endpoint_id, error = %e, "file playback failed");
            let was_present = self.endpoints.remove(&endpoint_id).is_some();
            if was_present {
                self.cleanup_endpoint_state(endpoint_id).await;
                self.rebuild_routing();
                self.metrics.endpoints_active.dec();
            }
            // Only emit when the endpoint was still present. If it was already
            // removed (e.g., endpoint.remove during download), the client was
            // already notified via the remove response.
            if was_present {
                self.send_event(
                    "endpoint.file.finished",
                    FileFinishedData {
                        endpoint_id,
                        reason: "error".to_string(),
                        error: Some(e.to_string()),
                    },
                );
            }
        }
    }

    fn handle_file_seek(
        &mut self,
        endpoint_id: EndpointId,
        position_ms: u64,
    ) -> anyhow::Result<()> {
        let result = match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::File(fep)) => fep.seek(position_ms),
            Some(_) => Err(anyhow::anyhow!("Not a file endpoint")),
            None => Err(anyhow::anyhow!("Endpoint not found")),
        };
        if result.is_ok() {
            info!(
                session_id = %self.session_id,
                endpoint_id = %endpoint_id,
                position_ms = position_ms,
                "file endpoint seeked"
            );
        }
        result
    }

    fn handle_file_pause(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        let state_change = match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::File(fep)) => {
                let old_state = fep.state;
                fep.pause()?;
                if old_state != fep.state {
                    Some((old_state, fep.state))
                } else {
                    None
                }
            }
            Some(_) => return Err(anyhow::anyhow!("Not a file endpoint")),
            None => return Err(anyhow::anyhow!("Endpoint not found")),
        };
        if let Some((old_state, new_state)) = state_change {
            info!(
                session_id = %self.session_id,
                endpoint_id = %endpoint_id,
                "file endpoint paused"
            );
            self.send_event(
                "endpoint.state_changed",
                EndpointStateChangedData {
                    endpoint_id,
                    old_state,
                    new_state,
                },
            );
        }
        Ok(())
    }

    fn handle_file_resume(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        let state_change = match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::File(fep)) => {
                let old_state = fep.state;
                fep.resume()?;
                if old_state != fep.state {
                    Some((old_state, fep.state))
                } else {
                    None
                }
            }
            Some(_) => return Err(anyhow::anyhow!("Not a file endpoint")),
            None => return Err(anyhow::anyhow!("Endpoint not found")),
        };
        if let Some((old_state, new_state)) = state_change {
            info!(
                session_id = %self.session_id,
                endpoint_id = %endpoint_id,
                "file endpoint resumed"
            );
            self.send_event(
                "endpoint.state_changed",
                EndpointStateChangedData {
                    endpoint_id,
                    old_state,
                    new_state,
                },
            );
        }
        Ok(())
    }

    // ── DTMF injection ──────────────────────────────────────────────

    fn handle_dtmf_inject(
        &mut self,
        endpoint_id: &EndpointId,
        digit: char,
        duration_ms: u32,
        volume: u8,
    ) -> anyhow::Result<()> {
        let injection = super::session_dtmf::build_dtmf_injection(
            &self.endpoints,
            &self.dtmf_injection,
            endpoint_id,
            digit,
            duration_ms,
            volume,
        )?;
        self.dtmf_injection = Some(injection);
        Ok(())
    }

    fn handle_dtmf_set_sensitive(
        &mut self,
        endpoint_id: EndpointId,
        enabled: bool,
    ) -> anyhow::Result<()> {
        if !self.endpoints.contains_key(&endpoint_id) {
            anyhow::bail!("Endpoint not found");
        }

        if enabled {
            self.sensitive_dtmf_endpoints.insert(endpoint_id);
        } else {
            self.sensitive_dtmf_endpoints.remove(&endpoint_id);
        }
        info!(
            session_id = %self.session_id,
            endpoint_id = %endpoint_id,
            enabled,
            "sensitive DTMF mode changed"
        );
        Ok(())
    }

    // ── VAD ─────────────────────────────────────────────────────────

    fn handle_vad_start(
        &mut self,
        endpoint_id: EndpointId,
        silence_interval_ms: u32,
        speech_threshold: f32,
    ) -> anyhow::Result<()> {
        let result = vad_tap::vad_start(
            &self.endpoints,
            &mut self.vad_monitors,
            endpoint_id,
            silence_interval_ms,
            speech_threshold,
        );
        // A tap makes an otherwise-transparent source non-transparent (analysis decodes in
        // order), so re-evaluate playout engagement.
        self.recompute_playout_policy();
        result
    }

    fn handle_vad_stop(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        let result = vad_tap::vad_stop(&mut self.vad_monitors, endpoint_id);
        self.prune_analysis_decoder(endpoint_id);
        self.recompute_playout_policy();
        result
    }

    // ── Fax tone detection ───────────────────────────────────────────

    fn handle_fax_detect_start(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        let result = fax_tap::fax_start(&self.endpoints, &mut self.fax_detectors, endpoint_id);
        self.recompute_playout_policy();
        result
    }

    fn handle_fax_detect_stop(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        let result = fax_tap::fax_stop(&mut self.fax_detectors, endpoint_id);
        self.prune_analysis_decoder(endpoint_id);
        self.recompute_playout_policy();
        result
    }

    /// Drop the shared analysis decoder once no analyser (VAD or fax) remains
    /// on the endpoint. The decoder is stateful (G.722/Opus) and is not fed
    /// while no analyser is active, so a stale instance must not survive to be
    /// reused by a later `vad.start`/`fax_detect.start` — that would decode a
    /// discontinuous bitstream and corrupt the PCM. While one analyser is still
    /// active the decoder keeps being fed, so it stays valid and is retained.
    fn prune_analysis_decoder(&mut self, endpoint_id: EndpointId) {
        if !self.vad_monitors.contains_key(&endpoint_id)
            && !self.fax_detectors.contains_key(&endpoint_id)
        {
            self.analysis_decoders.remove(&endpoint_id);
        }
    }

    // ── WebRTC / SRTP ───────────────────────────────────────────────

    fn handle_ice_restart(&mut self, endpoint_id: EndpointId) -> anyhow::Result<(String, u64)> {
        // Reject an ICE restart while a prior offer is still unanswered. str0m
        // keeps only one pending offer; overwriting it lets a later answer apply
        // against the wrong offer and silently kill media (see
        // WebRtcEndpoint::ice_restart and docs/protocol/endpoints.md). Two
        // overlapping ICE restarts only happen when a caller races them — the
        // softphone-bridge coalesces concurrent cRequestIceRestart — so a
        // non-zero rtpbridge_webrtc_ice_restart_conflicts means a caller bug.
        // A separate `get` keeps the conflict counter (on self.metrics)
        // borrow-disjoint from the `get_mut` below.
        let already_pending = match self.endpoints.get(&endpoint_id) {
            Some(Endpoint::WebRtc(wep)) => wep.pending_offer.is_some(),
            Some(_) => return Err(anyhow::anyhow!("Not a WebRTC endpoint")),
            None => return Err(anyhow::anyhow!("Endpoint not found")),
        };
        if already_pending {
            self.metrics.webrtc_ice_restart_conflicts.inc();
            warn!(
                session_id = %self.session_id,
                endpoint_id = %endpoint_id,
                "ICE restart requested while a prior offer is still pending; rejecting to avoid pending-offer overwrite"
            );
            anyhow::bail!("ICE restart already pending for this endpoint");
        }
        match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::WebRtc(wep)) => wep.ice_restart(),
            // Type can't change between the two lookups: the session task is the
            // sole mutator of `endpoints` and never yields between them.
            _ => Err(anyhow::anyhow!("Endpoint not found")),
        }
    }

    fn handle_srtp_rekey(&mut self, endpoint_id: EndpointId) -> anyhow::Result<String> {
        match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::Rtp(rep)) => rep.srtp_rekey(),
            Some(_) => Err(anyhow::anyhow!("Not a plain RTP endpoint")),
            None => Err(anyhow::anyhow!("Endpoint not found")),
        }
    }

    fn handle_update_direction(
        &mut self,
        endpoint_id: EndpointId,
        direction: EndpointDirectionUpdate,
    ) -> anyhow::Result<()> {
        // Capture pre-direction so we can detect a non-sending → sending
        // transition (i.e. unhold) and rotate the outbound SSRC.
        let prev_dir = self.endpoints.get(&endpoint_id).map(|e| e.direction());

        match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::Rtp(rep)) => {
                rep.set_direction_override(direction);
                rep.reset_addr_lock();
                if prev_dir.is_some_and(|d| !d.is_sending()) && rep.config.direction.is_sending() {
                    rep.bump_outbound_ssrc();
                }
            }
            Some(Endpoint::WebRtc(wep)) => {
                wep.set_direction_override(direction);
                if prev_dir.is_some_and(|d| !d.is_sending())
                    && wep.config.direction.is_sending()
                    && let Err(e) = wep.bump_outbound_ssrc()
                {
                    warn!(
                        endpoint_id = %endpoint_id,
                        error = %e,
                        "failed to rotate outbound SSRC on unhold"
                    );
                }
            }
            Some(Endpoint::Bridge(bep)) => {
                bep.set_direction_override(direction);
            }
            Some(Endpoint::WebSocket(wsep)) => {
                wsep.set_direction_override(direction);
            }
            Some(Endpoint::File(_) | Endpoint::Tone(_)) => {
                anyhow::bail!("Cannot update direction on file/tone endpoints");
            }
            None => anyhow::bail!("Endpoint not found"),
        }
        self.rebuild_routing();
        info!(
            session_id = %self.session_id,
            endpoint_id = %endpoint_id,
            direction_update = ?direction,
            "endpoint direction updated"
        );
        Ok(())
    }

    fn handle_update_remote_sdp(
        &mut self,
        endpoint_id: EndpointId,
        sdp: &str,
    ) -> anyhow::Result<String> {
        // Snapshot direction before applying the SDP so we can detect a
        // non-sending → sending transition (i.e. a SIP-style unhold via
        // re-INVITE) and rotate the outbound SSRC. Mirrors the same logic
        // in handle_update_direction.
        let prev_dir = self.endpoints.get(&endpoint_id).map(|e| e.direction());

        let result = match self.endpoints.get_mut(&endpoint_id) {
            Some(ep) => ep.update_remote_sdp(sdp),
            None => Err(anyhow::anyhow!("Endpoint not found")),
        };
        if result.is_ok() {
            if let Some(Endpoint::Rtp(rep)) = self.endpoints.get_mut(&endpoint_id)
                && prev_dir.is_some_and(|d| !d.is_sending())
                && rep.config.direction.is_sending()
            {
                rep.bump_outbound_ssrc();
            }
            self.rebuild_routing();
            if let Some(Endpoint::Rtp(rep)) = self.endpoints.get(&endpoint_id) {
                info!(
                    session_id = %self.session_id,
                    endpoint_id = %endpoint_id,
                    local_addr = %rep.local_rtp_addr,
                    remote_addr = ?rep.remote_rtp_addr,
                    remote_rtcp_addr = ?rep.remote_rtcp_addr,
                    codec = ?rep.send_codec.as_ref().map(|c| c.name),
                    "endpoint remote SDP updated"
                );
            } else {
                info!(
                    session_id = %self.session_id,
                    endpoint_id = %endpoint_id,
                    "endpoint remote SDP updated"
                );
            }
        }
        result
    }

    // ── Info ─────────────────────────────────────────────────────────

    fn get_info(&self) -> SessionDetails {
        let ep_infos: Vec<EndpointInfo> = self
            .endpoints
            .values()
            .map(|ep| {
                let mut local_rtp_addr = None;
                let mut local_rtcp_addr = None;
                let mut remote_rtp_addr = None;
                let mut remote_rtcp_addr = None;
                let mut offer_generation = None;
                let (ep_type, codec, shared_playback_id) = match ep {
                    Endpoint::WebRtc(w) => {
                        let (local, remote) = w.recording_addrs();
                        local_rtp_addr = local.map(|a| a.to_string());
                        remote_rtp_addr = remote.map(|a| a.to_string());
                        offer_generation = Some(w.offer_generation);
                        let codec = w
                            .negotiated_codec()
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "opus".to_string());
                        ("webrtc".to_string(), Some(codec), None)
                    }
                    Endpoint::Rtp(r) => {
                        local_rtp_addr = Some(r.local_rtp_addr.to_string());
                        local_rtcp_addr = r.rtcp_socket.local_addr().ok().map(|a| a.to_string());
                        remote_rtp_addr = r.remote_rtp_addr.map(|a| a.to_string());
                        remote_rtcp_addr = r.remote_rtcp_addr.map(|a| a.to_string());
                        (
                            "rtp".to_string(),
                            r.send_codec.as_ref().map(|c| c.name.to_string()),
                            None,
                        )
                    }
                    Endpoint::File(f) => {
                        let spid = if f.shared {
                            Some(
                                self.url_sources
                                    .get(&f.id)
                                    .map(|url| crate::playback::file_cache::cache_key(url))
                                    .unwrap_or_else(|| f.source_path().to_string()),
                            )
                        } else {
                            None
                        };
                        ("file".to_string(), None, spid)
                    }
                    Endpoint::Tone(_) => ("tone".to_string(), None, None),
                    Endpoint::Bridge(_) => {
                        ("bridge".to_string(), Some("L16/48000".to_string()), None)
                    }
                    Endpoint::WebSocket(w) => {
                        ("websocket".to_string(), Some(w.codec_label()), None)
                    }
                };
                EndpointInfo {
                    endpoint_id: ep.id(),
                    endpoint_type: ep_type,
                    direction: ep.direction(),
                    state: ep.state(),
                    codec,
                    shared_playback_id,
                    local_rtp_addr,
                    local_rtcp_addr,
                    remote_rtp_addr,
                    remote_rtcp_addr,
                    offer_generation,
                }
            })
            .collect();
        SessionDetails {
            endpoints: ep_infos,
            recordings: self.recording_mgr.active_recordings(),
            vad_active: self.vad_monitors.keys().cloned().collect(),
            fax_detect_active: self.fax_detectors.keys().cloned().collect(),
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────

    /// Remove all per-endpoint ancillary state. Does NOT remove from `self.endpoints`
    /// (caller is responsible for that) and does NOT touch recordings.
    async fn cleanup_endpoint_state(&mut self, endpoint_id: EndpointId) {
        self.dtmf_state.remove(&endpoint_id);
        self.sensitive_dtmf_endpoints.remove(&endpoint_id);
        // Clear any pending DTMF injection targeting this endpoint
        if self
            .dtmf_injection
            .as_ref()
            .is_some_and(|inj| inj.endpoint_id == endpoint_id)
        {
            self.dtmf_injection = None;
        }
        self.vad_monitors.remove(&endpoint_id);
        self.fax_detectors.remove(&endpoint_id);
        self.analysis_decoders.remove(&endpoint_id);
        self.file_rtp_states.remove(&endpoint_id);
        self.tone_rtp_states.remove(&endpoint_id);
        self.media_timeout_emitted.remove(&endpoint_id);
        // Drop the cached recording descriptor so a later recording doesn't replay
        // a dead endpoint.
        self.recording_mgr.forget_endpoint(&endpoint_id);
        if let Some(url) = self.url_sources.remove(&endpoint_id) {
            self.file_cache.release(&url).await;
        }
        self.transcode_cache
            .retain(|(src, dst), _| *src != endpoint_id && *dst != endpoint_id);
        // Remove mixer for this destination, and remove this endpoint as a source from all mixers
        self.mixers.remove(&endpoint_id);
        for mixer in self.mixers.values_mut() {
            mixer.remove_source(&endpoint_id);
        }
        self.playout_buffers.remove(&endpoint_id);
        self.playout_policy.remove(&endpoint_id);
    }

    fn rebuild_routing(&mut self) {
        let ep_list: Vec<_> = self
            .endpoints
            .iter()
            .filter(|(_, ep)| {
                matches!(
                    ep.state(),
                    EndpointState::Connected | EndpointState::Playing
                )
            })
            .map(|(id, ep)| (*id, ep.direction(), ep.is_bridge()))
            .collect();
        self.endpoint_count
            .store(self.endpoints.len(), std::sync::atomic::Ordering::Relaxed);
        self.routing.rebuild(&ep_list);
        self.rebuild_mixers();
        self.recompute_playout_policy();
        // Track when endpoint count drops to zero for empty session timeout
        if self.endpoints.is_empty() {
            if self.empty_since.is_none() {
                self.empty_since = Some(Instant::now());
            }
        } else {
            self.empty_since = None;
        }
    }

    /// Synchronize mixer state with the current routing table.
    /// Creates mixers for new multi-source destinations, removes stale ones,
    /// and prunes source lists in existing mixers.
    fn rebuild_mixers(&mut self) {
        let multi = self.routing.multi_source_destinations().clone();

        // Remove mixers for destinations that no longer need mixing
        self.mixers.retain(|dest_id, _| multi.contains(dest_id));

        // Create mixers for new multi-source destinations
        for &dest_id in &multi {
            if !self.mixers.contains_key(&dest_id)
                && let Some(ep) = self.endpoints.get(&dest_id)
                && let (Some(codec), Some(pt)) = (endpoint_audio_codec(ep), endpoint_send_pt(ep))
            {
                match super::mixer::DestinationMixer::new(codec, pt) {
                    Ok(mut mixer) => {
                        // Seed timestamp from the endpoint's last outbound
                        // timestamp for seamless passthrough→mixer transition.
                        if let Some(last_ts) = endpoint_last_rtp_timestamp(ep) {
                            mixer.continue_from_timestamp(last_ts);
                        }
                        self.mixers.insert(dest_id, mixer);
                    }
                    Err(e) => {
                        warn!(dest = %dest_id, error = %e, "failed to create mixer");
                    }
                }
            }
        }

        // Prune stale sources from existing mixers
        for (&dest_id, mixer) in &mut self.mixers {
            let active_sources = self.routing.sources_for(&dest_id);
            mixer.retain_sources(&active_sources);
        }
    }

    /// Recompute per-source playout engagement and reconcile the buffer set. Called whenever
    /// the routing graph or an analysis tap (VAD/fax) changes, since both affect whether a
    /// source is a transparent relay. A stale policy is the only way a bypassed source could
    /// reach a non-transparent consumer, so this must run on every such change.
    fn recompute_playout_policy(&mut self) {
        // Phase 1: decide policy per endpoint (immutable borrow of endpoints/routing/taps).
        let decisions: Vec<(EndpointId, Policy)> = self
            .endpoints
            .keys()
            .map(|&id| (id, self.playout_policy_for(id)))
            .collect();

        for (id, policy) in &decisions {
            match policy {
                Policy::Engaged(kind) => {
                    // Keep an existing buffer of the right kind AND mode so its timeline/SSRC
                    // stays stable across unrelated rebuilds; rebuild on a shallow↔deep
                    // (mixer-fed) transition so the mixer one-frame-per-tick invariant holds.
                    let matches_existing = self.playout_buffers.get(id).is_some_and(|b| {
                        b.kind() == *kind
                            && (*kind != PlayoutKind::Tracked
                                || b.is_mixer_fed() == self.source_is_mixer_fed(*id))
                    });
                    if !matches_existing && let Some(buf) = self.make_playout_buffer(*id, *kind) {
                        self.playout_buffers.insert(*id, buf);
                    }
                }
                Policy::Bypass => {
                    self.playout_buffers.remove(id);
                }
            }
            self.playout_policy.insert(*id, *policy);
        }

        // Drop policy/buffers for endpoints that no longer exist.
        self.playout_policy
            .retain(|id, _| self.endpoints.contains_key(id));
        self.playout_buffers
            .retain(|id, _| self.endpoints.contains_key(id));
    }

    /// Decide the playout policy for one source endpoint.
    fn playout_policy_for(&self, id: EndpointId) -> Policy {
        let Some(ep) = self.endpoints.get(&id) else {
            return Policy::Bypass;
        };
        match ep {
            // Clockless sources always need rtpbridge to be the clock master.
            Endpoint::WebSocket(_) | Endpoint::Bridge(_) => Policy::Engaged(PlayoutKind::Synth),
            // Generators are already paced; never buffered.
            Endpoint::File(_) | Endpoint::Tone(_) => Policy::Bypass,
            // Real network sources: buffer only where rtpbridge isn't transparent.
            Endpoint::Rtp(_) | Endpoint::WebRtc(_) => {
                // An active VAD/fax tap decodes the stream in arrival order, so it forces
                // buffering even with no routed destinations.
                let taps =
                    self.vad_monitors.contains_key(&id) || self.fax_detectors.contains_key(&id);
                let dests = self.routing.destinations(&id);
                let has_dests = dests.is_some_and(|d| !d.is_empty());
                if !has_dests {
                    return if taps {
                        Policy::Engaged(PlayoutKind::Tracked) // reorder for the analysis decode
                    } else {
                        Policy::Bypass // no consumers at all
                    };
                }
                let dests = dests.expect("has_dests implies Some");
                let src_codec = endpoint_audio_codec(ep);
                let mut mixed = false;
                let mut opaque = false;
                let mut all_transparent = true;
                for did in dests {
                    if self.routing.is_multi_source(did) {
                        mixed = true;
                    }
                    match self.endpoints.get(did) {
                        Some(dep) => {
                            let transcodes = endpoint_audio_codec(dep) != src_codec;
                            let is_plain_rtp = matches!(dep, Endpoint::Rtp(_));
                            if transcodes || is_plain_rtp {
                                opaque = true;
                            }
                            // Transparent only if every dest is WebRTC with the same codec.
                            if !matches!(dep, Endpoint::WebRtc(_)) || transcodes {
                                all_transparent = false;
                            }
                        }
                        None => all_transparent = false,
                    }
                }
                if !taps && !mixed && !opaque && all_transparent {
                    Policy::Bypass
                } else {
                    Policy::Engaged(PlayoutKind::Tracked)
                }
            }
        }
    }

    /// Construct a playout buffer of the requested kind for a source, deriving codec/rate.
    fn make_playout_buffer(&self, id: EndpointId, kind: PlayoutKind) -> Option<PlayoutBuffer> {
        let ep = self.endpoints.get(&id)?;
        let clock_rate = endpoint_rtp_clock_rate(ep);
        match kind {
            PlayoutKind::Synth => Some(PlayoutBuffer::synth(
                id,
                clock_rate,
                rand::random(),
                rand::random(),
                rand::random(),
            )),
            // Deep paced (mixer-fed) vs shallow reorder-only.
            PlayoutKind::Tracked => Some(PlayoutBuffer::tracked(
                id,
                clock_rate,
                self.source_is_mixer_fed(id),
            )),
        }
    }

    /// Whether a source routes to any multi-source (mixer) destination → deep paced Tracked.
    fn source_is_mixer_fed(&self, id: EndpointId) -> bool {
        self.routing
            .destinations(&id)
            .is_some_and(|dests| dests.iter().any(|d| self.routing.is_multi_source(d)))
    }

    fn emit_stats(&self) {
        let ep_stats: Vec<crate::control::protocol::EndpointStats> = self
            .endpoints
            .values()
            .map(|ep| {
                let stats = ep.stats();
                let include_diagnostics = self.stats_include_diagnostics;
                let (local_rtp_addr, remote_rtp_addr, offer_generation) = match ep {
                    Endpoint::WebRtc(w) => {
                        let (local, remote) = w.recording_addrs();
                        (
                            local.map(|a| a.to_string()),
                            remote.map(|a| a.to_string()),
                            Some(w.offer_generation),
                        )
                    }
                    Endpoint::Rtp(r) => (
                        Some(r.local_rtp_addr.to_string()),
                        r.remote_rtp_addr.map(|a| a.to_string()),
                        None,
                    ),
                    _ => (None, None, None),
                };
                crate::control::protocol::EndpointStats {
                    endpoint_id: ep.id(),
                    inbound: InboundStats {
                        packets: stats.inbound_packets,
                        bytes: stats.inbound_bytes,
                        packets_lost: ep.packets_lost(),
                        jitter_ms: ep.jitter_ms(),
                        last_received_ms_ago: stats.ms_since_last_received().unwrap_or(0),
                        raw_packets: include_diagnostics.then(|| ep.raw_recv_packets()).flatten(),
                        raw_bytes: include_diagnostics.then(|| ep.raw_recv_bytes()).flatten(),
                        raw_rtp_packets: include_diagnostics
                            .then(|| ep.raw_recv_rtp_packets())
                            .flatten(),
                        raw_rtp_bytes: include_diagnostics
                            .then(|| ep.raw_recv_rtp_bytes())
                            .flatten(),
                        raw_rtp_packets_lost: include_diagnostics
                            .then(|| ep.raw_recv_rtp_packets_lost())
                            .flatten(),
                        raw_rtp_sequence_gaps: include_diagnostics
                            .then(|| ep.raw_recv_rtp_sequence_gaps())
                            .flatten(),
                        raw_rtp_max_sequence_gap: include_diagnostics
                            .then(|| ep.raw_recv_rtp_max_sequence_gap())
                            .flatten(),
                        raw_rtp_duplicate_packets: include_diagnostics
                            .then(|| ep.raw_recv_rtp_duplicate_packets())
                            .flatten(),
                        raw_rtp_out_of_order_packets: include_diagnostics
                            .then(|| ep.raw_recv_rtp_out_of_order_packets())
                            .flatten(),
                        raw_rtp_sequence_resets: include_diagnostics
                            .then(|| ep.raw_recv_rtp_sequence_resets())
                            .flatten(),
                        raw_rtp_last_sequence: include_diagnostics
                            .then(|| ep.raw_recv_rtp_last_sequence())
                            .flatten(),
                        raw_rtp_last_ssrc: include_diagnostics
                            .then(|| ep.raw_recv_rtp_last_ssrc())
                            .flatten(),
                        recv_loop_gap_ms: include_diagnostics
                            .then(|| ep.raw_recv_loop_gap_ms())
                            .flatten(),
                        max_recv_loop_gap_ms: include_diagnostics
                            .then(|| ep.raw_recv_max_loop_gap_ms())
                            .flatten(),
                        enqueue_wait_ms: include_diagnostics
                            .then(|| ep.raw_recv_enqueue_wait_ms())
                            .flatten(),
                        max_enqueue_wait_ms: include_diagnostics
                            .then(|| ep.raw_recv_max_enqueue_wait_ms())
                            .flatten(),
                        dequeue_delay_ms: include_diagnostics
                            .then(|| ep.raw_recv_dequeue_delay_ms())
                            .flatten(),
                        max_dequeue_delay_ms: include_diagnostics
                            .then(|| ep.raw_recv_max_dequeue_delay_ms())
                            .flatten(),
                        channel_capacity: include_diagnostics
                            .then(|| ep.raw_recv_channel_capacity())
                            .flatten(),
                        min_channel_capacity: include_diagnostics
                            .then(|| ep.raw_recv_min_channel_capacity())
                            .flatten(),
                        channel_overflows: include_diagnostics
                            .then(|| ep.raw_recv_channel_overflows())
                            .flatten(),
                    },
                    outbound: OutboundStats {
                        packets: stats.outbound_packets,
                        bytes: stats.outbound_bytes,
                    },
                    rtt_ms: ep.rtt_ms(),
                    codec: ep.codec_name(),
                    state: format!("{:?}", ep.state()),
                    local_rtp_addr,
                    remote_rtp_addr,
                    offer_generation,
                    ice_state: ep.ice_state().map(str::to_string),
                }
            })
            .collect();

        self.send_event(
            "stats",
            StatsEvent {
                endpoints: ep_stats,
            },
        );
    }
}

/// Runs the media session event loop.
#[allow(clippy::too_many_arguments)]
pub async fn run_media_session(
    session_id: SessionId,
    media_bindings: Arc<MediaBindings>,
    media_dir: Option<std::path::PathBuf>,
    file_cache: Arc<crate::playback::file_cache::FileCache>,
    endpoint_count: Arc<std::sync::atomic::AtomicUsize>,
    max_endpoints: usize,
    max_recordings: usize,
    recording_flush_timeout_secs: u64,
    recording_channel_size: usize,
    session_idle_timeout_secs: u64,
    empty_session_timeout_secs: u64,
    media_timeout_secs: u64,
    transcode_cache_size: usize,
    metrics: Arc<crate::metrics::Metrics>,
    shared_playback: Arc<crate::playback::shared_playback::SharedPlaybackManager>,
    ws_audio_registry: Arc<crate::control::ws_audio::WsAudioRegistry>,
    cmd_tx: mpsc::Sender<SessionCommand>,
    mut cmd_rx: mpsc::Receiver<SessionCommand>,
) {
    // Note: the session span is applied via .instrument() at the spawn site in mod.rs,
    // not via span.enter() here, to avoid holding a guard across await points.

    let (packet_tx, mut packet_rx) = mpsc::channel::<InboundPacket>(256);
    let media_timeout = Duration::from_secs(media_timeout_secs);
    let idle_timeout = if session_idle_timeout_secs > 0 {
        Some(Duration::from_secs(session_idle_timeout_secs))
    } else {
        None
    };
    let empty_timeout = if empty_session_timeout_secs > 0 {
        Some(Duration::from_secs(empty_session_timeout_secs))
    } else {
        None
    };
    let mut last_activity = Instant::now();

    let mut state = SessionState {
        session_id,
        media_bindings,
        media_dir,
        file_cache,
        endpoint_count,
        max_endpoints,
        metrics: Arc::clone(&metrics),
        shared_playback,
        cmd_tx,
        event_tx: None,
        critical_event_tx: None,
        dropped_events: Arc::new(AtomicU64::new(0)),
        endpoints: HashMap::new(),
        dtmf_state: HashMap::new(),
        sensitive_dtmf_endpoints: HashSet::new(),
        routing: RoutingTable::new(),
        recording_mgr: RecordingManager::with_config(
            max_recordings,
            recording_flush_timeout_secs,
            recording_channel_size,
        ),
        vad_monitors: HashMap::new(),
        stats_interval: None,
        stats_include_diagnostics: false,
        last_stats_emit: Instant::now(),
        file_rtp_states: HashMap::new(),
        tone_rtp_states: HashMap::new(),
        transcode_cache: HashMap::new(),
        url_sources: HashMap::new(),
        fax_detectors: HashMap::new(),
        analysis_decoders: HashMap::new(),
        media_timeout_emitted: std::collections::HashSet::new(),
        dtmf_injection: None,
        last_timeout_check: Instant::now(),
        empty_since: Some(Instant::now()),
        mixers: HashMap::new(),
        playout_buffers: HashMap::new(),
        playout_policy: HashMap::new(),
        mix_grid: None,
        ws_audio_registry,
    };

    info!(session_id = %session_id, "media session started");

    #[allow(unused_assignments)]
    let mut next_webrtc_timeout: Option<Instant> = None;

    loop {
        let next_timeout =
            next_webrtc_timeout.unwrap_or_else(|| Instant::now() + Duration::from_secs(1));

        let has_playing_files = state
            .endpoints
            .values()
            .any(|ep| matches!(ep, Endpoint::File(f) if f.state == EndpointState::Playing));
        let has_playing_tones = state
            .endpoints
            .values()
            .any(|ep| matches!(ep, Endpoint::Tone(t) if t.state == EndpointState::Playing));
        let has_pending_dtmf = state.dtmf_injection.is_some();
        let has_active_dtmf = super::session_dtmf::has_active_dtmf(&state.dtmf_state);
        // Connecting-watchdog: if any WebRTC endpoint has an in-flight
        // negotiation, cap sleep at 1s so the periodic check that emits the
        // WARN can't be starved by a far-out str0m timeout.
        let has_pending_webrtc_negotiation = state
            .endpoints
            .values()
            .any(|ep| matches!(ep, Endpoint::WebRtc(wep) if wep.connecting_since.is_some()));
        let sleep_duration =
            if has_playing_files || has_playing_tones || has_pending_dtmf || has_active_dtmf {
                next_timeout
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .min(Duration::from_millis(20))
            } else {
                let raw = next_timeout
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO);
                if has_pending_webrtc_negotiation {
                    raw.min(Duration::from_secs(1))
                } else {
                    raw
                }
            };
        // Wake by the next playout-grid instant so engaged buffers drain on a 20 ms cadence
        // even when no other event (packet/command/timer) is pending.
        let sleep_duration = match state.mix_grid {
            Some(grid) => sleep_duration.min(grid.saturating_duration_since(Instant::now())),
            None => sleep_duration,
        };

        let mut inbound_rtp = Vec::new();
        let mut immediate_state_events: Vec<(EndpointId, EndpointState, EndpointState)> =
            Vec::new();
        let mut immediate_ice_state_events: Vec<(EndpointId, &'static str)> = Vec::new();

        tokio::select! {
            Some(pkt) = packet_rx.recv() => {
                last_activity = Instant::now();
                record_packet_dequeue_delay(&state.endpoints, &pkt);
                let (routed, rtcp_data, bye_info) = handle_inbound_packet(&mut state.endpoints, &pkt, &state.metrics);
                if let Some(routed) = routed {
                    // Record at arrival, before the playout buffer, framed with the
                    // datagram's REAL source (not the latched remote, which can go
                    // stale after a post-lock NAT rebind).
                    let (local, desc) = match state.endpoints.get(&routed.source_endpoint_id) {
                        Some(ep) => {
                            let local = endpoint_media_addrs(ep).0;
                            (local, endpoint_stream_descriptor(ep, local, Some(pkt.source)))
                        }
                        None => (None, None),
                    };
                    record_inbound(
                        &mut state.recording_mgr,
                        &routed,
                        &state.dtmf_state,
                        &state.sensitive_dtmf_endpoints,
                        desc.as_ref(),
                        local,
                        Some(pkt.source),
                        &state.event_tx,
                        &state.critical_event_tx,
                        &state.dropped_events,
                        &state.metrics,
                    );
                    inbound_rtp.push(routed);
                }
                drain_webrtc_output_into_inbound(
                    pkt.endpoint_id,
                    &mut state.endpoints,
                    &mut state.recording_mgr,
                    &state.dtmf_state,
                    &state.sensitive_dtmf_endpoints,
                    &state.event_tx,
                    &state.critical_event_tx,
                    &state.dropped_events,
                    &state.metrics,
                    &mut inbound_rtp,
                    &mut immediate_state_events,
                    &mut immediate_ice_state_events,
                );
                if let Some((eid, rtcp_bytes)) = rtcp_data {
                    // RTCP arrives on its own socket (non-mux) from the peer's RTCP
                    // port: frame remote = the datagram's real source, local = our
                    // RTCP-side local addr.
                    let local = state.endpoints.get(&eid).and_then(endpoint_rtcp_local);
                    let dead = state.recording_mgr.record_rtcp(
                        &eid,
                        &rtcp_bytes,
                        local,
                        Some(pkt.source),
                    );
                    emit_dead_recordings(&state.event_tx, &state.critical_event_tx, &state.dropped_events, &state.metrics, dead);
                }
                if let Some((eid, bye)) = bye_info {
                    state.send_event(
                        "endpoint.rtcp_bye",
                        serde_json::json!({
                            "endpoint_id": eid,
                            "ssrc_list": bye.ssrc_list,
                            "reason": bye.reason,
                        }),
                    );
                }
            }

            Some(cmd) = cmd_rx.recv() => {
                last_activity = Instant::now();
                if !state.handle_command(cmd, &packet_tx).await {
                    break;
                }
            }

            _ = tokio::time::sleep(sleep_duration) => {
                let now = Instant::now();
                for ep in state.endpoints.values_mut() {
                    if let Endpoint::WebRtc(wep) = ep
                        && let Err(e) = wep.handle_timeout(now) {
                            warn!(endpoint_id = %wep.id, error = %e, "timeout error");
                        }
                }
            }
        }

        // Batch-drain queued packets to reduce per-packet overhead at high rates
        for _ in 0..64 {
            match packet_rx.try_recv() {
                Ok(pkt) => {
                    last_activity = Instant::now();
                    record_packet_dequeue_delay(&state.endpoints, &pkt);
                    let (routed, rtcp_data, bye_info) =
                        handle_inbound_packet(&mut state.endpoints, &pkt, &state.metrics);
                    if let Some(routed) = routed {
                        // Record at arrival (pre-buffer) with the real datagram source.
                        let (local, desc) = match state.endpoints.get(&routed.source_endpoint_id) {
                            Some(ep) => {
                                let local = endpoint_media_addrs(ep).0;
                                (
                                    local,
                                    endpoint_stream_descriptor(ep, local, Some(pkt.source)),
                                )
                            }
                            None => (None, None),
                        };
                        record_inbound(
                            &mut state.recording_mgr,
                            &routed,
                            &state.dtmf_state,
                            &state.sensitive_dtmf_endpoints,
                            desc.as_ref(),
                            local,
                            Some(pkt.source),
                            &state.event_tx,
                            &state.critical_event_tx,
                            &state.dropped_events,
                            &state.metrics,
                        );
                        inbound_rtp.push(routed);
                    }
                    drain_webrtc_output_into_inbound(
                        pkt.endpoint_id,
                        &mut state.endpoints,
                        &mut state.recording_mgr,
                        &state.dtmf_state,
                        &state.sensitive_dtmf_endpoints,
                        &state.event_tx,
                        &state.critical_event_tx,
                        &state.dropped_events,
                        &state.metrics,
                        &mut inbound_rtp,
                        &mut immediate_state_events,
                        &mut immediate_ice_state_events,
                    );
                    if let Some((eid, rtcp_bytes)) = rtcp_data {
                        // Real RTCP source (the datagram's source) + our RTCP-side
                        // local addr (the dedicated RTCP socket).
                        let local = state.endpoints.get(&eid).and_then(endpoint_rtcp_local);
                        let dead = state.recording_mgr.record_rtcp(
                            &eid,
                            &rtcp_bytes,
                            local,
                            Some(pkt.source),
                        );
                        emit_dead_recordings(
                            &state.event_tx,
                            &state.critical_event_tx,
                            &state.dropped_events,
                            &state.metrics,
                            dead,
                        );
                    }
                    if let Some((eid, bye)) = bye_info {
                        state.send_event(
                            "endpoint.rtcp_bye",
                            serde_json::json!({
                                "endpoint_id": eid,
                                "ssrc_list": bye.ssrc_list,
                                "reason": bye.reason,
                            }),
                        );
                    }
                }
                Err(_) => break,
            }
        }

        if emit_webrtc_events(
            &state.event_tx,
            &state.critical_event_tx,
            &state.dropped_events,
            &state.metrics,
            immediate_state_events,
            immediate_ice_state_events,
        ) {
            state.rebuild_routing();
        }

        // Drain pending DTMF injection one packet at a time (non-blocking)
        if let Some(ref mut inj) = state.dtmf_injection
            && super::session_dtmf::drain_dtmf_injection(inj, &mut state.endpoints, &state.metrics)
                .await
        {
            state.dtmf_injection = None;
        }

        let needs_routing_rebuild;
        (next_webrtc_timeout, needs_routing_rebuild) = poll_and_route(
            &mut state.endpoints,
            &mut state.dtmf_state,
            &state.sensitive_dtmf_endpoints,
            &state.routing,
            &state.event_tx,
            &state.critical_event_tx,
            &state.dropped_events,
            &mut state.recording_mgr,
            &mut state.vad_monitors,
            &mut state.fax_detectors,
            &mut state.analysis_decoders,
            &state.metrics,
            inbound_rtp,
            &mut state.file_rtp_states,
            &mut state.tone_rtp_states,
            &mut state.transcode_cache,
            transcode_cache_size,
            &mut state.mixers,
            &mut state.playout_buffers,
            &state.playout_policy,
            &mut state.mix_grid,
        )
        .await;
        if needs_routing_rebuild {
            state.rebuild_routing();
        }
        if state.last_timeout_check.elapsed() >= Duration::from_secs(1) {
            check_media_timeouts(
                state.session_id,
                &state.endpoints,
                &state.event_tx,
                media_timeout,
                &mut state.media_timeout_emitted,
                &state.dropped_events,
                &state.metrics,
            );
            check_connecting_watchdog(&mut state.endpoints, &state.metrics);
            // Recv-task liveness: flag any WebRTC endpoint whose UDP receive task
            // never started or died (the media-datapath wedge). Runs on this
            // reliable 1 Hz elapsed gate — not the `sleep` select arm — so
            // co-session media load cannot starve it. See
            // docs/incident-research/webrtc-recv-task-wedge.md.
            for ep in state.endpoints.values_mut() {
                if let Endpoint::WebRtc(wep) = ep {
                    wep.supervise_recv();
                }
            }
            state.check_ws_connect_timeouts().await;
            // Roll up per-buffer playout counters into the process metrics.
            for buf in state.playout_buffers.values_mut() {
                let c = buf.take_counters();
                if c.late_drops > 0 {
                    state.metrics.playout_late_drops.inc_by(c.late_drops);
                }
                if c.overflow_drops > 0 {
                    state
                        .metrics
                        .playout_overflow_drops
                        .inc_by(c.overflow_drops);
                }
                if c.underflow_fills > 0 {
                    state
                        .metrics
                        .playout_underflow_fills
                        .inc_by(c.underflow_fills);
                }
            }
            state.last_timeout_check = Instant::now();
        }
        vad_tap::check_vad_timeouts(
            &mut state.vad_monitors,
            &state.event_tx,
            &state.dropped_events,
            &state.metrics,
        );
        fax_tap::check_fax_timeouts(&mut state.fax_detectors);
        super::session_dtmf::check_dtmf_timeouts(
            &mut state.dtmf_state,
            &state.sensitive_dtmf_endpoints,
            &state.endpoints,
            &state.event_tx,
            &state.dropped_events,
            &state.metrics,
        );

        // Count active file/tone playback as session activity so send-only
        // sessions with file/tone endpoints aren't destroyed as idle.
        if state.endpoints.values().any(|ep| {
            matches!(ep, Endpoint::File(f) if f.state == EndpointState::Playing)
                || matches!(ep, Endpoint::Tone(t) if t.state == EndpointState::Playing)
        }) {
            last_activity = Instant::now();
        }

        // Session-level idle timeout: auto-destroy if no activity for the configured duration
        if let Some(idle_dur) = idle_timeout
            && last_activity.elapsed() >= idle_dur
        {
            warn!(session_id = %session_id, idle_secs = idle_dur.as_secs(),
                      "session idle timeout expired, destroying");
            state.send_event(
                "session.idle_timeout",
                serde_json::json!({
                    "session_id": session_id.to_string(),
                    "idle_timeout_secs": idle_dur.as_secs(),
                }),
            );
            break;
        }

        // Empty session timeout: auto-destroy if no endpoints for the configured duration
        if let (Some(empty_dur), Some(empty_since)) = (empty_timeout, state.empty_since)
            && empty_since.elapsed() >= empty_dur
        {
            warn!(session_id = %session_id, empty_secs = empty_dur.as_secs(),
                      "session empty timeout expired, destroying");
            state.send_event(
                "session.empty_timeout",
                serde_json::json!({
                    "session_id": session_id.to_string(),
                    "empty_timeout_secs": empty_dur.as_secs(),
                }),
            );
            break;
        }

        if let Some(interval) = state.stats_interval
            && state.last_stats_emit.elapsed() >= interval
        {
            state.last_stats_emit = Instant::now();
            state.emit_stats();
        }

        for ep in state.endpoints.values_mut() {
            if let Endpoint::Rtp(rep) = ep {
                match rep.maybe_send_rtcp(&state.metrics).await {
                    // `maybe_send_rtcp` already transmits on the wire; we no longer
                    // record what we send (outbound recording dropped). Inbound RTCP
                    // is still captured at arrival above.
                    Ok(_) => {}
                    Err(e) => {
                        debug!(endpoint_id = %rep.id, error = %e, "RTCP send error");
                    }
                }
            }
        }
    }

    // Drop all pending WS audio connect tokens for this session up front, before
    // the teardown awaits below, so a racing audio connect can't claim a token
    // for a session that has stopped servicing commands.
    state.ws_audio_registry.remove_session(&session_id);

    let stopped_recs = state.recording_mgr.stop_all();
    for info in &stopped_recs {
        metrics.recordings_active.dec();
        state.send_event(
            "recording.stopped",
            RecordingStoppedData {
                recording_id: info.recording_id,
                file_path: info.file_path.clone(),
                duration_ms: info.duration_ms,
                packets: info.packets,
                dropped_packets: info.dropped_packets,
                reason: "session_destroyed".to_string(),
            },
        );
    }

    // Release URL file-cache references before clearing endpoints,
    // otherwise ref_count stays elevated and prevents cache eviction.
    for (_eid, url) in state.url_sources.drain() {
        state.file_cache.release(&url).await;
    }

    // Explicitly clean up shared playback subscribers before dropping endpoints,
    // so async ref_count decrement happens reliably.
    for ep in state.endpoints.values_mut() {
        if let Endpoint::File(fep) = ep
            && let Some(sub) = fep.shared_sub.take()
        {
            sub.cleanup().await;
        }
    }

    let remaining_ep_count = state.endpoints.len();
    for _ in 0..remaining_ep_count {
        metrics.endpoints_active.dec();
    }
    state.endpoints.clear();
    // Zero the shared endpoint count so the cleanup guard in mod.rs
    // doesn't double-decrement endpoints_active.
    state
        .endpoint_count
        .store(0, std::sync::atomic::Ordering::Release);
    drop(packet_tx);
    info!(session_id = %session_id, "media session ended");
}

/// Returns (RTP packet for routing, RTCP bytes for recording tap, optional RTCP BYE).
#[allow(clippy::type_complexity)]
fn handle_inbound_packet(
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    pkt: &InboundPacket,
    metrics: &crate::metrics::Metrics,
) -> (
    Option<RoutedRtpPacket>,
    Option<(EndpointId, Vec<u8>)>,
    Option<(EndpointId, crate::media::rtcp::ByePacket)>,
) {
    let now = pkt.recv_at;

    if let Some(ep) = endpoints.get_mut(&pkt.endpoint_id) {
        match ep {
            Endpoint::WebRtc(wep) => {
                // Dual-stack: a datagram is tagged with the socket (local addr)
                // it arrived on; fall back to the primary local addr for
                // single-socket endpoints (which leave `local` unset).
                let local = pkt.local.unwrap_or(wep.local_addr);
                if let Err(e) = wep.handle_receive(pkt.source, local, &pkt.data, now) {
                    metrics.webrtc_packet_errors.inc();
                    if !wep.packet_error_warned {
                        wep.packet_error_warned = true;
                        warn!(
                            endpoint_id = %pkt.endpoint_id,
                            error = %e,
                            "WebRTC packet error (first for this endpoint; subsequent errors only update rtpbridge_webrtc_packet_errors)"
                        );
                    } else {
                        debug!(endpoint_id = %pkt.endpoint_id, error = %e, "WebRTC packet error");
                    }
                }
            }
            Endpoint::Rtp(rep) => {
                // Classify RTCP: definitive from dedicated RTCP socket,
                // or via PT demux when rtcp-mux is negotiated
                let is_rtcp =
                    pkt.is_rtcp || (rep.rtcp_mux && RtpEndpoint::is_rtcp_mux_packet(&pkt.data));
                if is_rtcp {
                    let (bye, decrypted_rtcp) = rep.handle_rtcp(&pkt.data);
                    let rtcp_for_recording = decrypted_rtcp.map(|d| (pkt.endpoint_id, d));
                    let bye_info = bye.map(|b| (pkt.endpoint_id, b));
                    return (None, rtcp_for_recording, bye_info);
                } else {
                    let has_srtp = rep.has_srtp();
                    let result = rep.handle_rtp(&pkt.data, pkt.source);
                    if has_srtp && result.is_none() {
                        metrics.srtp_errors.inc();
                    }
                    return (result, None, None);
                }
            }
            Endpoint::File(_) | Endpoint::Tone(_) => {} // File/tone endpoints don't receive packets
            Endpoint::Bridge(bep) => {
                // Bridge packets arrive via channel with raw audio payload (no RTP header).
                // Construct a RoutedRtpPacket so the routing pipeline can forward it.
                bep.stats.record_inbound(pkt.data.len());
                return (
                    Some(RoutedRtpPacket {
                        source_endpoint_id: pkt.endpoint_id,
                        payload_type: 127, // L16 bridge PT
                        sequence_number: 0,
                        timestamp: 0,
                        ssrc: 0,
                        marker: false,
                        payload: pkt.data.clone(),
                    }),
                    None,
                    None,
                );
            }
            Endpoint::WebSocket(wsep) => {
                // Inbound native-rate L16 frame from the WS IO task. Wrapped raw; the
                // source's playout::SynthClock owns the monotonic timeline + pacing.
                return (Some(wsep.wrap_inbound(pkt.data.clone())), None, None);
            }
        }
    }
    (None, None, None)
}

fn record_packet_dequeue_delay(endpoints: &HashMap<EndpointId, Endpoint>, pkt: &InboundPacket) {
    if let Some(ep) = endpoints.get(&pkt.endpoint_id) {
        ep.record_raw_recv_dequeue_delay(Instant::now().saturating_duration_since(pkt.recv_at));
    }
}

/// Returns (min WebRTC timeout, whether routing table needs rebuild due to state changes).
#[allow(clippy::too_many_arguments)]
/// Ingest one audio packet from a source: into its playout buffer if engaged, otherwise
/// straight to the route set (transparent relay / no policy entry).
fn ingest_audio(
    pkt: RoutedRtpPacket,
    playout_policy: &HashMap<EndpointId, Policy>,
    playout_buffers: &mut HashMap<EndpointId, PlayoutBuffer>,
    packets_to_route: &mut Vec<RoutedRtpPacket>,
    now: Instant,
) {
    match playout_policy.get(&pkt.source_endpoint_id) {
        Some(Policy::Engaged(_)) => match playout_buffers.get_mut(&pkt.source_endpoint_id) {
            Some(buf) => buf.push(pkt, now),
            None => packets_to_route.push(pkt), // engaged but no buffer yet — don't drop
        },
        _ => packets_to_route.push(pkt),
    }
}

/// Advance the shared playout grid. When a 20 ms tick is due, drain one frame from every
/// engaged buffer into `packets_to_route` and step the grid (with a catch-up clamp so a
/// stalled loop re-syncs instead of bursting). Parks the grid (`None`) when nothing is
/// pending. Returns whether a tick fired this pass (gates the mixer `flush_tick`).
fn drive_grid(
    mix_grid: &mut Option<Instant>,
    playout_buffers: &mut HashMap<EndpointId, PlayoutBuffer>,
    packets_to_route: &mut Vec<RoutedRtpPacket>,
    now: Instant,
) -> bool {
    let fired = match *mix_grid {
        Some(g) => now >= g,
        None => {
            if playout_buffers.values().any(|b| b.has_pending()) {
                *mix_grid = Some(now);
                true
            } else {
                false
            }
        }
    };
    if fired {
        let g = mix_grid.unwrap_or(now);
        for buf in playout_buffers.values_mut() {
            // Synth and mixer-fed Tracked release one frame per tick; reorder-only Tracked
            // drains its whole releasable burst (no pacing — the downstream endpoint plays out).
            while let Some(pkt) = buf.drain_tick(g) {
                packets_to_route.push(pkt);
                if !buf.drains_burst() {
                    break;
                }
            }
        }
        let mut next = g + super::playout::FRAME;
        // Catch-up clamp (mirrors the file/tone pollers): if we fell >3 ticks behind, resync
        // to avoid emitting a multi-frame burst.
        if now > next + Duration::from_millis(60) {
            next = now + super::playout::FRAME;
        }
        *mix_grid = Some(next);
    }
    // Park the grid when no buffer has pending audio (Synth idle / Tracked empty).
    if !playout_buffers.values().any(|b| b.has_pending()) {
        *mix_grid = None;
    }
    fired
}

/// Real `(local, remote)` media socket addresses for an endpoint, used to frame
/// recorded packets with their true IP:port. `remote` is `None` until the peer
/// (plain RTP) or ICE-nominated address (WebRTC) is learned; endpoints with no
/// real socket (file/tone/bridge/websocket) return `(None, None)` and recording
/// falls back to synthetic markers.
fn endpoint_media_addrs(ep: &Endpoint) -> (Option<SocketAddr>, Option<SocketAddr>) {
    match ep {
        Endpoint::Rtp(r) => (Some(r.local_rtp_addr), r.remote_rtp_addr),
        // Use the nominated-family local socket, not the primary (dual-stack).
        Endpoint::WebRtc(w) => w.recording_addrs(),
        _ => (None, None),
    }
}

/// Local address for framing recorded inbound RTCP (plain RTP only — WebRTC RTCP
/// is consumed inside str0m). With rtcp-mux the RTCP is demuxed off the RTP socket,
/// so the RTP-socket local address is correct; otherwise it arrived on the
/// dedicated RTCP socket (RTP port + 1).
fn endpoint_rtcp_local(ep: &Endpoint) -> Option<SocketAddr> {
    match ep {
        Endpoint::Rtp(r) => {
            if r.rtcp_mux {
                Some(r.local_rtp_addr)
            } else {
                r.rtcp_socket.local_addr().ok()
            }
        }
        _ => None,
    }
}

/// Record one decrypted inbound RTP/DTMF packet at arrival — before the playout
/// jitter buffer — so the PCAP preserves real arrival order and timing, framed
/// with the real remote/local addresses. Cheap no-op when nothing is recording.
#[allow(clippy::too_many_arguments)]
fn record_inbound(
    recording_mgr: &mut RecordingManager,
    pkt: &RoutedRtpPacket,
    dtmf_state: &HashMap<EndpointId, EndpointDtmf>,
    sensitive_dtmf_endpoints: &HashSet<EndpointId>,
    descriptor: Option<&crate::recording::meta::StreamDescriptor>,
    local: Option<SocketAddr>,
    remote: Option<SocketAddr>,
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    if !recording_mgr.is_recording() {
        return;
    }
    if !should_record_inbound(pkt, dtmf_state, sensitive_dtmf_endpoints) {
        return;
    }
    // Refresh the cached descriptor (no-op unless codec/addr changed) so it is
    // prepended before this packet whenever it advanced — including after a
    // plain-RTP source-address latch, where `remote` changes here.
    if let Some(desc) = descriptor {
        recording_mgr.note_descriptor(&pkt.source_endpoint_id, desc, local, remote);
    }
    let raw_rtp = crate::media::rtp::RtpHeader::build(
        pkt.payload_type,
        pkt.sequence_number,
        pkt.timestamp,
        pkt.ssrc,
        pkt.marker,
        &pkt.payload,
    );
    let dead = recording_mgr.record_packet_addr(&pkt.source_endpoint_id, &raw_rtp, local, remote);
    emit_dead_recordings(event_tx, critical_event_tx, dropped_events, metrics, dead);
    metrics.packets_recorded.inc();
}

fn should_record_inbound(
    pkt: &RoutedRtpPacket,
    dtmf_state: &HashMap<EndpointId, EndpointDtmf>,
    sensitive_dtmf_endpoints: &HashSet<EndpointId>,
) -> bool {
    !sensitive_dtmf_endpoints.contains(&pkt.source_endpoint_id)
        || !super::session_dtmf::classify_dtmf(pkt, dtmf_state)
}

#[allow(clippy::too_many_arguments)]
fn drain_webrtc_output_into_inbound(
    endpoint_id: EndpointId,
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    recording_mgr: &mut RecordingManager,
    dtmf_state: &HashMap<EndpointId, EndpointDtmf>,
    sensitive_dtmf_endpoints: &HashSet<EndpointId>,
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
    inbound_rtp: &mut Vec<RoutedRtpPacket>,
    state_events: &mut Vec<(EndpointId, EndpointState, EndpointState)>,
    ice_state_events: &mut Vec<(EndpointId, &'static str)>,
) -> Option<Instant> {
    let Some(Endpoint::WebRtc(wep)) = endpoints.get_mut(&endpoint_id) else {
        return None;
    };

    match wep.poll_output() {
        Ok((events, timeout)) => {
            for event in events {
                match event {
                    WebRtcEvent::RtpPacket(pkt) => {
                        // str0m 0.21 RTP mode stores a single pending RTP packet.
                        // Drain after each handle_receive() so a later datagram
                        // cannot replace it before the session-level poll.
                        let (local, remote) = wep.recording_addrs();
                        let desc = wep.stream_descriptor(local, remote);
                        record_inbound(
                            recording_mgr,
                            &pkt,
                            dtmf_state,
                            sensitive_dtmf_endpoints,
                            desc.as_ref(),
                            local,
                            remote,
                            event_tx,
                            critical_event_tx,
                            dropped_events,
                            metrics,
                        );
                        inbound_rtp.push(pkt);
                    }
                    WebRtcEvent::StateChanged { old, new } => {
                        state_events.push((wep.id, old, new));
                    }
                    WebRtcEvent::IceStateChanged { state } => {
                        ice_state_events.push((wep.id, ice_state_str(state)));
                    }
                }
            }
            Some(timeout)
        }
        Err(e) => {
            warn!(endpoint_id = %wep.id, error = %e, "poll_output error");
            None
        }
    }
}

fn emit_webrtc_events(
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
    state_events: Vec<(EndpointId, EndpointState, EndpointState)>,
    ice_state_events: Vec<(EndpointId, &'static str)>,
) -> bool {
    let mut needs_routing_rebuild = false;
    for (eid, old, new) in state_events {
        if new == EndpointState::Connected && old != EndpointState::Connected {
            needs_routing_rebuild = true;
        }
        emit_event_with_priority(
            event_tx,
            critical_event_tx,
            "endpoint.state_changed",
            EndpointStateChangedData {
                endpoint_id: eid,
                old_state: old,
                new_state: new,
            },
            dropped_events,
            metrics,
        );
    }

    for (eid, ice_state) in ice_state_events {
        emit_event_with_priority(
            event_tx,
            critical_event_tx,
            "endpoint.ice_state_changed",
            IceStateChangedData {
                endpoint_id: eid,
                ice_state: ice_state.to_string(),
            },
            dropped_events,
            metrics,
        );
    }

    needs_routing_rebuild
}

#[allow(clippy::too_many_arguments)]
async fn poll_and_route(
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    dtmf_state: &mut HashMap<EndpointId, EndpointDtmf>,
    sensitive_dtmf_endpoints: &HashSet<EndpointId>,
    routing: &RoutingTable,
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    recording_mgr: &mut RecordingManager,
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    fax_detectors: &mut HashMap<EndpointId, FaxDetector>,
    analysis_decoders: &mut HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>>,
    metrics: &crate::metrics::Metrics,
    inbound_rtp: Vec<RoutedRtpPacket>,
    file_rtp_states: &mut HashMap<EndpointId, FileRtpState>,
    tone_rtp_states: &mut HashMap<EndpointId, super::tone_poll::ToneRtpState>,
    transcode_cache: &mut HashMap<(EndpointId, EndpointId), CachedTranscode>,
    transcode_cache_size: usize,
    mixers: &mut HashMap<EndpointId, super::mixer::DestinationMixer>,
    playout_buffers: &mut HashMap<EndpointId, PlayoutBuffer>,
    playout_policy: &HashMap<EndpointId, Policy>,
    mix_grid: &mut Option<Instant>,
) -> (Option<Instant>, bool) {
    let now = Instant::now();
    let mut packets_to_route: Vec<RoutedRtpPacket> = Vec::new();
    let mut dtmf_packets: Vec<RoutedRtpPacket> = Vec::new();
    let mut state_events: Vec<(EndpointId, EndpointState, EndpointState)> = Vec::new();
    let mut ice_state_events: Vec<(EndpointId, &'static str)> = Vec::new();
    let mut min_webrtc_timeout: Option<Instant> = None;
    let mut inbound_rtp = inbound_rtp;

    // Poll WebRTC endpoints for output and track their next timeouts. RTP emitted
    // here is folded into the common inbound path, same as packets drained
    // immediately after handle_receive().
    let webrtc_endpoint_ids: Vec<EndpointId> = endpoints
        .iter()
        .filter_map(|(id, ep)| matches!(ep, Endpoint::WebRtc(_)).then_some(*id))
        .collect();
    for endpoint_id in webrtc_endpoint_ids {
        if let Some(timeout) = drain_webrtc_output_into_inbound(
            endpoint_id,
            endpoints,
            recording_mgr,
            dtmf_state,
            sensitive_dtmf_endpoints,
            event_tx,
            critical_event_tx,
            dropped_events,
            metrics,
            &mut inbound_rtp,
            &mut state_events,
            &mut ice_state_events,
        ) {
            min_webrtc_timeout = Some(match min_webrtc_timeout {
                Some(prev) => prev.min(timeout),
                None => timeout,
            });
        }
    }

    // Inbound RTP (plain RTP / WS / Bridge / WebRTC). Telephone-event splits
    // off out-of-band first; audio is ingested into its playout buffer
    // (engaged) or routed directly (bypass).
    // (Recording happens at the recv sites — see `record_inbound` calls in the run
    // loop — so the PCAP captures the true datagram source before the buffer.)
    for pkt in inbound_rtp {
        if super::session_dtmf::classify_dtmf(&pkt, dtmf_state) {
            dtmf_packets.push(pkt);
        } else {
            ingest_audio(
                pkt,
                playout_policy,
                playout_buffers,
                &mut packets_to_route,
                now,
            );
        }
    }

    // File/tone endpoints are locally-generated sources that bypass the inbound
    // recv path; capture how many real-network packets are already queued so we can
    // record just the generated ones below (with synthetic framing — they have no
    // real socket).
    let generated_start = packets_to_route.len();

    // Poll file endpoints for PCM output
    super::file_poll::poll_file_endpoints(
        endpoints,
        file_rtp_states,
        event_tx,
        critical_event_tx,
        dropped_events,
        metrics,
        &mut packets_to_route,
    );

    // Poll tone endpoints for synthesized audio
    super::tone_poll::poll_tone_endpoints(
        endpoints,
        tone_rtp_states,
        event_tx,
        critical_event_tx,
        dropped_events,
        metrics,
        &mut packets_to_route,
    );

    // Record locally-generated file/tone audio (as inbound for that source). These
    // are paced generators with no real peer, so they frame with synthetic markers.
    if recording_mgr.is_recording() {
        for routed in &packets_to_route[generated_start..] {
            let (local, remote, desc) = match endpoints.get(&routed.source_endpoint_id) {
                Some(ep) => {
                    let (local, remote) = endpoint_media_addrs(ep);
                    (local, remote, endpoint_stream_descriptor(ep, local, remote))
                }
                None => (None, None, None),
            };
            record_inbound(
                recording_mgr,
                routed,
                dtmf_state,
                sensitive_dtmf_endpoints,
                desc.as_ref(),
                local,
                remote,
                event_tx,
                critical_event_tx,
                dropped_events,
                metrics,
            );
        }
    }

    // Shared 20 ms grid: drain each engaged buffer's due frame into the route set. All buffers
    // are evaluated against the same instant so a mixer's sources stay frame-aligned.
    let grid_fired = drive_grid(mix_grid, playout_buffers, &mut packets_to_route, now);

    let needs_routing_rebuild = emit_webrtc_events(
        event_tx,
        critical_event_tx,
        dropped_events,
        metrics,
        state_events,
        ice_state_events,
    );

    // Process DTMF packets: detect events, forward without transcoding
    super::session_dtmf::process_dtmf_packets(
        &dtmf_packets,
        dtmf_state,
        sensitive_dtmf_endpoints,
        routing,
        endpoints,
        event_tx,
        dropped_events,
        metrics,
    )
    .await;

    // (Inbound RTP/DTMF is recorded at arrival above — before the playout buffer —
    // so the PCAP keeps real arrival order/timing instead of the re-paced grid.)

    // Analysis tap: decode each packet to PCM once and feed VAD + fax detectors.
    audio_analysis::process_analysis(
        &packets_to_route,
        endpoints,
        vad_monitors,
        fax_detectors,
        analysis_decoders,
        event_tx,
        dropped_events,
        metrics,
    );

    // Route audio packets to destinations (with transcoding/mixing as needed)
    for pkt in packets_to_route {
        if let Some(dests) = routing.destinations(&pkt.source_endpoint_id) {
            // Derive source codec from the endpoint's negotiated codec, not the
            // packet PT — dynamic PTs (e.g. non-111 Opus) aren't in the static map.
            let src_codec = endpoints
                .get(&pkt.source_endpoint_id)
                .and_then(endpoint_audio_codec);
            let src_clock = src_codec.map(|c| c.rtp_clock_rate()).unwrap_or(8000);

            // Pass 1: collect destination info (immutable borrow on endpoints)
            let dest_info: Vec<(EndpointId, Option<AudioCodec>, Option<u8>, u32)> = dests
                .iter()
                .filter_map(|&did| {
                    endpoints.get(&did).map(|ep| {
                        (
                            did,
                            endpoint_audio_codec(ep),
                            endpoint_send_pt(ep),
                            endpoint_rtp_clock_rate(ep),
                        )
                    })
                })
                .collect();

            // Pass 2: route to each destination
            for (dest_id, dest_codec, dest_pt, dest_clock) in dest_info {
                // Multi-source destinations: feed to mixer (decoded to PCM internally)
                if let Some(mixer) = mixers.get_mut(&dest_id) {
                    if let Some(sc) = src_codec
                        && let Err(e) = mixer.feed(pkt.source_endpoint_id, sc, &pkt.payload)
                    {
                        debug!(
                            src = %pkt.source_endpoint_id,
                            dst = %dest_id,
                            error = %e,
                            "mixer feed error, dropping packet"
                        );
                    }
                    continue;
                }

                // Single-source destinations: transcode/passthrough as before
                let needs_transcode = matches!(
                    (src_codec, dest_codec),
                    (Some(s), Some(d)) if s != d
                );

                let routed = if needs_transcode {
                    let cache_key = (pkt.source_endpoint_id, dest_id);
                    let pipeline = if let Some(cached) = transcode_cache.get_mut(&cache_key) {
                        cached.last_used = Instant::now();
                        &mut cached.pipeline
                    } else {
                        let (sc, dc) = match (src_codec, dest_codec) {
                            (Some(s), Some(d)) => (s, d),
                            _ => {
                                warn!(
                                    src = %pkt.source_endpoint_id,
                                    dst = %dest_id,
                                    "transcode codecs unexpectedly None, dropping packet"
                                );
                                metrics.transcode_errors.inc();
                                continue;
                            }
                        };
                        match TranscodePipeline::new(sc, dc) {
                            Ok(p) => {
                                // Evict oldest entry if cache is at capacity
                                // O(n) LRU scan; acceptable for typical cache sizes (≤ 100 entries)
                                if transcode_cache.len() >= transcode_cache_size
                                    && let Some(oldest_key) = transcode_cache
                                        .iter()
                                        .min_by_key(|(_, v)| v.last_used)
                                        .map(|(k, _)| *k)
                                {
                                    transcode_cache.remove(&oldest_key);
                                }
                                transcode_cache.insert(
                                    cache_key,
                                    CachedTranscode {
                                        pipeline: p,
                                        last_used: Instant::now(),
                                    },
                                );
                                &mut transcode_cache
                                    .get_mut(&cache_key)
                                    .expect("just inserted cache_key must exist")
                                    .pipeline
                            }
                            Err(e) => {
                                warn!(
                                    src = %pkt.source_endpoint_id,
                                    dst = %dest_id,
                                    error = %e,
                                    "transcode pipeline creation failed, dropping packet"
                                );
                                metrics.transcode_errors.inc();
                                continue;
                            }
                        }
                    };
                    match pipeline.process(&pkt.payload) {
                        Ok(data) => {
                            let ts = if src_clock != dest_clock && src_clock > 0 {
                                ((pkt.timestamp as u64 * dest_clock as u64) / src_clock as u64)
                                    as u32
                            } else {
                                pkt.timestamp
                            };
                            RoutedRtpPacket {
                                source_endpoint_id: pkt.source_endpoint_id,
                                payload_type: dest_pt.unwrap_or(pkt.payload_type),
                                sequence_number: pkt.sequence_number,
                                timestamp: ts,
                                ssrc: pkt.ssrc,
                                marker: pkt.marker,
                                payload: data.to_vec(),
                            }
                        }
                        Err(e) => {
                            debug!(
                                src = %pkt.source_endpoint_id,
                                dst = %dest_id,
                                error = %e,
                                "transcode error, dropping packet"
                            );
                            metrics.transcode_errors.inc();
                            continue;
                        }
                    }
                } else {
                    // Same codec or unknown — passthrough with PT remap
                    let mut p = pkt.clone();
                    if let Some(pt) = dest_pt {
                        p.payload_type = pt;
                    }
                    p
                };

                if let Some(dest_ep) = endpoints.get_mut(&dest_id) {
                    let result: anyhow::Result<Option<Vec<u8>>> = match dest_ep {
                        Endpoint::WebRtc(wep) => wep.write_rtp(&routed).map(|()| None),
                        Endpoint::Rtp(rep) => rep.write_rtp(&routed, metrics).await,
                        Endpoint::File(_) | Endpoint::Tone(_) => Ok(None),
                        Endpoint::Bridge(bep) => bep.write_rtp(&routed).await.map(|()| None),
                        Endpoint::WebSocket(wsep) => wsep.write_rtp(&routed),
                    };
                    match result {
                        Err(e) => {
                            warn!(src = %pkt.source_endpoint_id, dst = %dest_id, error = %e, "route error")
                        }
                        // The write_rtp above already sent to the peer; we no longer
                        // record what we send (outbound recording dropped).
                        Ok(_) => {
                            metrics.packets_routed.inc();
                        }
                    }
                }
            }
        }
    }

    // On a grid tick, flush each mixer's accumulated frame so the mixer is wall-clock-clocked
    // when fed by paced playout buffers. Additive with feed()'s implicit second-contribution
    // flush (which still handles file/tone catch-up and arrival-fed RTP bursts); the inner
    // flush is guarded so an all-idle tick emits nothing.
    if grid_fired {
        for mixer in mixers.values_mut() {
            if let Err(e) = mixer.flush_tick() {
                warn!(error = %e, "mixer flush_tick error");
            }
        }
    }

    // Deliver mixed frames queued by feed() (flushed on frame boundaries) + grid flush_tick
    for (&dest_id, mixer) in mixers.iter_mut() {
        for routed in mixer.drain() {
            if let Some(dest_ep) = endpoints.get_mut(&dest_id) {
                let result: anyhow::Result<Option<Vec<u8>>> = match dest_ep {
                    Endpoint::WebRtc(wep) => wep.write_rtp(&routed).map(|()| None),
                    Endpoint::Rtp(rep) => rep.write_rtp(&routed, metrics).await,
                    Endpoint::File(_) | Endpoint::Tone(_) => Ok(None),
                    Endpoint::Bridge(bep) => bep.write_rtp(&routed).await.map(|()| None),
                    Endpoint::WebSocket(wsep) => wsep.write_rtp(&routed),
                };
                match result {
                    Err(e) => warn!(dst = %dest_id, error = %e, "mixer route error"),
                    // Outbound recording dropped — write_rtp already sent the packet.
                    Ok(_) => {
                        metrics.packets_routed.inc();
                    }
                }
            }
        }
    }

    (min_webrtc_timeout, needs_routing_rebuild)
}

fn check_media_timeouts(
    session_id: SessionId,
    endpoints: &HashMap<EndpointId, Endpoint>,
    event_tx: &Option<mpsc::Sender<Event>>,
    threshold: Duration,
    emitted: &mut std::collections::HashSet<EndpointId>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    for ep in endpoints.values() {
        if ep.state() != EndpointState::Connected {
            continue;
        }
        let eid = ep.id();
        // A WebRTC endpoint with an in-flight (re)negotiation (e.g. an ICE
        // restart) is briefly without inbound media by design, while its
        // top-level state stays `Connected`. Don't reap it on the media-timeout
        // path *during the negotiation window* — reaping at the shorter media
        // timeout would tear down a call that is mid-recovery and mask the
        // negotiation as a generic media gap. We reset the emitted entry so a
        // fresh window starts once it settles.
        //
        // BUT only suppress within the connecting-watchdog window. A healthy
        // ICE (re)negotiation completes in ~1–3s; if `connecting_since` is still
        // set past `WEBRTC_CONNECTING_WATCHDOG_SECS`, the negotiation is stuck
        // (str0m emitted neither Connected nor Disconnected), and the
        // connecting-watchdog only bumps a metric — it does NOT notify the
        // controller. So past that point we must let `endpoint.media_timeout`
        // fire, otherwise a blackholed call is suppressed indefinitely and loses
        // its only external recovery signal.
        if let Endpoint::WebRtc(wep) = ep
            && let Some(since) = wep.connecting_since
            && since.elapsed() < Duration::from_secs(WEBRTC_CONNECTING_WATCHDOG_SECS)
        {
            emitted.remove(&eid);
            continue;
        }
        let stats = ep.stats();
        let ms = stats
            .ms_since_last_received()
            .unwrap_or_else(|| stats.created_at.elapsed().as_millis() as u64);
        if ms > threshold.as_millis() as u64 {
            // Only emit once per timeout period; cleared when packet received
            if emitted.insert(eid) {
                let endpoint_type = ep.kind_label();
                metrics.record_endpoint_media_timeout(endpoint_type);
                warn!(
                    session_id = %session_id,
                    endpoint_id = %eid,
                    endpoint_type,
                    duration_ms = ms,
                    threshold_ms = threshold.as_millis() as u64,
                    state = ?ep.state(),
                    inbound_packets = stats.inbound_packets,
                    inbound_bytes = stats.inbound_bytes,
                    outbound_packets = stats.outbound_packets,
                    outbound_bytes = stats.outbound_bytes,
                    raw_packets = ?ep.raw_recv_packets(),
                    raw_bytes = ?ep.raw_recv_bytes(),
                    packets_lost = ep.packets_lost(),
                    jitter_ms = ep.jitter_ms(),
                    rtt_ms = ?ep.rtt_ms(),
                    local_rtp_addr = ?ep.local_rtp_addr(),
                    remote_rtp_addr = ?ep.remote_rtp_addr(),
                    offer_generation = ?ep.offer_generation(),
                    ice_state = ?ep.ice_state(),
                    "endpoint media timeout"
                );
                emit_event(
                    event_tx,
                    "endpoint.media_timeout",
                    MediaTimeoutData {
                        endpoint_id: eid,
                        duration_ms: ms,
                    },
                    dropped_events,
                    metrics,
                );
            }
        } else {
            // Receiving media again — reset so next timeout fires fresh
            emitted.remove(&eid);
        }
    }
}

/// Threshold after which a WebRTC endpoint stuck in `Connecting` triggers a
/// watchdog WARN. Picked to comfortably outlast a healthy ICE+DTLS handshake
/// (~1–3s in practice) while still firing well before SIP-level call timeouts.
const WEBRTC_CONNECTING_WATCHDOG_SECS: u64 = 15;

/// Warns once per negotiation attempt for any WebRTC endpoint whose
/// `connecting_since` has exceeded the watchdog threshold. Top-level
/// `EndpointState` is NOT gated on here: an ICE restart on an already-
/// `Connected` endpoint can stall without changing the state, and we still
/// want that stall to fire the watchdog. Cleared on str0m's Connected /
/// Disconnected events. Bumps `rtpbridge_webrtc_connecting_stuck` per warn.
fn check_connecting_watchdog(
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    metrics: &crate::metrics::Metrics,
) {
    let threshold = Duration::from_secs(WEBRTC_CONNECTING_WATCHDOG_SECS);
    for ep in endpoints.values_mut() {
        if let Endpoint::WebRtc(wep) = ep
            && let Some(since) = wep.connecting_since
            && !wep.connecting_warned
            && since.elapsed() >= threshold
        {
            wep.connecting_warned = true;
            metrics.webrtc_connecting_stuck.inc();
            warn!(
                endpoint_id = %wep.id,
                local_addr = %wep.local_addr,
                state = ?wep.state,
                stuck_secs = since.elapsed().as_secs(),
                "WebRTC negotiation stuck past watchdog threshold (no ICE/DTLS completion since last attempt)"
            );
        }
    }
}

pub(super) fn emit_event(
    event_tx: &Option<mpsc::Sender<Event>>,
    name: &str,
    data: impl serde::Serialize,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    emit_event_with_priority(event_tx, &None, name, data, dropped_events, metrics);
}

/// Emit an event, routing critical events to the priority channel first.
/// Mirrors the logic in `SessionState::send_event` for callers that don't
/// have access to the full SessionState.
pub(super) fn emit_event_with_priority(
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    name: &str,
    data: impl serde::Serialize,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
) {
    debug!(event = name, "emitting event");
    if let Some(tx) = event_tx {
        let event = Event::new(name, data);
        // Route critical events to the priority channel first
        if CRITICAL_EVENTS.contains(&name)
            && let Some(critical_tx) = critical_event_tx
        {
            match critical_tx.try_send(event) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(event)) => {
                    // Fall through to normal channel
                    if tx.try_send(event).is_err() {
                        dropped_events.fetch_add(1, Ordering::Relaxed);
                        metrics.events_dropped.inc();
                        tracing::warn!(
                            event_name = name,
                            "critical event dropped: both channels full"
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    dropped_events.fetch_add(1, Ordering::Relaxed);
                    metrics.events_dropped.inc();
                    tracing::warn!(event_name = name, "event dropped: channel closed");
                }
            }
            return;
        }
        if tx.try_send(event).is_err() {
            dropped_events.fetch_add(1, Ordering::Relaxed);
            metrics.events_dropped.inc();
            tracing::warn!(event_name = name, "event dropped: channel full or closed");
        }
    }
}

/// Emit recording.stopped events for recordings that died due to write errors.
fn emit_dead_recordings(
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    metrics: &crate::metrics::Metrics,
    dead: Vec<crate::recording::recorder::StoppedRecordingInfo>,
) {
    for info in dead {
        metrics.recordings_active.dec();
        emit_event_with_priority(
            event_tx,
            critical_event_tx,
            "recording.stopped",
            RecordingStoppedData {
                recording_id: info.recording_id,
                file_path: info.file_path,
                duration_ms: info.duration_ms,
                packets: info.packets,
                dropped_packets: info.dropped_packets,
                reason: "write_error".to_string(),
            },
            dropped_events,
            metrics,
        );
    }
}

#[cfg(test)]
#[path = "media_session_tests.rs"]
mod tests;
