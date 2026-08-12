# Statistics

## stats.subscribe

Subscribe to periodic session statistics.

Requires a bound session (`session.create` or `session.attach`). Statistics and `stats` events are for the currently bound session only.

```json
{"id":"1","method":"stats.subscribe","params":{"interval_ms":5000}}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `interval_ms` | u32 | `5000` | Emission interval in milliseconds (min: 500, max: 3600000) |
| `include_diagnostics` | bool | `false` | Include raw socket/RTP sequence counters and receive-queue timing fields. Leave off for normal always-on polling; enable for investigations, soak tests, and loss classification. |

Calling `stats.subscribe` again while already subscribed changes the interval and
diagnostic verbosity without resetting the emit timeline. The next `stats` event
fires at `interval - time_since_last_emit`: if the new (shorter) interval has
already elapsed since the last emit, one is published immediately; otherwise the
next emit lands at the diff. This means re-subscribing in a tight loop will not
starve emission.

Periodic `stats` events are normal-priority, at-most-once observations. A slow
controller can therefore receive `events.dropped`. That notification is
connection-wide rather than stats-specific, so consumers must treat any
unexpected cadence or drop notification as an unknown coverage gap rather than
interpolating missing counter samples.

## stats.snapshot

Return one synchronous statistics snapshot for the currently bound session.
The result has the same `{ "endpoints": [...] }` shape as the `stats` event's
`data`. It does not subscribe, unsubscribe, or alter the periodic emit timeline.
Controllers should use this immediately before terminal session teardown so the
last partial polling interval is represented.

```json
{"id":"2","method":"stats.snapshot","params":{}}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `include_diagnostics` | bool | `false` | Include the same raw socket/RTP and receive-queue diagnostic fields as `stats.subscribe`; does not modify a periodic subscription's verbosity. |

Snapshot values are cumulative counters plus the latest observations. The
controller timestamps receipt; rtpbridge does not assign a wall-clock timestamp
to the result. It works whether or not periodic statistics are subscribed. The
result is the same endpoint-statistics object serialized in `stats.data` (not an
event envelope). A snapshot and a periodic event may race and contain identical
counters; consumers must ingest them serially and de-duplicate rather than
forming a zero-length interval.

The method is session-bound and uses the common `NO_SESSION`, `SESSION_GONE`,
and `SESSION_UNRESPONSIVE` errors. A controller rolling against an older bridge
may receive `UNKNOWN_METHOD` and must fall back to its last periodic observation.

## stats.unsubscribe

Unsubscribe from periodic statistics for the currently bound session.

```json
{"id":"3","method":"stats.unsubscribe","params":{}}
```

## Stats Event

```json
{
  "event": "stats",
  "data": {
    "endpoints": [
      {
        "endpoint_id": "...",
        "inbound": {
          "packets": 1500,
          "bytes": 240000,
          "packets_lost": 0,
          "jitter_ms": 0.0,
          "last_received_ms_ago": 20
        },
        "outbound": {
          "packets": 1500,
          "bytes": 240000,
          "remote_packets_lost": 2,
          "remote_highest_sequence": 1501,
          "remote_report_generation": 1,
          "remote_report_count": 12,
          "remote_jitter_ms": 3.4,
          "remote_report_received_ms_ago": 1100
        },
        "rtt_ms": 28.4,
        "rtt_observed_ms_ago": 1100,
        "rtt_source": "rtcp_receiver_report",
        "codec": "opus",
        "state": "connected",
        "local_rtp_addr": "127.0.0.1:40000",
        "remote_rtp_addr": "198.51.100.10:50000",
        "offer_generation": 2,
        "ice_state": "connected"
      }
    ]
  }
}
```

### Field Notes

| Field | Notes |
|-------|-------|
| `inbound.packets` / `inbound.bytes` | **Media-plane** counters: validated RTP media only — post-demux for WebRTC, post-parse/decrypt for plain RTP. Datagrams that fail to parse, plus all STUN/ICE, DTLS, and RTCP, are excluded. |
| `inbound.packets_lost` / `inbound.jitter_ms` | Bridge-observed ingress quality for media sent by the remote endpoint toward rtpbridge. These fields can include impairment between socket receipt and session processing; they do not describe media sent by rtpbridge toward the endpoint. `packets_lost` is cumulative but may decrease when late/reordered packets repair the receive sequence. Consumers must treat the first snapshot as a baseline and a negative counter delta as an unknown/corrected interval. |
| `inbound.raw_packets` / `inbound.raw_bytes` | Diagnostic fields, present only when `include_diagnostics` is true. Wire-level counters: every datagram the endpoint's UDP socket(s) received, *before* any demux or parse — STUN/ICE bindings, DTLS, RTCP, RTP, and malformed junk. Present only for socket-backed endpoints (WebRTC, plain RTP); omitted for file/tone/bridge/websocket. Always `>=` the media-plane `packets`/`bytes`. Use the gap to detect a remote network failure: if `raw_packets` keeps climbing while `packets` is flat, the peer's path is alive but it has stopped sending media (silence/DTX); if **both** are flat, the path itself is dead. For WebRTC, STUN consent keepalives keep `raw_packets` moving during media silence; for plain RTP, RTCP does. |
| `inbound.raw_rtp_*` | Diagnostic fields, present only when `include_diagnostics` is true. RTP-looking datagram counters captured before endpoint media processing. For WebRTC, these are encrypted SRTP datagrams classified from the RTP header before str0m; for plain RTP/SRTP, they are datagrams on the RTP side before parse/decrypt. `raw_rtp_packets_lost`, `raw_rtp_sequence_gaps`, `raw_rtp_duplicate_packets`, and `raw_rtp_out_of_order_packets` separate upstream packet loss/reordering from bridge processing loss. If raw RTP is continuous but `inbound.packets_lost` rises, the loss was added after socket ingress. |
| `inbound.recv_loop_gap_ms` / `max_recv_loop_gap_ms` | Diagnostic fields, present only when `include_diagnostics` is true. Time between consecutive socket receive-loop datagrams for that endpoint. Large values indicate the endpoint recv task was not receiving packets on schedule, or the peer stopped sending. |
| `inbound.enqueue_wait_ms` / `max_enqueue_wait_ms` | Diagnostic fields, present only when `include_diagnostics` is true. Time spent waiting to enqueue a received datagram into the session task's bounded packet channel. Non-zero sustained values indicate session-channel backpressure. |
| `inbound.dequeue_delay_ms` / `max_dequeue_delay_ms` | Diagnostic fields, present only when `include_diagnostics` is true. Time between socket receive and session-task processing. Large values indicate the session task was delayed before it could process queued datagrams. |
| `inbound.channel_capacity` / `min_channel_capacity` / `channel_overflows` | Diagnostic fields, present only when `include_diagnostics` is true. Session packet-channel headroom observed by receive tasks. `channel_overflows` increments when WebRTC ingress drops a datagram because the session channel is full. |
| `outbound.remote_packets_lost` / `outbound.remote_highest_sequence` | Latest accepted receiver-reported cumulative loss and extended-highest-sequence values for media rtpbridge sent toward the remote endpoint. Present only after the peer reports outbound-media reception (an RTCP Receiver Report for plain RTP/SRTP, or str0m's remote media-egress observation for WebRTC). Plain RTP cumulative loss preserves RFC 3550's signed 24-bit semantics and may decrease after late packets. Consumers calculate a loss interval only from non-negative deltas within one `remote_report_generation`, using `deltaLost / deltaHighestSequence`; a decrease/reset/reordered report re-baselines the interval. These values are not inferred from `outbound.packets`. |
| `outbound.remote_report_generation` / `outbound.remote_report_count` | Generation changes when rtpbridge's outbound sender SSRC changes, or when a plain-RTP outbound codec clock changes while a report is retained. For plain RTP/SRTP, report count increases for every accepted report. WebRTC counts reports when str0m exposes distinct loss, remote-quality, or egress-RTT evidence; its public stats API cannot distinguish an exactly duplicated RR from a re-serialized cached snapshot. Consumers never calculate a delta across generations and must not treat an unchanged WebRTC count as evidence that the peer did not send an identical RR. |
| `outbound.remote_jitter_ms` | Latest receiver-reported interarrival jitter for outbound media, converted from RTP timestamp units with that outbound stream's RTP clock (48 kHz for WebRTC Opus and the negotiated send codec clock for RTP/SRTP). It is re-baselined with the remote-report generation. |
| `outbound.remote_report_received_ms_ago` | Age of the receiver report that supplied the outbound remote-quality fields. Consumers must reject stale values rather than repeatedly counting a cached observation as fresh. |
| `rtt_ms` | Latest round-trip time observation. Plain RTP/SRTP derives it from an RTCP Receiver Report referencing the bridge's Sender Report; WebRTC derives it from fresh str0m media-egress receiver-report statistics. `null` until observed and for non-RTP endpoints. RTT is not one-way latency. |
| `rtt_observed_ms_ago` | Age of the observation that supplied `rtt_ms`. Consumers must use this freshness signal when calculating sampled min/average/max RTT. |
| `rtt_source` | `rtcp_receiver_report` for plain RTP/SRTP and `webrtc_media_egress` for WebRTC. Other str0m WebRTC RTT values are omitted because ICE-lite exposes no usable freshness marker for them. Consumers may combine values only as explicitly labelled RTT samples. |
| `codec` | Negotiated codec name (e.g., `"opus"`, `"PCMU"`). Empty string `""` means no codec has been negotiated yet. |
| `state` | One of: `new`, `buffering`, `connecting`, `connected`, `playing`, `paused`, `disconnected`, `finished`. |
| `local_rtp_addr` / `remote_rtp_addr` | Current socket addresses for socket-backed endpoints. For WebRTC, these identify the selected/nominated local candidate base and peer address once str0m has transmitted on the selected path. Use them to confirm an ICE restart actually moved media to the expected candidate path. |
| `offer_generation` | WebRTC-only monotonic ICE-restart offer generation. `0` is the initial offer; each `endpoint.webrtc.ice_restart` increments it. Correlate this with bridge/client restart attempts and selected-path changes. |
| `ice_state` | str0m ICE connection state for **WebRTC** endpoints: `new`, `checking`, `connected`, `completed`, `disconnected`. Omitted for non-WebRTC endpoints and before the first ICE transition. `disconnected` is ICE consent loss (RFC 7675) — a remote network-path failure. Also surfaced live via the [`endpoint.ice_state_changed`](./events.md) event. |
