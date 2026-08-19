// Claude Thing: a tray-first Tauri app. No dock icon, no window until asked
// for one — the tray is the primary surface, the control page is a click away.

mod daemon;
mod tunnel;

use daemon::config::{port, HOST};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIcon,
    tray::TrayIconBuilder,
    Manager,
};
use tauri_plugin_autostart::ManagerExt;
use tunnel::{Tunnel, TunnelStatus};

struct DaemonUp(Arc<AtomicBool>);

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

            let daemon_up = Arc::new(AtomicBool::new(false));
            app.manage(DaemonUp(daemon_up.clone()));

            let tunnel = Tunnel::new();
            app.manage(tunnel.clone());
            // spawn_supervisor calls tokio::spawn internally, which panics if
            // it isn't already running inside a tokio task — .setup() itself
            // is a plain synchronous callback, so the kickoff needs its own
            // async_runtime::spawn wrapper.
            let tunnel_for_start = tunnel.clone();
            tauri::async_runtime::spawn(async move {
                tunnel_for_start.spawn_supervisor(port());
            });

            // A raw `kill`/SIGTERM bypasses Rust's Drop entirely (no signal
            // handler means the OS just terminates the process outright), so
            // kill_on_drop on the ssh child never gets a chance to run unless
            // we catch the signal ourselves and tear the tunnel down first —
            // the same reason mac/tunnel.sh trapped TERM/INT.
            #[cfg(unix)]
            {
                let tunnel_for_signal = tunnel.clone();
                tauri::async_runtime::spawn(async move {
                    use tokio::signal::unix::{signal, SignalKind};
                    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
                    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
                    tokio::select! {
                        _ = term.recv() => {}
                        _ = int.recv() => {}
                    }
                    tunnel_for_signal.quit();
                    std::process::exit(0);
                });
            }

            let handle = app.handle().clone();
            let paths = resolve_paths(&handle);
            let scripts_dir = paths.scripts_dir.clone();
            let daemon_up_task = daemon_up.clone();
            tauri::async_runtime::spawn(async move {
                let d = daemon::runtime::start(paths);
                let state = daemon::http_server::AppState {
                    hub: d.hub.clone(),
                    store: d.store.clone(),
                    permission_bridge: d.permission_bridge.clone(),
                    sources: d.sources.clone(),
                    scripts_dir,
                };
                let ready_flag = daemon_up_task.clone();
                let result = daemon::http_server::serve(state, HOST, port(), move || {
                    ready_flag.store(true, Ordering::SeqCst);
                })
                .await;
                daemon_up_task.store(false, Ordering::SeqCst);
                if let Err(err) = result {
                    daemon::log::log("--", &format!("http server failed: {err}"));
                }
            });

            let open_item = MenuItem::with_id(app, "open", "Open Dashboard", true, None::<&str>)?;
            let logs_item = MenuItem::with_id(app, "logs", "View Logs", true, None::<&str>)?;
            let restart_item =
                MenuItem::with_id(app, "restart_tunnel", "Restart Tunnel", true, None::<&str>)?;
            let login_item_checked = app.autolaunch().is_enabled().unwrap_or(false);
            let login_item = CheckMenuItem::with_id(
                app,
                "launch_at_login",
                "Launch at Login",
                true,
                login_item_checked,
                None::<&str>,
            )?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &restart_item,
                    &logs_item,
                    &separator,
                    &login_item,
                    &separator,
                    &quit_item,
                ],
            )?;

            let tray: TrayIcon = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .tooltip("Claude Thing")
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => show_dashboard(app),
                    "quit" => {
                        app.state::<Arc<Tunnel>>().quit();
                        app.exit(0);
                    }
                    "restart_tunnel" => app.state::<Arc<Tunnel>>().restart_now(),
                    "logs" => open_logs(app),
                    "launch_at_login" => toggle_launch_at_login(app, &login_item),
                    _ => {}
                })
                .build(app)?;

            let tunnel_for_poll = tunnel.clone();
            let daemon_up_for_poll = daemon_up.clone();
            // Same app icon, tinted per status, instead of a colored-blob
            // emoji title next to it — grey while scanning, red when the
            // daemon itself isn't up, green once the tunnel is connected.
            let icon_grey = Image::from_bytes(include_bytes!("../icons/tray/grey.png")).expect("tray icon grey.png");
            let icon_red = Image::from_bytes(include_bytes!("../icons/tray/red.png")).expect("tray icon red.png");
            let icon_green = Image::from_bytes(include_bytes!("../icons/tray/green.png")).expect("tray icon green.png");
            tauri::async_runtime::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(2));
                loop {
                    ticker.tick().await;
                    let up = daemon_up_for_poll.load(Ordering::SeqCst);
                    let (icon, tooltip) = match (up, tunnel_for_poll.status()) {
                        (false, _) => (&icon_red, "Claude Thing — daemon offline".to_string()),
                        (true, TunnelStatus::Scanning) => {
                            (&icon_grey, "Claude Thing — waiting for the Car Thing".to_string())
                        }
                        (true, TunnelStatus::Connected { ip }) => {
                            (&icon_green, format!("Claude Thing — connected ({ip})"))
                        }
                    };
                    let _ = tray.set_icon(Some(icon.clone()));
                    let _ = tray.set_tooltip(Some(&tooltip));
                }
            });

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

fn toggle_launch_at_login(app: &tauri::AppHandle, item: &CheckMenuItem<tauri::Wry>) {
    let autostart = app.autolaunch();
    let now_enabled = autostart.is_enabled().unwrap_or(false);
    let result = if now_enabled { autostart.disable() } else { autostart.enable() };
    if result.is_ok() {
        let _ = item.set_checked(!now_enabled);
    }
}
