import { execFile, spawn, type ChildProcess } from "node:child_process";
import { createWriteStream } from "node:fs";
import fs from "node:fs/promises";
import { createServer, type Server } from "node:http";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { chromium, type Browser } from "playwright";

import { connectControl, ControlClient } from "./control-client.js";
import { MediaMonitor } from "./media-monitor.js";
import { RunReporter } from "./report.js";
import { RtpPeer } from "./rtp-peer.js";
import { buildScenarioPlan } from "./scenario-plan.js";
import type {
  CallPlan,
  CallRuntime,
  EndpointRuntime,
  Failure,
  LoadProcessSample,
  LoadSample,
  MediaImpairmentPlan,
  MutationPlan,
  RunnerOptions,
  ScenarioPlan,
  WebRtcProfile,
} from "./types.js";
import {
  createRunDir,
  ensureDir,
  ensureHoldMusicWav,
  findFreeTcpPort,
  makeTempRoot,
  monotonicMs,
  nowIso,
  sleep,
  writeJson,
  writeRtpbridgeConfig,
} from "./utils.js";
import { WebRtcPeer } from "./webrtc-peer.js";

const execFileAsync = promisify(execFile);

interface BrowserOrigin {
  server: Server;
  url: string;
}

async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  const plan = buildScenarioPlan({
    calls: options.calls,
    seed: options.seed,
    durationScale: options.durationScale,
    startSpreadMs: options.startSpreadMs,
    webRtcImpairments: options.webRtcImpairments,
    rtpImpairments: options.rtpImpairments,
  });

  const runDir = await createRunDir(options.artifactDir, options.seed);
  const reporter = new RunReporter(runDir);
  await reporter.writePlan(plan);
  await reporter.event("run.plan.created", { run_dir: runDir, calls: plan.calls.length });

  if (options.dryRun) {
    await reporter.writeSummary({
      plan,
      runtimes: [],
      failures: [],
      maxFlatlineMs: 0,
      startedAt: nowIso(),
      finishedAt: nowIso(),
    });
    console.log(`Dry run wrote plan to ${runDir}`);
    return;
  }

  validateTurnOptions(plan, options);

  const failures: Failure[] = [];
  const startedAt = nowIso();
  const tempRoot = await makeTempRoot();
  const mediaDir = path.join(tempRoot, "media");
  const cacheDir = path.join(tempRoot, "cache");
  const recordingDir = path.join(tempRoot, "recordings");
  await Promise.all([ensureDir(mediaDir), ensureDir(cacheDir), ensureDir(recordingDir)]);
  const holdMusicPath = path.join(mediaDir, "hold-music.wav");
  await ensureHoldMusicWav(holdMusicPath);

  let browser: Browser | undefined;
  let browserOrigin: BrowserOrigin | undefined;
  let child: ChildProcess | undefined;
  let controlUrl = options.controlUrl;
  const runtimes: CallRuntime[] = [];
  const monitor = new MediaMonitor(options.sampleIntervalMs);
  let shuttingDown = false;
  let callsDone = false;
  let loadMonitorTask: Promise<void> | undefined;

  const fail = async (reason: string, detail?: unknown, callId?: string) => {
    const failure = { ts: nowIso(), reason, detail, callId };
    failures.push(failure);
    await reporter.event("run.failure", { call_id: callId, reason, detail });
  };

  process.once("SIGINT", () => {
    shuttingDown = true;
    void reporter.event("run.signal", { signal: "SIGINT" });
  });
  process.once("SIGTERM", () => {
    shuttingDown = true;
    void reporter.event("run.signal", { signal: "SIGTERM" });
  });

  try {
    if (!controlUrl) {
      if (!options.rtpbridgeBin) {
        throw new Error("provide --rtpbridge-bin or --control-url");
      }
      const port = await findFreeTcpPort(options.listenHost);
      const listen = `${options.listenHost}:${port}`;
      const configPath = await writeRtpbridgeConfig({
        dir: tempRoot,
        listen,
        mediaIp: options.mediaIp,
        mediaDir,
        cacheDir,
        recordingDir,
        rtpPortStart: options.rtpPortStart,
        rtpPortEnd: options.rtpPortEnd,
        logLevel: options.logLevel,
      });
      controlUrl = `ws://${listen}`;
      child = await startRtpbridge(options.rtpbridgeBin, configPath, path.join(runDir, "rtpbridge.log"));
      await reporter.event("rtpbridge.started", { control_url: controlUrl, config_path: configPath });
    }

    await waitForControl(controlUrl, options.startupTimeoutMs);
    loadMonitorTask = runLoadMonitor({
      reporter,
      options,
      rtpbridgePid: () => child?.pid,
      shouldStop: () => shuttingDown || callsDone,
    });
    await writeMetrics(controlUrl, path.join(runDir, "metrics-before.prom"));

    browserOrigin = await startBrowserOrigin();
    await reporter.event("browser.origin.started", { url: browserOrigin.url });
    const browserPageUrl = browserOrigin.url;

    browser = await chromium.launch({
      headless: true,
      args: [
        "--autoplay-policy=no-user-gesture-required",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-renderer-backgrounding",
        "--use-fake-device-for-media-stream",
        "--use-fake-ui-for-media-stream",
        "--no-sandbox",
      ],
    });

    const callTasks = plan.calls.map((call) =>
      runCall({
        call,
        controlUrl: controlUrl!,
        browser: browser!,
        browserPageUrl,
        options,
        holdMusicPath,
        reporter,
        runtimes,
        fail,
        shouldStop: () => shuttingDown,
      }),
    );

    const monitorTask = runMonitor({
      runtimes,
      monitor,
      reporter,
      options,
      shouldStop: () => shuttingDown || callsDone,
      fail,
    });

    const settled = await Promise.allSettled(callTasks);
    callsDone = true;
    for (const [index, result] of settled.entries()) {
      if (result.status === "rejected") {
        await fail(`call task failed: ${result.reason instanceof Error ? result.reason.message : String(result.reason)}`, undefined, plan.calls[index]?.id);
      }
    }
    shuttingDown = true;
    await Promise.all([monitorTask, loadMonitorTask]);

    await writeMetrics(controlUrl, path.join(runDir, "metrics-after.prom"));
  } finally {
    shuttingDown = true;
    await loadMonitorTask?.catch(() => undefined);
    await cleanupRuntimes(runtimes, reporter);
    if (browser) {
      await browser.close().catch(() => undefined);
    }
    if (browserOrigin) {
      await stopBrowserOrigin(browserOrigin.server).catch(() => undefined);
    }
    if (child) {
      await stopRtpbridge(child);
    }
    await fs.rm(tempRoot, { recursive: true, force: true }).catch(() => undefined);
  }

  const monitorSummary = monitor.summary();
  const allFailures = [...failures, ...monitorSummary.failures];
  await reporter.writeSummary({
    plan,
    runtimes,
    failures: allFailures,
    maxFlatlineMs: monitorSummary.maxFlatlineMs,
    startedAt,
    finishedAt: nowIso(),
  });

  if (allFailures.length > 0) {
    console.error(`soak50 failed with ${allFailures.length} failure(s); artifacts: ${runDir}`);
    process.exitCode = 1;
    return;
  }

  console.log(`soak50 passed; artifacts: ${runDir}`);
}

async function runCall(params: {
  call: CallPlan;
  controlUrl: string;
  browser: Browser;
  browserPageUrl: string;
  options: RunnerOptions;
  holdMusicPath: string;
  reporter: RunReporter;
  runtimes: CallRuntime[];
  fail: (reason: string, detail?: unknown, callId?: string) => Promise<void>;
  shouldStop: () => boolean;
}): Promise<void> {
  await sleep(params.call.startOffsetMs);
  if (params.shouldStop()) {
    return;
  }

  const control = await connectControl(params.controlUrl, params.call.id, params.options.startupTimeoutMs);
  const session = await control.requestOk<{ session_id: string }>("session.create", {});
  const runtime: CallRuntime = {
    plan: params.call,
    control,
    controlUrl: params.controlUrl,
    sessionId: session.session_id,
    endpoints: [],
    startedAtMs: 0,
    endsAtMs: 0,
    graceUntilMs: monotonicMs() + 20_000,
    destroyed: false,
  };

  control.on("event", (event: Record<string, unknown>) => {
    if (event.event === "endpoint.media_timeout" || event.event === "events.dropped") {
      void params.fail(`rtpbridge event ${String(event.event)}`, event, params.call.id);
    }
  });

  params.runtimes.push(runtime);
  await params.reporter.event("call.session.created", {
    call_id: params.call.id,
    session_id: runtime.sessionId,
    kind: params.call.kind,
  });
  await control.requestOk("stats.subscribe", {
    interval_ms: params.options.sampleIntervalMs,
    include_diagnostics: true,
  });

  try {
    if (params.call.kind === "rtp-rtp") {
      runtime.endpoints.push(await createRtpEndpoint(runtime, "a", params.call.frequencyHz, params.reporter));
      runtime.endpoints.push(await createRtpEndpoint(runtime, "b", params.call.frequencyHz + 83, params.reporter));
    } else if (params.call.kind === "rtp-webrtc") {
      runtime.endpoints.push(await createRtpEndpoint(runtime, "rtp", params.call.frequencyHz, params.reporter));
      runtime.endpoints.push(
        await createWebRtcEndpoint(
          runtime,
          "webrtc",
          params.call.webRtcProfiles[0] ?? "direct",
          params.call.frequencyHz + 83,
          params.browser,
          params.browserPageUrl,
          params.options,
          params.reporter,
        ),
      );
    } else {
      runtime.endpoints.push(
        await createWebRtcEndpoint(
          runtime,
          "webrtc-a",
          params.call.webRtcProfiles[0] ?? "direct",
          params.call.frequencyHz,
          params.browser,
          params.browserPageUrl,
          params.options,
          params.reporter,
        ),
      );
      runtime.endpoints.push(
        await createWebRtcEndpoint(
          runtime,
          "webrtc-b",
          params.call.webRtcProfiles[1] ?? "direct",
          params.call.frequencyHz + 83,
          params.browser,
          params.browserPageUrl,
          params.options,
          params.reporter,
        ),
      );
    }

    runtime.startedAtMs = monotonicMs();
    runtime.endsAtMs = runtime.startedAtMs + params.call.durationMs;
    runtime.graceUntilMs = runtime.startedAtMs + 20_000;
    await params.reporter.event("call.media.established", {
      call_id: params.call.id,
      session_id: runtime.sessionId,
      endpoints: runtime.endpoints.map((endpoint) => ({
        id: endpoint.id,
        kind: endpoint.kind,
        label: endpoint.label,
      })),
    });

    const impairmentTasks = params.call.impairments.map((impairment) =>
      runImpairment({
        runtime,
        impairment,
        reporter: params.reporter,
        fail: params.fail,
        shouldStop: params.shouldStop,
      }),
    );

    for (const mutation of params.call.mutations) {
      const waitMs = runtime.startedAtMs + mutation.atMs - monotonicMs();
      if (waitMs > 0) {
        await sleep(waitMs);
      }
      if (params.shouldStop() || monotonicMs() >= runtime.endsAtMs) {
        break;
      }
      await executeMutation({
        runtime,
        mutation,
        browser: params.browser,
        browserPageUrl: params.browserPageUrl,
        options: params.options,
        holdMusicPath: params.holdMusicPath,
        reporter: params.reporter,
        fail: params.fail,
      });
    }

    const remaining = runtime.endsAtMs - monotonicMs();
    if (remaining > 0) {
      await sleep(remaining);
    }
    await Promise.allSettled(impairmentTasks);
  } finally {
    await destroyCall(runtime, params.reporter);
  }
}

async function createRtpEndpoint(
  runtime: CallRuntime,
  label: string,
  frequencyHz: number,
  reporter: RunReporter,
): Promise<EndpointRuntime> {
  const peer = await RtpPeer.create(`${runtime.plan.id}-${label}`, "127.0.0.1", frequencyHz);
  const offer = peer.makeSdpOffer();
  await reporter.writeSdp(runtime.plan.id, `${label}-rtp-offer`, offer);
  const result = await runtime.control.requestOk<{ endpoint_id: string; sdp_answer: string }>(
    "endpoint.rtp.create_from_offer",
    { sdp: offer, direction: "sendrecv" },
  );
  await reporter.writeSdp(runtime.plan.id, `${label}-rtp-answer`, result.sdp_answer);
  peer.setRemoteFromSdp(result.sdp_answer);
  peer.startReceiving();
  await peer.sendActivationPacket();
  peer.startMediaLoop();
  return {
    id: result.endpoint_id,
    kind: "rtp",
    label,
    control: runtime.control,
    rtpPeer: peer,
  };
}

async function createWebRtcEndpoint(
  runtime: CallRuntime,
  label: string,
  profile: WebRtcProfile,
  frequencyHz: number,
  browser: Browser,
  browserPageUrl: string,
  options: RunnerOptions,
  reporter: RunReporter,
): Promise<EndpointRuntime> {
  const peer = await WebRtcPeer.create({
    browser,
    pageUrl: browserPageUrl,
    label: `${runtime.plan.id}-${label}`,
    profile,
    frequencyHz,
    turn: {
      url: options.turnUrl,
      user: options.turnUser,
      pass: options.turnPass,
    },
  });

  const result = await runtime.control.requestOk<{ endpoint_id: string; sdp_offer: string }>(
    "endpoint.webrtc.create_offer",
    { direction: "sendrecv" },
  );
  await reporter.writeSdp(runtime.plan.id, `${label}-webrtc-offer`, result.sdp_offer);
  const answer = await peer.acceptOffer(result.sdp_offer);
  await reporter.writeSdp(runtime.plan.id, `${label}-webrtc-answer`, answer);
  await runtime.control.requestOk("endpoint.webrtc.accept_answer", {
    endpoint_id: result.endpoint_id,
    sdp: answer,
  });
  await peer.waitConnected(15_000);

  return {
    id: result.endpoint_id,
    kind: "webrtc",
    label,
    control: runtime.control,
    webRtcPeer: peer,
  };
}

async function executeMutation(params: {
  runtime: CallRuntime;
  mutation: MutationPlan;
  browser: Browser;
  browserPageUrl: string;
  options: RunnerOptions;
  holdMusicPath: string;
  reporter: RunReporter;
  fail: (reason: string, detail?: unknown, callId?: string) => Promise<void>;
}): Promise<void> {
  const { runtime, mutation, reporter } = params;
  runtime.graceUntilMs = Math.max(runtime.graceUntilMs, monotonicMs() + mutation.durationMs + 15_000);
  await reporter.event("mutation.start", {
    call_id: runtime.plan.id,
    mutation_id: mutation.id,
    mutation_kind: mutation.kind,
  });

  try {
    if (mutation.kind === "webrtc-ice-restart-bridge") {
      await bridgeInitiatedIceRestart(runtime, reporter);
    } else if (mutation.kind === "webrtc-ice-restart-peer") {
      await peerInitiatedIceRestart(runtime, reporter);
    } else if (mutation.kind === "rtp-reinvite-hold") {
      await rtpReinviteHold(runtime, mutation.durationMs);
    } else if (mutation.kind === "rtp-port-migration") {
      await rtpPortMigration(runtime);
    } else if (mutation.kind === "hold-music") {
      await holdMusic(runtime, mutation.durationMs, params.holdMusicPath, reporter);
    } else if (mutation.kind === "endpoint-transfer") {
      await parkTransfer(runtime, mutation.durationMs, params.holdMusicPath, params.options, reporter);
    } else if (mutation.kind === "endpoint-replace") {
      await replaceEndpoint(runtime, params.browser, params.browserPageUrl, params.options, reporter);
    }
    mutation.completed = true;
    await reporter.event("mutation.complete", {
      call_id: runtime.plan.id,
      mutation_id: mutation.id,
      mutation_kind: mutation.kind,
    });
  } catch (error) {
    mutation.failed = error instanceof Error ? error.message : String(error);
    await params.fail(`mutation ${mutation.kind} failed`, mutation.failed, runtime.plan.id);
    await reporter.event("mutation.failed", {
      call_id: runtime.plan.id,
      mutation_id: mutation.id,
      mutation_kind: mutation.kind,
      error: mutation.failed,
    });
  }
}

async function runImpairment(params: {
  runtime: CallRuntime;
  impairment: MediaImpairmentPlan;
  reporter: RunReporter;
  fail: (reason: string, detail?: unknown, callId?: string) => Promise<void>;
  shouldStop: () => boolean;
}): Promise<void> {
  const { runtime, impairment, reporter } = params;
  const waitMs = runtime.startedAtMs + impairment.atMs - monotonicMs();
  if (waitMs > 0) {
    await sleep(waitMs);
  }
  if (params.shouldStop() || monotonicMs() >= runtime.endsAtMs) {
    return;
  }

  const endpoint = findImpairmentTarget(runtime, impairment);
  if (!endpoint) {
    impairment.failed = `target ${impairment.targetLabel} not found`;
    await params.fail("impairment target not found", impairment, runtime.plan.id);
    return;
  }

  await reporter.event("impairment.start", {
    call_id: runtime.plan.id,
    impairment_id: impairment.id,
    transport: impairment.transport,
    profile: impairment.profile,
    target_label: impairment.targetLabel,
    endpoint_id: endpoint.id,
    duration_ms: impairment.durationMs,
    loss_pct: impairment.lossPct,
    jitter_ms: impairment.jitterMs,
    spike_pct: impairment.spikePct,
    spike_ms: impairment.spikeMs,
    burst_pct: impairment.burstPct,
    max_burst_frames: impairment.maxBurstFrames,
  });

  try {
    const activeMs = Math.min(impairment.durationMs, Math.max(0, runtime.endsAtMs - monotonicMs()));
    if (impairment.transport === "webrtc") {
      if (!endpoint.webRtcPeer) {
        throw new Error("target endpoint is not WebRTC");
      }
      await endpoint.webRtcPeer.applyImpairment(impairment);
      await sleep(activeMs);
      await endpoint.webRtcPeer.clearImpairment(impairment.id);
    } else {
      if (!endpoint.rtpPeer) {
        throw new Error("target endpoint is not RTP");
      }
      endpoint.rtpPeer.applyImpairment(impairment);
      await sleep(activeMs);
      endpoint.rtpPeer.clearImpairment(impairment.id);
    }

    impairment.completed = true;
    await reporter.event("impairment.complete", {
      call_id: runtime.plan.id,
      impairment_id: impairment.id,
      transport: impairment.transport,
      profile: impairment.profile,
      target_label: impairment.targetLabel,
      endpoint_id: endpoint.id,
    });
  } catch (error) {
    impairment.failed = error instanceof Error ? error.message : String(error);
    await params.fail("impairment failed", impairment, runtime.plan.id);
    await reporter.event("impairment.failed", {
      call_id: runtime.plan.id,
      impairment_id: impairment.id,
      transport: impairment.transport,
      profile: impairment.profile,
      target_label: impairment.targetLabel,
      endpoint_id: endpoint.id,
      error: impairment.failed,
    });
  }
}

function findImpairmentTarget(runtime: CallRuntime, impairment: MediaImpairmentPlan): EndpointRuntime | undefined {
  return runtime.endpoints.find((endpoint) => {
    if (endpoint.kind !== impairment.transport) {
      return false;
    }
    return endpoint.label === impairment.targetLabel || endpoint.label.startsWith(`${impairment.targetLabel}-replacement`);
  });
}

async function bridgeInitiatedIceRestart(runtime: CallRuntime, reporter: RunReporter): Promise<void> {
  const endpoint = runtime.endpoints.find((candidate) => candidate.webRtcPeer);
  if (!endpoint?.webRtcPeer) {
    throw new Error("no WebRTC endpoint available");
  }
  const restart = await endpoint.control.requestOk<{ sdp_offer: string; offer_generation: number }>(
    "endpoint.webrtc.ice_restart",
    { endpoint_id: endpoint.id },
  );
  await reporter.writeSdp(runtime.plan.id, `${endpoint.label}-ice-restart-offer`, restart.sdp_offer);
  const answer = await endpoint.webRtcPeer.acceptOffer(restart.sdp_offer);
  await reporter.writeSdp(runtime.plan.id, `${endpoint.label}-ice-restart-answer`, answer);
  await endpoint.control.requestOk("endpoint.webrtc.accept_answer", {
    endpoint_id: endpoint.id,
    sdp: answer,
    offer_generation: restart.offer_generation,
  });
  await endpoint.webRtcPeer.waitConnected(10_000);
}

async function peerInitiatedIceRestart(runtime: CallRuntime, reporter: RunReporter): Promise<void> {
  const endpoint = runtime.endpoints.find((candidate) => candidate.webRtcPeer);
  if (!endpoint?.webRtcPeer) {
    throw new Error("no WebRTC endpoint available");
  }
  const offer = await endpoint.webRtcPeer.createRestartOffer();
  await reporter.writeSdp(runtime.plan.id, `${endpoint.label}-peer-ice-restart-offer`, offer);
  const answer = await endpoint.control.requestOk<{ sdp_answer: string }>(
    "endpoint.webrtc.accept_offer",
    { endpoint_id: endpoint.id, sdp: offer },
  );
  await reporter.writeSdp(runtime.plan.id, `${endpoint.label}-peer-ice-restart-answer`, answer.sdp_answer);
  await endpoint.webRtcPeer.acceptAnswer(answer.sdp_answer);
  await endpoint.webRtcPeer.waitConnected(10_000);
}

async function rtpReinviteHold(runtime: CallRuntime, holdMs: number): Promise<void> {
  const endpoint = runtime.endpoints.find((candidate) => candidate.rtpPeer);
  if (!endpoint?.rtpPeer) {
    throw new Error("no RTP endpoint available");
  }
  await endpoint.control.requestOk("endpoint.rtp.reinvite", {
    endpoint_id: endpoint.id,
    sdp: endpoint.rtpPeer.makeReinviteSdp("sendonly"),
  });
  await sleep(holdMs);
  await endpoint.control.requestOk("endpoint.rtp.reinvite", {
    endpoint_id: endpoint.id,
    sdp: endpoint.rtpPeer.makeReinviteSdp("sendrecv"),
  });
  await endpoint.rtpPeer.sendActivationPacket();
}

async function rtpPortMigration(runtime: CallRuntime): Promise<void> {
  const endpoint = runtime.endpoints.find((candidate) => candidate.rtpPeer);
  if (!endpoint?.rtpPeer) {
    throw new Error("no RTP endpoint available");
  }
  await endpoint.rtpPeer.rebind();
  await endpoint.control.requestOk("endpoint.rtp.reinvite", {
    endpoint_id: endpoint.id,
    sdp: endpoint.rtpPeer.makeReinviteSdp("sendrecv"),
  });
  await endpoint.rtpPeer.sendActivationPacket();
}

async function holdMusic(
  runtime: CallRuntime,
  holdMs: number,
  holdMusicPath: string,
  reporter: RunReporter,
): Promise<void> {
  const held = runtime.endpoints.find((endpoint) => endpoint.kind !== "file");
  if (!held) {
    throw new Error("no endpoint available for hold music");
  }
  const others = runtime.endpoints.filter((endpoint) => endpoint.kind !== "file" && endpoint.id !== held.id);

  await held.control.requestOk("endpoint.update_direction", {
    endpoint_id: held.id,
    direction: "recvonly",
  });
  for (const other of others) {
    await other.control.requestOk("endpoint.update_direction", {
      endpoint_id: other.id,
      direction: "inactive",
    });
  }

  const file = await runtime.control.requestOk<{ endpoint_id: string }>("endpoint.create_with_file", {
    source: holdMusicPath,
    shared: false,
    loop_count: null,
  });
  const fileEndpoint: EndpointRuntime = {
    id: file.endpoint_id,
    kind: "file",
    label: "hold-music",
    control: runtime.control,
  };
  runtime.endpoints.push(fileEndpoint);
  await reporter.event("hold_music.inserted", { call_id: runtime.plan.id, endpoint_id: file.endpoint_id });

  await sleep(holdMs);

  await runtime.control.requestOk("endpoint.remove", { endpoint_id: file.endpoint_id }).catch(() => undefined);
  runtime.endpoints = runtime.endpoints.filter((endpoint) => endpoint.id !== file.endpoint_id);
  for (const endpoint of [held, ...others]) {
    await endpoint.control.requestOk("endpoint.update_direction", {
      endpoint_id: endpoint.id,
      direction: "sendrecv",
    });
    await endpoint.rtpPeer?.sendActivationPacket();
  }
}

async function parkTransfer(
  runtime: CallRuntime,
  holdMs: number,
  holdMusicPath: string,
  options: RunnerOptions,
  reporter: RunReporter,
): Promise<void> {
  const endpoint = runtime.endpoints.find((candidate) => candidate.kind !== "file");
  if (!endpoint) {
    throw new Error("no transferable endpoint available");
  }
  const parking = await connectControl(options.controlUrl ?? runtime.controlUrl, `${runtime.plan.id}-park`);
  const parkingSession = await parking.requestOk<{ session_id: string }>("session.create", {});
  await parking.requestOk("stats.subscribe", {
    interval_ms: options.sampleIntervalMs,
    include_diagnostics: true,
  });
  const file = await parking.requestOk<{ endpoint_id: string }>("endpoint.create_with_file", {
    source: holdMusicPath,
    shared: false,
    loop_count: null,
  });

  try {
    await endpoint.control.requestOk("endpoint.transfer", {
      endpoint_id: endpoint.id,
      target_session_id: parkingSession.session_id,
    });
    endpoint.control = parking;
    await reporter.event("endpoint.parked", {
      call_id: runtime.plan.id,
      endpoint_id: endpoint.id,
      parking_session_id: parkingSession.session_id,
    });
    await sleep(holdMs);
    await parking.requestOk("endpoint.transfer", {
      endpoint_id: endpoint.id,
      target_session_id: runtime.sessionId,
    });
    endpoint.control = runtime.control;
    await endpoint.rtpPeer?.sendActivationPacket();
  } finally {
    await parking.requestOk("endpoint.remove", { endpoint_id: file.endpoint_id }).catch(() => undefined);
    await parking.requestOk("session.destroy", {}).catch(() => undefined);
    await parking.close().catch(() => undefined);
  }
}

async function replaceEndpoint(
  runtime: CallRuntime,
  browser: Browser,
  browserPageUrl: string,
  options: RunnerOptions,
  reporter: RunReporter,
): Promise<void> {
  const index = runtime.endpoints.findIndex((candidate) => candidate.kind !== "file");
  if (index < 0) {
    throw new Error("no endpoint available for replacement");
  }
  const old = runtime.endpoints[index]!;
  runtime.endpoints.splice(index, 1);
  await old.control.requestOk("endpoint.remove", { endpoint_id: old.id }).catch(() => undefined);
  await old.rtpPeer?.close();
  await old.webRtcPeer?.close();

  const replacement =
    old.kind === "rtp"
      ? await createRtpEndpoint(runtime, `${old.label}-replacement`, runtime.plan.frequencyHz + 127, reporter)
      : await createWebRtcEndpoint(
          runtime,
          `${old.label}-replacement`,
          old.webRtcPeer?.profile ?? "direct",
          runtime.plan.frequencyHz + 127,
          browser,
          browserPageUrl,
          options,
          reporter,
        );
  runtime.endpoints.splice(index, 0, replacement);
}

async function runMonitor(params: {
  runtimes: CallRuntime[];
  monitor: MediaMonitor;
  reporter: RunReporter;
  options: RunnerOptions;
  shouldStop: () => boolean;
  fail: (reason: string, detail?: unknown, callId?: string) => Promise<void>;
}): Promise<void> {
  while (!params.shouldStop()) {
    const active = params.runtimes.filter((runtime) => !runtime.destroyed && runtime.startedAtMs > 0);
    for (const runtime of active) {
      let sample;
      try {
        sample = await params.monitor.sampleCall(runtime, monotonicMs());
      } catch (error) {
        await params.fail(
          `monitor sample failed: ${error instanceof Error ? error.message : String(error)}`,
          undefined,
          runtime.plan.id,
        );
        continue;
      }
      await params.reporter.appendBridgeSample(runtime.plan.id, sample);
      await params.reporter.appendRtpSample(
        runtime.plan.id,
        sample.endpoints
          .filter((endpoint) => endpoint.kind === "rtp")
          .map((endpoint) => ({ endpointId: endpoint.endpointId, peer: endpoint.peer })),
      );
      await params.reporter.appendBrowserSample(
        runtime.plan.id,
        sample.endpoints
          .filter((endpoint) => endpoint.kind === "webrtc")
          .map((endpoint) => ({ endpointId: endpoint.endpointId, peer: endpoint.peer })),
      );
    }
    await sleep(params.options.sampleIntervalMs);
  }
}

async function runLoadMonitor(params: {
  reporter: RunReporter;
  options: RunnerOptions;
  rtpbridgePid: () => number | undefined;
  shouldStop: () => boolean;
}): Promise<void> {
  while (!params.shouldStop()) {
    try {
      await params.reporter.appendLoadSample(await sampleLoad(params.rtpbridgePid(), params.options.loadPids));
    } catch (error) {
      await params.reporter.event("load.sample_failed", {
        error: error instanceof Error ? error.message : String(error),
      });
    }
    await sleep(params.options.loadSampleIntervalMs);
  }
}

async function sampleLoad(rtpbridgePid: number | undefined, extraPids: number[]): Promise<LoadSample> {
  const roots = uniqueProcessRoots([
    { pid: process.pid, label: "soak-runner" },
    ...(rtpbridgePid ? [{ pid: rtpbridgePid, label: "rtpbridge" }] : []),
    ...extraPids.map((pid) => ({ pid, label: `pid-${pid}` })),
  ]);
  const processes = await sampleProcessTree(roots);
  return {
    ts: nowIso(),
    mono_ms: monotonicMs(),
    loadavg: os.loadavg(),
    cpu_count: os.cpus().length,
    total_mem_bytes: os.totalmem(),
    free_mem_bytes: os.freemem(),
    process_count: processes.length,
    process_groups: summarizeProcessGroups(processes),
    processes,
  };
}

async function sampleProcessTree(
  roots: Array<{ pid: number; label: string }>,
): Promise<LoadProcessSample[]> {
  if (roots.length === 0) {
    return [];
  }

  const { stdout } = await execFileAsync(
    "ps",
    [
      "-axo",
      "pid=,ppid=,pcpu=,pmem=,rss=,vsz=,stat=,command=",
    ],
    { maxBuffer: 4 * 1024 * 1024 },
  );
  const allProcesses = parsePsOutput(stdout);
  const byPid = new Map(allProcesses.map((sample) => [sample.pid, sample]));
  const children = new Map<number, LoadProcessSample[]>();
  for (const sample of allProcesses) {
    const existing = children.get(sample.ppid);
    if (existing) {
      existing.push(sample);
    } else {
      children.set(sample.ppid, [sample]);
    }
  }

  const selected = new Map<number, LoadProcessSample>();
  for (const root of roots) {
    const rootProcess = byPid.get(root.pid);
    if (!rootProcess) {
      continue;
    }
    const queue = [rootProcess];
    while (queue.length > 0) {
      const processSample = queue.shift()!;
      selected.set(processSample.pid, {
        ...processSample,
        label: processSample.pid === root.pid ? root.label : `${root.label}:child`,
      });
      for (const child of children.get(processSample.pid) ?? []) {
        queue.push(child);
      }
    }
  }

  return [...selected.values()].sort((left, right) => left.pid - right.pid);
}

function parsePsOutput(output: string): LoadProcessSample[] {
  const samples: LoadProcessSample[] = [];
  for (const line of output.split("\n")) {
    const match = line
      .trim()
      .match(/^(\d+)\s+(\d+)\s+([\d.]+)\s+([\d.]+)\s+(\d+)\s+(\d+)\s+(\S+)\s+(.+)$/);
    if (!match) {
      continue;
    }
    samples.push({
      label: "unlabeled",
      pid: Number(match[1]),
      ppid: Number(match[2]),
      cpu_pct: Number(match[3]),
      mem_pct: Number(match[4]),
      rss_kb: Number(match[5]),
      vsz_kb: Number(match[6]),
      state: match[7]!,
      command: match[8]!,
    });
  }
  return samples;
}

function summarizeProcessGroups(processes: LoadProcessSample[]): Array<{
  label: string;
  process_count: number;
  cpu_pct: number;
  rss_kb: number;
}> {
  const byLabel = new Map<string, { label: string; process_count: number; cpu_pct: number; rss_kb: number }>();
  for (const sample of processes) {
    const existing =
      byLabel.get(sample.label) ??
      {
        label: sample.label,
        process_count: 0,
        cpu_pct: 0,
        rss_kb: 0,
      };
    existing.process_count += 1;
    existing.cpu_pct += sample.cpu_pct;
    existing.rss_kb += sample.rss_kb;
    byLabel.set(sample.label, existing);
  }
  return [...byLabel.values()].sort((left, right) => left.label.localeCompare(right.label));
}

function uniqueProcessRoots(roots: Array<{ pid: number; label: string }>): Array<{ pid: number; label: string }> {
  const seen = new Set<number>();
  const unique: Array<{ pid: number; label: string }> = [];
  for (const root of roots) {
    if (!Number.isInteger(root.pid) || root.pid <= 0 || seen.has(root.pid)) {
      continue;
    }
    unique.push(root);
    seen.add(root.pid);
  }
  return unique;
}

async function destroyCall(runtime: CallRuntime, reporter: RunReporter): Promise<void> {
  if (runtime.destroyed) {
    return;
  }
  runtime.destroyed = true;
  await reporter.event("call.destroy.start", { call_id: runtime.plan.id, session_id: runtime.sessionId });
  for (const endpoint of runtime.endpoints) {
    endpoint.rtpPeer?.stopMediaLoop();
  }
  await runtime.control.requestOk("session.destroy", {}).catch(() => undefined);
  for (const endpoint of runtime.endpoints) {
    await endpoint.rtpPeer?.close().catch(() => undefined);
    await endpoint.webRtcPeer?.close().catch(() => undefined);
  }
  await runtime.control.close().catch(() => undefined);
  await reporter.event("call.destroy.complete", { call_id: runtime.plan.id, session_id: runtime.sessionId });
}

async function cleanupRuntimes(runtimes: CallRuntime[], reporter: RunReporter): Promise<void> {
  await Promise.allSettled(runtimes.map((runtime) => destroyCall(runtime, reporter)));
}

async function startBrowserOrigin(): Promise<BrowserOrigin> {
  const host = "127.0.0.1";
  const port = await findFreeTcpPort(host);
  const server = createServer((_request, response) => {
    response.writeHead(200, {
      "cache-control": "no-store",
      "content-type": "text/html; charset=utf-8",
    });
    response.end("<!doctype html><html><head><title>rtpbridge soak50</title></head><body></body></html>");
  });

  await new Promise<void>((resolve, reject) => {
    const onError = (error: Error) => {
      server.off("listening", onListening);
      reject(error);
    };
    const onListening = () => {
      server.off("error", onError);
      resolve();
    };
    server.once("error", onError);
    server.once("listening", onListening);
    server.listen(port, host);
  });

  return { server, url: `http://${host}:${port}/soak50.html` };
}

async function stopBrowserOrigin(server: Server): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    server.close((error) => {
      if (error) {
        reject(error);
      } else {
        resolve();
      }
    });
  });
}

async function startRtpbridge(
  binary: string,
  configPath: string,
  logPath: string,
): Promise<ChildProcess> {
  const log = createWriteStream(logPath, { flags: "a" });
  const child = spawn(binary, ["--config", configPath], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout?.pipe(log);
  child.stderr?.pipe(log);
  child.once("exit", (code, signal) => {
    log.write(`\nrtpbridge exited code=${code} signal=${signal}\n`);
  });
  return child;
}

async function stopRtpbridge(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return;
  }
  child.kill("SIGTERM");
  const exited = await Promise.race([
    new Promise<boolean>((resolve) => child.once("exit", () => resolve(true))),
    sleep(5000).then(() => false),
  ]);
  if (!exited) {
    child.kill("SIGKILL");
  }
}

async function waitForControl(controlUrl: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError: unknown;
  while (Date.now() < deadline) {
    try {
      const client = await connectControl(controlUrl, "preflight", 1000);
      await client.requestOk("server.info", {}, 1000);
      await client.close();
      return;
    } catch (error) {
      lastError = error;
      await sleep(250);
    }
  }
  throw new Error(`control preflight failed: ${lastError instanceof Error ? lastError.message : String(lastError)}`);
}

async function writeMetrics(controlUrl: string, file: string): Promise<void> {
  try {
    const httpUrl = controlUrl.replace(/^ws:/, "http:").replace(/^wss:/, "https:");
    const response = await fetch(`${httpUrl}/metrics`);
    if (!response.ok) {
      return;
    }
    await fs.writeFile(file, await response.text());
  } catch {
    // Metrics are useful but not required for local development.
  }
}

function validateTurnOptions(plan: ScenarioPlan, options: RunnerOptions): void {
  const needsTurn = plan.calls.some((call) => call.webRtcProfiles.includes("relay"));
  if (!needsTurn) {
    return;
  }
  if (!options.turnUrl || !options.turnUser || !options.turnPass) {
    if (options.requireTurn) {
      throw new Error("relay calls require TURN_URL, TURN_USER, and TURN_PASS");
    }
    throw new Error("scenario includes relay calls; pass --require-turn with TURN env vars, or use a seed/call count without relay");
  }
}

function parseArgs(argv: string[]): RunnerOptions {
  const values = new Map<string, string | boolean>();
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i]!;
    if (!arg.startsWith("--")) {
      continue;
    }
    const [rawKey, inline] = arg.slice(2).split("=", 2);
    const key = rawKey!;
    if (inline !== undefined) {
      values.set(key, inline);
    } else if (argv[i + 1] && !argv[i + 1]!.startsWith("--")) {
      values.set(key, argv[++i]!);
    } else {
      values.set(key, true);
    }
  }

  const num = (key: string, fallback: number) => {
    const value = values.get(key);
    if (value === undefined || typeof value === "boolean") {
      return fallback;
    }
    const parsed = Number(value);
    if (!Number.isFinite(parsed)) {
      throw new Error(`invalid numeric --${key}: ${value}`);
    }
    return parsed;
  };
  const str = (key: string, fallback?: string) => {
    const value = values.get(key);
    if (value === undefined || typeof value === "boolean") {
      return fallback;
    }
    return value;
  };
  const bool = (key: string, fallback = false) => {
    const value = values.get(key);
    if (value === undefined) {
      return fallback;
    }
    if (typeof value === "boolean") {
      return value;
    }
    return value === "true" || value === "1";
  };
  const loadSampleIntervalMs = num("load-sample-interval-ms", 2000);
  if (loadSampleIntervalMs <= 0) {
    throw new Error(`invalid numeric --load-sample-interval-ms: ${loadSampleIntervalMs}`);
  }

  return {
    calls: num("calls", 50),
    seed: num("seed", 1234),
    durationScale: num("duration-scale", 1),
    dryRun: bool("dry-run"),
    requireTurn: bool("require-turn"),
    controlUrl: str("control-url"),
    rtpbridgeBin: str("rtpbridge-bin"),
    artifactDir: str("artifact-dir", "artifacts")!,
    mediaIp: str("media-ip", "127.0.0.1")!,
    listenHost: str("listen-host", "127.0.0.1")!,
    rtpPortStart: num("rtp-port-start", 30000),
    rtpPortEnd: num("rtp-port-end", 39999),
    sampleIntervalMs: num("sample-interval-ms", 2000),
    loadSampleIntervalMs,
    loadPids: parsePidList(str("load-pids", "")!),
    startSpreadMs: num("start-spread-ms", 90_000),
    webRtcImpairments: num("webrtc-impairments", -1),
    rtpImpairments: num("rtp-impairments", -1),
    startupTimeoutMs: num("startup-timeout-ms", 15_000),
    logLevel: str("log-level", "info")!,
    turnUrl: str("turn-url", process.env.TURN_URL),
    turnUser: str("turn-user", process.env.TURN_USER),
    turnPass: str("turn-pass", process.env.TURN_PASS),
  };
}

function parsePidList(value: string): number[] {
  if (!value.trim()) {
    return [];
  }
  return value
    .split(",")
    .map((pid) => pid.trim())
    .filter(Boolean)
    .map((pid) => {
      const parsed = Number(pid);
      if (!Number.isInteger(parsed) || parsed <= 0) {
        throw new Error(`invalid --load-pids entry: ${pid}`);
      }
      return parsed;
    });
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
