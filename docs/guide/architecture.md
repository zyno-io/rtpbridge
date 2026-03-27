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

### Per-Endpoint Sockets

Each endpoint (WebRTC or RTP) binds its own UDP socket(s) rather than sharing a mux. WebRTC endpoints use a single socket (ICE multiplexes RTP and RTCP). Plain RTP endpoints use two sockets (RTP + RTCP) unless `rtcp-mux` is negotiated, in which case they also use one. This is simpler and works well with ICE, which handles port discovery. For WebRTC, the OS-assigned port becomes the ICE candidate.

### Symmetric RTP

Plain RTP endpoints use symmetric RTP with time-windowed address learning for NAT traversal. When an endpoint is created, the remote address from the SDP is recorded as the initial send target. During a 5-second learning window after creation, the source address of the first inbound packet overrides the SDP address. This handles NAT scenarios where the remote's actual transport address differs from what was advertised in the SDP.

Once the learning window expires, the address is locked and no further updates occur. If `rtcp-mux` is negotiated, the RTCP address tracks the RTP address; otherwise RTCP is sent to RTP port + 1.

### One Task Per Session

All endpoints in a session share a single tokio task. Media routing between endpoints is a function call, not cross-task messaging. This avoids `Arc<Mutex<>>` on str0m `Rtc` instances and keeps latency minimal.

The session task runs `tokio::select!` over:
- Inbound UDP packets (from all endpoint recv tasks)
- Control commands (from the WebSocket handler)
- Timers (str0m timeouts, RTCP intervals, file playback ptime)

### DTMF Never Transcoded

Telephone-event (RFC 4733) packets bypass the transcode pipeline entirely. They're identified by payload type, forwarded as-is with PT remapping if endpoints negotiated different dynamic PTs.

### VAD Independent of Recording

Voice Activity Detection and recording are completely separate features. VAD monitors an endpoint's incoming audio and emits events. Recording captures raw packets to PCAP. They can be used independently or together.

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
