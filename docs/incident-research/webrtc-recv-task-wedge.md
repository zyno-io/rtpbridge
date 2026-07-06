# WebRTC receive-task wedge — incident research / runbook

**One-line:** On an affected pod, *every* new WebRTC endpoint silently fails to
establish media — signaling succeeds, the endpoint's UDP socket is bound, but
**nothing ever reads it**, so ICE never completes, str0m never replies, and the
call dies with `endpoint media timeout`. Control plane, `/health`, `/metrics`
stay perfectly healthy. A pod restart fixes it; it recurs.

This is the failure the commit *"Add telemetry for WebRTC accept_answer wedge
investigation"* (`11e41e8`) was chasing. First fully diagnosed 2026-06-02.
Root-cause theories re-ranked 2026-06-09 from retained logs/metrics — see §3;
the datapath branch is now the front-runner, and the §4 supervision counters
are expected to stay silent during a recurrence (§4 expectation note).

## Current status (2026-07-06)

Status: **not root-cause closed, but no recurrence observed in recent production
Grafana data**.

Production Mimir/Loki review on 2026-07-06:

- `2026-06-22T21:33Z` → `2026-07-06T21:33Z`: no
  `rtpbridge_webrtc_recv_task_start_timeout_total`,
  `rtpbridge_webrtc_recv_task_dead_total`, or
  `rtpbridge_webrtc_recv_overflow_total` increases on `rtpbridge-0/1/2`.
  `rtpbridge_webrtc_recv_task_started_total` continued climbing normally on all
  pods (~30k per pod), and packet routing remained healthy.
- Same window had one `WebRTC negotiation stuck past watchdog threshold` log on
  `rtpbridge-0` at `2026-07-03T14:10:20Z`. Zoom-in showed a single-session
  failure, not this pod-wide wedge: the WebRTC endpoint reached `Connected`
  twice, that session routed packets around 100 pps, the pod returned to zero
  active sessions, and the same pod created new WebRTC endpoints and routed
  ~40k packets in the following hour.
- `2026-06-08T21:33Z` → `2026-06-22T21:33Z`: no recv-task supervision counter
  increases, no recv overflow, no `webrtc_connecting_stuck` increments, and no
  matching watchdog or recv-task liveness logs. Media timeouts existed but were
  distributed across pods and did not match the pod-wide blackhole signature.
- Exact 14 days before the July 3 warning
  (`2026-06-19T14:10:20Z` → `2026-07-03T14:10:20Z`) also showed zero
  recv-task supervision counter increases, zero recv overflow, zero
  `webrtc_connecting_stuck` increases, and no matching Loki warning logs.

Interpretation: keep this as an active incident-research/runbook document. The
recent evidence supports "quiet since the supervision work," not "fixed." A true
closure still needs either a captured recurrence that pins the trigger or a
follow-up that gives the readiness probe a direct media-path/self-probe signal.

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
polled the socket" from "socket readiness/packet delivery never arrived" (note
the fd is registered at bind time regardless — see §5 step 4); §5 has the
captures that separate those cases.

Because creation only `tokio::spawn`s the task and returns the SDP **without
confirming the task ever started**, this was completely silent — the endpoint
looked created, but its media leg was dead on arrival.

### What is *not* the cause (ruled out)
- Not coturn (live STUN + Allocate both succeed on the same pod/IP). NOTE this
  only vouches for coturn's *own* ports — coturn shares the pod's netns, so it
  proves UDP reaches the pod on its well-known port, nothing about the arbitrary
  high ports rtpbridge endpoints bind. It does NOT clear the node datapath.
- Not a deadlock or spin (the worker sits idle in `epoll_wait`; `/proc` thread
  state identical to a healthy pod).
- Not a panic (none logged in the pod's entire life; the recv task also
  `catch_unwind`s and would log `recv task panicked`).
- Not str0m credential divergence (that would still *read* the socket then
  reject; here the socket is never read). The unrelated `pending_offer` /
  ICE-restart-generation work is a *different*, per-call bug — see the
  `ice_restart` guard + `offer_generation` — not this process-wide wedge.
- Not a tokio 1.52 regression: the investigation telemetry (`11e41e8`,
  2026-05-13) predates the 1.50.0 → 1.52.3 bump (2026-05-15), so the wedge
  existed on both versions.

### Still open: the exact trigger — re-ranked 2026-06-09
We proved the *symptom* (socket never read by the process) and the *defect*
(unsupervised fire-and-forget recv loop), but not the trigger. A 2026-06-09
re-analysis of the June 2 incident from retained Loki/Mimir data (plus an
adversarial second review) re-ranked the theories. Key evidence:

- **Idle onset.** The wedged pod (up since Jun 1 ~17:00Z) routed media normally
  until Jun 2 ~00:40Z, then sat idle. Synthetic prober cycles completed cleanly
  at 01:15, 02:24, 02:25 (7–13 s each); the 06:21 cycle took 30 s (prober
  timeout?); the 10:13:13 probe got its SDP but its session was never destroyed.
  Onset is bounded to **02:25–10:13Z (likely ≤06:21)** with ZERO log lines at
  the transition and zero traffic — overload/backpressure triggers are out.
- **The runtime kept scheduling new tasks and serving new TCP fds during the
  wedge.** At 10:13 and ~12:24, brand-new control-WS TCP connections (and the
  freshly spawned session tasks behind them) worked end to end. So the
  scheduler and reactor were not globally dead. Caveat (from the adversarial
  review): TCP and UDP share the driver but not identical wait states (per-fd
  `ScheduledIo`, edge-triggered readiness), so a UDP-side per-fd divergence is
  not fully excluded by this.
- **Registration is NOT the recv task's job.** Verified against tokio 1.52.3 /
  mio 1.2.0 sources: `UdpSocket::bind` → `PollEvented::new` registers the fd
  (`EPOLL_CTL_ADD`) at construction, on the *session* task, inside
  `create_offer`/`from_offer` — before the recv task is spawned. Theory 1's
  "never registers the fd" framing conflated two things.
- **Port spread.** Failed-endpoint ports (40833, 33081, 51836, RTP 23032)
  interleave with healthy-period ports (34108, 35726, 57228): if it's datapath,
  it is a *generalized* UDP delivery failure to the pod, not a static
  port-range DNAT steal.
- **"Restart fixes it" does not isolate it in-process**: the Jun 2 mitigation
  was a pod *delete*; pod churn is exactly what makes svclb/kube-proxy
  reprogram node NAT/conntrack state. There was also node-level churn inside
  the onset window (kube-state-metrics moved instances ~08:00Z), and the
  May 15 burst hit two pods simultaneously (caveat: contaminated by the
  watchdog false-positives fixed in `fd76dbd` the same day).
- The first Jun 2 casualty was a **plain RTP** endpoint, so the wedge is
  probably not WebRTC-specific — weak corroboration only, since RTP recv tasks
  still use blocking `send().await` and could park on a full channel instead.

Theories, re-ranked (was 1 > 2 > 3):

1. **Packets never reach the socket** (node/datapath: NAT, conntrack, svclb /
   kube-proxy reprogramming, kernel drop) — now MOST probable. Fits the strace
   exactly (idle `epoll_wait`, zero `recvfrom` = nothing to deliver), survives
   the coturn and restart counter-arguments per above.
2. **Recv task polls, but readiness/waker delivery is broken** for UDP fds —
   still viable; no known tokio/mio bug of this shape in 1.50–1.52, but the
   new-TCP evidence doesn't fully exclude a per-fd UDP divergence.
3. **Recv task never polled** (scheduler loses the spawn) — least likely:
   during the wedge every other freshly spawned task ran, and the idle worker
   cuts against starvation. Not formally dead (the June 2 incident predates the
   `recv_started` telemetry, so "the task was polled" is inference, not data).

The supervision metrics now discriminate 3 from 1/2 automatically on the next
recurrence: theory 3 ⇒ `start_timeout` fires; theories 1/2 ⇒ counters stay
silent while calls fail (see §4 note). The 1-vs-2 split needs §5's captures on
a still-wedged pod.

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

**Expectation (2026-06-09 re-analysis):** under §3's now-leading theories the
recv task starts fine and parks forever in `recv_from`, so on the next wedge
`start_timeout`/`dead` likely stay at ZERO while `recv_task_started` keeps
climbing and calls fail. Counters at zero since the Jun 3 deploy means "no
recurrence yet" (`connecting_stuck` is also zero), NOT "fixed". Don't let the
silent supervision counters talk you out of the diagnosis — confirm with the
§2 probe.

---

## 5. Diagnosing a recurrence — capture BEFORE you restart

The pod restart is the mitigation, but it erases the evidence. If you can spare
the wedged pod for ~5 min, grab these (they are what's still missing to close
§3's open trigger). All read-only.

1. **Pin the pod & a live endpoint, and note the NODE.** Run the probe (§2)
   against the bad pod; note the `rtpbridge media candidate = <ip>:<port>` it
   prints, and record `kubectl get pod -o wide` (node identity matters if the
   datapath theory is right — check whether wedges correlate with a node or
   with svclb/kube-proxy/pod churn on it in the onset window).
2. **THE decisive first split — Recv-Q + drops on that endpoint socket, while
   the probe sends continuously:**
   ```bash
   # In the rtpbridge container (or a debug sidecar sharing its netns),
   # sample a few times while the probe is actively sending STUN:
   #   rx_queue grows         => kernel has packets, app isn't reading => task/readiness branch (stop chasing datapath)
   #   rx_queue 0, drops 0    => packets never arrive                  => datapath branch
   #   rx_queue 0, drops grow => kernel is dropping at the socket      => buffer/filter branch
   grep <hex(port)> /proc/<pid>/net/udp   # rx_queue and drops columns
   ```
3. **Datapath branch:** packet-capture at each hop until the packets vanish —
   host NIC → (pod veth, if not hostNetwork) → inside the pod netns
   (`tcpdump -ni any udp port <port>` from the debug container) — plus
   `conntrack -L | grep <port>`, `iptables-save`/`nft list ruleset` diffed
   against a healthy node. Rules inspection alone can miss the drop point;
   the capture shows it.
4. **Task/readiness branch: strace the worker** (needs `CAP_SYS_PTRACE`; the
   container has none, so use an ephemeral debug container):
   ```bash
   kubectl -n zynotalk-cluster debug <pod> --image=nicolaka/netshoot \
     --target=rtpbridge --profile=sysadmin -c dbg --attach=false -- sleep 3600
   # then, while a probe drives STUN at the endpoint socket:
   kubectl -n zynotalk-cluster exec <pod> -c dbg -- \
     timeout 15 strace -f -p 1 -e trace=%network,epoll_ctl,epoll_wait,recvfrom,sendto -yy -tt
   ```
   Interpretation (corrected 2026-06-09): the fd is `EPOLL_CTL_ADD`-ed at
   `UdpSocket::bind` time on the *session* task, BEFORE the recv task spawns —
   verified against tokio 1.52.3/mio 1.2.0 sources. So the ADD being present
   says nothing about whether the recv task ran (the old "no ADD ⇒ task never
   ran" inference was wrong; an absent ADD would instead mean bind-time
   registration failed, which `create_offer` would have surfaced). What this
   capture CAN show: `epoll_wait` returning events for the endpoint fd
   (`-yy` decodes fds) with no subsequent `recvfrom` ⇒ readiness arrives but
   the waker/poll path is broken (§3 theory 2); no events for that fd at all
   while Recv-Q grows would be kernel-internal and warrants a tokio/mio bug
   report. Combine with the supervision counters: `start_timeout` fired ⇒
   never-polled variant (§3 theory 3).
5. **Thread state** (`/proc/1/task/*/{stat,wchan,syscall,schedstat}`) — confirm
   the worker is idle in `ep_poll` and not spinning/futex-blocked (rules
   deadlock/spin in or out vs a healthy pod).
6. After capture, **restart** (§6).

When this trigger is finally pinned, update §3 and link the fix.

---

## 6. Mitigation

- **Restart the wedged pod:** `kubectl -n zynotalk-cluster delete pod <pod>`.
  Confirm recovery with the probe on the fresh pod. (2026-06-09 caveat: "it's
  in-process, same node is fine" is no longer safe to assume — pod delete also
  reprograms svclb/kube-proxy/conntrack node state, which under §3's leading
  theory may be the actual fix. If the replacement lands on the same node and
  the probe still fails, that's itself decisive data: capture §5 step 3 there.)
- The supervision alerts (`rtpbridge_webrtc_recv_task_start_timeout_total > 0`,
  `rtpbridge_webrtc_recv_task_dead_total > 0`) catch the never-polled and
  died-young variants — but per §4's expectation note they likely stay SILENT
  for this wedge. Also alert on the symptom directly, e.g. endpoints created
  but zero `Connected` transitions / `rtpbridge_packets_routed_total` flat over
  10m while `rtpbridge_sessions_active > 0` on a pod.
- **Recommended follow-ups** (not yet implemented):
  - Drive the readiness probe unhealthy when `webrtc_recv_task_start_timeout` or
    `webrtc_recv_task_dead` fires so k8s drains/restarts the pod automatically
    (the real auto-recovery).
  - **In-process UDP self-probe** in the 1 Hz sweep: a scratch socket sends a
    datagram to a live endpoint's local port and the sweep verifies the recv
    task saw it (count before str0m/routing). Limits: loopback bypasses the
    external datapath, so self-probe OK + external probe failing ⇒ datapath;
    self-probe failing ⇒ genuinely in-process. Either way it converts the next
    recurrence into an instant branch verdict and gives the readiness probe a
    real signal.
  - **Give the bridge-side prober a timeout** — on Jun 2 it hung forever on the
    10:13 probe (session never destroyed, WS never closed), so the wedge showed
    up as silence instead of probe failures.
  - **Extend `try_send` + supervision to RTP/RTCP recv tasks**
    (`endpoint_rtp.rs` still uses blocking `send().await` — the original
    parking hazard the WebRTC path was cured of).
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
