//! Per-source playout / re-pacing buffers.
//!
//! The media pipeline is otherwise arrival-clocked: a source's packet arrival timing becomes
//! the media timeline. That is wrong for two kinds of source:
//!
//! 1. **Clockless** sources (WebSocket PCM, cross-session Bridge) have no real-time clock and
//!    arrive in producer-paced bursts. [`SynthClock`] makes rtpbridge the clock master: it
//!    owns the output RTP timeline, paces one 20 ms frame per grid tick, silence-fills short
//!    underflows, and collapses long gaps DTX-style with a talkspurt marker.
//! 2. **Real network** sources (RTP, WebRTC) have a real sender clock but the network adds
//!    jitter / reorder / loss. [`TrackedClock`] reorders by extended sequence number, paces
//!    playout against the sender's RTP timestamp, and drops late packets — *only* where
//!    rtpbridge stops being a transparent relay (mixer / transcode / analysis / plain-RTP
//!    egress). Transparent same-codec WebRTC↔WebRTC paths are left unbuffered (`Policy::Bypass`).
//!
//! All engaged buffers are drained on a single shared 20 ms grid (see the `mix_grid` handling
//! in `media_session`), so every source feeding a given mixer becomes due on the same tick.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use crate::control::protocol::EndpointId;

use super::endpoint::RoutedRtpPacket;

/// Internal L16 payload type (matches Bridge / WS endpoints).
const L16_PT: u8 = 127;
/// One media frame is 20 ms.
pub const FRAME: Duration = Duration::from_millis(20);

// ── Tunables (frames unless noted) ──────────────────────────────────────────
/// Synth prebuffer / nominal cushion floor (~60 ms). The cushion is adaptive (see
/// [`SYNTH_MAX_TARGET_FRAMES`]); this is the starting depth and the relaxed floor.
const SYNTH_TARGET_FRAMES: usize = 3;
/// Adaptive prebuffer ceiling (~140 ms). The Synth cushion grows one frame toward this each
/// talkspurt that underruns mid-spurt (a too-shallow buffer for that producer's burstiness) and
/// relaxes one frame per clean talkspurt. Mirrors `TrackedClock`'s adaptive `target_delay`.
const SYNTH_MAX_TARGET_FRAMES: usize = 7;
/// Consecutive silence-fill ticks before a Synth source goes idle/DTX (~200 ms).
const SYNTH_IDLE_TICKS: u32 = 10;
/// Synth overflow cap (~280 ms); drop-oldest beyond this. Kept clear of
/// [`SYNTH_MAX_TARGET_FRAMES`] so a grown cushion isn't immediately trimmed by a normal burst.
const SYNTH_MAX_FRAMES: usize = 14;
/// Tracked reorder-only target delay (~40 ms) for transcode / RTP-egress / analysis paths.
const TRACKED_SHALLOW_MS: u64 = 40;
/// Tracked mixer-fed target delay (~60 ms).
const TRACKED_MIXER_TARGET_MS: u64 = 60;
/// Tracked mixer-fed adaptive cap (~200 ms).
const TRACKED_MIXER_MAX_MS: u64 = 200;
/// Paced (mixer-fed) overflow cap in frames beyond which we drop-oldest (latency bound).
const TRACKED_MAX_FRAMES: usize = 16;
/// Reorder-mode overflow cap. Generous (a pure memory bound, not latency): the grid fires
/// once per 20 ms, so a sub-tick burst accumulates here before a single multi-drain forwards
/// it. Sized to hold a large burst; only a pathological flood is dropped.
const REORDER_MAX_FRAMES: usize = 256;
/// Reorder-mode hold: release the head across a sequence gap once this many packets pile up
/// behind it (bounds reorder tolerance without wall-clock pacing).
const REORDER_DEPTH: usize = 3;
/// A wall-clock arrival gap longer than this starts a new talkspurt: the paced anchor is reset
/// so a resumed stream isn't scheduled against a stale (pre-silence) reference.
const TRACKED_GAP_RESET: Duration = Duration::from_millis(200);

/// Which clock model a source's buffer uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayoutKind {
    /// Clockless source (WS / Bridge): rtpbridge owns the timeline.
    Synth,
    /// Real network source (RTP / WebRTC): track the sender clock, reorder + pace.
    Tracked,
}

/// Engagement decision for a source, recomputed on routing/tap changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// No buffering — the source is a transparent relay (downstream de-jitters).
    Bypass,
    /// Buffer this source with the given clock model.
    Engaged(PlayoutKind),
}

/// Counters surfaced to metrics. Cumulative; the session samples deltas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayoutCounters {
    pub overflow_drops: u64,
    pub underflow_fills: u64,
    pub late_drops: u64,
}

/// A per-source playout buffer.
pub enum PlayoutBuffer {
    Synth(SynthClock),
    Tracked(TrackedClock),
}

impl PlayoutBuffer {
    /// Build a Synth buffer for a clockless source. `clock_rate` = the source's RTP clock
    /// (= wire sample rate for WS, 48 kHz for Bridge); the 20 ms frame is `clock_rate/50`
    /// samples of 16-bit LE silence.
    pub fn synth(source_id: EndpointId, clock_rate: u32, ssrc: u32, seq: u16, ts: u32) -> Self {
        let samples = (clock_rate / 50) as usize;
        PlayoutBuffer::Synth(SynthClock {
            source_id,
            seq,
            ts,
            ssrc,
            ts_step: clock_rate / 50,
            silence: vec![0u8; samples * 2],
            queue: VecDeque::new(),
            state: SynthState::Idle,
            pending_marker: false,
            prebuffer_ticks: 0,
            underflow_ticks: 0,
            target_frames: SYNTH_TARGET_FRAMES,
            underran_this_spurt: false,
            counters: PlayoutCounters::default(),
        })
    }

    /// Build a Tracked buffer for a real network source.
    pub fn tracked(source_id: EndpointId, clock_rate: u32, mixer_fed: bool) -> Self {
        let target_ms = if mixer_fed {
            TRACKED_MIXER_TARGET_MS
        } else {
            TRACKED_SHALLOW_MS
        };
        PlayoutBuffer::Tracked(TrackedClock {
            source_id,
            clock_rate: clock_rate.max(1),
            ssrc: None,
            roc: 0,
            max_seq: 0,
            seen_any: false,
            last_emitted_ext: None,
            last_arrival: None,
            anchor: None,
            target_delay: Duration::from_millis(target_ms),
            max_delay: Duration::from_millis(if mixer_fed {
                TRACKED_MIXER_MAX_MS
            } else {
                TRACKED_SHALLOW_MS
            }),
            mixer_fed,
            max_frames: if mixer_fed {
                TRACKED_MAX_FRAMES
            } else {
                REORDER_MAX_FRAMES
            },
            queue: BTreeMap::new(),
            pending_marker: true, // mark the first emitted packet of the stream
            underflow_run: 0,
            counters: PlayoutCounters::default(),
        })
    }

    pub fn kind(&self) -> PlayoutKind {
        match self {
            PlayoutBuffer::Synth(_) => PlayoutKind::Synth,
            PlayoutBuffer::Tracked(_) => PlayoutKind::Tracked,
        }
    }

    /// Enqueue an arrived audio frame (telephone-event already split off upstream).
    pub fn push(&mut self, pkt: RoutedRtpPacket, arrival: Instant) {
        match self {
            PlayoutBuffer::Synth(s) => s.push(pkt.payload),
            PlayoutBuffer::Tracked(t) => t.push(pkt, arrival),
        }
    }

    /// At a grid tick, emit at most one 20 ms frame (None = not due / underflow / idle).
    pub fn drain_tick(&mut self, grid_now: Instant) -> Option<RoutedRtpPacket> {
        match self {
            PlayoutBuffer::Synth(s) => s.drain_tick(),
            PlayoutBuffer::Tracked(t) => t.drain_tick(grid_now),
        }
    }

    /// Whether the shared grid should keep ticking for this buffer.
    pub fn has_pending(&self) -> bool {
        match self {
            PlayoutBuffer::Synth(s) => s.state != SynthState::Idle,
            PlayoutBuffer::Tracked(t) => !t.queue.is_empty(),
        }
    }

    /// Whether the buffer may release more than one frame per grid tick. True only for the
    /// reorder-only (non-mixer) Tracked mode, which forwards bursts in order without pacing —
    /// the downstream endpoint does the playout. Synth and mixer-fed Tracked stay one-per-tick
    /// (the latter to keep a mixer's sources frame-aligned).
    pub fn drains_burst(&self) -> bool {
        matches!(self, PlayoutBuffer::Tracked(t) if !t.mixer_fed)
    }

    /// Whether this is a mixer-fed (paced) Tracked buffer. Used to detect a shallow↔deep mode
    /// change so the buffer is rebuilt (Synth is never mixer-fed).
    pub fn is_mixer_fed(&self) -> bool {
        matches!(self, PlayoutBuffer::Tracked(t) if t.mixer_fed)
    }

    /// Take and reset the cumulative counters' delta since the last call.
    pub fn take_counters(&mut self) -> PlayoutCounters {
        let c = match self {
            PlayoutBuffer::Synth(s) => &mut s.counters,
            PlayoutBuffer::Tracked(t) => &mut t.counters,
        };
        std::mem::take(c)
    }
}

// ── Synth (clockless: WS / Bridge) ──────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SynthState {
    /// Not emitting; waiting for the first frame of a (new) talkspurt.
    Idle,
    /// Accumulating the prebuffer cushion; not yet emitting.
    Prebuffering,
    /// Emitting one frame per grid tick (real audio or silence).
    Active,
}

/// Clock-master buffer for a source with no real-time clock. Owns the output RTP timeline,
/// paces one frame per grid tick, silence-fills short gaps, and goes DTX-idle on long gaps
/// (collapsing the dead air with a contiguous timestamp + talkspurt marker on resume).
pub struct SynthClock {
    source_id: EndpointId,
    seq: u16,
    ts: u32,
    ssrc: u32,
    ts_step: u32,
    silence: Vec<u8>,
    queue: VecDeque<Vec<u8>>,
    state: SynthState,
    /// Set the marker bit on the next emitted frame (start of a talkspurt).
    pending_marker: bool,
    prebuffer_ticks: u32,
    underflow_ticks: u32,
    /// Adaptive prebuffer/cushion depth (frames). Floor [`SYNTH_TARGET_FRAMES`], grows toward
    /// [`SYNTH_MAX_TARGET_FRAMES`] when a producer underruns mid-talkspurt, relaxes on clean
    /// talkspurts. The average producer rate is assumed correct (clockless source), so a deeper
    /// cushion absorbs burst jitter without a standing deficit.
    target_frames: usize,
    /// Whether the current talkspurt underran mid-spurt (recovered from a silence-fill). Drives
    /// both "grow `target_frames` at most once per spurt" and "don't relax a spurt that needed the
    /// depth" — set even when already at the ceiling, so a still-underrunning spurt isn't misread
    /// as clean and relaxed back down.
    underran_this_spurt: bool,
    counters: PlayoutCounters,
}

impl SynthClock {
    fn push(&mut self, payload: Vec<u8>) {
        if self.state == SynthState::Idle {
            self.state = SynthState::Prebuffering;
            self.prebuffer_ticks = 0;
            self.pending_marker = true;
            self.underran_this_spurt = false;
        }
        self.queue.push_back(payload);
        // Overflow: bound latency by dropping the oldest queued audio.
        while self.queue.len() > SYNTH_MAX_FRAMES {
            self.queue.pop_front();
            self.counters.overflow_drops += 1;
        }
        if self.state == SynthState::Prebuffering && self.queue.len() >= self.target_frames {
            self.state = SynthState::Active;
        }
    }

    fn drain_tick(&mut self) -> Option<RoutedRtpPacket> {
        match self.state {
            SynthState::Idle => None,
            SynthState::Prebuffering => {
                // Leave prebuffer once the cushion is built or we've waited long enough
                // (a short utterance must not stall forever).
                self.prebuffer_ticks += 1;
                if self.queue.len() >= self.target_frames
                    || self.prebuffer_ticks >= self.target_frames as u32
                {
                    self.state = SynthState::Active;
                    self.emit_active()
                } else {
                    None
                }
            }
            SynthState::Active => self.emit_active(),
        }
    }

    fn emit_active(&mut self) -> Option<RoutedRtpPacket> {
        if let Some(payload) = self.queue.pop_front() {
            // Real audio resumed while `underflow_ticks > 0` ⇒ we just rode out a *mid-talkspurt*
            // gap with silence — the cushion was too shallow for this producer's burstiness. Mark
            // the spurt non-clean (so it won't relax — even at the ceiling, where it can't grow)
            // and grow the prebuffer target once per spurt, capped; the deeper target takes effect
            // when the next talkspurt re-prebuffers. (End-of-talkspurt silence never reaches here —
            // the queue stays empty through to idle — so this only fires on recover-from-gap.)
            if self.underflow_ticks > 0 {
                if !self.underran_this_spurt && self.target_frames < SYNTH_MAX_TARGET_FRAMES {
                    self.target_frames += 1;
                }
                self.underran_this_spurt = true;
            }
            self.underflow_ticks = 0;
            Some(self.emit(payload, false))
        } else {
            // Underflow: silence-fill to ride out producer jitter, then DTX-idle.
            self.underflow_ticks += 1;
            if self.underflow_ticks > SYNTH_IDLE_TICKS {
                // Talkspurt ended cleanly (never underran mid-spurt) → relax one step toward the
                // floor so a well-paced producer doesn't carry a deep cushion forever.
                if !self.underran_this_spurt && self.target_frames > SYNTH_TARGET_FRAMES {
                    self.target_frames -= 1;
                }
                self.state = SynthState::Idle;
                self.underflow_ticks = 0;
                return None;
            }
            self.counters.underflow_fills += 1;
            let silence = self.silence.clone();
            Some(self.emit(silence, true))
        }
    }

    /// Stamp one frame, advancing the contiguous monotonic timeline. `is_silence` suppresses
    /// the talkspurt marker (only the first *real* frame of a spurt is marked).
    fn emit(&mut self, payload: Vec<u8>, is_silence: bool) -> RoutedRtpPacket {
        let marker = self.pending_marker && !is_silence;
        if marker {
            self.pending_marker = false;
        }
        let pkt = RoutedRtpPacket {
            source_endpoint_id: self.source_id,
            payload_type: L16_PT,
            sequence_number: self.seq,
            timestamp: self.ts,
            ssrc: self.ssrc,
            marker,
            payload,
        };
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(self.ts_step);
        pkt
    }
}

// ── Tracked (real network: RTP / WebRTC) ────────────────────────────────────

/// Jitter/reorder buffer for a source with a real sender clock. Reorders by extended sequence
/// number, paces playout against the sender's RTP timestamp anchored to local time, and drops
/// late packets. Resets on SSRC change. Output preserves the source's seq/ts/ssrc/pt so a
/// transparent WebRTC egress still forwards real values.
pub struct TrackedClock {
    source_id: EndpointId,
    clock_rate: u32,
    ssrc: Option<u32>,
    // Extended-sequence state (16-bit seq + rollover counter).
    roc: u32,
    max_seq: u16,
    seen_any: bool,
    last_emitted_ext: Option<u64>,
    /// Wall-clock arrival of the previous push, to detect a talkspurt gap and re-anchor.
    last_arrival: Option<Instant>,
    // Playout anchor: (sender ts at anchor, local play time of that ts).
    anchor: Option<(u32, Instant)>,
    target_delay: Duration,
    max_delay: Duration,
    /// Mixer-fed: wall-clock-paced one-per-tick playout (deep, adaptive). Otherwise
    /// reorder-only: forward bursts in order, no pacing (the downstream endpoint plays out).
    mixer_fed: bool,
    /// Overflow cap (frames); mode-dependent (see [`TRACKED_MAX_FRAMES`]/[`REORDER_MAX_FRAMES`]).
    max_frames: usize,
    queue: BTreeMap<u64, RoutedRtpPacket>,
    pending_marker: bool,
    underflow_run: u32,
    counters: PlayoutCounters,
}

impl TrackedClock {
    fn reset(&mut self, ssrc: u32) {
        self.ssrc = Some(ssrc);
        self.roc = 0;
        self.seen_any = false;
        self.last_emitted_ext = None;
        self.last_arrival = None;
        self.anchor = None;
        self.queue.clear();
        self.pending_marker = true;
        self.underflow_run = 0;
    }

    /// Extend a 16-bit sequence number to a monotonic 64-bit value, tracking rollover.
    /// Does not mutate state for reordered (older) packets. Returns `None` when the packet
    /// predates the stream's first epoch (a reorder that would underflow the rollover counter)
    /// — the caller drops it as late.
    fn extend_seq(&mut self, seq: u16) -> Option<u64> {
        if !self.seen_any {
            self.seen_any = true;
            self.max_seq = seq;
            return Some(((self.roc as u64) << 16) | seq as u64);
        }
        let forward = seq.wrapping_sub(self.max_seq); // 0..=0x7fff = forward/equal
        if forward < 0x8000 {
            if seq < self.max_seq {
                self.roc = self.roc.wrapping_add(1); // wrapped past 0xffff
            }
            self.max_seq = seq;
            Some(((self.roc as u64) << 16) | seq as u64)
        } else if seq > self.max_seq {
            // Reordered/older, wrapped back into the previous 16-bit epoch.
            if self.roc == 0 {
                None // predates epoch 0 → genuinely old, drop as late
            } else {
                Some((((self.roc - 1) as u64) << 16) | seq as u64)
            }
        } else {
            // Reordered/older within the current epoch.
            Some(((self.roc as u64) << 16) | seq as u64)
        }
    }

    fn push(&mut self, pkt: RoutedRtpPacket, arrival: Instant) {
        match self.ssrc {
            Some(cur) if cur == pkt.ssrc => {}
            Some(_) => self.reset(pkt.ssrc),
            None => self.ssrc = Some(pkt.ssrc),
        }

        // Talkspurt gap: a long arrival silence *while the buffer is drained* means the stream
        // paused; drop the stale paced anchor so the resumed stream re-anchors to now (and
        // re-marks). Gated on an empty queue so a mid-burst inter-arrival gap (the buffer is
        // still draining) doesn't strand queued frames behind a cleared anchor. Harmless in
        // reorder mode (no anchor), where it just re-marks the resume.
        if self.queue.is_empty()
            && let Some(last) = self.last_arrival
            && arrival.saturating_duration_since(last) > TRACKED_GAP_RESET
        {
            self.anchor = None;
            self.pending_marker = true;
            self.underflow_run = 0;
        }
        self.last_arrival = Some(arrival);

        let Some(ext) = self.extend_seq(pkt.sequence_number) else {
            self.counters.late_drops += 1; // predates the stream → too old
            return;
        };
        if let Some(last) = self.last_emitted_ext
            && ext <= last
        {
            self.counters.late_drops += 1; // already played past this point
            return;
        }

        if self.anchor.is_none() {
            self.anchor = Some((pkt.timestamp, arrival + self.target_delay));
        }
        self.queue.insert(ext, pkt);

        // Overflow: bound depth by shedding the oldest queued packets.
        let mut dropped = false;
        while self.queue.len() > self.max_frames {
            if let Some((&oldest, _)) = self.queue.iter().next() {
                self.queue.remove(&oldest);
                self.last_emitted_ext = Some(oldest); // don't re-accept it
                self.counters.overflow_drops += 1;
                dropped = true;
            } else {
                break;
            }
        }
        // Fast-forward: re-anchor the surviving head to play within `target_delay` of now,
        // otherwise the dropped span would leave the retained frames scheduled far in the
        // future (their play-time is relative to the stale anchor).
        if dropped && let Some((_, head)) = self.queue.iter().next() {
            self.anchor = Some((head.timestamp, arrival + self.target_delay));
        }
    }

    /// Local play time for a sender timestamp, relative to the anchor. Uses a *signed* RTP
    /// timestamp delta so a packet behind the anchor (e.g. a reorder that arrived before its
    /// predecessor, which set the anchor) plays in the past and is due immediately.
    fn play_time(&self, ts: u32, anchor_ts: u32, anchor_play: Instant) -> Instant {
        let fwd = ts.wrapping_sub(anchor_ts);
        if fwd < 0x8000_0000 {
            let micros = (fwd as u64).saturating_mul(1_000_000) / self.clock_rate as u64;
            anchor_play + Duration::from_micros(micros)
        } else {
            let back = anchor_ts.wrapping_sub(ts);
            let micros = (back as u64).saturating_mul(1_000_000) / self.clock_rate as u64;
            anchor_play
                .checked_sub(Duration::from_micros(micros))
                .unwrap_or(anchor_play)
        }
    }

    fn drain_tick(&mut self, grid_now: Instant) -> Option<RoutedRtpPacket> {
        if self.mixer_fed {
            self.drain_paced(grid_now)
        } else {
            self.drain_reorder()
        }
    }

    /// Mixer-fed: emit at most one frame, when its sender-clock play time has arrived. Drops a
    /// head that is stale beyond a full buffer depth (event-loop/network stall — playing it
    /// would only add latency). Grows the target delay on repeated underflow (Phase 5).
    fn drain_paced(&mut self, grid_now: Instant) -> Option<RoutedRtpPacket> {
        let (anchor_ts, anchor_play) = self.anchor?;
        loop {
            let (head_ext, head_ts) = match self.queue.iter().next() {
                Some((&ext, pkt)) => (ext, pkt.timestamp),
                None => {
                    self.underflow_run += 1;
                    if self.underflow_run >= 3 && self.target_delay < self.max_delay {
                        self.target_delay = (self.target_delay + FRAME).min(self.max_delay);
                        self.underflow_run = 0;
                    }
                    return None;
                }
            };
            let play = self.play_time(head_ts, anchor_ts, anchor_play);
            if play > grid_now {
                return None; // not due yet
            }
            if grid_now.saturating_duration_since(play) > self.max_delay {
                // Stale: skip it and try the next, catching up instead of replaying old audio.
                self.queue.remove(&head_ext);
                self.last_emitted_ext = Some(head_ext);
                self.counters.late_drops += 1;
                continue;
            }
            self.underflow_run = 0;
            return Some(self.take_head(head_ext));
        }
    }

    /// Reorder-only: release the next in-order packet — the contiguous successor of the last
    /// emitted, or (across a sequence gap) the head once `REORDER_DEPTH` packets have piled up
    /// behind it. No wall-clock pacing: called repeatedly per tick, it forwards a whole burst.
    fn drain_reorder(&mut self) -> Option<RoutedRtpPacket> {
        let head_ext = *self.queue.keys().next()?;
        let releasable = match self.last_emitted_ext {
            // Stream start: release the lowest seq immediately (1:1 cadence, no startup hold).
            None => true,
            // In stream: the contiguous successor, or — across a gap — force-release the head
            // once the reorder window has filled behind it.
            Some(last) => head_ext == last.wrapping_add(1) || self.queue.len() >= REORDER_DEPTH,
        };
        releasable.then(|| self.take_head(head_ext))
    }

    /// Pop the given head, advance the emitted marker, and stamp the source id.
    fn take_head(&mut self, ext: u64) -> RoutedRtpPacket {
        let mut pkt = self.queue.remove(&ext).expect("head present");
        self.last_emitted_ext = Some(ext);
        if self.pending_marker {
            pkt.marker = true;
            self.pending_marker = false;
        }
        pkt.source_endpoint_id = self.source_id;
        pkt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn raw(payload: Vec<u8>) -> RoutedRtpPacket {
        RoutedRtpPacket {
            source_endpoint_id: Uuid::nil(),
            payload_type: L16_PT,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0,
            marker: false,
            payload,
        }
    }

    fn rtp(seq: u16, ts: u32, ssrc: u32) -> RoutedRtpPacket {
        RoutedRtpPacket {
            source_endpoint_id: Uuid::nil(),
            payload_type: 0,
            sequence_number: seq,
            timestamp: ts,
            ssrc,
            marker: false,
            payload: vec![1, 2, 3, 4],
        }
    }

    // ── Synth ──────────────────────────────────────────────────────────────

    #[test]
    fn synth_prebuffers_then_emits_one_per_tick() {
        let mut b = PlayoutBuffer::synth(Uuid::new_v4(), 8000, 100, 0, 0);
        // First two pushes: still prebuffering (target = 3), no emit.
        b.push(raw(vec![0u8; 320]), Instant::now());
        assert!(b.drain_tick(Instant::now()).is_none());
        b.push(raw(vec![0u8; 320]), Instant::now());
        assert!(b.drain_tick(Instant::now()).is_none());
        // Third push reaches target → Active; from here one frame per tick.
        b.push(raw(vec![0u8; 320]), Instant::now());
        let p0 = b.drain_tick(Instant::now()).expect("active emit");
        assert!(p0.marker, "first real frame of a talkspurt is marked");
        assert_eq!(p0.payload_type, L16_PT);
        let p1 = b.drain_tick(Instant::now()).expect("emit");
        assert_eq!(p1.sequence_number, p0.sequence_number.wrapping_add(1));
        assert_eq!(p1.timestamp, p0.timestamp.wrapping_add(160)); // 8000/50
        assert!(!p1.marker);
    }

    #[test]
    fn synth_prebuffer_times_out_for_short_utterance() {
        let mut b = PlayoutBuffer::synth(Uuid::new_v4(), 8000, 1, 0, 0);
        b.push(raw(vec![0u8; 320]), Instant::now());
        // Only one frame, but after SYNTH_TARGET_FRAMES ticks it must emit, not stall.
        let mut emitted = 0;
        for _ in 0..SYNTH_TARGET_FRAMES {
            if b.drain_tick(Instant::now()).is_some() {
                emitted += 1;
            }
        }
        assert!(emitted >= 1, "short utterance must drain, not stall");
    }

    #[test]
    fn synth_silence_fills_then_idles() {
        let mut b = PlayoutBuffer::synth(Uuid::new_v4(), 8000, 1, 0, 0);
        for _ in 0..SYNTH_TARGET_FRAMES {
            b.push(raw(vec![7u8; 320]), Instant::now());
        }
        // Drain the real frames.
        for _ in 0..SYNTH_TARGET_FRAMES {
            assert!(b.drain_tick(Instant::now()).is_some());
        }
        // Then silence-fill for up to SYNTH_IDLE_TICKS, then stop (Idle → None).
        let mut silence = 0;
        for _ in 0..(SYNTH_IDLE_TICKS + 2) {
            match b.drain_tick(Instant::now()) {
                Some(p) => {
                    assert!(!p.marker);
                    silence += 1;
                }
                None => break,
            }
        }
        assert_eq!(silence, SYNTH_IDLE_TICKS);
        assert!(!b.has_pending(), "idle after the DTX threshold");
    }

    #[test]
    fn synth_resume_after_idle_marks_new_talkspurt_with_contiguous_ts() {
        let mut b = PlayoutBuffer::synth(Uuid::new_v4(), 8000, 1, 0, 0);
        for _ in 0..SYNTH_TARGET_FRAMES {
            b.push(raw(vec![7u8; 320]), Instant::now());
        }
        let mut last_ts = 0u32;
        for _ in 0..SYNTH_TARGET_FRAMES {
            last_ts = b.drain_tick(Instant::now()).unwrap().timestamp;
        }
        // Drive to idle.
        while b.drain_tick(Instant::now()).is_some() {
            last_ts = last_ts.wrapping_add(160);
        }
        // New utterance → new talkspurt: marker set, timestamp contiguous (gap collapsed).
        b.push(raw(vec![9u8; 320]), Instant::now());
        let mut p = None;
        for _ in 0..SYNTH_TARGET_FRAMES {
            p = b.drain_tick(Instant::now());
            if p.is_some() {
                break;
            }
        }
        let p = p.expect("resumed emit");
        assert!(p.marker, "resume marks a new talkspurt");
        assert_eq!(
            p.timestamp,
            last_ts.wrapping_add(160),
            "timeline stays contiguous"
        );
    }

    #[test]
    fn synth_overflow_drops_oldest() {
        let mut b = PlayoutBuffer::synth(Uuid::new_v4(), 8000, 1, 0, 0);
        for i in 0..(SYNTH_MAX_FRAMES + 5) {
            b.push(raw(vec![i as u8; 320]), Instant::now());
        }
        let c = b.take_counters();
        assert_eq!(c.overflow_drops, 5);
    }

    #[test]
    fn synth_prebuffer_grows_on_midspurt_underrun_then_relaxes() {
        let target = |b: &PlayoutBuffer| match b {
            PlayoutBuffer::Synth(s) => s.target_frames,
            _ => unreachable!(),
        };
        let now = Instant::now();
        let mut b = PlayoutBuffer::synth(Uuid::new_v4(), 8000, 1, 0, 0);
        assert_eq!(target(&b), SYNTH_TARGET_FRAMES);

        // Build the cushion, go Active, drain it empty.
        for _ in 0..SYNTH_TARGET_FRAMES {
            b.push(raw(vec![1u8; 320]), now);
        }
        for _ in 0..SYNTH_TARGET_FRAMES {
            assert!(b.drain_tick(now).is_some());
        }
        // Mid-spurt gap: one silence-fill, then real audio resumes → cushion grows by one.
        assert!(!b.drain_tick(now).expect("silence fill").marker);
        b.push(raw(vec![2u8; 320]), now);
        assert!(b.drain_tick(now).is_some());
        assert_eq!(
            target(&b),
            SYNTH_TARGET_FRAMES + 1,
            "a mid-talkspurt underrun deepens the cushion"
        );

        // This spurt grew, so ending it must NOT relax.
        while b.drain_tick(now).is_some() {}
        assert!(!b.has_pending());
        assert_eq!(target(&b), SYNTH_TARGET_FRAMES + 1);

        // A fully clean talkspurt (no mid-spurt underrun) relaxes one step toward the floor.
        let deeper = SYNTH_TARGET_FRAMES + 1;
        for _ in 0..deeper {
            b.push(raw(vec![3u8; 320]), now);
        }
        while b.drain_tick(now).is_some() {} // drain reals, silence-fill to idle (clean → relax)
        assert_eq!(
            target(&b),
            SYNTH_TARGET_FRAMES,
            "a clean talkspurt relaxes the cushion back toward the floor"
        );
    }

    #[test]
    fn synth_prebuffer_pins_at_ceiling_under_sustained_underrun() {
        let target = |b: &PlayoutBuffer| match b {
            PlayoutBuffer::Synth(s) => s.target_frames,
            _ => unreachable!(),
        };
        let now = Instant::now();
        let mut b = PlayoutBuffer::synth(Uuid::new_v4(), 8000, 1, 0, 0);

        // One talkspurt that underruns mid-spurt (silence-fill), recovers, then idles.
        let bursty_spurt = |b: &mut PlayoutBuffer| {
            let t = target(b);
            for _ in 0..t {
                b.push(raw(vec![1u8; 320]), now);
            }
            for _ in 0..t {
                assert!(b.drain_tick(now).is_some());
            }
            assert!(b.drain_tick(now).is_some()); // mid-spurt underrun → silence
            b.push(raw(vec![2u8; 320]), now);
            assert!(b.drain_tick(now).is_some()); // recovery
            while b.drain_tick(now).is_some() {} // drain to idle
        };

        // Sustained burstiness drives the cushion to the ceiling (3 → 7 over four spurts).
        for _ in 0..4 {
            bursty_spurt(&mut b);
        }
        assert_eq!(target(&b), SYNTH_MAX_TARGET_FRAMES);

        // Further spurts that still underrun *at the ceiling* must stay pinned — not be
        // misclassified as clean and relaxed (the cap-state regression).
        for _ in 0..4 {
            bursty_spurt(&mut b);
            assert_eq!(
                target(&b),
                SYNTH_MAX_TARGET_FRAMES,
                "an underrun at the ceiling must not relax the cushion"
            );
        }
    }

    // ── Tracked: reorder mode (shallow, non-mixer) ──────────────────────────

    /// Drain a reorder-mode buffer the way the grid does: release the whole burst this tick.
    fn drain_burst(b: &mut PlayoutBuffer, grid: Instant) -> Vec<u16> {
        let mut out = Vec::new();
        while let Some(p) = b.drain_tick(grid) {
            out.push(p.sequence_number);
            if !b.drains_burst() {
                break;
            }
        }
        out
    }

    #[test]
    fn reorder_forwards_burst_in_order_without_pacing() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, false);
        assert!(b.drains_burst());
        let t0 = Instant::now();
        // A whole burst arrives at once; reorder mode forwards all of it, in order, one tick.
        for seq in 0u16..100 {
            b.push(rtp(seq, seq as u32 * 160, 42), t0);
        }
        let out = drain_burst(&mut b, t0);
        assert_eq!(
            out.len(),
            100,
            "reorder mode forwards the whole burst, not a paced subset"
        );
        assert!(out.windows(2).all(|w| w[0] < w[1]), "in order");
    }

    #[test]
    fn reorder_sorts_out_of_order_arrivals() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, false);
        let t0 = Instant::now();
        // Out of order within the reorder window (depth 3).
        b.push(rtp(2, 320, 42), t0);
        b.push(rtp(0, 0, 42), t0);
        b.push(rtp(1, 160, 42), t0);
        assert_eq!(drain_burst(&mut b, t0), vec![0, 1, 2]);
    }

    #[test]
    fn reorder_drops_late_packet() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, false);
        let t0 = Instant::now();
        for seq in 5u16..8 {
            b.push(rtp(seq, seq as u32 * 160, 42), t0);
        }
        assert_eq!(drain_burst(&mut b, t0), vec![5, 6, 7]);
        // seq 4 arrives after 5..7 already played → dropped.
        b.push(rtp(4, 640, 42), t0);
        assert!(drain_burst(&mut b, t0).is_empty());
        assert_eq!(b.take_counters().late_drops, 1);
    }

    #[test]
    fn reorder_resets_on_ssrc_change() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, false);
        let t0 = Instant::now();
        for seq in 100u16..103 {
            b.push(rtp(seq, seq as u32 * 160, 42), t0);
        }
        assert_eq!(drain_burst(&mut b, t0), vec![100, 101, 102]);
        // New SSRC: seq space resets; low seqs must not be treated as "late".
        for seq in 0u16..3 {
            b.push(rtp(seq, seq as u32 * 160, 99), t0);
        }
        let out = drain_burst(&mut b, t0);
        assert_eq!(out, vec![0, 1, 2]);
    }

    // ── Tracked: paced mode (deep, mixer-fed) ───────────────────────────────

    #[test]
    fn paced_releases_by_timestamp_one_per_tick() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, true);
        assert!(!b.drains_burst());
        let t0 = Instant::now();
        b.push(rtp(0, 0, 42), t0);
        b.push(rtp(1, 160, 42), t0);
        // At anchor time only the first is due.
        let at_anchor = t0 + Duration::from_millis(TRACKED_MIXER_TARGET_MS);
        assert_eq!(b.drain_tick(at_anchor).unwrap().sequence_number, 0);
        assert!(b.drain_tick(at_anchor).is_none(), "second not due yet");
        // 20 ms later the second becomes due.
        assert_eq!(b.drain_tick(at_anchor + FRAME).unwrap().sequence_number, 1);
    }

    #[test]
    fn paced_overflow_fast_forwards_so_survivors_play_soon() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, true);
        let t0 = Instant::now();
        // A burst far larger than the cap arrives at once (faster than real time).
        for i in 0..(TRACKED_MAX_FRAMES as u32 + 30) {
            b.push(rtp(i as u16, i * 160, 7), t0);
        }
        assert_eq!(b.take_counters().overflow_drops, 30);
        // The retained head must be due ~target_delay after the burst, not seconds later.
        let due = t0 + Duration::from_millis(TRACKED_MIXER_TARGET_MS) + FRAME;
        assert!(
            b.drain_tick(due).is_some(),
            "survivors must play within target_delay after overflow, not be stranded"
        );
    }

    #[test]
    fn paced_reanchors_after_talkspurt_gap() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, true);
        let t0 = Instant::now();
        // A short early spurt (e.g. NAT-latch packets) sets the anchor.
        b.push(rtp(0, 0, 42), t0);
        assert!(
            b.drain_tick(t0 + Duration::from_millis(TRACKED_MIXER_TARGET_MS))
                .is_some()
        );
        // Long silence, then the real stream resumes. Without re-anchoring, these frames would
        // be scheduled against the stale anchor and stale-dropped, delivering nothing.
        let resume = t0 + Duration::from_secs(1);
        b.push(rtp(1, 160, 42), resume);
        let due = resume + Duration::from_millis(TRACKED_MIXER_TARGET_MS) + FRAME;
        let p = b
            .drain_tick(due)
            .expect("resumed stream must play, not be stale-dropped");
        assert_eq!(p.sequence_number, 1);
        assert!(p.marker, "resume after a gap re-marks the talkspurt");
        assert_eq!(
            b.take_counters().late_drops,
            0,
            "no frames dropped on resume"
        );
    }

    #[test]
    fn paced_gap_during_draining_burst_does_not_strand_queue() {
        let mut b = PlayoutBuffer::tracked(Uuid::new_v4(), 8000, true);
        let t0 = Instant::now();
        for seq in 0u16..8 {
            b.push(rtp(seq, seq as u32 * 160, 42), t0);
        }
        // A late arrival (>gap threshold) while the queue is still non-empty must NOT clear the
        // anchor — otherwise drain_paced returns None forever while has_pending stays true (a
        // grid busy-loop). The buffer must still drain to empty (by emit and/or stale-drop).
        let late = t0 + Duration::from_secs(1);
        b.push(rtp(8, 8 * 160, 42), late);
        let mut grid = late;
        for _ in 0..100 {
            grid += FRAME;
            b.drain_tick(grid);
            if !b.has_pending() {
                break;
            }
        }
        assert!(
            !b.has_pending(),
            "buffer must drain to empty after a mid-burst gap, not busy-loop on a cleared anchor"
        );
    }

    #[test]
    fn extend_seq_handles_wrap() {
        let mut t = match PlayoutBuffer::tracked(Uuid::new_v4(), 8000, false) {
            PlayoutBuffer::Tracked(t) => t,
            _ => unreachable!(),
        };
        assert_eq!(t.extend_seq(65534), Some(65534));
        assert_eq!(t.extend_seq(65535), Some(65535));
        assert_eq!(t.extend_seq(0), Some(65536)); // rolled over
        assert_eq!(t.extend_seq(1), Some(65537));
        // A reordered older packet from the previous epoch extends below the rollover.
        assert_eq!(t.extend_seq(65535), Some(65535));
    }

    #[test]
    fn extend_seq_drops_packet_predating_first_epoch() {
        let mut t = match PlayoutBuffer::tracked(Uuid::new_v4(), 8000, false) {
            PlayoutBuffer::Tracked(t) => t,
            _ => unreachable!(),
        };
        // Stream starts at seq 0 (roc 0). A reordered older seq 65535 has no prior epoch to
        // extend into — it must be dropped, not treated as a far-future packet.
        assert_eq!(t.extend_seq(0), Some(0));
        assert_eq!(t.extend_seq(65535), None);
    }
}
