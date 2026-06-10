use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
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
/// session task. See docs/WEBRTC_RECV_TASK_WEDGE.md.
const RECV_TASK_START_GRACE: Duration = Duration::from_secs(2);

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
    pub rtc: Rtc,
    pub socket: Arc<UdpSocket>,
    pub local_addr: SocketAddr,
    /// Last destination str0m emitted a transmit to once ICE was nominated.
    /// Cleared on disconnect.
    pub remote_addr: Option<SocketAddr>,
    /// Mid for the audio media line (set after SDP negotiation)
    pub audio_mid: Option<Mid>,
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

impl WebRtcEndpoint {
    /// Create a new WebRTC endpoint with its own UDP socket
    async fn new_with_socket(
        id: EndpointId,
        config: EndpointConfig,
        bind_addr: SocketAddr,
        metrics: Arc<Metrics>,
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
            config: config.clone(),
            state: EndpointState::New,
            stats: EndpointStats::new(),
            raw_recv: Arc::new(RawRecvCounters::default()),
            ice_connection_state: None,
            rtcp_stats: RtcpStats::new(),
            rtc,
            socket,
            local_addr,
            remote_addr: None,
            audio_mid: None,
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
    /// `reset_stream_tx` rotates the SSRC, restarts the seq number, clears the
    /// RTP↔wallclock anchor, and forces a prompt SR/SDES emission for the new
    /// SSRC. CNAME is preserved (lives on the m-section, not the SSRC) so the
    /// receiver ties the new SSRC back to the same logical source.
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
        Ok(())
    }

    /// Start (or restart) the recv task that reads UDP packets and forwards them
    /// to the session. The receive path is otherwise fire-and-forget: a task the
    /// runtime never schedules silently blackholes all media for the endpoint
    /// (see docs/WEBRTC_RECV_TASK_WEDGE.md). To make that observable, the task
    /// flips `recv_started` the instant it reaches its loop and this arms
    /// `recv_start_deadline`; the session liveness sweep (`supervise_recv`) then
    /// flags a task that never starts or later dies — off the hot path, so
    /// nothing here blocks creation or the session task.
    pub fn start_recv_task(&mut self, packet_tx: mpsc::Sender<InboundPacket>) {
        let socket = Arc::clone(&self.socket);
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

                let mut buf = vec![0u8; 4096];
                let mut exit_reason = "cancelled";
                loop {
                    tokio::select! {
                        result = socket.recv_from(&mut buf) => {
                            match result {
                                Ok((n, source)) => {
                                    // Wire-level count: every datagram, BEFORE
                                    // str0m demuxes ICE/DTLS/RTCP from media. A
                                    // dropped (overflow) packet still arrived on
                                    // the path, so count before the try_send.
                                    raw_recv.record(n);
                                    let packet = InboundPacket {
                                        endpoint_id,
                                        source,
                                        data: buf[..n].to_vec(),
                                        is_rtcp: false,
                                    };
                                    // Non-blocking: a full session channel must
                                    // never PARK the reader — a parked reader
                                    // stops servicing the socket and blackholes
                                    // the endpoint entirely (the very wedge this
                                    // guards against), so we drop under
                                    // backpressure. NOTE this drops STUN/DTLS as
                                    // well as RTP/SRTP; a full 256-deep channel is
                                    // itself an overload signal
                                    // (`webrtc_recv_overflow`), and dropping a
                                    // setup packet is strictly better than wedging
                                    // the socket. Class-aware priority for
                                    // STUN/DTLS is a documented follow-up (runbook).
                                    match packet_tx.try_send(packet) {
                                        Ok(()) => {}
                                        Err(mpsc::error::TrySendError::Full(_)) => {
                                            metrics.webrtc_recv_overflow.inc();
                                        }
                                        Err(mpsc::error::TrySendError::Closed(_)) => {
                                            exit_reason = "session_dropped";
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(endpoint_id = %endpoint_id, error = %e, "UDP recv error");
                                    exit_reason = "udp_error";
                                    break;
                                }
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
    /// path. See docs/WEBRTC_RECV_TASK_WEDGE.md.
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
                 blackhole (see docs/WEBRTC_RECV_TASK_WEDGE.md)"
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
                 datapath wedge (see docs/WEBRTC_RECV_TASK_WEDGE.md)"
            );
        }
    }

    /// Create from a remote SDP offer, returning the SDP answer string
    pub async fn from_offer(
        id: EndpointId,
        direction: EndpointDirection,
        offer_sdp: &str,
        bind_addr: SocketAddr,
        packet_tx: mpsc::Sender<InboundPacket>,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<(Self, String)> {
        let config = EndpointConfig { direction };
        let mut endpoint = Self::new_with_socket(id, config, bind_addr, metrics).await?;

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
        endpoint.mark_negotiation_started();
        endpoint.start_recv_task(packet_tx);

        Ok((endpoint, answer_str))
    }

    /// Create an SDP offer for a new outgoing endpoint
    pub async fn create_offer(
        id: EndpointId,
        direction: EndpointDirection,
        bind_addr: SocketAddr,
        packet_tx: mpsc::Sender<InboundPacket>,
        metrics: Arc<Metrics>,
    ) -> anyhow::Result<(Self, String)> {
        let config = EndpointConfig { direction };
        let mut endpoint = Self::new_with_socket(id, config, bind_addr, metrics).await?;

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
                    if self.state == EndpointState::Connected {
                        self.remote_addr = Some(transmit.destination);
                    }
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
mod tests {
    use super::*;

    /// The recv task starts promptly after creation, signals liveness, and a
    /// healthy task within the grace window is NOT flagged by the liveness sweep.
    /// See docs/WEBRTC_RECV_TASK_WEDGE.md.
    #[tokio::test]
    async fn test_recv_task_starts_and_is_not_flagged() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let metrics = Arc::new(Metrics::new());

        let mut ep = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::SendRecv,
            bind_addr,
            tx,
            metrics.clone(),
        )
        .await
        .expect("create_offer should succeed")
        .0;

        // The recv task starts asynchronously; it should reach its loop promptly.
        for _ in 0..200 {
            if ep.recv_started.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            ep.recv_started.load(Ordering::Relaxed),
            "recv task should start"
        );
        assert_eq!(metrics.webrtc_recv_task_started.get(), 1);

        // A started task is never flagged, and start_timeout stays zero.
        ep.supervise_recv();
        assert!(!ep.recv_dead_reported);
        assert_eq!(metrics.webrtc_recv_task_start_timeout.get(), 0);
    }

    /// The liveness sweep flags (and counts, once) a recv task that was spawned
    /// but never reached its loop past the grace deadline — the receive-task
    /// wedge, including the transfer-restart path. See docs/WEBRTC_RECV_TASK_WEDGE.md.
    #[tokio::test]
    async fn test_supervise_recv_detects_never_started() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let metrics = Arc::new(Metrics::new());
        let mut ep = WebRtcEndpoint::new_with_socket(
            id,
            EndpointConfig {
                direction: EndpointDirection::SendRecv,
            },
            bind_addr,
            metrics.clone(),
        )
        .await
        .expect("new_with_socket should succeed");

        // Simulate a spawned-but-never-polled recv task: a live (unfinished) task
        // that never sets `recv_started`, with the grace deadline already past.
        ep.recv_task = Some(tokio::spawn(std::future::pending::<()>()));
        ep.recv_started.store(false, Ordering::Relaxed);
        ep.recv_start_deadline = Some(Instant::now() - Duration::from_secs(1));

        ep.supervise_recv();
        assert!(
            ep.recv_dead_reported,
            "a never-started task must be flagged"
        );
        assert_eq!(metrics.webrtc_recv_task_start_timeout.get(), 1);

        // Idempotent: a second sweep does not re-count.
        ep.supervise_recv();
        assert_eq!(metrics.webrtc_recv_task_start_timeout.get(), 1);
    }

    /// The session liveness sweep flags (once) a recv task that exited while its
    /// endpoint is still active — the "task gone, endpoint live" blackhole.
    #[tokio::test]
    async fn test_supervise_recv_detects_dead_task() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let metrics = Arc::new(Metrics::new());

        let mut ep = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::SendRecv,
            bind_addr,
            tx,
            metrics.clone(),
        )
        .await
        .expect("create_offer should succeed")
        .0;

        // Force the recv task to exit while the endpoint stays in place.
        ep.cancel_token.cancel();
        for _ in 0..200 {
            if ep.recv_task.as_ref().unwrap().is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert!(!ep.recv_dead_reported);
        ep.supervise_recv();
        assert!(ep.recv_dead_reported, "a dead recv task must be reported");
        assert_eq!(metrics.webrtc_recv_task_exited.get(), 1);
        assert_eq!(metrics.webrtc_recv_task_dead.get(), 1);

        // Idempotent: a second sweep does not re-log.
        ep.supervise_recv();
        assert!(ep.recv_dead_reported);
        assert_eq!(metrics.webrtc_recv_task_dead.get(), 1);
    }
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

        let (ep, offer_sdp) = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::RecvOnly,
            bind_addr,
            tx,
            Arc::new(Metrics::new()),
        )
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

        let (_ep, offer_sdp) = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::SendOnly,
            bind_addr,
            tx,
            Arc::new(Metrics::new()),
        )
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

    /// Regression: create_offer must NOT arm the connecting-watchdog. The
    /// offer can sit indefinitely without a counter-answer (ring-no-answer,
    /// caller hangup), and str0m's ICE agent will independently transition
    /// New→Checking on its own timer. Both paths previously armed the
    /// watchdog, causing false `webrtc_connecting_stuck` increments on
    /// every unanswered call. Verifies the fix on both sites:
    ///   1. create_offer no longer calls mark_negotiation_started.
    ///   2. Event::IceConnectionStateChange(Checking) skips arming while
    ///      pending_offer.is_some().
    #[tokio::test]
    async fn test_create_offer_does_not_arm_watchdog_before_answer() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let (mut ep, _offer_sdp) = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::SendRecv,
            bind_addr,
            tx,
            Arc::new(Metrics::new()),
        )
        .await
        .expect("create_offer should succeed");

        assert!(
            ep.connecting_since.is_none(),
            "watchdog must not be armed at create_offer time"
        );
        assert!(
            ep.pending_offer.is_some(),
            "pending_offer must be Some while waiting for the answer"
        );

        // Drive str0m well past the point where its ICE agent would emit
        // Checking on its own (handle_timeout-driven New→Checking transition).
        // With pending_offer.is_some(), the Checking arm site must skip.
        let start = Instant::now();
        for i in 0..200 {
            let now = start + std::time::Duration::from_millis(i * 100);
            let _ = ep.handle_timeout(now);
            let _ = ep.poll_output();
        }

        assert!(
            ep.connecting_since.is_none(),
            "watchdog must NOT be armed by str0m's pre-answer Checking transition \
             while pending_offer is still Some (regression for unanswered-call \
             false positive)"
        );
    }

    #[tokio::test]
    async fn test_ice_restart_rejected_while_offer_pending() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        // create_offer leaves an unanswered pending offer.
        let (mut ep, _offer) = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::SendRecv,
            bind_addr,
            tx,
            Arc::new(Metrics::new()),
        )
        .await
        .expect("create_offer should succeed");
        assert!(ep.pending_offer.is_some());

        // A second outstanding offer would discard str0m's pending offer and
        // let a later answer apply against the wrong one — must be refused.
        let err = ep
            .ice_restart()
            .expect_err("ice_restart must be rejected while an offer is pending");
        assert!(
            err.to_string().contains("already pending"),
            "unexpected error: {err}"
        );
        assert!(
            ep.pending_offer.is_some(),
            "the rejected ice_restart must NOT have disturbed the existing pending offer"
        );
    }

    #[tokio::test]
    async fn test_accept_answer_malformed_preserves_pending_offer() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let (mut ep, _offer) = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::SendRecv,
            bind_addr,
            tx,
            Arc::new(Metrics::new()),
        )
        .await
        .expect("create_offer should succeed");
        assert!(ep.pending_offer.is_some());

        // A malformed answer must be rejected BEFORE the pending offer is taken,
        // so a later well-formed retry can still complete the negotiation.
        let err = ep
            .accept_answer("this is not valid sdp")
            .expect_err("malformed answer must be rejected");
        assert!(
            err.to_string().contains("parse SDP answer"),
            "unexpected error: {err}"
        );
        assert!(
            ep.pending_offer.is_some(),
            "a malformed answer must NOT consume the pending offer"
        );
    }

    #[tokio::test]
    async fn test_ice_restart_increments_and_returns_offer_generation() {
        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);

        let (mut ep, _offer) = WebRtcEndpoint::create_offer(
            id,
            EndpointDirection::SendRecv,
            bind_addr,
            tx,
            Arc::new(Metrics::new()),
        )
        .await
        .expect("create_offer should succeed");
        assert_eq!(ep.offer_generation, 0, "the initial offer is generation 0");

        // Simulate the initial answer clearing the pending offer.
        ep.pending_offer = None;
        let (_o1, g1) = ep.ice_restart().expect("first ice_restart");
        assert_eq!(g1, 1, "first ICE restart is generation 1");
        assert_eq!(ep.offer_generation, 1);

        // Simulate that restart being answered, then restart again.
        ep.pending_offer = None;
        let (_o2, g2) = ep.ice_restart().expect("second ice_restart");
        assert_eq!(g2, 2, "generation is monotonic across restarts");
    }

    #[test]
    fn ice_state_str_maps_all_variants() {
        assert_eq!(ice_state_str(IceConnectionState::New), "new");
        assert_eq!(ice_state_str(IceConnectionState::Checking), "checking");
        assert_eq!(ice_state_str(IceConnectionState::Connected), "connected");
        assert_eq!(ice_state_str(IceConnectionState::Completed), "completed");
        assert_eq!(
            ice_state_str(IceConnectionState::Disconnected),
            "disconnected"
        );
    }

    /// A fresh endpoint has no ICE state and a zeroed wire-level counter; once a
    /// state is stored, the `Endpoint` accessors surface it (lowercased) for
    /// the WebRTC variant.
    #[tokio::test]
    async fn ice_state_and_raw_recv_surface_through_endpoint_enum() {
        use crate::session::endpoint_enum::Endpoint;

        let id = uuid::Uuid::new_v4();
        let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let metrics = Arc::new(Metrics::new());

        let mut ep =
            WebRtcEndpoint::create_offer(id, EndpointDirection::SendRecv, bind_addr, tx, metrics)
                .await
                .expect("create_offer should succeed")
                .0;

        assert!(
            ep.ice_connection_state.is_none(),
            "no ICE transition has happened yet"
        );
        // Simulate str0m reporting ICE consent loss.
        ep.ice_connection_state = Some(IceConnectionState::Disconnected);

        let wrapped = Endpoint::WebRtc(Box::new(ep));
        assert_eq!(wrapped.ice_state(), Some("disconnected"));
        // The wire-level counters exist for WebRTC and start at zero.
        assert_eq!(wrapped.raw_recv_packets(), Some(0));
        assert_eq!(wrapped.raw_recv_bytes(), Some(0));
    }
}
