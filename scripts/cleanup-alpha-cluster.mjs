#!/usr/bin/env node

// Clean up the alpha cluster so a normal in-cluster boot works:
//   1. Force-delete the stuck pod (releases holds on macvlan netns)
//   2. Clean up stale macvlan namespaces / containers on the node
//   3. Restore the StatefulSet to its normal command/args (in case a
//      dev-alpha-cluster session was interrupted before teardown)
//   4. Scale back to 1 so the pod comes up clean

import { execSync, spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const CTX = 's24-hq-staging-k3s-0';
const NS = 'zynotalk-cluster-alpha';
const STS = 'rtpbridge';
const POD = 'rtpbridge-0';
const NODE_SELECTOR = 's24.dev/node-type=rtc';

function spawnp(cmd, args, opts = {}) {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args, { stdio: 'inherit', ...opts });
    p.on('exit', (code) => resolve(code ?? 1));
    p.on('error', reject);
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

function rtcNodes() {
  const out = execSync(
    `kubectl --context ${CTX} get nodes -l ${NODE_SELECTOR} -o jsonpath='{.items[*].metadata.name}'`,
    { encoding: 'utf8' }
  ).trim().replace(/^'|'$/g, '');
  return out ? out.split(/\s+/) : [];
}

async function cleanupStaleNetnsOn(node) {
  console.log(`>>> Cleaning up stale macvlan namespaces on ${node}`);
  const scriptDir = dirname(fileURLToPath(import.meta.url));
  const script = readFileSync(join(scriptDir, 'cleanup-stale-netns.sh'), 'utf8');
  const podName = `netns-cleanup-${node.replace(/[^a-z0-9-]/g, '-')}-${Date.now()}`;
  const encoded = Buffer.from(script).toString('base64');
  try {
    await kc('run', podName, '--rm', '--attach', '--restart=Never',
      `--image=python:3-slim`,
      `--overrides=${JSON.stringify({
        apiVersion: 'v1',
        spec: {
          nodeName: node,
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

async function restoreStatefulSet() {
  console.log('>>> Restoring StatefulSet to normal command/args (best-effort)');
  await kcSafe('patch', 'statefulset', STS, '--type=json', '-p', JSON.stringify([
    { op: 'remove', path: '/spec/template/spec/containers/0/command' },
    { op: 'replace', path: '/spec/template/spec/containers/0/args', value: ['--config', '/rtpbridge-data/rtpbridge.toml'] },
  ]));
}

async function main() {
  console.log('\n>>> Cleaning up alpha cluster');
  await kcSafe('scale', 'statefulset', STS, '--replicas=0');
  await kcSafe('delete', 'pod', POD, '--force', '--grace-period=0');
  await kcSafe('wait', 'pod', POD, '--for=delete', '--timeout=30s');
  const nodes = rtcNodes();
  if (nodes.length === 0) throw new Error(`no nodes match selector ${NODE_SELECTOR}`);
  console.log(`>>> Cleaning ${nodes.length} rtc node(s): ${nodes.join(', ')}`);
  for (const node of nodes) {
    await cleanupStaleNetnsOn(node);
  }
  await restoreStatefulSet();
  await kcSafe('scale', 'statefulset', STS, '--replicas=1');
  console.log('\n>>> Cleanup complete\n');
}

main().catch((err) => {
  console.error(`\nError: ${err.message}`);
  process.exit(1);
});
