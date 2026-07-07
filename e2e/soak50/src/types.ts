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
  loadSampleIntervalMs: number;
  loadPids: number[];
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
    packets_lost?: number;
    jitter_ms?: number;
    last_received_ms_ago?: number;
    raw_packets?: number;
    raw_bytes?: number;
    raw_rtp_packets?: number;
    raw_rtp_bytes?: number;
    raw_rtp_packets_lost?: number;
    raw_rtp_sequence_gaps?: number;
    raw_rtp_max_sequence_gap?: number;
    raw_rtp_duplicate_packets?: number;
    raw_rtp_out_of_order_packets?: number;
    raw_rtp_sequence_resets?: number;
    raw_rtp_last_sequence?: number;
    raw_rtp_last_ssrc?: number;
    recv_loop_gap_ms?: number;
    max_recv_loop_gap_ms?: number;
    enqueue_wait_ms?: number;
    max_enqueue_wait_ms?: number;
    dequeue_delay_ms?: number;
    max_dequeue_delay_ms?: number;
    channel_capacity?: number;
    min_channel_capacity?: number;
    channel_overflows?: number;
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

export interface RtpReceiveQuality {
  packets: number;
  expectedPackets: number;
  lostPackets: number;
  duplicatePackets: number;
  outOfOrderPackets: number;
  sequenceGaps: number;
  maxGapPackets: number;
  ssrcChanges: number;
  interarrivalSamples: number;
  meanInterarrivalMs: number;
  maxInterarrivalMs: number;
  jitterMs: number;
  lastSequence?: number;
  lastTimestamp?: number;
  lastSsrc?: number;
}

export interface WebRtcReceiveQuality {
  inboundPacketsLost: number;
  inboundJitterMs: number;
  jitterBufferDelayMs: number;
  jitterBufferEmittedCount: number;
  concealedSamples: number;
  concealmentEvents: number;
  totalSamplesReceived: number;
}

export interface PeerCounters {
  sentPackets: number;
  receivedPackets: number;
  sentBytes: number;
  receivedBytes: number;
  rtpQuality?: RtpReceiveQuality;
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
  webRtcQuality?: WebRtcReceiveQuality;
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

export interface LoadProcessSample {
  label: string;
  pid: number;
  ppid: number;
  cpu_pct: number;
  mem_pct: number;
  rss_kb: number;
  vsz_kb: number;
  state: string;
  command: string;
}

export interface LoadProcessGroupSample {
  label: string;
  process_count: number;
  cpu_pct: number;
  rss_kb: number;
}

export interface LoadSample {
  ts: string;
  mono_ms: number;
  loadavg: number[];
  cpu_count: number;
  total_mem_bytes: number;
  free_mem_bytes: number;
  process_count: number;
  process_groups: LoadProcessGroupSample[];
  processes: LoadProcessSample[];
}
