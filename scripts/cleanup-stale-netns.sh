#!/bin/bash
# Clean up stale macvlan namespaces, containers, and processes left behind
# by force-deleted pods. Run on the Kubernetes node (not inside a pod).
# Usage: cleanup-stale-netns.sh
set -e

# --- Stop ALL stale rtpbridge/coturn containers in one batch ---

STOPPED=0
# Get the newest Ready sandbox ID
CURRENT_SANDBOX=$(crictl pods --name rtpbridge-0 --state Ready -o json 2>/dev/null \
  | python3 -c "import json,sys; pods=json.load(sys.stdin)['items']; print(sorted(pods, key=lambda p: p['createdAt'], reverse=True)[0]['id'][:13])" 2>/dev/null || true)

# Single python call to find all stale container IDs
STALE_CIDS=$(crictl ps -o json 2>/dev/null | python3 -c "
import json, sys
data = json.load(sys.stdin)
current = '${CURRENT_SANDBOX}'
for c in data.get('containers', []):
    name = c.get('metadata', {}).get('name', '')
    if name not in ('rtpbridge', 'coturn'):
        continue
    sandbox = c.get('podSandboxId', '')[:13]
    if current and sandbox == current:
        continue
    print(c['id'])
" 2>/dev/null || true)

for cid in $STALE_CIDS; do
  echo "Stopping stale container ${cid:0:12}"
  crictl stop "$cid" >/dev/null 2>&1 || true
  STOPPED=$((STOPPED + 1))
done
echo "Stopped $STOPPED stale container(s)"

# --- Remove ALL net1 macvlan interfaces from every namespace ---

CLEANED=0
for ns in $(ip netns list | awk '{print $1}'); do
  has=$(ip netns exec "$ns" ip link show net1 2>/dev/null | grep net1 || true)
  if [ -n "$has" ]; then
    echo "Removing net1 from $ns"
    ip netns exec "$ns" ip link del net1
    CLEANED=$((CLEANED + 1))
  fi
done
echo "Cleaned $CLEANED namespace(s)"
