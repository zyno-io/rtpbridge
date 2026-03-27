# Review Guide

Lessons learned from production readiness reviews that future reviewers should know.

## False Positives to Avoid

These were flagged as bugs by automated/AI review but are correct:

### SRTP Implementation is RFC-compliant
- **Key derivation is NOT per-SSRC.** RFC 3711 section 4.3.1 derives session keys from the master key using labels (0x00/0x01/0x02), not SSRCs. The SSRC is correctly used in the **IV computation** (`compute_iv` in `srtp.rs`), not in the KDF. Do not flag this as a security vulnerability.
- **Replay window `delta >= 64`** is correct. The 64-bit window covers bits 0-63 (packets from `highest_index` down to `highest_index - 63`). A packet at delta=64 is correctly outside the window.
- **`estimate_roc()` logic** is correct. The condition `seq > self.highest_seq.wrapping_add(0x8000)` handles the case where a late packet from a previous ROC arrives after a sequence number wraparound.

### Rust 2024 Edition is Valid
`edition = "2024"` in Cargo.toml is correct — it was stabilized in Rust 1.85 (Feb 2025). MSRV is 1.88 (required by `earshot` and `time` crates). Do not flag the edition or MSRV as invalid.

### Session Attach is Not a Race Condition
In `mod.rs:attach_session`, the DashMap `RefMut` holds the shard lock for the entire scope. `try_send()` is non-blocking and executes while the lock is held, so concurrent mutations to the same session entry are impossible.

### Graceful Shutdown is Implemented
`main.rs` waits for session drain with `wait_for_drain()` and a configurable `shutdown_max_wait_secs` timeout. `session_ended()` uses `fetch_update` with underflow protection. Do not flag missing shutdown handling.

### Resampler `<=` Boundary is Intentional
`resample.rs` uses `pos <= last_valid` to include the final sample. When `pos` equals the last index, the sample is read with `s1 = s0` (clamped). Tests verify this.

### Resampler Uses Linear Interpolation Without Anti-Aliasing
This is a documented tradeoff. For telephony voice (bandwidth-limited to 3.4kHz for PCMU, 7kHz for G.722), aliasing is negligible. The simplicity and low CPU cost outweigh the quality loss on high-frequency content.

### Blocking I/O in Recording Writer Task is a Design Choice
The recording background task uses `std::fs::File` + `BufWriter`. Individual writes go to the in-memory buffer (fast). Only the final `flush()` on channel close touches disk. File *creation* uses `spawn_blocking` to avoid blocking the session task.

### Error Messages Include Internal Details
The control plane is internal-only and accessed by trusted callers. JSON parse errors, path names, and debug info in error responses are acceptable. Do not flag "information leakage" — there are no untrusted consumers.

### RTCP Counters Use `wrapping_add`
`packets_received`, `octets_received`, `packets_sent`, `octets_sent` in `rtcp.rs` use `wrapping_add()` intentionally. RFC 3550 specifies 32-bit wrapping counters. Do not flag these as missing overflow checks.

### RTCP `fraction_lost` Uses u64 Intermediate With 255 Clamp
`fraction_lost_and_update()` computes `((lost as u64) << 8) / expected as u64).min(255) as u8`. The u64 intermediate prevents overflow when `lost_interval > 2^24`, and `.min(255)` ensures 100% loss correctly returns 255 (not 0 from u8 wrapping of 256). This matches RFC 3550 A.3 which defines fraction_lost as an 8-bit value (0-255).

### SRTCP IV Computation Uses u64 for 31-bit Index — Correct
`compute_srtcp_iv` casts the 31-bit `srtcp_index` (u32) to u64 before shifting left by 16. This is a widening conversion for the shift, not a semantic mismatch. The resulting 48-bit value fits cleanly in the 8-byte IV region. Do not flag the u64 cast as operating on more bits than the 31-bit index.

### RTCP RTT Uses SystemTime With Sanity Check
`process_rr()` in `rtcp.rs` uses `SystemTime::now()` (not `Instant`) for NTP timestamp computation in RTT calculation. If the system clock steps backward (NTP adjustment), the computed RTT can be nonsensical. The `(0.0..10000.0)` range check rejects bad values — `rtt_ms` stays `None` until the next valid RR. Audio flow is unaffected; only RTT reporting is transiently wrong. Do not flag as "should use monotonic clock" — NTP timestamps are inherently wall-clock.

### SharedPlayback Broadcast Error Check is Not a Race
In `shared_playback.rs`, `tx.send().is_err()` followed by `tx.receiver_count() == 0` is correct. `broadcast::Sender::send()` returns `Err` **only** when there are zero active receivers — lagged receivers still count as active and `send()` succeeds (the receiver sees `Lagged` on its next `recv()`). The `receiver_count()` re-check is a defensive guard against a new subscriber appearing between the failed send and the break.

### SDP Session ID Truncation is Fine
`id.as_u128() as u64` truncates a UUID to 64 bits for SDP session IDs. SDP session IDs are opaque per RFC 4566 — 64 bits of UUID4 entropy is more than sufficient for uniqueness.

### SDP Crypto Selection Only Accepts Supported Suites
In `sdp.rs`, the parser only stores `a=crypto` lines with `AES_CM_128_HMAC_SHA1_80`. Unsupported cipher suites are silently ignored — if an SDP offers only unsupported suites, `crypto` will be `None` (graceful degradation to plain RTP). Do not flag this as "dropping crypto" — it is intentional.

### Batch Packet Drain Cap is a Tuning Constant
The `select!` loop in `media_session.rs` drains up to 64 additional packets via `try_recv()` after processing the first. The cap of 64 prevents starvation of other event sources (commands, timers). This is not a bug.

### SRTCP Dual-Context Shares Rekey Timer
During SRTP rekey, both `srtp_rx_new` and `srtcp_rx_new` share the same `rekey_switchover` timer (5 seconds). The timer is set in `accept_answer()` when the remote's new key arrives. This is intentional — both should promote on the same deadline.

### Transcode Frame Size Mismatch Fires at `trace!` Level
The resampler may produce ±1 sample vs the target codec's `ptime_samples()`. The transcode pipeline pads/truncates and logs at `trace!` level. This is normal operation, not a bug.

### Opus Fixed 24kbps Bitrate
The Opus encoder uses a fixed 24kbps bitrate. This is adequate for voice telephony. Configurability would add complexity for negligible benefit in the telephony use case.

### SRTCP Index Wraps at 2^31
`srtcp_index` wraps with `& 0x7FFFFFFF`. At typical RTCP rates (every 5 seconds), this takes ~340 years to exhaust. A warning is logged at 87.5% of keyspace. Do not flag this as a missing rekey trigger.

### `check_media_timeouts` Throttled to 1Hz
The timeout check runs at most once per second (not on every event loop iteration). The media timeout threshold is 5+ seconds, so 1Hz is sufficient.

### serde_json Has a Default Recursion Limit
The comment at `connection.rs:115` says "serde_json has no recursion depth limit." This is misleading — serde_json 1.x (we use 1.0.149) limits recursion to 128 by default. Deeply nested JSON from a malicious client will return a parse error, not overflow the stack. The `unbounded_depth` feature (which would remove the limit) is NOT enabled. Do not flag this as a DoS vector.

### DTMF Interrupted Event Does Not Lose the New Digit
In `dtmf.rs`, when a new digit's end-bit arrives while an old digit is in progress, the detector returns the interrupted (old) event and stores the new digit in `self.current`. The new digit appears "lost" because its end-bit was seen but not acted on. However, RFC 4733 mandates 3 redundant end-bit copies — the second copy re-enters `process()`, matches the stored `current` event_id (no new interruption), and completes the digit normally. Additionally, `check_timeout()` provides a fallback if all redundant copies are lost. Do not flag this as a dropped digit.

### SRTP `protect()` Does Not Handle Out-of-Order — By Design
The `update_roc` method in `protect()` only tracks forward sequence progression. Out-of-order outbound packets would produce incorrect encryption. This is safe because the session task is single-threaded (see "Session Task is Single-Threaded" below) — outbound packets are always generated in order. Do not flag as missing out-of-order handling.

### SDP Parser Only Recognizes PCMU (PT 0), G.722 (PT 9), and Dynamic PTs ≥ 96
Static payload types 1–8 and 10–95 (including PCMA/PT 8, G.729/PT 18) are intentionally dropped by the SDP parser. The supported codec set is PCMU, G.722, and Opus. A remote offering only unsupported codecs will have an empty codec list; the endpoint is still created (with only telephone-event in the SDP answer) but cannot carry audio. This is graceful degradation, not a bug. Do not flag the `_ => {}` catch-all as silently dropping valid codecs — it reflects the supported codec set.

### Transcode Cache `.expect()` is Safe
`media_session.rs` uses `.expect("just inserted cache_key must exist")` after inserting into the transcode cache HashMap. This is safe because the session task is single-threaded — no concurrent removal can happen between the insert and the get. Do not flag as a potential panic.

### Biased Select Does Not Starve Normal Events
`connection.rs` uses `biased;` in the `select!` loop with critical events checked first. Critical events (endpoint state changes) are rare and low-volume. Under normal operation, normal events are not starved. Do not flag as a priority inversion or starvation bug.

### Rekey Switchover Correctly Handles Already-Promoted Contexts
In `endpoint_rtp.rs:check_rekey_switchover()`, if `srtp_rx_new` is `None` when the deadline fires, the timer is simply cleared. This is correct — it means the new context was already promoted via the try-new-first decryption path. Do not flag "switchover with None context" as a broken state.

### Recording Path Validation Uses `canonicalize()` — Symlink Escapes Are Handled
`handler.rs:validate_recording_path()` canonicalizes both the parent directory and `recording_dir`, then checks `starts_with`. The resolved (canonicalized) path is returned to the caller for `File::create_new`, closing the TOCTOU race. Unit tests verify symlink escape rejection. Do not flag path traversal via symlinks.

### SDP Duplicate Crypto Lines — Last Matching Suite Wins
When multiple `a=crypto` lines offer the same supported suite, the parser overwrites on each match, so the last line wins. This is acceptable — RFC 4568 does not mandate preference order for identical suites. Do not flag as "should prefer the first line."

## Intentional Design Decisions

These were reviewed and accepted for production:

- **SDP parser is lenient** — The SDP parser itself accepts malformed input (empty parse is a no-op), but `accept_answer` validates that the parsed result includes a connection address. Garbage SDP that parses to no `c=` line returns an `ENDPOINT_ERROR`.
- **`ftp://` URLs treated as local paths** — `is_url()` only recognizes `http://` and `https://`. Other schemes fall through to the local file handler, which rejects them if `media_dir` is unset.
- **No HEALTHCHECK in Dockerfile** — Intentional; use orchestrator-native health checks (k8s probes, ECS health checks). The HTTP `/health` endpoint is available at the control port.
- **WebRTC `poll_output` has no timeout wrapper** — str0m is sans-I/O and non-blocking by design.
- **Transcode cache is count-limited** (default 64) with O(n) LRU eviction — Pipelines are small and per-session; memory-based limits and fancier data structures add complexity for near-zero benefit.
- **Recording breaks on first write error** — Immediate failure is correct for PCAP I/O errors. No retry/backoff. The session emits a `recording.stopped` event with `reason: "write_error"`.
- **No SSRF protection in file downloads** — Callers are responsible for sanitizing URLs before passing them to rtpbridge. Enforce via an upstream proxy, API gateway, or application-layer validation.
- **Metrics Mutex uses poison recovery** — The registry data is still valid after a panic, and permanent metrics outage is worse than serving slightly-stale data.
- **FileCache broadcast channel(4)** — Intentionally larger than 1 to avoid `Lagged` errors when multiple waiters subscribe concurrently.
- **Control plane is unauthenticated/unencrypted** — By design. Enforce externally via mTLS reverse proxy, network policy, or firewall rules.
- **Recording downloads read entire file into memory** — Size is capped by `max_recording_download_bytes` (configurable, default 512MB). Streaming would add complexity for an infrequent maintenance endpoint.
- **Multi-source fan-in preserves source timestamps** — Each packet keeps its source's timestamp/seq. This can cause jitter at the receiver. Inherent to SFU-style forwarding without a mixer.
- **/recordings listing does recursive scan (depth-limited to 10)** — Pagination applied after collecting. Acceptable for a maintenance endpoint; large trees should use external tooling.
- **WebRTC endpoints use ephemeral ports (not `rtp_port_range`)** — WebRTC binds to port 0 (OS-assigned) because ICE negotiates connectivity dynamically. Firewall rules based on `rtp_port_range` only apply to plain RTP endpoints.
- **Default `media_ip` is loopback** — A startup warning is emitted when `media_ip` is a loopback address. This is acceptable for local development; production deployments must set a routable address.
- **`accept_answer` TX fallback generates independent key** — If `create_offer` was called without SRTP but the answer contains crypto, an independent TX key is generated with a warning. The remote peer won't know this key, so outbound SRTP may not be decryptable. This edge case is preferable to keystream reuse.
- **SDP `generate_sdp` hardcodes telephone-event at PT 101** — Safe with the current codec set (PCMU=0, G722=9, Opus=111). If a future codec is added at PT 101, the SDP generator would need updating.
- **SDP with two `m=audio` lines merges them** — The SDP parser does not reject multi-stream audio SDP; it merges ports, protocols, and codecs from both sections. This is acceptable for the telephony use case where multi-stream audio SDPs are not expected. If multi-stream support is added, the parser will need explicit handling.
- **VAD uses `endpoint_audio_codec` (send codec) for inbound audio decoding** — `endpoint_audio_codec()` returns the endpoint's `send_codec`, but VAD decodes inbound packets. This works because rtpbridge negotiates a single codec per endpoint in SDP (symmetric). If asymmetric codec negotiation is ever added, VAD will need a separate `recv_codec` field.
- **SRTCP context promotion is independent of SRTP** — When SRTP promotes early (taking `srtp_rx_new` and clearing `rekey_switchover`), `srtcp_rx_new` is NOT cleared. SRTCP's try-new-first decryption path in `handle_rtcp` still has its new context available and self-corrects on the next SRTCP packet. Do not flag the timer clearing as breaking SRTCP promotion.
- **FileCache `max_entries` is a soft limit** — Eviction only removes unreferenced entries. If all cached files are actively referenced, the cache can temporarily exceed `max_entries`. Self-correcting once references are released and cleanup runs.
- **`start_ms` and `cache_ttl_secs` have no upper-bound validation** — An extremely large `start_ms` causes immediate playback finish (seeks past EOF). A large `cache_ttl_secs` just extends cache lifetime. Neither causes a crash or unbounded resource growth.
- **`SdpCodec::fmtp` uses `&'static str`** — All codec fmtp parameters are compile-time constants. If per-session dynamic fmtp negotiation is needed (e.g., varying Opus `maxplaybackrate`), this field would need to change to `Option<String>` and the `CODEC_*` constants would become builder functions.

## Architecture Notes for Reviewers

### Session Task is Single-Threaded
All endpoint mutation and media routing happens in a single tokio task per session. No races between endpoint operations and media flow within a session. The `DashMap` in `SessionManager` only protects session-level state (create/destroy/attach/detach).

### RTCP Goes to Port+1
Plain RTP endpoints send RTCP to the peer's RTP port + 1. Integration tests that want to capture RTCP must bind a socket on port+1 or verify RTCP indirectly via `stats.subscribe`.

### DTMF Injection Sends to the Endpoint's Remote
`endpoint.dtmf.inject` on endpoint A sends RFC 4733 packets FROM the bridge TO peer A (not through the routing table).

### Stats JSON Structure
The stats event payload uses `event["data"]["endpoints"][i]["inbound"]["packets"]` — not `rx_packets` or `rtp.rx_packets`. Check `protocol.rs` for the exact schema (`InboundStats`, `OutboundStats`).

### PCAP Magic Byte Order
The `pcap-file` crate writes big-endian PCAP magic. Tests should accept either byte order.

### Recording Paths Must Be Absolute
`recording.start` requires an absolute `file_path`. Path validation canonicalizes the parent directory and verifies the resolved path is within `recording_dir` — this prevents symlink escapes and TOCTOU races.

### SRTP Key Material Handling
SRTP/SRTCP contexts zeroize all key material on drop — master keys, master salts, derived session keys, and intermediate KDF output Vecs. Auth tag comparisons use constant-time comparison (`subtle::ConstantTimeEq`).

### SRTP Answer Key is Independent (All Paths)
`from_offer()`, `create_offer()`+`accept_answer()`, and `srtp_rekey()`+`accept_answer()` all use independent keys per direction. `srtp_rekey()` only updates TX; `accept_answer()` handles the RX dual-context transition from the remote's answer. Do not flag any path as "using the same key for both directions."

### SRTP IV Computation Follows RFC 3711 §4.1.1
`compute_iv` and `compute_srtcp_iv` in `srtp.rs` place the 14-byte salt at `iv[0..14]` (shifted left by 16 bits in the 128-bit field), SSRC at bytes 4-7, and the packet index shifted left by 16 bits at bytes 8-13 with bytes 14-15 zero. This matches `IV = (k_s * 2^16) XOR (SSRC * 2^64) XOR (i * 2^16)`. Unit tests verify byte-level layout. Do not flag the salt position or index shift as incorrect.

### DTMF Detector Prioritizes Interrupted Events
In `dtmf.rs`, when an end-bit packet arrives for a new digit while a different digit is in progress, the detector returns the interrupted event first and defers the end-bit to redundant copies (RFC 4733 mandates 3). This is intentional — it ensures no digit is silently dropped.

### Media Timeout Covers Zero-Packet Endpoints
`check_media_timeouts` falls back to `created_at` elapsed time when `ms_since_last_received()` is `None` (no packets ever received). This means endpoints that connect but never receive media will still trigger `media_timeout` events. Do not flag "timeout never fires for new endpoints."

### PCAP Endpoint Indices are Recycled
`RecordingManager` recycles synthetic PCAP address indices when endpoints are removed via a free list. `get_endpoint_index` pops from the free list before incrementing the counter. A warning is logged if the index saturates at 0xFFFE. Do not flag "monotonically increasing index counter."

### Session Create Cleanup on `get_session_cmd_tx` Failure
In `handler.rs`, if `create_session()` succeeds but `get_session_cmd_tx()` returns `None`, the session appears to leak (no `destroy_session` call). This is safe — the session task's `SessionCleanupGuard` removes the session from the DashMap on task exit. The session will be cleaned up when the task completes. Do not flag as a resource leak.

### RTCP Jitter Spikes on RTP Timestamp Wraparound
The RFC 3550 Appendix A.8 jitter algorithm (`rtcp.rs:138-141`) produces a transient jitter spike when the RTP timestamp (u32) wraps, because the transit difference has a discontinuity. The EWMA absorbs the spike and decays over ~64 packets (~1.3s). This is inherent to the RFC algorithm and occurs every ~8.9 minutes at 8kHz or ~1.5 hours at 48kHz. Do not flag as a computation bug.

## Test Coverage Notes for Reviewers

### TestPeer `accept_answer` is a No-Op
`tests/helpers/test_peer.rs:accept_answer()` is a stub that discards the SDP. WebRTC integration tests that need real offer/answer use manual str0m driving instead. Do not rely on `TestPeer` for full WebRTC lifecycle testing.

### SRTP Self-Roundtrip Tests Do Not Verify Interop
The SRTP unit tests in `srtp.rs` verify protect/unprotect using the same implementation. They confirm correctness of the encrypt/decrypt cycle but NOT interoperability with external implementations. IV layout tests verify byte-level RFC compliance separately.

### Integration Tests Use Timing Multipliers
Many integration tests use `timing::scaled_ms()` to accommodate CI variability. If tests flake under load, increase `RTPBRIDGE_TEST_TIMING_MULTIPLIER` rather than adding fixed sleeps.

## Known Gaps (Accepted)

- No SSRF protection in file downloads (see Intentional Design Decisions).
- No load/performance regression tests in CI. Criterion benchmarks exist in `benches/` for manual profiling but are not gated.
- WebRTC-to-RTP direction media test (`test_webrtc_rtp_bidirectional_media`) is best-effort — DTLS-SRTP may not complete in the in-process str0m test setup, so the assertion is a soft warning. RTP→WebRTC is reliably tested.
- NTP timestamp in RTCP SR overflows (`ntp_secs << 32`) when NTP seconds exceed 2^32 (~February 2036). Wraps silently in release builds. RTT computation is unaffected (uses middle-32-bit `wrapping_sub`). Only the absolute NTP timestamp in SR packets is wrong.
