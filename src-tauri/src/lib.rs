//! PingMyBell — free, open-source voice notifications and command center
//! for AI coding agents (Claude Code, Codex CLI).
//!
//! App bootstrap: tray icon, hidden board window, ingest server, registry,
//! speaker. Also exposes headless `install-claude` / `uninstall-claude`
//! CLI entry points (see main.rs).

mod adapters;
mod ingest;
mod overlay;
mod platform;
mod registry;
mod speaker;
mod summarize;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
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

            let speaker = speaker::spawn();
            app.manage(speaker.clone());

            // Overlay failure degrades to voice-only: ingest and the speaker
            // are independent of it, and killing the whole app over a window
            // styling error would contradict the fail-open posture.
            let overlay = match overlay::init(app.handle(), registry.clone()) {
                Ok(overlay) => {
                    app.manage(overlay.clone());
                    Some(overlay)
                }
                Err(err) => {
                    log::error!("overlay disabled (init failed): {err}");
                    None
                }
            };

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = ingest::serve(handle, registry, speaker, overlay).await {
                    log::error!("ingest server exited: {err}");
                }
            });

            let mute = CheckMenuItemBuilder::with_id("mute", "Mute")
                .checked(false)
                .build(app)?;
            let install =
                MenuItemBuilder::with_id("install-claude", "Install Claude Code Integration")
                    .build(app)?;
            let uninstall =
                MenuItemBuilder::with_id("uninstall-claude", "Uninstall Claude Code Integration")
                    .build(app)?;
            let open = MenuItemBuilder::with_id("open-board", "Open Board").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit PingMyBell").build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&open)
                .item(&mute)
                .separator()
                .item(&install)
                .item(&uninstall)
                .separator()
                .item(&quit)
                .build()?;

            let mute_item = mute.clone();
            TrayIconBuilder::with_id("main")
                .icon(
                    app.default_window_icon()
                        .expect("bundle icon configured")
                        .clone(),
                )
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open-board" => {
                        if let Some(window) = app.get_webview_window("board") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "mute" => {
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        let checked = mute_item.is_checked().unwrap_or(false);
                        speaker.set_muted(checked);
                        log::info!("mute set to {checked}");
                    }
                    "install-claude" => {
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        match install_claude() {
                            Ok(report) => {
                                log::info!(
                                    "Claude Code integration installed into {}",
                                    report.settings_path.display()
                                );
                                speak_status(&speaker, "Claude Code integration installed.");
                            }
                            Err(err) => {
                                log::error!("install failed: {err}");
                                speak_status(&speaker, "Install failed. Check the logs.");
                            }
                        }
                    }
                    "uninstall-claude" => {
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        match uninstall_claude() {
                            Ok(()) => {
                                log::info!("Claude Code integration removed");
                                speak_status(&speaker, "Claude Code integration removed.");
                            }
                            Err(err) => {
                                log::error!("uninstall failed: {err}");
                                speak_status(&speaker, "Uninstall failed. Check the logs.");
                            }
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        // The overlay webview loads after setup's first emit; replay the
        // current state once the page is actually listening.
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Finished
                && webview.label() == "overlay"
            {
                if let Some(overlay) = webview.try_state::<Arc<overlay::Overlay>>() {
                    overlay.refresh();
                }
            }
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

fn speak_status(speaker: &speaker::SpeakerHandle, text: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // Unique session id per status message: two tray actions in quick
    // succession must both be voiced, not deduped against each other.
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    speaker.enqueue(speaker::Utterance {
        priority: speaker::Priority::Attention,
        session_id: format!("pingmybell-status-{seq}"),
        agent: registry::AgentKind::ClaudeCode,
        text: text.into(),
    });
}

fn claude_settings_path() -> io::Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".claude").join("settings.json"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))
}

/// The shim ships next to the app binary (same target dir in dev, same
/// bundle dir in release).
fn shim_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "app binary has no parent dir"))?;
    let shim = dir.join(if cfg!(windows) {
        "pingmybell-shim.exe"
    } else {
        "pingmybell-shim"
    });
    if !shim.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("shim binary not found at {}", shim.display()),
        ));
    }
    Ok(shim)
}

fn install_claude() -> io::Result<pingmybell_installers::InstallReport> {
    pingmybell_installers::claude_code::install(&shim_path()?, &claude_settings_path()?)
}

fn uninstall_claude() -> io::Result<()> {
    pingmybell_installers::claude_code::uninstall(&claude_settings_path()?)
}

/// Headless install for scripting/tests: `pingmybell install-claude`.
pub fn cli_install_claude() -> i32 {
    match install_claude() {
        Ok(report) => {
            println!(
                "Installed Claude Code hooks into {}",
                report.settings_path.display()
            );
            if let Some(backup) = report.backup_path {
                println!("Previous settings backed up to {}", backup.display());
            }
            println!("Events: {}", report.events.join(", "));
            0
        }
        Err(err) => {
            eprintln!("install failed: {err}");
            1
        }
    }
}

/// Headless uninstall: `pingmybell uninstall-claude`.
pub fn cli_uninstall_claude() -> i32 {
    match uninstall_claude() {
        Ok(()) => {
            println!("Removed PingMyBell hooks from Claude Code settings");
            0
        }
        Err(err) => {
            eprintln!("uninstall failed: {err}");
            1
        }
    }
}
