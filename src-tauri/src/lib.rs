//! PingMyBell — free, open-source voice notifications and command center
//! for AI coding agents (Claude Code, Codex CLI).
//!
//! App bootstrap: tray icon, hidden board window, ingest server, registry,
//! speaker. Also exposes headless `install-claude` / `uninstall-claude`
//! CLI entry points (see main.rs).

mod adapters;
mod broker;
mod config;
mod focus;
mod ingest;
mod overlay;
mod platform;
mod registry;
mod reply;
mod speaker;
mod summarize;
mod titles;
mod tmux;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            decide,
            answer_question,
            defer_question,
            open_reply,
            keep_question_alive,
            pending_reply,
            submit_reply,
            cancel_reply,
            overlay_hover,
            dismiss_attention,
            focus_session,
            board_snapshot,
            session_history,
            delete_session,
            list_voices,
            get_settings,
            set_voice,
            set_gate
        ])
        .setup(|app| {
            // Tray-resident app: no Dock icon on macOS.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            config::ensure_defaults();

            let db_path = ingest::data_dir()?.join("pingmybell.db");
            // Starts empty and stays that way until the first background
            // scan: reading the desktop app's session store is disk work and
            // setup runs on the main thread.
            let title_index = titles::TitleIndex::empty();
            let registry = Arc::new(registry::Registry::open(&db_path, title_index.clone())?);
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

            let broker = Arc::new(broker::Broker::default());
            app.manage(broker.clone());

            // Typed answers live in their own focusable window; it stays
            // hidden until a user click asks for it.
            app.manage(Arc::new(reply::ReplyController::new(
                app.handle().clone(),
            )));

            // Idle WAL maintenance. This app runs for days; without it the
            // write-ahead log parks at its high-water mark and never gives
            // the space back.
            let checkpoint_registry = registry.clone();
            tauri::async_runtime::spawn(async move {
                // Reclaim once shortly after startup — a restart should give
                // back whatever the last run left behind — then settle into a
                // slow cadence. The initial delay keeps this off the critical
                // path while recovery runs.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(10 * 60));
                loop {
                    tick.tick().await;
                    let registry = checkpoint_registry.clone();
                    // Checkpointing is blocking disk I/O — keep it off the
                    // async workers that agents are parked against.
                    let _ = tauri::async_runtime::spawn_blocking(move || registry.checkpoint())
                        .await;
                }
            });

            // Session names live in the Claude desktop app's own store, which
            // only changes when a session is created, renamed, or removed.
            // Poll it instead of watching the filesystem: the files are tiny,
            // and the cost of a missed rename is one cycle of a stale label.
            let title_registry = registry.clone();
            let title_overlay = overlay.clone();
            tauri::async_runtime::spawn(async move {
                let scanner = Arc::new(titles::TitleScanner::new(&title_index));
                // Fires immediately, so a restart picks up names before the
                // first agent event arrives.
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                loop {
                    tick.tick().await;
                    let scanner = scanner.clone();
                    // A directory scan is blocking disk I/O — keep it off the
                    // async workers that agents are parked against.
                    let changed = tauri::async_runtime::spawn_blocking(move || scanner.scan_once())
                        .await
                        .unwrap_or(false);
                    if !changed {
                        continue;
                    }
                    let registry = title_registry.clone();
                    let moved = tauri::async_runtime::spawn_blocking(move || registry.retitle())
                        .await
                        .unwrap_or(false);
                    if moved {
                        if let Some(overlay) = &title_overlay {
                            overlay.refresh();
                        }
                    }
                }
            });

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = ingest::serve(handle, registry, speaker, overlay, broker).await {
                    log::error!("ingest server exited: {err}");
                }
            });

            let mute = CheckMenuItemBuilder::with_id("mute", "Mute")
                .checked(false)
                .build(app)?;
            let gate = CheckMenuItemBuilder::with_id("gate", "Gate Claude Tool Calls")
                .checked(config::gate_tool_calls())
                .build(app)?;
            // Separate from the Claude gate on purpose, and three-state rather
            // than a checkbox: an approval belongs wherever its context is, so
            // the useful default is to MIRROR whatever the user already told
            // Codex rather than to override it (§5.2.3).
            let gate_codex_now = config::codex_gate();
            let gate_codex_auto = CheckMenuItemBuilder::with_id(
                "gate-codex-auto",
                "Match My Codex Setting (recommended)",
            )
            .checked(gate_codex_now == config::CodexGate::Auto)
            .build(app)?;
            let gate_codex_always = CheckMenuItemBuilder::with_id("gate-codex-always", "Always")
                .checked(gate_codex_now == config::CodexGate::Always)
                .build(app)?;
            let gate_codex_never = CheckMenuItemBuilder::with_id("gate-codex-never", "Never")
                .checked(gate_codex_now == config::CodexGate::Never)
                .build(app)?;
            let gate_codex = SubmenuBuilder::new(app, "Approve Codex Commands From Overlay")
                .item(&gate_codex_auto)
                .item(&gate_codex_always)
                .item(&gate_codex_never)
                .build()?;
            let autostart_enabled = {
                use tauri_plugin_autostart::ManagerExt;
                app.autolaunch().is_enabled().unwrap_or(false)
            };
            let login = CheckMenuItemBuilder::with_id("login", "Launch at Login")
                .checked(autostart_enabled)
                .build(app)?;
            let install =
                MenuItemBuilder::with_id("install-claude", "Install Claude Code Integration")
                    .build(app)?;
            let uninstall =
                MenuItemBuilder::with_id("uninstall-claude", "Uninstall Claude Code Integration")
                    .build(app)?;
            let install_codex_item =
                MenuItemBuilder::with_id("install-codex", "Install Codex Integration")
                    .build(app)?;
            let uninstall_codex_item =
                MenuItemBuilder::with_id("uninstall-codex", "Uninstall Codex Integration")
                    .build(app)?;
            let install_codex_hooks_item = MenuItemBuilder::with_id(
                "install-codex-hooks",
                "Install Codex Hooks (needs approval in Codex)",
            )
            .build(app)?;
            let uninstall_codex_hooks_item =
                MenuItemBuilder::with_id("uninstall-codex-hooks", "Uninstall Codex Hooks")
                    .build(app)?;
            let open = MenuItemBuilder::with_id("open-board", "Open Board").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit PingMyBell").build(app)?;
            let menu = MenuBuilder::new(app)
                .item(&open)
                .item(&mute)
                .item(&gate)
                .item(&gate_codex)
                .item(&login)
                .separator()
                .item(&install)
                .item(&uninstall)
                .item(&install_codex_item)
                .item(&uninstall_codex_item)
                .item(&install_codex_hooks_item)
                .item(&uninstall_codex_hooks_item)
                .separator()
                .item(&quit)
                .build()?;

            let mute_item = mute.clone();
            let gate_item = gate.clone();
            // The three Codex-gate items behave as radio buttons: whichever is
            // clicked wins and the other two are cleared. (Tauri has no radio
            // menu item on every platform, and a click always toggles the item
            // it landed on — including re-clicking the active one — so the
            // handler restates all three from the setting rather than trusting
            // the checkmarks.)
            let gate_codex_items = [
                (config::CodexGate::Auto, gate_codex_auto.clone()),
                (config::CodexGate::Always, gate_codex_always.clone()),
                (config::CodexGate::Never, gate_codex_never.clone()),
            ];
            let login_item = login.clone();
            let tray = TrayIconBuilder::with_id("main");
            // macOS: monochrome template silhouette so the system recolors it
            // to match the menu bar (light/dark) like every other status icon.
            #[cfg(target_os = "macos")]
            let tray = tray
                .icon(tauri::include_image!("icons/tray.png"))
                .icon_as_template(true);
            #[cfg(not(target_os = "macos"))]
            let tray = tray.icon(
                app.default_window_icon()
                    .expect("bundle icon configured")
                    .clone(),
            );
            tray.menu(&menu)
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
                    "gate" => {
                        let checked = gate_item.is_checked().unwrap_or(false);
                        config::set_gate_tool_calls(checked);
                        log::info!("gate_tool_calls set to {checked}");
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        speak_status(
                            &speaker,
                            if checked {
                                "Claude tool gating on."
                            } else {
                                "Claude tool gating off."
                            },
                        );
                    }
                    id @ ("gate-codex-auto" | "gate-codex-always" | "gate-codex-never") => {
                        let gate = match id {
                            "gate-codex-always" => config::CodexGate::Always,
                            "gate-codex-never" => config::CodexGate::Never,
                            _ => config::CodexGate::Auto,
                        };
                        config::set_codex_gate(gate);
                        for (owned, item) in &gate_codex_items {
                            if let Err(err) = item.set_checked(*owned == gate) {
                                // The setting is already persisted; only the
                                // menu's tick is wrong, and it is rebuilt from
                                // the config on next launch.
                                log::warn!("could not restate the Codex gate menu: {err}");
                            }
                        }
                        log::info!("gate_codex_approvals set to {}", gate.as_str());
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        speak_status(
                            &speaker,
                            match gate {
                                config::CodexGate::Auto => "Codex approvals follow your Codex setting.",
                                config::CodexGate::Always => "Codex approvals always on.",
                                config::CodexGate::Never => "Codex approvals off.",
                            },
                        );
                    }
                    "login" => {
                        use tauri_plugin_autostart::ManagerExt;
                        let checked = login_item.is_checked().unwrap_or(false);
                        let autolaunch = app.autolaunch();
                        let result = if checked {
                            autolaunch.enable()
                        } else {
                            autolaunch.disable()
                        };
                        match result {
                            Ok(()) => log::info!("launch at login set to {checked}"),
                            Err(err) => log::error!("autostart toggle failed: {err}"),
                        }
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
                    "install-codex" => {
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        match install_codex() {
                            Ok(report) => {
                                log::info!(
                                    "Codex integration installed into {}",
                                    report.settings_path.display()
                                );
                                speak_status(&speaker, "Codex integration installed.");
                            }
                            Err(err) => {
                                log::error!("codex install failed: {err}");
                                speak_status(&speaker, "Codex install failed. Check the logs.");
                            }
                        }
                    }
                    "uninstall-codex" => {
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        match uninstall_codex() {
                            Ok(()) => {
                                log::info!("Codex integration removed");
                                speak_status(&speaker, "Codex integration removed.");
                            }
                            Err(err) => {
                                log::error!("codex uninstall failed: {err}");
                                speak_status(&speaker, "Codex uninstall failed. Check the logs.");
                            }
                        }
                    }
                    "install-codex-hooks" => {
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        match install_codex_hooks() {
                            Ok(report) => {
                                log::info!(
                                    "Codex hooks installed into {} ({}) — {}",
                                    report.settings_path.display(),
                                    report.events.join(", "),
                                    CODEX_HOOK_TRUST_NOTE
                                );
                                // The trust step is not optional: without it
                                // the hooks silently never run and PingMyBell
                                // looks broken. Say so out loud. Note the
                                // plural: Codex trusts hooks individually, so
                                // there are two entries to approve.
                                speak_status(
                                    &speaker,
                                    "Codex hooks installed. \
                                     You still need to approve both of them in Codex settings, under Hooks.",
                                );
                            }
                            Err(err) => {
                                log::error!("codex hook install failed: {err}");
                                speak_status(
                                    &speaker,
                                    "Codex hook install failed. Check the logs.",
                                );
                            }
                        }
                    }
                    "uninstall-codex-hooks" => {
                        let speaker = app.state::<speaker::SpeakerHandle>();
                        match uninstall_codex_hooks() {
                            Ok(()) => {
                                log::info!("Codex hooks removed");
                                speak_status(&speaker, "Codex hooks removed.");
                            }
                            Err(err) => {
                                log::error!("codex hook uninstall failed: {err}");
                                speak_status(
                                    &speaker,
                                    "Codex hook uninstall failed. Check the logs.",
                                );
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

/// Overlay button → approval resolution (AC-6.2). Completes the parked
/// `/v1/approval` response, records the decision in session history, voices
/// it, and unpins the card.
// Async so it runs on the tokio runtime, not the main thread: overlay window
// syncing blocks on main-thread getters, and a sync command holding the
// window-ops lock on the main thread could deadlock against them.
#[tauri::command]
async fn decide(
    app: tauri::AppHandle,
    approval_id: String,
    decision: String,
) -> Result<(), String> {
    let Some(decision) = broker::Decision::parse(&decision) else {
        return Err(format!("unknown decision {decision:?}"));
    };
    let broker = app.state::<Arc<broker::Broker>>();
    let Some(info) = broker.decide(&approval_id, decision) else {
        // Timed out, double-clicked, or the shim's connection died: whatever
        // happened, make sure no stale card lingers.
        log::info!("decide: approval {approval_id} no longer pending");
        if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
            overlay.unpin_approval(&approval_id);
        }
        return Ok(());
    };
    log::info!(
        "decide: {} for approval {} (session {})",
        decision.as_str(),
        info.id,
        info.session_id
    );

    // Only flip the session back to Working when no sibling approval is
    // still parked for it.
    let resume = !broker.has_pending_for_session(&info.session_id);
    let registry = app.state::<Arc<registry::Registry>>();
    match registry.record_decision(&info.session_id, info.event_id, decision.as_str(), resume) {
        Ok(Some(session)) => {
            if let Err(err) = tauri::Emitter::emit(&app, "session-updated", &session) {
                log::warn!("failed to emit session-updated: {err}");
            }
        }
        Ok(None) => {}
        Err(err) => log::error!("failed to record decision: {err}"),
    }

    let speaker = app.state::<speaker::SpeakerHandle>();
    speaker.enqueue(speaker::Utterance {
        priority: speaker::Priority::Attention,
        session_id: info.session_id.clone(),
        agent: info.agent,
        text: speaker::decision_text(decision.as_str(), &info.tool_name, &info.title),
    });

    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.unpin_approval(&info.id);
    }
    Ok(())
}

/// Answer a parked `AskUserQuestion` from the overlay card (FR-6 sibling).
///
/// The card owns gathering the selections — including any free text routed
/// back from the reply window — and submits the whole set at once, so a
/// multi-question call reaches the agent as one answer.
#[tauri::command]
async fn answer_question(
    app: tauri::AppHandle,
    question_id: String,
    answers: Vec<broker::Answer>,
) -> Result<(), String> {
    let broker = app.state::<Arc<broker::Broker>>();
    match broker.answer(&question_id, broker::QuestionAnswer { answers }) {
        broker::AnswerResult::Accepted(info) => {
            log::info!(
                "answer: question {} answered (session {})",
                info.id,
                info.session_id
            );
            // Only resume the session when nothing else is still parked for
            // it — a sibling approval or question keeps it waiting.
            let resume = !broker.has_pending_for_session(&info.session_id);
            let registry = app.state::<Arc<registry::Registry>>();
            match registry.record_decision(&info.session_id, info.event_id, "answered", resume) {
                Ok(Some(session)) => {
                    if let Err(err) = tauri::Emitter::emit(&app, "session-updated", &session) {
                        log::warn!("failed to emit session-updated: {err}");
                    }
                }
                Ok(None) => {}
                Err(err) => log::error!("failed to record answer: {err}"),
            }

            let speaker = app.state::<speaker::SpeakerHandle>();
            speaker.enqueue(speaker::Utterance {
                priority: speaker::Priority::Attention,
                session_id: info.session_id.clone(),
                agent: info.agent,
                text: format!("Answered {}.", info.title),
            });

            if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
                overlay.unpin_question(&info.id);
            }
            close_reply(&app, &question_id);
            Ok(())
        }
        // Still parked: keep the card up so the user can try again. The Err
        // is what re-enables the card's buttons.
        broker::AnswerResult::Rejected => {
            log::info!("answer: nothing usable for question {question_id}; still parked");
            Err("no answer selected".into())
        }
        broker::AnswerResult::Gone => {
            log::info!("answer: question {question_id} no longer pending");
            if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
                overlay.unpin_question(&question_id);
            }
            close_reply(&app, &question_id);
            Ok(())
        }
    }
}

/// "Answer in the terminal" ✕ on the question card: stop parking and let
/// Claude Code render its own selector (the 204 path, without the wait).
#[tauri::command]
async fn defer_question(app: tauri::AppHandle, question_id: String) {
    let broker = app.state::<Arc<broker::Broker>>();
    if broker.defer_question(&question_id).is_none() {
        log::info!("defer: question {question_id} no longer pending");
    }
    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.unpin_question(&question_id);
    }
    close_reply(&app, &question_id);
}

/// Open the focusable reply window for one question of a parked call.
///
/// Opening it is the first hard evidence that a human is composing an answer,
/// so it also buys the park more time — typing a paragraph takes longer than
/// the base park, and losing the question (and the paragraph) mid-sentence is
/// the bug this extension exists to prevent.
#[tauri::command]
async fn open_reply(app: tauri::AppHandle, prompt: reply::ReplyPrompt) {
    let broker = app.state::<Arc<broker::Broker>>();
    if broker
        .extend_question(&prompt.id, ingest::TYPING_EXTENSION)
        .is_none()
    {
        // The question died between the click and this call (a stale card).
        // Do NOT open: a focusable window nobody can answer through would
        // float over the island, and `reply_open` would have no owner left to
        // ever clear it. The card is being unpinned anyway.
        log::info!("reply: question {} no longer parked; not opening", prompt.id);
        return;
    }
    app.state::<Arc<reply::ReplyController>>().open(prompt);
    // Hide the card behind it: the reply window repeats the question, and a
    // wider card peeking out around its edges reads as a rendering bug.
    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.set_reply_open(true);
    }
}

/// Heartbeat from the open reply window: the user is still there, still
/// typing. Pushes the park deadline out, bounded by the ceiling that keeps
/// the shim's answer inside the agent's hook timeout.
///
/// Returns the seconds still left on the park, or `None` when it is over
/// (expired, answered elsewhere, or the agent went away) — a heartbeat can
/// never resurrect a dead park. As the ceiling approaches this number stops
/// growing and starts shrinking, which is how the window knows to warn the
/// user instead of letting the banner arrive mid-sentence.
#[tauri::command]
async fn keep_question_alive(app: tauri::AppHandle, question_id: String) -> Option<u64> {
    app.state::<Arc<broker::Broker>>()
        .extend_question(&question_id, ingest::TYPING_EXTENSION)
        .map(|remaining| remaining.as_secs())
}

/// Prompt for the reply webview's cold-load path.
#[tauri::command]
async fn pending_reply(app: tauri::AppHandle) -> Option<reply::ReplyPrompt> {
    app.state::<Arc<reply::ReplyController>>().pending()
}

/// Typed answer: routed back to the question card, which owns submission.
/// The reply window never answers the broker directly, so there is exactly
/// one path to an answer regardless of how the user produced it.
#[tauri::command]
async fn submit_reply(
    app: tauri::AppHandle,
    id: String,
    question_index: usize,
    text: String,
) -> Result<(), String> {
    // Buy the round trip through the overlay card (which owns submission)
    // some room, so an answer typed right on the deadline is not lost to the
    // few milliseconds it takes to get there.
    app.state::<Arc<broker::Broker>>()
        .extend_question(&id, ingest::TYPING_EXTENSION);

    let controller = app.state::<Arc<reply::ReplyController>>();
    // Match the index too: one question id covers every question of a
    // multi-question call.
    if !controller.is_current(&id, question_index) {
        // A newer question (or a later question of the same call) replaced
        // this prompt while it was being typed.
        log::info!("reply: stale submit for question {id}[{question_index}]");
        return Err("this question is no longer waiting".into());
    }
    // Hand the text off BEFORE clearing: if the overlay is gone the user
    // keeps their window and their typing, and the Err re-enables Send.
    tauri::Emitter::emit_to(
        &app,
        "overlay",
        "reply-answer",
        serde_json::json!({ "question_id": id, "question_index": question_index, "text": text }),
    )
    .map_err(|err| err.to_string())?;
    controller.clear_if_current(&id);
    controller.close();
    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.set_reply_open(false);
    }
    Ok(())
}

/// Dismiss the reply window without answering; the card stays pinned. Also
/// the Close button on an expired draft, which has no pending prompt left.
#[tauri::command]
async fn cancel_reply(app: tauri::AppHandle, id: String) {
    // Refuses to act when a NEWER question has taken the window over — an Esc
    // that was really meant for the previous prompt must not hide the one the
    // user is now looking at.
    if !app.state::<Arc<reply::ReplyController>>().dismiss(&id) {
        log::info!("reply: stale cancel for question {id}");
        return;
    }
    // Cancelled: the question is still parked, so bring its card back.
    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.set_reply_open(false);
    }
}

/// Close the reply window if it is still showing `question_id` — used when a
/// question stops being answerable while its reply window is open.
fn close_reply(app: &tauri::AppHandle, question_id: &str) {
    reply::close_for(app, question_id);
}

/// Pointer entered/left the island (async: same main-thread deadlock
/// avoidance as `decide`).
#[tauri::command]
async fn overlay_hover(app: tauri::AppHandle, hovering: bool) {
    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.set_hover(hovering);
    }
}

/// System voices for the settings UI (English first).
#[tauri::command]
async fn list_voices() -> Vec<String> {
    tauri::async_runtime::spawn_blocking(speaker::available_voices)
        .await
        .unwrap_or_default()
}

/// Current user settings for the board's settings panel.
#[tauri::command]
async fn get_settings() -> serde_json::Value {
    serde_json::json!({
        "gate_tool_calls": config::gate_tool_calls(),
        "gate_codex_approvals": config::codex_gate().as_str(),
        "voice_claude": config::voice_for("claude-code"),
        "voice_codex": config::voice_for("codex"),
    })
}

/// Pick a voice for an agent and speak a short sample in it (AC-4.2).
#[tauri::command]
async fn set_voice(app: tauri::AppHandle, agent: String, voice: String) -> Result<(), String> {
    if !matches!(agent.as_str(), "claude-code" | "codex") {
        return Err(format!("unknown agent {agent:?}"));
    }
    config::set_voice(&agent, &voice);
    let speaker = app.state::<speaker::SpeakerHandle>();
    let kind = if agent == "codex" {
        registry::AgentKind::Codex
    } else {
        registry::AgentKind::ClaudeCode
    };
    speaker.enqueue(speaker::Utterance {
        priority: speaker::Priority::Attention,
        session_id: format!("voice-sample-{agent}"),
        agent: kind,
        text: format!("This is {voice}."),
    });
    Ok(())
}

/// Toggle tool-call gating from the board (mirrors the tray item; the tray
/// checkbox reflects it on next launch).
#[tauri::command]
async fn set_gate(enabled: bool) {
    config::set_gate_tool_calls(enabled);
}

/// Full board state on window load (live rows + latest summaries).
#[tauri::command]
async fn board_snapshot(app: tauri::AppHandle) -> Vec<registry::BoardRow> {
    app.state::<Arc<registry::Registry>>().board_rows()
}

/// Per-session history drawer (last 50 events, newest first).
#[tauri::command]
async fn session_history(
    app: tauri::AppHandle,
    session_id: String,
) -> Result<Vec<registry::HistoryEvent>, String> {
    app.state::<Arc<registry::Registry>>()
        .history(&session_id, 50)
        .map_err(|e| e.to_string())
}

/// Jump to a session's terminal (FR-8): overlay row / board click.
#[tauri::command]
async fn focus_session(app: tauri::AppHandle, session_id: String) {
    let registry = app.state::<Arc<registry::Registry>>();
    match registry.get(&session_id) {
        // `jump` shells out (tmux queries plus a bounded walk of `ps`), so it
        // must not run on a Tokio worker — each child process is only
        // timeout-bounded, and blocking the runtime here would stall the
        // ingest server that agents are parked against.
        Some(session) => {
            if let Err(err) =
                tauri::async_runtime::spawn_blocking(move || focus::jump(&session)).await
            {
                log::warn!("focus: jump task failed: {err}");
            }
        }
        None => log::info!("focus: unknown session {session_id}"),
    }
    // Tuck the island away — the user is leaving for the terminal.
    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.set_hover(false);
    }
}

/// Forget a session: remove the row and its history for good.
///
/// The board is the only surface that offers this, and it makes the user
/// type the session's name first — this is the one irreversible action in
/// the app.
#[tauri::command]
async fn delete_session(app: tauri::AppHandle, session_id: String) -> Result<bool, String> {
    let registry = app.state::<Arc<registry::Registry>>();
    let deleted = registry.delete(&session_id).map_err(|e| e.to_string())?;
    if deleted {
        log::info!("registry: deleted session {session_id}");
        // A pinned card for a session that no longer exists would outlive it.
        if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
            overlay.clear_attention(&session_id);
            overlay.refresh();
        }
    }
    Ok(deleted)
}

/// Dismiss a pinned ask-moment card without acting on it.
#[tauri::command]
async fn dismiss_attention(app: tauri::AppHandle, session_id: String) {
    if let Some(overlay) = app.try_state::<Arc<overlay::Overlay>>() {
        overlay.clear_attention(&session_id);
    }
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

fn codex_config_path() -> io::Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".codex").join("config.toml"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))
}

/// Codex reads hook config from `$CODEX_HOME/hooks.json` (§5.2).
fn codex_hooks_path() -> io::Result<PathBuf> {
    let home = match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => dirs::home_dir()
            .map(|h| h.join(".codex"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?,
    };
    Ok(home.join("hooks.json"))
}

/// What the user still has to do by hand: Codex starts every new or changed
/// hook UNTRUSTED, so the entries we just wrote do nothing until they are
/// approved in Codex's own hook-review UI. Trust is per hook, and we install
/// two (questions + approvals), so both need the click.
const CODEX_HOOK_TRUST_NOTE: &str =
    "Codex will ignore these hooks until you approve them: ChatGPT app → Settings → Hooks.";

fn install_codex_hooks() -> io::Result<pingmybell_installers::InstallReport> {
    pingmybell_installers::codex::install_hooks(&shim_path()?, &codex_hooks_path()?)
}

fn uninstall_codex_hooks() -> io::Result<()> {
    pingmybell_installers::codex::uninstall_hooks(&codex_hooks_path()?)
}

/// Headless install: `pingmybell install-codex-hooks`.
pub fn cli_install_codex_hooks() -> i32 {
    match install_codex_hooks() {
        Ok(report) => {
            println!(
                "Installed Codex hooks into {}",
                report.settings_path.display()
            );
            if let Some(backup) = report.backup_path {
                println!("Previous hooks backed up to {}", backup.display());
            }
            println!("Hooks: {}", report.events.join(", "));
            println!();
            println!("ACTION REQUIRED: {CODEX_HOOK_TRUST_NOTE}");
            println!(
                "Approvals additionally follow \"Approve Codex Commands From Overlay\" \
                 (gate_codex_approvals in ~/.pingmybell/config.json): \"auto\" (default) \
                 intercepts only while Codex itself is set to \"Ask for approval\", \
                 \"always\"/\"never\" override that. Questions work regardless."
            );
            0
        }
        Err(err) => {
            eprintln!("install failed: {err}");
            1
        }
    }
}

/// Headless uninstall: `pingmybell uninstall-codex-hooks`.
pub fn cli_uninstall_codex_hooks() -> i32 {
    match uninstall_codex_hooks() {
        Ok(()) => {
            println!("Removed the PingMyBell hooks from Codex hooks.json");
            0
        }
        Err(err) => {
            eprintln!("uninstall failed: {err}");
            1
        }
    }
}

fn install_codex() -> io::Result<pingmybell_installers::InstallReport> {
    pingmybell_installers::codex::install(&shim_path()?, &codex_config_path()?)
}

fn uninstall_codex() -> io::Result<()> {
    pingmybell_installers::codex::uninstall(&codex_config_path()?)
}

/// Headless install for scripting/tests: `pingmybell install-codex`.
pub fn cli_install_codex() -> i32 {
    match install_codex() {
        Ok(report) => {
            println!(
                "Installed Codex notify into {}",
                report.settings_path.display()
            );
            if let Some(backup) = report.backup_path {
                println!("Previous config backed up to {}", backup.display());
            }
            0
        }
        Err(err) => {
            eprintln!("install failed: {err}");
            1
        }
    }
}

/// Headless uninstall: `pingmybell uninstall-codex`.
pub fn cli_uninstall_codex() -> i32 {
    match uninstall_codex() {
        Ok(()) => {
            println!("Removed PingMyBell notify from Codex config");
            0
        }
        Err(err) => {
            eprintln!("uninstall failed: {err}");
            1
        }
    }
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
