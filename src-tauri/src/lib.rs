//! PingMyBell — free, open-source voice notifications and command center
//! for AI coding agents (Claude Code, Codex CLI).
//!
//! App bootstrap: tray icon, hidden board window, ingest server, registry.

mod ingest;
mod registry;

use std::sync::Arc;

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .setup(|app| {
            // Tray-resident app: no Dock icon on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let db_path = ingest::data_dir()?.join("pingmybell.db");
            let registry = Arc::new(registry::Registry::open(&db_path)?);
            app.manage(registry.clone());

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = ingest::serve(handle, registry).await {
                    log::error!("ingest server exited: {err}");
                }
            });

            let open = MenuItemBuilder::with_id("open-board", "Open Board").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit PingMyBell").build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&open)
                .separator()
                .item(&quit)
                .build()?;
            TrayIconBuilder::with_id("main")
                .icon(
                    app.default_window_icon()
                        .expect("bundle icon configured")
                        .clone(),
                )
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open-board" => {
                        if let Some(window) = app.get_webview_window("board") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        // Closing the board hides it; monitoring continues in the tray (AC-7.2).
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building PingMyBell")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                // Stale discovery files would point shims at a port another
                // process could later claim; remove them on clean shutdown.
                if let Ok(dir) = ingest::data_dir() {
                    let _ = std::fs::remove_file(dir.join("port"));
                    let _ = std::fs::remove_file(dir.join("token"));
                }
            }
        });
}
