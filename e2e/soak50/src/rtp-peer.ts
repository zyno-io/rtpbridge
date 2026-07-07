import dgram from "node:dgram";
import type { RemoteInfo, Socket } from "node:dgram";

import type { MediaImpairmentPlan, PeerCounters, RtpReceiveQuality } from "./types.js";
import { monotonicMs, sleep } from "./utils.js";

export interface RtpAddress {
  ip: string;
  port: number;
}

export class RtpPeer {
  private socket?: Socket;
  private local?: RtpAddress;
  private remote?: RtpAddress;
  private recvStarted = false;
  private stopRequested = false;
  private sendLoop?: Promise<void>;
  private seq = 0;
  private timestamp = 0;
  private readonly ssrc = Math.floor(Math.random() * 0xffffffff) >>> 0;
  private readonly counters: PeerCounters = {
    sentPackets: 0,
    receivedPackets: 0,
    sentBytes: 0,
    receivedBytes: 0,
    rtpQuality: {
      packets: 0,
      expectedPackets: 0,
      lostPackets: 0,
      duplicatePackets: 0,
      outOfOrderPackets: 0,
      sequenceGaps: 0,
      maxGapPackets: 0,
      ssrcChanges: 0,
      interarrivalSamples: 0,
      meanInterarrivalMs: 0,
      maxInterarrivalMs: 0,
      jitterMs: 0,
    },
    impairment: {
      active: false,
      supported: true,
      framesSeen: 0,
      droppedFrames: 0,
      delayedFrames: 0,
      totalDelayMs: 0,
    },
  };
  private impairment?: ActiveImpairment;
  private rngState = 1;
  private lastArrivalMs?: number;

  private constructor(
    readonly label: string,
    private readonly bindIp = "127.0.0.1",
    private readonly frequencyHz = 440,
  ) {}

  static async create(label: string, bindIp = "127.0.0.1", frequencyHz = 440): Promise<RtpPeer> {
    const peer = new RtpPeer(label, bindIp, frequencyHz);
    await peer.bind();
    return peer;
  }

  localAddress(): RtpAddress {
    if (!this.local) {
      throw new Error(`${this.label}: RTP peer is not bound`);
    }
    return this.local;
  }

  remoteAddress(): RtpAddress | undefined {
    return this.remote;
  }

  setRemote(remote: RtpAddress): void {
    this.remote = remote;
  }

  setRemoteFromSdp(sdp: string): void {
    const remote = parseRtpAddressFromSdp(sdp);
    if (!remote) {
      throw new Error(`${this.label}: could not parse remote RTP address from SDP`);
    }
    this.remote = remote;
  }

  makeSdpOffer(direction = "sendrecv"): string {
    return this.makeSdp(100, direction);
  }

  makeSdpAnswer(direction = "sendrecv"): string {
    return this.makeSdp(200, direction);
  }

  makeReinviteSdp(direction = "sendrecv"): string {
    return this.makeSdp(300, direction);
  }

  startReceiving(): void {
    if (this.recvStarted || !this.socket) {
      return;
    }
    this.recvStarted = true;
    this.socket.on("message", (message: Buffer, _remote: RemoteInfo) => {
      if (message.length < 12) {
        return;
      }
      this.counters.receivedPackets += 1;
      this.counters.receivedBytes += message.length;
      this.observeInboundRtp(message, monotonicMs());
    });
  }

  startMediaLoop(): void {
    if (this.sendLoop) {
      return;
    }
    this.stopRequested = false;
    this.sendLoop = this.mediaLoop();
  }

  stopMediaLoop(): void {
    this.stopRequested = true;
  }

  async sendActivationPacket(): Promise<void> {
    await this.sendPcmuPayload(Buffer.alloc(160, 0xff));
  }

  async rebind(): Promise<void> {
    const remote = this.remote;
    await this.closeSocket();
    this.local = undefined;
    this.recvStarted = false;
    await this.bind();
    if (remote) {
      this.remote = remote;
    }
    this.startReceiving();
  }

  snapshot(): PeerCounters {
    return {
      ...this.counters,
      rtpQuality: this.counters.rtpQuality ? { ...this.counters.rtpQuality } : undefined,
      impairment: this.counters.impairment ? { ...this.counters.impairment } : undefined,
    };
  }

  applyImpairment(impairment: MediaImpairmentPlan): void {
    this.rngState = impairment.seed >>> 0;
    this.impairment = {
      id: impairment.id,
      lossProbability: impairment.lossPct / 100,
      jitterMs: impairment.jitterMs,
      spikeProbability: impairment.spikePct / 100,
      spikeMs: impairment.spikeMs,
      burstProbability: impairment.burstPct / 100,
      maxBurstFrames: impairment.maxBurstFrames,
      burstFramesRemaining: 0,
    };
    if (this.counters.impairment) {
      this.counters.impairment.active = true;
      this.counters.impairment.id = impairment.id;
    }
  }

  clearImpairment(impairmentId: string): void {
    if (this.impairment?.id !== impairmentId) {
      return;
    }
    this.impairment = undefined;
    if (this.counters.impairment) {
      this.counters.impairment.active = false;
      delete this.counters.impairment.id;
    }
  }

  async close(): Promise<void> {
    this.stopRequested = true;
    await this.sendLoop?.catch(() => undefined);
    await this.closeSocket();
  }

  private async bind(): Promise<void> {
    const socket = dgram.createSocket(this.bindIp.includes(":") ? "udp6" : "udp4");
    await new Promise<void>((resolve, reject) => {
      socket.once("error", reject);
      socket.bind(0, this.bindIp, () => {
        socket.off("error", reject);
        const addr = socket.address();
        if (typeof addr === "string") {
          reject(new Error(`${this.label}: unexpected string socket address`));
          return;
        }
        this.local = { ip: addr.address, port: addr.port };
        this.socket = socket;
        resolve();
      });
    });
  }

  private async closeSocket(): Promise<void> {
    const socket = this.socket;
    this.socket = undefined;
    if (!socket) {
      return;
    }
    await new Promise<void>((resolve) => socket.close(() => resolve()));
  }

  private async mediaLoop(): Promise<void> {
    while (!this.stopRequested) {
      await this.sendTonePacket();
      await sleep(20);
    }
  }

  private observeInboundRtp(packet: Buffer, arrivalMs: number): void {
    const quality = this.counters.rtpQuality;
    if (!quality) {
      return;
    }

    const seq = packet.readUInt16BE(2);
    const timestamp = packet.readUInt32BE(4);
    const ssrc = packet.readUInt32BE(8);

    quality.packets += 1;

    if (quality.lastSsrc !== undefined && quality.lastSsrc !== ssrc) {
      quality.ssrcChanges += 1;
      this.resetInboundStreamQuality(quality, ssrc, seq, timestamp, arrivalMs);
      return;
    }

    this.observeInboundTiming(quality, timestamp, arrivalMs);

    let advanceSequence = true;
    if (quality.lastSequence === undefined) {
      quality.expectedPackets = 1;
    } else {
      const delta = rtpSequenceDelta(seq, quality.lastSequence);
      if (delta === 0) {
        quality.duplicatePackets += 1;
        advanceSequence = false;
      } else if (delta < 0x8000) {
        quality.expectedPackets += delta;
        if (delta > 1) {
          const missing = delta - 1;
          quality.lostPackets += missing;
          quality.sequenceGaps += 1;
          quality.maxGapPackets = Math.max(quality.maxGapPackets, missing);
        }
      } else {
        quality.outOfOrderPackets += 1;
        advanceSequence = false;
      }
    }

    if (advanceSequence) {
      quality.lastSequence = seq;
    }
    quality.lastTimestamp = timestamp;
    quality.lastSsrc = ssrc;
    this.lastArrivalMs = arrivalMs;
  }

  private observeInboundTiming(quality: RtpReceiveQuality, timestamp: number, arrivalMs: number): void {
    if (this.lastArrivalMs === undefined || quality.lastTimestamp === undefined) {
      return;
    }

    const arrivalDeltaMs = Math.max(0, arrivalMs - this.lastArrivalMs);
    quality.interarrivalSamples += 1;
    quality.meanInterarrivalMs +=
      (arrivalDeltaMs - quality.meanInterarrivalMs) / quality.interarrivalSamples;
    quality.maxInterarrivalMs = Math.max(quality.maxInterarrivalMs, arrivalDeltaMs);

    const timestampDeltaMs = rtpTimestampDelta(timestamp, quality.lastTimestamp) / 8;
    if (timestampDeltaMs >= 0 && timestampDeltaMs < 1000) {
      const variationMs = Math.abs(arrivalDeltaMs - timestampDeltaMs);
      quality.jitterMs += (variationMs - quality.jitterMs) / 16;
    }
  }

  private resetInboundStreamQuality(
    quality: RtpReceiveQuality,
    ssrc: number,
    seq: number,
    timestamp: number,
    arrivalMs: number,
  ): void {
    quality.expectedPackets += 1;
    quality.lastSequence = seq;
    quality.lastTimestamp = timestamp;
    quality.lastSsrc = ssrc;
    this.lastArrivalMs = arrivalMs;
  }

  private async sendTonePacket(): Promise<void> {
    const payload = Buffer.alloc(160);
    for (let i = 0; i < 160; i += 1) {
      const t = (this.timestamp + i) / 8000;
      const sample = Math.sin(2 * Math.PI * this.frequencyHz * t) * 14000;
      payload[i] = linearToUlaw(sample);
    }
    await this.sendPcmuPayload(payload);
  }

  private async sendPcmuPayload(payload: Buffer): Promise<void> {
    if (!this.socket || !this.remote) {
      return;
    }

    const packet = buildRtpPacket(0, this.seq, this.timestamp, this.ssrc, this.seq === 0, payload);
    this.seq = (this.seq + 1) & 0xffff;
    this.timestamp = (this.timestamp + 160) >>> 0;

    const impairment = this.impairment;
    if (impairment) {
      this.counters.impairment!.framesSeen += 1;
      if (this.shouldDrop(impairment)) {
        this.counters.impairment!.droppedFrames += 1;
        return;
      }
      const delayMs = this.delayFor(impairment);
      if (delayMs > 0) {
        this.counters.impairment!.delayedFrames += 1;
        this.counters.impairment!.totalDelayMs += delayMs;
        await sleep(delayMs);
      }
    }

    await new Promise<void>((resolve, reject) => {
      this.socket!.send(packet, this.remote!.port, this.remote!.ip, (error) => {
        if (error) {
          reject(error);
          return;
        }
        resolve();
      });
    });

    this.counters.sentPackets += 1;
    this.counters.sentBytes += packet.length;
  }

  private shouldDrop(impairment: ActiveImpairment): boolean {
    if (impairment.burstFramesRemaining > 0) {
      impairment.burstFramesRemaining -= 1;
      return true;
    }
    if (this.random() < impairment.burstProbability) {
      const burstFrames = 1 + Math.floor(this.random() * impairment.maxBurstFrames);
      impairment.burstFramesRemaining = Math.max(0, burstFrames - 1);
      return true;
    }
    return this.random() < impairment.lossProbability;
  }

  private delayFor(impairment: ActiveImpairment): number {
    let delayMs = impairment.jitterMs > 0 ? Math.floor(this.random() * impairment.jitterMs) : 0;
    if (impairment.spikeMs > 0 && this.random() < impairment.spikeProbability) {
      delayMs += impairment.spikeMs;
    }
    return delayMs;
  }

  private random(): number {
    let t = (this.rngState = (this.rngState + 0x6d2b79f5) >>> 0);
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  }

  private makeSdp(sessionVersion: number, direction: string): string {
    const local = this.localAddress();
    const family = local.ip.includes(":") ? "IP6" : "IP4";
    return [
      "v=0",
      `o=- ${sessionVersion} 1 IN ${family} ${local.ip}`,
      "s=-",
      `c=IN ${family} ${local.ip}`,
      "t=0 0",
      `m=audio ${local.port} RTP/AVP 0 101`,
      "a=rtpmap:0 PCMU/8000",
      "a=rtpmap:101 telephone-event/8000",
      "a=fmtp:101 0-16",
      `a=${direction}`,
      "",
    ].join("\r\n");
  }
}

interface ActiveImpairment {
  id: string;
  lossProbability: number;
  jitterMs: number;
  spikeProbability: number;
  spikeMs: number;
  burstProbability: number;
  maxBurstFrames: number;
  burstFramesRemaining: number;
}

export function parseRtpAddressFromSdp(sdp: string): RtpAddress | undefined {
  let ip = "";
  let port = 0;
  for (const raw of sdp.split(/\r?\n/)) {
    const line = raw.trim();
    if (line.startsWith("c=IN IP4 ")) {
      ip = line.slice("c=IN IP4 ".length).trim();
    } else if (line.startsWith("c=IN IP6 ")) {
      ip = line.slice("c=IN IP6 ".length).trim();
    } else if (line.startsWith("m=audio ")) {
      const parts = line.split(/\s+/);
      port = Number.parseInt(parts[1] ?? "0", 10);
    }
  }
  if (!ip || !Number.isFinite(port) || port <= 0) {
    return undefined;
  }
  return { ip, port };
}

function buildRtpPacket(
  payloadType: number,
  seq: number,
  timestamp: number,
  ssrc: number,
  marker: boolean,
  payload: Buffer,
): Buffer {
  const packet = Buffer.alloc(12 + payload.length);
  packet[0] = 0x80;
  packet[1] = (marker ? 0x80 : 0) | (payloadType & 0x7f);
  packet.writeUInt16BE(seq & 0xffff, 2);
  packet.writeUInt32BE(timestamp >>> 0, 4);
  packet.writeUInt32BE(ssrc >>> 0, 8);
  payload.copy(packet, 12);
  return packet;
}

function rtpSequenceDelta(seq: number, previousSeq: number): number {
  return (seq - previousSeq + 0x10000) & 0xffff;
}

function rtpTimestampDelta(timestamp: number, previousTimestamp: number): number {
  return (timestamp - previousTimestamp) >>> 0;
}

function linearToUlaw(sample: number): number {
  const BIAS = 0x84;
  const CLIP = 32635;
  let pcm = Math.max(-CLIP, Math.min(CLIP, Math.round(sample)));
  let sign = 0;
  if (pcm < 0) {
    pcm = -pcm;
    sign = 0x80;
  }
  pcm += BIAS;

  let exponent = 7;
  for (let expMask = 0x4000; (pcm & expMask) === 0 && exponent > 0; exponent -= 1, expMask >>= 1) {
    // Scan for the segment.
  }
  const mantissa = (pcm >> (exponent + 3)) & 0x0f;
  return (~(sign | (exponent << 4) | mantissa)) & 0xff;
}
