// Wires the ported daemon together: mirrors daemon/src/index.js's module
// construction + RPC method table.

use crate::daemon::bus::{emit, new_bus};
use crate::daemon::config::daemon_version;
use crate::daemon::focus::Focus;
use crate::daemon::hub::{HubState, MethodFuture, MethodHandler};
use crate::daemon::permission_bridge::PermissionBridge;
use crate::daemon::queue::Queue;
use crate::daemon::sessions::sources::Sources;
use crate::daemon::sessions::store::Store;
use crate::daemon::usage::Usage;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Paths {
    pub log_dir: PathBuf,
    pub state_dir: PathBuf,
    // Where daemon/scripts/{install,uninstall}-hooks.js live — see
    // http_server.rs's doc comment for why those stayed as Node scripts.
    pub scripts_dir: PathBuf,
}

pub struct Daemon {
    pub store: Store,
    pub queue: Queue,
    pub permission_bridge: PermissionBridge,
    pub usage: Usage,
    pub sources: Arc<Sources>,
    pub hub: Arc<HubState>,
    pub scripts_dir: PathBuf,
}

fn queue_snapshot(permission_bridge: &PermissionBridge, queue: &Queue) -> Value {
    let mut asks: Vec<Value> = permission_bridge.list();
    asks.extend(
        queue
            .list()
            .into_iter()
            .map(|a| serde_json::to_value(a).expect("Ask serializes")),
    );
    asks.sort_by_key(|a| a.get("createdTs").and_then(Value::as_i64).unwrap_or(0));
    json!({ "asks": asks })
}

fn method(f: impl Fn(Value, String) -> MethodFuture + Send + Sync + 'static) -> MethodHandler {
    Arc::new(f)
}

pub fn start(paths: Paths) -> Daemon {
    crate::daemon::log::init(&paths.log_dir);

    let bus = new_bus();
    let store = Store::new(bus.clone());
    let focus = Focus::new();
    let queue = Queue::new(store.clone(), focus.clone(), bus.clone());
    let permission_bridge = PermissionBridge::new(store.clone(), Some(queue.clone()), bus.clone());
    let usage = Usage::new(bus.clone(), paths.state_dir.clone());
    let sources = Arc::new(Sources::new(
        store.clone(),
        Some(permission_bridge.clone()),
        Some(queue.clone()),
    ));

    let hub = HubState::new(bus.clone());

    // Broadcast the whole waiting list whenever a client turns up — the only
    // moment a restarted daemon can correct a screen that has been showing
    // asks this one has never heard of.
    {
        let permission_bridge = permission_bridge.clone();
        let queue = queue.clone();
        let bus = bus.clone();
        hub.set_on_hello(Arc::new(move |_role: &str| {
            emit(&bus, "claude.queue.sync", queue_snapshot(&permission_bridge, &queue));
        }));
    }

    let mut methods: HashMap<String, MethodHandler> = HashMap::new();

    methods.insert("claude.ping".to_string(), {
        let store = store.clone();
        method(move |_params, _role| {
            let store = store.clone();
            Box::pin(async move { Ok(json!({ "daemonVersion": daemon_version(), "sessions": store.count() })) })
        })
    });

    methods.insert("claude.sessions.list".to_string(), {
        let store = store.clone();
        method(move |params, _role| {
            let store = store.clone();
            Box::pin(async move {
                let limit = params.get("limit").and_then(Value::as_u64).map(|n| n as usize);
                Ok(store.snapshot(limit))
            })
        })
    });

    methods.insert("claude.session.get".to_string(), {
        let store = store.clone();
        method(move |params, _role| {
            let store = store.clone();
            Box::pin(async move {
                let id = params.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                store.get(&id).ok_or_else(|| "unknown session".to_string())
            })
        })
    });

    methods.insert("claude.permission.answer".to_string(), {
        let pb = permission_bridge.clone();
        method(move |params, _role| {
            let pb = pb.clone();
            Box::pin(async move {
                let request_id = params.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
                let decision = params.get("decision").and_then(Value::as_str).unwrap_or("").to_string();
                let accepted = pb.answer(&request_id, &decision)?;
                Ok(json!({ "accepted": accepted }))
            })
        })
    });

    methods.insert("claude.queue.list".to_string(), {
        let pb = permission_bridge.clone();
        let q = queue.clone();
        method(move |_params, _role| {
            let pb = pb.clone();
            let q = q.clone();
            Box::pin(async move { Ok(queue_snapshot(&pb, &q)) })
        })
    });

    methods.insert("claude.question.answer".to_string(), {
        let q = queue.clone();
        method(move |params, _role| {
            let q = q.clone();
            Box::pin(async move {
                let id = params.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let answers = params
                    .get("answers")
                    .or_else(|| params.get("optionIndex"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(q.answer_question(&id, &answers).await)
            })
        })
    });

    methods.insert("claude.session.focus".to_string(), {
        let focus = focus.clone();
        method(move |params, _role| {
            let focus = focus.clone();
            Box::pin(async move {
                let id = params.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let focus2 = focus.clone();
                let result = focus.exclusive(move || async move { focus2.focus_session(&id).await }).await;
                Ok(serde_json::to_value(result).expect("FocusResult serializes"))
            })
        })
    });

    methods.insert("claude.usage.get".to_string(), {
        let usage = usage.clone();
        method(move |_params, _role| {
            let usage = usage.clone();
            Box::pin(async move { Ok(usage.get()) })
        })
    });

    hub.set_methods(methods);
    usage.start();

    crate::daemon::log::log(
        "--",
        &format!(
            "claude-thing daemon v{} ({})",
            daemon_version(),
            if crate::daemon::config::mock_sessions() {
                "MOCK sessions"
            } else {
                "real sources"
            }
        ),
    );

    Daemon {
        store,
        queue,
        permission_bridge,
        usage,
        sources,
        hub,
        scripts_dir: paths.scripts_dir,
    }
}
