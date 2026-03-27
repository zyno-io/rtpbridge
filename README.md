# rtpbridge

A high-performance RTP media routing server in Rust, loosely inspired by [rtpengine](https://github.com/sipwise/rtpengine).

rtpbridge sits between VoIP endpoints, routing audio between them with support for WebRTC, plain RTP/SRTP, codec transcoding, DTMF, recording, voice activity detection, and file playback. It is controlled via a WebSocket JSON interface where each connection maps 1:1 to a media session.

## Features

- **WebRTC endpoints** via [str0m](https://github.com/algesten/str0m) (DTLS, ICE lite, SRTP)
- **Plain RTP/SRTP endpoints** with SDES key exchange
- **Codec transcoding** between PCMU (G.711), G.722, and Opus
- **DTMF** detection, forwarding, and injection (RFC 4733 telephone-event)
- **PCAP recording** of decrypted media (RTP + RTCP) with accurate timestamps
- **Voice Activity Detection** via [earshot](https://github.com/pykeio/earshot) (independent of recording)
- **File playback** from local files or URLs (WAV, MP3, OGG, FLAC) with seek, pause, loop
- **Cross-session endpoint transfer** — move endpoints between sessions with zero overt media disruption (`endpoint.transfer`)
- **Session bridging** — bidirectional audio bridge between sessions using decoded PCM (L16 at 48kHz) with no lossy re-encoding at the bridge boundary (`session.bridge`)
- **Session management** with orphan timeout, reconnection (`session.attach`), and empty session auto-destroy
- **Graceful shutdown** with configurable drain wait (k8s compatible)
- **Split interface binding** for separate control and media networks
- **Symmetric RTP** with time-windowed address learning for NAT traversal

## Quick Start

### Build

```bash
# Requires Rust 1.88+, cmake (for Opus)
cargo build --release
```

### Run

```bash
# Default: WebSocket on 0.0.0.0:9100, media on 127.0.0.1
./target/release/rtpbridge --listen 0.0.0.0:9100 --media-ip 203.0.113.5

# With config file
./target/release/rtpbridge --config rtpbridge.toml
```

### Connect

Connect a WebSocket client and send JSON messages:

```bash
# Using websocat
websocat ws://localhost:9100
```

```json
{"id":"1","method":"session.create","params":{}}
```

## Configuration

### CLI Arguments

| Argument | Default | Description |
|----------|---------|-------------|
| `--listen` | `0.0.0.0:9100` | WebSocket control plane address |
| `--media-ip` | `127.0.0.1` | IP for all media sockets (RTP/WebRTC) |
| `--config` | — | Path to TOML config file |
| `--log-level` | `info` | Log level (trace/debug/info/warn/error) |

### TOML Configuration

```toml
listen = "0.0.0.0:9100"
media_ip = "203.0.113.5"
rtp_port_range = [30000, 39999]
disconnect_timeout_secs = 30
shutdown_max_wait_secs = 300
max_sessions = 10000
max_endpoints_per_session = 20
max_recordings_per_session = 100
session_idle_timeout_secs = 0     # 0 = disabled
recording_dir = "/var/lib/rtpbridge/recordings"
media_dir = "/var/lib/rtpbridge/media"
cache_dir = "/tmp/rtpbridge-cache"
cache_cleanup_interval_secs = 300
ws_ping_interval_secs = 30
log_level = "info"
# See rtpbridge.toml.example for all available options
```

## Control Protocol

All communication uses JSON over WebSocket. Each connection is bound to exactly one session.

### Methods

| Method | Description |
|--------|-------------|
| `session.create` | Create a new session |
| `session.attach` | Reattach to an orphaned session |
| `session.destroy` | Destroy the current session |
| `session.info` | Get session details |
| `session.list` | List all sessions on the server |
| `endpoint.create_from_offer` | Create endpoint from remote SDP offer (auto-detects WebRTC vs RTP) |
| `endpoint.create_offer` | Create a new endpoint and generate SDP offer |
| `endpoint.accept_answer` | Accept remote SDP answer |
| `endpoint.create_with_file` | Create file playback endpoint |
| `endpoint.remove` | Remove an endpoint |
| `endpoint.ice_restart` | ICE restart (WebRTC only) |
| `endpoint.file.seek` | Seek file playback position |
| `endpoint.file.pause` | Pause file playback |
| `endpoint.file.resume` | Resume file playback |
| `endpoint.dtmf.inject` | Inject DTMF digit into endpoint |
| `endpoint.srtp_rekey` | Initiate SRTP rekey (plain RTP/SRTP only) |
| `endpoint.transfer` | Transfer an endpoint to a different session |
| `session.bridge` | Bidirectional audio bridge between two sessions |
| `recording.start` | Start recording (full session or single leg) |
| `recording.stop` | Stop recording |
| `vad.start` | Start voice activity detection on endpoint |
| `vad.stop` | Stop voice activity detection |
| `stats.subscribe` | Subscribe to periodic session statistics |
| `stats.unsubscribe` | Unsubscribe from statistics |

### Events

| Event | Description |
|-------|-------------|
| `dtmf` | DTMF digit detected from remote endpoint |
| `endpoint.state_changed` | Endpoint state transition |
| `endpoint.file.finished` | File playback completed |
| `endpoint.media_timeout` | No media received from remote endpoint |
| `endpoint.rtcp_bye` | RTCP BYE received from remote endpoint |
| `endpoint.transferred_out` | Endpoint transferred to another session |
| `endpoint.transferred_in` | Endpoint transferred in from another session |
| `events.dropped` | Events dropped due to client backpressure |
| `recording.stopped` | Recording stopped externally |
| `session.idle_timeout` | Session destroyed due to inactivity |
| `session.empty_timeout` | Session destroyed due to zero endpoints timeout |
| `session.orphaned` | Control connection dropped, timeout running |
| `stats` | Periodic session statistics |
| `vad.speech_started` | Speech detected after silence |
| `vad.silence` | Periodic silence notification |

See [Events Reference](docs/protocol/events.md) for detailed payload schemas.

### HTTP REST API

rtpbridge also serves HTTP endpoints on the same listen address. See [Configuration — HTTP REST API](docs/guide/configuration.md#http-rest-api) for full details including status codes and pagination.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check — returns `{"status":"ok"}` |
| `/metrics` | GET | Prometheus-format metrics |
| `/sessions` | GET | List all active sessions |
| `/sessions/{id}` | GET | Get session details |
| `/recordings` | GET | List PCAP recording files |
| `/recordings/{path}` | GET | Download a recording file |
| `/recordings/{path}` | DELETE | Delete a recording file |

## Architecture

```
                    WebSocket Control (JSON)
                           |
                    +------v------+
                    | SessionMgr  |  DashMap<SessionId, Session>
                    +------+------+
                           |
              +------------v------------+
              |     Session Task        |  (one tokio task per session)
              |                         |
              |  +--------+ +--------+  |
              |  | WebRTC | |  RTP/  |  |
              |  |  str0m | | SRTP   |  |  +--------+  +----------+
              |  +---+----+ +---+----+  |  |  File   |  | Recording|
              |      |          |       |  |Playback |  |  (PCAP)  |
              |      +----+-----+       |  +----+----+  +----------+
              |           |             |       |
              |    Routing Table        |       |
              |  (sendrecv/recvonly/    |       |
              |   sendonly directions)  |       |
              +-------------------------+-------+
                           |
              +------------v------------+
              |    Per-Endpoint UDP     |
              |    Sockets              |
              +-------------------------+
```

## Testing

```bash
# Unit tests
cargo test

# Integration tests (serial — spawns server processes)
cargo test -- --test-threads=1
```

## License

MIT

## Links

- [Documentation](https://zyno-io.github.io/rtpbridge/)
- [Source](https://github.com/zyno-io/rtpbridge)
