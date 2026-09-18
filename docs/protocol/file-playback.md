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
    "cache_key": "prompt:3d644a34-65e8-4f8f-b31c-42b7c71b4553",
    "shared": false,
    "timeout_ms": 10000,
    "headers": { "Authorization": "Bearer ..." },
    "gain_db": 0.0
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `source` | string | required | File path or HTTP(S) URL |
| `start_ms` | u64 | `0` | Start playback at this position |
| `loop_count` | u32 or null | `0` | Number of additional replays: `0` = play once, `1` = play twice, `null` = loop infinitely. Maximum: 10000 |
| `cache_ttl_secs` | u32 | `300` | For URLs: cache lifetime in seconds. `0` = delete after use |
| `cache_key` | string or null | `null` | Optional logical cache identity for URL sources. When set, it replaces the source URL portion of the cache key; request headers remain included. It is local to rtpbridge and is never sent in the download request. Reuse it only for immutable bytes; change it when the media changes. |
| `shared` | bool | `false` | Share decode pipeline across sessions |
| `timeout_ms` | u32 | `10000` | Max milliseconds to wait for download (URL sources only; ignored for local files). Min: 1, max: 60000 |
| `headers` | object or null | `null` | Optional HTTP headers for URL sources. Header values are included in the cache key so authenticated URLs do not share cached content across different headers |
| `gain_db` | number | `0.0` | Playback gain in decibels; negative values attenuate, positive values amplify |

Supported formats: WAV, MP3, OGG/Vorbis, FLAC.

File endpoints are always **send-only** — they produce audio but don't receive it.

The creation response contains only the endpoint ID and does not mean media has started. URL sources can
still be downloading or buffering. Use `endpoint.file.started` as the authoritative playback boundary.

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

### endpoint.file.started

Emitted once, when the first RTP packet from the file enters media routing. `started_at_epoch_ms` uses the
media-host epoch clock shared by recording PCAP timestamps.

```json
{
  "event": "endpoint.file.started",
  "data": {
    "endpoint_id": "...",
    "started_at_epoch_ms": 1730000000000
  }
}
```

### endpoint.file.finished

Emitted when playback completes (all loops done) or encounters an error.

```json
{
  "event": "endpoint.file.finished",
  "data": {
    "endpoint_id": "...",
    "finished_at_epoch_ms": 1730000038000,
    "reason": "completed",
    "error": null
  }
}
```

`reason` is `"completed"` or `"error"`. If `"error"`, the `error` field contains the message.
`finished_at_epoch_ms` uses the same media-host epoch clock as the start event and recording PCAPs.
