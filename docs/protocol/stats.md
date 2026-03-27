# Statistics

## stats.subscribe

Subscribe to periodic session statistics.

```json
{"id":"1","method":"stats.subscribe","params":{"interval_ms":5000}}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `interval_ms` | u32 | `5000` | Emission interval in milliseconds (min: 500, max: 3600000) |

## stats.unsubscribe

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
        "codec": "",
        "state": "connected"
      }
    ]
  }
}
```

### Field Notes

| Field | Notes |
|-------|-------|
| `rtt_ms` | Round-trip time from RTCP. `null` until an RTCP Receiver Report referencing the bridge's Sender Report has been received. This applies to both plain RTP and WebRTC endpoints. |
| `codec` | Negotiated codec name (e.g., `"opus"`, `"PCMU"`). Empty string `""` means no codec has been negotiated yet. |
| `state` | One of: `new`, `buffering`, `connecting`, `connected`, `playing`, `paused`, `disconnected`, `finished`. |
