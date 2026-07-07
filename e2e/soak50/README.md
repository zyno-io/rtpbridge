# rtpbridge 50-Call Soak Harness

This is an opt-in black-box end-to-end harness for long-running media continuity
tests. It is not part of `cargo test`.

The runner creates a deterministic scenario with RTP <-> WebRTC,
WebRTC <-> WebRTC, and RTP <-> RTP calls. It samples rtpbridge stats, RTP peer
counters, and browser WebRTC `getStats()` counters while it runs scheduled ICE
restarts, RTP re-INVITEs, hold music insertion/removal, endpoint replacement,
transfer parking, and deliberate media impairments.

## Install

```bash
cd e2e/soak50
npm install
npx playwright install chromium
```

For relay scenarios, run or provide a TURN server:

```bash
docker compose -f docker-compose.turn.yml up -d
export TURN_URL=turn:127.0.0.1:3478
export TURN_USER=soak
export TURN_PASS=soakpass
```

For local relay runs, make sure rtpbridge `--media-ip` is reachable from the
TURN server. If coturn runs in Docker, `127.0.0.1` usually points at the coturn
container, not the host rtpbridge process. Use a host/LAN IP or a host-network
TURN setup for relay validation.

## Dry Run

Generate the deterministic call matrix without starting rtpbridge or browsers:

```bash
npm run dry-run -- --calls 50 --seed 1234
```

Artifacts are written under `artifacts/<timestamp>-seed-<seed>/`.

## Local Smoke

Build rtpbridge first:

```bash
cargo build
```

Run a short scaled scenario:

```bash
cd e2e/soak50
npm run soak -- \
  --calls 10 \
  --duration-scale 0.05 \
  --start-spread-ms 10000 \
  --webrtc-impairments 2 \
  --rtp-impairments 1 \
  --seed 1234 \
  --rtpbridge-bin ../../target/debug/rtpbridge \
  --media-ip 127.0.0.1 \
  --turn-url "$TURN_URL" \
  --turn-user "$TURN_USER" \
  --turn-pass "$TURN_PASS" \
  --load-pids "$(pgrep -x turnserver | paste -sd, -)"
```

## Full 50-Call Soak

```bash
cargo build --release

cd e2e/soak50
npm run soak -- \
  --calls 50 \
  --seed 1234 \
  --require-turn \
  --rtpbridge-bin ../../target/release/rtpbridge \
  --media-ip 127.0.0.1 \
  --turn-url "$TURN_URL" \
  --turn-user "$TURN_USER" \
  --turn-pass "$TURN_PASS" \
  --load-pids "$(pgrep -x turnserver | paste -sd, -)"
```

The default full run uses 2 to 15 minute call durations. Eight calls run longer
than 10 minutes.

By default the 50-call plan also schedules ten WebRTC impairments and four RTP
impairments. WebRTC impairments are mostly modeled as shaky Wi-Fi and cellular
conditions: burst drops, jitter, congestion spikes, and short handoff-like
periods. RTP impairments are milder packet-network loss/jitter cases. Override
with `--webrtc-impairments N` and `--rtp-impairments N`; use `0` to disable one
class.

## Existing Server

To run against an already-started rtpbridge:

```bash
npm run soak -- \
  --calls 50 \
  --seed 1234 \
  --control-url ws://127.0.0.1:9100 \
  --require-turn \
  --turn-url "$TURN_URL" \
  --turn-user "$TURN_USER" \
  --turn-pass "$TURN_PASS"
```

When `--control-url` is omitted, the runner starts `--rtpbridge-bin` with a
temporary config and temporary media/cache/recording directories.

## Assertions

The runner fails when:

- a control request unexpectedly fails;
- any active media direction flatlines beyond the allowed grace window;
- clean, non-grace media windows exceed RTP/WebRTC quality thresholds:
  RTP sequence gaps/loss, reordering, duplicates, high inter-arrival gaps,
  RTP jitter, WebRTC inbound loss, WebRTC jitter, or concealed audio samples;
- a WebRTC relay peer selects a non-relay local candidate;
- browser WebRTC enters `failed` or `closed` while active;
- rtpbridge emits `endpoint.media_timeout` or `events.dropped`;
- scheduled mutation execution fails.

Grace windows are applied around ICE restarts, RTP re-INVITEs, endpoint transfer
parking, endpoint replacement, and hold music insertion/removal. Deliberate
media impairments do not extend grace for packet-continuity checks; packets must
continue increasing while loss and jitter are active. Quality thresholds are
suppressed while a scheduled impairment is active, plus a short cooldown, so
expected injected degradation is captured in artifacts without failing the run as
bridge-caused clean-path degradation.

Quality failures include `detail.classification`, with `cause`, `path`,
`bridge_added`, and short evidence strings. Inbound bridge sequence loss/jitter
is classified as `bridge_rx_backpressure`, `bridge_session_dequeue_delay`, or
`bridge_rx_loop_gap` when bridge receive diagnostics show the socket reader or
session packet channel was late. For WebRTC ingress loss, raw SRTP/RTP header
sequence counters split pre-bridge browser/TURN/socket loss from
`bridge_webrtc_ingress_processing_loss`, where raw RTP ingress stayed continuous
but post-str0m RTP event stats reported loss. Receiver-side loss is classified
as upstream when source endpoint ingress loss rose in the same sample, as
`bridge_added_egress_loss` when bridge outbound packet counters fall behind the
receiver's expected sequence range, and otherwise as downstream
peer/browser/TURN receive-path loss. Timing gaps are marked
`bridge_added: "unknown"` unless packet counters prove an egress deficit,
because the black-box stats do not include per-packet bridge send timestamps.

## Artifacts

Each run writes:

```text
artifacts/<timestamp>-seed-<seed>/
  summary.json
  timeline.jsonl
  call-matrix.json
  rtpbridge.log
  metrics-before.prom
  metrics-after.prom
  sdp/
  browser-stats/
  bridge-stats/
  load-stats/
  rtp-peer-stats/
```

`summary.json` contains the final verdict and per-call status. `timeline.jsonl`
contains structured lifecycle and mutation events.
`bridge-stats/*.jsonl` includes the bridge's inbound loss/jitter view and peer
snapshots. The runner subscribes with `include_diagnostics: true`, so these
samples also include raw socket receive counters, WebRTC raw RTP sequence
counters, recv-loop gaps, enqueue waits, session dequeue delays, and channel
capacity/overflow diagnostics.
`rtp-peer-stats/*.jsonl` includes receive-side RTP sequence, loss, duplicate,
reorder, jitter, and inter-arrival measurements.
`browser-stats/*.jsonl` includes browser WebRTC inbound loss, jitter,
jitter-buffer, and concealed-sample measurements.
`load-stats/system.jsonl` samples OS load plus rtpbridge, the soak runner,
Chromium child processes, and any comma-separated process IDs passed with
`--load-pids` such as local coturn.

## Notes

- The harness uses browser WebRTC via Playwright/Chromium because this test is
  intended to validate real ICE, DTLS-SRTP, RTP, and TURN behavior.
- RTP peers use real UDP sockets and paced PCMU RTP packets.
- The generated hold music WAV lives in the temporary media directory when the
  runner starts rtpbridge.
- This harness is intentionally long-running and should be invoked explicitly.
