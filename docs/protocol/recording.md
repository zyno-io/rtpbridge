# Recording

Recording captures decrypted **inbound** RTP and RTCP — what each source produces —
to a PCAP file with accurate timestamps. Recordings operate at two independent
layers that can run simultaneously:

- **Full session**: records all legs (all endpoints)
- **Single leg**: records one specific endpoint (e.g., for voicemail)

Each recorded endpoint is captured at arrival (before the jitter buffer, so the
PCAP preserves real arrival order/timing), preceded by a **codec descriptor** packet
that declares how to decode it (see [Codec descriptors](#codec-descriptors)).
Recording is one-directional: rtpbridge does **not** record the RTP/RTCP it *sends*
toward peers (there is no `record_outbound`). Internally-generated sources (file
playback, tone) are captured too — file playback is recorded as native-rate L16.

## recording.start

```json
{
  "id": "1",
  "method": "recording.start",
  "params": {
    "endpoint_id": null,
    "file_path": "/recordings/call-123.pcap"
  }
}
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint_id` | string or null | optional (default: `null`) | `null` for full-session recording, endpoint ID for single leg |
| `file_path` | string | required | Absolute path within `recording_dir`. Symlinks are resolved; the real path must remain inside the configured directory |

**Response:**
```json
{"id":"1","result":{"recording_id":"...","started_at_epoch_ms":1730000000000}}
```

## recording.stop

```json
{"id":"2","method":"recording.stop","params":{"recording_id":"..."}}
```

**Response:**
```json
{
  "id": "2",
  "result": {
    "file_path": "/recordings/call-123.pcap",
    "duration_ms": 30000,
    "packets": 1500,
    "dropped_packets": 0,
    "stopped_at_epoch_ms": 1730000030000
  }
}
```

`started_at_epoch_ms` and `stopped_at_epoch_ms` use the media-host epoch clock. The latter is
captured at the media-session stop boundary, not after asynchronous cleanup.

## PCAP Format

- Link type: Ethernet
- Each packet has Ethernet/IP/UDP headers framed `source -> us` (`src = remote, dst = local`)
- Real endpoints (plain RTP/SRTP, WebRTC) are framed with their **real** IPv4 or IPv6 socket addresses (correct even after a NAT rebind)
- Endpoints with no real socket (file, tone, bridge, websocket) fall back to deterministic synthetic markers: `10.{(N/254)+1}.0.{(N%254)+1}:10000` where N is the endpoint index
- Timestamps are wall-clock time from when the packet was captured (arrival)
- Both RTP media packets and RTCP reports are captured; SRTP/SRTCP is decrypted — ciphertext is never written
- The PCAP file can be opened directly in Wireshark (descriptor packets show as plain UDP data)

## Codec descriptors

Before an endpoint's media, the recorder writes a small **descriptor packet** so a
consumer knows how to decode the RTP that follows. It is a synthetic UDP packet,
framed identically to that endpoint's media, whose payload is the 4-byte magic
`RBP1` followed by JSON:

```jsonc
{
  "v": 1,
  "endpoint_id": "0c2f…uuid",
  "role": "remote" | "internal",     // remote = real peer; internal = synthesized
  "type": "rtp"|"webrtc"|"file"|"tone"|"bridge"|"websocket",
  "codec": "PCMU"|"G722"|"opus"|"L16",
  "pt": 0,                            // payload type of the RTP that follows
  "clock_rate": 8000,                 // RTP clock (8000 for G.722 even though audio is 16 kHz)
  "channels": 1,
  "endian": "le",                     // L16 only: this codebase's L16 is little-endian
  "local": "…", "remote": "…"
}
```

- The magic byte `R` (0x52) has RTP version bits `01`, so descriptors can't be
  confused with RTP (V=2) or RTCP (PT 200–204), and Wireshark won't dissect them as RTP.
- A descriptor is (re)emitted whenever the codec or framing address changes
  (re-negotiation, source-address latch), always **before** the media it describes —
  even under channel backpressure the descriptor/media pair is dropped together, so
  media never precedes its descriptor.
- A recording started mid-call replays the current descriptors first, so the file is
  self-describing from byte 0.
- WebRTC codec/PT are read from the actual str0m negotiation (not assumed Opus/111).

## Decoding (pcap2audio)

The `pcap2audio` binary decodes a recording into a WAV file:

```
pcap2audio <input.pcap> -o <out.wav> [--mode multichannel|stereo] [--rate 48000] [--metadata <timeline.json>]
```

- `--mode multichannel` — one WAV channel per endpoint in first-appearance order
- `--mode stereo` (default) — left = first endpoint, right = all others summed
- Demuxes by the frame `(src,dst)` pair (bound to an endpoint by descriptors),
  treats each repeated descriptor as a new RTP epoch (including concatenated recording
  segments), reorders each epoch by RTP sequence (wrap-safe) before decoding (stateful
  Opus/G.722), fills timestamp gaps with silence, and aligns channels on the PCAP
  earliest decodable RTP capture-time origin. `--metadata` writes that exact sample-zero epoch
  and rendered WAV duration for downstream timeline alignment. Convert to Opus/MP3/etc. with external tooling (e.g. ffmpeg).

### Timing note: bridge / websocket sources

Bridge and WebSocket sources are captured at arrival, *before* their RTP timeline
is synthesized downstream (their recorded RTP timestamps are `0`). For those
channels the decoder paces by **PCAP capture wall-clock** instead of RTP
timestamps, so real inter-packet gaps are preserved. Real RTP/WebRTC sources use
their (accurate) RTP timestamps for intra-stream timing as usual.

| Field | Description |
|-------|-------------|
| `file_path` | Absolute path of the written PCAP file |
| `duration_ms` | Wall-clock recording duration |
| `packets` | Total packets written to the PCAP file |
| `dropped_packets` | Packets dropped due to disk I/O backpressure (see Bounded Channel below) |

## Bounded Channel

Recording uses a bounded channel (capacity 1000 packets) between the media path and the disk write task. If disk I/O stalls, packets are dropped from the recording rather than blocking the media path. Drops are logged at warn level and reported in the `dropped_packets` field of the `recording.stop` response.


Recording admission is limited to 32 writers process-wide. The existing packet channel is also bounded by 1 MiB of queued payload per recording and 16 MiB across the process, including a per-packet accounting allowance. File creation and writes run outside the session task. A flush timeout stops waiting; it cannot cancel a syscall or free the writer's admission early.

`GET /recordings/<path>` streams an opened-file snapshot in 64 KiB chunks, with four concurrent downloads, five-second socket write deadlines, ten-second read deadlines and a 120-second overall deadline. Symlinks and parent traversal are rejected at every path component. A growing recording can contain a partial final PCAP record; a download does not finalize or flush it.

The converter uses temporary encoded/PCM spools and blockwise WAV interleaving. Limits are 32 channels, one million captured packets, 64 KiB per captured frame, 16 KiB per encoded audio payload, one hour of output timeline, 1 GiB of WAV data and 2 GiB of temporary storage including the final output. Supported output rates are 8, 16 and 48 kHz. Inputs exceeding a limit fail before publishing the WAV; the completed output is published atomically. Sequence reordering is supported within descriptor epochs using signed 16-bit sequence deltas; gaps of half the sequence space or more are ambiguous and should be separated into recordings/epochs.
