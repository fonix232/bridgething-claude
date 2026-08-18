// Mirrors daemon/src/config.js. Fixed timing/behavior knobs are plain
// constants; anything that depends on where the app is installed or on the
// user's home directory is a function, resolved at call time.

use std::env;
use std::path::PathBuf;

pub fn port() -> u16 {
    env::var("CLAUDE_THING_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8790)
}

pub const HOST: &str = "127.0.0.1";

pub fn daemon_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn claude_dir() -> PathBuf {
    env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".claude"))
}

pub fn claude_settings() -> PathBuf {
    claude_dir().join("settings.json")
}

pub fn claude_projects_dir() -> PathBuf {
    claude_dir().join("projects")
}

fn home_dir() -> PathBuf {
    // Nothing platform-specific here worth pulling in the `dirs` crate for —
    // this app only ever runs on macOS.
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/".into()))
}

// Permission timing: daemon answers "ask" at HOLD_MS if nobody responded; the
// installed hook timeout must exceed this, or Claude Code gives up first and
// the device is answering into a closed connection. Ten minutes is long enough
// to notice a prompt and walk over to the device; the session is genuinely
// blocked for that whole time, which is the cost of a longer window.
pub fn permission_hold_ms() -> u64 {
    env::var("CLAUDE_THING_HOLD_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(595_000)
}
pub const HOOK_TIMEOUT_S: u64 = 600;

// Snapshots are unbounded — the device grid scrolls sideways through however
// many sessions exist. A client on a constrained transport can still ask for a
// slice via claude.sessions.list {limit}.
pub const SESSION_CAP: usize = 0;
pub const SNAPSHOT_DEBOUNCE_MS: u64 = 500;
// Detail events are per-session and fire on every store write. A streaming
// transcript writes several times a second, and every write used to go out as
// its own claude.session.update — the device full-repaints per frame, so N busy
// sessions multiplied into a render flood that froze the UI. Per-session
// trailing debounce: the latest state still arrives within a blink, the flood
// does not. Ended sessions bypass this — that farewell must not lose a race
// with the record's deletion.
pub const DETAIL_DEBOUNCE_MS: u64 = 200;
// A snapshot whose content hasn't changed still used to go out every 5s (the
// state re-derive tick), repainting every connected screen overnight. Unchanged
// snapshots are skipped, but not forever: one still goes out at this interval
// so the device's clock-skew correction (serverNowMs) stays fresh.
pub const SNAPSHOT_HEARTBEAT_MS: u64 = 30_000;
pub const BUSY_WINDOW_MS: u64 = 10_000; // activity within this = busy
// A finished turn is worth glancing at, and twenty seconds was shorter than the
// time it takes to look up at the device. A minute is long enough to notice.
pub const CELEBRATE_MS: u64 = 60_000; // celebrate decays to idle after this
pub const POLL_INTERVAL_MS: u64 = 3_000; // claude agents --json poll
// One listing is a claim, not a verdict. The registry is rewritten by every
// claude process on the machine — including the daemon's own usage and poll
// invocations — so a single read can catch it mid-write and come back empty or
// partial. Retiring on one such read wiped the whole grid for a poll interval.
// A session must be missing from this many consecutive good polls before it is
// retired (cost: a genuinely-gone session lingers one extra interval)...
pub const RETIRE_AFTER_MISSED_POLLS: u32 = 2;
// ...and a listing that claims *nothing at all* is alive while the store says
// otherwise is suspect enough to ignore entirely this many times in a row
// before its absences start counting.
pub const EMPTY_LISTING_GRACE_POLLS: u32 = 1;
// The registry's "working" verdict is the only thing that stays true while the
// model thinks and nothing is being written. Trust it for a few poll intervals
// so one dropped poll doesn't blink a working session to idle.
pub const AGENT_ACTIVE_TTL_MS: u64 = 3 * POLL_INTERVAL_MS;
// A turn normally ends at the Stop hook. If that never arrives — session killed
// mid-thought, hooks not installed — this stops the tile from claiming to work
// forever. Long enough not to cut off a genuinely slow turn.
pub const THINKING_TTL_MS: u64 = 10 * 60_000;

pub fn mock_sessions() -> bool {
    env::var("CLAUDE_THING_MOCK").as_deref() == Ok("1")
}

// Usage screen: real plan figures read from `claude -p "/usage"` once a minute.
pub const USAGE_REFRESH_MS: u64 = 60_000;
