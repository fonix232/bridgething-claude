// Session source facade — mirrors daemon/src/sessions/index.js. Real sources
// (hooks ingest + poller) or the mock source, feeding the same store.

use crate::daemon::config::{claude_settings, mock_sessions};
use crate::daemon::log::log;
use crate::daemon::permission_bridge::PermissionBridge;
use crate::daemon::queue::Queue;
use crate::daemon::sessions::source_hooks::HooksSource;
use crate::daemon::sessions::source_mock;
use crate::daemon::sessions::source_poller;
use crate::daemon::sessions::store::{now_ms, SessionPatch, Store};
use crate::daemon::sessions::tails::set_tail_interrupt_handler;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Notify;

// How fresh a transcript interrupt marker must be to also clear the
// session's tile flags — the ask-cleanup path guards by createdTs and
// tolerates any age; this one only clears a live Esc, not catch-up replay.
const INTERRUPT_FRESH_MS: i64 = 15_000;

pub struct Sources {
    hooks: Option<HooksSource>,
    mock_stop: Option<Arc<Notify>>,
    poller_stop: Option<Arc<Notify>>,
}

impl Sources {
    pub fn new(store: Store, permission_bridge: Option<PermissionBridge>, queue: Option<Queue>) -> Self {
        if mock_sessions() {
            let stop = source_mock::start(store, permission_bridge, queue);
            return Sources {
                hooks: None,
                mock_stop: Some(stop),
                poller_stop: None,
            };
        }

        let poller_stop = source_poller::start(store.clone());
        let hooks = HooksSource::new(store.clone(), queue.clone(), permission_bridge.clone());

        let q = queue.clone();
        let pb = permission_bridge.clone();
        // Esc fires no hook, so the transcript tail is the only path that
        // retires asks and tile flags for an interrupted turn.
        set_tail_interrupt_handler(Arc::new(move |session_id: String, ts: i64| {
            if let Some(q) = &q {
                q.on_interrupted(&session_id, ts);
            }
            if let Some(pb) = &pb {
                pb.cancel_session(&session_id, ts);
            }
            if now_ms() - ts < INTERRUPT_FRESH_MS {
                store.touch(
                    &session_id,
                    SessionPatch {
                        waiting_for_input: Some(false),
                        thinking: Some(false),
                        current_tool: Some(None),
                        stopped_ts: Some(Some(ts)),
                        ..Default::default()
                    },
                );
            }
        }));

        Sources {
            hooks: Some(hooks),
            mock_stop: None,
            poller_stop: Some(poller_stop),
        }
    }

    pub fn on_hook_event(&self, event: &str, payload: &Value) {
        match &self.hooks {
            Some(h) => h.on_hook_event(event, payload),
            None => log("HK", &format!("hook {event} ignored (mock mode)")),
        }
    }

    pub fn hooks_installed(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(claude_settings()) else {
            return false;
        };
        let Ok(settings) = serde_json::from_str::<Value>(&text) else {
            return false;
        };
        let flat = settings
            .get("hooks")
            .and_then(|h| h.get("PermissionRequest"))
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        flat.to_string().contains("/hook")
    }

    pub fn status(&self) -> Vec<&'static str> {
        if self.hooks.is_some() {
            vec!["poller", "hooks"]
        } else {
            vec!["mock"]
        }
    }

    pub fn stop(&self) {
        if let Some(h) = &self.hooks {
            h.stop();
        }
        if let Some(s) = &self.mock_stop {
            s.notify_waiters();
        }
        if let Some(s) = &self.poller_stop {
            s.notify_waiters();
        }
    }
}
