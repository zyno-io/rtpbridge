# WebRTC RTP-mode pending-packet loss - incident research / runbook

## Summary

In str0m 0.21 RTP mode, inbound RTP media is not queued internally. Each accepted
RTP packet is stored as a single pending packet and emitted on the next
`poll_output()`. If rtpbridge feeds more than one WebRTC RTP datagram into
str0m before polling output, a later packet can replace an earlier pending
packet. The bridge then reports RTP sequence loss even though the packet reached
the UDP socket and decoded successfully inside str0m.

The session loop must therefore drain a WebRTC endpoint's `poll_output()`
immediately after each WebRTC `handle_receive()` call. Batching UDP datagrams is
still fine for the session channel, but WebRTC RTP output must not wait until the
end of a multi-packet batch.

## Failure Signature

Observed in a 50-call, 5-minute TURN soak:

- Browser/TURN/pcap showed continuous RTP sequence numbers into rtpbridge.
- WebRTC raw RTP counters in rtpbridge also showed no raw sequence gaps,
  duplicates, reordering, or overflow.
- Post-str0m inbound RTP stats showed `packets_lost` increments.
- For each failing window, this invariant held:

```text
raw_rtp_packets == inbound.packets + inbound.packets_lost
```

That means every RTP packet reached the bridge socket, but some packets that
entered str0m were not emitted back to rtpbridge as `Event::RtpPacket`.

## Root Cause

str0m 0.21 stores one pending RTP packet in RTP mode:

```rust
self.pending_packet = Some(packet);
```

and later emits it with:

```rust
self.pending_packet.take()
```

Before the fix, rtpbridge's session loop could receive one packet from the
session channel, then batch-drain up to 64 more queued packets before polling
WebRTC endpoint output. When two or more RTP datagrams for the same WebRTC
endpoint were handled in that interval, the later str0m input overwrote the
earlier pending packet.

This was bridge-added loss. It was not caused by TURN, browser loss, kernel pcap
drops, SRTP auth errors, or session-channel overflow.

## Fix

After each WebRTC datagram is passed to `handle_receive()`, immediately call the
shared WebRTC output drain helper and push emitted RTP packets into the normal
inbound routing path. This preserves the existing routing, DTMF, analysis,
recording, playout, and mixer behavior while satisfying str0m's RTP-mode polling
contract.

The session still performs its normal periodic WebRTC poll to drive timeouts,
state changes, and any non-RTP output.

## Diagnostics To Keep

The raw receive diagnostics are intentionally lightweight and worth keeping:

- `raw_packets` / `raw_bytes`: any UDP datagram accepted by the endpoint socket.
- `raw_rtp_packets` / `raw_rtp_packets_lost` / sequence gap counters: RTP-looking
  datagrams before str0m processing.
- queue delay, channel capacity, and overflow counters: session-channel pressure.
- post-str0m `inbound.packets` / `inbound.packets_lost`: media packets emitted
  by the WebRTC engine.

Together these counters classify bad audio without packet captures:

| Evidence | Likely cause |
|---|---|
| raw RTP sequence gaps increase | upstream browser, TURN, network, or socket ingress loss |
| raw RTP continuous but post-str0m loss increases | bridge WebRTC ingress processing loss |
| channel overflow or large dequeue delay increases | bridge backpressure |
| bridge ingress clean but receiver loss increases | egress/downstream/browser receive path |

These counters add hot-path work, but only a small RTP-header parse and atomic
counter updates per datagram. They do not allocate, log, or lock per packet.
Detailed stats serialization is opt-in via `stats.subscribe` with
`include_diagnostics: true`, so clients that poll stats for every session keep
the compact default payload. E2E browser/RTP/load artifact collection is outside
the production media path.

## Verification

After the fix, the same seeded 50-call TURN soak passed:

- `failures: []`
- `failure_classification_counts: {}`
- `max_flatline_ms: 0`
- `rtpbridge_srtp_errors_total: 0`
- `rtpbridge_webrtc_packet_errors_total: 0`
- `rtpbridge_webrtc_recv_overflow_total: 0`
- aggregate WebRTC final stats had zero raw RTP loss/gaps/duplicates/reordering
  and zero post-str0m `packets_lost`

The key invariant after the fix is:

```text
raw_rtp_packets == inbound.packets
packets_lost == 0
```

for clean WebRTC RTP ingress.
