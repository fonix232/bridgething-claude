// Real usage, straight from Claude Code.
//
// `claude -p "/usage"` runs the slash command locally and prints the same
// figures the interactive /usage view shows — percentages against the actual
// plan limits, reset times, and the "what's contributing" breakdown. It costs
// no model tokens (no inference happens) and, with
// CLAUDE_CODE_SKIP_PROMPT_HISTORY=1, writes no transcript.
//
// Everything here is parsed from that text. We never invent a denominator.

import crypto from 'node:crypto';
import os from 'node:os';
import { execFile } from 'node:child_process';
import { USAGE_REFRESH_MS } from './config.js';
import { markOwnSession } from './own-sessions.js';
import { readState, writeState } from './persist.js';
import { log } from './log.js';

// Where the last good reading is kept between runs.
const STATE_NAME = 'usage';

// A run costs no inference but still boots Claude Code and reads the plan, which
// takes the better part of 20 seconds on a cold start. The old 25s ceiling cut
// slower runs off, and the timeout then reported whatever stderr happened to
// hold — which is how a harmless stdin warning ended up on the usage screen as
// the failure. Well under the 60s refresh, so runs still never overlap.
const RUN_TIMEOUT_MS = 45_000;

// "Current session: 11% used · resets Jul 30 at 5:19am (America/New_York)"
// — the CLI's older format. Kept for backward compatibility; CLI 2.1.x+ uses
// the progress-bar block matched by PROGRESS_BAR_RE below instead.
const LIMIT_RE = /^\s*Current\s+(session|week[^:]*):\s*(\d+)%\s*used(?:\s*·\s*resets\s+([^(\n]+?))?\s*(?:\(([^)]+)\))?\s*$/i;
// "Last 24h · 579 requests · 8 sessions" — older format's window header.
const WINDOW_RE = /^\s*Last\s+(\S+)\s*·\s*([\d,]+)\s+requests\s*·\s*([\d,]+)\s+sessions\s*$/i;
// "Top skills: /webapp-testing 4%, /frontend-design 1%" — older format.
const TOP_RE = /^Top\s+(skills|subagents|MCP servers):\s*(.+)$/i;

// Current CLI: a rendered progress bar (block-drawing chars) ending "NN% used",
// preceded by a heading line ("Claude Code and Cowork credit") and followed by
// a detail line ("One-time credit · Expires September 29"). The heading isn't
// reliable across plan types, so it's read from whatever precedes the bar.
const PROGRESS_BAR_RE = /(\d+)%\s*used\s*$/;
// "Skills                  % of usage" / "Subagents ..." / "MCP servers ..."
const TABLE_HEADER_RE = /^\s*(Skills|Subagents|MCP servers)\s{2,}%\s*of\s*usage\s*$/i;
// "<name>          <NN>%" — at least two spaces before the trailing percentage
// (the columns are padded to align), so a stray prose line can't be mistaken
// for a row — those never end in a bare "NN%" here.
const TABLE_ROW_RE = /^\s*(\S.*?)\s{2,}(\d+)%\s*$/;
// "Showing last-known usage as of 2m ago (rate limited — try again in a moment)"
const STALE_NOTICE_RE = /^\s*Showing\s+last-known\s+usage\s+as\s+of\s+(.+?)\s+ago/i;

// Trailing percentage per item; anything that doesn't match that shape is kept
// with an empty value rather than dropped, so an unparsed entry still shows.
function parseTopList(rest) {
  return String(rest).split(',').map((chunk) => {
    const item = chunk.trim();
    const m = /^(.*\S)\s+(\d+%)$/.exec(item);
    return m ? { name: m[1], pct: m[2] } : { name: item, pct: '' };
  }).filter((x) => x.name);
}

function labelFor(kind) {
  const k = kind.toLowerCase();
  if (k === 'session') return 'SESSION';
  const model = /week\s*\(([^)]+)\)/i.exec(kind);
  if (model) {
    const name = model[1].trim();
    return /all models/i.test(name) ? 'WEEK · ALL MODELS' : `WEEK · ${name.toUpperCase()}`;
  }
  return kind.toUpperCase();
}

export function parseUsage(text, now = Date.now()) {
  const lines = String(text).split('\n').map((l) => l.replace(/\s+$/, ''));
  const rawLines = String(text).split('\n');
  const limits = [];
  const windows = [];
  let current = null;
  let subscription = null;
  let stale = false;
  // Which table a run of rows belongs to — only set right after a table
  // header, cleared on any line that doesn't look like a row (esp. blanks).
  let inTable = null;

  function ensureWindow() {
    if (!current) {
      current = { window: 'Last 24h', requests: 0, sessions: 0, notes: [], skills: [], subagents: [], mcp: [] };
      windows.push(current);
    }
  }

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const raw = rawLines[i];
    if (!line.trim()) { inTable = null; continue; }

    if (/subscription to power your Claude Code usage/i.test(line)) {
      subscription = line.trim();
      continue;
    }

    if (STALE_NOTICE_RE.test(line)) {
      stale = true;
      continue;
    }

    const limit = LIMIT_RE.exec(line);
    if (limit) {
      limits.push({
        key: limit[1].toLowerCase().replace(/[^a-z]+/g, '-'),
        label: labelFor(limit[1]),
        used: Number(limit[2]) / 100,
        detail: limit[3] ? `resets ${limit[3].trim()}` : '',
      });
      continue;
    }

    const win = WINDOW_RE.exec(line);
    if (win) {
      current = {
        window: `Last ${win[1]}`,
        requests: Number(win[2].replace(/,/g, '')),
        sessions: Number(win[3].replace(/,/g, '')),
        notes: [], skills: [], subagents: [], mcp: [],
      };
      windows.push(current);
      continue;
    }

    // Current-format progress-bar block: heading line (previous), the bar
    // itself (this line), optional detail line (next).
    const bar = PROGRESS_BAR_RE.exec(line);
    if (bar) {
      const heading = (i > 0 ? lines[i - 1].trim() : '') || 'Usage';
      const nextLine = i + 1 < lines.length ? lines[i + 1].trim() : '';
      const detail = nextLine && !TABLE_HEADER_RE.test(nextLine) && !PROGRESS_BAR_RE.test(nextLine) && !LIMIT_RE.test(nextLine)
        ? nextLine : '';
      limits.push({
        key: heading.toLowerCase().replace(/[^a-z]+/g, '-').replace(/^-+|-+$/g, ''),
        label: heading.toUpperCase(),
        used: Number(bar[1]) / 100,
        detail,
      });
      continue;
    }

    const tableHeader = TABLE_HEADER_RE.exec(line);
    if (tableHeader) {
      const key = { skills: 'skills', subagents: 'subagents', 'mcp servers': 'mcp' }[tableHeader[1].toLowerCase()];
      if (key) {
        ensureWindow();
        inTable = key;
      }
      continue;
    }

    if (inTable) {
      const row = TABLE_ROW_RE.exec(line);
      if (row) {
        current[inTable].push({ name: row[1].trim(), pct: `${row[2]}%` });
        continue;
      }
      inTable = null;
    }

    // Old-format indented bullets under a window ("  Top skills: a 4%, b 1%").
    if (current && /^\s{2,}\S/.test(raw) && line.trim()) {
      const note = line.trim();
      current.notes.push(note);
      const top = TOP_RE.exec(note);
      if (top) {
        const key = { skills: 'skills', subagents: 'subagents', 'mcp servers': 'mcp' }[top[1].toLowerCase()];
        if (key) current[key] = parseTopList(top[2]);
      }
    }
  }

  if (!limits.length) return null;

  const result = {
    updatedTs: now,
    updatedLabel: 'updated ' + new Date(now).toTimeString().slice(0, 5) + ' · from claude /usage',
    subscription,
    limits,
    windows,
  };
  if (stale) result.stale = true;
  return result;
}

// --- keeping a reading honest across polls ------------------------------------
//
// `claude -p /usage` does not always ask the server. It prints whatever is in
// `cachedUsageUtilization` in ~/.claude.json, and that file is read-modify-
// written wholesale by every claude process on the machine. A long-lived
// session that loaded it an hour ago writes its own stale copy back over the
// fresh one, so consecutive polls a minute apart can disagree — the screen
// flips between the real figure and a stale lower one until something settles.
// Observed live: 1% used, then 0% used, inside the same five-hour window.
//
// The fix is the one fact the reading itself gives us: usage inside a window
// only ever goes up. A lower number for the same window is a stale read, not a
// refund, so the higher one stands. When the window really does roll over the
// reset clause changes with it, and the drop is taken at face value.

function sameWindow(prev, next) {
  // A reading with no reset clause carries no window identity of its own — the
  // zeroed-out shape a clobbered cache prints — so it cannot claim to be a new
  // window. Only a different, stated reset time counts as a rollover.
  if (!prev.detail || !next.detail) return true;
  return prev.detail === next.detail;
}

export function reconcileUsage(prev, next) {
  if (!next || !Array.isArray(next.limits)) return next;
  const before = new Map(((prev && prev.limits) || []).map((l) => [l.key, l]));
  const limits = next.limits.map((l) => {
    const p = before.get(l.key);
    if (!p || !(p.used > l.used) || !sameWindow(p, l)) return l;
    // Held: the previous reading wins, and keeps its reset clause if this one
    // arrived without any.
    return { ...l, used: p.used, detail: l.detail || p.detail };
  });
  return { ...next, limits };
}

// The last good reading, so a restart shows real figures instead of spending a
// minute on "READING USAGE…". Flagged stale until the first live poll lands.
function loadPersisted() {
  const saved = readState(STATE_NAME);
  if (!saved || !Array.isArray(saved.limits) || !saved.limits.length) return null;
  const at = saved.updatedTs ? new Date(saved.updatedTs).toTimeString().slice(0, 5) : '';
  return {
    ...saved,
    stale: true,
    error: undefined,
    updatedLabel: `last reading${at ? ' ' + at : ''} · from claude /usage`,
  };
}

// What actually went wrong, in the words the device has room for. A killed run
// timed out — say that, rather than quoting whichever line stderr happened to
// end on, which is how warnings get mistaken for causes.
export function describeFailure(err, stderr) {
  if (err && (err.killed || err.signal)) {
    return `timed out after ${Math.round(RUN_TIMEOUT_MS / 1000)}s`;
  }
  const line = String(stderr || '')
    .split('\n')
    .map((l) => l.trim())
    .find((l) => l && !/^warning:/i.test(l));
  return line || String((err && err.message) || 'unknown error').split('\n')[0];
}

export function createUsage({ emit }) {
  let latest = loadPersisted();
  let timer = null;
  let running = false;

  function run() {
    // Our own polling is still a Claude Code session. disableAllHooks stops it
    // reporting itself through the hook path, the explicit session id lets the
    // session sources recognise and skip it, and running from a temp dir keeps
    // it out of any project the user actually works in.
    const sessionId = crypto.randomUUID();
    markOwnSession(sessionId);

    return new Promise((resolve) => {
      execFile(
        'claude',
        ['--settings', '{"disableAllHooks":true}', '--session-id', sessionId, '-p', '/usage'],
        {
          timeout: RUN_TIMEOUT_MS,
          cwd: os.tmpdir(),
          env: { ...process.env, CLAUDE_CODE_SKIP_PROMPT_HISTORY: '1' },
          maxBuffer: 1024 * 1024,
          // Closed stdin, not an idle pipe. `claude -p` waits three seconds for
          // input that is never coming, warns about it, and only then starts —
          // three seconds of every run, spent on nothing.
          stdio: ['ignore', 'pipe', 'pipe'],
        },
        (err, stdout, stderr) => {
          if (err) return resolve({ error: describeFailure(err, stderr) });
          resolve({ text: String(stdout) });
        }
      );
    });
  }

  async function refresh() {
    if (running) return latest;
    running = true;
    try {
      const out = await run();
      if (out.error) {
        latest = {
          ...(latest || {}),
          stale: true,
          error: `claude /usage failed: ${out.error}`.slice(0, 120),
        };
      } else {
        const parsed = parseUsage(out.text);
        if (parsed) {
          latest = reconcileUsage(latest, parsed);
          writeState(STATE_NAME, latest);
        } else {
          latest = { ...(latest || {}), stale: true, error: 'could not parse /usage output' };
        }
      }
      emit('claude.usage.update', latest);
    } catch (err) {
      log('US', `usage refresh failed: ${err.message}`);
    } finally {
      running = false;
    }
    return latest;
  }

  return {
    start: () => { refresh(); timer = setInterval(refresh, USAGE_REFRESH_MS); },
    stop: () => clearInterval(timer),
    get: () => latest || { limits: [], error: 'usage not read yet' },
    refresh,
  };
}
