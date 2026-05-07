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

# --- Remove ALL net1 macvlan interfaces from every net namespace ---
# Enumerate from multiple sources and dedupe by inode. `ip netns list` only
# shows symlinks under /var/run/netns/ and misses (a) the host netns and
# (b) sandbox netns that aren't symlinked there. A stale net1 in any of
# those produces a "address already in use" MAC collision when CNI tries
# to bring up net1 in a new sandbox.

CLEANED=0
declare -A SEEN

remove_net1_from() {
  local path="$1"
  local desc="$2"
  local inode
  inode=$(stat -L -c '%i' "$path" 2>/dev/null) || return 0
  [ -n "${SEEN[$inode]}" ] && return 0
  SEEN[$inode]=1
  if nsenter --net="$path" ip link show net1 >/dev/null 2>&1; then
    local mac
    mac=$(nsenter --net="$path" ip link show net1 2>/dev/null | awk '/link\/ether/ {print $2}')
    echo "Removing net1 ($mac) from $desc"
    nsenter --net="$path" ip link del net1 2>/dev/null || true
    CLEANED=$((CLEANED + 1))
  fi
}

# Bind-mounted CNI netns (canonical CNI location)
for f in /var/run/netns/* /run/netns/*; do
  [ -e "$f" ] || continue
  remove_net1_from "$f" "$(basename "$f")"
done

# Every distinct process netns (catches the host netns and any sandbox
# whose netns isn't symlinked into /var/run/netns/)
for p in /proc/[0-9]*/ns/net; do
  [ -e "$p" ] || continue
  pid=${p#/proc/}
  pid=${pid%/ns/net}
  remove_net1_from "$p" "pid $pid"
done

echo "Cleaned $CLEANED net1 interface(s)"
