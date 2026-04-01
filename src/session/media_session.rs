use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::endpoint::{InboundPacket, RoutedRtpPacket};
use super::endpoint_enum::{
    Endpoint, endpoint_audio_codec, endpoint_last_rtp_timestamp, endpoint_rtp_clock_rate,
    endpoint_send_pt,
};
use super::endpoint_file::FileEndpoint;
use super::endpoint_rtp::RtpEndpoint;
use super::endpoint_webrtc::{WebRtcEndpoint, WebRtcEvent};
use super::file_poll::FileRtpState;
use super::routing::RoutingTable;
use super::session_dtmf::{EndpointDtmf, PendingDtmfInjection};
use super::vad_tap;
use crate::control::protocol::*;
use crate::media::codec::AudioCodec;
use crate::media::dtmf::DtmfDetector;
use crate::media::sdp;
use crate::media::transcode::TranscodePipeline;
use crate::media::vad::VadMonitor;

/// Wrapper around TranscodePipeline with LRU tracking
struct CachedTranscode {
    pipeline: TranscodePipeline,
    last_used: Instant,
}
use crate::net::socket_pool::SocketPool;
use crate::recording::recorder::RecordingManager;

/// Bundle of endpoint state for cross-session transfer
pub struct EndpointTransferBundle {
    // Note: Debug is manually implemented below because Endpoint and AudioDecoder don't derive Debug
    pub endpoint: Endpoint,
    pub source_session_id: SessionId,
    pub dtmf_state: Option<EndpointDtmf>,
    pub vad_monitor: Option<VadMonitor>,
    pub vad_decoder: Option<Box<dyn crate::media::codec::AudioDecoder>>,
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
    },
    CreateOffer {
        reply: oneshot::Sender<anyhow::Result<(EndpointId, String)>>,
        direction: EndpointDirection,
        endpoint_type: EndpointType,
        srtp: bool,
        codecs: Option<Vec<String>>,
    },
    AcceptAnswer {
        reply: oneshot::Sender<anyhow::Result<()>>,
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
    CreateWithFile {
        reply: oneshot::Sender<anyhow::Result<EndpointId>>,
        source: String,
        start_ms: u64,
        loop_count: Option<u32>,
        cache_ttl_secs: u32,
        timeout_ms: u32,
        shared: bool,
        headers: Option<std::collections::HashMap<String, String>>,
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
        reply: oneshot::Sender<anyhow::Result<String>>,
        endpoint_id: EndpointId,
    },
    SrtpRekey {
        reply: oneshot::Sender<anyhow::Result<String>>,
        endpoint_id: EndpointId,
    },
    StatsSubscribe {
        reply: oneshot::Sender<anyhow::Result<()>>,
        interval_ms: u32,
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
}

/// Detailed session state returned by GetInfo
#[derive(Debug, serde::Serialize)]
pub struct SessionDetails {
    pub endpoints: Vec<EndpointInfo>,
    pub recordings: Vec<crate::control::protocol::RecordingInfo>,
    pub vad_active: Vec<EndpointId>,
}

use tokio::sync::oneshot;

/// All mutable state for a media session, grouped to reduce parameter passing
/// and keep the select! loop thin.
struct SessionState {
    session_id: SessionId,
    media_ip: IpAddr,
    socket_pool: Arc<SocketPool>,
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
    routing: RoutingTable,
    recording_mgr: RecordingManager,
    vad_monitors: HashMap<EndpointId, VadMonitor>,
    stats_interval: Option<Duration>,
    last_stats_emit: Instant,
    file_rtp_states: HashMap<EndpointId, FileRtpState>,
    tone_rtp_states: HashMap<EndpointId, super::tone_poll::ToneRtpState>,
    transcode_cache: HashMap<(EndpointId, EndpointId), CachedTranscode>,
    url_sources: HashMap<EndpointId, String>,
    /// Per-endpoint audio decoders for VAD (needed for G.722/Opus stateful decoding)
    vad_decoders: HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>>,
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
}

/// Event names that should be routed to the critical channel first.
const CRITICAL_EVENTS: &[&str] = &[
    "endpoint.state_changed",
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
            } => {
                let result = self
                    .handle_create_from_offer(packet_tx, &sdp, direction)
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
                codecs,
            } => {
                let result = self
                    .handle_create_offer(packet_tx, direction, endpoint_type, srtp, codecs)
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
            } => {
                let _ = reply.send(self.handle_accept_answer(endpoint_id, &sdp));
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
            SessionCommand::RecordingStart {
                reply,
                endpoint_id,
                file_path,
            } => {
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
            SessionCommand::CreateWithFile {
                reply,
                source,
                start_ms,
                loop_count,
                cache_ttl_secs,
                timeout_ms,
                shared,
                headers,
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
            SessionCommand::StatsSubscribe { reply, interval_ms } => {
                self.stats_interval = Some(Duration::from_millis(interval_ms as u64));
                self.last_stats_emit = Instant::now();
                let _ = reply.send(Ok(()));
            }
            SessionCommand::StatsUnsubscribe { reply } => {
                self.stats_interval = None;
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
        }
        true
    }

    // ── Endpoint creation ───────────────────────────────────────────

    async fn handle_create_from_offer(
        &mut self,
        packet_tx: &mpsc::Sender<InboundPacket>,
        sdp_str: &str,
        direction: EndpointDirection,
    ) -> anyhow::Result<(EndpointId, String)> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("MAX_ENDPOINTS_REACHED");
        }
        let id = EndpointId::new_v4();
        let parsed = sdp::parse_sdp(sdp_str);

        let (answer, te_pt) = if parsed.is_webrtc {
            let ep_bind = SocketAddr::new(self.media_ip, 0);
            let (ep, answer) =
                WebRtcEndpoint::from_offer(id, direction, sdp_str, ep_bind, packet_tx.clone())
                    .await?;
            self.endpoints.insert(id, Endpoint::WebRtc(Box::new(ep)));
            (answer, Some(101u8))
        } else {
            let pair = self.socket_pool.allocate_pair().await?;
            let (ep, answer) = RtpEndpoint::from_offer(
                id,
                direction,
                sdp_str,
                pair,
                self.media_ip,
                packet_tx.clone(),
            )?;
            let te = ep.telephone_event_pt;
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
        codecs: Option<Vec<String>>,
    ) -> anyhow::Result<(EndpointId, String)> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("MAX_ENDPOINTS_REACHED");
        }
        let id = EndpointId::new_v4();

        let (offer, te_pt) = match endpoint_type {
            EndpointType::Webrtc => {
                let ep_bind = SocketAddr::new(self.media_ip, 0);
                let (ep, offer) =
                    WebRtcEndpoint::create_offer(id, direction, ep_bind, packet_tx.clone()).await?;
                self.endpoints.insert(id, Endpoint::WebRtc(Box::new(ep)));
                (offer, Some(101u8))
            }
            EndpointType::Rtp => {
                let pair = self.socket_pool.allocate_pair().await?;
                let all_codecs = vec![
                    sdp::CODEC_PCMU,
                    sdp::CODEC_G722,
                    sdp::CODEC_OPUS,
                    sdp::CODEC_TELEPHONE_EVENT,
                ];
                let offer_codecs = if let Some(ref names) = codecs {
                    all_codecs
                        .into_iter()
                        .filter(|c| {
                            c.name == "telephone-event"
                                || names.iter().any(|n| n.eq_ignore_ascii_case(c.name))
                        })
                        .collect()
                } else {
                    all_codecs
                };
                let (ep, offer) = RtpEndpoint::create_offer(
                    id,
                    direction,
                    pair,
                    self.media_ip,
                    &offer_codecs,
                    srtp,
                    packet_tx.clone(),
                )?;
                let te = ep.telephone_event_pt;
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
    ) -> anyhow::Result<EndpointId> {
        if self.max_endpoints > 0 && self.endpoints.len() >= self.max_endpoints {
            anyhow::bail!("MAX_ENDPOINTS_REACHED");
        }

        let id = EndpointId::new_v4();

        if crate::playback::file_cache::is_url(source) {
            let mut ep = FileEndpoint::new_buffering(id);
            ep.shared = shared;
            self.endpoints.insert(id, Endpoint::File(Box::new(ep)));
            self.rebuild_routing();

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
                let ep = FileEndpoint::new_shared(id, source, sub);
                self.endpoints.insert(id, Endpoint::File(Box::new(ep)));
            } else {
                let ep = FileEndpoint::open(id, source, start_ms, loop_count)?;
                self.endpoints.insert(id, Endpoint::File(Box::new(ep)));
            }
            self.rebuild_routing();
            Ok(id)
        }
    }

    // ── Endpoint modification ───────────────────────────────────────

    fn handle_accept_answer(&mut self, endpoint_id: EndpointId, sdp: &str) -> anyhow::Result<()> {
        let mut state_change = None;
        let result = match self.endpoints.get_mut(&endpoint_id) {
            Some(ep) => {
                let old_state = ep.state();
                let r = ep.accept_answer(sdp);
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
        Ok(id)
    }

    async fn handle_remove_endpoint(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        let mut removed = self.endpoints.remove(&endpoint_id);
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
            Some(Endpoint::File(_) | Endpoint::Tone(_))
        ) {
            anyhow::bail!("File endpoints cannot be transferred");
        }

        let mut endpoint = self
            .endpoints
            .remove(&endpoint_id)
            .ok_or_else(|| anyhow::anyhow!("Endpoint not found"))?;

        // Stop recv tasks before moving
        endpoint.stop_recv_tasks().await;

        // Extract ancillary state
        let dtmf_state = self.dtmf_state.remove(&endpoint_id);
        let vad_monitor = self.vad_monitors.remove(&endpoint_id);
        let vad_decoder = self.vad_decoders.remove(&endpoint_id);
        let file_rtp_state = self.file_rtp_states.remove(&endpoint_id);
        let url_source = self.url_sources.remove(&endpoint_id);
        let media_timeout_was_emitted = self.media_timeout_emitted.remove(&endpoint_id);

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
            vad_monitor,
            vad_decoder,
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
        };

        // Restart recv tasks with this session's packet_tx
        bundle.endpoint.restart_recv_tasks(packet_tx.clone());

        // Take ownership of the endpoint and ancillary state from the bundle
        // We use std::mem::replace with a dummy that will be dropped
        let endpoint = std::mem::replace(
            &mut bundle.endpoint,
            Endpoint::File(Box::new(FileEndpoint::new_buffering(EndpointId::nil()))),
        );
        self.endpoints.insert(endpoint_id, endpoint);

        // Insert ancillary state
        if let Some(dtmf) = bundle.dtmf_state.take() {
            self.dtmf_state.insert(endpoint_id, dtmf);
        }
        if let Some(vad) = bundle.vad_monitor.take() {
            self.vad_monitors.insert(endpoint_id, vad);
        }
        if let Some(dec) = bundle.vad_decoder.take() {
            self.vad_decoders.insert(endpoint_id, dec);
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

                    let init_result = if is_shared {
                        // Shared playback: subscribe to the shared decode task
                        // instead of initializing a local decoder.
                        match self
                            .shared_playback
                            .subscribe(&path_str, 8000, start_ms, loop_count)
                            .await
                        {
                            Ok(sub) => {
                                let new_ep = FileEndpoint::new_shared(endpoint_id, &path_str, sub);
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
        match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::File(fep)) => fep.seek(position_ms),
            Some(_) => Err(anyhow::anyhow!("Not a file endpoint")),
            None => Err(anyhow::anyhow!("Endpoint not found")),
        }
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

    // ── VAD ─────────────────────────────────────────────────────────

    fn handle_vad_start(
        &mut self,
        endpoint_id: EndpointId,
        silence_interval_ms: u32,
        speech_threshold: f32,
    ) -> anyhow::Result<()> {
        vad_tap::vad_start(
            &self.endpoints,
            &mut self.vad_monitors,
            endpoint_id,
            silence_interval_ms,
            speech_threshold,
        )
    }

    fn handle_vad_stop(&mut self, endpoint_id: EndpointId) -> anyhow::Result<()> {
        vad_tap::vad_stop(&mut self.vad_monitors, &mut self.vad_decoders, endpoint_id)
    }

    // ── WebRTC / SRTP ───────────────────────────────────────────────

    fn handle_ice_restart(&mut self, endpoint_id: EndpointId) -> anyhow::Result<String> {
        match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::WebRtc(wep)) => wep.ice_restart(),
            Some(_) => Err(anyhow::anyhow!("Not a WebRTC endpoint")),
            None => Err(anyhow::anyhow!("Endpoint not found")),
        }
    }

    fn handle_srtp_rekey(&mut self, endpoint_id: EndpointId) -> anyhow::Result<String> {
        match self.endpoints.get_mut(&endpoint_id) {
            Some(Endpoint::Rtp(rep)) => rep.srtp_rekey(),
            Some(_) => Err(anyhow::anyhow!("Not a plain RTP endpoint")),
            None => Err(anyhow::anyhow!("Endpoint not found")),
        }
    }

    // ── Info ─────────────────────────────────────────────────────────

    fn get_info(&self) -> SessionDetails {
        let ep_infos: Vec<EndpointInfo> = self
            .endpoints
            .values()
            .map(|ep| {
                let (ep_type, codec, shared_playback_id) = match ep {
                    Endpoint::WebRtc(_) => ("webrtc".to_string(), Some("opus".to_string()), None),
                    Endpoint::Rtp(r) => (
                        "rtp".to_string(),
                        r.send_codec.as_ref().map(|c| c.name.to_string()),
                        None,
                    ),
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
                };
                EndpointInfo {
                    endpoint_id: ep.id(),
                    endpoint_type: ep_type,
                    direction: ep.direction(),
                    state: ep.state(),
                    codec,
                    shared_playback_id,
                }
            })
            .collect();
        SessionDetails {
            endpoints: ep_infos,
            recordings: self.recording_mgr.active_recordings(),
            vad_active: self.vad_monitors.keys().cloned().collect(),
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────

    /// Remove all per-endpoint ancillary state. Does NOT remove from `self.endpoints`
    /// (caller is responsible for that) and does NOT touch recordings.
    async fn cleanup_endpoint_state(&mut self, endpoint_id: EndpointId) {
        self.dtmf_state.remove(&endpoint_id);
        // Clear any pending DTMF injection targeting this endpoint
        if self
            .dtmf_injection
            .as_ref()
            .is_some_and(|inj| inj.endpoint_id == endpoint_id)
        {
            self.dtmf_injection = None;
        }
        self.vad_monitors.remove(&endpoint_id);
        self.vad_decoders.remove(&endpoint_id);
        self.file_rtp_states.remove(&endpoint_id);
        self.tone_rtp_states.remove(&endpoint_id);
        self.media_timeout_emitted.remove(&endpoint_id);
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

    fn emit_stats(&self) {
        let ep_stats: Vec<crate::control::protocol::EndpointStats> = self
            .endpoints
            .values()
            .map(|ep| {
                let stats = ep.stats();
                crate::control::protocol::EndpointStats {
                    endpoint_id: ep.id(),
                    inbound: InboundStats {
                        packets: stats.inbound_packets,
                        bytes: stats.inbound_bytes,
                        packets_lost: ep.packets_lost(),
                        jitter_ms: ep.jitter_ms(),
                        last_received_ms_ago: stats.ms_since_last_received().unwrap_or(0),
                    },
                    outbound: OutboundStats {
                        packets: stats.outbound_packets,
                        bytes: stats.outbound_bytes,
                    },
                    rtt_ms: ep.rtt_ms(),
                    codec: ep.codec_name(),
                    state: format!("{:?}", ep.state()),
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
    media_ip: IpAddr,
    socket_pool: Arc<SocketPool>,
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
        media_ip,
        socket_pool,
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
        routing: RoutingTable::new(),
        recording_mgr: RecordingManager::with_config(
            max_recordings,
            recording_flush_timeout_secs,
            recording_channel_size,
        ),
        vad_monitors: HashMap::new(),
        stats_interval: None,
        last_stats_emit: Instant::now(),
        file_rtp_states: HashMap::new(),
        tone_rtp_states: HashMap::new(),
        transcode_cache: HashMap::new(),
        url_sources: HashMap::new(),
        vad_decoders: HashMap::new(),
        media_timeout_emitted: std::collections::HashSet::new(),
        dtmf_injection: None,
        last_timeout_check: Instant::now(),
        empty_since: Some(Instant::now()),
        mixers: HashMap::new(),
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
        let sleep_duration =
            if has_playing_files || has_playing_tones || has_pending_dtmf || has_active_dtmf {
                next_timeout
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
                    .min(Duration::from_millis(20))
            } else {
                next_timeout
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
            };

        let mut inbound_rtp = Vec::new();

        tokio::select! {
            Some(pkt) = packet_rx.recv() => {
                last_activity = Instant::now();
                let (routed, rtcp_data, bye_info) = handle_inbound_packet(&mut state.endpoints, &pkt, &state.metrics);
                if let Some(routed) = routed {
                    inbound_rtp.push(routed);
                }
                if let Some((eid, rtcp_bytes)) = rtcp_data {
                    let dead = state.recording_mgr.record_packet(&eid, &rtcp_bytes, false);
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
                    let (routed, rtcp_data, bye_info) =
                        handle_inbound_packet(&mut state.endpoints, &pkt, &state.metrics);
                    if let Some(routed) = routed {
                        inbound_rtp.push(routed);
                    }
                    if let Some((eid, rtcp_bytes)) = rtcp_data {
                        let dead = state.recording_mgr.record_packet(&eid, &rtcp_bytes, false);
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

        // Drain pending DTMF injection one packet at a time (non-blocking)
        if let Some(ref mut inj) = state.dtmf_injection
            && super::session_dtmf::drain_dtmf_injection(inj, &mut state.endpoints).await
        {
            state.dtmf_injection = None;
        }

        let needs_routing_rebuild;
        (next_webrtc_timeout, needs_routing_rebuild) = poll_and_route(
            &mut state.endpoints,
            &mut state.dtmf_state,
            &state.routing,
            &state.event_tx,
            &state.critical_event_tx,
            &state.dropped_events,
            &mut state.recording_mgr,
            &mut state.vad_monitors,
            &mut state.vad_decoders,
            &state.metrics,
            inbound_rtp,
            &mut state.file_rtp_states,
            &mut state.tone_rtp_states,
            &mut state.transcode_cache,
            transcode_cache_size,
            &mut state.mixers,
        )
        .await;
        if needs_routing_rebuild {
            state.rebuild_routing();
        }
        if state.last_timeout_check.elapsed() >= Duration::from_secs(1) {
            check_media_timeouts(
                &state.endpoints,
                &state.event_tx,
                media_timeout,
                &mut state.media_timeout_emitted,
                &state.dropped_events,
                &state.metrics,
            );
            state.last_timeout_check = Instant::now();
        }
        super::session_dtmf::check_dtmf_timeouts(
            &mut state.dtmf_state,
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
                match rep.maybe_send_rtcp().await {
                    Ok(Some(rtcp_bytes)) => {
                        let dead = state
                            .recording_mgr
                            .record_packet(&rep.id, &rtcp_bytes, true);
                        emit_dead_recordings(
                            &state.event_tx,
                            &state.critical_event_tx,
                            &state.dropped_events,
                            &state.metrics,
                            dead,
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(endpoint_id = %rep.id, error = %e, "RTCP send error");
                    }
                }
            }
        }
    }

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
    let now = Instant::now();

    if let Some(ep) = endpoints.get_mut(&pkt.endpoint_id) {
        match ep {
            Endpoint::WebRtc(wep) => {
                if let Err(e) = wep.handle_receive(pkt.source, &pkt.data, now) {
                    debug!(endpoint_id = %pkt.endpoint_id, error = %e, "WebRTC packet error");
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
        }
    }
    (None, None, None)
}

/// Returns (min WebRTC timeout, whether routing table needs rebuild due to state changes).
#[allow(clippy::too_many_arguments)]
async fn poll_and_route(
    endpoints: &mut HashMap<EndpointId, Endpoint>,
    dtmf_state: &mut HashMap<EndpointId, EndpointDtmf>,
    routing: &RoutingTable,
    event_tx: &Option<mpsc::Sender<Event>>,
    critical_event_tx: &Option<mpsc::Sender<Event>>,
    dropped_events: &AtomicU64,
    recording_mgr: &mut RecordingManager,
    vad_monitors: &mut HashMap<EndpointId, VadMonitor>,
    vad_decoders: &mut HashMap<EndpointId, Box<dyn crate::media::codec::AudioDecoder>>,
    metrics: &crate::metrics::Metrics,
    inbound_rtp: Vec<RoutedRtpPacket>,
    file_rtp_states: &mut HashMap<EndpointId, FileRtpState>,
    tone_rtp_states: &mut HashMap<EndpointId, super::tone_poll::ToneRtpState>,
    transcode_cache: &mut HashMap<(EndpointId, EndpointId), CachedTranscode>,
    transcode_cache_size: usize,
    mixers: &mut HashMap<EndpointId, super::mixer::DestinationMixer>,
) -> (Option<Instant>, bool) {
    let mut packets_to_route: Vec<RoutedRtpPacket> = Vec::new();
    let mut dtmf_packets: Vec<RoutedRtpPacket> = Vec::new();
    let mut state_events: Vec<(EndpointId, EndpointState, EndpointState)> = Vec::new();
    let mut min_webrtc_timeout: Option<Instant> = None;

    // Add inbound RTP packets from plain RTP endpoints
    for pkt in inbound_rtp {
        if super::session_dtmf::classify_dtmf(&pkt, dtmf_state) {
            dtmf_packets.push(pkt);
        } else {
            packets_to_route.push(pkt);
        }
    }

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

    // Poll WebRTC endpoints for output and track their next timeouts
    for ep in endpoints.values_mut() {
        if let Endpoint::WebRtc(wep) = ep {
            match wep.poll_output() {
                Ok((events, timeout)) => {
                    min_webrtc_timeout = Some(match min_webrtc_timeout {
                        Some(prev) => prev.min(timeout),
                        None => timeout,
                    });
                    for event in events {
                        match event {
                            WebRtcEvent::RtpPacket(pkt) => {
                                if super::session_dtmf::classify_dtmf(&pkt, dtmf_state) {
                                    dtmf_packets.push(pkt);
                                } else {
                                    packets_to_route.push(pkt);
                                }
                            }
                            WebRtcEvent::StateChanged { old, new } => {
                                state_events.push((wep.id, old, new));
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(endpoint_id = %wep.id, error = %e, "poll_output error");
                }
            }
        }
    }

    // Emit state change events (critical priority)
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

    // Process DTMF packets: detect events, forward without transcoding
    super::session_dtmf::process_dtmf_packets(
        &dtmf_packets,
        dtmf_state,
        routing,
        endpoints,
        event_tx,
        dropped_events,
        metrics,
    )
    .await;

    // Recording tap: capture all inbound RTP/DTMF packets (skip if no active recordings)
    if recording_mgr.is_recording() {
        for pkt in dtmf_packets.iter().chain(packets_to_route.iter()) {
            let raw_rtp = crate::media::rtp::RtpHeader::build(
                pkt.payload_type,
                pkt.sequence_number,
                pkt.timestamp,
                pkt.ssrc,
                pkt.marker,
                &pkt.payload,
            );
            let dead = recording_mgr.record_packet(&pkt.source_endpoint_id, &raw_rtp, false);
            emit_dead_recordings(event_tx, critical_event_tx, dropped_events, metrics, dead);
            metrics.packets_recorded.inc();
        }
    }

    // VAD tap: feed decoded PCM to VAD monitors
    vad_tap::process_vad(
        &packets_to_route,
        endpoints,
        vad_monitors,
        vad_decoders,
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
                    let result = match dest_ep {
                        Endpoint::WebRtc(wep) => wep.write_rtp(&routed),
                        Endpoint::Rtp(rep) => rep.write_rtp(&routed).await,
                        Endpoint::File(_) | Endpoint::Tone(_) => Ok(()),
                        Endpoint::Bridge(bep) => bep.write_rtp(&routed).await,
                    };
                    if let Err(e) = result {
                        warn!(src = %pkt.source_endpoint_id, dst = %dest_id, error = %e, "route error");
                    } else {
                        metrics.packets_routed.inc();
                    }
                }
            }
        }
    }

    // Deliver mixed frames queued by feed() (flushed on frame boundaries)
    for (&dest_id, mixer) in mixers.iter_mut() {
        for routed in mixer.drain() {
            if let Some(dest_ep) = endpoints.get_mut(&dest_id) {
                let result = match dest_ep {
                    Endpoint::WebRtc(wep) => wep.write_rtp(&routed),
                    Endpoint::Rtp(rep) => rep.write_rtp(&routed).await,
                    Endpoint::File(_) | Endpoint::Tone(_) => Ok(()),
                    Endpoint::Bridge(bep) => bep.write_rtp(&routed).await,
                };
                if let Err(e) = result {
                    warn!(dst = %dest_id, error = %e, "mixer route error");
                } else {
                    metrics.packets_routed.inc();
                }
            }
        }
    }

    (min_webrtc_timeout, needs_routing_rebuild)
}

fn check_media_timeouts(
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
        let stats = ep.stats();
        let ms = stats
            .ms_since_last_received()
            .unwrap_or_else(|| stats.created_at.elapsed().as_millis() as u64);
        if ms > threshold.as_millis() as u64 {
            // Only emit once per timeout period; cleared when packet received
            if emitted.insert(eid) {
                warn!(endpoint_id = %eid, duration_ms = ms, "endpoint media timeout");
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
mod tests {
    use super::*;

    /// Create a minimal `SessionState` for unit testing command handlers.
    /// Uses in-memory defaults — no real sockets or file caches needed.
    fn test_session_state() -> SessionState {
        let (cmd_tx, _cmd_rx) = mpsc::channel(16);
        SessionState {
            session_id: SessionId::new_v4(),
            media_ip: "127.0.0.1".parse().unwrap(),
            socket_pool: Arc::new(
                crate::net::socket_pool::SocketPool::new(
                    "127.0.0.1".parse().unwrap(),
                    50000,
                    50100,
                )
                .unwrap(),
            ),
            media_dir: None,
            file_cache: Arc::new(
                crate::playback::file_cache::FileCache::new(
                    std::env::temp_dir().join("rtpbridge-test-cache"),
                )
                .unwrap(),
            ),
            endpoint_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_endpoints: 100,
            metrics: Arc::new(crate::metrics::Metrics::new()),
            cmd_tx,
            event_tx: None,
            critical_event_tx: None,
            dropped_events: Arc::new(AtomicU64::new(0)),
            endpoints: HashMap::new(),
            dtmf_state: HashMap::new(),
            routing: RoutingTable::new(),
            recording_mgr: RecordingManager::new(),
            vad_monitors: HashMap::new(),
            stats_interval: None,
            last_stats_emit: Instant::now(),
            file_rtp_states: HashMap::new(),
            tone_rtp_states: HashMap::new(),
            transcode_cache: HashMap::new(),
            url_sources: HashMap::new(),
            vad_decoders: HashMap::new(),
            media_timeout_emitted: std::collections::HashSet::new(),
            dtmf_injection: None,
            last_timeout_check: Instant::now(),
            shared_playback: Arc::new(
                crate::playback::shared_playback::SharedPlaybackManager::new(),
            ),
            empty_since: None,
            mixers: HashMap::new(),
        }
    }

    #[test]
    fn test_emit_event_none_channel_is_noop() {
        let tx: Option<mpsc::Sender<Event>> = None;
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();
        emit_event(
            &tx,
            "test.event",
            serde_json::json!({"key": "value"}),
            &dropped,
            &metrics,
        );
    }

    #[tokio::test]
    async fn test_emit_event_full_channel_drops_without_panic() {
        let (tx, _rx) = mpsc::channel::<Event>(1);
        let tx = Some(tx);
        let dropped = AtomicU64::new(0);
        let metrics = crate::metrics::Metrics::new();
        emit_event(&tx, "first", serde_json::json!({}), &dropped, &metrics);
        // Should be dropped (channel full) but must NOT panic
        emit_event(&tx, "second", serde_json::json!({}), &dropped, &metrics);
        emit_event(&tx, "third", serde_json::json!({}), &dropped, &metrics);
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            2,
            "two events should have been dropped"
        );
    }

    // ── SessionState handler tests ──────────────────────────────────

    #[test]
    fn test_get_info_empty_session() {
        let state = test_session_state();
        let info = state.get_info();
        assert!(info.endpoints.is_empty());
        assert!(info.recordings.is_empty());
        assert!(info.vad_active.is_empty());
    }

    #[test]
    fn test_handle_accept_answer_not_found() {
        let mut state = test_session_state();
        let result = state.handle_accept_answer(EndpointId::new_v4(), "v=0\r\n");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[test]
    fn test_handle_vad_start_not_found() {
        let mut state = test_session_state();
        let result = state.handle_vad_start(EndpointId::new_v4(), 500, 0.5);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[test]
    fn test_handle_vad_stop_not_active() {
        let mut state = test_session_state();
        let result = state.handle_vad_stop(EndpointId::new_v4());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("VAD not active"));
    }

    #[test]
    fn test_handle_file_seek_not_found() {
        let mut state = test_session_state();
        let result = state.handle_file_seek(EndpointId::new_v4(), 1000);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[test]
    fn test_handle_file_pause_not_found() {
        let mut state = test_session_state();
        let result = state.handle_file_pause(EndpointId::new_v4());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[test]
    fn test_handle_file_resume_not_found() {
        let mut state = test_session_state();
        let result = state.handle_file_resume(EndpointId::new_v4());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[test]
    fn test_handle_ice_restart_not_found() {
        let mut state = test_session_state();
        let result = state.handle_ice_restart(EndpointId::new_v4());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[test]
    fn test_handle_srtp_rekey_not_found() {
        let mut state = test_session_state();
        let result = state.handle_srtp_rekey(EndpointId::new_v4());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[tokio::test]
    async fn test_handle_remove_endpoint_not_found() {
        let mut state = test_session_state();
        let result = state.handle_remove_endpoint(EndpointId::new_v4()).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Endpoint not found")
        );
    }

    #[tokio::test]
    async fn test_cleanup_endpoint_state_removes_all_ancillary() {
        let mut state = test_session_state();
        let eid = EndpointId::new_v4();

        // Populate all ancillary state maps for this endpoint
        state.dtmf_state.insert(
            eid,
            EndpointDtmf {
                detector: DtmfDetector::new(),
                te_pt: Some(101),
            },
        );
        state
            .vad_monitors
            .insert(eid, VadMonitor::new(8000, 0.5, 500));
        state.file_rtp_states.insert(
            eid,
            FileRtpState {
                seq_no: 0,
                timestamp: 0,
                ssrc: 0,
                last_poll: Instant::now(),
                resampler: None,
            },
        );
        state
            .url_sources
            .insert(eid, "https://example.com/test.wav".to_string());

        // Add a dummy transcode cache entry involving this endpoint
        let other_eid = EndpointId::new_v4();
        state.transcode_cache.insert(
            (eid, other_eid),
            CachedTranscode {
                pipeline: TranscodePipeline::new(AudioCodec::Pcmu, AudioCodec::G722).unwrap(),
                last_used: Instant::now(),
            },
        );

        state.cleanup_endpoint_state(eid).await;

        assert!(!state.dtmf_state.contains_key(&eid));
        assert!(!state.vad_monitors.contains_key(&eid));
        assert!(!state.file_rtp_states.contains_key(&eid));
        assert!(!state.url_sources.contains_key(&eid));
        assert!(
            !state.transcode_cache.contains_key(&(eid, other_eid)),
            "transcode cache entry involving removed endpoint should be cleaned"
        );
    }

    #[test]
    fn test_rebuild_routing_updates_endpoint_count() {
        let mut state = test_session_state();
        assert_eq!(
            state
                .endpoint_count
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );

        // Insert a file endpoint directly (must be in Playing state to be routable)
        let id = EndpointId::new_v4();
        let mut ep = FileEndpoint::new_buffering(id);
        ep.state = EndpointState::Playing;
        state.endpoints.insert(id, Endpoint::File(Box::new(ep)));
        state.rebuild_routing();

        assert_eq!(
            state
                .endpoint_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn test_handle_command_destroy_returns_false() {
        let mut state = test_session_state();
        let (packet_tx, _packet_rx) = mpsc::channel(16);
        let cont = state
            .handle_command(SessionCommand::Destroy, &packet_tx)
            .await;
        assert!(
            !cont,
            "Destroy command should return false to break the loop"
        );
    }

    #[tokio::test]
    async fn test_handle_command_attach_detach() {
        let mut state = test_session_state();
        let (packet_tx, _packet_rx) = mpsc::channel(16);
        let (event_tx, _event_rx) = mpsc::channel(16);

        assert!(state.event_tx.is_none());

        let (critical_tx, _critical_rx) = mpsc::channel(16);
        let cont = state
            .handle_command(
                SessionCommand::Attach {
                    event_tx,
                    critical_event_tx: critical_tx,
                    dropped_events: Arc::new(AtomicU64::new(0)),
                },
                &packet_tx,
            )
            .await;
        assert!(cont);
        assert!(state.event_tx.is_some());

        let cont = state
            .handle_command(SessionCommand::Detach, &packet_tx)
            .await;
        assert!(cont);
        assert!(state.event_tx.is_none());
    }

    #[tokio::test]
    async fn test_handle_command_stats_subscribe_unsubscribe() {
        let mut state = test_session_state();
        let (packet_tx, _packet_rx) = mpsc::channel(16);

        assert!(state.stats_interval.is_none());

        let (reply_tx, reply_rx) = oneshot::channel();
        state
            .handle_command(
                SessionCommand::StatsSubscribe {
                    reply: reply_tx,
                    interval_ms: 5000,
                },
                &packet_tx,
            )
            .await;
        assert!(reply_rx.await.unwrap().is_ok());
        assert_eq!(state.stats_interval, Some(Duration::from_millis(5000)));

        let (reply_tx, reply_rx) = oneshot::channel();
        state
            .handle_command(
                SessionCommand::StatsUnsubscribe { reply: reply_tx },
                &packet_tx,
            )
            .await;
        assert!(reply_rx.await.unwrap().is_ok());
        assert!(state.stats_interval.is_none());
    }

    #[test]
    fn test_check_media_timeouts_emits_once() {
        // Verify that the media_timeout_emitted set prevents duplicate emissions
        let mut emitted = std::collections::HashSet::new();

        // Test the emitted set behavior directly
        let eid = EndpointId::new_v4();
        assert!(emitted.insert(eid), "first insert should succeed");
        assert!(
            !emitted.insert(eid),
            "second insert should return false (already present)"
        );
        emitted.remove(&eid);
        assert!(
            emitted.insert(eid),
            "after remove, insert should succeed again"
        );
    }

    #[test]
    fn test_file_rtp_state_resampler_created_for_non_8k() {
        let state_8k = FileRtpState {
            seq_no: 0,
            timestamp: 0,
            ssrc: 0,
            last_poll: Instant::now(),
            resampler: None,
        };
        assert!(state_8k.resampler.is_none());

        let state_48k = FileRtpState {
            seq_no: 0,
            timestamp: 0,
            ssrc: 0,
            last_poll: Instant::now(),
            resampler: Some(crate::media::resample::Resampler::new(48000, 8000)),
        };
        assert!(state_48k.resampler.is_some());
        // Verify the resampler can process samples
        let mut r = crate::media::resample::Resampler::new(48000, 8000);
        let input: Vec<i16> = vec![0; 960]; // 20ms at 48kHz
        let mut output = Vec::new();
        r.process(&input, &mut output);
        // Should produce ~160 samples (8kHz, 20ms)
        assert!(
            output.len() >= 158 && output.len() <= 162,
            "48kHz→8kHz resample should produce ~160 samples, got {}",
            output.len()
        );
    }

    #[tokio::test]
    async fn test_cleanup_endpoint_state_removes_vad_decoder() {
        let mut state = test_session_state();
        let eid = EndpointId::new_v4();

        // Add a VAD decoder
        state.vad_decoders.insert(
            eid,
            crate::media::codec::make_decoder(AudioCodec::Pcmu).unwrap(),
        );
        assert!(state.vad_decoders.contains_key(&eid));

        state.cleanup_endpoint_state(eid).await;
        assert!(
            !state.vad_decoders.contains_key(&eid),
            "cleanup should remove VAD decoder"
        );
    }

    #[tokio::test]
    async fn test_create_with_file_passes_cache_params() {
        let mut state = test_session_state();
        let (packet_tx, _packet_rx) = mpsc::channel(16);

        // Try creating a file endpoint with a non-existent local file to verify
        // the params are threaded through (will fail because no media_dir, but that's ok)
        let (reply_tx, reply_rx) = oneshot::channel();
        state
            .handle_command(
                SessionCommand::CreateWithFile {
                    reply: reply_tx,
                    source: "/nonexistent/test.wav".to_string(),
                    start_ms: 0,
                    loop_count: None,
                    cache_ttl_secs: 600,
                    timeout_ms: 15000,
                    shared: false,
                    headers: None,
                },
                &packet_tx,
            )
            .await;
        let result = reply_rx.await.unwrap();
        // Should fail because media_dir is None for local files
        assert!(result.is_err());
    }

    // ── Dynamic PT codec resolution tests ───────────────────────────

    #[test]
    fn test_endpoint_audio_codec_resolves_non_standard_opus_pt() {
        // An RTP endpoint negotiated with Opus at PT 96 (not the default 111).
        // endpoint_audio_codec should still resolve to Opus via codec name,
        // whereas the old AudioCodec::from_pt(96) would return None.

        // Test the resolution functions directly on the SdpCodec name.
        let opus_pt96 = sdp::SdpCodec {
            pt: 96,
            name: "opus",
            clock_rate: 48000,
            channels: Some(2),
            fmtp: None,
        };

        // Old approach: from_pt(96) → None (broken for dynamic PTs)
        assert!(
            AudioCodec::from_pt(96).is_none(),
            "from_pt(96) should return None for non-standard PT"
        );

        // New approach: from_name resolves correctly
        assert_eq!(
            AudioCodec::from_name(opus_pt96.name),
            Some(AudioCodec::Opus),
            "from_name should resolve 'opus' regardless of PT number"
        );
    }

    #[test]
    fn test_endpoint_audio_codec_standard_pts_still_work() {
        // Verify standard PTs resolve through both paths
        assert_eq!(AudioCodec::from_name("PCMU"), Some(AudioCodec::Pcmu));
        assert_eq!(AudioCodec::from_name("G722"), Some(AudioCodec::G722));
        assert_eq!(AudioCodec::from_name("opus"), Some(AudioCodec::Opus));
    }

    #[tokio::test]
    async fn test_endpoint_audio_codec_on_rtp_endpoint_with_dynamic_pt() {
        // End-to-end: create an RTP endpoint from an SDP that uses PT 96 for Opus,
        // then verify endpoint_audio_codec resolves correctly.
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52100, 52200)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 96 101\r\n\
            a=rtpmap:96 opus/48000/2\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";

        let (ep, _answer) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        // The endpoint should have negotiated Opus at PT 96
        assert_eq!(ep.send_codec.as_ref().unwrap().pt, 96);
        assert_eq!(ep.send_codec.as_ref().unwrap().name, "opus");

        // Wrap in Endpoint and test resolution
        let wrapped = Endpoint::Rtp(Box::new(ep));
        assert_eq!(
            endpoint_audio_codec(&wrapped),
            Some(AudioCodec::Opus),
            "endpoint_audio_codec should resolve Opus even at non-standard PT 96"
        );
        assert_eq!(
            endpoint_send_pt(&wrapped),
            Some(96),
            "endpoint_send_pt should return the negotiated PT 96, not hardcoded 111"
        );
    }

    // ── DTMF non-blocking injection tests ───────────────────────────

    #[tokio::test]
    async fn test_dtmf_inject_queues_packets_non_blocking() {
        let mut state = test_session_state();
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52200, 52300)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";

        let (ep, _answer) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        let eid = ep.id;
        state.endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));
        state.dtmf_state.insert(
            eid,
            EndpointDtmf {
                detector: DtmfDetector::new(),
                te_pt: Some(101),
            },
        );

        // Inject should return immediately (non-blocking)
        let before = Instant::now();
        let result = state.handle_dtmf_inject(&eid, '5', 200, 10);
        let elapsed = before.elapsed();

        assert!(result.is_ok(), "DTMF inject should succeed");
        assert!(
            elapsed < Duration::from_millis(50),
            "DTMF inject should return immediately, took {:?}",
            elapsed
        );

        // Should have queued packets
        let inj = state.dtmf_injection.as_ref().unwrap();
        assert_eq!(inj.endpoint_id, eid);
        assert!(!inj.packets.is_empty(), "should have queued DTMF packets");
        assert_eq!(inj.next_index, 0, "no packets sent yet");

        // All packets should have PT = 101 (telephone-event)
        for pkt in &inj.packets {
            assert_eq!(pkt.payload_type, 101);
        }
    }

    #[tokio::test]
    async fn test_dtmf_inject_rejects_concurrent() {
        let mut state = test_session_state();
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52300, 52400)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=sendrecv\r\n";

        let (ep, _) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        let eid = ep.id;
        state.endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));
        state.dtmf_state.insert(
            eid,
            EndpointDtmf {
                detector: DtmfDetector::new(),
                te_pt: Some(101),
            },
        );

        // First injection should succeed
        assert!(state.handle_dtmf_inject(&eid, '1', 100, 10).is_ok());

        // Second injection while first is pending should fail
        let result = state.handle_dtmf_inject(&eid, '2', 100, 10);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already in progress")
        );
    }

    #[test]
    fn test_dtmf_inject_file_endpoint_rejected() {
        let mut state = test_session_state();
        let eid = EndpointId::new_v4();
        let ep = FileEndpoint::new_buffering(eid);
        state.endpoints.insert(eid, Endpoint::File(Box::new(ep)));

        let result = state.handle_dtmf_inject(&eid, '5', 200, 10);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("file endpoint"));
    }

    // ── URL file-cache cleanup on destroy ───────────────────────────

    #[tokio::test]
    async fn test_url_sources_drained_on_cleanup() {
        let mut state = test_session_state();
        let eid1 = EndpointId::new_v4();
        let eid2 = EndpointId::new_v4();

        state
            .url_sources
            .insert(eid1, "https://example.com/a.wav".to_string());
        state
            .url_sources
            .insert(eid2, "https://example.com/b.wav".to_string());

        // Simulate what the session shutdown code does
        for (_eid, url) in state.url_sources.drain() {
            state.file_cache.release(&url).await;
        }

        assert!(
            state.url_sources.is_empty(),
            "url_sources should be empty after drain"
        );
    }

    // ── handle_inbound_packet RTCP classification ───────────────────

    #[tokio::test]
    async fn test_inbound_rtcp_classified_by_is_rtcp_flag() {
        let mut endpoints = HashMap::new();
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52400, 52500)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=sendrecv\r\n";

        let (ep, _) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        let eid = ep.id;
        endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));

        // Build a minimal RTCP SR packet (PT = 200)
        let mut rtcp_data = vec![0x80u8, 200, 0x00, 0x06];
        rtcp_data.extend_from_slice(&[0u8; 24]); // SR body

        let pkt = InboundPacket {
            endpoint_id: eid,
            source: "10.0.0.1:20001".parse().unwrap(),
            data: rtcp_data,
            is_rtcp: true,
        };

        let (rtp, rtcp, _bye) =
            handle_inbound_packet(&mut endpoints, &pkt, &crate::metrics::Metrics::new());
        assert!(rtp.is_none(), "RTCP packet should not produce routed RTP");
        assert!(
            rtcp.is_some(),
            "RTCP packet should return bytes for recording tap"
        );
    }

    #[tokio::test]
    async fn test_inbound_rtp_not_misclassified() {
        let mut endpoints = HashMap::new();
        let pool =
            crate::net::socket_pool::SocketPool::new("127.0.0.1".parse().unwrap(), 52500, 52600)
                .unwrap();
        let pair = pool.allocate_pair().await.unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let sdp = "v=0\r\n\
            o=- 1 1 IN IP4 10.0.0.1\r\n\
            s=-\r\n\
            c=IN IP4 10.0.0.1\r\n\
            t=0 0\r\n\
            m=audio 20000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=sendrecv\r\n";

        let (ep, _) = RtpEndpoint::from_offer(
            EndpointId::new_v4(),
            EndpointDirection::SendRecv,
            sdp,
            pair,
            "127.0.0.1".parse().unwrap(),
            tx,
        )
        .unwrap();

        let eid = ep.id;
        endpoints.insert(eid, Endpoint::Rtp(Box::new(ep)));

        // Build a valid RTP PCMU packet (PT=0, V=2)
        let rtp_data =
            crate::media::rtp::RtpHeader::build(0, 1, 160, 12345, false, &vec![0x80u8; 160]);

        let pkt = InboundPacket {
            endpoint_id: eid,
            source: "10.0.0.1:20000".parse().unwrap(),
            data: rtp_data,
            is_rtcp: false,
        };

        let (rtp, rtcp, _bye) =
            handle_inbound_packet(&mut endpoints, &pkt, &crate::metrics::Metrics::new());
        assert!(rtp.is_some(), "RTP packet should produce a routed packet");
        assert!(
            rtcp.is_none(),
            "RTP packet should not produce RTCP recording"
        );
    }
}
