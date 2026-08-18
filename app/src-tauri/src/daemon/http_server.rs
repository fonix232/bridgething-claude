// Plain HTTP + WS upgrade at 127.0.0.1:<port>. Mirrors daemon/src/http-
// server.js, minus the control-page static file serving — the Tauri window
// loads that from bundled assets now, so there is nothing left to serve for
// it. Only the device (over the reverse SSH tunnel) and the Tauri window's
// WS client talk to this.

use crate::daemon::config::daemon_version;
use crate::daemon::hub::{handle_socket, HubState};
use crate::daemon::log::log;
use crate::daemon::permission_bridge::PermissionBridge;
use crate::daemon::sessions::sources::Sources;
use crate::daemon::sessions::store::Store;
use axum::{
    body::Bytes,
    extract::{ws::WebSocketUpgrade, Path as AxPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<HubState>,
    pub store: Store,
    pub permission_bridge: PermissionBridge,
    pub sources: Arc<Sources>,
    // daemon/scripts/{install,uninstall}-hooks.js stay as small standalone
    // Node utilities rather than being ported — they're one-shot CLI tools,
    // not daemon runtime logic, and Node is already a hard requirement here.
    pub scripts_dir: PathBuf,
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(state.hub.clone(), socket))
}

async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "daemonVersion": daemon_version(),
        "sessions": state.store.count(),
        "pendingPermissions": state.permission_bridge.pending_count(),
        "clients": state.hub.roles_online(),
        "sources": state.sources.status(),
        "hooks": state.sources.hooks_installed(),
    }))
}

async fn dispatch_hook(state: AppState, event: String, payload: Value) -> axum::response::Response {
    // PermissionRequest is held; everything else is acknowledged immediately
    // and forwarded to session sources.
    if event == "PermissionRequest" {
        let (tx, rx) = oneshot::channel();
        state.permission_bridge.on_hook_request(&payload, tx);
        let body = rx.await.unwrap_or_else(|_| "{}".to_string());
        return (StatusCode::OK, [("content-type", "application/json")], body).into_response();
    }
    state.sources.on_hook_event(&event, &payload);
    (StatusCode::OK, [("content-type", "application/json")], "{}".to_string()).into_response()
}

// Accepts /hook/<event> (command hooks) and bare /hook (type:"http" hooks —
// event read from the payload's hook_event_name).
async fn hook_handler(State(state): State<AppState>, body: Bytes) -> impl IntoResponse {
    let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    log("IN", &format!("POST /hook ({event})"));
    dispatch_hook(state, event, payload).await
}

async fn hook_event_handler(
    State(state): State<AppState>,
    AxPath(event): AxPath<String>,
    body: Bytes,
) -> impl IntoResponse {
    let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    log("IN", &format!("POST /hook/{event}"));
    dispatch_hook(state, event, payload).await
}

async fn run_hook_script(scripts_dir: &Path, script: &str) -> impl IntoResponse {
    let output = tokio::process::Command::new("node").arg(scripts_dir.join(script)).output().await;
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (StatusCode::OK, Json(json!({ "ok": true, "output": text })))
        }
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "ok": false, "output": text })))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "output": e.to_string() })),
        ),
    }
}

async fn hooks_install_handler(State(state): State<AppState>) -> impl IntoResponse {
    run_hook_script(&state.scripts_dir, "install-hooks.js").await
}

async fn hooks_uninstall_handler(State(state): State<AppState>) -> impl IntoResponse {
    run_hook_script(&state.scripts_dir, "uninstall-hooks.js").await
}

pub async fn serve(state: AppState, host: &str, port: u16) -> std::io::Result<()> {
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/status", get(status_handler))
        .route("/hook", post(hook_handler))
        .route("/hook/{event}", post(hook_event_handler))
        .route("/api/hooks/install", post(hooks_install_handler))
        .route("/api/hooks/uninstall", post(hooks_uninstall_handler))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse().expect("valid bind address");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log("--", &format!("http+ws on http://{host}:{port}  (ws path /ws)"));
    axum::serve(listener, app).await
}
