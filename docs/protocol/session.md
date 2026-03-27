# Sessions

## session.create

Create a new media session and bind it to this connection.

```json
{"id":"1","method":"session.create","params":{}}
```

**Response:**
```json
{"id":"1","result":{"session_id":"550e8400-e29b-41d4-a716-446655440000"}}
```

Only one session per connection. Returns `SESSION_ALREADY_BOUND` if the connection already has a session.

## session.attach

Reattach to an existing orphaned session.

```json
{"id":"2","method":"session.attach","params":{"session_id":"550e8400-..."}}
```

The session must be in `orphaned` state (control connection previously dropped). Media continues flowing during the orphan period.

**Response:**
```json
{"id":"2","result":{}}
```

**Error codes:**
- `SESSION_NOT_FOUND` — session was already destroyed (timeout expired or explicit destroy)
- `SESSION_NOT_ORPHANED` — session is still bound to another connection
- `SESSION_ALREADY_BOUND` — this connection already has a session

### Reconnection Workflow

When a WebSocket connection drops unexpectedly:

1. The server detects the drop and emits `session.orphaned` (best-effort delivery)
2. Media continues flowing between endpoints for `disconnect_timeout_secs`
3. The client opens a new WebSocket and calls `session.attach` with the session ID
4. On success, all events resume on the new connection
5. If the timeout expires before reattach, the session and all endpoints are destroyed

```json
// 1. Open new WebSocket connection
// 2. Reattach to the orphaned session
{"id":"1","method":"session.attach","params":{"session_id":"550e8400-e29b-41d4-a716-446655440000"}}
// 3. Success — session is now bound to this connection
{"id":"1","result":{}}
// 4. Resume normal operation (events flow, commands accepted)
```

## session.destroy

Destroy the session bound to this connection. All endpoints are torn down and recordings are flushed.

```json
{"id":"3","method":"session.destroy","params":{}}
```

**Response:**
```json
{"id":"3","result":{}}
```

## session.info

Get details about the current session.

```json
{"id":"4","method":"session.info","params":{}}
```

**Response:**
```json
{
  "id": "4",
  "result": {
    "session_id": "550e8400-...",
    "state": "active",
    "created_at": "1711324800s",
    "endpoints": [],
    "recordings": [],
    "vad_active": []
  }
}
```

## session.list

List all sessions on the server (not just this connection's session).

```json
{"id":"5","method":"session.list","params":{}}
```

**Response:**
```json
{
  "id": "5",
  "result": {
    "sessions": [
      { "session_id": "...", "state": "active", "endpoint_count": 2, "created_at": "..." }
    ]
  }
}
```

## session.bridge

Create a bidirectional audio bridge between the current session and a target session. This inserts a virtual "bridge endpoint" into each session. Audio routed to a bridge endpoint is forwarded to the paired session as decoded PCM (L16 at 48kHz), preserving full audio quality with zero lossy re-encoding at the bridge boundary.

Bridge-to-bridge routing is excluded to prevent audio loops — bridge endpoints only exchange audio with real endpoints (WebRTC, RTP, File), never with other bridge endpoints.

Removing either bridge endpoint via `endpoint.remove` automatically removes the paired endpoint in the other session.

```json
{
  "id": "1",
  "method": "session.bridge",
  "params": {
    "target_session_id": "...",
    "direction": "sendrecv"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `target_session_id` | string | required | Session to bridge to |
| `direction` | string | `"sendrecv"` | `"sendrecv"`, `"recvonly"`, or `"sendonly"` |

**Response:**
```json
{"id":"1","result":{"endpoint_id":"...","target_endpoint_id":"..."}}
```

`endpoint_id` is the bridge endpoint in the calling session. `target_endpoint_id` is the bridge endpoint in the target session.

**Error codes:**
- `NO_SESSION` — no session bound
- `INVALID_PARAMS` — self-bridge
- `SESSION_NOT_FOUND` — target session doesn't exist
- `ENDPOINT_ERROR` — max endpoints reached
