# WebSocket audio endpoint

A `websocket` endpoint streams raw PCM audio to/from an external peer over a
WebSocket, instead of negotiating RTP/SRTP via SDP. Internally it behaves like a
bridge endpoint (L16, payload type 127) but at the wire `sample_rate` rather than a
fixed 48 kHz, so it participates in routing, transcoding, and multi-party mixing
exactly like any other endpoint. Running at the wire rate means the session's
per-edge resampling converts directly to each peer (e.g. an 8 kHz PCMU leg goes
16 kHz↔8 kHz, not 16→48→8) — no unnecessary 48 kHz detour.

## Lifecycle

1. **Create (control plane).** Call `endpoint.create_websocket` on a session. The
   endpoint is created in the `connecting` state (not yet routed) and a single-use
   `connect_token` is returned.
2. **Dial in (audio plane).** The audio peer opens a WebSocket to
   `ws://<host>:<port>/audio/<connect_token>` on the same port as the control/HTTP
   server. On success the endpoint transitions to `connected`, joins the routing
   table, and an `endpoint.ws.connected` event is emitted.
3. **Stream.** Binary WebSocket frames carry raw audio (see Wire format). Audio
   routes to/from other endpoints in the session per the endpoint's direction.
4. **Disconnect.** When the socket closes, the endpoint moves to `disconnected`,
   leaves the routing table, and `endpoint.ws.disconnected` is emitted. There is no
   automatic reconnect — create a new endpoint to reconnect.

## `endpoint.create_websocket`

Request params:

| field         | type   | default    | notes                                                        |
|---------------|--------|------------|--------------------------------------------------------------|
| `direction`   | string | `sendrecv` | `sendrecv` / `recvonly` / `sendonly` / `inactive` (SDP sense)|
| `sample_rate` | number | `8000`     | wire PCM rate in Hz: `8000`, `16000`, or `48000`             |
| `flush_ms`    | number | `0`        | outbound coalescing window, multiple of 20 (0 = passthrough) |

Result:

```json
{ "endpoint_id": "<uuid>", "connect_token": "<uuid>" }
```

The client constructs the audio URL itself as `/audio/<connect_token>`.

Direction is from rtpbridge's perspective, matching plain RTP/WebRTC endpoints:

- `sendonly` — the endpoint is a **source only**: the peer's audio enters rtpbridge
  (other endpoints hear the WS peer).
- `recvonly` — the endpoint is a **destination only**: rtpbridge sends other
  endpoints' audio to the WS peer.
- `sendrecv` — both.

## Wire format

Binary WebSocket frames only. Each frame is **raw little-endian 16-bit mono PCM**
at the negotiated `sample_rate`. Frames may be any length; rtpbridge reframes the
inbound stream to 20 ms internally (a trailing partial sample is buffered until the
next frame). Text frames are ignored; Ping is answered with Pong; Close ends the
session.

- **Inbound** (peer → rtpbridge): carried as L16 at `sample_rate` (no resampling at
  the socket) and routed to destinations (resampled/transcoded to their codecs as
  needed). rtpbridge synthesizes a monotonic RTP timeline so downstream RTP/WebRTC
  peers see advancing timestamps.
- **Outbound** (rtpbridge → peer): each source is transcoded to the WS endpoint's
  L16 stream at `sample_rate` and written as binary frames. Output is
  **source-clocked**: a frame is produced for each 20 ms of routed audio (no
  synthetic silence is sent when sources are idle). With `flush_ms > 0`, that many
  milliseconds of audio are coalesced into a single WebSocket message
  (e.g. `flush_ms: 100` at 8 kHz → 1600-byte messages).

## Events

| event                        | data             | when                                       |
|------------------------------|------------------|--------------------------------------------|
| `endpoint.ws.connected`      | `{ endpoint_id }`| audio socket attached                      |
| `endpoint.ws.disconnected`   | `{ endpoint_id }`| audio socket closed / errored              |
| `endpoint.ws.connect_timeout`| `{ endpoint_id }`| no audio socket dialed in within 30 s; the endpoint is auto-removed |

## Notes & limits

- The `connect_token` is a single-use secret; reusing it (or presenting an unknown
  or malformed token) closes the audio socket with a 1008 (policy) close.
- A created endpoint that is never dialed into is auto-removed after 30 s
  (`endpoint.ws.connect_timeout`), reclaiming its endpoint slot and token.
- WebSocket endpoints cannot be transferred between sessions (`endpoint.transfer`).
- Backpressure: if the peer can't keep up, the newest outbound frame is dropped
  rather than blocking the session (same policy as bridge endpoints).
