// WS hub at /ws. Clients: the device app (over the reverse SSH tunnel) and
// the Tauri window (role "webpage"). Mirrors daemon/src/hub.js.
//
// SIMPLIFICATION vs the JS version: JS tracked each socket's own
// `bufferedAmount` and dropped superseding (sessions/usage) frames only for a
// client that had fallen behind by more than 64KB. Axum/tokio don't expose a
// per-socket write-buffer size this way, so this port relies on the broadcast
// channel's own lagged-receiver behavior instead (a slow client skips ahead
// rather than queuing forever) — same intent, coarser granularity.

use crate::daemon::bus::{emit, Emit};
use crate::daemon::config::daemon_version;
use crate::daemon::log::log;
use crate::daemon::sessions::store::now_ms;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

pub type MethodResult = Result<Value, String>;
pub type MethodFuture = Pin<Box<dyn Future<Output = MethodResult> + Send>>;
pub type MethodHandler = Arc<dyn Fn(Value, String) -> MethodFuture + Send + Sync>;
pub type OnHello = Arc<dyn Fn(&str) + Send + Sync>;

fn debug_events_enabled() -> bool {
    env::var("CLAUDE_THING_DEBUG_EVENTS").as_deref() == Ok("1")
}

pub struct HubState {
    bus: Emit,
    clients: Mutex<HashMap<u64, String>>,
    next_id: AtomicU64,
    methods: Mutex<HashMap<String, MethodHandler>>,
    on_hello: Mutex<Option<OnHello>>,
}

impl HubState {
    pub fn new(bus: Emit) -> Arc<Self> {
        let state = Arc::new(HubState {
            bus,
            clients: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            methods: Mutex::new(HashMap::new()),
            on_hello: Mutex::new(None),
        });

        // One dedicated subscriber logs every event exactly once, decoupled
        // from the per-client forwarding tasks (see handle_socket).
        let mut log_rx = state.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match log_rx.recv().await {
                    Ok(ev) => {
                        if !ev.topic.starts_with("claude.sessions.") || debug_events_enabled() {
                            log("EV", &ev.topic);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        state
    }

    pub fn set_methods(&self, methods: HashMap<String, MethodHandler>) {
        *self.methods.lock().unwrap() = methods;
    }

    pub fn set_on_hello(&self, f: OnHello) {
        *self.on_hello.lock().unwrap() = Some(f);
    }

    pub fn roles_online(&self) -> Value {
        let clients = self.clients.lock().unwrap();
        let mut roles: HashMap<String, u64> = HashMap::new();
        for role in clients.values() {
            *roles.entry(role.clone()).or_insert(0) += 1;
        }
        json!(roles)
    }

    fn broadcast_roles_online(&self) {
        emit(&self.bus, "bridge.clients", self.roles_online());
    }
}

pub async fn handle_socket(hub: Arc<HubState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let client_id = hub.next_id.fetch_add(1, Ordering::SeqCst);
    let mut bus_rx = hub.bus.subscribe();
    let role = Arc::new(Mutex::new("unknown".to_string()));

    hub.clients.lock().unwrap().insert(client_id, "unknown".to_string());
    hub.broadcast_roles_online();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        handle_message(&hub, client_id, &text, &role, &out_tx).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            event = bus_rx.recv() => {
                match event {
                    Ok(ev) => {
                        let frame = json!({
                            "type": "event", "topic": ev.topic, "data": ev.data,
                            "server_timestamp_ms": now_ms(),
                        });
                        let _ = out_tx.send(frame.to_string());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    hub.clients.lock().unwrap().remove(&client_id);
    let role_name = role.lock().unwrap().clone();
    log("--", &format!("client gone: {role_name}"));
    hub.broadcast_roles_online();
    drop(out_tx);
    let _ = writer.await;
}

async fn handle_message(
    hub: &Arc<HubState>,
    client_id: u64,
    text: &str,
    role_slot: &Arc<Mutex<String>>,
    out_tx: &mpsc::UnboundedSender<String>,
) {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if msg.get("type").and_then(Value::as_str) != Some("request") {
        return;
    }
    let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);

    if method == "bridge.hello" {
        let role = msg
            .get("params")
            .and_then(|p| p.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        *role_slot.lock().unwrap() = role.clone();
        hub.clients.lock().unwrap().insert(client_id, role.clone());
        log("--", &format!("client hello: {role}"));
        hub.broadcast_roles_online();
        let resp = json!({
            "type": "response", "id": id,
            "result": { "ok": true, "daemonVersion": daemon_version() },
        });
        let _ = out_tx.send(resp.to_string());
        // A restarted daemon can only correct a stale screen the moment a
        // client arrives — the device has no other way to know what went
        // stale while the socket was down.
        if let Some(on_hello) = hub.on_hello.lock().unwrap().as_ref() {
            on_hello(&role);
        }
        return;
    }

    let handler = hub.methods.lock().unwrap().get(&method).cloned();
    let Some(handler) = handler else {
        let _ = out_tx.send(json!({ "type": "error", "id": id, "error": "Unknown method" }).to_string());
        return;
    };
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let role = role_slot.lock().unwrap().clone();
    let result = handler(params, role.clone()).await;
    let frame = match result {
        Ok(v) => json!({ "type": "response", "id": id, "result": v }),
        Err(e) => json!({ "type": "error", "id": id, "error": e }),
    };
    let _ = out_tx.send(frame.to_string());
    if method != "claude.sessions.list" {
        log("RQ", &format!("{role} {method}"));
    }
}
