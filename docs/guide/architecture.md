# Architecture

## Overview

rtpbridge is an async media server built on [tokio](https://tokio.rs). It uses a task-per-session model where all endpoints in a session share a single tokio task, avoiding locks on media state.

```
                    WebSocket Control (JSON)
                           |
                    +------v------+
                    | SessionMgr  |  DashMap<SessionId, Session>
                    +------+------+
                           |
              +------------v------------+
              |     Session Task        |  (one per session)
              |                         |
              |  +--------+ +--------+  |
              |  | WebRTC | |  RTP/  |  |  +--------+  +----------+
              |  |  str0m | | SRTP   |  |  |  File   |  | Recording|
              |  +---+----+ +---+----+  |  |Playback |  |  (PCAP)  |
              |      |          |       |  +----+----+  +----------+
              |      +----+-----+-------+-------+
              |           |
              |    Routing Table
              +-------------------------+
                           |
              +------------v------------+
              |    Per-Endpoint UDP     |
              +-------------------------+
```

## Key Design Choices

### Sans-I/O WebRTC (str0m)

WebRTC endpoints use [str0m](https://github.com/algesten/str0m) in Sans-I/O mode. str0m has no internal threads or async runtime — all state is driven by our event loop via `handle_input()` and `poll_output()`. This gives us full control over packet routing and timing.

For WebRTC RTP mode, rtpbridge polls str0m output immediately after each inbound
RTP datagram is passed to `handle_input()`. This ordering is intentional: str0m
0.21 stores only one pending RTP packet for the next `poll_output()`, so feeding
multiple RTP datagrams before polling can replace an earlier pending packet.
The session task may batch-drain its packet channel, but each WebRTC datagram is
followed by an endpoint-local output drain before the next WebRTC datagram can
be handled.

### Per-Endpoint Sockets

Each socket-backed endpoint binds its own UDP socket(s) rather than sharing a mux. Session-created WebRTC endpoints bind one socket per configured media address family, with ICE multiplexing RTP/RTCP and DTLS/SRTP on that port. Both offer and answer creation use the inclusive `rtp_port_range` by default; optional `webrtc_port_range` overrides it for WebRTC only. `MediaBindings` carries the effective range for managers created directly or from configuration. Allocation starts at a random port, scans each port at most once, and skips addresses already in use. Exhaustion or another bind error fails endpoint creation; it never falls back outside the effective range. ICE host candidates advertise the actual bound socket addresses. A failed dual-stack allocation drops any sockets already bound for that endpoint.

Plain RTP/SRTP endpoints allocate an even/odd pair from `rtp_port_range` for RTP and RTCP sockets; if `rtcp-mux` is negotiated, RTCP traffic is demuxed on the RTP socket but the local pair is still allocated. When the ranges overlap, socket binding coordinates ownership: each allocator skips occupied ports, and a failed RTCP bind releases the tentative RTP socket. Single WebRTC sockets can fragment free RTP pairs, so deployments needing separate capacity can use the override. Endpoint teardown releases sockets, and ICE restart retains the existing endpoint sockets and ports.

### Symmetric RTP

Plain RTP and SDES-SRTP endpoints use symmetric RTP with first-packet tuple latching for NAT traversal. The SDP address is the initial send target, but the first valid inbound packet from an allowed initial source overrides it and pins the exact IP and port. The default source policy permits any initial IP, so direct clients behind arbitrary NATs can establish media; deployments can restrict initial sources to the SDP IP or trusted relay CIDRs through `rtp_source_networks`. A tuple stays pinned until an accepted SDP renegotiation or an explicit direction reset, so later packets cannot autonomously migrate the call.

Separate RTCP latches its own source and port, allowing NAT mappings that do not preserve RTP-plus-one. If `rtcp-mux` is negotiated, RTP and RTCP instead share the one pinned tuple.

### One Task Per Session

All endpoints in a session share a single tokio task. Media routing between endpoints is a function call, not cross-task messaging. This avoids `Arc<Mutex<>>` on str0m `Rtc` instances and keeps latency minimal.

The session task runs `tokio::select!` over:
- Inbound UDP packets (from all endpoint recv tasks)
- Control commands (from the WebSocket handler)
- Timers (str0m timeouts, RTCP intervals, file playback ptime)

The UDP recv tasks do not parse or route media. They stamp receive time, update
wire-level counters, and forward datagrams to the owning session task over a
bounded channel. The session task owns all endpoint state and is the only place
that drives str0m, RTP parsing/decryption, playout, mixing, and routing.

### Shared Audio Clock

The session keeps one 20 ms playout clock running while a destination mixer exists or an
engaged playout buffer has pending audio. File and tone sources bypass those buffers, but their
mixed output still uses this clock. Packet and control-command wakes between ticks must preserve
the next deadline, including when a mixer has consumed its current frames and is waiting for
the next ones. The clock parks only after all mixers are removed and the playout buffers are
idle. An idle mixer emits no audio.

### DTMF Never Transcoded

Telephone-event (RFC 4733) packets bypass the transcode pipeline entirely. They're identified by payload type, forwarded as-is with PT remapping if endpoints negotiated different dynamic PTs.

### Transcoding Observability

Routing uses each WebRTC endpoint's negotiated audio codec and RTP clock rate,
including PCMU; an unanswered endpoint has no known audio codec. Outbound
timeline steps use that negotiated RTP clock. A clock change clears the learned
packet duration and re-anchors the next packet with a marker, preserving the
destination-owned timestamp timeline. Inbound WebRTC jitter uses the received
payload type's negotiated RTP clock.

Transcoding metrics follow the live routing table, which includes only connected
or playing endpoints and respects their directions. They describe required work,
rather than packet throughput or the contents of the encoder cache.

A session with a codec mismatch on an RTP/WebRTC-to-RTP/WebRTC route contributes
once to the cumulative transcoding-session counter and once to the active gauge.
The first mismatch logs a warning with the session ID, endpoint IDs, and codec
pair. Removing every mismatched peer route clears the active contribution;
restoring one does not increment the cumulative counter again. Same-codec mixing,
DTMF, files, tones, WebSocket PCM, and bridge endpoints do not contribute to this
peer-codec mismatch signal.

File transcoding has a separate active gauge counting directed routes from
playing file endpoints to destinations with a different codec or PCM sample
rate. One file feeding three encoded destinations contributes three, including
when a destination mixes it with other sources. Shared file decoding does not
collapse the destination encodes. Buffering, paused, finished, unrouted, and
removed files contribute zero. Session-owned metric contributions are released
on task exit, including cancellation or panic. See
[Monitoring & Observability](./observability.md#transcoding-demand) for metric
names and alert queries.

### VAD Independent of Recording

Voice Activity Detection and recording are completely separate features. VAD monitors an endpoint's incoming audio and emits events. Recording captures raw packets to PCAP. They can be used independently or together.

VAD combines decoded audio time with monotonic silence timing so sparse Opus DTX packets cannot
stretch the cutoff. The exact event timing is owned by the [VAD protocol](../protocol/vad.md).
Applications that need outgoing RTP while listening can explicitly own a
[`silence` tone endpoint](../protocol/endpoints.md#endpoint-create-tone); recording and VAD alone do
not create outgoing audio.

## Threading Model

```
Main thread
  ├── WebSocket listener task
  │     └── Per-connection tasks (one per WS client)
  │           └── Sends commands to session via mpsc channel
  ├── Session tasks (one per media session)
  │     ├── Drives str0m Rtc instances
  │     ├── Routes media between endpoints
  │     ├── Processes DTMF, VAD, recording taps
  │     └── Sends events back to WS connection
  ├── Per-endpoint UDP recv tasks
  │     └── Forwards packets to session via mpsc channel
  ├── Recording write tasks (one per active recording)
  │     └── Writes PCAP packets from bounded channel
  └── Signal handler task (SIGINT/SIGTERM)
```
