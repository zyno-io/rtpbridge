import dgram from "node:dgram";
import fs from "node:fs/promises";
import net from "node:net";
import os from "node:os";
import path from "node:path";

export function nowIso(): string {
  return new Date().toISOString();
}

export function monotonicMs(): number {
  return Number(process.hrtime.bigint() / 1_000_000n);
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}

export class SeededRng {
  private state: number;

  constructor(seed: number) {
    this.state = seed >>> 0;
  }

  next(): number {
    let t = (this.state += 0x6d2b79f5);
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  }

  int(min: number, maxInclusive: number): number {
    return Math.floor(this.next() * (maxInclusive - min + 1)) + min;
  }

  pick<T>(items: T[]): T {
    if (items.length === 0) {
      throw new Error("cannot pick from an empty array");
    }
    return items[this.int(0, items.length - 1)]!;
  }

  shuffle<T>(items: T[]): T[] {
    const copy = [...items];
    for (let i = copy.length - 1; i > 0; i -= 1) {
      const j = this.int(0, i);
      [copy[i], copy[j]] = [copy[j]!, copy[i]!];
    }
    return copy;
  }
}

export async function findFreeTcpPort(host = "127.0.0.1"): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, host, () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        server.close();
        reject(new Error("failed to allocate a TCP port"));
        return;
      }
      const port = addr.port;
      server.close(() => resolve(port));
    });
  });
}

export async function ensureDir(dir: string): Promise<void> {
  await fs.mkdir(dir, { recursive: true });
}

export async function createRunDir(root: string, seed: number): Promise<string> {
  const safe = nowIso().replace(/[:.]/g, "-");
  const dir = path.resolve(root, `${safe}-seed-${seed}`);
  await ensureDir(dir);
  await ensureDir(path.join(dir, "sdp"));
  await ensureDir(path.join(dir, "browser-stats"));
  await ensureDir(path.join(dir, "bridge-stats"));
  await ensureDir(path.join(dir, "rtp-peer-stats"));
  return dir;
}

export async function writeJson(file: string, value: unknown): Promise<void> {
  await fs.writeFile(file, `${JSON.stringify(value, null, 2)}\n`);
}

export async function appendJsonl(file: string, value: unknown): Promise<void> {
  await fs.appendFile(file, `${JSON.stringify(value)}\n`);
}

export async function waitForUdpPortAvailable(host: string): Promise<number> {
  const socket = dgram.createSocket(host.includes(":") ? "udp6" : "udp4");
  return new Promise((resolve, reject) => {
    socket.once("error", reject);
    socket.bind(0, host, () => {
      const address = socket.address();
      const port = typeof address === "string" ? 0 : address.port;
      socket.close(() => resolve(port));
    });
  });
}

export async function writeRtpbridgeConfig(params: {
  dir: string;
  listen: string;
  mediaIp: string;
  mediaDir: string;
  cacheDir: string;
  recordingDir: string;
  rtpPortStart: number;
  rtpPortEnd: number;
  logLevel: string;
}): Promise<string> {
  const configPath = path.join(params.dir, "rtpbridge-soak.toml");
  const toml = [
    `listen = "${params.listen}"`,
    `media_ip = "${params.mediaIp}"`,
    `rtp_port_range = [${params.rtpPortStart}, ${params.rtpPortEnd}]`,
    "disconnect_timeout_secs = 30",
    "shutdown_max_wait_secs = 30",
    `media_dir = "${escapeTomlPath(params.mediaDir)}"`,
    `cache_dir = "${escapeTomlPath(params.cacheDir)}"`,
    `recording_dir = "${escapeTomlPath(params.recordingDir)}"`,
    "cache_cleanup_interval_secs = 300",
    "max_concurrent_downloads = 16",
    "max_sessions = 200",
    "max_endpoints_per_session = 20",
    "max_recordings_per_session = 100",
    "recording_flush_timeout_secs = 10",
    "ws_max_message_size_kb = 256",
    "max_sdp_size_kb = 64",
    "session_idle_timeout_secs = 0",
    "empty_session_timeout_secs = 0",
    "media_timeout_secs = 5",
    "max_connections = 1000",
    "ws_ping_interval_secs = 30",
    "event_channel_size = 2048",
    "critical_event_channel_size = 256",
    "transcode_cache_size = 256",
    "max_file_download_bytes = 104857600",
    "max_recording_download_bytes = 536870912",
    "recording_channel_size = 2000",
    `log_level = "${params.logLevel}"`,
    "",
  ].join("\n");
  await fs.writeFile(configPath, toml);
  return configPath;
}

function escapeTomlPath(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

export async function makeTempRoot(prefix = "rtpbridge-soak50-"): Promise<string> {
  return fs.mkdtemp(path.join(os.tmpdir(), prefix));
}

export async function ensureHoldMusicWav(file: string): Promise<void> {
  try {
    await fs.access(file);
    return;
  } catch {
    // Continue and create it.
  }

  await ensureDir(path.dirname(file));
  const sampleRate = 8000;
  const seconds = 4;
  const samples = sampleRate * seconds;
  const data = Buffer.alloc(samples * 2);

  for (let i = 0; i < samples; i += 1) {
    const t = i / sampleRate;
    const sample =
      Math.sin(2 * Math.PI * 440 * t) * 9000 +
      Math.sin(2 * Math.PI * 660 * t) * 3500;
    data.writeInt16LE(Math.max(-32768, Math.min(32767, Math.round(sample))), i * 2);
  }

  const header = Buffer.alloc(44);
  header.write("RIFF", 0);
  header.writeUInt32LE(36 + data.length, 4);
  header.write("WAVE", 8);
  header.write("fmt ", 12);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20);
  header.writeUInt16LE(1, 22);
  header.writeUInt32LE(sampleRate, 24);
  header.writeUInt32LE(sampleRate * 2, 28);
  header.writeUInt16LE(2, 32);
  header.writeUInt16LE(16, 34);
  header.write("data", 36);
  header.writeUInt32LE(data.length, 40);
  await fs.writeFile(file, Buffer.concat([header, data]));
}

export function scaleDuration(ms: number, scale: number): number {
  return Math.max(1000, Math.round(ms * scale));
}

