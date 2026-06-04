# Plan: ingress playout / re-pacing buffering

> Revision 2. Incorporates Codex round-1 review: DTMF split before buffering, shared
> mixer grid + flush ordering, one-frame-per-source-per-tick guarantee, policy recompute on
> tap changes, explicit `TrackedClock` SSRC/wrap spec.

## Problem recap

The whole media pipeline is **arrival-clocked**, not wall-clock-clocked. Every receive
source's arrival timing becomes the media timeline:

- WS PCM frames are emitted on parse (`endpoint_websocket.rs:301`) with a frame-counted,
  not wall-clock, synthesized timeline (`endpoint_websocket.rs:140-154`).
- The select loop batch-drains up to 64 queued packets per iteration
  (`media_session.rs:2366-2399`) and `poll_and_route` writes each to its destination
  immediately (`media_session.rs:2964-2971`).
- The mixer is arrival-clocked — `flush_frame` fires when a source contributes a *second*
  time (`mixer.rs:118-124`), no wall-clock timer.

Two distinct problems wear the name "jitter buffer":

1. **Clockless sources** (WS, Bridge): no real-time clock, no network (WS = in-order TCP,
   Bridge = in-process channel; zero loss/reorder). Need a **re-pacing / clock-master**
   buffer that imposes a 20 ms wall clock.
2. **Real network sources** (RTP, WebRTC from cellular): sender *has* a clock, RTP
   timestamps are meaningful; the *network* adds jitter/reorder/loss. Need **reorder +
   pace + drop-late**, and (for the mixer) adaptive depth.

A blanket per-endpoint jitter buffer is wrong: rtpbridge is a relay, and stacking a buffer
in front of an endpoint that already has its own playout buffer just adds latency. The
buffer must go **only where rtpbridge stops being transparent** — where it decodes,
re-originates sequence numbers, or consumes on its own clock.

### Where rtpbridge is NOT transparent (verified)

| Path | Why it needs in-order / paced input |
|---|---|
| Mixer | Arrival-clocked consumer; *is* the playout clock for each source (`mixer.rs:118`). |
| Transcode | Decodes per-packet in arrival order (`media_session.rs:2926`); reorder corrupts codec state. |
| VAD / fax taps | Stateful decoders need the stream in order (`audio_analysis.rs:78`, `process_analysis` at `media_session.rs:2813`). |
| Plain RTP egress | Re-originates its own seq (`endpoint_rtp.rs:1124,1131`) + destination-owned ts (`advance_outbound_timeline`), so downstream can't reorder what we linearized. |

### Where rtpbridge IS transparent (no buffer)

**WebRTC egress, same codec, no taps**: `write_rtp` passes the *source's*
`sequence_number`/`timestamp` straight to str0m (`endpoint_webrtc.rs:681-693`); the
downstream WebRTC receiver reorders and de-jitters. This is the common 1:1 cellular call —
it must stay zero-added-latency.

> **DTMF / telephone-event is never audio.** RFC 4733 telephone-event packets are detected
> out-of-band from their payload, not from decoded audio, and are forwarded by
> `process_dtmf_packets` (`session_dtmf.rs:115`). They must **bypass** the playout buffer
> entirely (event-duration timestamp semantics would break a timestamp-paced buffer). VAD
> and fax are the only audio-decoding taps that force buffering.

## Design: one per-source ingress buffer, two clock models

Insert a single **per-source playout buffer**, keyed by source `EndpointId`, at *ingress* —
between packet arrival and the assembly of `packets_to_route` in `poll_and_route`. It is
drained on a **shared 20 ms session grid** into `packets_to_route`; everything downstream of
that (recording, analysis, routing, mixer feed) is unchanged and sees clean, in-order, paced
input. The mixer becomes wall-clock-clocked as a consequence.

Two clock models behind one interface:

```
// src/session/playout.rs (new)

pub enum PlayoutBuffer {
    Synth(SynthClock),     // WS / Bridge — no usable source clock; owns the output timeline.
    Tracked(TrackedClock), // RTP / WebRTC — real source clock; reorder + pace + drop-late.
}

impl PlayoutBuffer {
    /// Enqueue an arrived AUDIO frame (telephone-event already split off). `arrival` = now.
    fn push(&mut self, pkt: RoutedRtpPacket, arrival: Instant);
    /// At a grid tick, emit AT MOST ONE 20 ms frame (None if not yet due / underflow).
    fn drain_tick(&mut self, grid_now: Instant) -> Option<RoutedRtpPacket>;
    fn next_due(&self) -> Option<Instant>;   // for the select-loop wakeup gate
    fn has_pending(&self) -> bool;
}
```

**Cadence is one frame per source per grid tick — never a burst.** Forward progress is
guaranteed; overflow never skips an emit:

- **Pacing**: at a grid tick a non-empty buffer always releases its single oldest queued
  frame, advancing the source's `next_due` by exactly 20 ms (grid-locked — `next_due` is
  always `prev_next_due + 20ms`, never re-anchored forward past the current tick).
- **Overflow**: *after* emitting this tick's frame, if the remaining queue still exceeds the
  buffer cap, drop from the **front** (oldest) down to the cap (`playout_overflow_drops`).
  This bounds latency without ever starving the current emission — a sustained fast producer
  loses old audio, not the live frame.
- **Loop starvation**: if `grid_now` is many ticks past the grid instant, the grid catches
  up but is clamped like the file/tone pollers (`tone_poll.rs:47-49`): re-sync the grid to
  `now - 20ms` rather than emitting a multi-frame catch-up burst.

**The buffer does only consumer-independent work:** reorder, pace, drop-late, bounded depth.
It does **not** decode and does **not** do PLC — concealment is a consumer concern (the mixer
sums only present sources → an absent source is silence; transcode/endpoint decoders conceal
loss). The one exception is `Synth`, which silence-fills (below), because a clockless source
has no real timeline for any downstream to fall back on.

### Shared 20 ms grid

A session-level `mix_grid: Option<Instant>` (next grid instant) advances by 20 ms. All
buffers' `drain_tick` are evaluated against the same `grid_now`, and `Tracked` snaps its
computed play-time up to the next grid instant, so all *buffered* sources feeding a mixer
become due on the *same* tick. Sources that are **not** grid-routed — file/tone generators,
and (until phase 4) arrival-fed RTP/WebRTC — still feed the mixer off-grid; the mixer's
retained implicit second-contribution flush absorbs those (see Mixer changes), with the
bounded catch-up residual noted there. The grid + `flush_tick` are what make the *buffered*
inputs and the mixer's emission cadence wall-clock-locked. When no buffer is engaged the grid
is `None` and nothing changes.

### `SynthClock` (WS / Bridge)

- Owns `seq`/`ts`/`ssrc`; advances exactly one 20 ms step per grid tick.
- Pre-buffers `synth_depth_ms` worth of frames before first emit (absorbs the burst).
- Each tick emits exactly one frame: real audio if queued, else **silence** (zeroed L16).
- **Talkspurt / long-gap — Synth owns the collapse.** Silence-fill short underflows
  (≤ depth) to ride out producer jitter; after `synth_idle_ms` of continuous underflow go
  **idle** (DTX) and stop emitting. The timeline is **contiguous across the gap**: on resume,
  the next frame's `ts = last_emitted_ts + 20ms step` (dead air collapsed, *not* a real-time
  gap) with the RTP **marker** set to signal the new talkspurt; `seq` continues monotonic;
  `ssrc` is stable. Synth must do this itself rather than relying on
  `advance_outbound_timeline`'s gap-collapse (`endpoint_rtp.rs:1018-1021`), because **WebRTC
  egress bypasses that path** — `write_rtp` passes the packet's `seq`/`ts`/`marker` straight
  to str0m (`endpoint_webrtc.rs:681-693`), so a real ts gap would surface as dead air at the
  far end. (For RTP egress, a contiguous +1-step ts means `advance_outbound_timeline` sees a
  normal step and the marker propagates — consistent either way. `ssrc` only matters for RTP
  egress/recording; `write_rtp` doesn't forward packet SSRC to str0m.)
- This replaces `WebSocketEndpoint::build_inbound_packet` and fixes the current bug where a
  long gap is silently collapsed into one 20 ms step with **no marker** (`endpoint_websocket.rs:151`).
- **Bridge fix**: Bridge inbound is `seq=0/ts=0/ssrc=0` every packet (`media_session.rs:2643`),
  and `advance_outbound_timeline` *holds* the wire ts when source ts doesn't advance
  (`endpoint_rtp.rs:1015-1017`). `SynthClock` gives Bridge a real monotonic timeline,
  removing that latent freeze.

### `TrackedClock` (RTP / WebRTC) — phase 4, spec'd now

Both inbound paths preserve the real `ssrc`/`sequence_number`/`timestamp`
(`endpoint_webrtc.rs:651`, `endpoint_rtp.rs:894`), so the buffer must handle real RTP
identifier semantics:

- **SSRC tracking.** The buffer is still stored per `EndpointId`, but it tracks the current
  SSRC and **resets on SSRC change** (str0m re-origination, RTP source change): flush/clear
  the queue, re-anchor the timestamp, set marker. (Seq/ts spaces reset while the EndpointId
  is stable, so a per-EndpointId buffer that ignored SSRC would corrupt ordering.)
- **Extended sequence**: maintain a rollover counter so the 16-bit `seq` orders correctly
  across wrap; emit in extended-seq order; drop any packet older than the last emitted.
- **Timestamp anchor with wrap**: `play_time(ts) = anchor + wrapping_delta(ts, ts0) /
  clock_rate`, using `u32` `wrapping_sub` for the delta and the endpoint's negotiated
  `clock_rate` (not assumed 8 k). Snap play-time up to the shared grid.
- **Drop-late**: a packet whose snapped play-time is already past, or whose extended seq is
  ≤ last emitted, is discarded (`playout_late_drops`).
- **Loss → gap**: missing seq = no frame that tick; no synthesis (mixer/decoder conceals).
- **Depth**: shallow (reorder-only, ~`tracked_shallow_ms`) for transcode/RTP-egress/taps;
  deep (target `tracked_mixer_target_ms`, cap `tracked_mixer_max_ms`) only for mixer-fed
  sources. Adaptive depth + drift tracking is a later refinement; v1 of phase 4 is fixed.

## Engagement / bypass policy

A `Policy` per source decides buffered-or-not and depth. Computed by one function
`recompute_playout_policy()` called from **both** `rebuild_routing`/`rebuild_mixers` **and**
VAD/fax start/stop handlers (taps mutate `vad_monitors`/`fax_detectors` without a routing
rebuild — a stale policy is the only way a bypassed source reaches a non-transparent
consumer).

```
fn policy(src) -> Policy {
    if src is WS or Bridge          => Engaged(Synth, depth = SYNTH)        // always
    if src is RTP or WebRTC {
        let taps  = vad OR fax active on src          // NOT dtmf/telephone-event
        let dests = routing.destinations(src)
        let mixed = any dest in dests is multi-source (routing.is_multi_source)
        let opaque = any dest needs transcode OR any dest is plain-RTP egress
        if !taps && !mixed && !opaque
            && every dest is WebRTC w/ same codec  => Bypass                // transparent 1:1
        if mixed                                   => Engaged(Tracked, deep)
        else                                       => Engaged(Tracked, shallow)
    }
}
```

Bypass keeps the common 1:1 cellular WebRTC↔WebRTC call zero-added-latency. Depth = the
deepest need across a source's destinations; a source feeding *both* a mixer and a
transparent passthrough takes conference latency on the passthrough too — an **intentional,
documented** cost (those rarely coexist), not an assumption that it can't matter.

## Data-flow changes in `poll_and_route`

Ingress classification must run **before** buffering so telephone-event packets bypass:

1. Poll file/tone endpoints → push **straight** to `packets_to_route` (already-paced
   generators; never buffered).
2. For each WebRTC `poll_output` RtpPacket (`media_session.rs:2743`): `classify_dtmf`
   (`media_session.rs:2744`) → telephone-event to `dtmf_packets`; audio to its buffer
   (engaged) or straight to `packets_to_route` (bypassed).
3. For each select-loop `inbound_rtp` (plain RTP / WS / Bridge): `classify_dtmf`
   (`media_session.rs:2703`) → telephone-event to `dtmf_packets`; audio to its buffer
   (engaged) or straight to `packets_to_route` (bypassed).
4. **Grid drain**: if `grid_now` reached, for every engaged buffer push
   `buf.drain_tick(grid_now)` (≤1 frame) into `packets_to_route`; advance `mix_grid += 20ms`.
5. Unchanged tail: `process_dtmf_packets` (out-of-band, today's path) → recording tap →
   analysis tap → route loop (transcode/passthrough/`mixer.feed`).
6. **Mixer grid flush** (only when a grid tick was processed this pass): after the route
   loop's `mixer.feed` calls (`media_session.rs:2852`) and **before** `mixer.drain()`
   (`media_session.rs:2995`), call `mixer.flush_tick()`.

### Mixer changes (purely additive — keep the existing flush)

- **Keep** the implicit second-contribution flush (`mixer.rs:118-124`). It is what safely
  handles sources that feed the mixer more than once per pass: file/tone catch-up bursts
  (`file_poll.rs:57`, `tone_poll.rs:43` emit multiple frames per poll) and — until phase 4 —
  arrival-fed RTP/WebRTC mixer inputs (batch-drain bursts). Removing it would smear those.
- **Add** `flush_tick()`: same body as `flush_frame` (zero `mix_buffer`, sum contributors,
  encode, advance `rtp_timestamp` by `clock_increment`, reset `contributed` flags), guarded
  by the existing `any_contributed` check (`mixer.rs:146`) so an all-idle conference emits
  **nothing**. `flush_tick` flushes the *once-per-tick* accumulation that the implicit path
  never triggers (a source contributing exactly once per grid tick). It is called only on a
  grid tick (step 6), never on sub-20 ms loop iterations, so it can't emit partial frames.
- The two compose cleanly: in steady state (one frame per source per tick) the implicit
  flush never fires and `flush_tick` emits one frame per tick; during bursts the implicit
  flush drains the surplus and `flush_tick` emits the tail. Both reset `contributed`, so no
  double-emit. **Residual**: under loop-starvation catch-up a multi-frame (off-grid) source
  can split a conference frame (a source's frame mixed without a late peer's), bounded by the
  poller/batch catch-up clamp — not necessarily exactly one tick, but it does **not** compound
  (missed time is re-anchored/clamped, never accumulated as backlog). Transient, only in an
  already-degraded condition — accepted for v1.
- **Timestamp values and monotonicity are unchanged** — only *when* a frame is emitted
  changes, not the `rtp_timestamp` sequence. The pre-existing WebRTC passthrough↔mixer
  timestamp discontinuity (mixer seeds via `continue_from_timestamp`, `None` for WebRTC dests
  because `endpoint_last_rtp_timestamp` returns `Some` only for `Endpoint::Rtp`,
  `endpoint_enum.rs:228`) is **orthogonal and out of scope**.

### Select-loop wakeup

The loop already caps `sleep_duration` to 20 ms when files/tones/DTMF are active
(`media_session.rs:2306-2321`). Extend that gate: if `mix_grid.is_some()` (any buffer
engaged), cap to the grid's next instant. No new timer task.

## Module / type changes

- **New** `src/session/playout.rs`: `PlayoutBuffer`, `SynthClock`, `TrackedClock`, `Policy`,
  depth constants. Unit-tested in isolation (no tokio).
- `MediaSession`: add `playout_buffers: HashMap<EndpointId, PlayoutBuffer>`, `playout_policy:
  HashMap<EndpointId, Policy>`, `mix_grid: Option<Instant>`. Policy (re)computed by
  `recompute_playout_policy()`; buffers removed on teardown next to the existing
  `analysis_decoders.remove` sites (`media_session.rs:1408,1858,2086`).
- `WebSocketEndpoint`: delete `build_inbound_packet` + `in_seq/in_ts/in_ssrc`; the IO task
  still reframes to 20 ms and sends `InboundPacket`s — `SynthClock` owns the timeline.
- `mixer.rs`: add `flush_tick` (grid-gated); **keep** `feed`'s implicit second-contribution
  flush (`mixer.rs:118-124`) as-is — the two compose (see Mixer changes).

## Tunables (config) + metrics

- `playout_synth_depth_ms` (~60), `playout_synth_idle_ms` (~200).
- `playout_tracked_shallow_ms` (~40), `playout_tracked_mixer_target_ms` (~60),
  `playout_tracked_mixer_max_ms` (~200).
- Metrics: `playout_late_drops`, `playout_overflow_drops`, `playout_underflow_fills`,
  `playout_depth_ms` gauge.

## Phasing

- **Phase 1 — Synth for WS** (the shippable cut for the reported problem): replace
  `build_inbound_packet`; grid drain; silence-fill + talkspurt marker + idle reset. No mixer
  or cellular *code* changes (a WS source may still feed the old mixer; see v1 note below).
- **Phase 2 — Synth for Bridge**: same buffer; fixes the `ts=0` freeze.
- **Phase 3 — Mixer grid clocking**: shared `mix_grid`, `flush_tick` ordering, one-frame
  guarantee, all-idle = emit-nothing. Makes the mixer wall-clock-clocked.
- **Phase 4 — Tracked for RTP/WebRTC** into mixer/transcode/taps: reorder + pace +
  drop-late, SSRC/wrap handling, engagement/bypass policy; transparent WebRTC↔WebRTC stays
  bypassed.
- **Phase 5 — Adaptive depth + drift** for the deep/mixer `TrackedClock`.

**v1 ships phases 1–2** (clockless re-pacing). This fully fixes the reported WS burst on all
**non-mixed** paths — the reported case. For a WS source in a **conference**, phases 1–2 are
safe but not the final fix: a paced Synth source feeds the still-arrival-clocked mixer, which
flushes on the Synth source's next (now-paced) contribution — so the mixer becomes
*effectively* paced by its paced input, but multi-source grid alignment and the
final-frame-until-next-feed lag are only cleaned up in **phase 3**. Phase 4 (Tracked) is
deliberately *not* bundled — its SSRC/wrap/drift edge cases warrant their own change.

## Resolved forks (from review)

1. **Ingress vs per-edge** → ingress, depth = deepest consumer; the passthrough-pays-mixer-
   latency case is an intentional documented cost.
2. **Synth long-gap** → collapse-with-marker (DTX); explicit `synth_idle_ms` threshold + reset.
3. **PLC ownership** → out of the buffer; mixer sums-present-only, decoders conceal.
4. **v1 scope** → phases 1–2 shippable; phase 3 after grid/flush fixes; Tracked deferred.
