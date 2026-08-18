// Real usage, straight from Claude Code. Mirrors daemon/src/usage.js.
//
// `claude -p "/usage"` runs the slash command locally and prints the same
// figures the interactive /usage view shows. Everything here is parsed from
// that text — never invent a denominator.

use crate::daemon::bus::{emit, Emit};
use crate::daemon::config::USAGE_REFRESH_MS;
use crate::daemon::own_sessions::mark_own_session;
use crate::daemon::persist::{read_state, write_state};
use crate::daemon::sessions::store::now_ms;
use chrono::{Local, TimeZone};
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::process::Command;
use tokio::time::Duration;
use uuid::Uuid;

const RUN_TIMEOUT_MS: u64 = 45_000;
const STATE_NAME: &str = "usage";

fn limit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*Current\s+(session|week[^:]*):\s*(\d+)%\s*used(?:\s*·\s*resets\s+([^(\n]+?))?\s*(?:\(([^)]+)\))?\s*$")
            .unwrap()
    })
}
fn window_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^\s*Last\s+(\S+)\s*·\s*([\d,]+)\s+requests\s*·\s*([\d,]+)\s+sessions\s*$").unwrap())
}
fn top_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^Top\s+(skills|subagents|MCP servers):\s*(.+)$").unwrap())
}
fn top_item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.*\S)\s+(\d+%)$").unwrap())
}
fn week_model_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)week\s*\(([^)]+)\)").unwrap())
}
fn non_alpha_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^a-z]+").unwrap())
}

fn hhmm_local(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

// Trailing percentage per item; anything that doesn't match that shape is
// kept with an empty value rather than dropped.
fn parse_top_list(rest: &str) -> Vec<Value> {
    rest.split(',')
        .filter_map(|chunk| {
            let item = chunk.trim();
            if item.is_empty() {
                return None;
            }
            Some(match top_item_re().captures(item) {
                Some(caps) => json!({ "name": caps[1].to_string(), "pct": caps[2].to_string() }),
                None => json!({ "name": item, "pct": "" }),
            })
        })
        .collect()
}

fn label_for(kind: &str) -> String {
    let k = kind.to_lowercase();
    if k == "session" {
        return "SESSION".to_string();
    }
    if let Some(caps) = week_model_re().captures(kind) {
        let name = caps[1].trim();
        return if name.to_lowercase().contains("all models") {
            "WEEK · ALL MODELS".to_string()
        } else {
            format!("WEEK · {}", name.to_uppercase())
        };
    }
    kind.to_uppercase()
}

pub fn parse_usage(text: &str, now: i64) -> Option<Value> {
    let mut limits: Vec<Value> = Vec::new();
    let mut windows: Vec<Value> = Vec::new();
    let mut current_idx: Option<usize> = None;
    let mut subscription: Option<String> = None;

    for raw in text.split('\n') {
        let line = raw.trim_end();

        if line.to_lowercase().contains("subscription to power your claude code usage") {
            subscription = Some(line.trim().to_string());
            continue;
        }

        if let Some(caps) = limit_re().captures(line) {
            let kind = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let used_pct: f64 = caps.get(2).map(|m| m.as_str()).unwrap_or("0").parse().unwrap_or(0.0);
            let detail = caps
                .get(3)
                .map(|m| format!("resets {}", m.as_str().trim()))
                .unwrap_or_default();
            let key = non_alpha_re().replace_all(&kind.to_lowercase(), "-").to_string();
            limits.push(json!({
                "key": key,
                "label": label_for(kind),
                "used": used_pct / 100.0,
                "detail": detail,
            }));
            continue;
        }

        if let Some(caps) = window_re().captures(line) {
            let requests: i64 = caps[2].replace(',', "").parse().unwrap_or(0);
            let sessions: i64 = caps[3].replace(',', "").parse().unwrap_or(0);
            windows.push(json!({
                "window": format!("Last {}", &caps[1]),
                "requests": requests,
                "sessions": sessions,
                "notes": [], "skills": [], "subagents": [], "mcp": [],
            }));
            current_idx = Some(windows.len() - 1);
            continue;
        }

        // indented bullets under a window: behaviours and top skills/etc.
        if let Some(idx) = current_idx {
            let leading_ws = raw.len() - raw.trim_start().len();
            if leading_ws >= 2 && !line.trim().is_empty() {
                let note = line.trim().to_string();
                if let Some(obj) = windows[idx].as_object_mut() {
                    obj.get_mut("notes").unwrap().as_array_mut().unwrap().push(json!(note));
                    if let Some(caps) = top_re().captures(&note) {
                        let key = match caps[1].to_lowercase().as_str() {
                            "skills" => Some("skills"),
                            "subagents" => Some("subagents"),
                            "mcp servers" => Some("mcp"),
                            _ => None,
                        };
                        if let Some(key) = key {
                            obj.insert(key.to_string(), Value::Array(parse_top_list(&caps[2])));
                        }
                    }
                }
            }
        }
    }

    if limits.is_empty() {
        return None;
    }
    Some(json!({
        "updatedTs": now,
        "updatedLabel": format!("updated {} · from claude /usage", hhmm_local(now)),
        "subscription": subscription,
        "limits": limits,
        "windows": windows,
    }))
}

fn same_window(prev: &Value, next: &Value) -> bool {
    let pd = prev.get("detail").and_then(Value::as_str).unwrap_or("");
    let nd = next.get("detail").and_then(Value::as_str).unwrap_or("");
    if pd.is_empty() || nd.is_empty() {
        return true;
    }
    pd == nd
}

// Usage inside a window only ever goes up. A lower number for the same
// window is a stale read (see daemon/src/usage.js), not a refund.
pub fn reconcile_usage(prev: Option<&Value>, next: Value) -> Value {
    let Some(next_limits) = next.get("limits").and_then(Value::as_array).cloned() else {
        return next;
    };
    let before: std::collections::HashMap<String, Value> = prev
        .and_then(|p| p.get("limits"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("key").and_then(Value::as_str).map(|k| (k.to_string(), l.clone())))
                .collect()
        })
        .unwrap_or_default();

    let limits: Vec<Value> = next_limits
        .into_iter()
        .map(|l| {
            let key = l.get("key").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(p) = before.get(&key) {
                let p_used = p.get("used").and_then(Value::as_f64).unwrap_or(0.0);
                let l_used = l.get("used").and_then(Value::as_f64).unwrap_or(0.0);
                if p_used > l_used && same_window(p, &l) {
                    let mut merged = l.clone();
                    let detail_l = l.get("detail").and_then(Value::as_str).unwrap_or("");
                    let detail_p = p.get("detail").and_then(Value::as_str).unwrap_or("");
                    merged["used"] = json!(p_used);
                    merged["detail"] = json!(if detail_l.is_empty() { detail_p } else { detail_l });
                    return merged;
                }
            }
            l
        })
        .collect();

    let mut out = next;
    out["limits"] = Value::Array(limits);
    out
}

fn describe_failure(timed_out: bool, stderr: &str) -> String {
    if timed_out {
        return format!("timed out after {}s", RUN_TIMEOUT_MS / 1000);
    }
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.to_lowercase().starts_with("warning:"))
        .map(str::to_string)
        .unwrap_or_else(|| "unknown error".to_string())
}

fn load_persisted(state_dir: &Path) -> Option<Value> {
    let saved: Value = read_state(state_dir, STATE_NAME)?;
    let limits = saved.get("limits").and_then(Value::as_array)?;
    if limits.is_empty() {
        return None;
    }
    let at = saved.get("updatedTs").and_then(Value::as_i64).map(hhmm_local).unwrap_or_default();
    let mut out = saved.clone();
    out["stale"] = json!(true);
    if let Some(obj) = out.as_object_mut() {
        obj.remove("error");
    }
    out["updatedLabel"] = json!(format!(
        "last reading{} · from claude /usage",
        if at.is_empty() { String::new() } else { format!(" {at}") }
    ));
    Some(out)
}

struct Inner {
    latest: Option<Value>,
    running: bool,
}

#[derive(Clone)]
pub struct Usage {
    inner: Arc<Mutex<Inner>>,
    emit_tx: Emit,
    state_dir: PathBuf,
}

impl Usage {
    pub fn new(emit_tx: Emit, state_dir: PathBuf) -> Self {
        let latest = load_persisted(&state_dir);
        Usage {
            inner: Arc::new(Mutex::new(Inner { latest, running: false })),
            emit_tx,
            state_dir,
        }
    }

    pub fn get(&self) -> Value {
        self.inner
            .lock()
            .unwrap()
            .latest
            .clone()
            .unwrap_or_else(|| json!({ "limits": [], "error": "usage not read yet" }))
    }

    // No explicit stop(): the refresh loop lives as long as the app process,
    // same as everything else spawned at startup.
    pub fn start(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(USAGE_REFRESH_MS));
            loop {
                ticker.tick().await;
                this.refresh().await;
            }
        });
    }

    async fn run_claude_usage(&self) -> Result<String, String> {
        let session_id = Uuid::new_v4().to_string();
        mark_own_session(session_id.clone());
        let mut cmd = Command::new("claude");
        cmd.args(["--settings", "{\"disableAllHooks\":true}", "--session-id", &session_id, "-p", "/usage"])
            .current_dir(std::env::temp_dir())
            .env("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| e.to_string())?;
        match tokio::time::timeout(Duration::from_millis(RUN_TIMEOUT_MS), child.wait_with_output()).await {
            Ok(Ok(output)) if output.status.success() => Ok(String::from_utf8_lossy(&output.stdout).to_string()),
            Ok(Ok(output)) => Err(describe_failure(false, &String::from_utf8_lossy(&output.stderr))),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(describe_failure(true, "")),
        }
    }

    pub async fn refresh(&self) -> Value {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.running {
                return inner.latest.clone().unwrap_or_else(|| json!({ "limits": [] }));
            }
            inner.running = true;
        }
        let result = self.run_claude_usage().await;
        {
            let mut inner = self.inner.lock().unwrap();
            match result {
                Err(err) => {
                    let mut latest = inner.latest.clone().unwrap_or_else(|| json!({}));
                    latest["stale"] = json!(true);
                    let msg: String = format!("claude /usage failed: {err}").chars().take(120).collect();
                    latest["error"] = json!(msg);
                    inner.latest = Some(latest);
                }
                Ok(text) => match parse_usage(&text, now_ms()) {
                    Some(parsed) => {
                        let reconciled = reconcile_usage(inner.latest.as_ref(), parsed);
                        inner.latest = Some(reconciled.clone());
                        write_state(&self.state_dir, STATE_NAME, &reconciled);
                    }
                    None => {
                        let mut latest = inner.latest.clone().unwrap_or_else(|| json!({}));
                        latest["stale"] = json!(true);
                        latest["error"] = json!("could not parse /usage output");
                        inner.latest = Some(latest);
                    }
                },
            }
            inner.running = false;
        }
        let value = self.inner.lock().unwrap().latest.clone().unwrap_or_else(|| json!({ "limits": [] }));
        emit(&self.emit_tx, "claude.usage.update", value.clone());
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_and_week_limits() {
        let text = concat!(
            "Current session: 11% used · resets Jul 30 at 5:19am (America/New_York)\n",
            "Current week (Sonnet): 42% used\n",
            "Last 24h · 579 requests · 8 sessions\n",
            "  Top skills: /webapp-testing 4%, /frontend-design 1%\n",
        );
        let parsed = parse_usage(text, 0).expect("should parse");
        let limits = parsed["limits"].as_array().unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0]["key"], "session");
        assert!((limits[0]["used"].as_f64().unwrap() - 0.11).abs() < 1e-9);
        assert_eq!(limits[1]["label"], "WEEK · SONNET");
        let windows = parsed["windows"].as_array().unwrap();
        assert_eq!(windows[0]["requests"], 579);
        assert_eq!(windows[0]["skills"][0]["pct"], "4%");
    }

    #[test]
    fn reconcile_keeps_higher_reading_same_window() {
        let prev = json!({ "limits": [{ "key": "session", "used": 0.5, "detail": "resets 5pm" }] });
        let next = json!({ "limits": [{ "key": "session", "used": 0.1, "detail": "resets 5pm" }] });
        let out = reconcile_usage(Some(&prev), next);
        assert!((out["limits"][0]["used"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn reconcile_takes_real_rollover() {
        let prev = json!({ "limits": [{ "key": "session", "used": 0.5, "detail": "resets 5pm" }] });
        let next = json!({ "limits": [{ "key": "session", "used": 0.05, "detail": "resets 9pm" }] });
        let out = reconcile_usage(Some(&prev), next);
        assert!((out["limits"][0]["used"].as_f64().unwrap() - 0.05).abs() < 1e-9);
    }
}
