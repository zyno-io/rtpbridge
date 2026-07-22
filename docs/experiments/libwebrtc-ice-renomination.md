# Libwebrtc ICE re-nomination experiment

Status: desktop protocol/media gate passed; runtime capability remains disabled
by default pending real iOS and Android staging gates.

## Current result (July 21, 2026)

Stages 1 through 4 and the desktop M144 protocol/media gate now pass:

- `is` parses and serializes authenticated `NOMINATION (0xC001)` values.
- Zero, stale, duplicate, malformed, tampered, and unnegotiated nominations do
  not change the selected route.
- A newer nomination selects a lower-priority pair and emits a new
  `NominatedSend` event.
- An ICE restart resets the nomination sequence, and ordinary `USE-CANDIDATE`
  remains a working fallback.
- str0m advertises and enables the extension only after bilateral SDP support.
- A synthetic str0m `Rtc` test moves the actual send address from a
  Wi-Fi-equivalent pair to a lower-priority cellular-equivalent pair while
  preserving the same local and remote ICE credentials.
- rtpbridge advertises `a=ice-options:trickle renomination` only when its
  disabled-by-default runtime capability is enabled.
- The exact `WebRTC-SDK 144.7559.08` macOS framework installed by ZynoTalk
  answered that offer with `a=ice-options:trickle renomination`, established
  ICE and DTLS without an ICE restart, and sent authenticated increasing
  nominations that the patched controlled agent accepted.
- In rtpbridge session `a4dd6e48-f693-4aa8-8531-3da2ce6077b1`, a bidirectional
  fault on the first selected tuple caused M144 to move from
  `10.24.0.236:53703` (`NOMINATION=1`) to `10.24.0.30:56228`
  (`NOMINATION=2`). No offer/answer or ICE-restart RPC occurred. The same
  peer connection continued sending and receiving encrypted RTP: M144 counters
  advanced from 76/76 packets to 350/663 packets inbound/outbound, while
  rtpbridge's validated media counters advanced from 76/76 to 466/635. M144's
  inbound/outbound SSRCs remained `76711352`/`814641561`, and rtpbridge's offer
  generation remained unchanged.
- rtpbridge learned the selected tuple from str0m's authoritative selected-pair
  state rather than treating the destination of every STUN transmit as the
  nominated media route.

The protocol works, but the latency target has not passed. With M144 defaults,
the synthetic bidirectional hard-blackhole took about 6.06 seconds from fault
activation to the replacement nomination. A still-aggressive timer set
(`backup=1000 ms`, `receiving=1000 ms`, strong checks `250 ms`, unwritable
`500 ms`/2 checks) reduced this to about 4.32 seconds. More extreme 100-250 ms
settings caused initial ICE setup to fail. These are single local runs, not a
statistical benchmark, but they prove that merely enabling the protocol does
not make a blackholed path seamless and that blindly minimizing ICE timers is
unsafe.

A real OS-reported interface removal may switch faster than this synthetic
blackhole because libwebrtc's network manager can invalidate the departed
network immediately. Real iOS/Android Wi-Fi/cellular tests, gap measurement,
SSRC/sequence continuity assertions, false-switch tests under transient loss,
and battery/radio measurement remain stage 5 go/no-go gates.

## Question

Can rtpbridge's str0m 0.21 controlled ICE-lite endpoint accept libwebrtc's
legacy ICE re-nomination and move an established bidirectional media session
between candidate pairs without an offer/answer exchange, ICE restart, DTLS
restart, SRTP reset, SSRC change, or RTP timeline reset?

## Protocol under test

- SDP capability token: `renomination`
- STUN attribute: `NOMINATION` (`0xC001`), exactly four bytes
- Value: non-zero unsigned 32-bit nomination number in network byte order
- Sender: the full, controlling libwebrtc ICE agent
- Receiver: the controlled rtpbridge ICE-lite agent

The receiver applies a nomination only after the Binding Request has passed the
existing username, ICE generation, role, and message-integrity checks. Within
one ICE generation, only a value greater than the highest authenticated value
already accepted can replace the nominated send pair. Zero, stale, duplicate,
malformed, wrong-generation, and unauthenticated values cannot change routing.
Pair pruning and later connectivity events cannot select an older nomination;
if the newest nominated pair disappears, the existing recovery ladder must wait
for a newer nomination or perform an ICE restart. An ICE restart resets the
remote nomination sequence.

Ordinary `USE-CANDIDATE` remains the baseline for peers that do not negotiate
the extension. Merely adding the SDP token is forbidden: after negotiation,
libwebrtc can use `NOMINATION` in place of `USE-CANDIDATE`, so parsing and pair
selection must be complete before an offer advertises support.

## Experiment stages

1. Add STUN parse/serialize fixtures to the `is` crate for valid, malformed,
   zero, duplicate, stale, and unauthenticated nomination attributes.
2. Add controlled-agent state tests with two successful pairs where the newly
   nominated pair has lower ICE priority than the current pair.
3. Add str0m SDP capability plumbing and a pair-change test that preserves the
   existing ICE, DTLS, SRTP, and RTP session state.
4. Point a local rtpbridge experiment build at the patched checkout. Keep the
   capability disabled by default and expose it only through an explicit
   experiment configuration.
5. Run a real M144 libwebrtc endpoint against that build, capture SDP and STUN,
   force Wi-Fi/cellular-equivalent path changes, and verify receiver RTP in both
   directions.

### Safe local path-failure injection

The experiment feature may expose an additional, opt-in fault injector through
`RTPBRIDGE_RENOMINATION_DROP_FIRST_PATH_AFTER_MS`. It learns the first selected
ICE pair from str0m, waits the configured interval, then discards inbound and
outbound datagrams for only that selected remote socket tuple. It leaves every
other candidate path untouched. This simulates bidirectional loss of one
interface without disabling, firewalling, or otherwise modifying a developer
machine's real network interfaces.

The injector is permitted only in builds compiled with
`legacy-ice-renomination-experiment`, is inactive when the environment variable
is absent, drops only the first selected path, and must log both the injected
failure and the replacement selected pair. It is experiment scaffolding, not a
production recovery mechanism.

The M144 probe accepts explicit environment inputs for backup-pair ping,
receiving, strong/weak/minimum check, unwritable, and inactive timers. These
exist to measure a safe operating envelope. No tested value is a proposed
production default yet.

The patched str0m and `is` crates are pinned to immutable fork commit
`750888c45c338bcbad64c51f009417b0bf459bd1`. A normal build contains the
capability but leaves it disabled. Enable it in a staging configuration with:

```toml
legacy_ice_renomination = true
```

or at process startup with `--legacy-ice-renomination`. The standard PR image
does not contain the path-failure injector. To reproduce the desktop fault test
locally, compile that additional scaffolding explicitly:

```bash
cargo test \
  --features legacy-ice-renomination-experiment
```

The Cargo feature gates only the fault injector. Capability advertisement is
controlled solely by the runtime setting, so the same image can be enabled in a
staging canary and disabled immediately without rebuilding or changing tags.

## Pass criteria

- A real M144 endpoint emits increasing `NOMINATION` values after negotiating
  the SDP option.
- A newer nomination selects its pair even when that pair has lower ICE
  priority than the previously nominated pair.
- The next rtpbridge transmit uses the new remote address without a new SDP,
  ICE generation, DTLS association, SRTP context, SSRC, sequence origin, or RTP
  timestamp origin.
- Inbound and outbound RTP advance on the new path within the experiment's
  bounded verification interval.
- Duplicate, stale, malformed, zero, and unauthenticated attributes do not
  change the route.
- Calls without negotiated support continue to nominate with `USE-CANDIDATE`.
- A forced failure still recovers through the existing ICE-restart flow.

## Stop criteria

Stop and reassess before production integration if address migration requires a
broad rewrite of str0m's DTLS/SRTP or session model, if authenticated nomination
cannot be made monotonic without breaking standard ICE, or if real-device media
gaps do not consistently improve on the current restart path. The alternatives
are true bridge-assisted make-before-break and a measured WebRTC-engine bake-off,
in that order.
