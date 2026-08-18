// Bring a session's terminal window to the front, as if the user clicked it.
// Mirrors daemon/src/focus.js.
//
// Chain: session_id -> ~/.claude/sessions/<pid>.json -> pid -> tty ->
// Terminal.app tab. Terminal.app is the only mainstream emulator that exposes
// `tty` on its tabs, so other emulators fall back to raising the app.
//
// Permissions: raising Terminal needs Automation -> Terminal. Typing a
// keystroke additionally needs Automation -> System Events, which macOS may
// deny. When it is denied we say so and stop — we never work around a denied
// permission.

use crate::daemon::config::claude_dir;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

const KEY_GAP_S: f64 = 0.15;

fn key_code(key: &str) -> Option<u32> {
    match key {
        "return" => Some(36),
        "tab" => Some(48),
        _ => None,
    }
}

fn sessions_dir() -> PathBuf {
    claude_dir().join("sessions")
}

struct OsaResult {
    ok: bool,
    denied: bool,
    out: String,
    error: String,
}

async fn osa(script: &str, timeout_ms: u64) -> OsaResult {
    let fut = Command::new("osascript").arg("-e").arg(script).output();
    match tokio::time::timeout(Duration::from_millis(timeout_ms), fut).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if output.status.success() {
                OsaResult { ok: true, denied: false, out: stdout, error: String::new() }
            } else {
                let denied = stderr.contains("-1743") || stderr.contains("Not authorized");
                let error = stderr.lines().next().unwrap_or("osascript failed").to_string();
                OsaResult { ok: false, denied, out: String::new(), error }
            }
        }
        _ => OsaResult {
            ok: false,
            denied: false,
            out: String::new(),
            error: "osascript timed out".into(),
        },
    }
}

async fn ps(args: &[&str]) -> String {
    match Command::new("ps").args(args).output().await {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

fn read_registry(dir: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str(&text) {
                out.push(v);
            }
        }
    }
    out
}

// session_id -> registry record from Claude Code's own registry.
pub fn lookup_session(session_id: &str, dir: Option<&Path>) -> Option<Value> {
    let owned;
    let dir = match dir {
        Some(d) => d,
        None => {
            owned = sessions_dir();
            &owned
        }
    };
    read_registry(dir)
        .into_iter()
        .find(|rec| rec.get("sessionId").and_then(Value::as_str) == Some(session_id))
}

// A background job is not always windowless — look for the window parked on
// it (see daemon/src/focus.js's comment on `parkedJobId`) and raise that.
pub fn host_window_for(rec: &Value, dir: Option<&Path>) -> Option<Value> {
    let owned;
    let dir = match dir {
        Some(d) => d,
        None => {
            owned = sessions_dir();
            &owned
        }
    };
    let job_id = rec.get("jobId").and_then(Value::as_str).unwrap_or("").to_string();
    let sid = rec.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    if job_id.is_empty() && sid.is_empty() {
        return None;
    }
    read_registry(dir).into_iter().find(|other| {
        if other.get("kind").and_then(Value::as_str) != Some("interactive") {
            return false;
        }
        let parked = match other.get("parkedJobId").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => p,
            _ => return false,
        };
        parked == job_id || parked == sid || (sid.starts_with(parked))
    })
}

async fn tty_for(pid: i64) -> Option<String> {
    let out = ps(&["-o", "tty=", "-p", &pid.to_string()]).await;
    let t = out.trim();
    if t.is_empty() || t == "??" {
        return None;
    }
    Some(if t.starts_with("/dev/") {
        t.to_string()
    } else {
        format!("/dev/{t}")
    })
}

// walk up the process tree to whichever app owns the terminal
async fn owner_app(pid: i64) -> Option<&'static str> {
    let mut current = pid;
    for _ in 0..8 {
        let out = ps(&["-o", "ppid=,comm=", "-p", &current.to_string()]).await;
        if out.is_empty() {
            return None;
        }
        let mut parts = out.splitn(2, char::is_whitespace);
        let ppid_str = parts.next().unwrap_or("").trim();
        let comm = parts.next().unwrap_or("").trim();
        if comm.contains("Terminal.app") {
            return Some("Terminal");
        }
        if comm.contains("iTerm") {
            return Some("iTerm2");
        }
        if comm.contains("Ghostty") {
            return Some("Ghostty");
        }
        if comm.contains("WezTerm") {
            return Some("WezTerm");
        }
        if comm.contains("Code Helper") || comm.contains("Visual Studio Code") {
            return Some("Code");
        }
        let ppid: i64 = ppid_str.parse().unwrap_or(0);
        if ppid <= 1 {
            return None;
        }
        current = ppid;
    }
    None
}

fn terminal_raise_script(tty: &str) -> String {
    format!(
        r#"tell application "Terminal"
  set matched to false
  repeat with w in windows
    repeat with t in tabs of w
      if tty of t is "{tty}" then
        set selected of t to true
        set frontmost of w to true
        set matched to true
      end if
    end repeat
  end repeat
  if matched then activate
  return matched
end tell"#
    )
}

#[derive(Debug, Clone, Default)]
pub struct FocusResult {
    pub focused: bool,
    pub reason: Option<String>,
    pub app: Option<String>,
    pub exact: bool,
    pub tty: Option<String>,
    pub via_host: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TypeResult {
    pub typed: bool,
    pub reason: Option<String>,
}

// There is one keyboard and one frontmost window, so there can be one focus
// (or focus+type) in flight at a time — otherwise two answers arriving close
// together interleave keystrokes into whatever window happens to be in front.
pub struct Focus {
    automation_denied: AtomicBool,
    lock: Mutex<()>,
}

impl Focus {
    pub fn new() -> Arc<Self> {
        Arc::new(Focus {
            automation_denied: AtomicBool::new(false),
            lock: Mutex::new(()),
        })
    }

    pub async fn exclusive<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = self.lock.lock().await;
        f().await
    }

    pub async fn focus_session(&self, session_id: &str) -> FocusResult {
        let Some(rec) = lookup_session(session_id, None) else {
            return FocusResult {
                reason: Some("no session registry entry".into()),
                ..Default::default()
            };
        };

        let kind = rec.get("kind").and_then(Value::as_str).unwrap_or("interactive");
        let (target, via_host) = if !kind.is_empty() && kind != "interactive" {
            match host_window_for(&rec, None) {
                Some(host) => (host, true),
                None => {
                    return FocusResult {
                        reason: Some("background agent — no window to focus".into()),
                        ..Default::default()
                    }
                }
            }
        } else {
            (rec.clone(), false)
        };

        let pid = target.get("pid").and_then(Value::as_i64).unwrap_or(0);
        let Some(tty) = tty_for(pid).await else {
            let reason = if via_host {
                "background agent — parked window is gone"
            } else {
                "session has no tty"
            };
            return FocusResult {
                reason: Some(reason.into()),
                via_host,
                ..Default::default()
            };
        };

        if let Some(app) = owner_app(pid).await {
            if app != "Terminal" {
                let r = osa(&format!(r#"tell application "{app}" to activate"#), 5000).await;
                return if r.ok {
                    FocusResult {
                        focused: true,
                        app: Some(app.to_string()),
                        exact: false,
                        reason: Some(format!("{app} raised (tab targeting unsupported)")),
                        ..Default::default()
                    }
                } else {
                    FocusResult {
                        reason: Some(if r.denied {
                            format!("automation denied for {app}")
                        } else {
                            r.error
                        }),
                        ..Default::default()
                    }
                };
            }
        }

        let r = osa(&terminal_raise_script(&tty), 5000).await;
        if !r.ok {
            if r.denied {
                self.automation_denied.store(true, Ordering::SeqCst);
                return FocusResult {
                    reason: Some(
                        "macOS denied Automation for Terminal — allow it in System Settings › Privacy & Security › Automation"
                            .into(),
                    ),
                    ..Default::default()
                };
            }
            return FocusResult { reason: Some(r.error), ..Default::default() };
        }
        if r.out != "true" {
            return FocusResult {
                reason: Some("no Terminal tab owns that tty".into()),
                ..Default::default()
            };
        }
        let name = target
            .get("name")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| session_id.chars().take(8).collect());
        crate::daemon::log::log(
            "FC",
            &format!("focused {name} ({tty}){}", if via_host { " [parked host]" } else { "" }),
        );
        FocusResult {
            focused: true,
            app: Some("Terminal".into()),
            exact: true,
            tty: Some(tty),
            via_host,
            reason: None,
        }
    }

    // Types a whole sequence in ONE osascript call — a multi-question dialog
    // needs several keys landing in the same window, and re-invoking osascript
    // per key opens a gap where focus can move and half the answer lands
    // somewhere else.
    pub async fn type_sequence(&self, keys: &[String]) -> TypeResult {
        if self.automation_denied.load(Ordering::SeqCst) {
            return TypeResult {
                reason: Some("automation denied".into()),
                ..Default::default()
            };
        }
        let mut lines: Vec<String> = Vec::new();
        for key in keys {
            if !lines.is_empty() {
                lines.push(format!("  delay {KEY_GAP_S}"));
            }
            if let Some(code) = key_code(key) {
                lines.push(format!("  key code {code}"));
                continue;
            }
            let safe: String = key.chars().take(1).collect::<String>().replace(['"', '\\'], "");
            if safe.is_empty() {
                return TypeResult {
                    reason: Some("nothing to type".into()),
                    ..Default::default()
                };
            }
            lines.push(format!("  keystroke \"{safe}\""));
        }
        if lines.is_empty() {
            return TypeResult {
                reason: Some("nothing to type".into()),
                ..Default::default()
            };
        }
        let script = format!("tell application \"System Events\"\n{}\nend tell", lines.join("\n"));
        let timeout_ms = 3000 + keys.len() as u64 * 400;
        let r = osa(&script, timeout_ms).await;
        if r.ok {
            return TypeResult { typed: true, reason: None };
        }
        if r.denied {
            return TypeResult {
                reason: Some(
                    "macOS denied Automation for System Events — allow it in System Settings › Privacy & Security › Automation"
                        .into(),
                ),
                ..Default::default()
            };
        }
        TypeResult { reason: Some(r.error), ..Default::default() }
    }
}
