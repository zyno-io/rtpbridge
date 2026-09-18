# Changelog

Notable changes to rtpbridge are documented here. Changes under **Unreleased** have not been assigned a release version.

## Unreleased

No changes yet.

## 0.2.0 - 2026-09-17

### Breaking changes and upgrade notes

- The control listener now defaults to `127.0.0.1:9100`. Non-loopback listeners require TLS and HMAC authentication. Trusted TLS proxy deployments can explicitly set `allow_plaintext_control`; `allow_unauthenticated_control` is a separate development exception.
- Privileged requests with a browser `Origin` require HMAC authentication. Unauthenticated loopback clients must send a loopback IP or `localhost` in `Host`. Proxies preserving a public hostname must authenticate privileged upstream requests. WebSocket audio continues to use its single-use connection token.
- Plain RTP/RTCP now validates peer addresses and learns validated symmetric source ports. Configure `rtp_source_networks` for approved SBC or NAT gateway addresses that differ from the SDP peer address.
- URL playback requires an exact scheme/host/port entry in `file_download_origins`. Private, loopback and special-use destinations additionally require `file_download_networks`. Every redirect is checked; HTTPS downgrades are rejected and caller-supplied headers are removed on cross-origin redirects.
- `max_sessions` must be between 1 and 65,536; `max_endpoints_per_session` must be between 1 and 128. Zero no longer disables these limits. `transcode_cache_size` must cover the endpoint limit. Defaults remain 10,000 sessions and 20 endpoints per session.
- Each process requires its own locked playback cache directory. Downloads and cached files have finite admission and disk budgets. Reclaiming files from the previous cache naming scheme requires a stopped-server migration.
- File playback now runs once by default instead of looping indefinitely. Set `loop_count` explicitly for repeating playback.
- Endpoint directions in API responses and events are expressed from the peer's SDP perspective. Explicit directions are preserved across the initial offer/answer exchange, including `inactive`.
- Media binding is dual-stack: `media_ip` accepts a comma-separated IPv4 and IPv6 address, at most one of each, and `server.info` reports `media_ip` as an array. Plain RTP selects the remote SDP address family; WebRTC advertises a host candidate for each configured family.
- Builds no longer automatically use vendored OpenSSL. The new `scripts/build-openssl.sh` helper builds checksum-verified OpenSSL 3.6.4 for development, CI and containers. Startup validates and logs the linked version; supported system branches are 3.5.x at 3.5.8 or newer, 3.6.x at 3.6.4 or newer, and later release lines. Follow the updated [build instructions](docs/guide/getting-started.md).
- The container now uses Distroless Debian 13 and runs as UID/GID `65532:65532`. Update writable volume ownership and mounted secret permissions before upgrading; see [deployment](docs/guide/deployment.md).
- Recording HTTP downloads contain the file length observed when opened. Downloads of active recordings can end within a PCAP record; stop and flush recording before exporting a finalized capture.

See [configuration](docs/guide/configuration.md) for the new policies and [performance](docs/guide/performance.md) for resource limits.

### Added

- Add native-rate WebSocket PCM endpoints for 8, 16 and 48 kHz audio, including connection tokens, bounded framing and lifecycle events.
- Add CNG and CED fax-tone detection using the shared audio-analysis decode path.
- Add runtime endpoint direction and remote-SDP updates, generation-aware ICE restarts, WebRTC receive-task supervision and improved ICE restart telemetry.
- Add per-source `Synth` and `Tracked` playout buffers, including adaptive prebuffering for bursty clockless producers.
- Add wire-level packet counters, WebRTC ICE state, RTT and receiver-report statistics, plus media diagnostics and a per-pod Grafana dashboard.
- Add URL playback headers, per-endpoint gain and Opus file decoding.
- Record real inbound network tuples and framing, codec metadata, native L16 and media-clock alignment. Add the `pcap2audio` conversion tool and publish it as a static binary and container artifact.
- Add endpoint-scoped DTMF privacy controls and scrubbed control-protocol logging.

### Security

- Preserve SRTP/SRTCP replay protection and rollover state when renegotiating with the same key. Expire retired SRTCP keys on the same deadline as retired RTP keys.
- Validate secure SDP and exact SDES key lengths before changing negotiation state. Reject invalid secure negotiation and plaintext downgrades after opportunistic SRTP keys have been established.
- Validate RTP payload types, source tuples and complete RTCP framing before updating accepted packet state. Learn separate RTCP source ports independently and honor `a=rtcp`.
- Pin validated DNS destinations for URL downloads, disable environment proxies, and bound outstanding resolver work even after callers cancel. Download errors and logs omit URL paths, query contents and caller header values.
- Reject duplicate `Host` and `Authorization` headers and protect unauthenticated loopback control against browser cross-site access and DNS rebinding.
- Open recording paths through directory handles to prevent traversal and symlink races.
- Bound control requests, WebSocket writes, transfers, downloads, storage work and recording queues. Work that outlives a cancelled request retains admission until it actually finishes.
- Replace the custom PEM parser with a maintained parser, update the yanked ChaCha20 dependency, and require patched native OpenSSL versions.
- Reject WebRTC renegotiation that changes the pinned DTLS fingerprint.
- Add HMAC-authenticated WSS control connections, avoid logging DTMF digits and redact sensitive control payload fields.
- Negotiate opportunistic SRTP for carrier RTP while preventing established secure sessions from downgrading to plaintext.

### Performance

- Share each source's audio decoding across conference mixing, transcoding, VAD and fax detection, with shared resampling per output rate. Preserve destination-specific encoders and decode-free same-codec passthrough when audio analysis is inactive.
- Move filesystem and playback decoding work off session media tasks into bounded workers with PCM prefetch. Shared playback open/decode operations now have queue-inclusive deadlines and report terminal failures.
- Stream recording downloads in 64 KiB chunks with bounded concurrency and deadlines instead of buffering whole captures. Bound recording writer queues and skip recording descriptor construction when recording is disabled.
- Use a bounded byte ring for WebSocket audio, preserving partial samples and pacing frames at 20 ms under backpressure. Independent bounded writers prevent slow WebSocket peers from holding session work indefinitely.
- Spool PCAP conversion data to bounded temporary storage and reject excessive capture lengths, durations, channels and output sizes before large allocations or silence expansion.
- Prefer the highest-quality mutually supported codec in offers and answers while honoring caller order as the tie-breaker.

### Fixed

- Handle variable-duration audio packets from 2.5 to 120 ms without truncating samples. Accumulate conference audio at wider precision and clamp once to avoid source-order-dependent clipping.
- Retain shared decoder state while any routing or analysis consumer still needs it. Reset queued playout frames and clocks when codec or payload mappings change.
- Correct RTCP jitter across RTP timestamp rollover and avoid reporting phantom packet loss when a packet from the previous sequence-number cycle arrives late.
- Prevent endpoint transfer deadlocks, endpoint loss and incorrect rollback during queue saturation, cancellation and overlapping transfers.
- Preserve the original orphan expiry when session attachment fails.
- Release stalled WebSocket connection admission on write or Pong timeout and emit a single disconnect event.
- Keep cache leases tied to the exact download headers and file instance. Retain accounting for undeleted files after cancellation, propagate startup recovery failures, and bound shutdown cleanup.
- Prevent old shared-playback generations from deleting their replacements. Keep short and paused files seekable until EOF is consumed, and release decoder admission on completion or failure.
- Preserve final padded playback samples and loop boundaries. Rebuild routes after the final file or tone frame so completed generators do not retain obsolete mixers.
- Prevent continuous speech from emitting repeated `vad.speech_started` events.
- Prevent a single injected DTMF digit from arriving as multiple digits on plain-RTP legs, and negotiate telephone-event correctly across multi-rate SIP offers with its own RFC 4733 clock.
- Fix WebRTC RTP-mode ingress packet loss, avoid arming the connecting watchdog before an offer is answered and supervise receive-task termination.
- Reset the SRTP transmit rollover counter when rotating an outbound SSRC, and keep replay and rollover state per SSRC.
- Latch symmetric RTP on the first valid packet and re-anchor the learning window when an answer is accepted.
- Preserve the statistics emission timeline when a client re-subscribes, report WebRTC jitter and RTT correctly, and avoid inflated loss after an SSRC change.
- Select the first active audio media section in SDP and ignore rejected `m=audio 0` sections; reject ambiguous offers containing multiple active audio sections.

### Build and tooling

- Scan the exact container candidate, including unfixed HIGH/CRITICAL vulnerabilities, and generate its SBOM before publication. Tag releases now wait for CI and successful image publication before creating the GitHub release.
- Share native dependency setup across CI jobs and embed the release tag or canary commit in published build versions.
- Update documentation dependencies and add Node 22, reproducible installation and dependency auditing to documentation CI.
- Add conference benchmarks for 2, 3, 10 and 20 participants using PCMU, G.722, Opus and mixed codecs.
- Emit soak quality failures during execution and record Node event-loop delay without changing failure thresholds.
- Expand regression coverage and document configuration migration, resource bounds and measured performance. See the [remediation results](docs/security-performance-remediation-results.md) for validation evidence and its limits.
- Raise the minimum supported Rust version to 1.94, move builds to Debian 13, use the system `libopus`, and update the dependency stack including str0m, Symphonia and opus2.
- Embed the build version in binaries and container images, and add media soak testing and diagnostics tooling.

## 0.1.0 - 2026-03-27

- Initial release of the WebSocket JSON control plane with one media session per connection, orphan recovery and graceful shutdown.
- Add WebRTC, plain RTP/SRTP, file, tone and cross-session bridge endpoints with per-endpoint UDP sockets and symmetric RTP learning.
- Add PCMU, G.722, Opus and L16 routing, transcoding and per-destination conference mixing.
- Add DTMF detection, forwarding and injection; voice activity detection; local and URL audio playback; and decrypted PCAP recording.
- Add endpoint transfer, session bridging, periodic statistics, Prometheus metrics and health, session and recording HTTP APIs.
- Add container images, CI, benchmarks, deployment examples and protocol, operations and architecture documentation.
