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
| `rtpbridge_events_dropped_total` | WebSocket events dropped due to client backpressure |

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
| `rate(srtp_errors_total[5m])` | 0 | > 0.1/s | > 1/s sustained 5m |
| `rate(transcode_errors_total[5m])` | 0 | > 0 | > 0 sustained 10m |
| `rate(events_dropped_total[5m])` | 0 | > 0 | > 0 sustained 5m |
| `sessions_active` vs configured `max_sessions` | < 50% | > 70% | > 80% |
| `rate(packets_routed_total[5m])` with active sessions | > 0 | == 0 for 2m | == 0 for 5m |

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
