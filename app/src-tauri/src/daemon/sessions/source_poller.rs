// Polls `claude agents --json` to discover sessions. Mirrors
// daemon/src/sessions/source-poller.js. Output schema is parsed
// defensively — unknown shapes are skipped, never crash.

use crate::daemon::config::{claude_dir, EMPTY_LISTING_GRACE_POLLS, POLL_INTERVAL_MS, RETIRE_AFTER_MISSED_POLLS};
use crate::daemon::log::log;
use crate::daemon::own_sessions::is_own_session;
use crate::daemon::sessions::store::{now_ms, SessionPatch, Store};
use crate::daemon::sessions::tails::{ensure_tail, stop_tail, transcript_path_for};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::time::Duration;

fn pick<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| obj.get(*k).filter(|v| !v.is_null()))
}
fn pick_str(obj: &Value, keys: &[&str]) -> Option<String> {
    pick(obj, keys).and_then(|v| v.as_str()).map(str::to_string)
}
fn basename(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".to_string())
}

// `claude agents --json` reports a coarse lifecycle verdict; map it onto ours.
pub fn apply_agent_state(patch: &mut SessionPatch, verdict: Option<&str>) {
    let now = now_ms();
    match verdict.unwrap_or("").to_lowercase().as_str() {
        "running" | "working" | "busy" => {
            patch.agent_active = Some(true);
            patch.agent_active_ts = Some(now);
        }
        "blocked" | "waiting" => {
            patch.waiting_for_input = Some(true);
            patch.agent_active = Some(false);
        }
        "idle" => {
            patch.agent_active = Some(false);
        }
        "completed" | "stopped" | "failed" => {
            patch.ended = Some(true);
            patch.waiting_for_input = Some(false);
            patch.agent_active = Some(false);
            patch.thinking = Some(false);
        }
        _ => {}
    }
}

// A transcript is what separates a real conversation from a viewport onto a
// background job's transcript — a viewport never writes one.
pub fn is_viewport(item: &Value, id: &str, cwd: &str) -> bool {
    if item.get("kind").and_then(Value::as_str).unwrap_or("") != "interactive" {
        return false;
    }
    match transcript_path_for(id, cwd) {
        Some(p) => !p.exists(),
        None => true,
    }
}

fn resume_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"--resume\s+(\S+?)\.jsonl").unwrap())
}
fn session_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"--session-id\s+(\S+)").unwrap())
}

// The fork link (background job -> the window it forked from) is only on the
// child's command line — read out of `ps`, since the registry JSON has no
// parent field.
pub fn parse_fork_links(ps_output: &str) -> HashMap<String, String> {
    let mut links = HashMap::new();
    for line in ps_output.lines() {
        if !line.contains("--fork-session") {
            continue;
        }
        let Some(parent_caps) = resume_re().captures(line) else {
            continue;
        };
        let parent_path = &parent_caps[1];
        let parent_basename = Path::new(parent_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| parent_path.to_string());
        let child = session_id_re().captures(line).map(|c| c[1].to_string());
        let key = child.unwrap_or_else(|| format!("anon:{}", links.len()));
        links.insert(key, parent_basename);
    }
    links
}

pub fn parse_fork_parents(ps_output: &str) -> HashSet<String> {
    parse_fork_links(ps_output).into_values().collect()
}

pub fn listed_ids(list: &[Value]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for item in list {
        for key in ["session_id", "sessionId", "id"] {
            if let Some(v) = item.get(key).and_then(Value::as_str) {
                ids.insert(v.to_string());
            }
        }
    }
    ids
}

// The registry says a window handed its turn to a job via `parkedJobId`,
// written the moment the job is created — before it shows up in any listing.
pub fn read_parked_windows(dir: &Path) -> HashMap<String, String> {
    let mut parked = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return parked;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(rec) = serde_json::from_str::<Value>(&text) {
                if let (Some(sid), Some(job)) = (
                    rec.get("sessionId").and_then(Value::as_str),
                    rec.get("parkedJobId").and_then(Value::as_str),
                ) {
                    parked.insert(sid.to_string(), job.to_string());
                }
            }
        }
    }
    parked
}

pub fn live_job_ids(list: &[Value]) -> HashSet<String> {
    list.iter().filter_map(|item| pick_str(item, &["jobId", "id"])).collect()
}

// An array is the listing; an object with an agents/sessions array is the
// listing wrapped. Anything else is NOT an empty listing — returning None
// here means "skip this poll", not "nothing is alive".
pub fn parse_listing(stdout: &str) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(stdout).ok()?;
    if let Some(arr) = parsed.as_array() {
        return Some(arr.clone());
    }
    pick(&parsed, &["agents", "sessions"]).and_then(Value::as_array).cloned()
}

pub struct Reconciler {
    store: Store,
    miss_counts: Mutex<HashMap<String, u32>>,
    empty_streak: Mutex<u32>,
}

impl Reconciler {
    pub fn new(store: Store) -> Self {
        Reconciler {
            store,
            miss_counts: Mutex::new(HashMap::new()),
            empty_streak: Mutex::new(0),
        }
    }

    // An orphan that is still working keeps its tile — only once it goes
    // quiet is there nothing to show and nobody to show it to.
    fn orphan_is_working(&self, id: &str, agent_active: bool) -> bool {
        if agent_active {
            return true;
        }
        match self.store.get(id) {
            Some(view) => view.get("state").and_then(Value::as_str) != Some("idle"),
            None => false,
        }
    }

    pub fn reconcile(
        &self,
        list: &[Value],
        parents: &HashSet<String>,
        parked: &HashMap<String, String>,
        fork_links: &HashMap<String, String>,
    ) {
        let count = self.store.count();
        if list.is_empty() && count > 0 {
            let mut streak = self.empty_streak.lock().unwrap();
            *streak += 1;
            if *streak <= EMPTY_LISTING_GRACE_POLLS {
                log(
                    "PL",
                    &format!("empty listing with {count} sessions in store — holding last known good"),
                );
                return;
            }
        } else if !list.is_empty() {
            *self.empty_streak.lock().unwrap() = 0;
        }

        let mut current: HashSet<String> = HashSet::new();
        let jobs = live_job_ids(list);
        let alive = listed_ids(list);

        for item in list {
            let Some(id) = pick_str(item, &["session_id", "sessionId", "id"]) else {
                continue;
            };
            if is_own_session(&id) {
                continue;
            }

            let cwd = pick_str(item, &["cwd", "workingDirectory", "project"]).unwrap_or_default();
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("");

            if cwd.is_empty() && kind == "interactive" {
                current.insert(id);
                continue;
            }
            let hook_transcript_exists = self
                .store
                .raw(&id)
                .and_then(|s| s.transcript_path)
                .map(|p| Path::new(&p).exists())
                .unwrap_or(false);
            if is_viewport(item, &id, &cwd) && !hook_transcript_exists {
                continue;
            }
            if parents.contains(&id) || parked.get(&id).map(|job| jobs.contains(job)).unwrap_or(false) {
                if self.store.raw(&id).is_some() {
                    self.miss_counts.lock().unwrap().remove(&id);
                    stop_tail(&id);
                    self.store.remove(&id);
                    log(
                        "PL",
                        &format!("park {}: its turn belongs to a background job", &id[..id.len().min(8)]),
                    );
                }
                continue;
            }
            current.insert(id.clone());
            let started_at = pick(item, &["startedAt", "startedTs"]).and_then(Value::as_i64);
            let is_new = self.store.raw(&id).is_none();
            let mut patch = SessionPatch {
                name: Some(pick_str(item, &["name", "title"]).unwrap_or_else(|| basename(&cwd))),
                ..Default::default()
            };
            if !cwd.is_empty() {
                patch.cwd = Some(cwd.clone());
            }
            if let Some(model) = pick_str(item, &["model"]) {
                if !model.is_empty() {
                    patch.model = Some(model);
                }
            }
            if let Some(started) = started_at {
                patch.started_ts = Some(started);
            }
            if is_new {
                patch.last_activity_ts = Some(started_at.unwrap_or_else(now_ms));
            }
            let verdict = pick_str(item, &["state", "status"]);
            apply_agent_state(&mut patch, verdict.as_deref());

            if patch.ended == Some(true) {
                log(
                    "PL",
                    &format!(
                        "drop {}: registry verdict {}",
                        &id[..id.len().min(8)],
                        verdict.unwrap_or_else(|| "?".into())
                    ),
                );
                stop_tail(&id);
                self.store.remove(&id);
                continue;
            }

            if let Some(window) = fork_links.get(&id) {
                if !alive.contains(window) && !self.orphan_is_working(&id, patch.agent_active == Some(true)) {
                    current.remove(&id);
                    self.miss_counts.lock().unwrap().remove(&id);
                    stop_tail(&id);
                    self.store.remove(&id);
                    log(
                        "PL",
                        &format!(
                            "orphan {}: window {} is gone and the job is idle",
                            &id[..id.len().min(8)],
                            &window[..window.len().min(8)]
                        ),
                    );
                    continue;
                }
            }

            self.store.upsert(&id, patch);
            ensure_tail(&self.store, &id, transcript_path_for(&id, &cwd));
        }

        let entries = self.store.entries();
        let mut miss_counts = self.miss_counts.lock().unwrap();
        for (id, _) in &entries {
            if current.contains(id) {
                miss_counts.remove(id);
                continue;
            }
            let misses = miss_counts.get(id).copied().unwrap_or(0) + 1;
            if misses < RETIRE_AFTER_MISSED_POLLS {
                miss_counts.insert(id.clone(), misses);
                continue;
            }
            miss_counts.remove(id);
            stop_tail(id);
            self.store.remove(id);
            log("PL", &format!("retire {} after {misses} missed polls", &id[..id.len().min(8)]));
        }
        let stale: Vec<String> = miss_counts
            .keys()
            .filter(|id| self.store.raw(id).is_none())
            .cloned()
            .collect();
        for id in stale {
            miss_counts.remove(&id);
        }
    }
}

async fn poll_once(reconciler: &Arc<Reconciler>, warned: &Arc<AtomicBool>) {
    let output = tokio::time::timeout(
        Duration::from_millis(10_000),
        Command::new("claude")
            .args(["--settings", "{\"disableAllHooks\":true}", "agents", "--json"])
            .output(),
    )
    .await;
    let stdout = match output {
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout).to_string(),
        _ => {
            if !warned.swap(true, Ordering::SeqCst) {
                log("PL", "claude agents --json unavailable — relying on hooks only");
            }
            return;
        }
    };
    let Some(list) = parse_listing(&stdout) else {
        if !warned.swap(true, Ordering::SeqCst) {
            log("PL", "claude agents --json returned an unrecognized shape — skipping poll");
        }
        return;
    };
    warned.store(false, Ordering::SeqCst);

    // Every process, not `ps -p <listed pids>` — see daemon/src/sessions/
    // source-poller.js for why a narrower call is unreliable on macOS.
    let ps_text = Command::new("ps")
        .args(["-A", "-o", "command="])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let fork_links = parse_fork_links(&ps_text);
    let parents = parse_fork_parents(&ps_text);
    let parked = read_parked_windows(&claude_dir().join("sessions"));
    reconciler.reconcile(&list, &parents, &parked, &fork_links);
}

pub fn start(store: Store) -> Arc<Notify> {
    let stop = Arc::new(Notify::new());
    let reconciler = Arc::new(Reconciler::new(store));
    let warned = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    tokio::spawn(async move {
        loop {
            poll_once(&reconciler, &warned).await;
            tokio::select! {
                _ = stop_clone.notified() => break,
                _ = tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)) => {}
            }
        }
    });
    stop
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_state_maps_verdicts() {
        let mut p = SessionPatch::default();
        apply_agent_state(&mut p, Some("busy"));
        assert_eq!(p.agent_active, Some(true));

        let mut p = SessionPatch::default();
        apply_agent_state(&mut p, Some("completed"));
        assert_eq!(p.ended, Some(true));
        assert_eq!(p.thinking, Some(false));

        let mut p = SessionPatch::default();
        apply_agent_state(&mut p, Some("waiting"));
        assert_eq!(p.waiting_for_input, Some(true));
    }

    #[test]
    fn fork_links_parse_resume_and_session_id() {
        let ps = "1234 claude daemon run --fork-session --resume /tmp/proj/abcd1234.jsonl --session-id ef01ef01";
        let links = parse_fork_links(ps);
        assert_eq!(links.get("ef01ef01"), Some(&"abcd1234".to_string()));
        let parents = parse_fork_parents(ps);
        assert!(parents.contains("abcd1234"));
    }

    #[test]
    fn listing_accepts_array_or_wrapped_object() {
        assert_eq!(parse_listing("[1,2]").unwrap().len(), 2);
        assert_eq!(parse_listing(r#"{"agents":[1,2,3]}"#).unwrap().len(), 3);
        assert!(parse_listing(r#"{"error":"nope"}"#).is_none());
        assert!(parse_listing("not json").is_none());
    }

    #[test]
    fn listed_and_live_job_ids() {
        let list = vec![json!({ "session_id": "a", "id": "job-a" }), json!({ "sessionId": "b" })];
        let ids = listed_ids(&list);
        assert!(ids.contains("a") && ids.contains("b"));
        let jobs = live_job_ids(&list);
        assert!(jobs.contains("job-a"));
    }
}
