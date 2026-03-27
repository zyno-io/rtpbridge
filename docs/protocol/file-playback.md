# File Playback

## endpoint.create_with_file

Create a send-only endpoint that plays audio from a local file or URL.

```json
{
  "id": "1",
  "method": "endpoint.create_with_file",
  "params": {
    "source": "/path/to/audio.wav",
    "start_ms": 0,
    "loop_count": 0,
    "cache_ttl_secs": 300,
    "shared": false,
    "timeout_ms": 10000
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `source` | string | required | File path or HTTP(S) URL |
| `start_ms` | u64 | `0` | Start playback at this position |
| `loop_count` | u32 or null | `null` | Number of additional replays: `0` = play once, `1` = play twice, `null` = loop infinitely. Maximum: 10000 |
| `cache_ttl_secs` | u32 | `300` | For URLs: cache lifetime in seconds. `0` = delete after use |
| `shared` | bool | `false` | Share decode pipeline across sessions |
| `timeout_ms` | u32 | `10000` | Max milliseconds to wait for download (URL sources only; ignored for local files). Min: 1, max: 60000 |

Supported formats: WAV, MP3, OGG/Vorbis, FLAC.

File endpoints are always **send-only** — they produce audio but don't receive it.

## endpoint.file.seek

Seek to a position in the file.

```json
{"id":"2","method":"endpoint.file.seek","params":{"endpoint_id":"...","position_ms":5000}}
```

## endpoint.file.pause / resume

```json
{"id":"3","method":"endpoint.file.pause","params":{"endpoint_id":"..."}}
{"id":"4","method":"endpoint.file.resume","params":{"endpoint_id":"..."}}
```

## Events

### endpoint.file.finished

Emitted when playback completes (all loops done) or encounters an error.

```json
{
  "event": "endpoint.file.finished",
  "data": {
    "endpoint_id": "...",
    "reason": "completed",
    "error": null
  }
}
```

`reason` is `"completed"` or `"error"`. If `"error"`, the `error` field contains the message.
