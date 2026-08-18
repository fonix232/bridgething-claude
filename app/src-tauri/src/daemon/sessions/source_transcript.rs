// Tails a session's transcript JSONL for token usage + activity. Mirrors
// daemon/src/sessions/source-transcript.js. The format is Claude Code-
// internal — every field access is defensive.
//
// Polls only, no native fs-watch: JS used both fs.watch (fast path) and a 3s
// poll (fallback). A watcher crate is more than this needs — a short poll
// interval gets the same felt latency without the extra dependency.

use crate::daemon::sessions::store::{now_ms, SessionPatch, Store};
use chrono::DateTime;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Notify;
use tokio::time::Duration;

const POLL_MS: u64 = 500;

fn parse_ts(raw: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

async fn harvest(store: &Store, session_id: &str, line: &str, on_interrupt: &(dyn Fn(i64) + Send + Sync)) {
    let obj: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Permission mode is its own record type and has no message to harvest.
    if obj.get("type").and_then(Value::as_str) == Some("permission-mode") {
        if let Some(mode) = obj.get("permissionMode").and_then(Value::as_str) {
            if !mode.is_empty() {
                store.upsert(
                    session_id,
                    SessionPatch {
                        permission_mode: Some(mode.to_string()),
                        ..Default::default()
                    },
                );
            }
        }
        return;
    }

    let msg = obj.get("message").cloned().unwrap_or_else(|| obj.clone());
    let usage = msg.get("usage").or_else(|| obj.get("usage"));

    let mut patch = SessionPatch::default();
    let mut touched = false;

    if let Some(usage) = usage {
        let raw = store.raw(session_id);
        let input = usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        // What a turn sent is what the window currently holds — an overwrite,
        // not a sum. Lifetime counters below are the opposite arithmetic on
        // the same numbers.
        patch.context_tokens = Some(input + cache_read + cache_creation);
        let prior_in = raw.as_ref().map(|r| r.tokens_in).unwrap_or(0);
        let prior_out = raw.as_ref().map(|r| r.tokens_out).unwrap_or(0);
        let prior_cache = raw.as_ref().map(|r| r.cache_read).unwrap_or(0);
        patch.tokens_in = Some(prior_in + input + cache_creation);
        patch.tokens_out = Some(prior_out + output);
        patch.cache_read = Some(prior_cache + cache_read);
        touched = true;
    }
    if let Some(model) = msg.get("model").and_then(Value::as_str) {
        patch.model = Some(model.to_string());
        touched = true;
    }
    if let Some(effort) = obj.get("effort").and_then(Value::as_str) {
        if !effort.is_empty() {
            patch.effort = Some(effort.to_string());
            touched = true;
        }
    }
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        let text: String = content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        if !text.is_empty() {
            patch.last_message = Some(text.chars().take(200).collect());
            touched = true;
            // Esc fires no hook at all; this is the only signal a pending ask
            // just died, surfaced with the line's own timestamp so catch-up
            // replay can tell a stale marker from a live Esc.
            if text.starts_with("[Request interrupted") {
                let ts = obj
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .or_else(|| msg.get("timestamp").and_then(Value::as_str))
                    .and_then(parse_ts)
                    .unwrap_or_else(now_ms);
                on_interrupt(ts);
            }
        }
    }
    let ts_raw = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .or_else(|| msg.get("timestamp").and_then(Value::as_str));
    if let Some(ts) = ts_raw.and_then(parse_ts) {
        patch.last_activity_ts = Some(ts);
        touched = true;
    }

    if touched {
        store.upsert(session_id, patch);
    }
}

pub async fn run(
    store: Store,
    session_id: String,
    path: PathBuf,
    stop: Arc<Notify>,
    on_interrupt: Arc<dyn Fn(i64) + Send + Sync>,
) {
    let mut offset: u64 = 0;
    loop {
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            let size = meta.len();
            if size > offset {
                if let Ok(mut file) = tokio::fs::File::open(&path).await {
                    if file.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                        let mut buf = Vec::new();
                        if file.read_to_end(&mut buf).await.is_ok() {
                            offset = size;
                            let text = String::from_utf8_lossy(&buf);
                            for line in text.split('\n') {
                                if !line.trim().is_empty() {
                                    harvest(&store, &session_id, line, on_interrupt.as_ref()).await;
                                }
                            }
                        }
                    }
                }
            }
        }
        tokio::select! {
            _ = stop.notified() => break,
            _ = tokio::time::sleep(Duration::from_millis(POLL_MS)) => {}
        }
    }
}
