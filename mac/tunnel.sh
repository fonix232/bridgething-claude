#!/usr/bin/env bash
# Puts the Mac daemon on the Car Thing's loopback.
#
# The kiosk chromium runs --proxy-server=socks5://127.0.0.1:1080 with no bypass
# list, so anything but loopback is pushed into a SOCKS proxy that nothing is
# listening on — that path is dead here. A reverse tunnel puts the daemon at
# 127.0.0.1:8790 ON THE DEVICE, which the kiosk reaches directly — and the
# daemon keeps its 127.0.0.1 bind, so the permission API is never exposed to a
# network interface.
#
# Reverse, not forward: the Mac holds passwordless root SSH to the device; the
# device holds no credentials for the Mac.
#
# The device's address on the USB gadget subnet isn't fixed, so this runs its
# own find-connect-rescan loop rather than assuming one or being told one:
# no device found yet, rescan after a short pause; found, open the tunnel and
# block until that connection drops (unplugged, rebooted, sshd killed), then
# go back to scanning. It does not exit in normal operation — launchd
# (com.claudething.tunnel, KeepAlive) is only the backstop for a real crash.
set -uo pipefail
cd "$(dirname "$0")"

PORT="${CLAUDE_THING_PORT:-8790}"
RESCAN_DELAY_S="${CLAUDE_THING_RESCAN_DELAY:-3}"

# Forwarded to the ssh child so unloading the LaunchAgent (or a manual kill)
# stops the tunnel immediately instead of leaving it running as an orphan.
ssh_pid=""
cleanup() {
  [ -n "$ssh_pid" ] && kill "$ssh_pid" 2>/dev/null
  exit 0
}
trap cleanup TERM INT

while true; do
  DEVICE="$(./find-device.sh)" || { sleep "$RESCAN_DELAY_S"; continue; }

  # NOT plain `ssh` — that is aliased to Kitty's ssh kitten and breaks here.
  /usr/bin/ssh -N \
    -R "${PORT}:127.0.0.1:${PORT}" \
    -o ExitOnForwardFailure=yes \
    -o ServerAliveInterval=15 \
    -o ServerAliveCountMax=3 \
    -o ConnectTimeout=10 \
    -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile="$HOME/.ssh/known_hosts_carthing" \
    "root@${DEVICE}" &
  ssh_pid=$!
  wait "$ssh_pid"
  ssh_pid=""

  sleep "$RESCAN_DELAY_S"
done
