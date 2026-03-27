# rtpbridge

RTP media routing server in Rust, loosely inspired by rtpengine.

## Build & Test

```bash
cargo build
cargo test
cargo test -- --test-threads=1  # for integration tests that spawn server processes
```

## Architecture

- **Control plane**: WebSocket JSON (tokio-tungstenite) on configurable port
- **Session model**: 1 WS connection = 1 session. Orphan timeout on disconnect, `session.attach` to reclaim.
- **Endpoint types**: WebRTC (str0m), Plain RTP/SRTP, File playback (local files and URLs with WAV, MP3, OGG, FLAC)
- **Threading**: One tokio task per session. All endpoints in a session share the task. No Arc/Mutex on str0m Rtc instances.
- **UDP sockets**: Per-endpoint (not shared mux). Each endpoint binds its own port.
- **Routing**: Auto-rebuilt routing table respecting sendrecv/recvonly/sendonly directions.

## Implementation Plan

Full plan at `~/.Codex/plans/happy-finding-abelson.md`. Progress tracked in Codex memory.

**Completed**: All 8 phases fully implemented
**152 unit tests + 75 integration tests = 227 tests passing**

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
- Endpoint type: `webrtc`, `rtp`
- `endpoint.create_from_offer` auto-detects WebRTC vs plain RTP from SDP content
