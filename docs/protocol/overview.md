# Protocol Overview

rtpbridge is controlled via JSON messages over WebSocket.

## Connection Model

Each WebSocket connection is bound to exactly **one media session**. After `session.create` or `session.attach`, all subsequent commands operate on that session — no `session_id` is needed on most commands.

When a WebSocket connection drops, the session enters an **orphaned** state. Media continues flowing. A new connection can reclaim the session via `session.attach` within the configurable timeout (default 30 seconds).

## Message Format

### Request (client to server)

```json
{
  "id": "req-1",
  "method": "session.create",
  "params": {}
}
```

- `id` — Client-generated correlation ID (string). Returned in the response.
- `method` — Method name (string).
- `params` — Method parameters (object). Can be omitted or `{}` for no params.

### Success Response

```json
{
  "id": "req-1",
  "result": { "session_id": "550e8400-..." }
}
```

### Error Response

```json
{
  "id": "req-1",
  "error": {
    "code": "SESSION_NOT_FOUND",
    "message": "No session with that ID"
  }
}
```

### Event (server to client, unsolicited)

```json
{
  "event": "dtmf",
  "data": { "endpoint_id": "...", "digit": "5", "duration_ms": 160 }
}
```

Events are emitted asynchronously and are not correlated to requests.

### Event Delivery Semantics

- **Asynchronous**: Events are emitted independently of request/response pairs. They are not correlated to any specific command.
- **Ordering**: Events within the same priority tier are delivered in order. Priority events (see below) may be delivered ahead of normal events queued at the same time.
- **Delivery guarantee**: At-most-once. Under backpressure (slow client), events may be dropped rather than block the media pipeline.
- **Drop notification**: When events are dropped, the server sends an `events.dropped` notification with a `count` field indicating how many events were lost since the last successful delivery.
- **Priority events**: Critical events (`endpoint.state_changed`, `recording.stopped`, `endpoint.file.finished`, `session.idle_timeout`) are routed through a separate priority channel and are dropped only when both the priority and normal channels are full.

## Error Codes

| Code | Description |
|------|-------------|
| `PARSE_ERROR` | Invalid JSON |
| `UNKNOWN_METHOD` | Method not recognized |
| `INVALID_PARAMS` | Missing or invalid parameters |
| `NO_SESSION` | No session bound to this connection |
| `SESSION_ALREADY_BOUND` | Connection already has a session |
| `SESSION_NOT_FOUND` | Session ID does not exist |
| `SESSION_NOT_ORPHANED` | Session exists but is not orphaned |
| `SESSION_GONE` | Session task ended unexpectedly |
| `SESSION_UNRESPONSIVE` | Session did not respond to command within timeout |
| `SESSION_BUSY` | Session command channel is full |
| `SHUTTING_DOWN` | Server is shutting down |
| `MAX_SESSIONS_REACHED` | Resource limit exceeded |
| `ENDPOINT_ERROR` | Endpoint operation failed |
| `RECORDING_ERROR` | Recording operation failed |
| `VAD_ERROR` | VAD operation failed |
| `STATS_ERROR` | Statistics subscribe/unsubscribe failed |
| `INVALID_REQUEST` | Request `id` field is empty or invalid |
| `INTERNAL_ERROR` | Internal server error (e.g., serialization failure) |

## Error Codes by Method

Most methods that require a session can return `NO_SESSION` (no session bound), `SESSION_GONE` (session task ended), and `SESSION_UNRESPONSIVE` (5-second command timeout). These common codes are listed once here and apply to all session-bound methods below.

| Method | Additional Error Codes |
|--------|----------------------|
| `session.create` | `SESSION_ALREADY_BOUND`, `SHUTTING_DOWN`, `MAX_SESSIONS_REACHED` |
| `session.attach` | `SESSION_ALREADY_BOUND`, `INVALID_PARAMS`, `SESSION_NOT_FOUND`, `SESSION_NOT_ORPHANED`, `SESSION_BUSY` |
| `session.destroy` | _(none beyond common)_ |
| `session.info` | _(none beyond common)_ |
| `session.list` | _(always succeeds)_ |
| `endpoint.create_from_offer` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.create_offer` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.accept_answer` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.remove` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.dtmf.inject` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.create_with_file` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.file.seek` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.file.pause` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.file.resume` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.ice_restart` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `endpoint.srtp_rekey` | `INVALID_PARAMS`, `ENDPOINT_ERROR` |
| `recording.start` | `INVALID_PARAMS`, `RECORDING_ERROR` |
| `recording.stop` | `INVALID_PARAMS`, `RECORDING_ERROR` |
| `vad.start` | `INVALID_PARAMS`, `VAD_ERROR` |
| `vad.stop` | `INVALID_PARAMS`, `VAD_ERROR` |
| `stats.subscribe` | `INVALID_PARAMS`, `STATS_ERROR` |
| `stats.unsubscribe` | `STATS_ERROR` |

Unknown methods return `UNKNOWN_METHOD`. Invalid JSON returns `PARSE_ERROR`.

## Graceful Shutdown

During graceful shutdown, `session.create` immediately returns `SHUTTING_DOWN`. Existing sessions continue operating — media flows and commands work normally until connections close or sessions are explicitly destroyed. If a session task has already exited during shutdown, commands may return `SESSION_UNRESPONSIVE`.
