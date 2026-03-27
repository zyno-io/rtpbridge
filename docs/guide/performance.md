# Performance Tuning

## Codec Selection

The choice of codec has a significant impact on CPU usage due to transcoding overhead. rtpbridge supports three codecs with different characteristics:

| Codec | Sample Rate | Bandwidth (20ms ptime) | CPU Cost | Quality |
|-------|-------------|----------------------|----------|---------|
| PCMU (G.711) | 8 kHz | 64 kbps | Minimal | Narrowband (telephone) |
| G.722 | 16 kHz | 64 kbps | Low | Wideband |
| Opus | 48 kHz | ~24 kbps (VBR) | High | Fullband |

### Transcoding Impact

When two endpoints in a session use different codecs, rtpbridge transcodes every packet through a decode-resample-encode pipeline. The cost depends on the codec pair:

| Path | Relative Cost | Notes |
|------|--------------|-------|
| Same codec (passthrough) | ~0 | No transcoding, packets forwarded directly |
| PCMU <-> G.722 | Low | Resample 8 kHz <-> 16 kHz + codec |
| PCMU <-> Opus | High | Resample 8 kHz <-> 48 kHz + Opus encode/decode |
| G.722 <-> Opus | High | Resample 16 kHz <-> 48 kHz + Opus encode/decode |

Opus encoding is the most expensive operation. If you control both sides, prefer matching codecs to avoid transcoding entirely.

### Recommendations

- **Homogeneous deployments** — If all endpoints use the same codec, there's zero transcoding overhead. This is the most efficient configuration.
- **WebRTC to PSTN** — WebRTC typically uses Opus; PSTN uses PCMU. This is the most expensive transcoding path. If CPU is a concern, consider G.722 as a compromise for the WebRTC side.
- **Opus bitrate** — rtpbridge uses 24 kbps for Opus encoding (VoIP application mode). This is fixed and optimized for voice.

## Ptime Values

Ptime (packetization time) determines how many milliseconds of audio each RTP packet carries. rtpbridge uses 20ms ptime, which means:

| Codec | Samples per Packet | Bytes per Packet | Packets per Second |
|-------|-------------------|------------------|-------------------|
| PCMU | 160 | 160 | 50 |
| G.722 | 320 | 160 | 50 |
| Opus | 960 | ~60 (VBR) | 50 |

At 50 packets/second per direction, a two-party session generates 100 packets/second of routing work. A three-party session (full mesh) generates 300 packets/second.

## Resource Sizing

### CPU

CPU usage is dominated by:
1. **Transcoding** — Opus encode/decode is the primary CPU consumer
2. **Packet routing** — Negligible per-packet cost
3. **SRTP** — Encrypt/decrypt adds modest overhead per packet

**Rules of thumb:**
- Passthrough sessions (same codec, no SRTP): ~100-200 per CPU core
- PCMU<->Opus transcoding sessions: ~50-100 per CPU core
- Actual numbers depend heavily on hardware; benchmark your specific deployment

### Memory

Memory usage is modest:
- ~128 MB base footprint
- ~1-5 KB per session (routing tables, state)
- ~10-50 KB per active transcoding pipeline (codec state, resample buffers)
- ~1 KB per active recording (channel buffer overhead)

512 MB is sufficient for most deployments. 1 GB provides comfortable headroom for thousands of sessions.

### Network

Each endpoint consumes:
- 1–2 UDP sockets (1 for WebRTC, 2 for plain RTP without rtcp-mux)
- Bandwidth proportional to codec bitrate and ptime

For plain RTP endpoints, each endpoint uses one port from `rtp_port_range`. The default range (30000-39999) provides 10,000 ports. With even/odd pairing, this supports up to 5,000 concurrent RTP endpoint pairs.

### Disk I/O (Recordings)

Each active recording writes PCAP data at approximately:
- PCMU: ~10 KB/s (160 bytes payload + 42 bytes headers per packet, 50 pps)
- Opus: ~5 KB/s (variable payload + 42 bytes headers per packet, 50 pps)

A continuous 24-hour recording at PCMU rates (~10 KB/s) produces approximately 864 MB. Plan disk provisioning accordingly for long-lived sessions.

For heavy recording workloads, use a separate volume with adequate IOPS. If the recording writer falls behind, packets are dropped from the recording (media routing is unaffected).

### Recording Flush Timeout

When a recording is stopped, the background writer task must flush buffered data to disk. The `recording_flush_timeout_secs` configuration (default: 10) controls how long to wait for this flush. If the flush does not complete in time, the task is aborted and the PCAP file may be truncated. Increase this value for slow storage (NFS, network-attached block devices):

```toml
recording_flush_timeout_secs = 30  # 30s for slow NFS
```

## Configuration Tuning

### Port Range

The default `rtp_port_range = [30000, 39999]` provides 10,000 ports. Increase this if you need more concurrent plain RTP endpoints:

```toml
rtp_port_range = [20000, 49999]  # 30,000 ports
```

The start port must be even (RTP uses even/odd port pairs) and >= 1024.

### Session Limits

Set limits to protect against resource exhaustion:

```toml
max_sessions = 5000                 # Hard cap on concurrent sessions
max_endpoints_per_session = 10      # Prevent runaway endpoint creation
max_recordings_per_session = 100    # Cap disk usage per session
```

Set to 0 for unlimited (not recommended in production).

### WebSocket Tuning

```toml
ws_max_message_size_kb = 256    # Max WebSocket message size
max_sdp_size_kb = 64            # Max SDP offer/answer size
```

Increase `max_sdp_size_kb` if you have endpoints with many codec lines or ICE candidates. The default of 64 KB handles virtually all real-world SDPs.

### Disconnect and Idle Timeouts

```toml
disconnect_timeout_secs = 30        # Time to keep orphaned sessions alive
session_idle_timeout_secs = 0       # 0 = disabled; auto-destroy idle sessions
```

In production, consider setting `session_idle_timeout_secs` to a reasonable value (e.g., 300) to prevent leaked sessions from consuming resources.

### File Descriptors

Each session can consume multiple file descriptors (UDP sockets, recording files). Set the process fd limit accordingly:

```bash
ulimit -n 65536
```

Or in your systemd unit:

```ini
[Service]
LimitNOFILE=65536
```

For large deployments (thousands of sessions), you may need 100,000+.

## Scaling

rtpbridge is a single-process server. To scale beyond a single instance:

- **Horizontal scaling** — Run multiple instances behind a load balancer. Sessions are independent and don't share state between instances. Route all WebSocket connections for a given session to the same instance (sticky sessions).
- **Vertical scaling** — rtpbridge uses tokio's multi-threaded runtime and scales well across CPU cores. More cores = more concurrent transcoding.
- **Shutdown draining** — Use graceful shutdown (`shutdown_max_wait_secs`) to drain sessions before stopping an instance. See [Deployment](./deployment.md) for Kubernetes configuration.
