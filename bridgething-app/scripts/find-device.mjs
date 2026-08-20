// Finds the connected Car Thing's CDP endpoint on the USB gadget subnet.
//
// Mirrors mac/find-device.sh: BridgeThing's address within 10.42.1.0/24 isn't
// fixed, only the subnet is, so this probes every address for a hit on CDP's
// /json/version. CARTHING_CDP, if set, skips the scan and is used verbatim —
// same override device.mjs and press.mjs have always honored.
const SUBNET = process.env.CARTHING_SUBNET || '10.42.1';
const CDP_PORT = 9222;
const PROBE_TIMEOUT_MS = 300;

async function probe(ip) {
  const timer = AbortSignal.timeout(PROBE_TIMEOUT_MS);
  try {
    const res = await fetch(`http://${ip}:${CDP_PORT}/json/version`, { signal: timer });
    return res.ok ? ip : null;
  } catch {
    return null;
  }
}

export async function findDevice() {
  if (process.env.CARTHING_CDP) return process.env.CARTHING_CDP;

  const ips = Array.from({ length: 254 }, (_, i) => `${SUBNET}.${i + 1}`);
  // One batch of 254 short-timeout fetches — well under a second end to end,
  // unlike the SSH-confirmed scan in mac/find-device.sh.
  const hits = (await Promise.all(ips.map(probe))).filter(Boolean);
  if (hits.length === 0) {
    throw new Error(`no Car Thing found on ${SUBNET}.0/24 (set CARTHING_CDP to skip the scan)`);
  }
  return `http://${hits[0]}:${CDP_PORT}`;
}
