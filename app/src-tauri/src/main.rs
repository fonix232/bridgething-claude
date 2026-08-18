// Claude Thing: a tray-first Tauri app. No dock icon, no window until asked
// for one — the tray is the primary surface, the control page is a click away.

mod daemon;

use daemon::config::{port, HOST};
use std::path::PathBuf;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};

// Resolved from Tauri's own app-data dir for logs/state (works whether this
// runs from the git checkout or an installed .app), except scripts_dir:
// daemon/scripts/{install,uninstall}-hooks.js are only ever found relative to
// this checkout right now — see daemon::http_server's doc comment. Fine for
// development; an installed, relocated .app would need this resolved some
// other way (not yet built).
fn resolve_paths(app: &tauri::AppHandle) -> daemon::runtime::Paths {
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("com.claudething.app"));
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    daemon::runtime::Paths {
        log_dir: base.join("logs"),
        state_dir: base.join("state"),
        scripts_dir: project_root.join("daemon").join("scripts"),
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let paths = resolve_paths(&handle);
            let scripts_dir = paths.scripts_dir.clone();
            tauri::async_runtime::spawn(async move {
                let d = daemon::runtime::start(paths);
                let state = daemon::http_server::AppState {
                    hub: d.hub.clone(),
                    store: d.store.clone(),
                    permission_bridge: d.permission_bridge.clone(),
                    sources: d.sources.clone(),
                    scripts_dir,
                };
                if let Err(err) = daemon::http_server::serve(state, HOST, port()).await {
                    daemon::log::log("--", &format!("http server failed: {err}"));
                }
            });

            let open_item = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)?;
            let logs_item = MenuItem::with_id(app, "logs", "View Logs", true, None::<&str>)?;
            let restart_item =
                MenuItem::with_id(app, "restart_tunnel", "Restart Tunnel", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &restart_item,
                    &logs_item,
                    &separator,
                    &quit_item,
                ],
            )?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .title("\u{26AA}") // white circle: status unknown until the first poll
                .tooltip("Claude Thing")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_dashboard(app),
                    "quit" => app.exit(0),
                    "restart_tunnel" => {
                        // wired up once the tunnel supervisor exists (see mac/tunnel.sh port)
                    }
                    "logs" => open_logs(app),
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the dashboard window hides it instead of quitting the app —
            // the tray, not the window, owns the app's lifetime.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                window.hide().ok();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running claude-thing-app");
}

fn show_dashboard(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        window.show().ok();
        window.set_focus().ok();
    }
}

fn open_logs(app: &tauri::AppHandle) {
    let paths = resolve_paths(app);
    let _ = std::process::Command::new("open").arg(paths.log_dir).spawn();
}
