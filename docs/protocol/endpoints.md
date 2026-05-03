# Endpoints

Endpoint commands are grouped by transport:
- `endpoint.webrtc.*` for WebRTC offer/answer and ICE operations
- `endpoint.rtp.*` for plain RTP/SRTP signaling and re-INVITE flows
- generic `endpoint.*` for cross-transport lifecycle operations

## WebRTC Commands

### endpoint.webrtc.create_from_offer

Create a WebRTC endpoint from a remote WebRTC SDP offer.

```json
{
  "id": "1",
  "method": "endpoint.webrtc.create_from_offer",
  "params": {
    "sdp": "v=0\r\no=...",
    "direction": "sendrecv"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `sdp` | string | required | Remote SDP offer |
| `direction` | string | `"sendrecv"` | `"sendrecv"`, `"recvonly"`, or `"sendonly"` |

**Response:**
```json
{"id":"1","result":{"endpoint_id":"...","sdp_answer":"v=0\r\no=..."}}
```

### endpoint.webrtc.create_offer

Create a new WebRTC endpoint and generate an SDP offer to send to the remote peer.

```json
{
  "id": "2",
  "method": "endpoint.webrtc.create_offer",
  "params": {
    "direction": "sendrecv"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `direction` | string | `"sendrecv"` | Endpoint direction |

**Response:**
```json
{"id":"2","result":{"endpoint_id":"...","sdp_offer":"v=0\r\no=..."}}
```

### endpoint.webrtc.accept_answer

Accept a remote SDP answer for an endpoint created with `endpoint.webrtc.create_offer`.

```json
{
  "id": "3w",
  "method": "endpoint.webrtc.accept_answer",
  "params": {
    "endpoint_id": "...",
    "sdp": "v=0\r\no=..."
  }
}
```

**Response:**
```json
{"id":"3w","result":{}}
```

### endpoint.webrtc.accept_offer

Accept a remote SDP offer for an existing WebRTC endpoint and return an SDP answer.
Use this for remote-initiated re-negotiation, including remote ICE restarts.

```json
{
  "id": "3b",
  "method": "endpoint.webrtc.accept_offer",
  "params": {
    "endpoint_id": "...",
    "sdp": "v=0\r\no=..."
  }
}
```

**Response:**
```json
{"id":"3b","result":{"sdp_answer":"v=0\r\no=..."}}
```

### endpoint.webrtc.ice_restart

Perform an ICE restart on a WebRTC endpoint. Returns a new SDP offer with fresh ICE credentials. Deliver this to the remote peer and feed back their answer via `endpoint.webrtc.accept_answer`.

```json
{"id":"5","method":"endpoint.webrtc.ice_restart","params":{"endpoint_id":"..."}}
```

**Response:**
```json
{"id":"5","result":{"sdp_offer":"v=0\r\no=..."}}
```

#### ICE Restart Workflow

1. Detect degradation via `endpoint.state_changed` (`disconnected`)
2. Call `endpoint.webrtc.ice_restart`
3. Send returned offer to peer via signaling
4. Receive peer answer and submit via `endpoint.webrtc.accept_answer`
5. Monitor for `endpoint.state_changed` back to `connected`

```json
// Step 2
{"id":"5","method":"endpoint.webrtc.ice_restart","params":{"endpoint_id":"ep-abc"}}
{"id":"5","result":{"sdp_offer":"v=0\r\no=..."}}

// Step 4
{"id":"6","method":"endpoint.webrtc.accept_answer","params":{"endpoint_id":"ep-abc","sdp":"v=0\r\n..."}}
{"id":"6","result":{}}
```

**Failure scenarios:**
- If the remote peer is unreachable, ICE will time out and the endpoint remains `disconnected`
- If the endpoint was removed, the request returns `ENDPOINT_ERROR`
- Multiple rapid ICE restarts are safe; each generates fresh credentials

## RTP Commands

### endpoint.rtp.create_from_offer

Create a plain RTP endpoint from a remote RTP/SRTP SDP offer.

```json
{
  "id": "1r",
  "method": "endpoint.rtp.create_from_offer",
  "params": {
    "sdp": "v=0\r\no=...",
    "direction": "sendrecv"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `sdp` | string | required | Remote SDP offer |
| `direction` | string | `"sendrecv"` | `"sendrecv"`, `"recvonly"`, or `"sendonly"` |

**Response:**
```json
{"id":"1r","result":{"endpoint_id":"...","sdp_answer":"v=0\r\no=..."}}
```

### endpoint.rtp.create_offer

Create a new plain RTP endpoint and generate an SDP offer.

```json
{
  "id": "2r",
  "method": "endpoint.rtp.create_offer",
  "params": {
    "direction": "sendrecv",
    "srtp": false,
    "codecs": ["pcmu", "opus"]
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `direction` | string | `"sendrecv"` | Endpoint direction |
| `srtp` | bool | `false` | Include `a=crypto` in offer |
| `codecs` | string[] | all | Preferred codec order |

**Response:**
```json
{"id":"2r","result":{"endpoint_id":"...","sdp_offer":"v=0\r\no=..."}}
```

### endpoint.rtp.accept_answer

Accept a remote SDP answer for an endpoint created with `endpoint.rtp.create_offer`.

```json
{
  "id": "3r",
  "method": "endpoint.rtp.accept_answer",
  "params": {
    "endpoint_id": "...",
    "sdp": "v=0\r\no=..."
  }
}
```

**Response:**
```json
{"id":"3r","result":{}}
```

### endpoint.rtp.reinvite

Update an RTP endpoint from a re-INVITE SDP body without touching codec state. Use this for hold/unhold flows where the peer may send a different codec list/PT mapping; applying that SDP through `endpoint.rtp.accept_answer` can corrupt codec/PT state.

```json
{
  "id": "5",
  "method": "endpoint.rtp.reinvite",
  "params": {
    "endpoint_id": "...",
    "sdp": "v=0\r\no=..."
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string | required | Endpoint to update |
| `sdp` | string | required | Re-INVITE SDP body |

**Response:**
```json
{"id":"5","result":{"sdp_answer":"v=0\r\no=rtpbridge ..."}}
```

**What it updates:**
- Remote RTP/RTCP addresses (`rtcp-mux` and `a=rtcp:` aware)
- SRTP RX key (if changed) with 5-second dual-context transition
- SRTP/SRTCP RX replay/sequence state (reset for resumed streams)
- Remote SSRC tracker (relearned from next inbound packet)

**What it does NOT update:**
- `codecs`
- `send_codec`
- `telephone_event_pt`
- Outbound SRTP/SRTCP state

Only supported on RTP endpoints.

### endpoint.rtp.srtp_rekey

Initiate an SDES SRTP rekey on a plain RTP endpoint. Not applicable to WebRTC endpoints.

```json
{"id":"6","method":"endpoint.rtp.srtp_rekey","params":{"endpoint_id":"..."}}
```

**Response:**
```json
{"id":"6","result":{"sdp":"v=0\r\no=..."}}
```

## Generic Endpoint Commands

### endpoint.update_direction

Change endpoint direction policy in the routing table.

- Explicit directions (`sendrecv`, `sendonly`, `recvonly`, `inactive`) set a manual override.
- `auto` clears the manual override.

```json
{
  "id": "4",
  "method": "endpoint.update_direction",
  "params": {
    "endpoint_id": "...",
    "direction": "auto"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string | required | Endpoint to update |
| `direction` | string | required | `"auto"`, `"sendrecv"`, `"sendonly"`, `"recvonly"`, or `"inactive"` |

**Response:**
```json
{"id":"4","result":{}}
```

Notes:
- `auto` clears manual override for RTP, WebRTC, and Bridge endpoints.
- On RTP endpoints, `auto` resumes following SDP direction from initial offer/answer and `endpoint.rtp.reinvite`.
- On WebRTC and Bridge endpoints, `auto` restores the endpoint's baseline direction.

### endpoint.remove

Remove an endpoint from the session.

```json
{"id":"7","method":"endpoint.remove","params":{"endpoint_id":"..."}}
```

**Response:**
```json
{"id":"7","result":{}}
```

### endpoint.transfer

Transfer an endpoint from the current session to a different session. The endpoint keeps its connection (sockets, ICE, DTLS, SRTP state). Active recordings on the endpoint are stopped. File endpoints cannot be transferred.

```json
{
  "id": "8",
  "method": "endpoint.transfer",
  "params": {
    "endpoint_id": "...",
    "target_session_id": "..."
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string | required | Endpoint to transfer |
| `target_session_id` | string | required | Destination session UUID |

**Response:**
```json
{"id":"8","result":{"endpoint_id":"...","target_session_id":"..."}}
```

**Events:**
- Source session receives `endpoint.transferred_out`
- Target session receives `endpoint.transferred_in`

If the target session is at capacity (`max_endpoints_per_session`), the transfer fails and the endpoint is rolled back to the source session.

**Error codes:**
- `NO_SESSION` — no session bound
- `INVALID_PARAMS` — self-transfer or file endpoint
- `SESSION_NOT_FOUND` — target session doesn't exist
- `ENDPOINT_ERROR` — endpoint not found or extraction failed
- `TRANSFER_FAILED` — insertion into target failed (endpoint rolled back)

## Endpoint State Transitions

Endpoint state is reported in `endpoint.state_changed` events and session detail queries.

| Endpoint Type | States | Description |
|--------------|--------|-------------|
| RTP | `new` → `connected` → `disconnected` | Transitions to `connected` on first received packet |
| WebRTC | `new` → `connecting` → `connected` → `disconnected` | ICE/DTLS handshake phases |
| File (local) | `playing` → `paused` → `playing` → `finished` | Controlled via pause/resume commands |
| File (URL) | `buffering` → `playing` → `paused` → `playing` → `finished` | `buffering` until download completes |
| Tone | `playing` → `finished` | Auto-finish after optional duration |
| Bridge | `new` → `connected` | Virtual wiring endpoint for cross-session bridge |
