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

## endpoint.update_direction

Change an endpoint's direction in the routing table (e.g. when a phone toggles hold/unhold via re-INVITE). Resets the symmetric RTP address lock so the endpoint will re-learn the remote source address — useful when the peer resumes from a new NAT binding after hold.

```json
{
  "id": "4",
  "method": "endpoint.update_direction",
  "params": {
    "endpoint_id": "...",
    "direction": "sendrecv"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string | required | Endpoint to update |
| `direction` | string | required | `"sendrecv"`, `"sendonly"`, or `"recvonly"` |

**Response:**
```json
{"id":"4","result":{}}
```

Returns an empty result `{}` on success. Only supported on RTP, WebRTC, and Bridge endpoints — File and Tone endpoints do not have a meaningful remote direction and will return an error.

## endpoint.update_remote_sdp

Update an RTP endpoint's remote RTP/RTCP address (and SRTP keys, if present) from a re-INVITE SDP body, **without touching codec state**. Use this for hold/unhold or other mid-call re-INVITEs where the remote may send a different codec list or PT mapping than the original answer — applying that SDP via `endpoint.accept_answer` would re-parse codecs and corrupt the session's codec/telephone-event PT state.

```json
{
  "id": "5",
  "method": "endpoint.update_remote_sdp",
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

Returns an SDP answer that the caller can send back as the `200 OK` body for the re-INVITE. The answer reflects the endpoint's **current state** — local address/port, currently-selected codec listed first (so the peer obeys RFC 3264 and continues sending the codec we're already sending), and the existing TX SRTP key if SRTP is active. The codec list is reordered so `send_codec` precedes the rest; this is critical for phones (e.g. Grandstream GXP21xx) that re-derive their outbound codec from the first PT in the answer.

**What it updates:**
- Remote RTP and RTCP addresses (honors `a=rtcp-mux` and explicit `a=rtcp:` lines)
- SRTP RX key, **only if** the `a=crypto` line differs from the currently installed RX key. When the key changes, the 5-second dual-context transition applies (the endpoint accepts packets encrypted with either the old or new key during the window).
- SRTP/SRTCP RX sequence + replay state. Same-key or different-key, the inbound replay window (`highest_seq` / `replay_window` for SRTP, `highest_recv_index` / `replay_window` for SRTCP) is reset on the live RX context. Phones commonly send RTCP BYE on hold and resume with a new SSRC + reset RTP sequence on unhold; without this reset, the post-resume packets are rejected as "too old" and decrypt silently fails for the rest of the call. The derived session keys (`cipher_key`, `auth_key`, `cipher_salt`) are preserved.
- Remote SSRC tracker. The cached remote SSRC is cleared and relearned from the next inbound packet (the SSRC may change across hold/unhold). This also defers outbound media until the peer is confirmed reachable on the new address (NAT safety).

**What it deliberately does NOT update:**
- `codecs` list
- `send_codec`
- `telephone_event_pt`
- Outbound (TX) SRTP/SRTCP state — our own ROC/sequence number/SRTCP index keep advancing so the peer's replay window stays valid for our outbound stream.

The address lock is reset so the endpoint re-learns the remote source address from inbound packets (necessary when the peer's NAT binding changes after hold).

Only supported on RTP endpoints. WebRTC, File, Tone, and Bridge endpoints will return an error.

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
