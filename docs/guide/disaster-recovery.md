# Disaster Recovery

## Session Recovery After Crash

rtpbridge does not persist session state to disk. If the process crashes or is killed (SIGKILL), all in-flight sessions are lost. Recovery depends on your application's reconnection logic.

### Graceful Shutdown vs Crash

| Scenario | Behavior |
|----------|----------|
| SIGINT / SIGTERM | Graceful shutdown: stops accepting new sessions, waits for existing sessions to drain (up to `shutdown_max_wait_secs`), then exits |
| SIGKILL / OOM kill / crash | Immediate termination: all sessions, endpoints, and recordings are lost |
| Process restart | Clean start with zero sessions; no state is carried over |

### Application-Level Recovery

Since rtpbridge is stateless between restarts, your application must handle reconnection:

1. **Detect disconnection** — Monitor the WebSocket connection to rtpbridge. When it drops, your application should treat all sessions on that instance as lost.
2. **Re-establish sessions** — Create new sessions and endpoints on the restarted (or replacement) instance.
3. **Re-negotiate media** — Send new SDP offers to remote participants to establish media paths through the new endpoints.

### Orphan Timeout Behavior

If only the control WebSocket drops (not the rtpbridge process), sessions enter an orphaned state:

- Media continues flowing between endpoints for `disconnect_timeout_secs` (default: 30s)
- Your application can reclaim the session via `session.attach` on a new WebSocket connection within this window
- If the timeout expires, the session is destroyed

This handles transient network issues between your application and rtpbridge without disrupting media.

### Kubernetes Considerations

For planned restarts (rolling deployments, node drains):

```yaml
spec:
  terminationGracePeriodSeconds: 86400  # Non-default: for long-session scenarios (default shutdown_max_wait_secs is 300s)
```

The graceful shutdown coordinator:
1. Stops accepting new WebSocket connections
2. Rejects new `session.create` requests with `SHUTTING_DOWN` error
3. Waits for all active sessions to complete naturally
4. Force-exits after `shutdown_max_wait_secs`

During a rolling update, new traffic goes to the new pod while existing sessions drain on the old pod. Set `shutdown_max_wait_secs` to the maximum expected session duration.

## Recording File Recovery

### PCAP File Format

Recordings are written as standard PCAP files (libpcap format) with Ethernet link type. Each file:
- Starts with a 24-byte PCAP global header (magic number `0xa1b2c3d4`)
- Contains sequential packet records, each with a 16-byte header followed by frame data
- Frames contain synthetic Ethernet + IPv4 + UDP headers wrapping the raw RTP/RTCP payload

### Validating Recording Files

After a crash, recording files may be incomplete (missing the final packets or truncated mid-write). To check if a PCAP file is valid:

```bash
# Quick validation with capinfos (part of Wireshark)
capinfos recording.pcap

# Count packets
tshark -r recording.pcap | wc -l

# Check for truncation errors
editcap -F pcap recording.pcap /dev/null 2>&1
```

### Incomplete Recordings

PCAP is a streaming format — there is no footer or end marker. A truncated file is still valid up to the last complete packet record. Wireshark and tshark will read all complete packets and stop at the truncation point.

Recording files may be incomplete after a crash because:
- The recording task uses a `BufWriter` with an internal buffer. On normal stop, this buffer is explicitly flushed. On a crash, unflushed data is lost.
- The `recording_flush_timeout_secs` config (default: 10s) controls how long a recording stop waits for the writer to flush before aborting.

**Impact**: At most ~8 KB of buffered data may be lost on crash (the default `BufWriter` buffer size), which corresponds to roughly 50-100 packets depending on codec.

### Recovering Partial Files

Partial PCAP files can still be analyzed:

```bash
# Open directly in Wireshark — it handles truncated files gracefully
wireshark recording.pcap

# Extract RTP streams from a partial capture
tshark -r recording.pcap -Y rtp -T fields -e rtp.ssrc -e rtp.seq -e rtp.timestamp

# Filter by synthetic endpoint address
tshark -r recording.pcap -Y "ip.src == 10.1.0.1"
```

### Synthetic Addresses in Recordings

Endpoints are assigned deterministic synthetic IP addresses in PCAP files:

| Endpoint | Address Pattern |
|----------|----------------|
| First endpoint | `10.1.0.1:10000` |
| Second endpoint | `10.1.0.2:10000` |
| Nth endpoint | `10.{(N/254)+1}.0.{(N%254)+1}:10000` |
| Bridge (outbound side) | `10.255.0.1:10000` |

Use these addresses as Wireshark display filters to isolate traffic for a specific endpoint.

### Storage Considerations

- **Disk space** — PCAP files grow at ~5-10 KB/s per recording depending on codec. Plan storage accordingly for long recordings.
- **Separate volume** — Use a dedicated volume for `recording_dir` so that disk pressure from recordings doesn't affect the main application.
- **Cleanup** — rtpbridge does not automatically delete old recordings. Implement your own retention policy (e.g., a cron job or external cleanup process).
- **File paths** — Recordings are validated at creation time. If the `recording_dir` becomes unavailable (e.g., unmounted volume), new recordings will fail with a clear error. Existing recordings will stop with a write error and emit a `recording.stopped` event.

## Backup and Redundancy

rtpbridge is designed as a stateless media plane component:

- **No persistent state** — Nothing to back up. All state is ephemeral and held in memory.
- **No clustering** — Each instance is independent. Sessions don't migrate between instances.
- **Redundancy** — Run multiple instances behind a load balancer. If one crashes, only its sessions are affected. New sessions are created on healthy instances.
- **Recording durability** — If recording durability matters, write to a networked/replicated filesystem (e.g., EBS, NFS, Ceph). Local disk recordings are lost if the node fails.
