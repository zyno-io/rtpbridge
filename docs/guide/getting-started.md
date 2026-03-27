# Getting Started

## Prerequisites

- Rust 1.88+ with Cargo
- libopus-dev (Debian/Ubuntu), libopus-devel (Fedora), or opus (macOS Homebrew) — required for Opus codec support
- A WebSocket client for testing (e.g., [websocat](https://github.com/vi/websocat))

## Build

```bash
git clone https://github.com/zyno-io/rtpbridge.git
cd rtpbridge
cargo build --release
```

## Run

```bash
# Default: WS on 0.0.0.0:9100, media on 127.0.0.1
./target/release/rtpbridge

# With specific media IP
./target/release/rtpbridge --media-ip 203.0.113.5

# With config file
./target/release/rtpbridge --config rtpbridge.toml
```

## Your First Session

Connect with a WebSocket client and create a session:

```bash
websocat ws://localhost:9100
```

```json
{"id":"1","method":"session.create","params":{}}
```

Response:
```json
{"id":"1","result":{"session_id":"550e8400-e29b-41d4-a716-446655440000"}}
```

Create a WebRTC endpoint with an SDP offer:

```json
{"id":"2","method":"endpoint.create_offer","params":{"type":"webrtc","direction":"sendrecv"}}
```

Response includes the SDP offer to send to the remote peer:
```json
{"id":"2","result":{"endpoint_id":"...","sdp_offer":"v=0\r\no=..."}}
```

## Test

```bash
cargo test                      # all tests
cargo test -- --test-threads=1  # integration tests (serial)
```
