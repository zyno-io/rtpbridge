# WebRTC receive-task wedge — runbook

**One-line:** On an affected pod, *every* new WebRTC endpoint silently fails to
establish media — signaling succeeds, the endpoint's UDP socket is bound, but
**nothing ever reads it**, so ICE never completes, str0m never replies, and the
call dies with `endpoint media timeout`. Control plane, `/health`, `/metrics`
stay perfectly healthy. A pod restart fixes it; it recurs.

This is the failure the commit *"Add telemetry for WebRTC accept_answer wedge
investigation"* (`11e41e8`) was chasing. First fully diagnosed 2026-06-02.

---

## 1. How it presents

- Users on calls routed to one pod: "dropped calls", "no audio then it hangs
  up". Intermittent across the fleet because `rtpbridge-lb` spreads calls over
  all pods — only the calls that land on the wedged pod fail (≈ `1/N`).
- Client (softphone) logs: ICE → `failed`, *"no active candidate pair found"*,
  slow/relayed-only candidate gathering, `cRequestIceRestart`, then teardown.
- Bridge logs: SIP + SDP complete (INVITE→200→ACK, answer relayed, DTLS role
  stored), then `endpoint.media_timeout`, zero packets.
- rtpbridge (the wedged pod): `WebRTC negotiation stuck past watchdog threshold`,
  `endpoint media timeout duration_ms≈5000`, endpoints stuck `Connecting`.
- **No panic, no ERROR, no restart** in the pod's logs. The wedge is silent and
  develops mid-life (not at startup).

### Blast radius that confirms it's *this*
- Multiple devices on different networks fail → server-side, not client/network.
- Isolated to **one pod**: `rtpbridge_packets_routed_total` is ~flat for the bad
  pod while it still holds `rtpbridge_sessions_active` > 0; peers route media
  normally.
- coturn on the **same pod/IP** answers STUN + TURN Allocate fine → not the node,
  not the network, not coturn.

---

## 2. Confirm it fast (≈2 min)

**Supervision metrics (after the supervision fix):**

```promql
# Any non-zero value means a live endpoint lost its WebRTC UDP reader.
increase(rtpbridge_webrtc_recv_task_start_timeout_total[15m])
or
increase(rtpbridge_webrtc_recv_task_dead_total[15m])
```

For the never-started variant specifically:

```promql
increase(rtpbridge_webrtc_recv_task_start_timeout_total[15m])
```

Also useful:
- `rtpbridge_webrtc_recv_task_started_total` flattening while calls keep arriving
  means new recv tasks are not reaching their receive loop. If it continues
  climbing, the task-start path is alive and the failure is further downstream
  (socket readiness or packet delivery).
- `rtpbridge_webrtc_connecting_stuck_total` climbs; `..._packets_routed_total`
  flat; `rtpbridge_sessions_active{pod=...}` > 0 with no packets.
- Grep the pod's logs for `recv task never started within the grace window` /
  `recv task is gone but endpoint is still active`.

**Live probe (reproduces it on demand, A/B against a healthy pod):**

```bash
# Control WS is cluster-internal; port-forward the SUSPECT and a HEALTHY pod.
kubectl -n zynotalk-cluster port-forward pod/rtpbridge-<bad> 9111:9100 &
kubectl -n zynotalk-cluster port-forward pod/rtpbridge-<good> 9112:9100 &

cargo run --example media_probe -- --ws 127.0.0.1:9111 --label bad  --secs 9
cargo run --example media_probe -- --ws 127.0.0.1:9112 --label good --secs 9
```

`examples/media_probe.rs` connects to the control WS, creates a WebRTC endpoint,
acts as a full-ICE str0m peer, and sends real SRTP. On a healthy pod you see
`ICE: CONNECTED` and rtpbridge's `inbound.packets` climbing. On a wedged pod you
see `ICE: NOT CONNECTED` — the endpoint's media socket never answered.

---

## 3. Root cause

The WebRTC UDP receive path is **per-endpoint and fire-and-forget**:

- Each endpoint binds its own UDP socket (`new_with_socket`,
  `src/session/endpoint_webrtc.rs`) and spawns a task that loops on
  `socket.recv_from()` → forwards `InboundPacket` to the session's `packet_tx`
  (`start_recv_task`). The session driver drains `packet_rx`
  (`run_media_session`, `src/session/media_session.rs`) and feeds str0m via
  `Input::Receive`.
- str0m is **downstream** of this. If the recv task never reads the socket,
  str0m never sees the inbound STUN, never validates a candidate pair, never
  replies, never emits `Connected`. The call then trips the media-timeout reaper.

**Decisive evidence for the blackhole (strace of the pod's single tokio worker
while a probe sent STUN to the endpoint's socket):** the socket is
`bind`+`getsockname`'d, then **zero `recvfrom`/`sendto` on it, ever**, while the
same worker concurrently and healthily services the control WS, `/health`,
`/metrics`. So the process never services that endpoint socket, while the rest
of the process runs fine. By itself, that trace does not distinguish "task never
registered/polled the socket" from "socket readiness/packet delivery never
arrived"; §5 has the captures that separate those cases.

Because creation only `tokio::spawn`s the task and returns the SDP **without
confirming the task ever started**, this was completely silent — the endpoint
looked created, but its media leg was dead on arrival.

### What is *not* the cause (ruled out)
- Not coturn (live STUN + Allocate both succeed on the same pod/IP).
- Not the node/network (coturn on the same public IP is reachable; node UDP
  error counters clean).
- Not a deadlock or spin (the worker sits idle in `epoll_wait`; `/proc` thread
  state identical to a healthy pod).
- Not a panic (none logged in the pod's entire life; the recv task also
  `catch_unwind`s and would log `recv task panicked`).
- Not str0m credential divergence (that would still *read* the socket then
  reject; here the socket is never read). The unrelated `pending_offer` /
  ICE-restart-generation work is a *different*, per-call bug — see the
  `ice_restart` guard + `offer_generation` — not this process-wide wedge.

### Still open: the exact trigger
We proved the *symptom* (socket never read) and the *defect* (unsupervised
fire-and-forget recv loop), but not the precise event that makes *every* new
endpoint's media socket fail process-wide. Each endpoint gets a fresh
`CancellationToken`, so "cancelled before first poll" doesn't obviously explain
all of them. The surviving theories:

1. **Recv task never scheduled / never registers the fd** — a tokio
   scheduling/runtime wedge on the effectively single-worker runtime
   (`#[tokio::main]`, container CPU-limited to ~1 core). Restart-fixes-it
   strongly favors this (in-process state).
2. **Recv task starts, but readiness/waker delivery is broken** — the fd is
   registered with epoll, but Tokio never observes socket readiness and therefore
   never issues `recvfrom`.
3. **Packets never reach the socket** (per-socket kernel/datapath) — far less likely;
   a same-node restart fixes it and coturn on the same IP works.

The measurements that would settle it need the pod **still wedged** (see §5).

---

## 4. The supervision fix (what changed)

Goal: turn a silent, multi-hour, hard-to-attribute blackhole into a **loud,
observable, attributable** failure — and prevent one variant outright.
(`src/session/endpoint_webrtc.rs`, `src/session/media_session.rs`,
`src/metrics.rs`.)

1. **Liveness flag + sweep (off the hot path).** The recv task flips
   `recv_started` the instant it reaches its loop, and `start_recv_task` arms a
   `recv_start_deadline` (grace = `RECV_TASK_START_GRACE`, 2s). The session's
   reliable 1 Hz maintenance pass (the elapsed-gated block that also runs the
   connecting-watchdog — NOT the starvable `sleep` select arm) calls
   `supervise_recv()` per WebRTC endpoint, which — once per endpoint — flags and
   counts two failure modes: a task that **never reached its receive loop**
   (flag still false past the deadline → the never-started variant) and a task
   that **started then died** (`JoinHandle::is_finished()` while the endpoint is
   still active).
   This covers `create_offer`, `from_offer`, AND the transfer-restart path
   uniformly, and **never blocks creation or the session task**. (An earlier
   draft awaited the start signal inside `create_offer`; on a wedged endpoint
   that 2s await would stall *co-session* endpoints' media/commands — the session
   is one task — so it was moved to the sweep.)
2. **Non-blocking forward.** `packet_tx.send(..).await` → `try_send`. A full
   session channel can no longer *park* the reader (which stops it servicing the
   socket and blackholes the endpoint — the very failure mode). Packets are
   dropped under backpressure and counted. **This drops STUN/DTLS as well as
   RTP/SRTP**: a full 256-deep channel is itself an overload signal, and dropping
   a setup packet beats wedging the socket — but under sustained cross-endpoint
   backpressure it can slow ICE/DTLS setup. Class-aware priority for STUN/DTLS is
   a follow-up (§6).
3. **Metrics** (all `rtpbridge_…_total`):
   - `webrtc_recv_task_started` — recv tasks that reached their receive loop
     (task-start heartbeat; flattening while calls arrive means the never-started
     variant).
   - `webrtc_recv_task_exited` — recv loops that exited **cooperatively**
     (cancellation / session-close / UDP error). NOT `Drop`-aborts, so it sits
     below `_started` rather than mirroring it; a spike = abnormal exits.
   - `webrtc_recv_task_dead` — liveness sweep found a finished recv task while
     the endpoint was still active. Non-zero ⇒ live endpoint has no UDP reader.
   - `webrtc_recv_task_start_timeout` — endpoints whose recv task never started
     within the grace window. Non-zero ⇒ the never-started variant.
   - `webrtc_recv_overflow` — packets dropped on a full session channel.

Detection for the supervised variants lands within ~grace + one sweep (≈2–3s),
before the call's `endpoint media timeout` (5s). It makes those failures visible
and attributable; it does **not** prove or auto-recover a readiness/datapath
failure where the recv task started and stayed alive. See §5/§6.

---

## 5. Diagnosing a recurrence — capture BEFORE you restart

The pod restart is the mitigation, but it erases the evidence. If you can spare
the wedged pod for ~5 min, grab these (they are what's still missing to close
§3's open trigger). All read-only.

1. **Pin the pod & a live endpoint.** Run the probe (§2) against the bad pod;
   note the `rtpbridge media candidate = <ip>:<port>` it prints.
2. **Recv-Q on that endpoint socket** — first split: are packets reaching the
   kernel socket?
   ```bash
   # In the rtpbridge container (or a debug sidecar sharing its netns):
   #   nonzero Recv-Q  => kernel has packets, app isn't reading  => task/readiness branch
   #   zero    Recv-Q  => packets aren't arriving                 => datapath branch
   grep <hex(port)> /proc/<pid>/net/udp   # rx_queue column
   ```
3. **strace the worker** (needs `CAP_SYS_PTRACE`; the container has none, so use
   an ephemeral debug container):
   ```bash
   kubectl -n zynotalk-cluster debug <pod> --image=nicolaka/netshoot \
     --target=rtpbridge --profile=sysadmin -c dbg --attach=false -- sleep 3600
   # then, while a probe drives STUN at the endpoint socket:
   kubectl -n zynotalk-cluster exec <pod> -c dbg -- \
     timeout 15 strace -f -p 1 -e trace=%network,epoll_ctl,epoll_wait,recvfrom,sendto -yy -tt
   ```
   Add `epoll_ctl` (vs the original capture): if the endpoint UDP fd is never
   `EPOLL_CTL_ADD`-ed, the socket was never registered ⇒ the task never ran. If
   it *is* registered but never `recvfrom`'d, the readiness/waker path is the
   problem.
4. **Thread state** (`/proc/1/task/*/{stat,wchan,syscall,schedstat}`) — confirm
   the worker is idle in `ep_poll` and not spinning/futex-blocked (rules
   deadlock/spin in or out vs a healthy pod).
5. After capture, **restart** (§6).

When this trigger is finally pinned, update §3 and link the fix.

---

## 6. Mitigation

- **Restart the wedged pod:** `kubectl -n zynotalk-cluster delete pod <pod>`. It's
  in-process state — same-node restart is fine (coturn on the same node/IP works,
  so it's not the node). Confirm recovery with the probe on the fresh pod.
- With the supervision fix deployed, the two recv-task failure variants are
  **observable**: alert on `rtpbridge_webrtc_recv_task_start_timeout_total > 0`
  or `rtpbridge_webrtc_recv_task_dead_total > 0` and auto-drain/restart the pod
  (or have the bridge stop routing to it).
- **Recommended follow-ups** (not yet implemented):
  - Drive the readiness probe unhealthy when `webrtc_recv_task_start_timeout` or
    `webrtc_recv_task_dead` fires so k8s drains/restarts the pod automatically
    (the real auto-recovery).
  - Attempt a bounded recv-task restart in the liveness sweep before giving up.
  - Class-aware backpressure: demux inbound by RFC 5764 packet class (STUN /
    DTLS / SRTP) so STUN/DTLS are not dropped under RTP overload, while keeping
    RTP/SRTP lossy (see the `try_send` note in §4).

---

## 7. Correlation keys & references

- Client `callId` == SIP `Call-ID`. Bridge log `rtpbridge session created` joins
  `callId` ⇄ rtpbridge `session_id`. rtpbridge endpoint logs carry
  `endpoint_id` + `local_addr` (the media socket).
- Cross-stack methodology: `zynotalk-mobile/docs/BACKEND_INCIDENT_RESEARCH.md`.
- Code: `src/session/endpoint_webrtc.rs` (`start_recv_task`,
  `await_recv_task_started`, `supervise_recv`), `src/session/media_session.rs`
  (`run_media_session` 1 Hz sweep, `create_offer`/`from_offer` call sites),
  `src/metrics.rs`. Repro tool: `examples/media_probe.rs`.
