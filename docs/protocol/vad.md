# Voice Activity Detection

VAD monitors an endpoint's incoming audio and emits events when speech starts or silence persists. It is **completely independent of recording** — they can be used separately or together.

## Use Cases

- **IVR / Voicemail**: Detect when a caller finishes speaking after a prompt
- **LLM Streaming**: Know when the user is done talking before sending audio to an LLM
- **Call Quality**: Monitor whether participants are actively speaking

## vad.start

```json
{
  "id": "1",
  "method": "vad.start",
  "params": {
    "endpoint_id": "...",
    "silence_interval_ms": 1000,
    "speech_threshold": 0.5
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string | required | Endpoint to monitor |
| `silence_interval_ms` | u32 | `1000` | How often to emit silence events (ms, 100-3600000) |
| `speech_threshold` | f32 | `0.5` | earshot score threshold (0.0-1.0) |

## vad.stop

```json
{"id":"2","method":"vad.stop","params":{"endpoint_id":"..."}}
```

## Events

### vad.speech_started

Emitted when speech is detected after a period of silence.

```json
{"event":"vad.speech_started","data":{"endpoint_id":"..."}}
```

### vad.silence

Emitted periodically while silence persists (every `silence_interval_ms`). The first emission occurs after `silence_interval_ms` of continuous silence.

```json
{"event":"vad.silence","data":{"endpoint_id":"...","silence_duration_ms":3000}}
```

### vad.error

Emitted when the VAD decoder cannot be created for the endpoint's codec.

```json
{"event":"vad.error","data":{"endpoint_id":"...","error":"VAD decoder creation failed: ..."}}
```

## IVR Pattern

For an IVR scenario where a greeting plays before listening for a response:

1. Start file playback (greeting prompt)
2. Start VAD on the caller's endpoint
3. **Ignore** `vad.silence` events while the greeting is still playing
4. When `endpoint.file.finished` fires, start paying attention to VAD events
5. Wait for `vad.speech_started` (caller begins responding)
6. Wait for `vad.silence` with sufficient `silence_duration_ms` (caller finished)
7. Process the response

This pattern keeps the VAD logic simple in rtpbridge while giving the control application full flexibility.
