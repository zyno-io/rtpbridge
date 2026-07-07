import type {
  CallRuntime,
  EndpointRuntime,
  EndpointStats,
  Failure,
  PeerCounters,
  RtpReceiveQuality,
  WebRtcCounters,
  WebRtcReceiveQuality,
} from "./types.js";
import { nowIso } from "./utils.js";

interface DirectionState {
  packets: number;
  flatlineSinceMs?: number;
}

interface QualityState {
  bridge?: EndpointStats;
  rtp?: RtpReceiveQuality;
  webRtc?: WebRtcReceiveQuality;
  peerReceivedPackets?: number;
  webRtcReceivedPackets?: number;
}

type QualityPath =
  | "peer_to_bridge"
  | "browser_turn_to_bridge"
  | "bridge_receive_path"
  | "bridge_to_peer"
  | "bridge_to_browser_turn"
  | "bridge_to_receiver";

interface QualityClassification {
  cause:
    | "upstream_peer_to_bridge_loss"
    | "upstream_browser_turn_to_bridge_loss"
    | "upstream_peer_to_bridge_jitter"
    | "upstream_browser_turn_to_bridge_jitter"
    | "bridge_rx_backpressure"
    | "bridge_rx_loop_gap"
    | "bridge_session_dequeue_delay"
    | "bridge_webrtc_ingress_processing_loss"
    | "bridge_added_egress_loss"
    | "downstream_peer_receive_loss"
    | "downstream_browser_turn_receive_loss"
    | "downstream_peer_timing_gap"
    | "downstream_browser_turn_jitter"
    | "downstream_browser_concealment";
  bridge_added: boolean | "unknown";
  path: QualityPath;
  evidence: string[];
}

const RTP_CLEAN_MAX_LOSS_PACKETS = 2;
const RTP_CLEAN_MAX_LOSS_RATIO = 0.002;
const RTP_CLEAN_MAX_REORDERED_PACKETS = 1;
const RTP_CLEAN_MAX_DUPLICATE_PACKETS = 1;
const RTP_CLEAN_MAX_JITTER_MS = 80;
const RTP_CLEAN_MAX_INTERARRIVAL_MS = 250;

const BRIDGE_RX_MAX_ENQUEUE_WAIT_MS = 20;
const BRIDGE_RX_MAX_DEQUEUE_DELAY_MS = 100;
const BRIDGE_RX_MAX_LOOP_GAP_MS = 200;

const WEBRTC_DIRECT_MAX_LOSS_PACKETS = 3;
const WEBRTC_DIRECT_MAX_LOSS_RATIO = 0.01;
const WEBRTC_RELAY_MAX_LOSS_RATIO = 0.03;
const WEBRTC_DIRECT_MAX_JITTER_MS = 120;
const WEBRTC_RELAY_MAX_JITTER_MS = 250;
const WEBRTC_DIRECT_MAX_CONCEALED_RATIO = 0.03;
const WEBRTC_RELAY_MAX_CONCEALED_RATIO = 0.05;
const WEBRTC_MAX_CONCEALED_SAMPLES = 4800;

export interface EndpointSample {
  endpointId: string;
  label: string;
  kind: EndpointRuntime["kind"];
  bridge?: EndpointStats;
  peer?: PeerCounters | WebRtcCounters;
}

export interface CallSample {
  callId: string;
  monoMs: number;
  inGrace: boolean;
  endpoints: EndpointSample[];
}

function cloneEndpointStats(stats: EndpointStats | undefined): EndpointStats | undefined {
  if (!stats) {
    return undefined;
  }
  return {
    ...stats,
    inbound: stats.inbound ? { ...stats.inbound } : undefined,
    outbound: stats.outbound ? { ...stats.outbound } : undefined,
  };
}

export class MediaMonitor {
  private readonly directions = new Map<string, DirectionState>();
  private readonly quality = new Map<string, QualityState>();
  private readonly qualitySuppressedUntilMs = new Map<string, number>();
  private readonly failures: Failure[] = [];
  private maxFlatlineMs = 0;

  constructor(private readonly sampleIntervalMs: number) {}

  async sampleCall(call: CallRuntime, monoMs: number): Promise<CallSample> {
    const inGrace = monoMs < call.graceUntilMs || monoMs < call.startedAtMs + 15_000;
    const endpointSamples: EndpointSample[] = [];

    for (const endpoint of call.endpoints) {
      const bridge = endpoint.control.statsFor(endpoint.id);
      const peer = await this.peerSnapshot(endpoint);
      endpointSamples.push({
        endpointId: endpoint.id,
        label: endpoint.label,
        kind: endpoint.kind,
        bridge,
        peer,
      });
    }

    const sampleByEndpoint = new Map(endpointSamples.map((sample) => [sample.endpointId, sample]));
    const previousQualityByEndpoint = new Map(
      call.endpoints.map((endpoint) => [endpoint.id, this.quality.get(this.qualityKey(call, endpoint))]),
    );
    const callImpaired = endpointSamples.some((sample) => Boolean(sample.peer?.impairment?.active));
    if (callImpaired) {
      this.qualitySuppressedUntilMs.set(call.plan.id, monoMs + this.sampleIntervalMs * 2);
    }
    const suppressQuality =
      inGrace || callImpaired || monoMs < (this.qualitySuppressedUntilMs.get(call.plan.id) ?? 0);

    for (const endpoint of call.endpoints) {
      const sample = sampleByEndpoint.get(endpoint.id);
      const bridge = sample?.bridge;
      const peer = sample?.peer;

      if (endpoint.kind === "file" || !peer || !bridge) {
        continue;
      }

      this.checkDirection({
        call,
        endpoint,
        direction: "send",
        packets: Math.min(
          peer.sentPackets,
          bridge.inbound?.packets ?? 0,
        ),
        monoMs,
        inGrace,
      });
      this.checkDirection({
        call,
        endpoint,
        direction: "recv",
        packets: Math.min(
          peer.receivedPackets,
          bridge.outbound?.packets ?? 0,
        ),
        monoMs,
        inGrace,
      });

      if (endpoint.kind === "webrtc") {
        const webRtc = peer as WebRtcCounters;
        if (
          !inGrace &&
          (webRtc.iceConnectionState === "failed" ||
            webRtc.connectionState === "failed" ||
            webRtc.connectionState === "closed")
        ) {
          this.fail(call.plan.id, `${endpoint.label} WebRTC connection failed`, webRtc);
        }

        if (
          endpoint.webRtcPeer?.profile === "relay" &&
          webRtc.selectedLocalCandidateType &&
          webRtc.selectedLocalCandidateType !== "relay"
        ) {
          this.fail(call.plan.id, `${endpoint.label} expected relay candidate`, webRtc);
        }
      }

      if (suppressQuality) {
        continue;
      } else {
        this.checkQuality({
          call,
          endpoint,
          bridge,
          peer,
          endpointSamples,
          previousQualityByEndpoint,
          previousState: previousQualityByEndpoint.get(endpoint.id),
        });
      }
    }

    for (const endpoint of call.endpoints) {
      const sample = sampleByEndpoint.get(endpoint.id);
      const bridge = sample?.bridge;
      const peer = sample?.peer;
      if (endpoint.kind === "file" || !peer || !bridge) {
        continue;
      }
      this.rememberQuality(call, endpoint, peer, bridge);
    }

    return {
      callId: call.plan.id,
      monoMs,
      inGrace,
      endpoints: endpointSamples,
    };
  }

  allFailures(): Failure[] {
    return [...this.failures];
  }

  summary(): { failures: Failure[]; maxFlatlineMs: number } {
    return {
      failures: this.allFailures(),
      maxFlatlineMs: this.maxFlatlineMs,
    };
  }

  private async peerSnapshot(endpoint: EndpointRuntime): Promise<PeerCounters | WebRtcCounters | undefined> {
    if (endpoint.rtpPeer) {
      return endpoint.rtpPeer.snapshot();
    }
    if (endpoint.webRtcPeer) {
      return endpoint.webRtcPeer.snapshot().catch((error) => {
        const message = error instanceof Error ? error.message : String(error);
        if (
          message.includes("Target page, context or browser has been closed") ||
          message.includes("Target closed") ||
          message.includes("unknown WebRTC peer")
        ) {
          return undefined;
        }
        throw error;
      });
    }
    return undefined;
  }

  private checkDirection(params: {
    call: CallRuntime;
    endpoint: EndpointRuntime;
    direction: "send" | "recv";
    packets: number;
    monoMs: number;
    inGrace: boolean;
  }): void {
    const key = `${params.call.plan.id}:${params.endpoint.id}:${params.direction}`;
    const previous = this.directions.get(key);
    if (!previous) {
      this.directions.set(key, { packets: params.packets });
      return;
    }

    if (params.packets > previous.packets) {
      previous.packets = params.packets;
      previous.flatlineSinceMs = undefined;
      return;
    }

    if (params.inGrace) {
      return;
    }

    if (!previous.flatlineSinceMs) {
      previous.flatlineSinceMs = params.monoMs;
      return;
    }

    const flatlineMs = params.monoMs - previous.flatlineSinceMs;
    this.maxFlatlineMs = Math.max(this.maxFlatlineMs, flatlineMs);
    if (flatlineMs >= this.sampleIntervalMs * 2.5) {
      this.fail(
        params.call.plan.id,
        `${params.endpoint.label} ${params.direction} packets flatlined for ${flatlineMs}ms`,
        {
          endpointId: params.endpoint.id,
          direction: params.direction,
          packets: params.packets,
          state: params.endpoint.control.statsFor(params.endpoint.id),
        },
      );
      previous.flatlineSinceMs = params.monoMs;
    }
  }

  private checkQuality(params: {
    call: CallRuntime;
    endpoint: EndpointRuntime;
    bridge: EndpointStats;
    peer: PeerCounters | WebRtcCounters;
    endpointSamples: EndpointSample[];
    previousQualityByEndpoint: Map<string, QualityState | undefined>;
    previousState?: QualityState;
  }): void {
    const previous = params.previousState;
    if (!previous) {
      return;
    }

    if (previous.bridge) {
      this.checkBridgeQuality(params.call, params.endpoint, params.bridge, previous.bridge);
    }

    if (params.peer.rtpQuality && previous.rtp) {
      this.checkRtpQuality(
        params.call,
        params.endpoint,
        params.bridge,
        params.peer,
        params.peer.rtpQuality,
        previous,
        params.endpointSamples,
        params.previousQualityByEndpoint,
      );
    }

    const webRtc = params.peer as WebRtcCounters;
    if (webRtc.webRtcQuality && previous.webRtc) {
      this.checkWebRtcQuality(
        params.call,
        params.endpoint,
        params.bridge,
        webRtc,
        webRtc.webRtcQuality,
        previous,
        params.endpointSamples,
        params.previousQualityByEndpoint,
      );
    }
  }

  private checkBridgeQuality(
    call: CallRuntime,
    endpoint: EndpointRuntime,
    current: EndpointStats,
    previous: EndpointStats,
  ): void {
    const packetDelta = Math.max(0, (current.inbound?.packets ?? 0) - (previous.inbound?.packets ?? 0));
    const lostDelta = Math.max(0, (current.inbound?.packets_lost ?? 0) - (previous.inbound?.packets_lost ?? 0));
    const allowedLost = Math.max(RTP_CLEAN_MAX_LOSS_PACKETS, Math.ceil(packetDelta * RTP_CLEAN_MAX_LOSS_RATIO));
    if (lostDelta > allowedLost) {
      this.fail(call.plan.id, `${endpoint.label} bridge inbound sequence loss exceeded clean threshold`, {
        endpointId: endpoint.id,
        packetDelta,
        lostDelta,
        allowedLost,
        classification: classifyBridgeIngress(endpoint, "loss", current, previous),
        bridgeReceive: bridgeReceiveDiagnostics(current, previous),
        current,
        previous,
      });
    }

    const jitterMs = current.inbound?.jitter_ms ?? 0;
    const previousJitterMs = previous.inbound?.jitter_ms ?? 0;
    if (jitterMs > RTP_CLEAN_MAX_JITTER_MS && jitterMs > previousJitterMs) {
      this.fail(call.plan.id, `${endpoint.label} bridge inbound jitter exceeded clean threshold`, {
        endpointId: endpoint.id,
        jitterMs,
        previousJitterMs,
        allowedJitterMs: RTP_CLEAN_MAX_JITTER_MS,
        classification: classifyBridgeIngress(endpoint, "jitter", current, previous),
        bridgeReceive: bridgeReceiveDiagnostics(current, previous),
        current,
      });
    }
  }

  private checkRtpQuality(
    call: CallRuntime,
    endpoint: EndpointRuntime,
    bridge: EndpointStats,
    peer: PeerCounters | WebRtcCounters,
    current: RtpReceiveQuality,
    previousState: QualityState,
    endpointSamples: EndpointSample[],
    previousQualityByEndpoint: Map<string, QualityState | undefined>,
  ): void {
    const previous = previousState.rtp;
    if (!previous) {
      return;
    }
    const expectedDelta = Math.max(0, current.expectedPackets - previous.expectedPackets);
    const lostDelta = Math.max(0, current.lostPackets - previous.lostPackets);
    const allowedLost = Math.max(RTP_CLEAN_MAX_LOSS_PACKETS, Math.ceil(expectedDelta * RTP_CLEAN_MAX_LOSS_RATIO));
    if (lostDelta > allowedLost) {
      this.fail(call.plan.id, `${endpoint.label} RTP receive sequence loss exceeded clean threshold`, {
        endpointId: endpoint.id,
        expectedDelta,
        lostDelta,
        allowedLost,
        classification: classifyReceiveLoss({
          endpoint,
          currentBridge: bridge,
          previousState,
          endpointSamples,
          previousQualityByEndpoint,
          expectedDelta,
          receivedDelta: counterDelta(peer.receivedPackets, previousState.peerReceivedPackets),
          lostDelta,
          receiver: "rtp",
        }),
        current,
        previous,
      });
    }

    const outOfOrderDelta = Math.max(0, current.outOfOrderPackets - previous.outOfOrderPackets);
    if (outOfOrderDelta > RTP_CLEAN_MAX_REORDERED_PACKETS) {
      this.fail(call.plan.id, `${endpoint.label} RTP receive reordering exceeded clean threshold`, {
        endpointId: endpoint.id,
        outOfOrderDelta,
        allowedOutOfOrder: RTP_CLEAN_MAX_REORDERED_PACKETS,
        classification: downstreamClassification({
          endpoint,
          currentBridge: bridge,
          previousState,
          receiver: "rtp",
          cause: "downstream_peer_receive_loss",
          evidence: [
            "RTP peer observed reordered sequence numbers after packets left the bridge",
            "rtpbridge inbound counters do not identify this as pre-bridge ingress loss",
          ],
        }),
        current,
        previous,
      });
    }

    const duplicateDelta = Math.max(0, current.duplicatePackets - previous.duplicatePackets);
    if (duplicateDelta > RTP_CLEAN_MAX_DUPLICATE_PACKETS) {
      this.fail(call.plan.id, `${endpoint.label} RTP receive duplicates exceeded clean threshold`, {
        endpointId: endpoint.id,
        duplicateDelta,
        allowedDuplicates: RTP_CLEAN_MAX_DUPLICATE_PACKETS,
        classification: downstreamClassification({
          endpoint,
          currentBridge: bridge,
          previousState,
          receiver: "rtp",
          cause: "downstream_peer_receive_loss",
          evidence: [
            "RTP peer observed duplicate sequence numbers after packets left the bridge",
            "duplicate packets are not attributable to bridge inbound loss",
          ],
        }),
        current,
        previous,
      });
    }

    if (current.jitterMs > RTP_CLEAN_MAX_JITTER_MS && current.jitterMs > previous.jitterMs) {
      this.fail(call.plan.id, `${endpoint.label} RTP receive jitter exceeded clean threshold`, {
        endpointId: endpoint.id,
        jitterMs: current.jitterMs,
        previousJitterMs: previous.jitterMs,
        allowedJitterMs: RTP_CLEAN_MAX_JITTER_MS,
        classification: downstreamClassification({
          endpoint,
          currentBridge: bridge,
          previousState,
          receiver: "rtp",
          cause: "downstream_peer_timing_gap",
          evidence: [
            "RTP peer inter-arrival jitter increased after bridge outbound delivery",
            "current bridge stats do not expose per-packet egress timestamps",
          ],
        }),
        current,
      });
    }

    if (
      current.maxInterarrivalMs > previous.maxInterarrivalMs &&
      current.maxInterarrivalMs > RTP_CLEAN_MAX_INTERARRIVAL_MS
    ) {
      this.fail(call.plan.id, `${endpoint.label} RTP receive inter-arrival gap exceeded clean threshold`, {
        endpointId: endpoint.id,
        maxInterarrivalMs: current.maxInterarrivalMs,
        previousMaxInterarrivalMs: previous.maxInterarrivalMs,
        allowedInterarrivalMs: RTP_CLEAN_MAX_INTERARRIVAL_MS,
        classification: downstreamClassification({
          endpoint,
          currentBridge: bridge,
          previousState,
          receiver: "rtp",
          cause: "downstream_peer_timing_gap",
          evidence: [
            "RTP peer measured a receive-time gap after bridge outbound delivery",
            "current bridge stats do not expose per-packet egress timestamps, so bridge scheduling versus OS/Node receive timing is not separable from this metric alone",
          ],
        }),
        current,
      });
    }
  }

  private checkWebRtcQuality(
    call: CallRuntime,
    endpoint: EndpointRuntime,
    bridge: EndpointStats,
    peer: WebRtcCounters,
    current: WebRtcReceiveQuality,
    previousState: QualityState,
    endpointSamples: EndpointSample[],
    previousQualityByEndpoint: Map<string, QualityState | undefined>,
  ): void {
    const previous = previousState.webRtc;
    if (!previous) {
      return;
    }

    const isRelay = endpoint.webRtcPeer?.profile === "relay";
    const previousReceivedPackets = previousState.webRtcReceivedPackets ?? peer.receivedPackets;
    const receivedDelta = Math.max(0, peer.receivedPackets - previousReceivedPackets);
    const lostDelta = Math.max(0, current.inboundPacketsLost - previous.inboundPacketsLost);
    const lossRatioLimit = isRelay ? WEBRTC_RELAY_MAX_LOSS_RATIO : WEBRTC_DIRECT_MAX_LOSS_RATIO;
    const allowedLost = Math.max(WEBRTC_DIRECT_MAX_LOSS_PACKETS, Math.ceil(receivedDelta * lossRatioLimit));
    if (lostDelta > allowedLost) {
      this.fail(call.plan.id, `${endpoint.label} WebRTC receive loss exceeded clean threshold`, {
        endpointId: endpoint.id,
        receivedDelta,
        lostDelta,
        allowedLost,
        relay: isRelay,
        classification: classifyReceiveLoss({
          endpoint,
          currentBridge: bridge,
          previousState,
          endpointSamples,
          previousQualityByEndpoint,
          expectedDelta: receivedDelta + lostDelta,
          receivedDelta,
          lostDelta,
          receiver: "webrtc",
        }),
        current,
        previous,
      });
    }

    const jitterLimitMs = isRelay ? WEBRTC_RELAY_MAX_JITTER_MS : WEBRTC_DIRECT_MAX_JITTER_MS;
    if (current.inboundJitterMs > jitterLimitMs && current.inboundJitterMs > previous.inboundJitterMs) {
      this.fail(call.plan.id, `${endpoint.label} WebRTC receive jitter exceeded clean threshold`, {
        endpointId: endpoint.id,
        jitterMs: current.inboundJitterMs,
        previousJitterMs: previous.inboundJitterMs,
        allowedJitterMs: jitterLimitMs,
        relay: isRelay,
        classification: downstreamClassification({
          endpoint,
          currentBridge: bridge,
          previousState,
          receiver: "webrtc",
          cause: "downstream_browser_turn_jitter",
          evidence: [
            "browser WebRTC inbound jitter increased after bridge outbound delivery",
            "relay profiles may include TURN scheduling/network delay on the bridge-to-browser path",
          ],
        }),
        current,
      });
    }

    const concealedDelta = Math.max(0, current.concealedSamples - previous.concealedSamples);
    const sampleDelta = Math.max(0, current.totalSamplesReceived - previous.totalSamplesReceived);
    const concealedRatioLimit = isRelay ? WEBRTC_RELAY_MAX_CONCEALED_RATIO : WEBRTC_DIRECT_MAX_CONCEALED_RATIO;
    const allowedConcealed = Math.max(WEBRTC_MAX_CONCEALED_SAMPLES, Math.ceil(sampleDelta * concealedRatioLimit));
    if (concealedDelta > allowedConcealed) {
      this.fail(call.plan.id, `${endpoint.label} WebRTC concealed audio exceeded clean threshold`, {
        endpointId: endpoint.id,
        sampleDelta,
        concealedDelta,
        allowedConcealed,
        relay: isRelay,
        classification: downstreamClassification({
          endpoint,
          currentBridge: bridge,
          previousState,
          receiver: "webrtc",
          cause: "downstream_browser_concealment",
          evidence: [
            "browser reported concealed audio samples after receiving media from the bridge",
            "concealment can be caused by downstream loss/jitter or a bridge egress timing problem; current bridge stats do not expose per-packet egress timestamps",
          ],
        }),
        current,
        previous,
      });
    }
  }

  private rememberQuality(
    call: CallRuntime,
    endpoint: EndpointRuntime,
    peer: PeerCounters | WebRtcCounters,
    bridge?: EndpointStats,
  ): void {
    const webRtc = peer as WebRtcCounters;
    this.quality.set(this.qualityKey(call, endpoint), {
      bridge: cloneEndpointStats(bridge),
      rtp: peer.rtpQuality ? { ...peer.rtpQuality } : undefined,
      webRtc: webRtc.webRtcQuality ? { ...webRtc.webRtcQuality } : undefined,
      peerReceivedPackets: peer.receivedPackets,
      webRtcReceivedPackets: webRtc.webRtcQuality ? peer.receivedPackets : undefined,
    });
  }

  private qualityKey(call: CallRuntime, endpoint: EndpointRuntime): string {
    return `${call.plan.id}:${endpoint.id}`;
  }

  private fail(callId: string | undefined, reason: string, detail?: unknown): void {
    const duplicate = this.failures.some(
      (failure) => failure.callId === callId && failure.reason === reason,
    );
    if (duplicate) {
      return;
    }
    this.failures.push({
      ts: nowIso(),
      callId,
      reason,
      detail,
    });
  }
}

function classifyBridgeIngress(
  endpoint: EndpointRuntime,
  signal: "loss" | "jitter",
  current: EndpointStats,
  previous: EndpointStats,
): QualityClassification {
  const receive = bridgeReceiveDiagnostics(current, previous);
  if (receive.channelOverflowsDelta > 0) {
    return {
      cause: "bridge_rx_backpressure",
      bridge_added: true,
      path: "bridge_receive_path",
      evidence: [
        `endpoint recv task dropped ${receive.channelOverflowsDelta} packet(s) because the session packet channel was full`,
        "the packet reached the bridge socket path, but bridge receive backpressure prevented normal session processing",
      ],
    };
  }

  if (
    receive.maxEnqueueWaitMs > BRIDGE_RX_MAX_ENQUEUE_WAIT_MS &&
    receive.maxEnqueueWaitMs > receive.previousMaxEnqueueWaitMs
  ) {
    return {
      cause: "bridge_rx_backpressure",
      bridge_added: true,
      path: "bridge_receive_path",
      evidence: [
        `endpoint recv task waited ${receive.maxEnqueueWaitMs}ms to enqueue into the session packet channel`,
        `clean threshold is ${BRIDGE_RX_MAX_ENQUEUE_WAIT_MS}ms`,
      ],
    };
  }

  if (
    receive.maxDequeueDelayMs > BRIDGE_RX_MAX_DEQUEUE_DELAY_MS &&
    receive.maxDequeueDelayMs > receive.previousMaxDequeueDelayMs
  ) {
    return {
      cause: "bridge_session_dequeue_delay",
      bridge_added: true,
      path: "bridge_receive_path",
      evidence: [
        `packet spent ${receive.maxDequeueDelayMs}ms in the session packet channel before processing`,
        `clean threshold is ${BRIDGE_RX_MAX_DEQUEUE_DELAY_MS}ms`,
      ],
    };
  }

  if (
    receive.maxRecvLoopGapMs > BRIDGE_RX_MAX_LOOP_GAP_MS &&
    receive.maxRecvLoopGapMs > receive.previousMaxRecvLoopGapMs
  ) {
    return {
      cause: "bridge_rx_loop_gap",
      bridge_added: "unknown",
      path: "bridge_receive_path",
      evidence: [
        `endpoint recv task observed a ${receive.maxRecvLoopGapMs}ms gap between socket reads`,
        "this proves bridge-side receive-loop starvation or host scheduling delay, but packet capture is needed to separate bridge runtime from kernel/host scheduling",
      ],
    };
  }

  const webRtc = endpoint.kind === "webrtc";
  if (webRtc && signal === "loss" && hasRawRtpCounters(current, previous)) {
    const postStr0mLostDelta = counterDelta(current.inbound?.packets_lost, previous.inbound?.packets_lost);
    if (receive.rawRtpLostDelta > 0 || receive.rawRtpGapDelta > 0) {
      return {
        cause: "upstream_browser_turn_to_bridge_loss",
        bridge_added: false,
        path: "browser_turn_to_bridge",
        evidence: [
          `raw WebRTC RTP sequence tracking saw ${receive.rawRtpLostDelta} missing packet(s) across ${receive.rawRtpGapDelta} gap(s) before str0m`,
          "the SRTP/RTP header gap was observed at bridge socket ingress, so the loss is before bridge media processing",
        ],
      };
    }

    if (postStr0mLostDelta > 0 && receive.rawRtpPacketDelta > 0) {
      return {
        cause: "bridge_webrtc_ingress_processing_loss",
        bridge_added: receive.rawRtpOutOfOrderDelta > 0 ? "unknown" : true,
        path: "bridge_receive_path",
        evidence: [
          `post-str0m RTP sequence loss increased by ${postStr0mLostDelta} packet(s) while raw WebRTC RTP ingress showed no sequence loss`,
          `raw WebRTC RTP packet delta was ${receive.rawRtpPacketDelta}; raw out-of-order delta was ${receive.rawRtpOutOfOrderDelta}`,
          "the packets reached the bridge socket path, but did not emerge as the same continuous RTP event stream",
        ],
      };
    }
  }

  return {
    cause: webRtc
      ? signal === "loss"
        ? "upstream_browser_turn_to_bridge_loss"
        : "upstream_browser_turn_to_bridge_jitter"
      : signal === "loss"
        ? "upstream_peer_to_bridge_loss"
        : "upstream_peer_to_bridge_jitter",
    bridge_added: false,
    path: webRtc ? "browser_turn_to_bridge" : "peer_to_bridge",
    evidence: [
      "rtpbridge observed the degradation on endpoint inbound RTP before routing or mixing",
      webRtc
        ? "for WebRTC relay calls, this path includes browser and TURN before the bridge"
        : "for RTP endpoints, this path is the external RTP peer before the bridge",
    ],
  };
}

function bridgeReceiveDiagnostics(current: EndpointStats, previous: EndpointStats): {
  recvLoopGapMs: number;
  maxRecvLoopGapMs: number;
  previousMaxRecvLoopGapMs: number;
  enqueueWaitMs: number;
  maxEnqueueWaitMs: number;
  previousMaxEnqueueWaitMs: number;
  dequeueDelayMs: number;
  maxDequeueDelayMs: number;
  previousMaxDequeueDelayMs: number;
  channelCapacity?: number;
  minChannelCapacity?: number;
  channelOverflowsDelta: number;
  rawRtpPackets: number;
  rawRtpPacketDelta: number;
  rawRtpLostDelta: number;
  rawRtpGapDelta: number;
  rawRtpMaxSequenceGap: number;
  previousRawRtpMaxSequenceGap: number;
  rawRtpDuplicateDelta: number;
  rawRtpOutOfOrderDelta: number;
  rawRtpSequenceResetDelta: number;
  rawRtpLastSequence?: number;
  rawRtpLastSsrc?: number;
} {
  return {
    recvLoopGapMs: current.inbound?.recv_loop_gap_ms ?? 0,
    maxRecvLoopGapMs: current.inbound?.max_recv_loop_gap_ms ?? 0,
    previousMaxRecvLoopGapMs: previous.inbound?.max_recv_loop_gap_ms ?? 0,
    enqueueWaitMs: current.inbound?.enqueue_wait_ms ?? 0,
    maxEnqueueWaitMs: current.inbound?.max_enqueue_wait_ms ?? 0,
    previousMaxEnqueueWaitMs: previous.inbound?.max_enqueue_wait_ms ?? 0,
    dequeueDelayMs: current.inbound?.dequeue_delay_ms ?? 0,
    maxDequeueDelayMs: current.inbound?.max_dequeue_delay_ms ?? 0,
    previousMaxDequeueDelayMs: previous.inbound?.max_dequeue_delay_ms ?? 0,
    channelCapacity: current.inbound?.channel_capacity,
    minChannelCapacity: current.inbound?.min_channel_capacity,
    channelOverflowsDelta: counterDelta(current.inbound?.channel_overflows, previous.inbound?.channel_overflows),
    rawRtpPackets: current.inbound?.raw_rtp_packets ?? 0,
    rawRtpPacketDelta: counterDelta(current.inbound?.raw_rtp_packets, previous.inbound?.raw_rtp_packets),
    rawRtpLostDelta: counterDelta(current.inbound?.raw_rtp_packets_lost, previous.inbound?.raw_rtp_packets_lost),
    rawRtpGapDelta: counterDelta(current.inbound?.raw_rtp_sequence_gaps, previous.inbound?.raw_rtp_sequence_gaps),
    rawRtpMaxSequenceGap: current.inbound?.raw_rtp_max_sequence_gap ?? 0,
    previousRawRtpMaxSequenceGap: previous.inbound?.raw_rtp_max_sequence_gap ?? 0,
    rawRtpDuplicateDelta: counterDelta(
      current.inbound?.raw_rtp_duplicate_packets,
      previous.inbound?.raw_rtp_duplicate_packets,
    ),
    rawRtpOutOfOrderDelta: counterDelta(
      current.inbound?.raw_rtp_out_of_order_packets,
      previous.inbound?.raw_rtp_out_of_order_packets,
    ),
    rawRtpSequenceResetDelta: counterDelta(
      current.inbound?.raw_rtp_sequence_resets,
      previous.inbound?.raw_rtp_sequence_resets,
    ),
    rawRtpLastSequence: current.inbound?.raw_rtp_last_sequence,
    rawRtpLastSsrc: current.inbound?.raw_rtp_last_ssrc,
  };
}

function hasRawRtpCounters(current: EndpointStats, previous: EndpointStats): boolean {
  return current.inbound?.raw_rtp_packets !== undefined && previous.inbound?.raw_rtp_packets !== undefined;
}

function classifyReceiveLoss(params: {
  endpoint: EndpointRuntime;
  currentBridge: EndpointStats;
  previousState: QualityState;
  endpointSamples: EndpointSample[];
  previousQualityByEndpoint: Map<string, QualityState | undefined>;
  expectedDelta: number;
  receivedDelta: number;
  lostDelta: number;
  receiver: "rtp" | "webrtc";
}): QualityClassification {
  const sourceIngress = sourceIngressDeltas(
    params.endpoint,
    params.endpointSamples,
    params.previousQualityByEndpoint,
  );
  if (sourceIngress.lostDelta > 0) {
    const webRtcSource = sourceIngress.sources.some((source) => source.kind === "webrtc");
    return {
      cause: webRtcSource ? "upstream_browser_turn_to_bridge_loss" : "upstream_peer_to_bridge_loss",
      bridge_added: false,
      path: webRtcSource ? "browser_turn_to_bridge" : "peer_to_bridge",
      evidence: [
        `source bridge inbound loss increased by ${sourceIngress.lostDelta} packet(s) in the same sample window`,
        `affected source labels: ${sourceIngress.sources.map((source) => source.label).join(", ")}`,
        "receive-side sequence loss is consistent with pre-bridge loss propagated through the bridge",
      ],
    };
  }

  if (sourceIngress.bridgeProcessingLostDelta > 0) {
    return {
      cause: "bridge_webrtc_ingress_processing_loss",
      bridge_added: true,
      path: "bridge_receive_path",
      evidence: [
        `source post-str0m RTP loss increased by ${sourceIngress.bridgeProcessingLostDelta} packet(s) while raw WebRTC RTP ingress stayed continuous`,
        `affected source labels: ${sourceIngress.bridgeProcessingSources.map((source) => source.label).join(", ")}`,
        "receive-side degradation is consistent with bridge WebRTC ingress processing before routing",
      ],
    };
  }

  const outboundDelta = bridgeOutboundDelta(params.currentBridge, params.previousState);
  const outboundDeficit = Math.max(0, params.expectedDelta - outboundDelta);
  const samplingSkew = Math.max(5, Math.ceil(params.expectedDelta * 0.05));
  if (outboundDeficit > samplingSkew) {
    return {
      cause: "bridge_added_egress_loss",
      bridge_added: true,
      path: params.receiver === "webrtc" ? "bridge_to_browser_turn" : "bridge_to_peer",
      evidence: [
        `receiver expected ${params.expectedDelta} packet(s) but bridge outbound advanced by ${outboundDelta}`,
        `outbound deficit ${outboundDeficit} exceeds sampling-skew allowance ${samplingSkew}`,
        "source bridge inbound loss did not increase in the same sample window",
      ],
    };
  }

  return {
    cause: params.receiver === "webrtc" ? "downstream_browser_turn_receive_loss" : "downstream_peer_receive_loss",
    bridge_added: false,
    path: params.receiver === "webrtc" ? "bridge_to_browser_turn" : "bridge_to_peer",
    evidence: [
      `bridge outbound advanced by ${outboundDelta} packet(s) for ${params.receivedDelta} received and ${params.lostDelta} lost packet(s) at the receiver`,
      "source bridge inbound loss did not increase in the same sample window",
      params.receiver === "webrtc"
        ? "loss was reported by browser WebRTC stats after bridge outbound delivery"
        : "loss was reported by the RTP peer after bridge outbound delivery",
    ],
  };
}

function downstreamClassification(params: {
  endpoint: EndpointRuntime;
  currentBridge: EndpointStats;
  previousState: QualityState;
  receiver: "rtp" | "webrtc";
  cause:
    | "downstream_peer_receive_loss"
    | "downstream_peer_timing_gap"
    | "downstream_browser_turn_jitter"
    | "downstream_browser_concealment";
  evidence: string[];
}): QualityClassification {
  const outboundDelta = bridgeOutboundDelta(params.currentBridge, params.previousState);
  return {
    cause: params.cause,
    bridge_added: "unknown",
    path: params.receiver === "webrtc" ? "bridge_to_browser_turn" : "bridge_to_peer",
    evidence: [...params.evidence, `bridge outbound packet delta during sample: ${outboundDelta}`],
  };
}

function sourceIngressDeltas(
  destination: EndpointRuntime,
  endpointSamples: EndpointSample[],
  previousQualityByEndpoint: Map<string, QualityState | undefined>,
): {
  packetDelta: number;
  lostDelta: number;
  sources: Array<{ endpointId: string; label: string; kind: EndpointRuntime["kind"]; lostDelta: number }>;
  bridgeProcessingLostDelta: number;
  bridgeProcessingSources: Array<{ endpointId: string; label: string; kind: EndpointRuntime["kind"]; lostDelta: number }>;
} {
  const sources: Array<{ endpointId: string; label: string; kind: EndpointRuntime["kind"]; lostDelta: number }> = [];
  const bridgeProcessingSources: Array<{
    endpointId: string;
    label: string;
    kind: EndpointRuntime["kind"];
    lostDelta: number;
  }> = [];
  let packetDelta = 0;
  let lostDelta = 0;
  let bridgeProcessingLostDelta = 0;

  for (const sample of endpointSamples) {
    if (sample.endpointId === destination.id || sample.kind === "file") {
      continue;
    }
    const previous = previousQualityByEndpoint.get(sample.endpointId)?.bridge;
    if (!sample.bridge || !previous) {
      continue;
    }
    const samplePacketDelta = counterDelta(sample.bridge.inbound?.packets, previous.inbound?.packets);
    const samplePostStr0mLostDelta = counterDelta(sample.bridge.inbound?.packets_lost, previous.inbound?.packets_lost);
    const rawTracked = sample.kind === "webrtc" && hasRawRtpCounters(sample.bridge, previous);
    const sampleRawRtpLostDelta = rawTracked
      ? counterDelta(sample.bridge.inbound?.raw_rtp_packets_lost, previous.inbound?.raw_rtp_packets_lost)
      : 0;
    const sampleLostDelta = rawTracked ? sampleRawRtpLostDelta : samplePostStr0mLostDelta;
    packetDelta += samplePacketDelta;
    lostDelta += sampleLostDelta;
    if (sampleLostDelta > 0) {
      sources.push({
        endpointId: sample.endpointId,
        label: sample.label,
        kind: sample.kind,
        lostDelta: sampleLostDelta,
      });
    }
    if (rawTracked && sampleRawRtpLostDelta === 0 && samplePostStr0mLostDelta > 0) {
      bridgeProcessingLostDelta += samplePostStr0mLostDelta;
      bridgeProcessingSources.push({
        endpointId: sample.endpointId,
        label: sample.label,
        kind: sample.kind,
        lostDelta: samplePostStr0mLostDelta,
      });
    }
  }

  return { packetDelta, lostDelta, sources, bridgeProcessingLostDelta, bridgeProcessingSources };
}

function bridgeOutboundDelta(current: EndpointStats, previousState: QualityState): number {
  return counterDelta(current.outbound?.packets, previousState.bridge?.outbound?.packets);
}

function counterDelta(current: number | undefined, previous: number | undefined): number {
  return Math.max(0, (current ?? 0) - (previous ?? 0));
}
