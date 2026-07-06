import type {
  CallRuntime,
  EndpointRuntime,
  EndpointStats,
  Failure,
  PeerCounters,
  WebRtcCounters,
} from "./types.js";
import { nowIso } from "./utils.js";

interface DirectionState {
  packets: number;
  flatlineSinceMs?: number;
}

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

export class MediaMonitor {
  private readonly directions = new Map<string, DirectionState>();
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
          message.includes("Target closed")
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
