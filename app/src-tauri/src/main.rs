// Claude Thing: a tray-first Tauri app. No dock icon, no window until asked
// for one — the tray is the primary surface, the control page is a click away.

mod daemon;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

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
                        // wired up once the tunnel supervisor exists (see queue.js/tunnel port)
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
    let _ = app;
    // TODO: open the daemon + tunnel log directories in Finder once their
    // on-disk locations are settled by the ported daemon.
}
