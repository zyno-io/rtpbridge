# Troubleshooting

## Common Failure Modes

### Session creation fails with `SHUTTING_DOWN`

The server is in graceful shutdown (received SIGINT/SIGTERM). New sessions are rejected while existing sessions drain. Wait for the restart to complete or redirect traffic to another instance.

### Endpoint creation fails with SDP errors

- **"SDP too large"** — The offer exceeds `max_sdp_size_kb` (default: 64 KB). If legitimate, increase the limit in your config.
- **"No supported codec"** — The SDP offer doesn't include any of the supported codecs (PCMU, G.722, Opus). Ensure the remote side offers at least one.
- **"Port allocation failed"** — All ports in `rtp_port_range` are in use. Either increase the range or reduce the number of concurrent endpoints.

### WebSocket connection drops

When a control WebSocket disconnects, the session enters an orphaned state for `disconnect_timeout_secs` (default: 30s). During this window:
- Media continues flowing between endpoints
- The session can be reclaimed via `session.attach` on a new connection
- After the timeout, the session and all its endpoints are destroyed

If you see frequent orphaning, check network stability between your application and rtpbridge.

### No audio / one-way audio

1. **Check endpoint directions** — Verify `sendrecv` / `sendonly` / `recvonly` are set correctly. A `recvonly` endpoint won't forward media.
2. **Check routing** — Use `stats.subscribe` to monitor packet counts. If `inbound.packets` increments but `outbound.packets` doesn't, the routing table may not include the expected path.
3. **Check codec mismatch** — If two endpoints negotiate different codecs, transcoding kicks in automatically. Monitor `rtpbridge_transcode_errors_total` for failures.
4. **Check NAT/firewall** — For plain RTP endpoints, ensure the remote can reach the `media_ip` and allocated ports. WebRTC endpoints use ICE and handle NAT traversal.

### SRTP errors

A rising `rtpbridge_srtp_errors_total` counter indicates authentication or replay failures. Common causes:
- **Key mismatch** — The SRTP keys in the SDP don't match what the remote is using.
- **Replay attack detection** — Duplicate or reordered packets beyond the replay window. Usually indicates network issues.
- **SSRC collision** — Two sources using the same SSRC. Rare but possible.

### Recording failures

- **"Cannot create recording file"** — The `recording_dir` doesn't exist, isn't writable, or disk is full.
- **"Maximum concurrent recordings reached"** — Hit the `max_recordings_per_session` limit (default: 100). Stop unused recordings or increase the limit.
- **Channel full warnings** — The recording writer can't keep up with packet rate. This usually means disk I/O is saturated. Packets are dropped from the recording (not from the media path).

### Events dropped

`rtpbridge_events_dropped_total` incrementing means the WebSocket client isn't consuming events fast enough. The event channel has a fixed buffer; when full, new events are dropped. Ensure your client reads messages promptly without blocking.

### Session idle timeout

If `session_idle_timeout_secs` is configured (non-zero), sessions with no media packets and no commands for that duration are automatically destroyed. Check this setting if sessions are disappearing unexpectedly.

## Debugging

### Enable debug logging

```bash
rtpbridge --log-level debug
```

Or in your config file:

```toml
log_level = "debug"
```

Use `trace` for maximum verbosity (includes per-packet logging).

### Inspect live sessions

Query the HTTP API on the control plane port:

```bash
# List all active sessions
curl http://localhost:9100/sessions

# Check server health
curl http://localhost:9100/health

# Dump Prometheus metrics
curl http://localhost:9100/metrics
```

### Inspect recordings with Wireshark

Recording files are standard PCAP format with synthetic Ethernet/IPv4/UDP headers. Open them directly in Wireshark:

```bash
wireshark /var/lib/rtpbridge/recordings/session-abc/recording.pcap
```

Wireshark will automatically dissect RTP/RTCP protocols. Endpoints are assigned deterministic synthetic IP addresses (10.x.0.y:10000) for easy filtering. The bridge side of outbound packets uses 10.255.0.1.

### Use stats for live diagnosis

Subscribe to per-endpoint statistics over the WebSocket:

```json
{"id":"1","method":"stats.subscribe","params":{"interval_ms":1000}}
```

Key fields to watch:
- `inbound.packets` / `outbound.packets` — Are packets flowing?
- `inbound.packets_lost` — Network-level loss
- `inbound.jitter_ms` — Network jitter
- `inbound.last_received_ms_ago` — Time since last packet (high values indicate stalled media)
- `rtt_ms` — Round-trip time (WebRTC/RTCP only)

### Check resource limits

If sessions or endpoints fail to create, verify your limits:

```toml
max_sessions = 10000               # 0 = unlimited
max_endpoints_per_session = 20     # 0 = unlimited
max_recordings_per_session = 100
```

Also check OS-level limits:
- **Open file descriptors** — Each UDP socket and recording file consumes an fd. With 10,000 sessions and multiple endpoints each, you may need `ulimit -n` in the hundreds of thousands.
- **UDP port range** — The `rtp_port_range` must have enough ports for all concurrent plain RTP endpoints. WebRTC endpoints use separate ports outside this range.
