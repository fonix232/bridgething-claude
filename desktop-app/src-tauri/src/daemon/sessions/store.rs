// Merged session state + busy/attention/celebrate/idle machine. Mirrors
// daemon/src/sessions/store.js. Sources call upsert()/touch()/remove(); the
// store derives state and emits debounced snapshots/details on the bus.

use crate::daemon::bus::{emit, Emit};
use crate::daemon::config;
use crate::daemon::context_window::context_fraction;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::Duration;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn local_tz_offset_min() -> i64 {
    -(chrono::Local::now().offset().local_minus_utc() as i64) / 60
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Permission {
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub tool: String,
    pub summary: String,
    pub intent: String,
    #[serde(rename = "createdTs")]
    pub created_ts: i64,
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub name: Option<String>,
    pub started_ts: i64,
    pub last_activity_ts: i64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub context_tokens: u64,
    pub cache_read: u64,
    pub model: String,
    pub effort: Option<String>,
    pub pending_permission: bool,
    pub permission: Option<Permission>,
    pub permission_mode: Option<String>,
    pub ended: bool,
    pub cwd: String,
    pub current_tool: Option<String>,
    pub last_message: String,
    pub waiting_for_input: bool,
    pub thinking: bool,
    pub thinking_ts: i64,
    pub agent_active: bool,
    pub agent_active_ts: i64,
    pub stopped_ts: Option<i64>,
    pub wake_seq: u64,
    pub last_prompt: String,
    pub transcript_path: Option<String>,
}

impl Session {
    fn new(id: String) -> Self {
        let now = now_ms();
        Session {
            id,
            name: None,
            started_ts: now,
            last_activity_ts: now,
            tokens_in: 0,
            tokens_out: 0,
            context_tokens: 0,
            cache_read: 0,
            model: String::new(),
            effort: None,
            pending_permission: false,
            permission: None,
            permission_mode: None,
            ended: false,
            cwd: String::new(),
            current_tool: None,
            last_message: String::new(),
            waiting_for_input: false,
            thinking: false,
            thinking_ts: 0,
            agent_active: false,
            agent_active_ts: 0,
            stopped_ts: None,
            wake_seq: 0,
            last_prompt: String::new(),
            transcript_path: None,
        }
    }
}

/// Fields a source can update in one upsert/touch call. `None` = leave alone.
/// Fields that are nullable in `Session` use `Option<Option<T>>` so a patch can
/// explicitly clear them (mirrors JS's `{ field: null }`).
#[derive(Debug, Clone, Default)]
pub struct SessionPatch {
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub transcript_path: Option<Option<String>>,
    pub permission_mode: Option<String>,
    pub ended: Option<bool>,
    pub started_ts: Option<i64>,
    pub stopped_ts: Option<Option<i64>>,
    pub waiting_for_input: Option<bool>,
    pub thinking: Option<bool>,
    pub current_tool: Option<Option<String>>,
    pub last_message: Option<String>,
    pub last_prompt: Option<String>,
    pub pending_permission: Option<bool>,
    pub permission: Option<Option<Permission>>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_tokens: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cache_read: Option<u64>,
    pub last_activity_ts: Option<i64>,
    pub agent_active: Option<bool>,
    pub agent_active_ts: Option<i64>,
}

impl SessionPatch {
    fn apply(&self, s: &mut Session) {
        if let Some(v) = &self.name {
            s.name = Some(v.clone());
        }
        if let Some(v) = &self.cwd {
            s.cwd = v.clone();
        }
        if let Some(v) = &self.transcript_path {
            s.transcript_path = v.clone();
        }
        if let Some(v) = &self.permission_mode {
            s.permission_mode = Some(v.clone());
        }
        if let Some(v) = self.ended {
            s.ended = v;
        }
        if let Some(v) = self.started_ts {
            s.started_ts = v;
        }
        if let Some(v) = self.stopped_ts {
            s.stopped_ts = v;
        }
        if let Some(v) = self.waiting_for_input {
            s.waiting_for_input = v;
        }
        if let Some(v) = self.thinking {
            s.thinking = v;
            // Every fresh assertion that the session is thinking restamps the
            // clock, not just the false->true edge — the TTL measures silence
            // since the last proof of life, not time since the turn began.
            if v {
                s.thinking_ts = now_ms();
            }
        }
        if let Some(v) = &self.current_tool {
            s.current_tool = v.clone();
        }
        if let Some(v) = &self.last_message {
            s.last_message = v.clone();
        }
        if let Some(v) = &self.last_prompt {
            s.last_prompt = v.clone();
        }
        if let Some(v) = self.pending_permission {
            s.pending_permission = v;
        }
        if let Some(v) = &self.permission {
            s.permission = v.clone();
        }
        if let Some(v) = &self.model {
            s.model = v.clone();
        }
        if let Some(v) = &self.effort {
            s.effort = Some(v.clone());
        }
        if let Some(v) = self.context_tokens {
            s.context_tokens = v;
        }
        if let Some(v) = self.tokens_in {
            s.tokens_in = v;
        }
        if let Some(v) = self.tokens_out {
            s.tokens_out = v;
        }
        if let Some(v) = self.cache_read {
            s.cache_read = v;
        }
        if let Some(v) = self.last_activity_ts {
            s.last_activity_ts = v;
        }
        if let Some(v) = self.agent_active {
            s.agent_active = v;
        }
        if let Some(v) = self.agent_active_ts {
            s.agent_active_ts = v;
        }
    }
}

fn working(s: &Session, now: i64) -> bool {
    if s.ended {
        return false;
    }
    if s.thinking && now - s.thinking_ts < config::THINKING_TTL_MS as i64 {
        return true;
    }
    if s.agent_active && now - s.agent_active_ts < config::AGENT_ACTIVE_TTL_MS as i64 {
        return true;
    }
    now - s.last_activity_ts < config::BUSY_WINDOW_MS as i64
}

fn derive_state(s: &Session) -> &'static str {
    let now = now_ms();
    if s.pending_permission || s.waiting_for_input {
        return "attention";
    }
    if let Some(stopped) = s.stopped_ts {
        if now - stopped < config::CELEBRATE_MS as i64 {
            return "celebrate";
        }
    }
    if working(s, now) {
        return "busy";
    }
    "idle"
}

fn summary(s: &Session) -> Value {
    let name: String = s.name.clone().unwrap_or_else(|| "session".into());
    json!({
        "id": s.id,
        "name": name.chars().take(32).collect::<String>(),
        "state": derive_state(s),
        "lastActivityTs": s.last_activity_ts,
        "tokens": { "in": s.tokens_in, "out": s.tokens_out },
        "context": context_fraction(&s.model, s.context_tokens),
        "pendingPermission": s.pending_permission,
        "permissionMode": s.permission_mode,
        "effort": s.effort,
        "model": s.model,
        "ended": s.ended,
    })
}

fn detail(s: &Session) -> Value {
    let mut v = summary(s);
    let obj = v.as_object_mut().expect("summary() always returns an object");
    obj.insert("contextTokens".into(), json!(s.context_tokens));
    obj.insert("cacheRead".into(), json!(s.cache_read));
    obj.insert("cwd".into(), json!(s.cwd));
    let started = if s.started_ts != 0 {
        s.started_ts
    } else {
        s.last_activity_ts
    };
    obj.insert("startedTs".into(), json!(started));
    obj.insert("currentTool".into(), json!(s.current_tool));
    let last_message: String = s.last_message.chars().take(200).collect();
    obj.insert("lastMessage".into(), json!(last_message));
    obj.insert("permission".into(), json!(s.permission));
    v
}

struct Row {
    view: Value,
    is_idle: bool,
    wake_seq: u64,
    last_activity_ts: i64,
}

struct Inner {
    sessions: HashMap<String, Session>,
    wake_counter: u64,
    last_snapshot_key: String,
    last_snapshot_ts: i64,
    snapshot_scheduled: bool,
    detail_scheduled: HashSet<String>,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
    emit_tx: Emit,
}

impl Store {
    pub fn new(emit_tx: Emit) -> Self {
        let store = Store {
            inner: Arc::new(Mutex::new(Inner {
                sessions: HashMap::new(),
                wake_counter: 0,
                last_snapshot_key: String::new(),
                last_snapshot_ts: 0,
                snapshot_scheduled: false,
                detail_scheduled: HashSet::new(),
            })),
            emit_tx,
        };
        // periodic re-derive so celebrate->idle / busy->idle transitions emit
        let this = store.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            loop {
                ticker.tick().await;
                this.schedule_snapshot();
            }
        });
        store
    }

    pub fn upsert(&self, id: &str, patch: SessionPatch) {
        let ended_now;
        let detail_value;
        {
            let mut inner = self.inner.lock().unwrap();
            let s = inner
                .sessions
                .entry(id.to_string())
                .or_insert_with(|| Session::new(id.to_string()));
            patch.apply(s);
            // An ended session leaves at once rather than sitting out a grace
            // period: it can't be acted on, and a headstone tile reads as a
            // live session.
            if s.ended {
                s.thinking = false;
                s.agent_active = false;
            }
            ended_now = s.ended;
            detail_value = detail(s);
            if ended_now {
                inner.sessions.remove(id);
            }
        }
        if ended_now {
            self.cancel_detail(id);
            self.schedule_snapshot();
            emit(&self.emit_tx, "claude.session.update", detail_value);
        } else {
            self.schedule_snapshot();
            self.schedule_detail(id.to_string());
        }
    }

    pub fn touch(&self, id: &str, mut patch: SessionPatch) {
        patch.last_activity_ts = Some(now_ms());
        self.upsert(id, patch);
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        let inner = self.inner.lock().unwrap();
        inner.sessions.get(id).map(detail)
    }

    pub fn raw(&self, id: &str) -> Option<Session> {
        self.inner.lock().unwrap().sessions.get(id).cloned()
    }

    pub fn remove(&self, id: &str) {
        self.cancel_detail(id);
        let removed = self.inner.lock().unwrap().sessions.remove(id).is_some();
        if removed {
            self.schedule_snapshot();
        }
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().sessions.len()
    }

    pub fn entries(&self) -> Vec<(String, Session)> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn snapshot(&self, limit: Option<usize>) -> Value {
        let mut inner = self.inner.lock().unwrap();
        self.snapshot_locked(&mut inner, limit)
    }

    // Ordering by raw activity alone made the list thrash: a session may only
    // overtake *idle* sessions. Live ones hold the order they woke up in — the
    // mark is taken on the idle->live edge and left alone however much the
    // session works — and idle ones stay newest-first behind them.
    fn snapshot_locked(&self, inner: &mut Inner, limit: Option<usize>) -> Value {
        let ids: Vec<String> = inner.sessions.keys().cloned().collect();
        for id in &ids {
            let is_idle = derive_state(&inner.sessions[id]) == "idle";
            if is_idle {
                inner.sessions.get_mut(id).unwrap().wake_seq = 0;
            } else if inner.sessions[id].wake_seq == 0 {
                inner.wake_counter += 1;
                let seq = inner.wake_counter;
                inner.sessions.get_mut(id).unwrap().wake_seq = seq;
            }
        }

        let mut rows: Vec<Row> = ids
            .iter()
            .map(|id| {
                let s = &inner.sessions[id];
                let view = summary(s);
                Row {
                    is_idle: view["state"] == "idle",
                    wake_seq: s.wake_seq,
                    last_activity_ts: s.last_activity_ts,
                    view,
                }
            })
            .collect();

        rows.sort_by(|a, b| {
            if a.is_idle != b.is_idle {
                return if a.is_idle {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }
            if !a.is_idle {
                a.wake_seq.cmp(&b.wake_seq)
            } else {
                b.last_activity_ts.cmp(&a.last_activity_ts)
            }
        });

        let cap = limit.unwrap_or(config::SESSION_CAP); // 0 = unbounded
        if cap > 0 {
            rows.truncate(cap);
        }
        let list: Vec<Value> = rows.into_iter().map(|r| r.view).collect();
        let active = list.iter().filter(|v| v["state"] == "busy").count();
        let attention = list.iter().filter(|v| v["state"] == "attention").count();

        json!({
            "sessions": list,
            "stats": { "active": active, "attention": attention },
            "serverNowMs": now_ms(),
            "tzOffsetMin": local_tz_offset_min(),
        })
    }

    fn schedule_snapshot(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.snapshot_scheduled {
                return;
            }
            inner.snapshot_scheduled = true;
        }
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(config::SNAPSHOT_DEBOUNCE_MS)).await;
            let (snap, key) = {
                let mut inner = this.inner.lock().unwrap();
                inner.snapshot_scheduled = false;
                let snap = this.snapshot_locked(&mut inner, None);
                let key = json!([snap["sessions"], snap["stats"], snap["tzOffsetMin"]]).to_string();
                (snap, key)
            };
            let now = now_ms();
            let should_emit = {
                let mut inner = this.inner.lock().unwrap();
                if key == inner.last_snapshot_key
                    && now - inner.last_snapshot_ts < config::SNAPSHOT_HEARTBEAT_MS as i64
                {
                    false
                } else {
                    inner.last_snapshot_key = key;
                    inner.last_snapshot_ts = now;
                    true
                }
            };
            if should_emit {
                emit(&this.emit_tx, "claude.sessions.update", snap);
            }
        });
    }

    // One trailing timer per session: however many writes land inside the
    // window, one detail goes out carrying the state as of the emit (rebuilt
    // at fire time, not capture time, so it is never stale).
    fn schedule_detail(&self, id: String) {
        {
            let mut inner = self.inner.lock().unwrap();
            if !inner.detail_scheduled.insert(id.clone()) {
                return;
            }
        }
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(config::DETAIL_DEBOUNCE_MS)).await;
            let still_scheduled = this.inner.lock().unwrap().detail_scheduled.remove(&id);
            if !still_scheduled {
                return; // cancelled (e.g. the session ended before this fired)
            }
            let value = this.inner.lock().unwrap().sessions.get(&id).map(detail);
            if let Some(v) = value {
                emit(&this.emit_tx, "claude.session.update", v);
            }
        });
    }

    fn cancel_detail(&self, id: &str) {
        self.inner.lock().unwrap().detail_scheduled.remove(id);
    }
}
