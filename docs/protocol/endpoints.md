# Endpoints

## endpoint.create_from_offer

Create an endpoint from a remote SDP offer. Automatically detects WebRTC (ICE/DTLS present) vs plain RTP from the SDP content. SRTP is detected from `a=crypto` lines.

```json
{
  "id": "1",
  "method": "endpoint.create_from_offer",
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

## endpoint.create_offer

Create a new endpoint and generate an SDP offer to send to the remote peer.

```json
{
  "id": "2",
  "method": "endpoint.create_offer",
  "params": {
    "type": "webrtc",
    "direction": "sendrecv",
    "srtp": false,
    "codecs": ["pcmu", "opus"]
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | required | `"webrtc"` or `"rtp"` |
| `direction` | string | `"sendrecv"` | Endpoint direction |
| `srtp` | bool | `false` | For `"rtp"` type: include `a=crypto` |
| `codecs` | string[] | all | Preferred codec order |

**Response:**
```json
{"id":"2","result":{"endpoint_id":"...","sdp_offer":"v=0\r\no=..."}}
```

## endpoint.accept_answer

Accept a remote SDP answer for an endpoint created with `endpoint.create_offer`.

```json
{
  "id": "3",
  "method": "endpoint.accept_answer",
  "params": {
    "endpoint_id": "...",
    "sdp": "v=0\r\no=..."
  }
}
```

**Response:**
```json
{"id":"3","result":{}}
```

Returns an empty result `{}` on success. The SDP answer is consumed by the endpoint but no data is returned to the caller.

## endpoint.remove

Remove an endpoint from the session.

```json
{"id":"4","method":"endpoint.remove","params":{"endpoint_id":"..."}}
```

**Response:**
```json
{"id":"4","result":{}}
```

## endpoint.ice_restart

Perform an ICE restart on a WebRTC endpoint. Returns a new SDP offer with fresh ICE credentials. Deliver this to the remote peer and feed back their answer via `endpoint.accept_answer`.

```json
{"id":"5","method":"endpoint.ice_restart","params":{"endpoint_id":"..."}}
```

**Response:**
```json
{"id":"5","result":{"sdp_offer":"v=0\r\no=..."}}
```

### ICE Restart Workflow

Use ICE restart when a WebRTC endpoint's connectivity degrades:

1. Detect the issue via `endpoint.state_changed` event (state goes to `disconnected`)
2. Call `endpoint.ice_restart` to get a fresh SDP offer
3. Deliver the returned SDP offer to the remote peer via your signaling channel
4. Receive the remote peer's SDP answer and feed it back via `endpoint.accept_answer`
5. ICE re-negotiation proceeds; monitor `endpoint.state_changed` for `connected`

```json
// Step 2: Request ICE restart
{"id":"5","method":"endpoint.ice_restart","params":{"endpoint_id":"ep-abc"}}
{"id":"5","result":{"sdp_offer":"v=0\r\no=..."}}

// Step 4: Feed back the remote's answer
{"id":"6","method":"endpoint.accept_answer","params":{"endpoint_id":"ep-abc","sdp":"v=0\r\n..."}}
{"id":"6","result":{}}
```

**Failure scenarios:**
- If the remote peer is unreachable, ICE will time out and the endpoint remains `disconnected`
- If the endpoint was removed, the request returns `ENDPOINT_ERROR`
- Multiple rapid ICE restarts are safe; each generates fresh credentials

## endpoint.srtp_rekey

Initiate an SRTP rekey on a plain RTP endpoint with SDES SRTP enabled. Not applicable to WebRTC endpoints (which use DTLS-SRTP managed by the DTLS handshake). Generates a new crypto key and returns an updated SDP containing the new `a=crypto` line. The remote peer should be signaled with this new SDP. During the transition, the endpoint accepts packets encrypted with either the old or new key.

```json
{"id":"6","method":"endpoint.srtp_rekey","params":{"endpoint_id":"..."}}
```

**Response:**
```json
{"id":"6","result":{"sdp":"v=0\r\no=..."}}
```

## endpoint.transfer

Transfer an endpoint from the current session to a different session. The endpoint keeps its connection (sockets, ICE, DTLS, SRTP state) — the remote peer sees no change. Active recordings on the endpoint are stopped. File endpoints cannot be transferred.

```json
{
  "id": "1",
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
{"id":"1","result":{"endpoint_id":"...","target_session_id":"..."}}
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
