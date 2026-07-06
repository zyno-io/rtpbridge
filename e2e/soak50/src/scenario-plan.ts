import type {
  CallKind,
  CallPlan,
  ImpairmentProfile,
  MutationKind,
  MutationPlan,
  MediaImpairmentPlan,
  ScenarioPlan,
  WebRtcProfile,
} from "./types.js";
import { scaleDuration, SeededRng } from "./utils.js";

interface BuildOptions {
  calls: number;
  seed: number;
  durationScale: number;
  startSpreadMs: number;
  webRtcImpairments: number;
  rtpImpairments: number;
}

export function buildScenarioPlan(options: BuildOptions): ScenarioPlan {
  const rng = new SeededRng(options.seed);
  const kinds = buildCallKinds(options.calls, rng);
  const durations = buildDurations(options.calls, options.durationScale, rng);
  const profileAllocator = new WebRtcProfileAllocator(options.calls, rng);

  const calls: CallPlan[] = kinds.map((kind, index) => ({
    id: `call-${String(index + 1).padStart(3, "0")}`,
    kind,
    durationMs: durations[index]!,
    startOffsetMs: rng.int(0, options.startSpreadMs),
    webRtcProfiles: profileAllocator.next(kind),
    mutations: [],
    impairments: [],
    frequencyHz: 360 + ((index * 37) % 520),
  }));

  assignMutations(calls, rng, options.durationScale);
  assignMediaImpairments(calls, rng, options.durationScale, {
    webRtcCount: options.webRtcImpairments,
    rtpCount: options.rtpImpairments,
  });
  return { seed: options.seed, calls };
}

function buildCallKinds(count: number, rng: SeededRng): CallKind[] {
  if (count === 50) {
    return rng.shuffle([
      ...repeat<CallKind>("rtp-webrtc", 15),
      ...repeat<CallKind>("webrtc-webrtc", 15),
      ...repeat<CallKind>("rtp-rtp", 20),
    ]);
  }

  const rtpRtp = Math.max(1, Math.round(count * 0.4));
  const rtpWebRtc = Math.max(1, Math.round(count * 0.3));
  const webRtcWebRtc = Math.max(0, count - rtpRtp - rtpWebRtc);
  return rng
    .shuffle([
      ...repeat<CallKind>("rtp-rtp", rtpRtp),
      ...repeat<CallKind>("rtp-webrtc", rtpWebRtc),
      ...repeat<CallKind>("webrtc-webrtc", webRtcWebRtc),
    ])
    .slice(0, count);
}

function buildDurations(count: number, scale: number, rng: SeededRng): number[] {
  const durationFor = (minMs: number, maxMs: number) => scaleDuration(rng.int(minMs, maxMs), scale);
  if (count === 50) {
    return rng.shuffle([
      ...repeatWith(28, () => durationFor(2 * 60_000, 4 * 60_000)),
      ...repeatWith(14, () => durationFor(5 * 60_000, 8 * 60_000)),
      ...repeatWith(8, () => durationFor(12 * 60_000, 15 * 60_000)),
    ]);
  }

  return repeatWith(count, (_, index) => {
    if (index < Math.max(1, Math.round(count * 0.16))) {
      return durationFor(12 * 60_000, 15 * 60_000);
    }
    if (index < Math.max(2, Math.round(count * 0.44))) {
      return durationFor(5 * 60_000, 8 * 60_000);
    }
    return durationFor(2 * 60_000, 4 * 60_000);
  });
}

function assignMutations(calls: CallPlan[], rng: SeededRng, scale: number): void {
  const counts: Array<[MutationKind, number, (call: CallPlan) => boolean]> = [
    ["webrtc-ice-restart-bridge", targetCount(calls.length, 8), hasWebRtc],
    ["webrtc-ice-restart-peer", targetCount(calls.length, 4), hasWebRtc],
    ["rtp-reinvite-hold", targetCount(calls.length, 5), hasRtp],
    ["rtp-port-migration", targetCount(calls.length, 4), hasRtp],
    ["hold-music", targetCount(calls.length, 5), () => true],
    ["endpoint-transfer", targetCount(calls.length, 3), () => true],
    ["endpoint-replace", targetCount(calls.length, 6), () => true],
  ];

  for (const [kind, count, predicate] of counts) {
    const candidates = rng.shuffle(calls.filter(predicate));
    for (const call of candidates.slice(0, count)) {
      call.mutations.push(makeMutation(call, kind, rng, scale));
    }
  }

  for (const call of calls) {
    call.mutations.sort((a, b) => a.atMs - b.atMs);
  }
}

function makeMutation(call: CallPlan, kind: MutationKind, rng: SeededRng, scale: number): MutationPlan {
  const minMargin = Math.min(scaleDuration(30_000, scale), Math.max(1000, Math.floor(call.durationMs * 0.15)));
  const latest = Math.max(minMargin + 1000, call.durationMs - minMargin);
  const atMs = rng.int(minMargin, latest);
  const durationMs =
    kind === "hold-music" || kind === "endpoint-transfer" || kind === "rtp-reinvite-hold"
      ? scaleDuration(rng.int(20_000, 45_000), scale)
      : scaleDuration(rng.int(1000, 4000), scale);
  return {
    id: `${call.id}-${kind}-${call.mutations.length + 1}`,
    kind,
    atMs,
    durationMs,
  };
}

function assignMediaImpairments(
  calls: CallPlan[],
  rng: SeededRng,
  scale: number,
  counts: { webRtcCount: number; rtpCount: number },
): void {
  const webRtcCandidates = rng.shuffle(calls.filter(hasWebRtc));
  const webRtcTarget =
    counts.webRtcCount >= 0
      ? Math.min(webRtcCandidates.length, counts.webRtcCount)
      : Math.min(webRtcCandidates.length, targetCount(calls.length, 10));

  const webRtcProfiles = rng.shuffle([
    wifiBurstyProfile(),
    wifiBurstyProfile(),
    wifiBurstyProfile(),
    wifiCongestedProfile(),
    wifiCongestedProfile(),
    cellularEdgeProfile(),
    cellularEdgeProfile(),
    cellularHandoffProfile(),
    cellularHandoffProfile(),
    cellularUplinkCongestionProfile(),
  ]);
  for (const [index, call] of webRtcCandidates.slice(0, webRtcTarget).entries()) {
    call.impairments.push(makeWebRtcImpairment(call, rng, scale, webRtcProfiles[index % webRtcProfiles.length]!));
  }

  const rtpCandidates = rng.shuffle(calls.filter(hasRtp));
  const rtpTarget =
    counts.rtpCount >= 0
      ? Math.min(rtpCandidates.length, counts.rtpCount)
      : Math.min(rtpCandidates.length, targetCount(calls.length, 4));

  const rtpProfiles = rng.shuffle([
    rtpLightLossProfile(),
    rtpLightLossProfile(),
    rtpJitterProfile(),
    rtpJitterProfile(),
  ]);
  for (const [index, call] of rtpCandidates.slice(0, rtpTarget).entries()) {
    call.impairments.push(makeRtpImpairment(call, rng, scale, rtpProfiles[index % rtpProfiles.length]!));
  }

  for (const call of calls) {
    call.impairments.sort((a, b) => a.atMs - b.atMs);
  }
}

function makeWebRtcImpairment(
  call: CallPlan,
  rng: SeededRng,
  scale: number,
  profile: ImpairmentProfileParams,
): MediaImpairmentPlan {
  const targetLabel =
    call.kind === "rtp-webrtc" ? "webrtc" : rng.pick<"webrtc-a" | "webrtc-b">(["webrtc-a", "webrtc-b"]);
  const durationMs = boundedImpairmentDuration(call, rng, scale, profile.durationMinMs, profile.durationMaxMs);
  const atMs = impairmentStartMs(call, rng, scale, durationMs);

  return {
    id: `${call.id}-${profile.profile}-${call.impairments.length + 1}`,
    transport: "webrtc",
    atMs,
    durationMs,
    targetLabel,
    seed: rng.int(1, 0x7fffffff),
    ...stripDurationBounds(profile),
  };
}

function makeRtpImpairment(
  call: CallPlan,
  rng: SeededRng,
  scale: number,
  profile: ImpairmentProfileParams,
): MediaImpairmentPlan {
  const targetLabel =
    call.kind === "rtp-webrtc" ? "rtp" : rng.pick<"a" | "b">(["a", "b"]);
  const durationMs = boundedImpairmentDuration(call, rng, scale, profile.durationMinMs, profile.durationMaxMs);
  const atMs = impairmentStartMs(call, rng, scale, durationMs);

  return {
    id: `${call.id}-${profile.profile}-${call.impairments.length + 1}`,
    transport: "rtp",
    atMs,
    durationMs,
    targetLabel,
    seed: rng.int(1, 0x7fffffff),
    ...stripDurationBounds(profile),
  };
}

function boundedImpairmentDuration(
  call: CallPlan,
  rng: SeededRng,
  scale: number,
  minMs: number,
  maxMs: number,
): number {
  return Math.min(
    scaleDuration(rng.int(minMs, maxMs), scale),
    Math.max(1000, Math.floor(call.durationMs * 0.35)),
  );
}

function impairmentStartMs(call: CallPlan, rng: SeededRng, scale: number, durationMs: number): number {
  const minMargin = Math.min(scaleDuration(20_000, scale), Math.max(1000, Math.floor(call.durationMs * 0.15)));
  const latest = Math.max(minMargin, call.durationMs - minMargin - durationMs);
  return latest > minMargin
    ? rng.int(minMargin, latest)
    : Math.max(0, Math.floor((call.durationMs - durationMs) / 2));
}

interface ImpairmentProfileParams {
  profile: ImpairmentProfile;
  durationMinMs: number;
  durationMaxMs: number;
  lossPct: number;
  jitterMs: number;
  spikePct: number;
  spikeMs: number;
  burstPct: number;
  maxBurstFrames: number;
}

function stripDurationBounds(profile: ImpairmentProfileParams): Omit<ImpairmentProfileParams, "durationMinMs" | "durationMaxMs"> {
  const { durationMinMs: _durationMinMs, durationMaxMs: _durationMaxMs, ...rest } = profile;
  return rest;
}

function wifiBurstyProfile(): ImpairmentProfileParams {
  return {
    profile: "wifi-bursty",
    durationMinMs: 30_000,
    durationMaxMs: 70_000,
    lossPct: 2,
    jitterMs: 35,
    spikePct: 1.5,
    spikeMs: 120,
    burstPct: 0.8,
    maxBurstFrames: 4,
  };
}

function wifiCongestedProfile(): ImpairmentProfileParams {
  return {
    profile: "wifi-congested",
    durationMinMs: 45_000,
    durationMaxMs: 90_000,
    lossPct: 4,
    jitterMs: 90,
    spikePct: 3,
    spikeMs: 240,
    burstPct: 0.5,
    maxBurstFrames: 3,
  };
}

function cellularEdgeProfile(): ImpairmentProfileParams {
  return {
    profile: "cellular-edge",
    durationMinMs: 45_000,
    durationMaxMs: 120_000,
    lossPct: 3,
    jitterMs: 140,
    spikePct: 4,
    spikeMs: 420,
    burstPct: 0.35,
    maxBurstFrames: 5,
  };
}

function cellularHandoffProfile(): ImpairmentProfileParams {
  return {
    profile: "cellular-handoff",
    durationMinMs: 18_000,
    durationMaxMs: 45_000,
    lossPct: 5,
    jitterMs: 180,
    spikePct: 8,
    spikeMs: 750,
    burstPct: 0.9,
    maxBurstFrames: 10,
  };
}

function cellularUplinkCongestionProfile(): ImpairmentProfileParams {
  return {
    profile: "cellular-uplink-congestion",
    durationMinMs: 60_000,
    durationMaxMs: 150_000,
    lossPct: 4,
    jitterMs: 220,
    spikePct: 6,
    spikeMs: 520,
    burstPct: 0.6,
    maxBurstFrames: 6,
  };
}

function rtpLightLossProfile(): ImpairmentProfileParams {
  return {
    profile: "rtp-light-loss",
    durationMinMs: 30_000,
    durationMaxMs: 90_000,
    lossPct: 1.5,
    jitterMs: 20,
    spikePct: 0.5,
    spikeMs: 80,
    burstPct: 0.1,
    maxBurstFrames: 2,
  };
}

function rtpJitterProfile(): ImpairmentProfileParams {
  return {
    profile: "rtp-jitter",
    durationMinMs: 30_000,
    durationMaxMs: 90_000,
    lossPct: 1,
    jitterMs: 60,
    spikePct: 1,
    spikeMs: 160,
    burstPct: 0.15,
    maxBurstFrames: 2,
  };
}

function targetCount(totalCalls: number, fullCount: number): number {
  if (totalCalls === 50) {
    return fullCount;
  }
  return Math.min(totalCalls, Math.max(1, Math.round((totalCalls / 50) * fullCount)));
}

function hasWebRtc(call: CallPlan): boolean {
  return call.kind === "rtp-webrtc" || call.kind === "webrtc-webrtc";
}

function hasRtp(call: CallPlan): boolean {
  return call.kind === "rtp-webrtc" || call.kind === "rtp-rtp";
}

function repeat<T>(value: T, count: number): T[] {
  return Array.from({ length: count }, () => value);
}

function repeatWith<T>(count: number, make: (rngIndex: number, index: number) => T): T[] {
  return Array.from({ length: count }, (_, index) => make(index, index));
}

class WebRtcProfileAllocator {
  private rtpWebRtcProfiles: WebRtcProfile[];
  private webRtcWebRtcProfiles: WebRtcProfile[][];

  constructor(callCount: number, rng: SeededRng) {
    if (callCount === 50) {
      this.rtpWebRtcProfiles = rng.shuffle([
        ...repeat<WebRtcProfile>("direct", 8),
        ...repeat<WebRtcProfile>("relay", 7),
      ]);
      this.webRtcWebRtcProfiles = rng.shuffle([
        ...repeat<WebRtcProfile[]>(["direct", "direct"], 5),
        ...repeat<WebRtcProfile[]>(["direct", "relay"], 5),
        ...repeat<WebRtcProfile[]>(["relay", "relay"], 5),
      ]);
      return;
    }

    this.rtpWebRtcProfiles = rng.shuffle([
      ...repeat<WebRtcProfile>("direct", Math.ceil(callCount * 0.15)),
      ...repeat<WebRtcProfile>("relay", Math.ceil(callCount * 0.15)),
    ]);
    this.webRtcWebRtcProfiles = rng.shuffle([
      ...repeat<WebRtcProfile[]>(["direct", "direct"], Math.ceil(callCount * 0.1)),
      ...repeat<WebRtcProfile[]>(["direct", "relay"], Math.ceil(callCount * 0.1)),
      ...repeat<WebRtcProfile[]>(["relay", "relay"], Math.ceil(callCount * 0.1)),
    ]);
  }

  next(kind: CallKind): WebRtcProfile[] {
    if (kind === "rtp-rtp") {
      return [];
    }
    if (kind === "rtp-webrtc") {
      return [this.rtpWebRtcProfiles.shift() ?? "direct"];
    }
    return [...(this.webRtcWebRtcProfiles.shift() ?? ["direct", "direct"])];
  }
}
