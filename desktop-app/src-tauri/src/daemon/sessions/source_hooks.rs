// Ingests Claude Code hook events POSTed to /hook and keeps the store
// current. Mirrors daemon/src/sessions/source-hooks.js.

use crate::daemon::focus::lookup_session;
use crate::daemon::log::log;
use crate::daemon::own_sessions::is_own_session;
use crate::daemon::permission_bridge::PermissionBridge;
use crate::daemon::queue::Queue;
use crate::daemon::sessions::store::{now_ms, SessionPatch, Store};
use crate::daemon::sessions::tails::{ensure_tail, stop_all_tails, stop_tail, transcript_path_for};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn asking_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)permission|approve|allow|confirm").unwrap())
}

fn basename(cwd: &str) -> Option<String> {
    Path::new(cwd).file_name().map(|s| s.to_string_lossy().to_string())
}

fn has_name(known: &Option<crate::daemon::sessions::store::Session>) -> bool {
    known
        .as_ref()
        .and_then(|k| k.name.as_deref())
        .map(|n| !n.is_empty())
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct HooksSource {
    store: Store,
    queue: Option<Queue>,
    permission_bridge: Option<PermissionBridge>,
}

impl HooksSource {
    pub fn new(store: Store, queue: Option<Queue>, permission_bridge: Option<PermissionBridge>) -> Self {
        HooksSource {
            store,
            queue,
            permission_bridge,
        }
    }

    pub fn on_hook_event(&self, event: &str, payload: &Value) {
        let Some(id) = payload.get("session_id").and_then(Value::as_str) else {
            return;
        };
        if is_own_session(id) {
            return;
        }
        let known = self.store.raw(id);
        // A session whose very first hook is SessionEnd was never on the
        // grid — short-lived `claude` invocations end this way.
        if known.is_none() && event == "SessionEnd" {
            return;
        }

        let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
        let known_has_name = has_name(&known);
        // A hook's cwd moves as the agent cds around, so it must never
        // overwrite a real name — only a fallback for a session never named.
        let registry_name = if known_has_name {
            None
        } else {
            lookup_session(id, None).and_then(|rec| rec.get("name").and_then(Value::as_str).map(str::to_string))
        };

        let mut base = SessionPatch::default();
        if !cwd.is_empty() {
            base.cwd = Some(cwd.clone());
        }
        if let Some(tp) = payload.get("transcript_path").and_then(Value::as_str) {
            base.transcript_path = Some(Some(tp.to_string()));
        }
        if let Some(pm) = payload.get("permission_mode").and_then(Value::as_str) {
            base.permission_mode = Some(pm.to_string());
        }
        if !known_has_name {
            base.name = registry_name.or_else(|| if cwd.is_empty() { None } else { basename(&cwd) });
        }
        // Any hook but SessionEnd is proof of life.
        if event != "SessionEnd" {
            base.ended = Some(false);
        }

        let transcript_path = payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| transcript_path_for(id, &cwd));
        ensure_tail(&self.store, id, transcript_path);

        match event {
            "SessionStart" => {
                let mut p = base.clone();
                p.ended = Some(false);
                p.started_ts = Some(now_ms());
                p.stopped_ts = Some(None);
                self.store.touch(id, p);
            }
            "UserPromptSubmit" => {
                let mut p = base.clone();
                p.waiting_for_input = Some(false);
                p.stopped_ts = Some(None);
                p.thinking = Some(true);
                let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("");
                p.last_message = Some(prompt.chars().take(200).collect());
                let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
                p.last_prompt = Some(collapsed.chars().take(120).collect());
                self.store.touch(id, p);
                // A declined plan or an Esc'd question fires no PostToolUse —
                // this is where those asks die.
                if let Some(q) = &self.queue {
                    q.on_question_answered(payload);
                }
            }
            "PreToolUse" => {
                let tool_name = payload.get("tool_name").and_then(Value::as_str);
                // A subagent's own tool call carries `agent_id`: it proves the
                // parent session is still alive, but the tool belongs to the
                // subagent's status, not the session's — setting the
                // session's currentTool here would flicker it back and forth
                // against whatever the subagent's own Task call already set.
                if let Some(agent_id) = payload.get("agent_id").and_then(Value::as_str) {
                    self.store.touch(id, base.clone());
                    self.store.subagent_tool(id, agent_id, tool_name.map(str::to_string));
                } else {
                    let mut p = base.clone();
                    p.current_tool = Some(tool_name.map(str::to_string));
                    p.stopped_ts = Some(None);
                    p.waiting_for_input = Some(false);
                    p.thinking = Some(true);
                    self.store.touch(id, p);
                    if tool_name == Some("AskUserQuestion") {
                        if let Some(q) = &self.queue {
                            self.store.touch(
                                id,
                                SessionPatch {
                                    waiting_for_input: Some(true),
                                    ..Default::default()
                                },
                            );
                            q.on_question(payload);
                        }
                    }
                }
            }
            "PostToolUse" => {
                let tool_name = payload.get("tool_name").and_then(Value::as_str).unwrap_or("");
                if let Some(agent_id) = payload.get("agent_id").and_then(Value::as_str) {
                    self.store.touch(id, base.clone());
                    self.store.subagent_tool(id, agent_id, None);
                } else {
                    let mut p = base.clone();
                    p.current_tool = Some(None);
                    self.store.touch(id, p);
                }
                if let Some(pb) = &self.permission_bridge {
                    pb.release_ran(id, tool_name, now_ms());
                }
                if self.queue.is_some() && (tool_name == "AskUserQuestion" || tool_name == "ExitPlanMode") {
                    self.store.touch(
                        id,
                        SessionPatch {
                            waiting_for_input: Some(false),
                            ..Default::default()
                        },
                    );
                    if let Some(q) = &self.queue {
                        q.on_question_answered(payload);
                    }
                }
            }
            "SubagentStart" => {
                self.store.touch(id, base.clone());
                if let Some(agent_id) = payload.get("agent_id").and_then(Value::as_str) {
                    let agent_type = payload.get("agent_type").and_then(Value::as_str).unwrap_or("agent");
                    self.store.subagent_start(id, agent_id, agent_type);
                }
            }
            "SubagentStop" => {
                self.store.touch(id, base.clone());
                if let Some(agent_id) = payload.get("agent_id").and_then(Value::as_str) {
                    self.store.subagent_stop(id, agent_id);
                }
            }
            "Stop" => {
                let mut p = base.clone();
                p.stopped_ts = Some(Some(now_ms()));
                p.current_tool = Some(None);
                p.waiting_for_input = Some(false);
                p.thinking = Some(false);
                self.store.touch(id, p);
                if let Some(q) = &self.queue {
                    q.on_question_answered(payload);
                }
            }
            "Notification" => {
                let message = payload.get("message").and_then(Value::as_str).unwrap_or("");
                let mut p = base.clone();
                if asking_re().is_match(message) {
                    p.waiting_for_input = Some(true);
                }
                self.store.touch(id, p);
            }
            "SessionEnd" => {
                let mut p = base.clone();
                p.ended = Some(true);
                p.current_tool = Some(None);
                p.waiting_for_input = Some(false);
                self.store.upsert(id, p);
                stop_tail(id);
            }
            _ => {
                self.store.touch(id, base.clone());
            }
        }
        log("HK", &format!("{event} {}", &id[..id.len().min(8)]));
    }

    pub fn stop(&self) {
        stop_all_tails();
    }
}
