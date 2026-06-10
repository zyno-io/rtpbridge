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
          "last_received_ms_ago": 20,
          "raw_packets": 1512,
          "raw_bytes": 241200
        },
        "outbound": {
          "packets": 1500,
          "bytes": 240000
        },
        "rtt_ms": null,
        "codec": "",
        "state": "connected",
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
| `inbound.raw_packets` / `inbound.raw_bytes` | **Wire-level** counters: every datagram the endpoint's UDP socket(s) received, *before* any demux or parse — STUN/ICE bindings, DTLS, RTCP, RTP, and malformed junk. Present only for socket-backed endpoints (WebRTC, plain RTP); omitted for file/tone/bridge/websocket. Always `>=` the media-plane `packets`/`bytes`. Use the gap to detect a remote network failure: if `raw_packets` keeps climbing while `packets` is flat, the peer's path is alive but it has stopped sending media (silence/DTX); if **both** are flat, the path itself is dead. For WebRTC, STUN consent keepalives keep `raw_packets` moving during media silence; for plain RTP, RTCP does. |
| `rtt_ms` | Round-trip time from RTCP. `null` until an RTCP Receiver Report referencing the bridge's Sender Report has been received. This applies to both plain RTP and WebRTC endpoints. |
| `codec` | Negotiated codec name (e.g., `"opus"`, `"PCMU"`). Empty string `""` means no codec has been negotiated yet. |
| `state` | One of: `new`, `buffering`, `connecting`, `connected`, `playing`, `paused`, `disconnected`, `finished`. |
| `ice_state` | str0m ICE connection state for **WebRTC** endpoints: `new`, `checking`, `connected`, `completed`, `disconnected`. Omitted for non-WebRTC endpoints and before the first ICE transition. `disconnected` is ICE consent loss (RFC 7675) — a remote network-path failure. Also surfaced live via the [`endpoint.ice_state_changed`](./events.md) event. |
