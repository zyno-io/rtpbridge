# M144 ICE re-nomination probe

This standalone macOS probe uses the exact `WebRTC-SDK` framework installed in
the ZynoTalk mobile workspace. It is intentionally outside the production app.
It checks that:

1. the experimental rtpbridge offer advertises `trickle renomination`;
2. M144 includes `renomination` in its answer;
3. ICE reaches `connected` without an ICE restart;
4. M144 sends an increasing nomination after the selected path is blackholed;
5. both peers' RTP counters and M144's SSRCs remain continuous across the
   selected-pair change.

The probe enables continual gathering. It sends the current local description
after the first candidate batch instead of waiting for gathering to become
`complete`, because continual gathering may remain in `gathering` by design.

Run rtpbridge with re-nomination and the experiment-only path fault enabled:

```sh
RTPBRIDGE_RENOMINATION_DROP_FIRST_PATH_AFTER_MS=5000 cargo run \
  --bin rtpbridge \
  --features legacy-ice-renomination-experiment \
  -- \
  --legacy-ice-renomination \
  --listen 127.0.0.1:19100 \
  --media-ip 127.0.0.1 \
  --log-level 'rtpbridge=info,is::agent=info'
```

Compile and run the probe from the rtpbridge repository:

```sh
ZYNOTALK_MOBILE_REPO='/path/to/zynotalk-mobile'
FRAMEWORK_DIR="$ZYNOTALK_MOBILE_REPO/ios/Pods/WebRTC-SDK/WebRTC.xcframework/macos-arm64_x86_64"

swiftc \
  -swift-version 5 \
  -F "$FRAMEWORK_DIR" \
  -framework WebRTC \
  experiments/m144-renomination-client/main.swift \
  -o /tmp/m144-renomination-client

DYLD_FRAMEWORK_PATH="$FRAMEWORK_DIR" \
  M144_EXPECT_RENOMINATION=1 \
  M144_SEND_AUDIO=1 \
  M144_BACKUP_PING_MS=1000 \
  M144_RECEIVING_TIMEOUT_MS=1000 \
  M144_STRONG_CHECK_MS=250 \
  M144_UNWRITABLE_TIMEOUT_MS=500 \
  M144_UNWRITABLE_MIN_CHECKS=2 \
  /tmp/m144-renomination-client ws://127.0.0.1:19100
```

The corresponding rtpbridge trace should show an authenticated Binding Request
with `NOMINATION=1`, activation of the first-path fault, and an increasing
nomination on a different socket tuple. That wire observation—not the SDP lines
alone—is the compatibility gate. The probe also routes a generated rtpbridge
tone to M144 and, when `M144_SEND_AUDIO=1`, adds a local M144 audio track. It
requires the corresponding inbound and outbound RTP counters to advance before
and after the selected-pair change.

`M144_BACKUP_PING_MS` maps to M144's
`iceBackupCandidatePairPingInterval`. Test multiple intervals and record both
the fault-to-nomination delay and battery/radio cost before choosing an app
default.

`M144_RECEIVING_TIMEOUT_MS` maps to M144's
`iceConnectionReceivingTimeout`. Aggressive values can turn short packet-loss
bursts into path churn, so they are experiment inputs—not rollout defaults.

The `M144_STRONG_CHECK_MS`, `M144_WEAK_CHECK_MS`, `M144_MIN_CHECK_MS`,
`M144_UNWRITABLE_TIMEOUT_MS`, `M144_UNWRITABLE_MIN_CHECKS`, and
`M144_INACTIVE_TIMEOUT_MS` inputs map to the corresponding advanced M144 ICE
configuration. They directly trade failure-detection latency for radio traffic
and false path switches; test a matrix under ordinary transient loss before
proposing production values.
