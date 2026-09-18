# Deployment

The container and CI release builds statically link the checksum-verified OpenSSL 3.6.4 source built by `scripts/build-openssl.sh`. The binary logs its linked OpenSSL version at startup and rejects older vulnerable releases; system builds support patched 3.5.x (3.5.8+), 3.6.x (3.6.4+), and later release lines. Build-time native dependencies are covered by the [build instructions](./getting-started.md). Container package scanning does not replace reviewing embedded native-library advisories.


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
        # When `[tls]` is configured, mount the PEM certificate/key read-only.
        - name: control-tls
          mountPath: /etc/rtpbridge/tls
          readOnly: true
        # Mount the HMAC key separately from the config file. Give this only to
        # rtpbridge and trusted control clients, never audio-only consumers.
        - name: control-auth
          mountPath: /etc/rtpbridge/auth
          readOnly: true
  volumes:
    - name: config
      configMap:
        name: rtpbridge-config
    - name: control-tls
      secret:
        secretName: rtpbridge-control-tls
    - name: control-auth
      secret:
        secretName: rtpbridge-control-auth
```

The wildcard listener in this example requires a config with `auth_hmac_secret_file` and a `[tls]` certificate/key section, backed by the mounted secrets. Both secrets are required. For a TLS-terminating proxy, explicitly set `allow_plaintext_control = true`, retain HMAC, and restrict the upstream listener to that proxy. The default loopback listener is reachable only inside its network namespace; publishing a container port alone does not expose it.

Resource requirements depend on workload — transcoding (especially Opus) is CPU-intensive. Monitor actual usage and adjust accordingly.

### Port Ranges

Plain RTP endpoints allocate even/odd RTP/RTCP port pairs from `rtp_port_range` (default: 30000-39999), so expose that UDP range for plain RTP/SRTP.

WebRTC endpoints do **not** use `rtp_port_range`: each WebRTC endpoint binds one OS-assigned UDP port on `media_ip`, and that port is advertised as the ICE host candidate. In environments with external firewalls or NAT, allow the host/container ephemeral UDP range or run with a network model where those OS-assigned host-candidate ports are directly reachable.

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

When `[tls]` is configured, add `scheme: HTTPS` to both probe `httpGet`
blocks. Configure Prometheus to use an HTTPS target too, and provide its trust
store for the issuing CA. Health and metrics do not need HMAC authorization;
they still need surrounding network-policy or firewall restrictions.

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
| `rtpbridge_playout_late_drops_total` | Counter | Playout packets dropped after their play slot |
| `rtpbridge_playout_overflow_drops_total` | Counter | Playout frames dropped to bound latency |
| `rtpbridge_playout_underflow_fills_total` | Counter | Synthesized silence fills for clockless-source underflow |
| `rtpbridge_events_dropped_total` | Counter | Events dropped (backpressure) |
| `rtpbridge_webrtc_packet_errors_total` | Counter | Inbound WebRTC packets rejected by str0m |
| `rtpbridge_webrtc_connecting_stuck_total` | Counter | WebRTC endpoints stuck in Connecting past watchdog threshold |
| `rtpbridge_webrtc_ice_restart_conflicts_total` | Counter | ICE restarts rejected because an unanswered offer was pending |
| `rtpbridge_webrtc_recv_task_started_total` | Counter | WebRTC receive tasks that reached their receive loop |
| `rtpbridge_webrtc_recv_task_exited_total` | Counter | WebRTC receive tasks that exited cooperatively |
| `rtpbridge_webrtc_recv_task_dead_total` | Counter | Live WebRTC endpoints whose receive task had finished |
| `rtpbridge_webrtc_recv_task_start_timeout_total` | Counter | WebRTC receive tasks that did not start within the grace window |
| `rtpbridge_webrtc_recv_overflow_total` | Counter | Inbound WebRTC packets dropped because the session channel was full |

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

## TLS and control authorization

rtpbridge can serve TLS itself on its ordinary control port: configure `[tls]`
with PEM certificate and key paths and use `wss://<host>:9100` / `https://<host>:9100`.
It does not mix TLS and plaintext on one listener. A reverse proxy remains
optional when it provides a separate operational benefit, but is not required
for WSS.

Set `auth_hmac_secret_file` to require HMAC-SHA256 Authorization signatures for
control WebSocket upgrades and session/recording HTTP routes. The secret belongs
only to rtpbridge and trusted control issuers such as Nexus or the legacy bridge;
mount it as a file from your secret manager. AI agents retain only their
server-minted single-use `/audio/<token>` capability, never the HMAC key. See
[configuration](./configuration.md#tls-and-hmac-authorization) for the exact
signature format.

`/health` and `/metrics` deliberately remain unauthenticated so Kubernetes and
Prometheus can scrape them; restrict those paths with the surrounding network
policy. Do not expose the control port directly to an untrusted network.

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

Plain RTP/SRTP endpoints use 2 UDP sockets (RTP + RTCP). WebRTC endpoints use 1 UDP socket. File, tone, bridge, and WebSocket audio endpoints use 0 UDP sockets. Size your file descriptor limit from the expected endpoint mix:

```
fd_estimate = (plain_rtp_endpoints × 2) + webrtc_endpoints + recording_files + control_connections + 200
```

For example, 5000 sessions with two plain RTP endpoints each: `(10,000 × 2) + 200 = 20,200`. Set `LimitNOFILE` in your systemd unit to at least your estimated value.

### Event Backpressure

The control plane uses bounded channels (`event_channel_size`, default 256) for events. If a WebSocket client reads events slower than they are produced, excess events are dropped and an `events.dropped` event is sent with the count. Critical events (such as endpoint state changes, ICE state changes, recording stops, file completion, and session timeout events) use a separate priority channel (`critical_event_channel_size`, default 64) to avoid being dropped during bursts of normal events.

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
- **For NAT environments**: Set `media_ip` to the public IP. Forward `rtp_port_range` (UDP) for plain RTP/SRTP endpoints, and also allow/forward the OS-assigned UDP ports used by WebRTC endpoints.
- **WebRTC peers** must be able to reach the advertised `media_ip:port` host candidates directly. Since rtpbridge only offers host candidates (no server-reflexive or relay candidates), peers behind symmetric NATs may fail to connect unless they use a TURN server on the peer side.
- **Plain RTP endpoints** use even/odd port pairs from `rtp_port_range` for RTP/RTCP. Ensure this range is open in your firewall.
- **Firewall rules**: Open `rtp_port_range` (UDP) for plain RTP/SRTP, the reachable WebRTC UDP host-candidate port range, and the control port (default 9100 TCP) for WebSocket/HTTP.

## Docker

The repository Dockerfile builds the Rust binary with the checksum-verified static OpenSSL library, then copies it and libopus into a Distroless Debian 13 runtime. The final image runs as UID/GID `65532:65532`. Build from the repository root so the native build helper is included:

```bash
docker build -t rtpbridge:local .
```

Pass the same version into Docker that Cargo uses locally:

```bash
if TAG=$(git describe --tags --exact-match HEAD 2>/dev/null); then
  BUILD_VERSION="v${TAG#v}"
else
  BUILD_VERSION="canary-$(TZ=UTC0 git show -s --format=%cd --date=format-local:'%y.%-m%d.%-H%M' HEAD)"
fi
docker build --build-arg BUILD_VERSION="$BUILD_VERSION" -t rtpbridge:"$BUILD_VERSION" .
```

For container health checks, use your orchestrator's native mechanism (e.g., Kubernetes `livenessProbe`) pointed at `GET /health` on the control port.

### Runtime image and volume migration

The runtime uses [Distroless Debian 13](https://github.com/GoogleContainerTools/distroless) with libopus, runs as UID/GID `65532:65532`, and contains no shell or package manager. Existing writable recording, media, and cache volumes must permit access by that UID/GID before upgrading; read-only configuration, HMAC, and TLS key mounts must be readable by it. Set the Kubernetes pod `securityContext.fsGroup` to `65532` where the volume driver supports it, or provision ownership on the host. Use a separate diagnostic container for shell access. CI scans the candidate image and generates its SBOM before publishing.
