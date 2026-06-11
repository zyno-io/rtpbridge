# DTMF

DTMF uses telephone-event RTP packets (RFC 4733). WebRTC endpoints use payload type 101, and plain RTP/SRTP endpoints use the negotiated telephone-event payload type when present. File, tone, bridge, and WebSocket audio endpoints do not have a telephone-event payload type. DTMF packets are **never transcoded** — they bypass the audio transcode pipeline and are forwarded directly between compatible RTP/WebRTC endpoints, with payload type remapping if needed.

## Detection

When a remote endpoint sends DTMF via telephone-event RTP packets, rtpbridge automatically detects the completed digit and emits a `dtmf` event:

```json
{
  "event": "dtmf",
  "data": {
    "endpoint_id": "...",
    "digit": "5",
    "duration_ms": 160
  }
}
```

Supported digits: `0`-`9`, `*`, `#`, `A`-`D`.

## Injection

### endpoint.dtmf.inject

Send a DTMF digit into an endpoint's outbound stream. The endpoint must have a telephone-event payload type, so this is supported for WebRTC and plain RTP/SRTP endpoints.

```json
{
  "id": "1",
  "method": "endpoint.dtmf.inject",
  "params": {
    "endpoint_id": "...",
    "digit": "5",
    "duration_ms": 160,
    "volume": 10
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string | required | Target endpoint |
| `digit` | string | required | `"0"`-`"9"`, `"*"`, `"#"`, `"A"`-`"D"` |
| `duration_ms` | u32 | `160` | Duration in milliseconds (max 10,000) |
| `volume` | u8 | `10` | Volume in dBm0 (0-63, RFC 4733 6-bit field) |

The digit is sent as RFC 4733 telephone-event RTP packets with proper start/continuation/end signaling (3x redundant end packets per the RFC).

## Forwarding

DTMF packets received from one endpoint are forwarded to compatible destinations in the session according to the routing table. If the source and destination endpoints negotiated different payload types for telephone-event, the PT is remapped automatically.
