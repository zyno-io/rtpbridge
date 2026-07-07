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

## stats.unsubscribe

Unsubscribe from periodic statistics for the currently bound session.

```json
{"id":"2","method":"stats.unsubscribe","params":{}}
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
          "bytes": 240000
        },
        "rtt_ms": null,
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
| `inbound.raw_packets` / `inbound.raw_bytes` | Diagnostic fields, present only when `include_diagnostics` is true. Wire-level counters: every datagram the endpoint's UDP socket(s) received, *before* any demux or parse — STUN/ICE bindings, DTLS, RTCP, RTP, and malformed junk. Present only for socket-backed endpoints (WebRTC, plain RTP); omitted for file/tone/bridge/websocket. Always `>=` the media-plane `packets`/`bytes`. Use the gap to detect a remote network failure: if `raw_packets` keeps climbing while `packets` is flat, the peer's path is alive but it has stopped sending media (silence/DTX); if **both** are flat, the path itself is dead. For WebRTC, STUN consent keepalives keep `raw_packets` moving during media silence; for plain RTP, RTCP does. |
| `inbound.raw_rtp_*` | Diagnostic fields, present only when `include_diagnostics` is true. RTP-looking datagram counters captured before endpoint media processing. For WebRTC, these are encrypted SRTP datagrams classified from the RTP header before str0m; for plain RTP/SRTP, they are datagrams on the RTP side before parse/decrypt. `raw_rtp_packets_lost`, `raw_rtp_sequence_gaps`, `raw_rtp_duplicate_packets`, and `raw_rtp_out_of_order_packets` separate upstream packet loss/reordering from bridge processing loss. If raw RTP is continuous but `inbound.packets_lost` rises, the loss was added after socket ingress. |
| `inbound.recv_loop_gap_ms` / `max_recv_loop_gap_ms` | Diagnostic fields, present only when `include_diagnostics` is true. Time between consecutive socket receive-loop datagrams for that endpoint. Large values indicate the endpoint recv task was not receiving packets on schedule, or the peer stopped sending. |
| `inbound.enqueue_wait_ms` / `max_enqueue_wait_ms` | Diagnostic fields, present only when `include_diagnostics` is true. Time spent waiting to enqueue a received datagram into the session task's bounded packet channel. Non-zero sustained values indicate session-channel backpressure. |
| `inbound.dequeue_delay_ms` / `max_dequeue_delay_ms` | Diagnostic fields, present only when `include_diagnostics` is true. Time between socket receive and session-task processing. Large values indicate the session task was delayed before it could process queued datagrams. |
| `inbound.channel_capacity` / `min_channel_capacity` / `channel_overflows` | Diagnostic fields, present only when `include_diagnostics` is true. Session packet-channel headroom observed by receive tasks. `channel_overflows` increments when WebRTC ingress drops a datagram because the session channel is full. |
| `rtt_ms` | Round-trip time from RTCP for plain RTP/SRTP endpoints. `null` until an RTCP Receiver Report referencing the bridge's Sender Report has been received. Currently `null` for WebRTC and non-RTP endpoints. |
| `codec` | Negotiated codec name (e.g., `"opus"`, `"PCMU"`). Empty string `""` means no codec has been negotiated yet. |
| `state` | One of: `new`, `buffering`, `connecting`, `connected`, `playing`, `paused`, `disconnected`, `finished`. |
| `local_rtp_addr` / `remote_rtp_addr` | Current socket addresses for socket-backed endpoints. For WebRTC, these identify the selected/nominated local candidate base and peer address once str0m has transmitted on the selected path. Use them to confirm an ICE restart actually moved media to the expected candidate path. |
| `offer_generation` | WebRTC-only monotonic ICE-restart offer generation. `0` is the initial offer; each `endpoint.webrtc.ice_restart` increments it. Correlate this with bridge/client restart attempts and selected-path changes. |
| `ice_state` | str0m ICE connection state for **WebRTC** endpoints: `new`, `checking`, `connected`, `completed`, `disconnected`. Omitted for non-WebRTC endpoints and before the first ICE transition. `disconnected` is ICE consent loss (RFC 7675) — a remote network-path failure. Also surfaced live via the [`endpoint.ice_state_changed`](./events.md) event. |
