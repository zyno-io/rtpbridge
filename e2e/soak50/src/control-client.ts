import { EventEmitter } from "node:events";
import WebSocket from "ws";

import type { EndpointStats, StatsEvent } from "./types.js";

interface PendingRequest {
  method: string;
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timeout: NodeJS.Timeout;
}

export interface ControlResponse {
  id: string;
  result?: unknown;
  error?: {
    code?: string;
    message?: string;
    [key: string]: unknown;
  };
}

export class ControlClient extends EventEmitter {
  private ws?: WebSocket;
  private nextId = 1;
  private pending = new Map<string, PendingRequest>();
  private latestStats = new Map<string, EndpointStats>();
  private closed = false;

  constructor(
    private readonly url: string,
    readonly label: string,
  ) {
    super();
  }

  async connect(timeoutMs = 10_000): Promise<void> {
    if (this.ws) {
      return;
    }

    await new Promise<void>((resolve, reject) => {
      const ws = new WebSocket(this.url);
      const timeout = setTimeout(() => {
        ws.close();
        reject(new Error(`control connect timeout for ${this.url}`));
      }, timeoutMs);

      ws.once("open", () => {
        clearTimeout(timeout);
        this.ws = ws;
        this.bindSocket(ws);
        resolve();
      });
      ws.once("error", (error) => {
        clearTimeout(timeout);
        reject(error);
      });
    });
  }

  async request<T = unknown>(
    method: string,
    params: Record<string, unknown> = {},
    timeoutMs = 10_000,
  ): Promise<T> {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error(`control client ${this.label} is not connected`);
    }

    const id = `${this.label}-${this.nextId++}`;
    const message = JSON.stringify({ id, method, params });

    const result = await new Promise<unknown>((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`request timeout: ${method}`));
      }, timeoutMs);

      this.pending.set(id, { method, resolve, reject, timeout });
      this.ws!.send(message, (error) => {
        if (error) {
          clearTimeout(timeout);
          this.pending.delete(id);
          reject(error);
        }
      });
    });

    return result as T;
  }

  async requestOk<T = unknown>(
    method: string,
    params: Record<string, unknown> = {},
    timeoutMs = 10_000,
  ): Promise<T> {
    return this.request<T>(method, params, timeoutMs);
  }

  statsFor(endpointId: string): EndpointStats | undefined {
    return this.latestStats.get(endpointId);
  }

  allStats(): EndpointStats[] {
    return [...this.latestStats.values()];
  }

  async close(): Promise<void> {
    this.closed = true;
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timeout);
      pending.reject(new Error(`control client closed before ${pending.method} completed`));
      this.pending.delete(id);
    }

    if (!this.ws || this.ws.readyState === WebSocket.CLOSED) {
      return;
    }

    await new Promise<void>((resolve) => {
      this.ws!.once("close", () => resolve());
      this.ws!.close();
      setTimeout(resolve, 1000).unref();
    });
  }

  private bindSocket(ws: WebSocket): void {
    ws.on("message", (data) => {
      const text = data.toString();
      let parsed: unknown;
      try {
        parsed = JSON.parse(text);
      } catch (error) {
        this.emit("protocol-error", error);
        return;
      }

      if (!parsed || typeof parsed !== "object") {
        return;
      }

      const obj = parsed as Record<string, unknown>;
      if (typeof obj.event === "string") {
        this.handleEvent(obj);
        return;
      }

      const response = obj as unknown as ControlResponse;
      const id = String(response.id ?? "");
      const pending = this.pending.get(id);
      if (!pending) {
        this.emit("unexpected-response", response);
        return;
      }

      clearTimeout(pending.timeout);
      this.pending.delete(id);

      if (response.error) {
        const code = response.error.code ? `${response.error.code}: ` : "";
        pending.reject(new Error(`${pending.method} failed: ${code}${response.error.message ?? "unknown error"}`));
        return;
      }

      pending.resolve(response.result ?? {});
    });

    ws.on("close", () => {
      if (!this.closed) {
        this.emit("closed");
      }
      for (const [id, pending] of this.pending) {
        clearTimeout(pending.timeout);
        pending.reject(new Error(`control connection closed during ${pending.method}`));
        this.pending.delete(id);
      }
    });

    ws.on("error", (error) => this.emit("ws-error", error));
  }

  private handleEvent(event: Record<string, unknown>): void {
    if (event.event === "stats") {
      const stats = event as unknown as StatsEvent;
      for (const endpoint of stats.data?.endpoints ?? []) {
        this.latestStats.set(endpoint.endpoint_id, endpoint);
      }
    }
    this.emit("event", event);
    this.emit(String(event.event), event);
  }
}

export async function connectControl(url: string, label: string, timeoutMs?: number): Promise<ControlClient> {
  const client = new ControlClient(url, label);
  await client.connect(timeoutMs);
  return client;
}
