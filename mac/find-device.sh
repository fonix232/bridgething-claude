#!/usr/bin/env bash
# Finds the connected Car Thing on the USB gadget subnet.
#
# BridgeThing's address within 10.42.1.0/24 depends on DHCP lease order —
# only the subnet is fixed, not the host, and there is no override: this
# always scans. This sweeps the subnet for a host with port 22 open, then
# confirms every hit with a real, non-interactive SSH login as root: a port
# merely being open could be any host, but a successful login as root is the
# device.
#
# Prints the device's IP on stdout and exits 0. Prints nothing and exits 1 if
# nothing on the subnet answered — mac/tunnel.sh calls this in a loop and
# treats that as "not plugged in yet", not an error.
set -uo pipefail

SUBNET="${CLAUDE_THING_SUBNET:-10.42.1}"
KNOWN_HOSTS="$HOME/.ssh/known_hosts_carthing"
PROBE_TIMEOUT="${CLAUDE_THING_PROBE_TIMEOUT:-1}"

hits="$(mktemp)"
trap 'rm -f "$hits"' EXIT

# macOS's nc does not reliably bound connect() by -w — measured at 75s (the
# kernel's own TCP connect timeout) against both a live and a dead address on
# this subnet, -w1 notwithstanding. So the timeout is enforced by hand: kill
# the probe outright if it hasn't returned by then, rather than trust its flag.
probe() {
  local ip="$1"
  nc -z "$ip" 22 2>/dev/null &
  local pid=$!
  ( sleep "$PROBE_TIMEOUT"; kill -9 "$pid" 2>/dev/null ) &
  local watchdog=$!
  if wait "$pid" 2>/dev/null; then
    echo "$ip" >> "$hits"
  fi
  kill "$watchdog" 2>/dev/null
}

# Scanned in batches, not all 254 at once, to keep this a well-behaved
# neighbor to whatever process-per-user limit the machine has.
n=1
while [ "$n" -le 254 ]; do
  batch_end=$((n + 31))
  [ "$batch_end" -gt 254 ] && batch_end=254
  for ((i = n; i <= batch_end; i++)); do
    probe "$SUBNET.$i" &
  done
  wait
  n=$((batch_end + 1))
done

while IFS= read -r ip; do
  if /usr/bin/ssh -o BatchMode=yes -o ConnectTimeout=2 \
        -o StrictHostKeyChecking=accept-new \
        -o UserKnownHostsFile="$KNOWN_HOSTS" \
        "root@$ip" true 2>/dev/null; then
    echo "$ip"
    exit 0
  fi
done < "$hits"

exit 1
