// One transcript tail per session, shared by the hooks and poller sources.
// Mirrors daemon/src/sessions/tails.js. Two tails on the same file would
// double-count tokens, so everything goes through this registry.

use crate::daemon::config;
use crate::daemon::sessions::source_transcript;
use crate::daemon::sessions::store::Store;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Notify;

pub type InterruptHandler = Arc<dyn Fn(String, i64) + Send + Sync>;

fn interrupt_handler() -> &'static Mutex<Option<InterruptHandler>> {
    static HANDLER: OnceLock<Mutex<Option<InterruptHandler>>> = OnceLock::new();
    HANDLER.get_or_init(|| Mutex::new(None))
}

// Esc in the terminal fires no hook, so the transcript tail is the only place
// an interrupt is visible. Whoever owns the ask queue registers here.
pub fn set_tail_interrupt_handler(handler: InterruptHandler) {
    *interrupt_handler().lock().unwrap() = Some(handler);
}

fn fire_interrupt(session_id: String, ts: i64) {
    if let Some(h) = interrupt_handler().lock().unwrap().as_ref() {
        h(session_id, ts);
    }
}

// Claude Code stores transcripts at
// ~/.claude/projects/<cwd with non-alphanumerics replaced by '-'>/<sessionId>.jsonl
pub fn transcript_path_for(session_id: &str, cwd: &str) -> Option<PathBuf> {
    if cwd.is_empty() {
        return None;
    }
    let dir: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(config::claude_projects_dir().join(dir).join(format!("{session_id}.jsonl")))
}

struct Registry {
    tails: HashMap<String, Arc<Notify>>,
}

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        Mutex::new(Registry {
            tails: HashMap::new(),
        })
    })
}

pub fn ensure_tail(store: &Store, session_id: &str, transcript_path: Option<PathBuf>) {
    let Some(path) = transcript_path else {
        return;
    };
    {
        let reg = registry().lock().unwrap();
        if reg.tails.contains_key(session_id) {
            return;
        }
    }
    if !path.exists() {
        return;
    }
    let stop = Arc::new(Notify::new());
    registry()
        .lock()
        .unwrap()
        .tails
        .insert(session_id.to_string(), stop.clone());

    let sid = session_id.to_string();
    let store = store.clone();
    tokio::spawn(async move {
        let sid_for_interrupt = sid.clone();
        let on_interrupt: Arc<dyn Fn(i64) + Send + Sync> =
            Arc::new(move |ts| fire_interrupt(sid_for_interrupt.clone(), ts));
        source_transcript::run(store, sid, path, stop, on_interrupt).await;
    });
}

pub fn stop_tail(session_id: &str) {
    if let Some(stop) = registry().lock().unwrap().tails.remove(session_id) {
        stop.notify_waiters();
    }
}

pub fn stop_all_tails() {
    let mut reg = registry().lock().unwrap();
    for (_, stop) in reg.tails.drain() {
        stop.notify_waiters();
    }
}
