# rtpbridge

RTP media routing server in Rust, loosely inspired by rtpengine.

## Build & Test

```bash
cargo build
cargo test
```

## Architecture

- **Control plane**: WebSocket JSON (tokio-tungstenite) on configurable port
- **Session model**: 1 WS connection = 1 session. Orphan timeout on disconnect, `session.attach` to reclaim.
- **Endpoint types**: WebRTC (str0m), Plain RTP/SRTP, File playback, Tone generator
- **Threading**: One tokio task per session. All endpoints in a session share the task. No Arc/Mutex on str0m Rtc instances.
- **UDP sockets**: Per-endpoint (not shared mux). Each endpoint binds its own port.
- **Routing**: Auto-rebuilt routing table respecting sendrecv/recvonly/sendonly directions.
- **Mixing**: Per-destination audio mixer for 3+ party conferences. When a destination receives from 2+ sources, all sources are decoded to PCM, summed with saturation, and re-encoded with monotonic timestamps. With exactly 1 source, packets are forwarded directly (passthrough/transcode). Mixer state lives in `session/mixer.rs`; lifecycle managed by `rebuild_mixers()` on routing table changes.

## Implementation Plan

Full plan at `~/.claude/plans/happy-finding-abelson.md`. Progress tracked in Claude memory.

**Completed**: All 8 phases + audio mixing fully implemented

## Key Crates

- `str0m` 0.17 — Sans-I/O WebRTC (ICE lite, RTP mode)
- `tokio-tungstenite` — WebSocket
- `serde`/`serde_json` — JSON protocol
- `dashmap` — Concurrent session storage
- `xlaw` 0.0.3 — G.711 mu-law (pure Rust)
- `ezk-g722` 0.1 — G.722 (pure Rust, use `libg722::encoder/decoder` sub-modules)
- `opus` 0.3 — Opus (FFI to libopus)
- `pcap-file` 2.0 — PCAP recording with custom timestamps
- `etherparse` 0.19 — Synthetic Ethernet/IPv4/UDP headers for PCAP
- `earshot` 1.0 — Pure Rust VAD (voice activity detection)
- `symphonia` 0.5 — Audio file decode (WAV/MP3/OGG/FLAC)
- `reqwest` 0.12 — Async HTTP for URL file downloads

## Wire Format Notes

- Direction enum: `sendrecv`, `recvonly`, `sendonly` (SDP convention, no underscores)
- Endpoint type: `webrtc`, `rtp`, `bridge`
- `endpoint.create_from_offer` auto-detects WebRTC vs plain RTP from SDP content
- `endpoint.transfer` moves an endpoint between sessions (keeps sockets/ICE/DTLS state)
- `session.bridge` creates paired bridge endpoints for cross-session audio (PCM L16 at 48kHz)
- Transfer events: `endpoint.transferred_out`, `endpoint.transferred_in`
- Empty session event: `session.empty_timeout` (fired when `empty_session_timeout_secs` triggers)
- `endpoint.create_tone` creates a tone generator (sendonly): `tone` = `ringback`/`ringing`/`busy`/`beep`/`sine`, optional `frequency` (for sine), optional `duration_ms`
- Tone finished event: `endpoint.tone.finished` (fired when `duration_ms` expires)
