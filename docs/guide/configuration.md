# Configuration

rtpbridge is configured via CLI arguments and/or a TOML config file. CLI arguments override config file values.

## CLI Arguments

```bash
rtpbridge [OPTIONS]
```

| Argument | Default | Description |
|----------|---------|-------------|
| `-l, --listen <ADDRS>` | `0.0.0.0:9100` | WebSocket/HTTP control plane listen address(es), comma-separated |
| `-m, --media-ip <IP>` | `127.0.0.1` | IP address for all media sockets |
| `-c, --config <PATH>` | — | Path to TOML configuration file |
| `--legacy-ice-renomination` | disabled | Advertise legacy libwebrtc ICE re-nomination for WebRTC endpoints |
| `--log-level <LEVEL>` | `info` | Log level: trace, debug, info, warn, error |

## TOML Config File

```toml
# WebSocket/HTTP control plane (comma-separated addresses accepted)
listen = "0.0.0.0:9100"

# Media plane IP(s) — used for RTP sockets and SDP/ICE candidates.
# A single address, or a comma-separated IPv4+IPv6 pair for dual-stack.
media_ip = "203.0.113.5"
# media_ip = "203.0.113.5, 2001:db8::5"   # dual-stack

# UDP port range for plain RTP endpoints [start, end]
rtp_port_range = [30000, 39999]
legacy_ice_renomination = false

# Session disconnect timeout (seconds)
# When a WS connection drops, the session stays alive for this long
disconnect_timeout_secs = 30

# Maximum shutdown drain wait (seconds)
# On SIGINT/SIGTERM, wait this long for sessions to finish
shutdown_max_wait_secs = 300

# File cache directory for URL downloads
cache_dir = "/tmp/rtpbridge-cache"

# How often to clean up expired cache entries (seconds)
cache_cleanup_interval_secs = 300

# Allowed base directory for local file playback
# If unset, local file playback is disabled (only URLs allowed)
# media_dir = "/var/lib/rtpbridge/media"

# Base directory for PCAP recordings
# Recorded files are also served over HTTP from this path (GET /recordings/<path>)
recording_dir = "/var/lib/rtpbridge/recordings"

# Resource limits
max_sessions = 10000
max_endpoints_per_session = 20

# Maximum concurrent PCAP recordings per session
max_recordings_per_session = 100

# Seconds to wait for recording tasks to flush before aborting
recording_flush_timeout_secs = 10

# Maximum concurrent HTTP downloads for URL-based file playback (default: 16)
max_concurrent_downloads = 16

# Connection limits
max_connections = 1000
ws_ping_interval_secs = 30
event_channel_size = 256
critical_event_channel_size = 64

# WebSocket and SDP size limits
ws_max_message_size_kb = 256             # Max WebSocket message size
max_sdp_size_kb = 64                     # Max SDP offer/answer size

# Session idle timeout (0 = disabled)
session_idle_timeout_secs = 0

# Empty session timeout (0 = disabled)
# Sessions with zero endpoints for this duration are auto-destroyed
# empty_session_timeout_secs = 0

# Media timeout event threshold (seconds)
media_timeout_secs = 5

# Recording channel buffer size (packets)
recording_channel_size = 1000

# Transcode and download limits
transcode_cache_size = 64
max_file_download_bytes = 104857600       # 100 MB
max_recording_download_bytes = 536870912  # 512 MB

# Log level
log_level = "info"
```

## Split Interface Binding

rtpbridge supports binding the control plane and media plane to different network interfaces:

- **`listen`** — The WebSocket control plane. Bind to your management network (e.g., `10.0.1.5:9100`).
- **`media_ip`** — All RTP/WebRTC UDP sockets. Bind to your media network (e.g., `10.0.2.5`). Accepts a single address or a comma-separated list — at most one IPv4 and one IPv6 — to dual-bind both families on one instance.

The `media_ip` is used directly in:
- SDP `c=` lines (connection address)
- ICE host candidates for WebRTC
- Plain RTP socket binding

No STUN/TURN discovery is performed — the configured IP is assumed to be directly reachable.

### Dual-stack (IPv4 + IPv6)

Set `media_ip` to one IPv4 and one IPv6 address to serve both families from a single instance:

```toml
media_ip = "203.0.113.5, 2001:db8::5"
```

Behavior:
- **Plain RTP/SRTP** answers each offer with the family matching the remote SDP's `c=` line, allocating from that family's port pool. An offer for a family you did **not** configure is rejected (`ENDPOINT_ERROR`) rather than answered with an unreachable address. Re-negotiations (`update_remote_sdp`) that flip a bound endpoint's family are likewise rejected — sockets are not migrated across families.
- **WebRTC** binds one UDP socket per family and offers an ICE host candidate for each; ICE nominates the working pair. (Note: the ICE library's default local preference favors IPv6, so a dual-stack browser typically connects over IPv6.)
- **Offers rtpbridge originates** for plain RTP advertise a single `c=` line and prefer IPv4 when both families are configured.

Validation rejects unspecified (`0.0.0.0` / `::`) and multicast media addresses; loopback and link-local addresses warn (IPv6 link-local cannot carry a scope id in SDP).

**Limitation:** PCAP recordings always synthesize IPv4 framing regardless of the real media family — recorded packets do not carry real IPv6 headers.

## HTTP REST API

In addition to the WebSocket control protocol, rtpbridge serves HTTP endpoints on the same listen address:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check — returns `{"status":"ok"}` with `200 OK` |
| `/metrics` | GET | Prometheus-format metrics (OpenMetrics text exposition) |
| `/sessions` | GET | List all active sessions (JSON) |
| `/sessions/{id}` | GET | Get session details by UUID (JSON) |
| `/recordings` | GET | List `.pcap` recording files. Supports query params: `startsWith`, `skip`, `limit` |
| `/recordings/{path}` | GET | Download a specific PCAP recording file |
| `/recordings/{path}` | DELETE | Delete a specific PCAP recording file |

All HTTP responses include `Connection: close`. Recording file paths are validated against the configured `recording_dir` to prevent path traversal.

## Security

### Path Traversal Protection

Both `recording_dir` and `media_dir` enforce strict path validation:

- Paths containing `..` components are rejected outright
- Symlinks in parent directories are resolved (canonicalized) before validation
- The resolved path must reside within the configured base directory
- This eliminates TOCTOU races — the resolved path is used directly for file creation

The HTTP recording API (`GET /recordings/{path}`, `DELETE /recordings/{path}`) applies the same validation, preventing path traversal via HTTP requests.

### Response Status Codes

| Endpoint | Status | Condition |
|----------|--------|-----------|
| `GET /health` | `200 OK` | Always |
| `GET /metrics` | `200 OK` | Metrics encoded successfully |
| `GET /metrics` | `500 Internal Server Error` | Metrics encoding failed |
| `GET /sessions` | `200 OK` | Always (returns `[]` if none) |
| `GET /sessions/{id}` | `200 OK` | Session found |
| `GET /sessions/{id}` | `400 Bad Request` | Invalid UUID |
| `GET /sessions/{id}` | `404 Not Found` | No session with that ID |
| `GET /recordings` | `200 OK` | Directory listed successfully |
| `GET /recordings` | `500 Internal Server Error` | Recording directory not readable |
| `GET /recordings/{path}` | `200 OK` | File returned (`application/vnd.tcpdump.pcap`) |
| `GET /recordings/{path}` | `403 Forbidden` | Path traversal attempt detected |
| `GET /recordings/{path}` | `404 Not Found` | File does not exist |
| `GET /recordings/{path}` | `413 Payload Too Large` | Recording exceeds `max_recording_download_bytes` |
| `DELETE /recordings/{path}` | `200 OK` | File deleted (`{"deleted":true}`) |
| `DELETE /recordings/{path}` | `404 Not Found` | File does not exist |
| `DELETE /recordings/{path}` | `500 Internal Server Error` | Deletion failed |
| Any | `405 Method Not Allowed` | Unsupported HTTP method for route |

### Response Schemas

#### GET /sessions/{id}

```json
{
  "session_id": "550e8400-e29b-41d4-a716-446655440000",
  "state": "active",
  "created_at": "2024-01-15T10:30:00Z",
  "endpoints": [
    {
      "endpoint_id": "...",
      "endpoint_type": "rtp",
      "state": "connected",
      "direction": "sendrecv",
      "codec": "PCMU",
      "local_rtp_addr": "203.0.113.5:30000",
      "local_rtcp_addr": "203.0.113.5:30001",
      "remote_rtp_addr": "198.51.100.10:40000",
      "remote_rtcp_addr": "198.51.100.10:40001"
    }
  ],
  "recordings": [
    {
      "recording_id": "...",
      "file_path": "/var/lib/rtpbridge/recordings/call-1.pcap",
      "endpoint_id": null,
      "state": "active"
    }
  ],
  "vad_active": ["endpoint-id-1"],
  "fax_detect_active": ["endpoint-id-1"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string (UUID) | The session identifier |
| `state` | string | `"active"` or `"orphaned"` |
| `created_at` | string (RFC 3339) | When the session was created |
| `endpoints` | array | List of endpoints in the session |
| `recordings` | array | Active recordings |
| `vad_active` | array of strings | Endpoint IDs with active VAD |
| `fax_detect_active` | array of strings | Endpoint IDs with active fax tone detection |

### Recording Pagination

The `GET /recordings` endpoint supports pagination via query parameters:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `startsWith` | — | Filter recordings whose relative path starts with this prefix (URL-decoded) |
| `skip` | `0` | Number of results to skip |
| `limit` | `100` | Maximum results to return (capped at 1000) |

Response body:

```json
{
  "recordings": ["call-123.pcap", "call-456.pcap"],
  "total": 42,
  "skip": 0,
  "limit": 100
}
```

## Complete Configuration Reference

All configuration options with their types, defaults, and descriptions. All changes require a service restart.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `listen` | `ip:port[,ip:port...]` | `0.0.0.0:9100` | WebSocket/HTTP control plane listen address(es), comma-separated |
| `media_ip` | `ip[,ip]` | `127.0.0.1` | IP(s) for RTP/WebRTC UDP sockets; appears in SDP and ICE candidates. Comma-separated for dual-stack (≤1 IPv4, ≤1 IPv6) |
| `rtp_port_range` | `[u16, u16]` | `[30000, 39999]` | UDP port range for plain RTP endpoints (must start even, >= 1024) |
| `disconnect_timeout_secs` | `u64` | `30` | Seconds to keep orphaned sessions alive after WebSocket disconnect |
| `shutdown_max_wait_secs` | `u64` | `300` | Maximum wait for session drain on graceful shutdown |
| `media_dir` | `path?` | *(none)* | Base directory for local file playback; unset disables local files |
| `recording_dir` | `path` | `/var/lib/rtpbridge/recordings` | PCAP recording output and HTTP serving directory |
| `cache_dir` | `path` | `/tmp/rtpbridge-cache` | File cache directory for URL downloads |
| `cache_cleanup_interval_secs` | `u64` | `300` | Interval for cache cleanup of expired entries |
| `max_concurrent_downloads` | `usize` | `16` | Maximum concurrent HTTP downloads for URL file playback |
| `max_sessions` | `usize` | `10000` | Maximum concurrent sessions (0 = unlimited) |
| `max_endpoints_per_session` | `usize` | `20` | Maximum endpoints per session (0 = unlimited) |
| `max_recordings_per_session` | `usize` | `100` | Maximum concurrent recordings per session |
| `recording_flush_timeout_secs` | `u64` | `10` | Seconds to wait for recording tasks to flush on stop |
| `ws_max_message_size_kb` | `usize` | `256` | Maximum WebSocket message/frame size in KB |
| `max_sdp_size_kb` | `usize` | `64` | Maximum SDP size in KB; rejects oversized offers/answers |
| `session_idle_timeout_secs` | `u64` | `0` | Auto-destroy sessions with no activity for this duration (0 = disabled) |
| `empty_session_timeout_secs` | `u64` | `0` | Auto-destroy sessions with zero endpoints for this duration (0 = disabled) |
| `legacy_ice_renomination` | `bool` | `false` | Advertise legacy libwebrtc ICE re-nomination; disable as the rollout kill switch |
| `max_connections` | `usize` | `1000` | Maximum concurrent WebSocket connections (0 = unlimited) |
| `ws_ping_interval_secs` | `u64` | `30` | WebSocket ping interval for keepalive and dead connection detection |
| `event_channel_size` | `usize` | `256` | Buffer size for normal event channel per connection |
| `critical_event_channel_size` | `usize` | `64` | Buffer size for priority event channel per connection |
| `transcode_cache_size` | `usize` | `64` | Maximum entries in the transcode pipeline LRU cache per session |
| `max_file_download_bytes` | `u64` | `104857600` | Maximum file download size (100 MB) for URL playback |
| `max_recording_download_bytes` | `u64` | `536870912` | Maximum recording file size (512 MB) for HTTP GET serving |
| `recording_channel_size` | `usize` | `1000` | Buffer size (packets) for the channel between session task and recording writer task |
| `media_timeout_secs` | `u64` | `5` | Seconds without RTP packets before firing `endpoint.media_timeout` event |
| `log_level` | `string` | `info` | Log level: `trace`, `debug`, `info`, `warn`, `error` |

## Validation Rules

All values are validated at startup. Invalid configurations cause an immediate exit with a descriptive error.

### Numeric Lower Bounds

The following fields must be greater than zero:

`disconnect_timeout_secs`, `shutdown_max_wait_secs`, `cache_cleanup_interval_secs`,
`max_concurrent_downloads`, `max_recordings_per_session`, `recording_flush_timeout_secs`,
`ws_max_message_size_kb`, `max_sdp_size_kb`, `ws_ping_interval_secs`,
`event_channel_size`, `critical_event_channel_size`, `recording_channel_size`,
`transcode_cache_size`, `media_timeout_secs`

Note: `max_sessions`, `max_endpoints_per_session`, and `max_connections` accept `0` to mean unlimited.

### Numeric Upper Bounds

| Field | Maximum | Notes |
|-------|---------|-------|
| `ws_max_message_size_kb` | 512000 | 512 MB |
| `max_sdp_size_kb` | 10000 | 10 MB |
| `media_timeout_secs` | 300 | 5 minutes |

### Port Range Rules

- Start must be &le; end
- Both ports must be &ge; 1024 (privileged ports are not allowed)
- Start must be even (RTP uses even/odd port pairs per RFC 3550)
- End must be &le; 65534 (reserving room for the RTCP port in each pair)
- Range must span at least 2 ports, i.e. one even/odd pair (e.g., `[30000, 30001]`)

### Path Validation

- `media_dir` (if set): must exist and be a directory
- `recording_dir`: parent directory must exist (unless using the default `/var/lib/rtpbridge/recordings`); must be writable if it already exists
- `cache_dir`: parent directory must exist (unless using the default `/tmp/rtpbridge-cache`); must be writable if it already exists
