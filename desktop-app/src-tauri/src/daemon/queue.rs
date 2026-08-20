// Everything waiting on a human that a hook cannot answer: AskUserQuestion
// dialogs and ExitPlanMode plan approval, both answered by focusing the
// terminal and typing keys. Mirrors daemon/src/queue.js.

use crate::daemon::bus::{emit, Emit};
use crate::daemon::focus::Focus;
use crate::daemon::log::log;
use crate::daemon::sessions::store::{now_ms, Store};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::{Arc, Mutex};
use tokio::time::Duration;

fn question_ttl_ms() -> u64 {
    env::var("CLAUDE_THING_QUESTION_TTL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10 * 60_000)
}
const MAX_EXPIRED: usize = 32;

#[derive(Debug, Clone, Serialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShapedQuestion {
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(rename = "multiSelect")]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ask {
    pub kind: &'static str,
    pub id: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(rename = "sessionName")]
    pub session_name: String,
    pub intent: String,
    pub questions: Vec<ShapedQuestion>,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(rename = "multiSelect")]
    pub multi_select: bool,
    #[serde(rename = "createdTs")]
    pub created_ts: i64,
}

fn shape_question(q: &Value) -> ShapedQuestion {
    let options = q
        .get("options")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .take(8)
                .map(|o| QuestionOption {
                    label: o
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect(),
                    description: o
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .collect(),
                })
                .collect()
        })
        .unwrap_or_default();
    ShapedQuestion {
        header: q
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("QUESTION")
            .to_uppercase(),
        question: q
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(300)
            .collect(),
        options,
        multi_select: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
    }
}

// The keys Claude Code's question dialog takes — see daemon/src/queue.js for
// the full rationale (read out of the CLI itself, not guessed).
pub fn key_sequence(questions: &[ShapedQuestion], answers: &[Vec<usize>]) -> Vec<String> {
    let mut keys = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        if let Some(picks) = answers.get(i) {
            for pick in picks {
                keys.push((pick + 1).to_string());
            }
        }
        if q.multi_select {
            keys.push("tab".to_string());
        }
    }
    if questions.len() > 1 {
        keys.push("return".to_string());
    }
    keys
}

fn normalize_answers(raw: &Value, questions: &[ShapedQuestion]) -> Option<Vec<Vec<usize>>> {
    let list: Vec<Value> = match raw.as_array() {
        Some(arr) => arr.clone(),
        None => vec![Value::Array(vec![raw.clone()])],
    };
    if list.len() != questions.len() {
        return None;
    }
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        let picks: Vec<Value> = match item.as_array() {
            Some(arr) => arr.clone(),
            None => vec![item.clone()],
        };
        let q = &questions[i];
        if !q.multi_select && picks.len() != 1 {
            return None;
        }
        let mut idx_picks = Vec::with_capacity(picks.len());
        for p in &picks {
            let n = p.as_i64()?;
            if n < 0 || n as usize >= q.options.len() {
                return None;
            }
            idx_picks.push(n as usize);
        }
        let mut seen = HashSet::new();
        for p in &idx_picks {
            if !seen.insert(*p) {
                return None;
            }
        }
        out.push(idx_picks);
    }
    Some(out)
}

struct Inner {
    questions: HashMap<String, Ask>,
    expired: HashMap<String, Ask>,
    expired_order: VecDeque<String>,
}

#[derive(Clone)]
pub struct Queue {
    inner: Arc<Mutex<Inner>>,
    store: Store,
    focus: Arc<Focus>,
    emit_tx: Emit,
}

impl Queue {
    pub fn new(store: Store, focus: Arc<Focus>, emit_tx: Emit) -> Self {
        Queue {
            inner: Arc::new(Mutex::new(Inner {
                questions: HashMap::new(),
                expired: HashMap::new(),
                expired_order: VecDeque::new(),
            })),
            store,
            focus,
            emit_tx,
        }
    }

    fn session_name(&self, session_id: Option<&str>) -> String {
        session_id
            .and_then(|id| self.store.get(id))
            .and_then(|d| d.get("name").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "session".into())
    }

    fn intent_for(&self, session_id: Option<&str>) -> String {
        session_id
            .and_then(|id| self.store.raw(id))
            .filter(|s| !s.last_prompt.is_empty())
            .map(|s| format!("you asked: {}", s.last_prompt))
            .unwrap_or_default()
    }

    pub fn on_question(&self, payload: &Value) {
        let input = payload.get("tool_input").cloned().unwrap_or_else(|| json!({}));
        let raw_questions = input
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let list: Vec<ShapedQuestion> = raw_questions
            .iter()
            .map(shape_question)
            .filter(|q| !q.options.is_empty())
            .collect();
        if list.is_empty() {
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let session_id = payload.get("session_id").and_then(Value::as_str).map(str::to_string);
        let count = list.len();
        let ask = Ask {
            kind: "question",
            id: id.clone(),
            session_id: session_id.clone(),
            session_name: self.session_name(session_id.as_deref()),
            intent: self.intent_for(session_id.as_deref()),
            header: list[0].header.clone(),
            question: list[0].question.clone(),
            options: list[0].options.clone(),
            multi_select: list[0].multi_select,
            questions: list,
            created_ts: now_ms(),
        };
        self.inner.lock().unwrap().questions.insert(id.clone(), ask.clone());
        self.schedule_expire(id);
        emit(&self.emit_tx, "claude.question.request", json!(ask));
        log(
            "QQ",
            &format!(
                "question queued: {} ({} question{})",
                ask.header,
                count,
                if count == 1 { "" } else { "s" }
            ),
        );
    }

    // Called from the PermissionRequest hook when Claude presents a plan.
    pub fn on_plan_approval(&self, payload: &Value) {
        let plan = payload
            .get("tool_input")
            .and_then(|t| t.get("plan"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let heading = plan.lines().find(|l| !l.trim().is_empty()).unwrap_or("Ready to code?");
        let question_text = heading.trim_start_matches(|c: char| c == '#' || c == ' ').to_string();
        let shaped = shape_question(&json!({
            "header": "PLAN",
            "question": if question_text.is_empty() { "Ready to code?".to_string() } else { question_text },
            "options": [
                { "label": "Yes, bypass permissions", "description": "approve the plan and run without permission prompts" },
                { "label": "Yes, manually approve", "description": "approve the plan and confirm each edit" },
            ],
        }));
        let id = uuid::Uuid::new_v4().to_string();
        let session_id = payload.get("session_id").and_then(Value::as_str).map(str::to_string);
        let ask = Ask {
            kind: "question",
            id: id.clone(),
            session_id: session_id.clone(),
            session_name: self.session_name(session_id.as_deref()),
            intent: self.intent_for(session_id.as_deref()),
            header: shaped.header.clone(),
            question: shaped.question.clone(),
            options: shaped.options.clone(),
            multi_select: false,
            questions: vec![shaped],
            created_ts: now_ms(),
        };
        self.inner.lock().unwrap().questions.insert(id.clone(), ask.clone());
        self.schedule_expire(id);
        if let Some(sid) = &session_id {
            self.store.touch(
                sid,
                crate::daemon::sessions::store::SessionPatch {
                    waiting_for_input: Some(true),
                    ..Default::default()
                },
            );
        }
        emit(&self.emit_tx, "claude.question.request", json!(ask));
        log("QQ", &format!("plan queued: {}", ask.question.chars().take(60).collect::<String>()));
    }

    // The terminal prompt is gone once the tool returns, so drop ours too.
    pub fn on_question_answered(&self, payload: &Value) {
        let session_id = payload.get("session_id").and_then(Value::as_str);
        let mut inner = self.inner.lock().unwrap();
        let mut resolved = Vec::new();
        inner.questions.retain(|id, ask| {
            if ask.session_id.as_deref() == session_id {
                resolved.push(id.clone());
                false
            } else {
                true
            }
        });
        let expired_ids: Vec<String> = inner
            .expired
            .iter()
            .filter(|(_, ask)| ask.session_id.as_deref() == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired_ids {
            inner.expired.remove(id);
            inner.expired_order.retain(|x| x != id);
            resolved.push(id.clone());
        }
        drop(inner);
        for id in resolved {
            emit(
                &self.emit_tx,
                "claude.question.resolved",
                json!({ "id": id, "resolution": "answered" }),
            );
        }
    }

    // Esc kills the terminal dialog without firing any hook. Only asks older
    // than the marker die — the tail replays the whole transcript on catch-up.
    pub fn on_interrupted(&self, session_id: &str, ts: i64) {
        let mut inner = self.inner.lock().unwrap();
        let mut resolved = Vec::new();
        inner.questions.retain(|id, ask| {
            if ask.session_id.as_deref() == Some(session_id) && ask.created_ts <= ts {
                resolved.push((id.clone(), ask.header.clone()));
                false
            } else {
                true
            }
        });
        let expired_hits: Vec<(String, String)> = inner
            .expired
            .iter()
            .filter(|(_, ask)| ask.session_id.as_deref() == Some(session_id) && ask.created_ts <= ts)
            .map(|(id, ask)| (id.clone(), ask.header.clone()))
            .collect();
        for (id, header) in &expired_hits {
            inner.expired.remove(id);
            inner.expired_order.retain(|x| x != id);
            resolved.push((id.clone(), header.clone()));
        }
        drop(inner);
        for (id, header) in resolved {
            emit(
                &self.emit_tx,
                "claude.question.resolved",
                json!({ "id": id, "resolution": "interrupted" }),
            );
            log("QQ", &format!("question interrupted: {header}"));
        }
    }

    fn schedule_expire(&self, id: String) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(question_ttl_ms())).await;
            this.expire(&id);
        });
    }

    // A timed-out question leaves the queue but stays on the device on
    // purpose — it is still up in the terminal, and the card is how you get
    // back to it.
    fn expire(&self, id: &str) {
        let ask = {
            let mut inner = self.inner.lock().unwrap();
            match inner.questions.remove(id) {
                Some(ask) => {
                    inner.expired.insert(id.to_string(), ask.clone());
                    inner.expired_order.push_back(id.to_string());
                    while inner.expired_order.len() > MAX_EXPIRED {
                        if let Some(oldest) = inner.expired_order.pop_front() {
                            inner.expired.remove(&oldest);
                        }
                    }
                    Some(ask)
                }
                None => None,
            }
        };
        if ask.is_none() {
            return;
        }
        let id = id.to_string();
        let this = self.clone();
        let id_for_task = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(question_ttl_ms())).await;
            let mut inner = this.inner.lock().unwrap();
            inner.expired.remove(&id_for_task);
            inner.expired_order.retain(|x| x != &id_for_task);
        });
        emit(
            &self.emit_tx,
            "claude.question.resolved",
            json!({ "id": id, "resolution": "timeout" }),
        );
    }

    pub async fn answer_question(&self, id: &str, raw_answers: &Value) -> Value {
        let ask = {
            let inner = self.inner.lock().unwrap();
            inner.questions.get(id).or_else(|| inner.expired.get(id)).cloned()
        };
        let Some(ask) = ask else {
            log("QQ", &format!("answer refused: {} already resolved", &id[..id.len().min(8)]));
            return json!({ "accepted": false, "reason": "already resolved" });
        };
        let Some(answers) = normalize_answers(raw_answers, &ask.questions) else {
            return json!({ "accepted": false, "reason": "bad answer shape" });
        };
        let chosen: String = ask
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let labels: Vec<&str> = answers[i].iter().map(|&p| q.options[p].label.as_str()).collect();
                if labels.is_empty() {
                    "none".to_string()
                } else {
                    labels.join(" + ")
                }
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let keys = key_sequence(&ask.questions, &answers);

        let this = self.clone();
        let id = id.to_string();
        self.focus
            .exclusive(move || async move { this.type_answer(&id, &ask, &chosen, &keys).await })
            .await
    }

    async fn type_answer(&self, id: &str, ask: &Ask, chosen: &str, keys: &[String]) -> Value {
        let (still_pending, timed_out) = {
            let inner = self.inner.lock().unwrap();
            let pending = inner.questions.contains_key(id) || inner.expired.contains_key(id);
            let timed_out = !inner.questions.contains_key(id);
            (pending, timed_out)
        };
        if !still_pending {
            log("QQ", &format!("answer dropped: {} resolved while queued", &id[..id.len().min(8)]));
            return json!({ "accepted": false, "reason": "already resolved" });
        }

        let f = match &ask.session_id {
            Some(sid) => self.focus.focus_session(sid).await,
            None => crate::daemon::focus::FocusResult {
                reason: Some("unknown session".into()),
                ..Default::default()
            },
        };

        let mut typed = crate::daemon::focus::TypeResult {
            reason: Some("not attempted".into()),
            ..Default::default()
        };
        if f.focused && f.exact {
            typed = self.focus.type_sequence(keys).await;
        }

        if typed.typed {
            let mut inner = self.inner.lock().unwrap();
            inner.questions.remove(id);
            inner.expired.remove(id);
            inner.expired_order.retain(|x| x != id);
            drop(inner);
            emit(
                &self.emit_tx,
                "claude.question.resolved",
                json!({ "id": id, "resolution": "answered" }),
            );
            log(
                "QQ",
                &format!("answered by keypress [{}]: {} -> {chosen}", keys.join(" "), ask.header),
            );
            return json!({ "accepted": true, "viaKeyboard": true, "option": chosen, "keys": keys });
        }

        let reason = if f.focused {
            typed.reason.clone().unwrap_or_else(|| "could not type".into())
        } else {
            f.reason.clone().unwrap_or_default()
        };
        log(
            "QQ",
            &format!(
                "answer {}{} [{}]: {reason}",
                if f.focused { "focused" } else { "failed" },
                if timed_out { " (timed out)" } else { "" },
                keys.join(" ")
            ),
        );
        json!({
            "accepted": f.focused,
            "viaKeyboard": false,
            "option": chosen,
            "questionCount": ask.questions.len(),
            "focused": f.focused,
            "timedOut": timed_out,
            "reason": reason,
        })
    }

    pub fn list(&self) -> Vec<Ask> {
        let inner = self.inner.lock().unwrap();
        let mut list: Vec<Ask> = inner.questions.values().cloned().collect();
        list.sort_by_key(|a| a.created_ts);
        list
    }

    pub fn size(&self) -> usize {
        self.inner.lock().unwrap().questions.len()
    }
}
