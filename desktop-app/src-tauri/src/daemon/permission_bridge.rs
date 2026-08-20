// Holds Claude Code PermissionRequest hook responses until a device answers
// or PERMISSION_HOLD_MS elapses. Never auto-denies: timeout answers
// behavior:"ask" so the normal terminal prompt takes over. Mirrors
// daemon/src/permission-bridge.js.
//
// KNOWN GAP vs the JS version: JS also released a pending permission the
// moment the *hook's own HTTP connection* closed early (Claude Code gave up
// first). This port only releases on the PERMISSION_HOLD_MS timeout or an
// explicit answer/cancel/release call — an early hook disconnect is not
// detected, so a permission whose caller already gave up can sit on the
// device for up to the full hold window instead of clearing immediately.

use crate::daemon::bus::{emit, Emit};
use crate::daemon::config::permission_hold_ms;
use crate::daemon::log::log;
use crate::daemon::queue::Queue;
use crate::daemon::sessions::store::{now_ms, Permission, SessionPatch, Store};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio::time::Duration;
use uuid::Uuid;

fn hook_decision(behavior: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": { "behavior": behavior }
        }
    })
    .to_string()
}

struct Pending {
    responder: Option<oneshot::Sender<String>>,
    session_id: Option<String>,
    tool: String,
    summary: String,
    intent: String,
    created_ts: i64,
}

struct Inner {
    pending: HashMap<String, Pending>,
}

#[derive(Clone)]
pub struct PermissionBridge {
    inner: Arc<Mutex<Inner>>,
    store: Store,
    queue: Option<Queue>,
    emit_tx: Emit,
}

impl PermissionBridge {
    pub fn new(store: Store, queue: Option<Queue>, emit_tx: Emit) -> Self {
        PermissionBridge {
            inner: Arc::new(Mutex::new(Inner { pending: HashMap::new() })),
            store,
            queue,
            emit_tx,
        }
    }

    fn intent_for(&self, session_id: Option<&str>) -> String {
        session_id
            .and_then(|id| self.store.raw(id))
            .filter(|s| !s.last_prompt.is_empty())
            .map(|s| format!("you asked: {}", s.last_prompt))
            .unwrap_or_default()
    }

    // The device renders the tool name separately, so the summary is just the
    // salient argument.
    fn summarize_tool(tool_name: &str, tool_input: Option<&Value>) -> String {
        let Some(input) = tool_input else {
            return tool_name.to_string();
        };
        for key in ["command", "file_path", "url", "path", "pattern"] {
            if let Some(s) = input.get(key).and_then(Value::as_str) {
                if !s.is_empty() {
                    return s.chars().take(200).collect();
                }
            }
        }
        if let Some(obj) = input.as_object() {
            for v in obj.values() {
                if let Some(s) = v.as_str() {
                    if !s.trim().is_empty() {
                        return s.chars().take(200).collect();
                    }
                }
            }
            if !obj.is_empty() {
                let joined = obj.keys().cloned().collect::<Vec<_>>().join(", ");
                return joined.chars().take(200).collect();
            }
        }
        tool_name.to_string()
    }

    fn finish(&self, request_id: &str, resolution: &str, behavior: Option<&str>) -> bool {
        let pending = self.inner.lock().unwrap().pending.remove(request_id);
        let Some(mut p) = pending else {
            return false;
        };
        if let Some(tx) = p.responder.take() {
            let _ = tx.send(hook_decision(behavior.unwrap_or("ask")));
        }
        if let Some(sid) = &p.session_id {
            self.store.upsert(
                sid,
                SessionPatch {
                    pending_permission: Some(false),
                    permission: Some(None),
                    ..Default::default()
                },
            );
        }
        emit(
            &self.emit_tx,
            "claude.permission.resolved",
            json!({ "requestId": request_id, "resolution": resolution }),
        );
        log("PB", &format!("permission {request_id} -> {resolution}"));
        true
    }

    // Called by the http layer with the parsed hook payload and a oneshot
    // sender for the (eventually) held response body.
    pub fn on_hook_request(&self, payload: &Value, responder: oneshot::Sender<String>) {
        let tool_name = payload.get("tool_name").and_then(Value::as_str).unwrap_or("unknown");

        // ExitPlanMode and AskUserQuestion are dialogs, not permissions — both
        // already reach the device through the question queue.
        if self.queue.is_some() && (tool_name == "ExitPlanMode" || tool_name == "AskUserQuestion") {
            let _ = responder.send(hook_decision("ask"));
            if tool_name == "ExitPlanMode" {
                if let Some(q) = &self.queue {
                    q.on_plan_approval(payload);
                }
            }
            return;
        }

        let request_id = Uuid::new_v4().to_string();
        let session_id = payload.get("session_id").and_then(Value::as_str).map(str::to_string);
        let summary = Self::summarize_tool(tool_name, payload.get("tool_input"));
        let intent = self.intent_for(session_id.as_deref());
        let created_ts = now_ms();
        let hold_ms = permission_hold_ms();

        self.inner.lock().unwrap().pending.insert(
            request_id.clone(),
            Pending {
                responder: Some(responder),
                session_id: session_id.clone(),
                tool: tool_name.to_string(),
                summary: summary.clone(),
                intent: intent.clone(),
                created_ts,
            },
        );

        let this = self.clone();
        let rid = request_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(hold_ms)).await;
            this.finish(&rid, "timeout", Some("ask"));
        });

        if let Some(sid) = &session_id {
            self.store.upsert(
                sid,
                SessionPatch {
                    pending_permission: Some(true),
                    permission: Some(Some(Permission {
                        request_id: request_id.clone(),
                        tool: tool_name.to_string(),
                        summary: summary.clone(),
                        intent: intent.clone(),
                        created_ts,
                        timeout_ms: hold_ms,
                    })),
                    cwd: payload.get("cwd").and_then(Value::as_str).map(str::to_string),
                    current_tool: Some(Some(tool_name.to_string())),
                    ..Default::default()
                },
            );
        }
        emit(
            &self.emit_tx,
            "claude.permission.request",
            json!({
                "requestId": request_id, "tool": tool_name, "summary": summary,
                "intent": intent, "createdTs": created_ts, "timeoutMs": hold_ms,
                "sessionId": session_id,
            }),
        );
        log("PB", &format!("permission held: {summary} ({request_id})"));
    }

    pub fn answer(&self, request_id: &str, decision: &str) -> Result<bool, String> {
        if decision != "allow" && decision != "deny" {
            return Err("decision must be allow|deny".into());
        }
        Ok(self.finish(request_id, decision, Some(decision)))
    }

    // Esc during a held permission: releasing with "ask" is safe either way —
    // the turn is dead, so nobody is left to show a prompt.
    pub fn cancel_session(&self, session_id: &str, ts: i64) {
        let ids: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            inner
                .pending
                .iter()
                .filter(|(_, p)| p.session_id.as_deref() == Some(session_id) && p.created_ts <= ts)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.finish(&id, "interrupted", Some("ask"));
        }
    }

    // A PostToolUse arriving while a permission is still held for the same
    // session+tool means that call was never gated (pre-approved / allowlisted).
    pub fn release_ran(&self, session_id: &str, tool_name: &str, ts: i64) -> bool {
        let oldest = {
            let inner = self.inner.lock().unwrap();
            inner
                .pending
                .iter()
                .filter(|(_, p)| {
                    p.session_id.as_deref() == Some(session_id) && p.tool == tool_name && p.created_ts <= ts
                })
                .min_by_key(|(_, p)| p.created_ts)
                .map(|(id, _)| id.clone())
        };
        match oldest {
            Some(id) => self.finish(&id, "preapproved", Some("ask")),
            None => false,
        }
    }

    pub fn list(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        let mut items: Vec<Value> = inner
            .pending
            .iter()
            .map(|(id, p)| {
                let session_name = p
                    .session_id
                    .as_deref()
                    .and_then(|sid| self.store.get(sid))
                    .and_then(|d| d.get("name").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "session".into());
                json!({
                    "kind": "permission", "id": id, "sessionId": p.session_id,
                    "sessionName": session_name, "tool": p.tool, "summary": p.summary,
                    "intent": p.intent, "createdTs": p.created_ts, "timeoutMs": permission_hold_ms(),
                })
            })
            .collect();
        items.sort_by_key(|v| v["createdTs"].as_i64().unwrap_or(0));
        items
    }

    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().pending.len()
    }
}
