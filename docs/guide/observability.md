# Monitoring & Observability

## Metrics Overview

rtpbridge exposes Prometheus metrics at `GET /metrics` on the control plane port (default: 9100) in OpenMetrics text format.

All metrics use the `rtpbridge_` prefix.

### Counters

| Metric | Description |
|--------|-------------|
| `rtpbridge_sessions_total` | Total sessions created since startup |
| `rtpbridge_endpoints_total` | Total endpoints created since startup |
| `rtpbridge_packets_routed_total` | Total RTP packets routed between endpoints |
| `rtpbridge_packets_recorded_total` | Total packets written to PCAP recordings |
| `rtpbridge_srtp_errors_total` | SRTP authentication or replay check failures |
| `rtpbridge_transcode_errors_total` | Codec transcode failures (decode or encode) |
| `rtpbridge_dtmf_events_total` | DTMF digits detected |
| `rtpbridge_playout_late_drops_total` | Playout packets dropped after their play slot |
| `rtpbridge_playout_overflow_drops_total` | Playout frames dropped to bound latency |
| `rtpbridge_playout_underflow_fills_total` | Synthesized silence fills for clockless-source underflow |
| `rtpbridge_events_dropped_total` | WebSocket events dropped due to client backpressure |
| `rtpbridge_webrtc_packet_errors_total` | Inbound WebRTC packets rejected by str0m |
| `rtpbridge_webrtc_connecting_stuck_total` | WebRTC endpoints stuck in Connecting past the watchdog threshold |
| `rtpbridge_webrtc_ice_restart_conflicts_total` | ICE restart requests rejected because an unanswered offer was pending |
| `rtpbridge_webrtc_recv_task_started_total` | WebRTC receive tasks that reached their receive loop |
| `rtpbridge_webrtc_recv_task_exited_total` | WebRTC receive tasks that exited cooperatively |
| `rtpbridge_webrtc_recv_task_dead_total` | Live WebRTC endpoints whose receive task had finished |
| `rtpbridge_webrtc_recv_task_start_timeout_total` | WebRTC receive tasks that did not start within the grace window |
| `rtpbridge_webrtc_recv_overflow_total` | Inbound WebRTC packets dropped because the session channel was full |
| `rtpbridge_webrtc_udp_send_ok_total` | WebRTC UDP datagrams successfully handed to the OS socket |
| `rtpbridge_webrtc_udp_send_dropped_total` | WebRTC UDP datagrams dropped at the socket send step |

### Gauges

| Metric | Description |
|--------|-------------|
| `rtpbridge_sessions_active` | Currently active sessions |
| `rtpbridge_endpoints_active` | Currently active endpoints |
| `rtpbridge_recordings_active` | Currently active PCAP recordings |

## Interpreting Metrics

### Throughput

`rate(rtpbridge_packets_routed_total[5m])` gives you the packet routing rate. For reference:
- A single two-party call at 20ms ptime generates ~100 packets/sec (50 in each direction)
- At 10ms ptime, this doubles to ~200 packets/sec

Scale linearly with active sessions to estimate expected throughput.

### Error Rates

**SRTP errors** — `rate(rtpbridge_srtp_errors_total[5m])` should be near zero in normal operation. A sustained rate above 1/sec usually indicates a key mismatch or network-level packet corruption. Investigate the specific session causing errors.

**Transcode errors** — `rate(rtpbridge_transcode_errors_total[5m])` should be zero. Non-zero values indicate codec failures (e.g., corrupt Opus frames). Occasional errors on noisy links are expected; sustained errors suggest a codec negotiation problem.

**Events dropped** — `rate(rtpbridge_events_dropped_total[5m])` > 0 means clients aren't reading WebSocket events fast enough. This doesn't affect media flow but means your application is missing events (DTMF, VAD, stats).

**WebRTC receive supervision** — `increase(rtpbridge_webrtc_recv_task_start_timeout_total[15m])` or `increase(rtpbridge_webrtc_recv_task_dead_total[15m])` > 0 means a live WebRTC endpoint lost, or never started, its UDP receive task. Treat this as a media-path incident.

**WebRTC send results** — endpoint outbound RTP counters mean media was written
into str0m. `rate(rtpbridge_webrtc_udp_send_ok_total[1m])` and
`rate(rtpbridge_webrtc_udp_send_dropped_total[1m])` show what happened at the UDP
socket send step. Any sustained non-zero drop rate means rtpbridge is failing to
hand datagrams to the kernel or str0m selected a local candidate base that no
bound endpoint socket owns.

**Playout drops/fills** — `rate(rtpbridge_playout_late_drops_total[5m])`, `rate(rtpbridge_playout_overflow_drops_total[5m])`, and `rate(rtpbridge_playout_underflow_fills_total[5m])` show buffering pressure for paced sources. Occasional fills on bursty clockless sources can be normal; sustained drops indicate jitter, overload, or a producer running ahead.

### Capacity

`rtpbridge_sessions_active` relative to `max_sessions` tells you how much headroom you have. If you're consistently above 80% capacity, scale out.

`rtpbridge_endpoints_active / rtpbridge_sessions_active` gives you the average endpoints per session. A sudden increase might indicate a misconfigured client creating too many endpoints.

## Alerting

### Recommended Alert Rules

```yaml
groups:
  - name: rtpbridge
    rules:
      # Instance down
      - alert: RtpbridgeDown
        expr: up{job="rtpbridge"} == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "rtpbridge instance {{ $labels.instance }} is down"

      # SRTP errors sustained
      - alert: RtpbridgeSrtpErrors
        expr: rate(rtpbridge_srtp_errors_total[5m]) > 1
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Sustained SRTP errors on {{ $labels.instance }}"

      # Transcode errors
      - alert: RtpbridgeTranscodeErrors
        expr: rate(rtpbridge_transcode_errors_total[5m]) > 0
        for: 10m
        labels:
          severity: warning
        annotations:
          summary: "Transcode errors on {{ $labels.instance }}"

      # Client backpressure
      - alert: RtpbridgeEventsDropped
        expr: rate(rtpbridge_events_dropped_total[5m]) > 0
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Events being dropped on {{ $labels.instance }} — client too slow"

      # WebRTC receive task lost
      - alert: RtpbridgeWebrtcRecvTaskDead
        expr: increase(rtpbridge_webrtc_recv_task_dead_total[15m]) > 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "Live WebRTC endpoint lost its receive task on {{ $labels.instance }}"

      # WebRTC receive task never started
      - alert: RtpbridgeWebrtcRecvTaskStartTimeout
        expr: increase(rtpbridge_webrtc_recv_task_start_timeout_total[15m]) > 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "WebRTC receive task failed to start on {{ $labels.instance }}"

      # Approaching session capacity
      # Adjust the threshold to 80% of your configured max_sessions value.
      - alert: RtpbridgeHighSessionLoad
        expr: rtpbridge_sessions_active > 4000
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "{{ $labels.instance }} is above 80% session capacity"

      # No packets routed (but sessions active)
      - alert: RtpbridgeNoTraffic
        expr: >
          rtpbridge_sessions_active > 0
          and rate(rtpbridge_packets_routed_total[5m]) == 0
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Active sessions but no packets routed on {{ $labels.instance }}"
```

### Threshold Guidelines

| Metric | Normal | Investigate | Alert |
|--------|--------|-------------|-------|
| `rate(rtpbridge_srtp_errors_total[5m])` | 0 | > 0.1/s | > 1/s sustained 5m |
| `rate(rtpbridge_transcode_errors_total[5m])` | 0 | > 0 | > 0 sustained 10m |
| `rate(rtpbridge_events_dropped_total[5m])` | 0 | > 0 | > 0 sustained 5m |
| `increase(rtpbridge_webrtc_recv_task_dead_total[15m])` | 0 | > 0 | > 0 |
| `increase(rtpbridge_webrtc_recv_task_start_timeout_total[15m])` | 0 | > 0 | > 0 |
| `rate(rtpbridge_webrtc_udp_send_dropped_total[1m])` | 0 | > 0 | > 0 sustained 2m |
| `rtpbridge_sessions_active` vs configured `max_sessions` | < 50% | > 70% | > 80% |
| `rate(rtpbridge_packets_routed_total[5m])` with active sessions | > 0 | == 0 for 2m | == 0 for 5m |

## Dashboards

### Grafana Dashboard Panels

Recommended panels for a Grafana dashboard:

**Row 1 — Overview**
- Sessions active (gauge) — `rtpbridge_sessions_active`
- Endpoints active (gauge) — `rtpbridge_endpoints_active`
- Packet rate (graph) — `rate(rtpbridge_packets_routed_total[1m])`
- Recordings active (gauge) — `rtpbridge_recordings_active`

**Row 2 — Errors**
- SRTP error rate (graph) — `rate(rtpbridge_srtp_errors_total[1m])`
- Transcode error rate (graph) — `rate(rtpbridge_transcode_errors_total[1m])`
- Events dropped rate (graph) — `rate(rtpbridge_events_dropped_total[1m])`

**Row 3 — Cumulative**
- Total sessions (counter) — `rtpbridge_sessions_total`
- Total DTMF events (counter) — `rtpbridge_dtmf_events_total`
- Packets recorded (counter) — `rtpbridge_packets_recorded_total`

**Row 4 — WebRTC / Playout**
- WebRTC receive task failures — `increase(rtpbridge_webrtc_recv_task_dead_total[15m])`, `increase(rtpbridge_webrtc_recv_task_start_timeout_total[15m])`
- WebRTC receive overflow — `rate(rtpbridge_webrtc_recv_overflow_total[1m])`
- WebRTC UDP send drops — `rate(rtpbridge_webrtc_udp_send_dropped_total[1m])`
- Playout drops/fills — `rate(rtpbridge_playout_late_drops_total[1m])`, `rate(rtpbridge_playout_overflow_drops_total[1m])`, `rate(rtpbridge_playout_underflow_fills_total[1m])`

## Per-Session Observability

Beyond Prometheus metrics, rtpbridge provides per-session observability through the WebSocket control protocol:

### Statistics Subscription

```json
{"id":"1","method":"stats.subscribe","params":{"interval_ms":5000}}
```

Returns per-endpoint stats including packet counts, loss, jitter, RTT, codec, and state. See [Statistics](../protocol/stats.md) for full details.

### Event Stream

All session events (DTMF, state changes, VAD, recording stops, media timeouts) are delivered as JSON over the same WebSocket connection. See [Events](../protocol/events.md) for the full list.

### HTTP Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health check — returns `{"status":"ok"}` |
| `GET /metrics` | Prometheus metrics |
| `GET /sessions` | List active sessions with endpoint counts |
| `GET /recordings` | List recording files with pagination |
