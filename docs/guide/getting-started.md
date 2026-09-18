# Getting Started

## Prerequisites

- Rust 1.94+ with Cargo
- A C compiler, make, Perl, curl and shasum for the pinned OpenSSL build
- libopus-dev (Debian/Ubuntu), libopus-devel (Fedora), or opus (macOS Homebrew) — required for Opus codec support
- A WebSocket client for testing (e.g., [websocat](https://github.com/vi/websocat))

## Build

```bash
git clone https://github.com/zyno-io/rtpbridge.git
cd rtpbridge
sh scripts/build-openssl.sh target/openssl
export OPENSSL_DIR="$PWD/target/openssl" OPENSSL_STATIC=1
cargo build --release
```

The helper verifies and builds OpenSSL 3.6.4; CI and the container use the same source checksum. Keep these environment variables set for subsequent Cargo commands. A system OpenSSL 3.5.8+, 3.6.4+, or a later release line can also be used without the helper. Startup checks the linked version to reject releases missing the [August 2026 DTLS security fix](https://openssl-library.org/news/secadv/20260825.txt). The previous automatic vendored build has been removed because its compatible Rust package still embeds 3.6.3.

## Run

```bash
# Default: WS on 127.0.0.1:9100, media on 127.0.0.1
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
{"id":"2","method":"endpoint.webrtc.create_offer","params":{"direction":"sendrecv"}}
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
