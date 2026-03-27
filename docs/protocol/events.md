# Events Reference

All events are for the session bound to the current connection. Events are JSON objects with an `event` field (string) and a `data` field (object).

## dtmf

DTMF digit detected from a remote endpoint.

```json
{"event":"dtmf","data":{"endpoint_id":"...","digit":"5","duration_ms":160}}
```

## endpoint.state_changed

Endpoint state transition. States: `new`, `buffering`, `connecting`, `connected`, `playing`, `paused`, `disconnected`, `finished`.

```json
{"event":"endpoint.state_changed","data":{"endpoint_id":"...","old_state":"connecting","new_state":"connected"}}
```

## endpoint.file.finished

File playback completed or errored.

```json
{"event":"endpoint.file.finished","data":{"endpoint_id":"...","reason":"completed","error":null}}
```

## endpoint.media_timeout

No RTP packets received from a remote endpoint for 5 seconds. This threshold defaults to 5 seconds and can be configured via `media_timeout_secs`. The event is factual — it could mean the remote is silent, or there's a network issue. The event fires once per timeout period and resets when packets resume.

```json
{"event":"endpoint.media_timeout","data":{"endpoint_id":"...","duration_ms":5000}}
```

## endpoint.rtcp_bye

RTCP BYE packet received from a remote endpoint, indicating graceful departure. This is informational — the endpoint remains in its current state. The application should decide whether to tear down the endpoint.

```json
{"event":"endpoint.rtcp_bye","data":{"endpoint_id":"...","ssrc_list":[12345],"reason":"User hung up"}}
```

> **Note:** `reason` may be `null` if the BYE packet contains no reason string.

## endpoint.transferred_out

Emitted on the source session when an endpoint is transferred to another session.

```json
{"event":"endpoint.transferred_out","data":{"endpoint_id":"...","target_session_id":"..."}}
```

## endpoint.transferred_in

Emitted on the target session when an endpoint is transferred in from another session.

```json
{"event":"endpoint.transferred_in","data":{"endpoint_id":"...","source_session_id":"...","endpoint_type":"rtp","direction":"sendrecv","state":"connected"}}
```

## recording.stopped

Recording was stopped externally (not by `recording.stop`).

```json
{"event":"recording.stopped","data":{"recording_id":"...","file_path":"...","duration_ms":30000,"packets":1500,"dropped_packets":0,"reason":"..."}}
```

## session.idle_timeout

Session was automatically destroyed because no activity (media packets or commands) occurred for the configured `session_idle_timeout_secs` period. This is a critical event — it is delivered with priority over normal events.

```json
{"event":"session.idle_timeout","data":{"session_id":"550e8400-...","idle_timeout_secs":300}}
```

## session.empty_timeout

Session was auto-destroyed because it had zero endpoints for the configured `empty_session_timeout_secs` duration. This is a critical event.

```json
{"event":"session.empty_timeout","data":{"session_id":"...","empty_timeout_secs":30}}
```

## session.orphaned

Control connection dropped. The session remains alive for the configured `disconnect_timeout_secs`. This event is delivered on a **best-effort** basis — if the WebSocket is already in the closing handshake or the TCP connection has dropped, the event may not reach the client. Use `session.attach` on a new connection to reclaim the session before the timeout expires.

```json
{"event":"session.orphaned","data":{"timeout_remaining_ms":30000}}
```

## events.dropped

Fired when events were dropped due to client backpressure. The server uses a bounded event channel; when a slow client can't keep up, excess events are dropped and this notification is sent with the count of lost events.

```json
{"event":"events.dropped","data":{"count":5}}
```

> **Recovery:** When events are dropped, the client may have missed state transitions (endpoint state changes, recording stops). Poll `session.info` on the same connection to reconcile any missed state.

## stats

Periodic session statistics (see [Statistics](./stats.md)).

## vad.speech_started

Speech detected after silence.

```json
{"event":"vad.speech_started","data":{"endpoint_id":"..."}}
```

## vad.silence

Periodic silence notification.

```json
{"event":"vad.silence","data":{"endpoint_id":"...","silence_duration_ms":3000}}
```
