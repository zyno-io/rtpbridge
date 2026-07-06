use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::rtp::SeqNo;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

use super::endpoint::{EndpointConfig, InboundPacket, RoutedRtpPacket};
use super::stats::{EndpointStats, RawRecvCounters};
use crate::control::protocol::{
    EndpointDirection, EndpointDirectionUpdate, EndpointId, EndpointState,
};
use crate::media::rtcp::RtcpStats;
use crate::metrics::Metrics;

/// RTP timestamp clock for Opus, the only audio codec we negotiate for WebRTC.
/// Kept in sync with `endpoint_enum::endpoint_rtp_clock_rate` for WebRTC.
const WEBRTC_OPUS_RTP_CLOCK_HZ: u32 = 48_000;

/// Grace window for a per-endpoint UDP recv task to reach its receive loop after
/// being spawned. The task normally starts in well under a millisecond; if it
/// has not signalled liveness within this window the session's liveness sweep
/// flags the never-started variant (the runtime did not schedule/register the
/// socket reader) and increments `webrtc_recv_task_start_timeout`. Checked off
/// the hot path (in the 1 Hz sweep), so it never blocks endpoint creation or the
/// session task. See docs/incident-research/webrtc-recv-task-wedge.md.
const RECV_TASK_START_GRACE: Duration = Duration::from_secs(2);

/// The negotiated audio codec on a WebRTC endpoint, derived from str0m's media
/// line. `name` is one of `opus`/`PCMU`/`PCMA` (str0m has no G.722 audio codec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedCodec {
    pub name: &'static str,
    pub pt: u8,
    pub clock_rate: u32,
    pub channels: u8,
}

/// A WebRTC endpoint backed by str0m
pub struct WebRtcEndpoint {
    pub id: EndpointId,
    pub config: EndpointConfig,
    pub state: EndpointState,
    pub stats: EndpointStats,
    /// Wire-level inbound datagram counters: ALL datagrams the UDP socket
    /// delivers (STUN/ICE, DTLS, RTCP, RTP, junk) before str0m demuxes them.
    /// Shared with the recv task, which increments it; the session task reads
    /// it for stats. Diverging from `stats.inbound_*` (validated media only)
    /// while this climbs means the peer's path is alive but producing no media.
    pub raw_recv: Arc<RawRecvCounters>,
    /// Last ICE connection state str0m reported (None before the first
    /// transition). `Disconnected` is str0m's RFC 7675 consent-freshness
    /// verdict — the canonical "remote path lost" signal. Surfaced in stats
    /// and via `endpoint.ice_state_changed`.
    pub ice_connection_state: Option<IceConnectionState>,
    /// RFC 3550 §A.3/A.8 inbound stats (jitter, loss, sequence tracking).
    /// str0m computes these internally but does not expose them on
    /// MediaIngressStats, so we run our own pass over Event::RtpPacket.
    pub rtcp_stats: RtcpStats,
    /// Most recent RTT to the WebRTC peer, in milliseconds, captured from
    /// str0m's periodic stats (`PeerStats`/egress/ingress `rtt`). str0m owns
    /// RTCP for WebRTC, so — unlike plain RTP — our own `rtcp_stats` never
    /// computes RTT here; this is the value `Endpoint::rtt_ms()` reports for
    /// WebRTC legs.
    pub peer_rtt_ms: Option<f64>,
    pub rtc: Rtc,
    /// One UDP socket per bound address family (IPv4 and/or IPv6), each paired
    /// with its local address. We register one ICE host candidate per socket and
    /// let ICE nominate a pair; inbound datagrams are tagged with the socket they
    /// arrived on, and outbound `Transmit`s are routed by `transmit.source`.
    pub sockets: Vec<(SocketAddr, Arc<UdpSocket>)>,
    /// Primary local address (the first bound socket). Used for stats, logging,
    /// and event payloads; not the sole transport address when dual-stack.
    pub local_addr: SocketAddr,
    /// Last destination str0m emitted a transmit to once ICE was nominated.
    /// Cleared on disconnect.
    pub remote_addr: Option<SocketAddr>,
    /// Mid for the audio media line (set after SDP negotiation)
    pub audio_mid: Option<Mid>,
    /// Destination-owned outbound RTP sequence number. str0m owns the outgoing
    /// SSRC, but in RTP mode it uses the seq/timestamp values we pass to
    /// `write_rtp`; forwarding source seq numbers verbatim would make Chrome see
    /// discontinuities on hold music, mixer, file, or endpoint replacement
    /// source switches.
    outbound_seq_no: SeqNo,
    /// Destination-owned outbound RTP timestamp timeline. Set after the first
    /// successful write_rtp so subsequent packets can advance from a known
    /// reference instead of inheriting whatever the source last sent.
    last_outbound_ts: Option<u32>,
    /// Source endpoint of the last packet we wrote. Used to detect source
    /// changes (mixer<->passthrough, file<->normal, source A<->source B) so we
    /// can preserve the destination's RTP timeline across the switch.
    last_source_id: Option<EndpointId>,
    /// Source's RTP timestamp at the last packet we wrote, matched to
    /// `last_source_id`. Lets us compute a delta within the same source.
    last_source_ts: Option<u32>,
    /// Smoothed packet-duration estimate in RTP timestamp units, learned from
    /// recent same-source deltas. Used as the bump on source changes or
    /// discontinuity clamps. Falls back to 20ms at the WebRTC audio RTP clock.
    learned_step: Option<u32>,
    /// Pending offer (when we created an offer, waiting for answer)
    pub pending_offer: Option<SdpPendingOffer>,
    /// Monotonic generation of the most recently minted server offer.
    /// Incremented on each `ice_restart`. Returned to the caller so a later
    /// `accept_answer` can be tagged with the generation it answers; the
    /// session rejects an answer whose generation no longer matches (a stale
    /// answer for an offer that has since been superseded). The initial offer
    /// is generation 0 and is not verified (no overlap risk before connect).
    pub offer_generation: u64,
    /// Baseline direction this endpoint uses in auto mode.
    auto_direction: EndpointDirection,
    /// When the most-recent negotiation attempt (initial offer/answer, re-offer,
    /// or ICE restart) started. Cleared when str0m reports the endpoint
    /// connected (or disconnected). Used by the connecting-watchdog to detect
    /// negotiations that never reach Connected.
    pub connecting_since: Option<Instant>,
    /// Whether the watchdog has already WARN'd for the current `connecting_since`
    /// period. Reset whenever a new negotiation begins.
    pub connecting_warned: bool,
    /// Whether a packet-input error has already been WARN'd for the current
    /// negotiation attempt. Reset whenever a new negotiation begins so a
    /// re-attempt after errors can produce a fresh WARN.
    pub packet_error_warned: bool,
    /// Handle to the recv task (aborted on drop)
    recv_task: Option<tokio::task::JoinHandle<()>>,
    /// Cancellation token for cooperative recv task shutdown (cloned in start_recv_task, cancelled in drop)
    #[allow(dead_code)]
    cancel_token: CancellationToken,
    /// Shared metrics so the per-endpoint recv task can record its own lifecycle
    /// (started / exited / overflow) without threading metrics through every
    /// transfer/restart path.
    metrics: Arc<Metrics>,
    /// Set true by the recv task the instant it reaches its receive loop, before
    /// its first await. The session liveness sweep reads it: a task still false
    /// past `recv_start_deadline` means the runtime never scheduled/registered
    /// the reader instead of silently dropping all media.
    recv_started: Arc<AtomicBool>,
    /// When the current recv task must have signalled `recv_started` by. Set on
    /// each (re)start; `None` before the first start. The sweep flags a wedge if
    /// the deadline passes with `recv_started` still false.
    recv_start_deadline: Option<Instant>,
    /// Whether the session liveness sweep has already reported (once) that this
    /// endpoint's recv task is wedged — died, or never started — while the
    /// endpoint was still active.
    recv_dead_reported: bool,
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "non-string panic payload"
    }
}

/// What the recv task should do after handling one datagram.
enum RecvAction {
    Continue,
    Stop(&'static str),
}

/// Forward a single received datagram from a WebRTC socket into the session's
/// packet channel, tagging it with the `local` address it arrived on (so the
/// session can give str0m the correct `destination` for the dual-stack case).
/// Shared by all sockets a single recv task multiplexes.
#[allow(clippy::too_many_arguments)]
fn forward_datagram(
    result: std::io::Result<(usize, SocketAddr)>,
    buf: &[u8],
    local: SocketAddr,
    endpoint_id: EndpointId,
    raw_recv: &RawRecvCounters,
    metrics: &Metrics,
    packet_tx: &mpsc::Sender<InboundPacket>,
) -> RecvAction {
    match result {
        Ok((n, source)) => {
            // Wire-level count: every datagram, BEFORE str0m demuxes
            // ICE/DTLS/RTCP from media. A dropped (overflow) packet still
            // arrived on the path, so count before the try_send.
            raw_recv.record(n);
            let packet = InboundPacket {
                endpoint_id,
                source,
                data: buf[..n].to_vec(),
                is_rtcp: false,
                local: Some(local),
            };
            // Non-blocking: a full session channel must never PARK the reader — a
            // parked reader stops servicing the socket and blackholes the
            // endpoint entirely (the very wedge this guards against), so we drop
            // under backpressure. NOTE this drops STUN/DTLS as well as RTP/SRTP;
            // a full channel is itself an overload signal
            // (`webrtc_recv_overflow`), and dropping a setup packet is strictly
            // better than wedging the socket.
            match packet_tx.try_send(packet) {
                Ok(()) => RecvAction::Continue,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    metrics.webrtc_recv_overflow.inc();
                    RecvAction::Continue
                }
                Err(mpsc::error::TrySendError::Closed(_)) => RecvAction::Stop("session_dropped"),
            }
        }
        Err(e) => {
            warn!(endpoint_id = %endpoint_id, error = %e, "UDP recv error");
            RecvAction::Stop("udp_error")
        }
    }
}

impl WebRtcEndpoint {
    /// Create a new WebRTC endpoint with its own UDP socket
    async fn new_with_socket(
        id: EndpointId,
        config: EndpointConfig,
        bind_addrs: &[SocketAddr],
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<Self> {
        // WebRTC endpoints use OS-assigned ephemeral ports (not rtp_port_range).
        // ICE negotiates connectivity dynamically, so fixed port ranges don't apply.
        // One socket per configured address family (dual-stack): each becomes an
        // ICE host candidate, and ICE nominates the working pair.
        if bind_addrs.is_empty() {
            anyhow::bail!("WebRTC endpoint requires at least one bind address");
        }
        let mut sockets = Vec::with_capacity(bind_addrs.len());
        for &bind_addr in bind_addrs {
            let socket = UdpSocket::bind(bind_addr).await?;
            let local_addr = socket.local_addr()?;
            sockets.push((local_addr, Arc::new(socket)));
        }
        let local_addr = sockets[0].0;

        let rtc = RtcConfig::new()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            // Emit periodic stats so we can surface RTT for the WebRTC leg.
            // str0m owns RTCP here, so RTT only reaches us via these events
            // (see `peer_rtt_ms` and the stats arms in the event loop).
            .set_stats_interval(Some(Duration::from_secs(1)))
            .build(Instant::now());

        Ok(Self {
            id,
            config: config.clone(),
            state: EndpointState::New,
            stats: EndpointStats::new(),
            raw_recv: Arc::new(RawRecvCounters::default()),
            ice_connection_state: None,
            rtcp_stats: RtcpStats::new(),
            peer_rtt_ms: None,
            rtc,
            sockets,
            local_addr,
            remote_addr: None,
            audio_mid: None,
            outbound_seq_no: (rand::random::<u16>() as u64).into(),
            last_outbound_ts: None,
            last_source_id: None,
            last_source_ts: None,
            learned_step: None,
            pending_offer: None,
            offer_generation: 0,
            auto_direction: config.direction,
            connecting_since: None,
            connecting_warned: false,
            packet_error_warned: false,
            recv_task: None,
            cancel_token: CancellationToken::new(),
            metrics,
            recv_started: Arc::new(AtomicBool::new(false)),
            recv_start_deadline: None,
            recv_dead_reported: false,
        })
    }

    /// Register one ICE host candidate per bound socket. With dual-stack this
    /// offers both an IPv4 and an IPv6 host candidate and lets ICE nominate the
    /// pair that connects. Local addresses are collected first so we don't hold a
    /// borrow of `self.sockets` across the `&mut self.rtc` call.
    fn add_host_candidates(&mut self) -> anyhow::Result<()> {
        let locals: Vec<SocketAddr> = self.sockets.iter().map(|(la, _)| *la).collect();
        for local in locals {
            let candidate = Candidate::host(local, "udp")?;
            self.rtc.add_local_candidate(candidate);
        }
        Ok(())
    }

    /// Mark the start of a negotiation attempt for the connecting-watchdog.
    /// Called only from sites where negotiation is genuinely in flight
    /// (remote credentials known, ICE can progress): from_offer, accept_answer,
    /// accept_offer. NOT called from create_offer or ice_restart, where the
    /// offer can sit indefinitely without a counter-answer (e.g. ring-no-answer
    /// or caller hangup) — those rely on accept_answer / IceConnectionState::
    /// Checking to arm at the right moment. Resets `connecting_warned` and
    /// `packet_error_warned` so the next stall produces a fresh WARN.
    fn mark_negotiation_started(&mut self) {
        self.connecting_since = Some(Instant::now());
        self.connecting_warned = false;
        self.packet_error_warned = false;
    }

    fn reset_outbound_rtp_timeline(&mut self) {
        self.outbound_seq_no = (rand::random::<u16>() as u64).into();
        self.last_outbound_ts = None;
        self.last_source_id = None;
        self.last_source_ts = None;
        self.learned_step = None;
    }

    pub fn set_direction_override(&mut self, update: EndpointDirectionUpdate) {
        if let Some(dir) = update.as_direction() {
            self.config.direction = dir;
        } else {
            self.config.direction = self.auto_direction;
        }
    }

    /// Rotate the outbound SSRC on the audio TX stream.
    ///
    /// Used on hold→unhold transitions to force libwebrtc on the receiver to
    /// rebuild its `AudioReceiveStream` with a fresh wall-clock anchor. Without
    /// this, a paused-then-resumed iOS receiver discards post-resume packets as
    /// "too late" because the server's RTP timeline is real-time-continuous
    /// while the local audio clock paused for the duration of the hold.
    ///
    /// `reset_stream_tx` rotates the SSRC, clears str0m's RTP↔wallclock anchor,
    /// and forces a prompt SR/SDES emission for the new SSRC. We also restart
    /// our own seq/timestamp state because in RTP mode str0m uses the values we
    /// pass into `write_rtp`. CNAME is preserved (lives on the m-section, not
    /// the SSRC) so the receiver ties the new SSRC back to the same logical
    /// source.
    pub fn bump_outbound_ssrc(&mut self) -> anyhow::Result<()> {
        let mid = self
            .audio_mid
            .ok_or_else(|| anyhow::anyhow!("no audio mid negotiated"))?;
        let new_ssrc: str0m::rtp::Ssrc = rand::random::<u32>().into();
        let mut api = self.rtc.direct_api();
        if api.reset_stream_tx(mid, None, new_ssrc, None).is_none() {
            // None means: no stream for mid, or new SSRC equals current. The
            // SSRC collision is astronomically unlikely; missing stream means
            // we got called before the first negotiation, which is a no-op.
            debug!(endpoint_id = %self.id, "bump_outbound_ssrc: no TX stream to rotate (yet)");
            return Ok(());
        }
        debug!(
            endpoint_id = %self.id,
            new_ssrc = *new_ssrc,
            "WebRTC outbound SSRC rotated"
        );
        self.reset_outbound_rtp_timeline();
        Ok(())
    }

    /// Start (or restart) the recv task that reads UDP packets and forwards them
    /// to the session. The receive path is otherwise fire-and-forget: a task the
    /// runtime never schedules silently blackholes all media for the endpoint
    /// (see docs/incident-research/webrtc-recv-task-wedge.md). To make that
    /// observable, the task flips `recv_started` the instant it reaches its loop
    /// and this arms
    /// `recv_start_deadline`; the session liveness sweep (`supervise_recv`) then
    /// flags a task that never starts or later dies — off the hot path, so
    /// nothing here blocks creation or the session task.
    pub fn start_recv_task(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        let sockets = self.sockets.clone();
        let endpoint_id = self.id;
        let local_addr = self.local_addr;
        let token = self.cancel_token.clone();
        let metrics = Arc::clone(&self.metrics);
        let raw_recv = Arc::clone(&self.raw_recv);
        let recv_started = Arc::clone(&self.recv_started);
        // Fresh attempt: clear the liveness flag (a prior task on transfer
        // restart set it true), arm the start-grace deadline, and let the sweep
        // report this attempt afresh.
        recv_started.store(false, Ordering::Relaxed);
        self.recv_start_deadline = Some(Instant::now() + RECV_TASK_START_GRACE);
        self.recv_dead_reported = false;

        let handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(async move {
                // Signal liveness the instant the loop is reachable, BEFORE the
                // first await, so a task the runtime cannot schedule is caught by
                // the sweep rather than silently dropping all inbound media.
                recv_started.store(true, Ordering::Relaxed);
                metrics.webrtc_recv_task_started.inc();

                // A single recv task multiplexes every bound socket (one socket
                // for single-family, two for dual-stack v4+v6). Keeping it one
                // task preserves the liveness model (one `recv_started`, one
                // handle for the wedge sweep). At most one v4 + one v6 socket, so
                // a two-arm select covers it; each datagram is tagged with the
                // local address it arrived on.
                let (la0, s0) = sockets[0].clone();
                let s1: Option<(SocketAddr, Arc<UdpSocket>)> = sockets.get(1).cloned();

                let mut buf0 = vec![0u8; 4096];
                let mut buf1 = vec![0u8; 4096];
                let mut exit_reason = "cancelled";
                loop {
                    tokio::select! {
                        result = s0.recv_from(&mut buf0) => {
                            match forward_datagram(result, &buf0, la0, endpoint_id, &raw_recv, &metrics, &packet_tx) {
                                RecvAction::Continue => {}
                                RecvAction::Stop(reason) => { exit_reason = reason; break; }
                            }
                        }
                        // Disabled (and never polled) when there's no second
                        // socket. The async block is lazy, so the unwrap only
                        // runs when `s1` is Some.
                        result = async { s1.as_ref().unwrap().1.recv_from(&mut buf1).await }, if s1.is_some() => {
                            let la1 = s1.as_ref().unwrap().0;
                            match forward_datagram(result, &buf1, la1, endpoint_id, &raw_recv, &metrics, &packet_tx) {
                                RecvAction::Continue => {}
                                RecvAction::Stop(reason) => { exit_reason = reason; break; }
                            }
                        }
                        _ = token.cancelled() => {
                            break;
                        }
                    }
                }

                // Counts COOPERATIVE loop exits only (cancellation observed in
                // the select!, session-channel close, UDP error). A `Drop`-driven
                // teardown aborts the JoinHandle and does NOT run this.
                metrics.webrtc_recv_task_exited.inc();
                if exit_reason == "cancelled" {
                    debug!(
                        endpoint_id = %endpoint_id,
                        %local_addr,
                        reason = exit_reason,
                        "WebRTC recv task exiting"
                    );
                } else {
                    warn!(
                        endpoint_id = %endpoint_id,
                        %local_addr,
                        reason = exit_reason,
                        "WebRTC recv task exiting"
                    );
                }
            })
            .catch_unwind()
            .await;

            if let Err(payload) = result {
                error!(
                    endpoint_id = %endpoint_id,
                    panic = %panic_payload_message(payload.as_ref()),
                    "WebRTC recv task panicked"
                );
            }
        });

        self.recv_task = Some(handle);
    }

    /// Session liveness sweep (called ~1×/s by `run_media_session`). Reports —
    /// once — a recv task that is wedged while its endpoint is still active, the
    /// "task gone/never-started, endpoint live = guaranteed media blackhole"
    /// condition. Covers BOTH failure modes and all (re)start paths off the hot
    /// path. See docs/incident-research/webrtc-recv-task-wedge.md.
    pub fn supervise_recv(&mut self) {
        if self.recv_dead_reported {
            return;
        }
        // (a) Task started, then exited/panicked while the endpoint is live.
        if let Some(handle) = &self.recv_task
            && handle.is_finished()
        {
            self.recv_dead_reported = true;
            self.metrics.webrtc_recv_task_dead.inc();
            error!(
                endpoint_id = %self.id,
                local_addr = %self.local_addr,
                state = ?self.state,
                "WebRTC recv task is gone but endpoint is still active — media will \
                 blackhole (see docs/incident-research/webrtc-recv-task-wedge.md)"
            );
            return;
        }
        // (b) Task spawned but never reached its loop within the grace window.
        // Count this never-started variant so a pod can be alerted/drained.
        if !self.recv_started.load(Ordering::Relaxed)
            && let Some(deadline) = self.recv_start_deadline
            && Instant::now() > deadline
        {
            self.recv_dead_reported = true;
            self.metrics.webrtc_recv_task_start_timeout.inc();
            error!(
                endpoint_id = %self.id,
                local_addr = %self.local_addr,
                grace_ms = RECV_TASK_START_GRACE.as_millis() as u64,
                "WebRTC recv task never started within the grace window — media \
                 datapath wedge (see docs/incident-research/webrtc-recv-task-wedge.md)"
            );
        }
    }

    /// Create from a remote SDP offer, returning the SDP answer string
    pub async fn from_offer(
        id: EndpointId,
        direction: EndpointDirection,
        offer_sdp: &str,
        bind_addrs: &[SocketAddr],
        packet_tx: mpsc::Sender<InboundPacket>,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<(Self, String)> {
        let config = EndpointConfig { direction };
        let mut endpoint = Self::new_with_socket(id, config, bind_addrs, metrics).await?;

        // One ICE host candidate per bound socket (IPv4 and/or IPv6).
        endpoint.add_host_candidates()?;

        // Parse SDP offer (try raw SDP string first, then JSON)
        let offer = SdpOffer::from_sdp_string(offer_sdp).or_else(|_| {
            serde_json::from_str::<SdpOffer>(offer_sdp)
                .map_err(|e| anyhow::anyhow!("Failed to parse SDP offer: {e}"))
        })?;

        let answer = endpoint.rtc.sdp_api().accept_offer(offer)?;
        let answer_str = answer.to_sdp_string();

        endpoint.state = EndpointState::Connecting;
        endpoint.mark_negotiation_started();
        endpoint.start_recv_task(packet_tx);

        Ok((endpoint, answer_str))
    }

    /// Create an SDP offer for a new outgoing endpoint
    pub async fn create_offer(
        id: EndpointId,
        direction: EndpointDirection,
        bind_addrs: &[SocketAddr],
        packet_tx: mpsc::Sender<InboundPacket>,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<(Self, String)> {
        let config = EndpointConfig { direction };
        let mut endpoint = Self::new_with_socket(id, config, bind_addrs, metrics).await?;

        // One ICE host candidate per bound socket (IPv4 and/or IPv6).
        endpoint.add_host_candidates()?;

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
        // Watchdog is NOT armed here: a created offer that the caller never
        // answers (hangup during ringing, ring-no-answer) would otherwise
        // false-fire at 15s. Negotiation only becomes "in flight" when the
        // answer arrives (accept_answer) or ICE moves to Checking — both arm
        // the watchdog at the right moment.
        endpoint.start_recv_task(packet_tx);

        Ok((endpoint, offer_str))
    }

    /// Accept a remote SDP answer (after we created an offer)
    pub fn accept_answer(&mut self, answer_sdp: &str) -> anyhow::Result<()> {
        // Parse the answer BEFORE taking `pending_offer`. If parsing fails, the
        // pending offer must survive so a later, well-formed retry of the same
        // answer can still be applied — taking first would discard it on a
        // malformed body and wedge the endpoint with no way to complete.
        let answer = SdpAnswer::from_sdp_string(answer_sdp).or_else(|_| {
            serde_json::from_str::<SdpAnswer>(answer_sdp)
                .map_err(|e| anyhow::anyhow!("Failed to parse SDP answer: {e}"))
        })?;

        let pending = self
            .pending_offer
            .take()
            .ok_or_else(|| anyhow::anyhow!("No pending offer to accept answer for"))?;

        self.rtc.sdp_api().accept_answer(pending, answer)?;

        // Restart the watchdog so a stall after the answer is applied is
        // measured from now, not from the original offer.
        self.mark_negotiation_started();

        Ok(())
    }

    /// Accept a remote SDP offer (re-negotiation), returning our SDP answer.
    pub fn accept_offer(&mut self, offer_sdp: &str) -> anyhow::Result<String> {
        let offer = SdpOffer::from_sdp_string(offer_sdp).or_else(|_| {
            serde_json::from_str::<SdpOffer>(offer_sdp)
                .map_err(|e| anyhow::anyhow!("Failed to parse SDP offer: {e}"))
        })?;

        let answer = self.rtc.sdp_api().accept_offer(offer)?;

        // If we had a local offer in flight, a remote offer supersedes it.
        // Dropping pending_offer is equivalent to rolling back the local offer.
        self.pending_offer = None;
        // Only arm the watchdog for re-offers received before we ever finished
        // the initial handshake. A re-offer on an already-Connected endpoint is
        // often a pure SDP/direction change that does NOT trigger a new ICE or
        // DTLS completion event — arming the watchdog there would false-positive
        // 15s later. True ICE-restart re-offers on a Connected transport are a
        // known blind spot here.
        if self.state != EndpointState::Connected {
            self.mark_negotiation_started();
        }

        Ok(answer.to_sdp_string())
    }

    /// Perform an ICE restart, returning the new SDP offer and its generation.
    /// The generation is monotonic per endpoint; the caller echoes it back on
    /// `accept_answer` so a stale answer for a superseded offer is rejected.
    pub fn ice_restart(&mut self) -> anyhow::Result<(String, u64)> {
        // str0m keeps only ONE pending offer per peer connection. Starting a new
        // ICE restart while a prior offer is still unanswered would discard that
        // offer; a later answer to it would then be applied against this new
        // offer, diverging the two peers' ICE credentials so no candidate pair
        // validates and media silently dies (no error, only a media timeout,
        // recovered only by tearing down the call). Refuse instead — the caller
        // must apply the outstanding answer first. The RPC layer
        // (`handle_ice_restart`) also enforces this and counts conflicts; this
        // guard keeps the endpoint method correct in isolation.
        // See docs/protocol/endpoints.md → endpoint.webrtc.ice_restart.
        if self.pending_offer.is_some() {
            anyhow::bail!("ICE restart already pending; outstanding offer must be answered first");
        }
        let mut api = self.rtc.sdp_api();
        let _creds = api.ice_restart(true); // keep local candidates
        let (offer, pending) = api
            .apply()
            .ok_or_else(|| anyhow::anyhow!("Failed to create ICE restart offer"))?;

        let offer_str = offer.to_sdp_string();
        self.pending_offer = Some(pending);
        self.offer_generation += 1;
        // Watchdog is NOT armed here: like create_offer, the restart offer
        // can sit indefinitely if the remote never answers. Arming happens
        // on accept_answer or when ICE re-enters Checking — both fire only
        // once the negotiation is genuinely in flight.
        Ok((offer_str, self.offer_generation))
    }

    /// Feed a received UDP packet into the str0m state machine. `local` is the
    /// address of the socket the datagram arrived on — for dual-stack endpoints
    /// this differs per family, and str0m must be told the correct destination
    /// so it matches the right local ICE candidate base.
    pub fn handle_receive(
        &mut self,
        source: SocketAddr,
        local: SocketAddr,
        data: &[u8],
        now: Instant,
    ) -> anyhow::Result<()> {
        let receive = Receive::new(Protocol::Udp, source, local, data)?;
        let input = Input::Receive(now, receive);
        self.rtc.handle_input(input)?;
        Ok(())
    }

    /// Handle a timeout
    pub fn handle_timeout(&mut self, now: Instant) -> anyhow::Result<()> {
        self.rtc.handle_input(Input::Timeout(now))?;
        Ok(())
    }

    /// `(local, remote)` socket addresses for recording: the ICE-nominated peer
    /// and the bound local socket of the *matching family*. With dual-stack, ICE
    /// may nominate the non-primary (e.g. IPv6) socket, so we must not assume the
    /// primary `local_addr`. Returns `(None, None)` until a peer is nominated, so
    /// pre-connection packets fall back to synthetic framing rather than a
    /// mismatched v4-local / v6-remote frame.
    pub fn recording_addrs(&self) -> (Option<SocketAddr>, Option<SocketAddr>) {
        match self.remote_addr {
            Some(remote) => {
                // Strictly same-family: if no bound socket matches the nominated
                // remote's family (shouldn't happen — our candidates are our
                // sockets), return None so framing falls back to synthetic rather
                // than emitting a mismatched v4-local / v6-remote frame.
                let local = self
                    .sockets
                    .iter()
                    .find(|(la, _)| la.is_ipv6() == remote.is_ipv6())
                    .map(|(la, _)| *la);
                (local, Some(remote))
            }
            None => (None, None),
        }
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
                    if self.state == EndpointState::Connected {
                        self.remote_addr = Some(transmit.destination);
                    }
                    // Route by `transmit.source`: str0m sets it to the local
                    // candidate base (== one of our bound socket addresses), so
                    // for dual-stack we must send from the socket of the
                    // nominated family. An unmatched source means the ICE state
                    // is inconsistent with our sockets; drop + warn rather than
                    // silently sending from the wrong family.
                    match self
                        .sockets
                        .iter()
                        .find(|(local, _)| *local == transmit.source)
                    {
                        Some((_, socket)) => {
                            // Non-blocking send to avoid spawning a task per
                            // packet. UDP sends almost never block; if the socket
                            // isn't ready we drop (acceptable for real-time media).
                            match socket.try_send_to(&transmit.contents, transmit.destination) {
                                Ok(_) => {
                                    self.metrics.webrtc_udp_send_ok.inc();
                                    self.metrics.record_udp_send_ok(
                                        "webrtc",
                                        "datagram",
                                        transmit.destination,
                                    );
                                }
                                Err(e) => {
                                    self.metrics.webrtc_udp_send_dropped.inc();
                                    self.metrics.record_udp_send_error(
                                        "webrtc",
                                        "datagram",
                                        transmit.destination,
                                    );
                                    trace!(error = %e, "UDP send dropped (would block)");
                                }
                            }
                        }
                        None => {
                            self.metrics.webrtc_udp_send_dropped.inc();
                            self.metrics.record_udp_send_error(
                                "webrtc",
                                "datagram",
                                transmit.destination,
                            );
                            warn!(
                                endpoint_id = %self.id,
                                source = %transmit.source,
                                destination = %transmit.destination,
                                "WebRTC transmit.source matches no bound socket — dropping \
                                 (ICE state inconsistent with bound sockets)"
                            );
                        }
                    }
                }
                Output::Event(event) => match event {
                    Event::Connected => {
                        debug!(endpoint_id = %self.id, "WebRTC connected");
                        let old = self.state;
                        self.state = EndpointState::Connected;
                        self.connecting_since = None;
                        self.connecting_warned = false;
                        events.push(WebRtcEvent::StateChanged {
                            old,
                            new: self.state,
                        });
                    }
                    Event::IceConnectionStateChange(ice_state) => {
                        debug!(endpoint_id = %self.id, ?ice_state, "ICE state change");
                        // Surface the raw ICE state independently of the
                        // collapsed endpoint state. str0m only emits this on a
                        // genuine transition, but guard against a no-op repeat
                        // so consumers never see a spurious duplicate. This is
                        // the finer-grained signal: `Disconnected` here is ICE
                        // consent loss (remote path failure), which the
                        // endpoint state also reports, but `Checking` vs
                        // `Connected` etc. are only visible at this level.
                        if self.ice_connection_state != Some(ice_state) {
                            self.ice_connection_state = Some(ice_state);
                            events.push(WebRtcEvent::IceStateChanged { state: ice_state });
                        }
                        match ice_state {
                            IceConnectionState::Checking => {
                                // str0m enters Checking whenever a new ICE
                                // attempt begins — initial handshake AND every
                                // ICE restart. Use it as the canonical arm
                                // signal: catches remote-initiated ICE restarts
                                // via `accept_offer` (where we skip arming in
                                // the post-Connected pure-SDP case) without
                                // false-positiving on those pure-SDP re-offers,
                                // which do NOT emit Checking.
                                //
                                // GATED on `pending_offer.is_none()`: str0m's
                                // ICE agent will transition New→Checking on its
                                // own timeout regardless of whether remote
                                // credentials have arrived. For the offerer
                                // flow, `pending_offer` stays Some until
                                // accept_answer runs, so we skip arming here
                                // and let accept_answer be the canonical arm
                                // point. Otherwise an unanswered offer (e.g.
                                // ring-no-answer) would false-fire at 15s.
                                if self.pending_offer.is_none() {
                                    self.connecting_since = Some(Instant::now());
                                    self.connecting_warned = false;
                                    self.packet_error_warned = false;
                                }
                            }
                            IceConnectionState::Connected | IceConnectionState::Completed => {
                                // Always disarm the watchdog on a successful ICE
                                // (re-)connection, even if our top-level state is
                                // already Connected — a post-Connected ICE restart
                                // re-arms the watchdog while leaving `state`
                                // unchanged, and we still need to clear it here.
                                self.connecting_since = None;
                                self.connecting_warned = false;
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
                                // Disarm watchdog regardless of prior state — a
                                // disconnect means there's no longer an in-flight
                                // attempt to wait for, even from a no-op
                                // Disconnected→Disconnected event.
                                self.connecting_since = None;
                                self.connecting_warned = false;
                                if self.state != EndpointState::Disconnected {
                                    let old = self.state;
                                    self.state = EndpointState::Disconnected;
                                    self.remote_addr = None;
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
                        self.rtcp_stats.record_received(
                            *pkt.header.ssrc,
                            pkt.header.sequence_number,
                            pkt.header.timestamp,
                            pkt.payload.len(),
                            WEBRTC_OPUS_RTP_CLOCK_HZ,
                        );
                        events.push(WebRtcEvent::RtpPacket(RoutedRtpPacket {
                            source_endpoint_id: self.id,
                            payload_type: *pkt.header.payload_type,
                            sequence_number: pkt.header.sequence_number,
                            timestamp: pkt.header.timestamp,
                            ssrc: *pkt.header.ssrc,
                            marker: pkt.header.marker,
                            payload: pkt.payload.to_vec(),
                        }));
                    }
                    Event::MediaAdded(media) => {
                        debug!(endpoint_id = %self.id, mid = %media.mid, kind = ?media.kind, "media added");
                        if media.kind == MediaKind::Audio {
                            self.audio_mid = Some(media.mid);
                        }
                    }
                    // RTT for the WebRTC leg. We capture whichever stats event
                    // carries a value: PeerStats is ICE/transport-derived, but
                    // because we run ICE-lite (str0m doesn't initiate STUN
                    // binding requests) its `rtt` is often None, so the
                    // egress/ingress RTCP round-trip is usually what fills this.
                    // Any `Some` updates the field; a `None` never clobbers it.
                    Event::PeerStats(s) => {
                        if let Some(rtt) = s.rtt {
                            self.peer_rtt_ms = Some(rtt.as_secs_f64() * 1000.0);
                        }
                    }
                    Event::MediaEgressStats(s) => {
                        if let Some(rtt) = s.rtt {
                            self.peer_rtt_ms = Some(rtt.as_secs_f64() * 1000.0);
                        }
                    }
                    Event::MediaIngressStats(s) => {
                        if let Some(rtt) = s.rtt {
                            self.peer_rtt_ms = Some(rtt.as_secs_f64() * 1000.0);
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
        let seq_no = self.outbound_seq_no.inc();
        let (outbound_ts, marker) = self.advance_outbound_timeline(
            packet.source_endpoint_id,
            packet.timestamp,
            packet.marker,
        );

        let mut api = self.rtc.direct_api();
        let stream_tx = api
            .stream_tx_by_mid(mid, None)
            .ok_or_else(|| anyhow::anyhow!("No TX stream for mid {mid}"))?;

        // Clone needed: str0m takes ownership, but the same packet may route to multiple destinations
        let rtp = str0m::rtp::RtpWrite::new(
            pt,
            seq_no,
            outbound_ts,
            Instant::now(),
            packet.payload.clone(),
        )
        .marker(marker);
        stream_tx.write_rtp(rtp);

        self.stats.record_outbound(packet.payload.len());
        Ok(())
    }

    /// Advance the destination-owned outbound RTP timestamp timeline.
    ///
    /// WebRTC TX uses one SSRC per endpoint. When the routed source changes
    /// under that SSRC (hold music/file insertion, mixer<->passthrough,
    /// endpoint replacement), the source packet timestamp can jump into an
    /// unrelated domain. Collapse those jumps to one packet duration and set
    /// marker so the receiver re-anchors instead of treating the stream as a
    /// massive loss/jitter event.
    fn advance_outbound_timeline(
        &mut self,
        source_id: EndpointId,
        source_ts: u32,
        source_marker: bool,
    ) -> (u32, bool) {
        let nominal_step = WEBRTC_OPUS_RTP_CLOCK_HZ / 50;
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
                    (last_out, false)
                } else if delta > max_safe {
                    (last_out.wrapping_add(bump), true)
                } else {
                    if delta <= bump.saturating_mul(2) {
                        self.learned_step = Some(delta);
                    }
                    (last_out.wrapping_add(delta), false)
                }
            }
            (Some(last_out), _, _) => (last_out.wrapping_add(bump), true),
            _ => (source_ts, source_marker),
        };

        self.last_outbound_ts = Some(outbound_ts);
        self.last_source_id = Some(source_id);
        self.last_source_ts = Some(source_ts);
        (outbound_ts, source_marker || marker_override)
    }

    /// The negotiated primary audio codec, read from str0m's media line after the
    /// SDP answer is applied. Returns `None` until an audio mid exists and the
    /// remote has agreed at least one audio payload type (pre-negotiation / pre-ICE).
    ///
    /// Replaces the hardcoded `opus`/PT-111 assumption — notably on the answerer /
    /// re-negotiation path where the remote may pick a different PT. str0m's `Codec`
    /// enum only carries Opus/PCMU/PCMA for audio (no G.722), so those are the
    /// possible names.
    pub fn negotiated_codec(&self) -> Option<NegotiatedCodec> {
        let mid = self.audio_mid?;
        let media = self.rtc.media(mid)?;
        let remote_pts = media.remote_pts();
        if remote_pts.is_empty() {
            return None;
        }
        for p in self.rtc.codec_config().params() {
            if !remote_pts.contains(&p.pt()) {
                continue;
            }
            let spec = p.spec();
            if !spec.codec.is_audio() {
                continue;
            }
            let name = match spec.codec {
                str0m::format::Codec::Opus => "opus",
                str0m::format::Codec::PCMU => "PCMU",
                str0m::format::Codec::PCMA => "PCMA",
                // Skip non-audio / telephone-event-like / Null / Unknown.
                _ => continue,
            };
            return Some(NegotiatedCodec {
                name,
                pt: *p.pt(),
                clock_rate: spec.clock_rate.get(),
                channels: spec.channels.unwrap_or(1),
            });
        }
        None
    }

    /// Build this endpoint's recording stream descriptor from the negotiated codec,
    /// or `None` if nothing is negotiated yet. Role is always `remote` (real peer).
    pub fn stream_descriptor(
        &self,
        local: Option<std::net::SocketAddr>,
        remote: Option<std::net::SocketAddr>,
    ) -> Option<crate::recording::meta::StreamDescriptor> {
        let nc = self.negotiated_codec()?;
        Some(crate::recording::meta::StreamDescriptor {
            v: crate::recording::meta::VERSION,
            endpoint_id: self.id.to_string(),
            role: "remote".to_string(),
            ep_type: "webrtc".to_string(),
            codec: nc.name.to_string(),
            pt: nc.pt,
            clock_rate: nc.clock_rate,
            channels: nc.channels,
            endian: None,
            ssrc: None,
            local: local
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".to_string()),
            remote: remote
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".to_string()),
        })
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
        // Transfer path. `start_recv_task` re-arms the start-grace deadline and
        // resets the report flag, so the liveness sweep (`supervise_recv`) covers
        // a restarted task that never reaches its loop just like a fresh one.
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
    /// str0m ICE connection state transitioned. Finer-grained than
    /// `StateChanged`; `Disconnected` is the remote-path-lost signal.
    IceStateChanged {
        state: IceConnectionState,
    },
    RtpPacket(RoutedRtpPacket),
}

/// Lowercase wire name for an ICE connection state, used in stats and the
/// `endpoint.ice_state_changed` event.
pub fn ice_state_str(state: IceConnectionState) -> &'static str {
    match state {
        IceConnectionState::New => "new",
        IceConnectionState::Checking => "checking",
        IceConnectionState::Connected => "connected",
        IceConnectionState::Completed => "completed",
        IceConnectionState::Disconnected => "disconnected",
    }
}

#[cfg(test)]
#[path = "endpoint_webrtc_tests.rs"]
mod tests;
