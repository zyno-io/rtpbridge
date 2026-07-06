import { randomUUID } from "node:crypto";
import type { Browser, Page } from "playwright";

import type { MediaImpairmentPlan, WebRtcCounters, WebRtcProfile } from "./types.js";
import { sleep } from "./utils.js";

export interface TurnConfig {
  url?: string;
  user?: string;
  pass?: string;
}

const sharedPages = new WeakMap<Browser, Promise<Page>>();

async function getSharedPage(browser: Browser, pageUrl: string): Promise<Page> {
  let pagePromise = sharedPages.get(browser);
  if (!pagePromise) {
    pagePromise = (async () => {
      const page = await browser.newPage();
      await page.goto(pageUrl, { waitUntil: "domcontentloaded" });
      await page.evaluate(`(() => {
        if (window.__soak50) {
          return;
        }

        const AudioContextCtor = window.AudioContext || window.webkitAudioContext;
        const audioContext = new AudioContextCtor({ sampleRate: 48000, latencyHint: "interactive" });
        const silentSink = audioContext.createGain();
        silentSink.gain.value = 0;
        silentSink.connect(audioContext.destination);

        const keepAudioRunning = () => audioContext.resume().catch(() => undefined);
        const audioKeepAlive = window.setInterval(keepAudioRunning, 500);
        document.addEventListener("visibilitychange", keepAudioRunning);
        keepAudioRunning();

        window.__soak50 = {
          audioContext,
          audioKeepAlive,
          keepAudioRunning,
          peers: new Map(),
          silentSink
        };
      })()`);
      return page;
    })();
    sharedPages.set(browser, pagePromise);
  }
  return pagePromise;
}

export class WebRtcPeer {
  private constructor(
    readonly label: string,
    readonly profile: WebRtcProfile,
    private readonly page: Page,
    private readonly peerId: string,
  ) {}

  static async create(params: {
    browser: Browser;
    pageUrl: string;
    label: string;
    profile: WebRtcProfile;
    frequencyHz: number;
    turn: TurnConfig;
  }): Promise<WebRtcPeer> {
    const page = await getSharedPage(params.browser, params.pageUrl);
    const peerId = randomUUID();
    const init = JSON.stringify({
      id: peerId,
      profile: params.profile,
      frequencyHz: params.frequencyHz,
      turn: params.turn,
    });

    await page.evaluate(`(async () => {
      const init = ${init};
      const runtime = window.__soak50;
      const iceServers =
        init.profile === "relay" && init.turn.url
          ? [{ urls: init.turn.url, username: init.turn.user || "", credential: init.turn.pass || "" }]
          : [];

      await runtime.audioContext.resume();

      const pc = new RTCPeerConnection({
        iceServers,
        iceTransportPolicy: init.profile === "relay" ? "relay" : "all",
        bundlePolicy: "max-bundle",
        rtcpMuxPolicy: "require",
        encodedInsertableStreams: true
      });

      const oscillator = runtime.audioContext.createOscillator();
      oscillator.type = "sine";
      oscillator.frequency.value = init.frequencyHz;

      const gain = runtime.audioContext.createGain();
      gain.gain.value = 0.08;

      const destination = runtime.audioContext.createMediaStreamDestination();
      oscillator.connect(gain);
      gain.connect(destination);
      gain.connect(runtime.silentSink);
      oscillator.start();

      const stream = destination.stream;
      const senders = [];
      for (const track of stream.getAudioTracks()) {
        senders.push(pc.addTrack(track, stream));
      }

      const audios = [];
      pc.ontrack = (event) => {
        const inbound = event.streams[0] || new MediaStream([event.track]);
        const audio = document.createElement("audio");
        audio.autoplay = true;
        audio.muted = true;
        audio.srcObject = inbound;
        document.body.appendChild(audio);
        audios.push(audio);
        audio.play().catch(() => undefined);
      };

      const peer = {
        pc,
        oscillator,
        gain,
        destination,
        tracks: stream.getTracks(),
        audios,
        impairment: null,
        impairmentStats: {
          supported: senders.every((sender) => typeof sender.createEncodedStreams === "function"),
          framesSeen: 0,
          droppedFrames: 0,
          delayedFrames: 0,
          totalDelayMs: 0
        },
        rngState: 1
      };
      runtime.peers.set(init.id, peer);

      const random = () => {
        let t = peer.rngState = (peer.rngState + 0x6d2b79f5) >>> 0;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
      };

      const shouldDrop = (impairment) => {
        if ((impairment.burstFramesRemaining || 0) > 0) {
          impairment.burstFramesRemaining -= 1;
          return true;
        }
        if (random() < impairment.burstProbability) {
          const burstFrames = 1 + Math.floor(random() * impairment.maxBurstFrames);
          impairment.burstFramesRemaining = Math.max(0, burstFrames - 1);
          return true;
        }
        return random() < impairment.lossProbability;
      };

      const delayFor = (impairment) => {
        let delayMs = impairment.jitterMs > 0 ? Math.floor(random() * impairment.jitterMs) : 0;
        if (impairment.spikeMs > 0 && random() < impairment.spikeProbability) {
          delayMs += impairment.spikeMs;
        }
        return delayMs;
      };

      for (const sender of senders) {
        if (typeof sender.createEncodedStreams !== "function") {
          continue;
        }
        const streams = sender.createEncodedStreams();
        streams.readable
          .pipeThrough(new TransformStream({
            async transform(frame, controller) {
              const impairment = peer.impairment;
              if (!impairment || !impairment.active) {
                controller.enqueue(frame);
                return;
              }

              peer.impairmentStats.framesSeen += 1;
              if (shouldDrop(impairment)) {
                peer.impairmentStats.droppedFrames += 1;
                return;
              }

              const delayMs = delayFor(impairment);
              if (delayMs > 0) {
                peer.impairmentStats.delayedFrames += 1;
                peer.impairmentStats.totalDelayMs += delayMs;
                await new Promise((resolve) => setTimeout(resolve, delayMs));
              }
              controller.enqueue(frame);
            }
          }))
          .pipeTo(streams.writable)
          .catch(() => undefined);
      }
    })()`);

    return new WebRtcPeer(params.label, params.profile, page, peerId);
  }

  async acceptOffer(offerSdp: string): Promise<string> {
    const peerId = JSON.stringify(this.peerId);
    return this.page.evaluate<string>(`(async () => {
      const runtime = window.__soak50;
      await runtime.audioContext.resume();
      const peer = runtime.peers.get(${peerId});
      if (!peer) {
        throw new Error("unknown WebRTC peer");
      }
      const pc = peer.pc;
      await pc.setRemoteDescription({ type: "offer", sdp: ${JSON.stringify(offerSdp)} });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      if (pc.iceGatheringState !== "complete") {
        await new Promise((resolve) => {
          const timeout = setTimeout(resolve, 4000);
          const onState = () => {
            if (pc.iceGatheringState === "complete") {
              clearTimeout(timeout);
              pc.removeEventListener("icegatheringstatechange", onState);
              resolve();
            }
          };
          pc.addEventListener("icegatheringstatechange", onState);
        });
      }
      return pc.localDescription.sdp;
    })()`);
  }

  async createRestartOffer(): Promise<string> {
    const peerId = JSON.stringify(this.peerId);
    return this.page.evaluate<string>(`(async () => {
      const runtime = window.__soak50;
      await runtime.audioContext.resume();
      const peer = runtime.peers.get(${peerId});
      if (!peer) {
        throw new Error("unknown WebRTC peer");
      }
      const pc = peer.pc;
      pc.restartIce();
      const offer = await pc.createOffer({ iceRestart: true });
      await pc.setLocalDescription(offer);
      if (pc.iceGatheringState !== "complete") {
        await new Promise((resolve) => {
          const timeout = setTimeout(resolve, 4000);
          const onState = () => {
            if (pc.iceGatheringState === "complete") {
              clearTimeout(timeout);
              pc.removeEventListener("icegatheringstatechange", onState);
              resolve();
            }
          };
          pc.addEventListener("icegatheringstatechange", onState);
        });
      }
      return pc.localDescription.sdp;
    })()`);
  }

  async acceptAnswer(answerSdp: string): Promise<void> {
    const peerId = JSON.stringify(this.peerId);
    await this.page.evaluate(`(async () => {
      const peer = window.__soak50.peers.get(${peerId});
      if (!peer) {
        throw new Error("unknown WebRTC peer");
      }
      await peer.pc.setRemoteDescription({ type: "answer", sdp: ${JSON.stringify(answerSdp)} });
    })()`);
  }

  async waitConnected(timeoutMs: number): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    const peerId = JSON.stringify(this.peerId);
    while (Date.now() < deadline) {
      const state = await this.page.evaluate<{
        iceConnectionState: string;
        connectionState: string;
      }>(`(async () => {
        const runtime = window.__soak50;
        await runtime.audioContext.resume();
        const peer = runtime.peers.get(${peerId});
        if (!peer) {
          throw new Error("unknown WebRTC peer");
        }
        const pc = peer.pc;
        return { iceConnectionState: pc.iceConnectionState, connectionState: pc.connectionState };
      })()`);
      if (
        state.iceConnectionState === "connected" ||
        state.iceConnectionState === "completed" ||
        state.connectionState === "connected"
      ) {
        return;
      }
      if (state.iceConnectionState === "failed" || state.connectionState === "failed") {
        throw new Error(`${this.label}: WebRTC failed to connect`);
      }
      await sleep(100);
    }
    throw new Error(`${this.label}: WebRTC connect timeout`);
  }

  async applyImpairment(impairment: MediaImpairmentPlan): Promise<void> {
    const peerId = JSON.stringify(this.peerId);
    const payload = JSON.stringify({
      id: impairment.id,
      seed: impairment.seed,
      lossProbability: impairment.lossPct / 100,
      jitterMs: impairment.jitterMs,
      spikeProbability: impairment.spikePct / 100,
      spikeMs: impairment.spikeMs,
      burstProbability: impairment.burstPct / 100,
      maxBurstFrames: impairment.maxBurstFrames,
    });
    await this.page.evaluate(`(() => {
      const peer = window.__soak50.peers.get(${peerId});
      if (!peer) {
        throw new Error("unknown WebRTC peer");
      }
      if (!peer.impairmentStats || !peer.impairmentStats.supported) {
        throw new Error("WebRTC encoded frame transforms are not supported by this browser");
      }
      const impairment = ${payload};
      peer.rngState = impairment.seed >>> 0;
      peer.impairment = {
        ...impairment,
        active: true,
        burstFramesRemaining: 0
      };
    })()`);
  }

  async clearImpairment(impairmentId: string): Promise<void> {
    const peerId = JSON.stringify(this.peerId);
    await this.page.evaluate(`(() => {
      const peer = window.__soak50.peers.get(${peerId});
      if (!peer || !peer.impairment) {
        return;
      }
      if (peer.impairment.id === ${JSON.stringify(impairmentId)}) {
        peer.impairment.active = false;
        peer.impairment = null;
      }
    })()`);
  }

  async snapshot(): Promise<WebRtcCounters> {
    const peerId = JSON.stringify(this.peerId);
    return this.page.evaluate<WebRtcCounters>(`(async () => {
      const runtime = window.__soak50;
      await runtime.audioContext.resume();
      const peer = runtime.peers.get(${peerId});
      if (!peer) {
        throw new Error("unknown WebRTC peer");
      }
      const pc = peer.pc;
      const report = await pc.getStats();
      let sentPackets = 0;
      let sentBytes = 0;
      let receivedPackets = 0;
      let receivedBytes = 0;
      let selectedLocalCandidateType;
      let selectedRemoteCandidateType;

      const isAudioRtp = (stat) =>
        stat.kind === "audio" ||
        stat.mediaType === "audio" ||
        (stat.kind === undefined && stat.mediaType === undefined);
      for (const stat of report.values()) {
        if (stat.type === "outbound-rtp" && isAudioRtp(stat) && !stat.isRemote) {
          sentPackets += stat.packetsSent || 0;
          sentBytes += stat.bytesSent || 0;
        } else if (stat.type === "inbound-rtp" && isAudioRtp(stat) && !stat.isRemote) {
          receivedPackets += stat.packetsReceived || 0;
          receivedBytes += stat.bytesReceived || 0;
        }
      }

      for (const stat of report.values()) {
        if (stat.type === "transport" && stat.selectedCandidatePairId) {
          const pair = report.get(stat.selectedCandidatePairId);
          if (pair) {
            const local = report.get(pair.localCandidateId);
            const remote = report.get(pair.remoteCandidateId);
            selectedLocalCandidateType = local && local.candidateType;
            selectedRemoteCandidateType = remote && remote.candidateType;
          }
        }
      }

      if (!selectedLocalCandidateType) {
        for (const stat of report.values()) {
          if (stat.type === "candidate-pair" && (stat.selected || (stat.nominated && stat.state === "succeeded"))) {
            const local = report.get(stat.localCandidateId);
            const remote = report.get(stat.remoteCandidateId);
            selectedLocalCandidateType = local && local.candidateType;
            selectedRemoteCandidateType = remote && remote.candidateType;
            break;
          }
        }
      }

      return {
        sentPackets,
        sentBytes,
        receivedPackets,
        receivedBytes,
        iceConnectionState: pc.iceConnectionState,
        connectionState: pc.connectionState,
        audioContextState: runtime.audioContext && runtime.audioContext.state,
        trackStates: (peer.tracks || []).map((track) => ({
          readyState: track.readyState,
          enabled: track.enabled,
          muted: track.muted
        })),
        selectedLocalCandidateType,
        selectedRemoteCandidateType,
        impairment: peer.impairmentStats
          ? {
              active: !!(peer.impairment && peer.impairment.active),
              id: peer.impairment && peer.impairment.id,
              supported: !!peer.impairmentStats.supported,
              framesSeen: peer.impairmentStats.framesSeen || 0,
              droppedFrames: peer.impairmentStats.droppedFrames || 0,
              delayedFrames: peer.impairmentStats.delayedFrames || 0,
              totalDelayMs: peer.impairmentStats.totalDelayMs || 0
            }
          : undefined
      };
    })()`);
  }

  async close(): Promise<void> {
    const peerId = JSON.stringify(this.peerId);
    await this.page
      .evaluate(`(() => {
        const runtime = window.__soak50;
        if (!runtime) {
          return;
        }
        const peer = runtime.peers.get(${peerId});
        if (!peer) {
          return;
        }
        for (const track of peer.tracks || []) {
          track.stop();
        }
        for (const audio of peer.audios || []) {
          audio.remove();
        }
        try { peer.oscillator && peer.oscillator.stop(); } catch (_) {}
        peer.pc && peer.pc.close();
        runtime.peers.delete(${peerId});
      })()`)
      .catch(() => undefined);
  }
}
