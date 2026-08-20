// Mock session source: fake sessions with evolving state + a scripted
// permission/question cycle, for device-app development with zero real
// Claude Code integration. Enabled with CLAUDE_THING_MOCK=1. Mirrors
// daemon/src/sessions/source-mock.js (tool inputs simplified to a single
// "command" field for all three mock tools — a dev-only convenience, not
// behavior anything else depends on).

use crate::daemon::log::log;
use crate::daemon::permission_bridge::PermissionBridge;
use crate::daemon::queue::Queue;
use crate::daemon::sessions::store::{now_ms, SessionPatch, Store};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{oneshot, Notify};
use tokio::time::Duration;

const NAMES: [&str; 9] = [
    "claude-thing",
    "carthing-ui",
    "credit_card_picker",
    "api-server",
    "design-system",
    "infra-terraform",
    "docs-site",
    "ml-pipeline",
    "mobile-app",
];
const TOOLS: [(&str, &str); 3] = [("Bash", "npm test"), ("Edit", "src/index.js"), ("Write", "README.md")];

pub fn start(store: Store, permission_bridge: Option<PermissionBridge>, queue: Option<Queue>) -> Arc<Notify> {
    let stop = Arc::new(Notify::new());
    let ids: Vec<String> = NAMES.iter().enumerate().map(|(i, name)| format!("mock-{i}-{name}")).collect();

    let context_fracs: [f64; 9] = [0.34, 0.62, 0.41, 0.88, 0.22, 0.55, 0.13, 0.71, 0.09];
    let modes: [Option<&str>; 6] = [
        Some("plan"),
        Some("bypassPermissions"),
        Some("auto"),
        Some("default"),
        Some("acceptEdits"),
        None,
    ];
    let efforts: [Option<&str>; 7] = [
        Some("low"),
        Some("medium"),
        Some("high"),
        Some("xhigh"),
        Some("max"),
        Some("ultrathink"),
        None,
    ];

    for (i, name) in NAMES.iter().enumerate() {
        let id = &ids[i];
        store.upsert(
            id,
            SessionPatch {
                name: Some(name.to_string()),
                cwd: Some(format!("/Users/dev/{name}")),
                model: Some("claude-fable-5".to_string()),
                tokens_in: Some(1200 * (i as u64 + 1)),
                tokens_out: Some(800 * (i as u64 + 1)),
                context_tokens: Some((context_fracs[i % 9] * 1_000_000.0).round() as u64),
                last_message: Some("Initialized mock session.".to_string()),
                last_activity_ts: Some(now_ms() - i as i64 * 60_000),
                permission_mode: modes[i % 6].map(str::to_string),
                effort: efforts[i % 7].map(str::to_string),
                last_prompt: if i == 2 { None } else { Some(format!("tidy up the {name} build")) },
                ..Default::default()
            },
        );
    }
    log("MK", &format!("mock source: {} sessions, permission every 60s, question every 90s", ids.len()));

    spawn_churn(store.clone(), ids.clone(), stop.clone());
    if let Some(pb) = permission_bridge {
        spawn_permission_cycle(pb, ids.clone(), stop.clone());
    }
    if let Some(q) = queue {
        spawn_question_cycle(q, ids, stop.clone());
    }
    stop
}

fn spawn_churn(store: Store, ids: Vec<String>, stop: Arc<Notify>) {
    tokio::spawn(async move {
        let mut n: u64 = 0;
        loop {
            tokio::select! {
                _ = stop.notified() => break,
                _ = tokio::time::sleep(Duration::from_millis(8_000)) => {}
            }
            n += 1;
            let id = &ids[(n as usize) % ids.len()];
            let (tool, _) = TOOLS[(n as usize) % TOOLS.len()];
            let raw = store.raw(id);
            let tokens_in = raw.as_ref().map(|s| s.tokens_in).unwrap_or(0) + 300;
            let tokens_out = raw.as_ref().map(|s| s.tokens_out).unwrap_or(0) + 150;
            let context_tokens = (raw.as_ref().map(|s| s.context_tokens).unwrap_or(0) + 9_000).min(1_000_000);
            store.touch(
                id,
                SessionPatch {
                    current_tool: Some(Some(tool.to_string())),
                    tokens_in: Some(tokens_in),
                    tokens_out: Some(tokens_out),
                    context_tokens: Some(context_tokens),
                    last_message: Some(format!("Running {tool} (mock activity #{n})")),
                    stopped_ts: Some(None),
                    ..Default::default()
                },
            );
            if n % 7 == 0 {
                store.upsert(
                    id,
                    SessionPatch {
                        stopped_ts: Some(Some(now_ms())),
                        current_tool: Some(None),
                        last_message: Some("Done (mock).".to_string()),
                        ..Default::default()
                    },
                );
            }
        }
    });
}

fn spawn_permission_cycle(pb: PermissionBridge, ids: Vec<String>, stop: Arc<Notify>) {
    tokio::spawn(async move {
        let mut perm_n = 0u64;
        let mut n = 0u64;
        loop {
            tokio::select! {
                _ = stop.notified() => break,
                _ = tokio::time::sleep(Duration::from_millis(60_000)) => {}
            }
            let id = &ids[0];
            let (mut tool, mut command) = TOOLS[(n as usize) % TOOLS.len()];
            n += 1;
            // Every other one is destructive, so the two-press arming chip
            // gets exercised in dev alongside the plain allow.
            if perm_n % 2 == 1 {
                tool = "Bash";
                command = "rm -rf node_modules && npm ci";
            }
            perm_n += 1;
            let (tx, rx) = oneshot::channel();
            pb.on_hook_request(
                &json!({
                    "session_id": id, "tool_name": tool, "tool_input": { "command": command },
                    "cwd": format!("/Users/dev/{}", NAMES[0]),
                }),
                tx,
            );
            tokio::spawn(async move {
                if let Ok(body) = rx.await {
                    log("MK", &format!("mock hook answered: {}", body.chars().take(80).collect::<String>()));
                }
            });
        }
    });
}

fn mock_questions(turn: usize) -> serde_json::Value {
    let sets = [
        json!([{
            "header": "Deploy target", "question": "Where should this build go?", "multiSelect": false,
            "options": [
                { "label": "Staging", "description": "Safe. Runs the smoke suite first." },
                { "label": "Production", "description": "Ships to users immediately." },
                { "label": "Skip deploy", "description": "Build only, keep the artifact." },
                { "label": "Preview branch", "description": "Ephemeral URL for review." },
            ],
        }]),
        json!([
            {
                "header": "Auth method", "question": "How should the API authenticate callers?", "multiSelect": false,
                "options": [
                    { "label": "OAuth", "description": "Delegated, no passwords stored here." },
                    { "label": "API keys", "description": "Simplest. Rotation is on you." },
                    { "label": "mTLS", "description": "Strongest, painful to operate." },
                ],
            },
            {
                "header": "Environments", "question": "Which environments get the new pipeline?", "multiSelect": true,
                "options": [
                    { "label": "Dev", "description": "Rebuilt on every push." },
                    { "label": "Staging", "description": "Mirrors production data shape." },
                    { "label": "Production", "description": "Customer traffic." },
                ],
            },
            {
                "header": "Rollout", "question": "How fast should it go out?", "multiSelect": false,
                "options": [
                    { "label": "All at once", "description": "One deploy, one blast radius." },
                    { "label": "Canary 10%", "description": "Watch an hour, then widen." },
                ],
            },
        ]),
    ];
    sets[turn % sets.len()].clone()
}

fn spawn_question_cycle(q: Queue, ids: Vec<String>, stop: Arc<Notify>) {
    tokio::spawn(async move {
        let mut turn = 0usize;
        loop {
            tokio::select! {
                _ = stop.notified() => break,
                _ = tokio::time::sleep(Duration::from_millis(90_000)) => {}
            }
            let questions = mock_questions(turn);
            turn += 1;
            q.on_question(&json!({
                "session_id": ids[1], "tool_name": "AskUserQuestion",
                "tool_input": { "questions": questions },
            }));
        }
    });
}
