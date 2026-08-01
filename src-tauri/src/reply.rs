//! Free-text reply window: the ONE window in PingMyBell that is deliberately
//! focusable.
//!
//! The overlay island may never take keyboard focus (AC-5.1, release-blocking),
//! so it can only ever host *click* answers. When an agent question needs typed
//! text ("Other" / "Type something"), the user clicks through to this separate
//! window, which is created on an explicit user gesture and is allowed to take
//! the keyboard — the invariant protects the always-resident island, not a
//! window the user just asked for.
//!
//! Lifecycle: `open()` on a click → user types → `submit`/`cancel` command →
//! `close()`. The window is declared hidden in tauri.conf.json and its webview
//! loads lazily, so the prompt is ALSO parked in `pending` for the frontend to
//! pull on mount (same late-load race the overlay solves via `on_page_load`).

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewWindow};

pub const WINDOW_LABEL: &str = "reply";

const WIDTH: f64 = 520.0;
const HEIGHT: f64 = 208.0;

/// What the reply window renders. `id` is the broker's question id — the
/// answer is routed back by it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyPrompt {
    pub id: String,
    /// Short label for the question (AskUserQuestion's `header`).
    pub header: String,
    /// The question text itself.
    pub question: String,
    /// Which question of the call this answers — the typed text is routed
    /// back to the card, which owns submitting the whole set.
    pub question_index: usize,
    pub agent: String,
    /// Session title, for context ("which project is asking").
    pub title: String,
}

/// Owns the reply window's visibility and the pending prompt.
pub struct ReplyController {
    app: AppHandle,
    pending: Mutex<Option<ReplyPrompt>>,
}

impl ReplyController {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            pending: Mutex::new(None),
        }
    }

    /// Show the reply window for `prompt` and give it the keyboard.
    ///
    /// Only ever called from an explicit user click on the overlay card.
    pub fn open(&self, prompt: ReplyPrompt) {
        *self.pending.lock().expect("reply mutex poisoned") = Some(prompt.clone());

        let window = match self.window() {
            Some(w) => w,
            None => {
                log::warn!("reply: window {WINDOW_LABEL} not found; typed answers unavailable");
                return;
            }
        };

        // Emit for an already-loaded webview; a cold one pulls `pending_reply`
        // on mount instead. Both paths are idempotent.
        if let Err(err) = self.app.emit_to(WINDOW_LABEL, "reply-prompt", &prompt) {
            log::warn!("reply: emit failed: {err}");
        }

        // Placement reads NSScreen and the window level is an AppKit call, so
        // all of it must happen on the main thread — `open` is reached from an
        // async command running on a tokio worker.
        let main_thread = window.clone();
        if let Err(err) = window.run_on_main_thread(move || {
            place(&main_thread);
            // Float above the island (level 26): the question card overlaps
            // this window, and a focused box the user cannot see or click is
            // worse than no typed answers at all.
            #[cfg(target_os = "macos")]
            if let Ok(ptr) = main_thread.ns_window() {
                unsafe { crate::platform::macos::apply_reply_styles(ptr) };
            }
            if let Err(err) = main_thread.show() {
                log::warn!("reply: show failed: {err}");
                return;
            }
            // Unlike every other window in this app, this one WANTS the keyboard.
            if let Err(err) = main_thread.set_focus() {
                log::warn!("reply: focus failed: {err}");
            }
        }) {
            log::warn!("reply: could not reach the main thread: {err}");
        }
    }

    /// Hide the window and drop any pending prompt.
    pub fn close(&self) {
        *self.pending.lock().expect("reply mutex poisoned") = None;
        if let Some(window) = self.window() {
            if let Err(err) = window.hide() {
                log::warn!("reply: hide failed: {err}");
            }
        }
    }

    /// Prompt for a webview that just finished loading.
    pub fn pending(&self) -> Option<ReplyPrompt> {
        self.pending.lock().expect("reply mutex poisoned").clone()
    }

    /// Drop the pending prompt only if it is still the one being answered.
    /// Guards the race where a second question opens the window while the
    /// first is being submitted.
    pub fn clear_if_current(&self, id: &str) -> bool {
        let mut pending = self.pending.lock().expect("reply mutex poisoned");
        match pending.as_ref() {
            Some(p) if p.id == id => {
                *pending = None;
                true
            }
            _ => false,
        }
    }

    /// True when `(id, question_index)` is exactly what the window is showing.
    /// One question id covers every question of a multi-question call, so the
    /// index has to match too — otherwise a stale webview still showing
    /// question 0 gets accepted as an answer to question 0 while the user has
    /// already moved on to question 1.
    pub fn is_current(&self, id: &str, question_index: usize) -> bool {
        let pending = self.pending.lock().expect("reply mutex poisoned");
        matches!(pending.as_ref(), Some(p) if p.id == id && p.question_index == question_index)
    }

    fn window(&self) -> Option<WebviewWindow> {
        self.app.get_webview_window(WINDOW_LABEL)
    }
}

/// Close the reply window if — and only if — it is still showing
/// `question_id`. Called from every path where a question stops being
/// answerable (answered, deferred, expired, shim died), so a focused window
/// can never outlive the question it belongs to.
pub fn close_for(app: &AppHandle, question_id: &str) {
    if let Some(controller) = app.try_state::<std::sync::Arc<ReplyController>>() {
        if controller.clear_if_current(question_id) {
            controller.close();
        }
    }
}

/// Centre horizontally, just under the notch/menu bar — close to the card the
/// user clicked, so the typed answer feels like the same object.
///
/// Must run on the main thread (reads NSScreen).
fn place(window: &WebviewWindow) {
    let probe = crate::platform::probe_primary_screen();
    if let Err(err) = window.set_size(LogicalSize::new(WIDTH, HEIGHT)) {
        log::warn!("reply: set_size failed: {err}");
        return;
    }
    let Ok(Some(monitor)) = window.primary_monitor() else {
        return;
    };
    let scale = monitor.scale_factor();
    let screen_width = monitor.size().to_logical::<f64>(scale).width;
    let x = (screen_width - WIDTH) / 2.0;
    let y = probe.top_inset + 12.0;
    if let Err(err) = window.set_position(LogicalPosition::new(x, y)) {
        log::warn!("reply: set_position failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(id: &str) -> ReplyPrompt {
        ReplyPrompt {
            id: id.into(),
            header: "License".into(),
            question: "Which license?".into(),
            question_index: 0,
            agent: "Claude".into(),
            title: "Galena Data".into(),
        }
    }

    /// The controller's prompt bookkeeping is pure state — testable without a
    /// running Tauri app, which is where the interesting races live.
    #[test]
    fn clear_if_current_only_clears_the_matching_question() {
        let pending = Mutex::new(Some(prompt("q1")));

        // Simulates ReplyController::clear_if_current without an AppHandle.
        let clear = |id: &str| -> bool {
            let mut guard = pending.lock().unwrap();
            match guard.as_ref() {
                Some(p) if p.id == id => {
                    *guard = None;
                    true
                }
                _ => false,
            }
        };

        // A stale submit for a question that was already replaced must not
        // clear the newer prompt.
        assert!(!clear("q0"));
        assert!(pending.lock().unwrap().is_some());

        assert!(clear("q1"));
        assert!(pending.lock().unwrap().is_none());

        // Double-submit is a no-op rather than a panic.
        assert!(!clear("q1"));
    }

    #[test]
    fn prompt_round_trips_through_json() {
        let json = serde_json::to_string(&prompt("q7")).unwrap();
        let back: ReplyPrompt = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prompt("q7"));
    }
}
