# Deployment

## Kubernetes

rtpbridge is designed for k8s deployment with graceful shutdown support.

### Graceful Shutdown

On `SIGINT` or `SIGTERM`:

1. Stop accepting new WebSocket connections
2. Stop accepting new `session.create` requests (returns `SHUTTING_DOWN` error)
3. Wait for all active sessions to complete naturally
4. Force shutdown after `shutdown_max_wait_secs` (default: 300 = 5 minutes)

During graceful shutdown, existing orphaned sessions remain attachable until their `disconnect_timeout_secs` expires or the server exits. Only new `session.create` calls are rejected with `SHUTTING_DOWN`. WebSocket connections already established continue to function normally.

Set `terminationGracePeriodSeconds` in your pod spec to match or exceed `shutdown_max_wait_secs`.

### Example Pod Spec

```yaml
apiVersion: v1
kind: Pod
spec:
  terminationGracePeriodSeconds: 300
  containers:
    - name: rtpbridge
      image: ghcr.io/zyno-io/rtpbridge:latest
      args:
        - --listen=0.0.0.0:9100
        - --media-ip=$(POD_IP)
        - --config=/etc/rtpbridge/config.toml
      ports:
        - containerPort: 9100
          name: control
          protocol: TCP
        - containerPort: 30000
          name: rtp-start
          protocol: UDP
      env:
        - name: POD_IP
          valueFrom:
            fieldRef:
              fieldPath: status.podIP
      resources:
        requests:
          cpu: 250m
          memory: 128Mi
        limits:
          cpu: "2"
          memory: 512Mi
      volumeMounts:
        - name: config
          mountPath: /etc/rtpbridge
  volumes:
    - name: config
      configMap:
        name: rtpbridge-config
```

Resource requirements depend on workload — transcoding (especially Opus) is CPU-intensive. Monitor actual usage and adjust accordingly.

### Port Ranges

For plain RTP endpoints, you need to expose the configured `rtp_port_range` (default: 30000-39999). WebRTC endpoints use ICE and can work with any available ports.

### Health Checks

rtpbridge exposes a simple HTTP health endpoint:

```
GET /health → 200 {"status":"ok"}
```

Use this for k8s liveness and readiness probes:

```yaml
livenessProbe:
  httpGet:
    path: /health
    port: 9100
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /health
    port: 9100
  initialDelaySeconds: 2
  periodSeconds: 5
```

## Prometheus Metrics

rtpbridge exposes metrics at `GET /metrics` on the control plane port in OpenMetrics text format (Prometheus 0.0.4 compatible).

### Scrape Config

```yaml
scrape_configs:
  - job_name: rtpbridge
    scrape_interval: 15s
    metrics_path: /metrics
    static_configs:
      - targets: ["rtpbridge:9100"]
```

In Kubernetes with service discovery:

```yaml
scrape_configs:
  - job_name: rtpbridge
    kubernetes_sd_configs:
      - role: pod
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_label_app]
        regex: rtpbridge
        action: keep
      - source_labels: [__meta_kubernetes_pod_ip]
        target_label: __address__
        replacement: "$1:9100"
```

### Key Metrics

All metrics use the `rtpbridge_` prefix:

| Metric | Type | Description |
|--------|------|-------------|
| `rtpbridge_sessions_total` | Counter | Total sessions created |
| `rtpbridge_sessions_active` | Gauge | Currently active sessions |
| `rtpbridge_endpoints_total` | Counter | Total endpoints created |
| `rtpbridge_endpoints_active` | Gauge | Currently active endpoints |
| `rtpbridge_packets_routed_total` | Counter | Total packets routed |
| `rtpbridge_srtp_errors_total` | Counter | SRTP auth/replay errors |
| `rtpbridge_transcode_errors_total` | Counter | Transcode failures |
| `rtpbridge_packets_recorded_total` | Counter | Total packets recorded to PCAP |
| `rtpbridge_recordings_active` | Gauge | Currently active recordings |
| `rtpbridge_dtmf_events_total` | Counter | DTMF events detected |
| `rtpbridge_events_dropped_total` | Counter | Events dropped (backpressure) |

### Suggested Alerts

```yaml
groups:
  - name: rtpbridge
    rules:
      - alert: RtpbridgeDown
        expr: up{job="rtpbridge"} == 0
        for: 1m
      - alert: HighSrtpErrors
        expr: rate(rtpbridge_srtp_errors_total[5m]) > 1
        for: 5m
      - alert: EventsDropped
        expr: rate(rtpbridge_events_dropped_total[5m]) > 0
        for: 5m
```

## Reverse Proxy / TLS

The control plane serves plain WebSocket and HTTP. To add TLS, place a reverse proxy in front of rtpbridge.

### nginx

```nginx
upstream rtpbridge {
    server 127.0.0.1:9100;
}

server {
    listen 443 ssl;
    server_name rtpbridge.example.com;

    ssl_certificate     /etc/ssl/certs/rtpbridge.crt;
    ssl_certificate_key /etc/ssl/private/rtpbridge.key;

    location / {
        proxy_pass http://rtpbridge;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_read_timeout 86400s;
    }
}
```

Set `proxy_read_timeout` high enough to cover long-lived WebSocket sessions. The media plane (RTP/UDP) is not proxied — it must be directly reachable.

## Security

The WebSocket control plane does not implement authentication or encryption. It is expected to be secured at the infrastructure level — for example, via an mTLS service mesh (Istio, Linkerd), a reverse proxy with client certificate verification, or network policies that restrict access to trusted services only.

Do not expose the control plane port directly to untrusted networks.

## Systemd

Example unit file for bare-metal or VM deployments:

```ini
[Unit]
Description=rtpbridge media bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/rtpbridge --config=/etc/rtpbridge/config.toml
Restart=on-failure
RestartSec=5
User=rtpbridge
Group=rtpbridge
LimitNOFILE=65536
TimeoutStopSec=310

# Security hardening
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/rtpbridge/recordings /var/lib/rtpbridge/cache
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
```

Key settings:

- **TimeoutStopSec** should exceed `shutdown_max_wait_secs` (default 300s) to allow graceful shutdown.
- **LimitNOFILE** should accommodate the expected number of UDP sockets (2 per RTP endpoint + 1 per WebRTC endpoint + overhead).
- **ReadWritePaths** must include the recording directory and file cache directory.

> **Note:** The default `cache_dir` is `/tmp/rtpbridge-cache`, which is ephemeral and may be cleared on reboot. For production systemd deployments, set `cache_dir = "/var/lib/rtpbridge/cache"` in your config file to match the `ReadWritePaths` above.

## Operational Guidance

### File Descriptor Sizing

Each RTP endpoint uses 2 UDP sockets (RTP + RTCP). WebRTC endpoints use 1. File playback endpoints use 0. Size your file descriptor limit accordingly:

```
fd_estimate = (max_sessions × avg_endpoints_per_session × 2) + 200
```

For example, 5000 sessions with 2 endpoints each: `(5000 × 2 × 2) + 200 = 20,200`. Set `LimitNOFILE` in your systemd unit to at least this value.

### Event Backpressure

The control plane uses bounded channels (`event_channel_size`, default 256) for events. If a WebSocket client reads events slower than they are produced, excess events are dropped and an `events.dropped` event is sent with the count. Critical events (like `session.idle_timeout` and `recording.stopped`) use a separate priority channel (`critical_event_channel_size`, default 64) to avoid being dropped during bursts of normal events.

If you observe frequent `events.dropped` events, increase `event_channel_size` or ensure your client processes events promptly.

### Key Prometheus Metrics

The `/metrics` endpoint exposes Prometheus-format metrics. Key metrics for alerting:

| Metric | Type | Alert Condition |
|--------|------|-----------------|
| `rtpbridge_sessions_active` | Gauge | Approaching `max_sessions` |
| `rtpbridge_endpoints_active` | Gauge | Unusually high per-session count |
| `rtpbridge_recordings_active` | Gauge | Approaching per-session limit |
| `rtpbridge_sessions_total` | Counter | Rate drop indicates service issues |

## Load Balancing

### WebSocket Sticky Sessions

The WebSocket control plane requires **sticky sessions** (session affinity). A client must reach the same rtpbridge instance for the duration of its WebSocket connection. Without sticky sessions, `session.attach` cannot reconnect to an orphaned session on a different node.

For HTTP/1.1 WebSocket upgrade, most load balancers maintain the connection naturally. However, if the client reconnects (e.g., after a network blip), ensure the reconnection reaches the same instance:

- **nginx**: Use `ip_hash` or `sticky cookie` upstream directive.
- **HAProxy**: Use `stick-table` with `stick on src` or cookie-based persistence.
- **AWS ALB**: Enable sticky sessions with application cookie or duration-based stickiness.
- **Kubernetes Ingress**: Annotate with `nginx.ingress.kubernetes.io/affinity: cookie`.

### UDP Media Plane

RTP/UDP media packets must reach the rtpbridge instance directly. Load balancers should **not** proxy UDP media traffic. Clients learn the media IP and ports from SDP negotiation and send directly.

If running multiple instances behind a load balancer, each instance must advertise its own `--media-ip` so SDP answers contain the correct reachable address.

## Network Topology & NAT

rtpbridge uses **ICE-lite** for WebRTC endpoints: the server is always the controlled agent and advertises only host candidates. No STUN or TURN servers are used or needed on the server side.

This means:

- **`media_ip` must be directly reachable** by all peers. Set it to the public/external IP address if peers connect over the internet.
- **For NAT environments**: Set `media_ip` to the public IP and ensure the entire `rtp_port_range` (default 30000-39999) is port-forwarded (UDP) to the rtpbridge host.
- **WebRTC peers** must be able to reach `media_ip` directly. Since rtpbridge only offers host candidates (no server-reflexive or relay candidates), peers behind symmetric NATs may fail to connect unless they use a TURN server on the peer side.
- **Plain RTP endpoints** use even/odd port pairs from `rtp_port_range` for RTP/RTCP. Ensure this range is open in your firewall.
- **Firewall rules**: Open `rtp_port_range` (UDP) for media traffic and the control port (default 9100 TCP) for WebSocket/HTTP.

## Docker

```dockerfile
# Pin Rust version to match rust-version in Cargo.toml
FROM rust:1.88-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends libopus-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Create stubs so Cargo.toml parses (benches/ excluded by .dockerignore)
RUN mkdir -p src benches \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > benches/codec_bench.rs \
    && echo 'fn main() {}' > benches/srtp_bench.rs \
    && echo 'fn main() {}' > benches/routing_bench.rs \
    && echo 'fn main() {}' > benches/pcap_bench.rs \
    && cargo build --release \
    && rm -rf src

COPY . .
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-20250317-slim
RUN apt-get update && apt-get install -y --no-install-recommends libopus0 libssl3 ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd -r -s /sbin/nologin rtpbridge \
    && mkdir -p /var/lib/rtpbridge/recordings /var/lib/rtpbridge/media /var/lib/rtpbridge/cache \
    && chown -R rtpbridge:rtpbridge /var/lib/rtpbridge
COPY --from=builder /app/target/release/rtpbridge /usr/local/bin/rtpbridge
EXPOSE 9100
USER rtpbridge
ENTRYPOINT ["rtpbridge"]
```

For container health checks, use your orchestrator's native mechanism (e.g., Kubernetes `livenessProbe`) pointed at `GET /health` on the control port.
