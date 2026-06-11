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
- **Endpoint types**: WebRTC (str0m), Plain RTP/SRTP, File playback, Tone generator, WebSocket audio, Bridge
- **Threading**: One tokio task per session. All endpoints in a session share the task. No Arc/Mutex on str0m Rtc instances.
- **UDP sockets**: Per socket-backed endpoint (not shared mux). WebRTC binds one OS-assigned UDP port per configured `media_ip` family; plain RTP/SRTP binds an even/odd RTP/RTCP pair from `rtp_port_range`. File, tone, bridge, and WebSocket endpoints do not allocate RTP ports.
- **Dual-stack**: `media_ip` is a list (≤1 IPv4, ≤1 IPv6, via `MediaBindings` in `net/socket_pool.rs`). Plain RTP picks the family matching the remote SDP `c=` line (rejects an unbound/known family, and rejects a family flip on re-negotiation — no socket migration); WebRTC offers a host candidate per family and lets ICE nominate. `server.info`'s `media_ip` is an array. PCAP recording stays IPv4-synthetic.
- **Routing**: Auto-rebuilt routing table respecting sendrecv/recvonly/sendonly/inactive directions.
- **Mixing**: Per-destination audio mixer for 3+ party conferences. When a destination receives from 2+ sources, all sources are decoded to PCM, summed with saturation, and re-encoded with monotonic timestamps. With exactly 1 source, packets are forwarded directly (passthrough/transcode). Mixer state lives in `session/mixer.rs`; lifecycle managed by `rebuild_mixers()` on routing table changes.

## Implementation Plan

Full plan at `~/.claude/plans/happy-finding-abelson.md`. Progress tracked in Claude memory.

**Completed**: All 8 phases + audio mixing fully implemented

## Key Crates

- `str0m` — Sans-I/O WebRTC (ICE lite, RTP mode)
- `tokio-tungstenite` — WebSocket
- `serde`/`serde_json` — JSON protocol
- `dashmap` — Concurrent session storage
- `xlaw` — G.711 mu-law (pure Rust)
- `ezk-g722` — G.722 (pure Rust, use `libg722::encoder/decoder` sub-modules)
- `opus` — Opus (FFI to libopus)
- `pcap-file` — PCAP recording with custom timestamps
- `etherparse` — Synthetic Ethernet/IPv4/UDP headers for PCAP
- `earshot` — Pure Rust VAD (voice activity detection)
- `symphonia` — Audio file decode (WAV/MP3/OGG/FLAC)
- `reqwest` — Async HTTP for URL file downloads

## Wire Format Notes

- Direction enum: `sendrecv`, `recvonly`, `sendonly`, `inactive` (SDP convention, no underscores)
- Endpoint type labels in session/event payloads: `webrtc`, `rtp`, `file`, `tone`, `bridge`, `websocket`
- `endpoint.create_from_offer` auto-detects WebRTC vs plain RTP from SDP content
- `endpoint.transfer` moves an endpoint between sessions (keeps sockets/ICE/DTLS state)
- `session.bridge` creates paired bridge endpoints for cross-session audio (PCM L16 at 48kHz)
- Transfer events: `endpoint.transferred_out`, `endpoint.transferred_in`
- Empty session event: `session.empty_timeout` (fired when `empty_session_timeout_secs` triggers)
- `endpoint.create_websocket` creates a WebSocket audio endpoint (dial-in): params `direction`, `sample_rate` (8000/16000/48000, mono 16-bit LE), optional `flush_ms` (outbound coalescing, multiple of 20; 0 = passthrough). Returns `{endpoint_id, connect_token}`; the peer dials `/audio/<connect_token>` on the control/HTTP port to stream raw PCM as binary frames. Internally an L16 endpoint (PT 127) like Bridge, but at the wire `sample_rate` (not a fixed 48k — `AudioCodec::L16` carries its rate), with a synthesized monotonic inbound RTP timeline and a single IO task that only reframes to 20ms (no resampling; the session's per-edge resampler converts to peers directly). Bridge stays pinned at L16@48k as the cross-session canonical rate. Events: `endpoint.ws.connected`, `endpoint.ws.disconnected`, `endpoint.ws.connect_timeout`. Not transferable. See `docs/protocol/websocket.md`. Code: `session/endpoint_websocket.rs`, `control/ws_audio.rs` (connect-token registry on `SessionManager`).
- `endpoint.create_tone` creates a tone generator (sendonly): `tone` = `ringback`/`ringing`/`busy`/`beep`/`sine`, optional `frequency` (for sine), optional `duration_ms`
- Tone finished event: `endpoint.tone.finished` (fired when `duration_ms` expires)
- `fax_detect.start`/`fax_detect.stop` arm Goertzel fax-tone detection on an endpoint (CNG 1100Hz, CED 2100Hz). Events: `fax.cng_detected`, `fax.ced_detected`, `fax.error`. Detection is notification-only (no T.38/passthrough action). VAD and fax share one PCM decode per packet via `session/audio_analysis.rs`; `feed_vad`/`feed_fax` consume that PCM. `fax_detect_active` lists monitored endpoints in `session.info`.
