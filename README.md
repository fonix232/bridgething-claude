# claude-thing

Turn a Spotify Car Thing into a desk monitor for Claude Code: every session at a
glance, a queue of everything waiting on you, live usage bars, and permission
approve/deny from the dial.

<img src="docs/screenshots/sessions.png" alt="The sessions grid: three session tiles showing state, model and context fill, one flagged ATTENTION" width="100%">

<details>
<summary><b>More screens</b> — queue, permission prompt, usage, ambient clock</summary>
<br>
<table>
<tr>
<td width="50%"><img src="docs/screenshots/queue.png" alt="The queue: a permission request card with ALLOW and DENY buttons" width="100%"></td>
<td width="50%"><img src="docs/screenshots/permission.png" alt="A permission request filling the screen, showing the full bash command with allow, deny and skip" width="100%"></td>
</tr>
<tr>
<td><sub><b>Queue</b> — everything waiting on a human, newest filling the screen.</sub></td>
<td><sub><b>Permission</b> — the real prompt. The dial answers it.</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/screenshots/usage.png" alt="The usage screen: session and weekly limit bars with reset times, plus skills and subagents tables" width="100%"></td>
<td width="50%"><img src="docs/screenshots/ambient.png" alt="The ambient clock reading 10:16 PM with NOTHING BLOCKED and a sleeping mascot" width="100%"></td>
</tr>
<tr>
<td><sub><b>Usage</b> — real session and weekly limits, with reset times.</sub></td>
<td><sub><b>Ambient</b> — the clock, when nothing needs you.</sub></td>
</tr>
</table>
</details>

---

## What you'll need

- A **Mac**, and a **Car Thing already running bridgething**.
- A **USB-C cable that carries data** — charge-only cables will not work.
- **Node 18+**, **[bun](https://bun.sh)**, **Rust** (via [rustup](https://rustup.rs)),
  and **Claude Code** installed and signed in.

Check the four:

```sh
node -v && bun --version && rustc --version && claude --version
```

## Install

```sh
git clone <this repo> && cd claude-thing
npm --prefix desktop-app install
node desktop-app/hooks/scripts/install-hooks.js
npm --prefix desktop-app run tauri build
```

`install-hooks.js` merges the Claude Code hooks into `~/.claude/settings.json`
(**a backup is written first, and nothing you already had is removed**) — the
one piece of the old Mac daemon that stayed a small Node script rather than
moving into the app. `tauri build` produces
`desktop-app/src-tauri/target/release/bundle/macos/Claude Thing.app`; move that
to `/Applications` and open it.

The app is a menu-bar item, not a Dock icon or a window — look for it in the
menu bar. It runs its own daemon and reverse SSH tunnel in-process, scanning
for the Car Thing on its own; the menu bar icon is tinted grey (still looking),
red (daemon not up) or green (connected), and has **Open Dashboard**,
**Restart Tunnel**, **View Logs**, **Launch at Login** and **Quit**.

Then plug the Car Thing in over USB and push the app onto it:

```sh
cd bridgething-app && bun install && bun run push
```

The device switches to it immediately. Your sessions should appear.

To undo everything: quit the app, uncheck **Launch at Login** first if it was
checked, delete it from `/Applications`, and re-run
`node desktop-app/hooks/scripts/uninstall-hooks.js` to remove the Claude Code
hooks.

### Or install the app from the catalog

The device half is published as a bridgething catalog source. Add this url in the
companion app and install **Claude** from it:

```
https://github.com/jstgnkl/bridgething-claude/releases/latest/download/catalog.v1.json
```

That always resolves to the newest published release and lists every version
still available, not just the latest — see [Releases](#releases) below. It
replaces `bun run push` only: the Mac app and the hooks still come from the
Install steps above, without them the app has nothing to talk to and shows
`DAEMON OFFLINE`.

## Using it

| Control | What it does |
|---|---|
| **Preset 1** | **Sessions** — every session, its state, model, context fill and an animated mascot. |
| **Preset 2** | **Queue** — everything waiting on a human. The next one fills the screen. |
| **Preset 3** | **Usage** — real session and weekly limits, with reset times. |
| **Preset 4** | Denies the permission on screen. |
| **Dial** | Turn to move, **press to select or allow**. |
| **Back** | Up a level. |
| **M** | Ambient clock. |
| **Touch** | Everything on screen is tappable. |

Two things worth knowing:

**Permissions are answered for real.** Allow/deny from the dial is the actual
decision — the daemon holds Claude Code's permission hook open until you answer.
Multiple-choice questions can't work that way, so answering one focuses that
session's terminal and types the option number instead; that needs macOS
**Automation → System Events**.

**Nothing ever auto-denies.** If the daemon is down, or nobody answers in time,
the prompt goes back to the terminal untouched.

**The hooks are global.** Every Claude Code session on this Mac routes its
permission prompts through the daemon, including whichever one you use to work on
this repo. That is the point, but it does mean your own tooling shows up in the
queue.

## How it works

```
Car Thing kiosk (chromium 800x480, --proxy-server=socks5://127.0.0.1:1080)
  └─ this webapp                   http://127.0.0.1:8891/
       └─ WebSocket ────────────►  ws://127.0.0.1:8790/ws
                                        ▲
                    reverse SSH tunnel over the USB link
                                        │
Mac ── Claude Thing.app, on 127.0.0.1:8790
        ├─ Claude Code hooks (PermissionRequest, PreToolUse, …)
        ├─ claude agents --json, transcripts
        └─ claude -p "/usage"
```

The kiosk's chromium proxies **everything except loopback** through a SOCKS proxy
that nothing here is listening on, so that path is dead. The app opens the
reverse tunnel itself (scanning the USB gadget subnet for the device, no
configuration needed) and puts itself on the device's *own* loopback, where the
kiosk reaches it directly — its `127.0.0.1` bind means the permission API is
never exposed to a network interface. There is no longer a separate daemon
process or LaunchAgent: the app is a single menu-bar binary that owns the
control-page window, the daemon logic, and the tunnel, all in one process.

| Path | What |
|---|---|
| `bridgething-app/` | The device app: `src/` (vanilla ES modules, string-builder screens), `public/`, `test/`, and `scripts/` (push/share/release + the CDP dev tools below). Builds to a bridgething app package. |
| `desktop-app/src-tauri/` | The Rust menu-bar app: daemon logic (sessions, permissions, usage, the WS hub) and the reverse-tunnel supervisor. Compiles for macOS, Windows and Linux. |
| `desktop-app/src/` | The control page, loaded straight into the app's window. Upstream's Bluetooth page removed. |
| `desktop-app/hooks/` | Down to two small Node scripts now — `scripts/install-hooks.js` and `uninstall-hooks.js`, the app's own `/api/hooks/*` still shells out to them. |
| `docs/` | Release artifacts and the `catalog.v1.json` history — stays at the repo root; see [Releases](#releases). |

The app registers with the daemon's hub as `role: "device"`; the hub accepts any
string.

## Developing

```sh
cd bridgething-app
bun install
bun run dev     # vite at 800x480 in any browser — talks to 127.0.0.1:8790 directly
bun run test    # unit tests, no device needed
bun run build
bun run push    # build + install onto the connected device
```

Cutting a release by hand still works (bump `version` in `public/manifest.json`
first) — but ordinarily this is CI's job now, see [Releases](#releases):

```sh
bun run release --changelog "what changed"
```

That writes `docs/claude-thing-bridgething-v<version>.zip` (dist/ at the zip root,
sourcemaps excluded) and folds the version into `docs/catalog.v1.json` with its
size and sha256. Never edit the `id` in `public/manifest.json`: it keys
upgrade-in-place and the device's key-value namespace.

The dev loop needs no device and no tunnel — the daemon is already on the Mac's
loopback. Save the hardware for final checks.

Three tools for driving real hardware, all raw-WebSocket CDP (**Playwright's
`connectOverCDP` hangs against this chromium**):

```sh
node scripts/device.mjs shot.png    # screenshot + banner/route state
node scripts/press.mjs 2            # press a control: 1-4, Enter, Escape, m, dial+, dial-
node scripts/measure.mjs            # assert screen geometry in a real 800x480 Chrome
```

`press.mjs Enter` and `press.mjs 4` **answer real permission requests**.

`measure.mjs` is the one that catches layout bugs the unit tests structurally
cannot — the screens are string builders, so a card that lays out 30px taller
than its container is invisible to them. It needs a headless Chrome; see its
header for the invocation.

## Releases

`.github/workflows/ci.yml` builds and tests both apps on every push/PR to
`main`. `.github/workflows/release.yml` runs on pushes to `main` that touch
`bridgething-app/`, `desktop-app/`, or `docs/catalog.v1.json`, and does the
rest end to end:

1. Bumps the shared patch version (the `VERSION` file at the repo root is the
   source of truth; `.github/scripts/bump-version.mjs` propagates it into both
   apps' manifests) and pushes that commit + a `vX.Y.Z` tag to `main`.
2. Builds `desktop-app` for macOS, Windows and Linux, and cuts a
   `bridgething-app` catalog release the same way `bun run release` does.
3. Publishes everything to one GitHub Release tagged `vX.Y.Z`, including an
   updated `catalog.v1.json` that still lists every previously published
   version — that's the file
   `/releases/latest/download/catalog.v1.json` always resolves to.

Both apps share one version number. A commit that only touches README/docs
(other than `catalog.v1.json`) doesn't trigger a release.

The version-bump and catalog-update steps push straight to `main` with the
repo's default `GITHUB_TOKEN` — if branch protection requires PR review on
`main`, they'll fail without a token that can bypass it. The macOS build is
ad-hoc signed only (no Apple Developer ID configured); Gatekeeper will warn on
a downloaded, quarantined copy until real signing/notarization secrets are
added.

## Troubleshooting

**The device shows the launcher, not the app.** A device reboot can wipe `/var`,
taking the installed webapp with it. Re-push.

**"DAEMON OFFLINE — CHECK THE MAC TUNNEL".** Check the menu-bar icon first: 🔴
means the app's own HTTP server never bound (another process already holds
port 8790 — `lsof -i :8790`); 🟡 means it's up but still scanning for the
device; 🟢 means it's connected. If it's stuck on 🟡, confirm the device sees it
once connected —

```sh
/usr/bin/ssh root@<device-ip> 'wget -q -O - -T 5 http://127.0.0.1:8790/status'
```

The device's address on the USB gadget subnet isn't fixed — the app scans
10.42.1.0/24 for it and keeps rescanning on its own whenever the device is
unplugged, so there is nothing to configure. **Restart Tunnel** in the menu
forces an immediate rescan instead of waiting out the current link.

After a reconnect the banner can take up to 30s to clear — that is the app's
backoff, not a failure.

**The tunnel won't start.** Something else may already hold the device's port
8790; `ExitOnForwardFailure` makes that fail loudly rather than forward nothing.
Quit the app, `lsof -i :8790` to find whatever else is bound, and relaunch.

**After reflashing the device**, its SSH host key changes:
`rm ~/.ssh/known_hosts_carthing`.

## Status

Working on real hardware over the SSH tunnel: the app renders live session data,
the controls all map correctly, the usage screen shows real limits, and a dial
press answers a real permission request end to end.

> A port of [claude-thing](https://github.com/rithkott/claude-thing) to the
> [bridgething](https://github.com/JoeyEamigh/bridgething) Car Thing image.

## License

The launcher icon (`public/icon.svg`) is Anthropic's Claude mark, used to
identify the Claude Code sessions this app monitors. It is Anthropic's
trademark, not covered by this repo's licence.

GPL-3.0 — see [LICENSE](LICENSE), matching
[Nocturne](https://github.com/usenocturne/nocturne) upstream, which
[claude-thing](https://github.com/rithkott/claude-thing) forks and this ports.
The Car Thing platform work, Nocturne, bridgething and claude-thing all belong to
their respective authors; this repo only moves one of them onto another.
