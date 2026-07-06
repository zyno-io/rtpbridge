import type { ControlClient } from "./control-client.js";
import type { RtpPeer } from "./rtp-peer.js";
import type { WebRtcPeer } from "./webrtc-peer.js";

export type CallKind = "rtp-webrtc" | "webrtc-webrtc" | "rtp-rtp";
export type WebRtcProfile = "direct" | "relay";

export type MutationKind =
  | "webrtc-ice-restart-bridge"
  | "webrtc-ice-restart-peer"
  | "rtp-reinvite-hold"
  | "rtp-port-migration"
  | "hold-music"
  | "endpoint-transfer"
  | "endpoint-replace";

export interface MutationPlan {
  id: string;
  kind: MutationKind;
  atMs: number;
  durationMs: number;
  completed?: boolean;
  failed?: string;
}

export type ImpairmentTransport = "webrtc" | "rtp";
export type ImpairmentTargetLabel = "webrtc" | "webrtc-a" | "webrtc-b" | "rtp" | "a" | "b";
export type ImpairmentProfile =
  | "wifi-bursty"
  | "wifi-congested"
  | "cellular-edge"
  | "cellular-handoff"
  | "cellular-uplink-congestion"
  | "rtp-light-loss"
  | "rtp-jitter";

export interface MediaImpairmentPlan {
  id: string;
  transport: ImpairmentTransport;
  profile: ImpairmentProfile;
  atMs: number;
  durationMs: number;
  targetLabel: ImpairmentTargetLabel;
  seed: number;
  lossPct: number;
  jitterMs: number;
  spikePct: number;
  spikeMs: number;
  burstPct: number;
  maxBurstFrames: number;
  completed?: boolean;
  failed?: string;
}

export interface CallPlan {
  id: string;
  kind: CallKind;
  durationMs: number;
  startOffsetMs: number;
  webRtcProfiles: WebRtcProfile[];
  mutations: MutationPlan[];
  impairments: MediaImpairmentPlan[];
  frequencyHz: number;
}

export interface ScenarioPlan {
  seed: number;
  calls: CallPlan[];
}

export interface RunnerOptions {
  calls: number;
  seed: number;
  durationScale: number;
  dryRun: boolean;
  requireTurn: boolean;
  controlUrl?: string;
  rtpbridgeBin?: string;
  artifactDir: string;
  mediaIp: string;
  listenHost: string;
  rtpPortStart: number;
  rtpPortEnd: number;
  sampleIntervalMs: number;
  startSpreadMs: number;
  webRtcImpairments: number;
  rtpImpairments: number;
  startupTimeoutMs: number;
  logLevel: string;
  turnUrl?: string;
  turnUser?: string;
  turnPass?: string;
}

export interface EndpointRuntime {
  id: string;
  kind: "rtp" | "webrtc" | "file";
  label: string;
  control: ControlClient;
  rtpPeer?: RtpPeer;
  webRtcPeer?: WebRtcPeer;
}

export interface CallRuntime {
  plan: CallPlan;
  control: ControlClient;
  controlUrl: string;
  sessionId: string;
  endpoints: EndpointRuntime[];
  startedAtMs: number;
  endsAtMs: number;
  graceUntilMs: number;
  destroyed: boolean;
}

export interface EndpointStats {
  endpoint_id: string;
  inbound?: {
    packets?: number;
    bytes?: number;
    raw_packets?: number;
    raw_bytes?: number;
  };
  outbound?: {
    packets?: number;
    bytes?: number;
  };
  state?: string;
  ice_state?: string;
  codec?: string;
}

export interface StatsEvent {
  event: "stats";
  data: {
    endpoints: EndpointStats[];
  };
}

export interface PeerCounters {
  sentPackets: number;
  receivedPackets: number;
  sentBytes: number;
  receivedBytes: number;
  impairment?: {
    active: boolean;
    id?: string;
    supported: boolean;
    framesSeen: number;
    droppedFrames: number;
    delayedFrames: number;
    totalDelayMs: number;
  };
}

export interface WebRtcCounters extends PeerCounters {
  iceConnectionState: string;
  connectionState: string;
  audioContextState?: string;
  trackStates?: Array<{ readyState: string; enabled: boolean; muted: boolean }>;
  selectedLocalCandidateType?: string;
  selectedRemoteCandidateType?: string;
}

export interface TimelineEvent {
  ts: string;
  mono_ms: number;
  call_id?: string;
  type: string;
  [key: string]: unknown;
}

export interface Failure {
  ts: string;
  callId?: string;
  reason: string;
  detail?: unknown;
}
