#!/usr/bin/env node

import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const CTX = 's24-hq-staging-k3s-0';
const NS = 'zynotalk-cluster-alpha';
const STS = 'rtpbridge';
const POD = 'rtpbridge-0';
const CONTAINER = 'rtpbridge';
const NODE = 's24-hq-linux0';
const BINARY_DEST = '/tmp/rtpbridge';
const TARGET = 'x86_64-unknown-linux-gnu';
const LOCAL_BINARY = `target/${TARGET}/release/rtpbridge`;
const REMOTE_RECORDINGS = '/var/lib/rtpbridge/recordings/';
const LOCAL_RECORDINGS = 'tmp/';

const children = new Set();
let tearing = false;
let sigintCount = 0;

async function killRemote() {
  const kcExec = (...args) => spawnp('kubectl', [
    '--context', CTX, '-n', NS,
    'exec', POD, '-c', CONTAINER, '--',
    'sh', '-c', ...args,
  ]).catch(() => {});
  await kcExec('kill -INT $(pidof rtpbridge) 2>/dev/null; true');
  await new Promise(r => setTimeout(r, 2000));
  await kcExec('kill -KILL $(pidof rtpbridge) 2>/dev/null; true');
}

function killLocal() {
  for (const p of children) {
    try { process.kill(-p.pid, 'SIGKILL'); } catch {}
  }
  children.clear();
}

function spawnp(cmd, args, opts = {}) {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args, { stdio: 'inherit', detached: true, ...opts });
    children.add(p);
    p.on('exit', (code) => {
      children.delete(p);
      resolve(code ?? 1);
    });
    p.on('error', (err) => {
      children.delete(p);
      reject(err);
    });
  });
}

async function kc(...args) {
  console.log(`  $ kubectl ${args.join(' ')}`);
  const code = await spawnp('kubectl', ['--context', CTX, '-n', NS, ...args]);
  if (code !== 0) throw new Error(`kubectl ${args[0]} failed (exit ${code})`);
}

async function kcSafe(...args) {
  try { await kc(...args); } catch {}
}

/// Check if the pod is Running (not Terminating) and exec-able.
async function isPodUsable() {
  try {
    const code = await spawnp('kubectl', [
      '--context', CTX, '-n', NS,
      'get', 'pod', POD, '-o', 'jsonpath={.metadata.deletionTimestamp}{.status.phase}',
    ], { stdio: ['ignore', 'pipe', 'ignore'] });
    // If deletionTimestamp is set, pod is terminating
    // We can't easily read stdout from spawnp, so just try exec
    if (code !== 0) return false;
  } catch {
    return false;
  }
  try {
    const code = await spawnp('kubectl', [
      '--context', CTX, '-n', NS,
      'exec', POD, '-c', CONTAINER, '--', 'true',
    ], { stdio: 'ignore' });
    return code === 0;
  } catch {
    return false;
  }
}

async function prepare() {
  if (await isPodUsable()) {
    console.log('\n>>> Pod already running, killing old rtpbridge process');
    await killRemote();
    return;
  }

  console.log('\n>>> Preparing cluster');
  await kc('scale', 'statefulset', STS, '--replicas=0');
  await kcSafe('delete', 'pod', POD, '--force', '--grace-period=0');
  await kcSafe('wait', 'pod', POD, '--for=delete', '--timeout=30s');
  await cleanupStaleNetns();
  await kc('patch', 'statefulset', STS, '--type=json', '-p', JSON.stringify([
    { op: 'add', path: '/spec/template/spec/containers/0/command', value: ['sleep', 'infinity'] },
    { op: 'replace', path: '/spec/template/spec/containers/0/args', value: [] },
  ]));
  await kc('scale', 'statefulset', STS, '--replicas=1');
  await kc('wait', 'pod', POD, '--for=condition=Ready', '--timeout=120s');
  console.log('>>> Cluster ready\n');
}

async function build() {
  console.log('\n>>> Building rtpbridge (amd64)');
  const code = await spawnp('cargo', ['zigbuild', '--release', '--target', TARGET, '--features', 'vendored-openssl', '-v'], {
    env: { ...process.env, CMAKE_POLICY_VERSION_MINIMUM: '3.5' },
  });
  if (code !== 0) throw new Error(`build failed (exit ${code})`);
  console.log('>>> Build complete\n');
}

async function deploy() {
  console.log('>>> Copying binary to pod');
  await kc('cp', LOCAL_BINARY, `${POD}:${BINARY_DEST}`, '-c', CONTAINER);
}

async function clearRecordings() {
  console.log('>>> Clearing remote recordings');
  await kc('exec', POD, '-c', CONTAINER, '--', 'sh', '-c', `rm -rf ${REMOTE_RECORDINGS}*`);
}

async function downloadRecordings() {
  console.log('>>> Downloading recordings');
  await spawnp('mkdir', ['-p', LOCAL_RECORDINGS]);
  await kcSafe('cp', `${POD}:${REMOTE_RECORDINGS}`, LOCAL_RECORDINGS, '-c', CONTAINER);
}

/// Clean up stale macvlan namespaces and containers.
async function cleanupStaleNetns() {
  console.log('>>> Cleaning up stale macvlan namespaces on node');
  const scriptDir = dirname(fileURLToPath(import.meta.url));
  const script = readFileSync(join(scriptDir, 'cleanup-stale-netns.sh'), 'utf8');
  const podName = `netns-cleanup-${Date.now()}`;
  const encoded = Buffer.from(script).toString('base64');
  try {
    await kc('run', podName, '--rm', '--attach', '--restart=Never',
      `--image=python:3-slim`,
      `--overrides=${JSON.stringify({
        apiVersion: 'v1',
        spec: {
          nodeName: NODE,
          hostPID: true,
          hostNetwork: true,
          containers: [{
            name: podName,
            image: 'python:3-slim',
            command: ['chroot', '/host', 'bash', '-c', `echo ${encoded} | base64 -d | bash`],
            securityContext: { privileged: true },
            volumeMounts: [{ name: 'host', mountPath: '/host' }],
          }],
          volumes: [{ name: 'host', hostPath: { path: '/' } }],
          restartPolicy: 'Never',
        },
      })}`,
    );
  } catch {
    // Best-effort cleanup
  }
  await kcSafe('delete', 'pod', podName, '--force', '--grace-period=0');
}

async function portForward() {
  console.log('>>> Port-forwarding 9100 -> pod 9100');
  spawnp('kubectl', [
    '--context', CTX, '-n', NS,
    'port-forward', POD, '9100:9100',
  ]);
}

async function exec() {
  console.log('>>> Exec rtpbridge\n');
  return spawnp('kubectl', [
    '--context', CTX, '-n', NS,
    'exec', '-i', POD, '-c', CONTAINER, '--',
    BINARY_DEST, '--config', '/rtpbridge-data/rtpbridge.toml',
    //  '--log-level', 'trace',
  ]);
}

async function teardown() {
  console.log('\n>>> Tearing down');
  await kcSafe('scale', 'statefulset', STS, '--replicas=0');
  await kcSafe('delete', 'pod', POD, '--force', '--grace-period=0');
  await kcSafe('wait', 'pod', POD, '--for=delete', '--timeout=30s');
  await cleanupStaleNetns();
  await kcSafe('patch', 'statefulset', STS, '--type=json', '-p', JSON.stringify([
    { op: 'remove', path: '/spec/template/spec/containers/0/command' },
    { op: 'replace', path: '/spec/template/spec/containers/0/args', value: ['--config', '/rtpbridge-data/rtpbridge.toml'] },
  ]));
  await kcSafe('scale', 'statefulset', STS, '--replicas=1');
  console.log('>>> Teardown complete\n');
}

// Ctrl+C: kill rtpbridge, leave pod running for fast re-deploy
// Ctrl+C twice: force quit
// Ctrl+\: full teardown (restore pod to normal)
async function handleSigint() {
  sigintCount++;
  if (sigintCount >= 2 || tearing) {
    console.error('\nForce quit');
    process.exit(1);
  }
  console.log('\nStopping rtpbridge (Ctrl+C again to force quit, Ctrl+\\ to teardown)...');
  await killRemote();
  killLocal();
  await downloadRecordings();
  process.exit(0);
}

async function handleTeardown() {
  if (tearing) {
    console.error('\nForce quit');
    process.exit(1);
  }
  tearing = true;
  console.log('\nTearing down...');
  await killRemote();
  killLocal();
  await downloadRecordings();
  await teardown();
  process.exit(0);
}

process.on('SIGINT', handleSigint);
process.on('SIGQUIT', handleTeardown);  // Ctrl+backslash
process.on('SIGTERM', handleTeardown);
process.on('exit', killLocal);

try {
  await build();
  await prepare();
  await deploy();
  await clearRecordings();
  await portForward();
  const code = await exec();
  if (!tearing) console.log(`\nrtpbridge exited (code ${code})`);
} catch (err) {
  if (!tearing) console.error(`\nError: ${err.message}`);
} finally {
  if (!tearing) {
    await downloadRecordings();
    // Pod left running for fast re-deploy. Use Ctrl+\ to teardown.
  }
}
