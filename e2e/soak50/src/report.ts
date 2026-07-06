import fs from "node:fs/promises";
import path from "node:path";

import type { CallPlan, CallRuntime, Failure, ScenarioPlan, TimelineEvent } from "./types.js";
import { appendJsonl, monotonicMs, nowIso, writeJson } from "./utils.js";

export class RunReporter {
  readonly timelineFile: string;

  constructor(readonly runDir: string) {
    this.timelineFile = path.join(runDir, "timeline.jsonl");
  }

  async writePlan(plan: ScenarioPlan): Promise<void> {
    await writeJson(path.join(this.runDir, "call-matrix.json"), plan);
  }

  async event(type: string, fields: Omit<TimelineEvent, "ts" | "mono_ms" | "type"> = {}): Promise<void> {
    await appendJsonl(this.timelineFile, {
      ts: nowIso(),
      mono_ms: monotonicMs(),
      type,
      ...fields,
    });
  }

  async writeSdp(callId: string, name: string, sdp: string): Promise<void> {
    const file = path.join(this.runDir, "sdp", `${callId}-${name}.sdp`);
    await fs.writeFile(file, sdp);
  }

  async appendBridgeSample(callId: string, sample: unknown): Promise<void> {
    await appendJsonl(path.join(this.runDir, "bridge-stats", `${callId}.jsonl`), sample);
  }

  async appendBrowserSample(callId: string, sample: unknown): Promise<void> {
    await appendJsonl(path.join(this.runDir, "browser-stats", `${callId}.jsonl`), sample);
  }

  async appendRtpSample(callId: string, sample: unknown): Promise<void> {
    await appendJsonl(path.join(this.runDir, "rtp-peer-stats", `${callId}.jsonl`), sample);
  }

  async writeSummary(params: {
    plan: ScenarioPlan;
    runtimes: CallRuntime[];
    failures: Failure[];
    maxFlatlineMs: number;
    startedAt: string;
    finishedAt: string;
  }): Promise<void> {
    const byKind = countBy(params.plan.calls, (call) => call.kind);
    const mutationCounts = countBy(
      params.plan.calls.flatMap((call) => call.mutations),
      (mutation) => mutation.kind,
    );
    const impairmentCounts = countBy(
      params.plan.calls.flatMap((call) => call.impairments),
      (impairment) => `${impairment.transport}:${impairment.profile}`,
    );
    const longCalls = params.plan.calls.filter((call) => call.durationMs >= 10 * 60_000).length;

    await writeJson(path.join(this.runDir, "summary.json"), {
      started_at: params.startedAt,
      finished_at: params.finishedAt,
      seed: params.plan.seed,
      total_calls: params.plan.calls.length,
      call_type_counts: byKind,
      mutation_counts: mutationCounts,
      impairment_counts: impairmentCounts,
      long_calls: longCalls,
      failures: params.failures,
      max_flatline_ms: params.maxFlatlineMs,
      calls: params.plan.calls.map((call) => summarizeCall(call, params.failures)),
      sessions: params.runtimes.map((runtime) => ({
        call_id: runtime.plan.id,
        session_id: runtime.sessionId,
        endpoint_count: runtime.endpoints.length,
        destroyed: runtime.destroyed,
      })),
      verdict: params.failures.length === 0 ? "pass" : "fail",
    });
  }
}

function summarizeCall(call: CallPlan, failures: Failure[]): unknown {
  return {
    id: call.id,
    kind: call.kind,
    duration_ms: call.durationMs,
    web_rtc_profiles: call.webRtcProfiles,
    mutations: call.mutations,
    impairments: call.impairments,
    verdict: failures.some((failure) => failure.callId === call.id) ? "fail" : "pass",
  };
}

function countBy<T>(items: T[], key: (item: T) => string): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const item of items) {
    const value = key(item);
    counts[value] = (counts[value] ?? 0) + 1;
  }
  return counts;
}
