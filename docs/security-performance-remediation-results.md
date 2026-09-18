# Security and performance remediation results

Implemented September 5, 2026, against `7614703`. All 16 numbered findings have corresponding code changes. The [plan](./security-performance-remediation-plan.md) retains the expanded validation and release checklist. No image was published and no running deployment was changed.

## Follow-up review — September 8, 2026

Repeated review found thirteen additional state, cancellation and security cases. The first seven and the final four were reproduced by regressions that failed before the correction. Shutdown and generator-completion regressions were added with their fixes:

| Missed case | Impact | Correction |
| --- | --- | --- |
| Two transfers of the same endpoint overlap | The rejected transfer could release the first transfer’s rollback capacity, allowing a later rollback to lose the endpoint | Each transfer records whether it owns the reservation; duplicate preparation is rejected and only the owner releases capacity |
| An opportunistic SRTP offer receives a second, plaintext answer | Established SRTP could lose its transmit encryption while retaining encrypted receive state | Plaintext fallback is accepted only before receive keys have been established; rejection preserves keys and addresses |
| VAD/fax stops while routing still needs PCM | Removing an analysis tap could discard shared decoder state and a partial audio frame | Decoder retention checks all remaining analysis, mixer and transcode consumers |
| Codec or payload mapping changes with queued RTP | Playout could retain the old clock or decode old queued packets under the new format | Buffers track codec, payload type and RTP clock and are recreated when the negotiated format changes |
| A short file reaches EOF during prefetch | A still-playing or paused endpoint could no longer seek because its worker had already exited | The worker keeps seek service until the session consumes EOF; observing EOF cancels it and releases decoder admission |
| Cache cleanup is cancelled while deletion waits for a worker | Undeleted files could lose cache ownership and byte/entry accounting | Retain garbage in cache state until deletion is confirmed; temporary claims prevent concurrent deletion |
| Shared playback storage stalls | Subscribers could stay silent indefinitely without a terminal event | Apply a ten-second queue-inclusive deadline to open/decode jobs; running work retains its lease and admission |
| Shutdown arrives during periodic cache cleanup | The cleanup task could wait behind a stalled storage job before observing shutdown | Make periodic cleanup cancellable and retain the bounded final cleanup window |
| A file or tone finishes while callers remain | Stale routes kept unnecessary mixer/decoder state and prevented direct forwarding | Report generator completion to the actor and rebuild routes after routing the final output |
| A cancelled URL download leaves system DNS running | Repeated cancellation could accumulate resolver jobs after download admission was released | Four DNS jobs hold admission until the actual resolver call exits; detached workers do not hold Tokio shutdown open |
| A web page accesses an unauthenticated loopback listener | Cross-site WebSocket upgrades could obtain control, and a rebound Host could read privileged HTTP data | Reject Origin-bearing privileged requests without HMAC and require loopback Host values for unauthenticated loopback clients; audio retains its separate token |
| The 32-bit RTP timestamp wraps | Jitter calculation treated rollover as a huge time discontinuity | Compute signed wrapping timestamp deltas before converting to microseconds, with monotonic arrival deltas and clock-change reset |
| A packet from before a sequence-number rollover arrives late | Four received packets could be reported as 65,536 lost packets | Advance the extended highest sequence only for a forward modular delta; retain the previous cycle for late packets |

Cache startup also propagates recovery read/delete failures instead of continuing with unaccounted stale files. New WebSocket fault tests verify that a 256 KiB burst survives media-queue backpressure with 20 ms pacing, and that a missing Pong produces one disconnect and releases connection admission. Playout/mixer comments now match variable-duration draining and the 240 ms mixer queue bound.

The native dependency review also found that the compatible `openssl-src` package embeds OpenSSL 3.6.3, which is affected by [CVE-2026-54874 and the August 25 advisories](https://openssl-library.org/news/secadv/20260825.txt). Container scanning reported no language-specific files and did not identify that embedded library. The automatic vendored feature was removed. Development, CI and container builds now use the same SHA-256-verified OpenSSL 3.6.4 source; startup validates the linked library and logs its version. Other locked Rust dependency versions are unchanged. System builds accept patched 3.5.x (3.5.8+), 3.6.x (3.6.4+), or later release lines; see the updated [build instructions](./guide/getting-started.md).

The tag-release workflow had a separate publication gap: it pushed before scanning and tried to scan a `v`-prefixed tag it had not built. Tag releases now wait for the reusable CI checks, build one local candidate, generate its SBOM, scan it including unfixed HIGH/CRITICAL findings, then promote that exact image. GitHub release creation waits for successful image publication. Published binaries receive the release tag or canary commit as their build version. The obsolete duplicate Dockerfile in the deployment guide was removed.

The documentation dependency audit found five advisories (three high, two moderate). Patched Nano ID/PostCSS dependencies and a Vite 6.4.3 override remove them while retaining stable VitePress 1.6.4; the override follows [the maintainer’s Vite 6 compatibility guidance](https://github.com/vuejs/vitepress/discussions/5072). Documentation CI now uses Node 22, a frozen `npm ci` install and `npm audit` before building. The site build also exposed a broken link to the soak README, which is corrected. The soak harness dependency audit reported zero advisories.

September 8 validation uses the corrected runtime and patched OpenSSL library:

| Check | Result | Local evidence |
| --- | --- | --- |
| Release unit tests | 733 passed | `/tmp/rtpbridge-ready-release-unit.log` |
| Debug unit/converter tests | 733 library tests and 8 converter tests passed; the main target repeats the library tests | `/tmp/rtpbridge-ready-unit.log` |
| Full serial integration suite | 314 passed across 35 suites, zero failures or ignored tests | `/tmp/rtpbridge-ready-integration.log` |
| Formatting, whitespace, required Clippy | Passed | `/tmp/rtpbridge-ready-clippy.log`; `cargo fmt --check`; `git diff --check` |
| Rust dependency audit/policy | `cargo audit` and `cargo deny check` passed | `/tmp/rtpbridge-ship-audit.log`, `/tmp/rtpbridge-ship-deny.log` |
| Workflow and shell validation | `actionlint` and `sh -n scripts/build-openssl.sh` passed | `/tmp/rtpbridge-ship-actionlint.log` |
| Documentation build/browser | Site rendered, client-side navigation worked, no page errors | `/tmp/rtpbridge-final-docs-build.log`, `/tmp/rtpbridge-ship-docs-browser.log` |
| Documentation/soak dependencies | Zero npm audit advisories in either project | `/tmp/rtpbridge-ship-docs-audit-final.json`, `/tmp/rtpbridge-ship-soak-audit.json` |
| Container startup/health | Nonroot startup with linked OpenSSL 3.6.4 and `GET /health` passed | `/tmp/rtpbridge-ready-smoke.log`, `/tmp/rtpbridge-ready-smoke-health.json` |
| Container scan/SBOM | Zero HIGH/CRITICAL, 13 MEDIUM and 7 LOW findings with no distribution fix listed | `/tmp/rtpbridge-ready-scan/vuln.json`, `/tmp/rtpbridge-ready-scan/sbom.spdx.json` |
| Final 50-call TURN soak | Passed; 35 mutations, 14 impairment windows, zero failures/flatlines, teardown gauges zero | `/tmp/rtpbridge-ready-soak/2026-09-08T23-50-28-682Z-seed-1234/summary.json` |

All benchmark targets compiled during review, and the conference benchmark was rebuilt and run against the final source. The first September 8 soak (`/tmp/rtpbridge-ship-soak/2026-09-08T22-46-08-807Z-seed-1234`) completed 50 calls, 35 mutations and 14 impairment windows with zero flatlines, packet loss and teardown gauges, but **failed 12 receive-timing checks**: Node RTP peers in nine calls reported 306–513 ms gaps during one sample window. Host load average was about 118 on ten cores at that time. Bridge receive loops also showed delayed input around that window. Host contention is a plausible explanation, but existing data cannot separate bridge scheduling from OS/Node peer timing, so this run is not a pass. Live status based only on control/transaction timeline events missed these quality failures, which the old harness retained until its final summary. The harness now emits `quality.failure` events during the run and records Node event-loop delay; failure criteria are unchanged. The instrumented repeat (`/tmp/rtpbridge-ship-soak-repeat/2026-09-08T23-06-26-758Z-seed-1234`) also completed all 50 calls with zero flatlines and zero teardown gauges, but failed ten receive-gap checks (267–283 ms in one window) and one inbound jitter check. The receive-gap window coincided with a 196 ms Node event-loop delay. Investigation of the jitter alert exposed the RTP timestamp-rollover bug above; its regression produced 268 seconds of false jitter before correction. The original run did not capture the RTP timestamps needed to prove that rollover caused that specific alert. The receive gaps remain a separate timing investigation. Both failed runs are retained, and the final candidate soak (`/tmp/rtpbridge-ready-soak/2026-09-08T23-50-28-682Z-seed-1234`) **passed** after 15 minutes 44 seconds: all 50 calls, 35 mutations and 14 impairment windows completed, with zero failures or flatlines. Its largest measured Node event-loop delay was 83.4 ms. This pass does not prove the earlier shared-host timing failures cannot recur.

The final capture retained 1,750,362 packets with zero kernel capture drops, including 1,746,763 PCMU RTP packets. All 142 captured RTP gaps above 250 ms aligned with lifecycle grace (8) or scheduled impairment/cooldown (134), using the monitor's two-sample/four-second contextual margin. This independent packet check does not alter the harness verdict. The capture contains RTP headers only, from the isolated 42000–42399 loopback test range. After teardown, session/endpoint/recording gauges were zero, every port in that range was available, and the owned server, browser, harness and TURN processes had exited. Peak observed server RSS was 47.5 MiB; maximum sampled `ps` CPU was 34.6%. These are observations for this run, not per-session resource guarantees. Evidence includes `/tmp/rtpbridge-ready-capture-classification.json`, `/tmp/rtpbridge-ready-resource-summary.json` and `/tmp/rtpbridge-ready-soak-rtp-headers.pcap`.

The final source and binary match the recorded manifest after all checks. The results support a staged rollout with the documented configuration changes. Dedicated-host capacity/SLO measurements, the expanded load matrix and an actual canary deployment remain outside this local verification.


Historical September 5 measurements remain separately identified. The final September 8 image ID is `sha256:716267955dc437ebc275cd097c7440a60e7c00e9bce3c9d520de50ed61e16147`. The checked source manifest is `/tmp/rtpbridge-ready-manifest.json`; the release binary SHA-256 is `1fe34c8c2b2459af77e45337ea19ae8e1b04115360ddad9a18e1f54fa73b49bc`. Remaining scanner advisories concern glibc/zlib; none were suppressed. The package SBOM does not identify the embedded OpenSSL library: its source version/checksum are pinned in the build helper and its actual linked version is verified by tests and startup logs.

## Finding coverage

| Finding | Delivered behavior | Regression evidence |
| --- | --- | --- |
| 1: same-key replay reset | SRTP/SRTCP replay and rollover state survive renegotiation | `security_regression_same_key_preserves_replay_and_rollover` and SRTCP replay tests |
| 2: foreign RTP/RTCP accepted | SDP/network policy, learned tuples, negotiated payload checks and complete RTCP validation precede accepted state; rejected sources have a separate diagnostic counter | Foreign-source and independent RTCP NAT tests |
| 3: mandatory SRTP downgrade | Validate secure SDP and exact SDES keys before committing negotiation; preserve established state on failure | `security_regression_invalid_secure_sdp_is_transactional` and secure-offer tests |
| 4: transfer deadlock | Nonblocking UDP enqueue, bounded receive-task joins, reserved command admission, one transfer owner and rollback | Full-packet-queue cancellation, full destination and lost-commit-reply tests |
| 5: leaked cache references | Exact header-sensitive, instance-specific leases own cache files through real decoder completion | Header-variant leases, late cancellation and cleanup tests |
| 6: filesystem work on media tasks | Bounded storage workers and PCM prefetch; separately admitted recording writers retain permits through actual completion | Worker cancellation/heartbeat, file, shared-playback and recording suites |
| 7: whole-recording HTTP buffering | Four downloads, 64 KiB chunks, opened-handle length snapshot, read/write/overall deadlines | 2 MiB slow-reader fixture with concurrent file growth; path-walk containment tests |
| 8: unrestricted URL destinations | Required origins, private-network policy, checked/pinned DNS answers, manual redirects, no environment proxy, cross-origin header stripping | Address-policy matrix and local redirect/header fixtures |
| 9: unbounded pending downloads | Owner, pending, active, entry and byte admission; queue time included in deadlines; last-owner cancellation | Admission saturation and surviving-owner tests |
| 10: old SRTCP key retained | Shared retirement deadline enforced by both receive paths and endpoint timers | New-key RTP followed by expired old-key SRTCP regression |
| 11: stalled WebSocket ownership | Independent bounded writer, write/Pong deadlines, cancellation and supervisor cleanup | Stalled-write byte-budget release and supervisor-drop regressions; WS integration suite |
| 12: failed attach loses expiry | Reserve delivery before attachment commits; orphan generation guards expiry | Paused-clock `failed_attach_preserves_original_orphan_deadline` |
| 13: repeated conference decoding | One source decode shared by transcode, mixing, VAD and fax; shared per-rate resampling; destination encoders retained | Source-audio, mixing, analysis and bridge suites; conference benchmark |
| 14: variable-duration samples lost | Bounded zero-or-many 20 ms PCM framing, short-packet draining, duration-correct clocks and wide final-clamp mixing | 2.5–120 ms framing, codec duration/rollover and clipping-order tests |
| 15: signed URLs in logs | Categorical download errors and summaries omit paths, query contents and header values | Logging redaction and forbidden-redirect error tests |
| 16: old playback deletes replacement | Per-generation playback/subscriber cleanup and observable terminal errors | Finished-generation replacement and invalid/shared-playback tests |

Additional fixes replace WS front-draining vectors with paced byte-ring input, gate recording descriptors when recording is disabled, preserve short-file EOF/loop samples, recreate encoders on codec changes, and spool PCAP payload/PCM data within record, duration, channel and disk limits. Converter tests reject forged capture lengths and extreme time gaps before large allocation or silence expansion.

## Compatibility and operation

- Control defaults to loopback. Non-loopback listeners require TLS and HMAC unless the corresponding explicit exception is configured. One HMAC key grants administrative access across sessions.
- Configure `rtp_source_networks` for approved alternate media addresses and `file_download_origins`/`file_download_networks` before enabling remote playback. Plain RTP tuple validation cannot authenticate spoofed IP traffic.
- Session and endpoint limits are finite and nonzero; the transcode cache must cover the endpoint cap. Fixed worker, queue and byte bounds are documented in [performance](./guide/performance.md).
- Each process needs its own locked cache directory. Cache files from the old naming scheme require a stopped-server migration if their disk space must be reclaimed.
- The runtime container now uses Distroless Debian 13 and UID/GID `65532:65532`. Adjust writable volume ownership and secret readability before upgrading. See [deployment](./guide/deployment.md).
- Recording stop marks the session boundary. A timed-out filesystem job can continue, retaining its resources until actual completion. Active recording downloads contain only the opened-handle snapshot and can end within a PCAP record.

## Initial implementation validation — September 5

| Check | Result | Local evidence |
| --- | --- | --- |
| Release unit tests | 717 passed | `/tmp/rtpbridge-remediation-release-tests.log` |
| Debug unit/converter tests | 715 library tests and 8 converter tests passed; the two subsequently added WS fault tests also passed; main duplicates the library suite | `unit-final3.log`, `ws-faults.log` under the artifact prefix |
| Full serial integration suite | 313 passed across 35 suites; zero failures or ignored tests | `/tmp/rtpbridge-remediation-integration-final.log` |
| Required Clippy, formatting, diff whitespace | Passed | `/tmp/rtpbridge-remediation-clippy-final.log`; `cargo fmt --check`; `git diff --check` |
| Release/container builds and all benchmark compilation | Passed | `docker-final2.log`, `bench-compile.log` under the artifact prefix |
| Rust dependency policy/advisories | `cargo deny check`: advisories, bans, licenses and sources passed | `/tmp/rtpbridge-remediation-deny.log` |
| 50-call full-duration soak, seed 1234, TURN required | Pass; zero failures/flatlines, all 50 calls complete, teardown gauges zero | `/tmp/rtpbridge-remediation-soak/2026-09-05T18-55-23-089Z-seed-1234/summary.json` and `metrics-after.prom` |
| Final candidate image smoke test | Nonroot startup and `GET /health` passed | Local image `rtpbridge-remediation:review` |
| Final candidate image scan/SBOM | Zero HIGH/CRITICAL; 13 MEDIUM and 7 LOW native-package advisories remain without a distribution fix listed by the scanner | `/tmp/rtpbridge-remediation-trivy-final.json`, `/tmp/rtpbridge-remediation-sbom.spdx.json` |
| Markdown/MDX | All changed documents passed static validation; no rendering server was available | mdxserve validator |

Primary commands were `cargo test --lib --bins -- --test-threads=1`, targeted WS fault tests, `cargo test --release --lib -- --test-threads=1`, `cargo test --test '*' -- --test-threads=1`, `cargo clippy -- -D warnings`, `cargo bench --no-run`, `cargo deny check`, and the existing soak runner with `--calls 50 --seed 1234 --require-turn`. The soak lasted 16 minutes 6 seconds and included eight calls over ten minutes. It used the release implementation before the final codec-transition and short-file fixes; those final changes passed the subsequent release/unit and full integration suites.

The required Clippy command passes. An exploratory `--all-targets` lint run exposed existing test-fixture style warnings; those unrelated fixture rewrites were not included. Local advisory validation used cargo-deny; cargo-audit remains a separate CI gate.

Raw local artifacts use the prefix `/tmp/rtpbridge-remediation-`. The image was built and scanned locally without pushing it. Its image ID is `sha256:1816ae746b52a3c99a03fae1d00f9923f1960cc6126582fee5af615acaafcbb5`. The scanned native packages include libopus `1.5.2-2`, OpenSSL `3.5.7-1~deb13u2` and glibc `2.41-12+deb13u3`. Remaining scanner advisories are in glibc and zlib; no blanket suppression was added. CI now scans the exact candidate, including unfixed high/critical findings, before promotion.

The development host is an Apple M1 Max with 64 GiB RAM, macOS 26.5.2, Rust 1.94.1/LLVM 21.1.8, native libopus 1.6.1 and OpenSSL 3.6.3. Conference benchmarks measure media processing per 20 ms conference interval, including destination encoding. The two-participant mixer benchmark deliberately exercises a mixer; production same-codec two-party passthrough does not decode unless analysis requires it.

The standard 50-call browser/RTP/TURN soak covers its existing call-mutation and impairment matrix. It does not establish a sustained conference/SRTP-rekey/recording/IPv6 load envelope. Unit and integration tests cover those individual paths. Dedicated-host scheduling p95/p99, allocation profiles, long resource-churn tests and a production canary remain release validation; no calls-per-core guarantee is inferred from decode-count reductions.

## Final candidate benchmark repeats — September 8

Three sequential repeats used the final compiled benchmark, without concurrent builds or soak traffic from this review. Other host workloads remained active (load averages 94–115 on ten cores), so these are comparisons within the benchmark, not dedicated-host capacity results. Each row gives the range of the three Criterion point estimates in milliseconds per 20 ms conference interval:

| 20-party workload | Per-destination decode | Shared decode | Ratio within each repeat |
| --- | ---: | ---: | ---: |
| Opus | 11.983–12.314 ms | 3.843–3.904 ms | 3.07–3.18× |
| Mixed PCMU/G.722/Opus | 3.760–3.791 ms | 0.820–0.846 ms | 4.44–4.63× |

The earlier September 8 mixed/shared result of 0.942 ms triggered a Criterion regression warning against its stored baseline. The three final repeats were lower, but remained about 2–6% above September 5's 0.800 ms point estimate. This host cannot establish whether that small difference is a regression. Opus intervals remain broad (roughly 3.01–4.94 ms for shared decoding across the repeats). Preserve that uncertainty; the consistently lower shared-decode cost does not establish an end-to-end scheduling SLO. Logs are `/tmp/rtpbridge-ready-benchmark-{1,2,3}.log`, with extracted estimates in `/tmp/rtpbridge-ready-benchmark-summary.json`.

## Conference processing measurements

The original implementation was built in an isolated checkout of `7614703`, retrospectively after the code changes. Baseline and new binaries ran sequentially without concurrent builds or soak traffic, using the same codec inputs, participant counts, toolchain and native libraries. Criterion used ten samples, one second of warmup and approximately two seconds of measurement per case. The mixed case cycles PCMU/G.722/Opus participants. Times are milliseconds to process one complete 20 ms conference interval; they exclude network, session scheduling, encryption, recording and analysis taps.

| Codec mix | Participants | Baseline ms | Shared decode ms | Baseline / shared |
| --- | ---: | ---: | ---: | ---: |
| Opus | 2 | 0.190 | 0.192 | 0.99× |
| Opus | 3 | 0.346 | 0.287 | 1.21× |
| Opus | 10 | 2.947 | 1.047 | 2.81× |
| Opus | 20 | 12.813 | 3.853 | 3.33× |
| Mixed | 2 | 0.014 | 0.016 | 0.92× |
| Mixed | 3 | 0.139 | 0.120 | 1.16× |
| Mixed | 10 | 0.977 | 0.377 | 2.59× |
| Mixed | 20 | 3.759 | 0.800 | 4.70× |

The two-party synthetic mixer is approximately unchanged for Opus and about 8% slower for the mixed case; production two-party routing uses passthrough or a single transcode edge, so this is not a measured regression of that route. Larger fanout benefits from shared decoding. The 20-party Opus estimate has substantial variability (95% interval: baseline 12.07–13.79 ms, shared 3.02–5.01 ms); use the interval, not a precise speedup promise. The 20-party mixed interval is tighter: baseline 3.68–3.85 ms and shared 0.798–0.804 ms.

Peak resident memory for each complete benchmark process was about 43.4 MiB baseline and 36.7 MiB shared. These numbers include Criterion, bootstrap analysis and allocator retention; they are not per-session memory measurements. Source decoding work for a fully active 20-party 20 ms conference decreases from 19,000 to 1,000 real input decodes/s, while summation and destination encoding remain. The live metric `rtpbridge_audio_packets_decoded_total` exposes this work for deployment measurements.

Raw timing logs are `/tmp/rtpbridge-remediation-baseline-bench.log` and `/tmp/rtpbridge-remediation-shared-bench-final.log`; the initial estimate JSON is `/tmp/rtpbridge-remediation-benchmark-summary-first.json`. The repository benchmark also includes PCMU and G.722 at 2/3/10/20 participants, but those cases are outside this isolated baseline table. Earlier exploratory timings ran alongside other work and were not used for this comparison.

A repeat of the 20-party Opus case measured 11.64 ms baseline (95% interval 11.56–11.69) and 3.78 ms shared (2.94–4.86). The improvement persisted, while the variability reinforces the need for a dedicated scheduling/load study. Repeat logs are `/tmp/rtpbridge-remediation-opus-repeat-baseline.log` and `/tmp/rtpbridge-remediation-opus-repeat-shared.log`.

A workspace-local evidence bundle is saved at `tmp/security-review-2026-09-08.zip` (ignored by Git). It contains this report, the source/binary manifest, final validation logs, all three September 8 soak verdicts, final load/teardown evidence, benchmark estimates, scan/SBOM outputs, and the final RTP-header capture with its analysis scripts.
