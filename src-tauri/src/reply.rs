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
        self.hide();
    }

    fn hide(&self) {
        if let Some(window) = self.window() {
            if let Err(err) = window.hide() {
                log::warn!("reply: hide failed: {err}");
            }
        }
    }

    /// The user asked to put the window away — Cancel, Esc, or Close on an
    /// expired draft. Returns false, and does nothing, when the window has
    /// since been taken over by a DIFFERENT question: a stale click must not
    /// hide a prompt somebody is currently answering.
    ///
    /// Note the third case: an expired draft has no pending prompt at all, and
    /// must still be closable — which is why this is not just `close_for`.
    pub fn dismiss(&self, id: &str) -> bool {
        {
            let mut pending = self.pending.lock().expect("reply mutex poisoned");
            match pending.as_ref() {
                Some(p) if p.id != id => return false,
                Some(_) => *pending = None,
                None => {}
            }
        }
        self.hide();
        true
    }

    /// Move an expired window OUT of the island's way without closing it.
    ///
    /// It floats above the overlay by design (§5.1.1), so leaving a dead draft
    /// parked under the notch would occlude the next approval card — an
    /// unclickable approval is worse than the bug this window is saving us
    /// from. Dropping it to the bottom of the screen keeps every character the
    /// user typed while giving the island its space back.
    pub fn park_expired(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let main_thread = window.clone();
        if let Err(err) = window.run_on_main_thread(move || place_out_of_the_way(&main_thread)) {
            log::warn!("reply: could not reach the main thread to move the window: {err}");
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
/// `question_id`. Called from the paths where the user is DONE with the
/// question (they answered it, or they chose to answer in the terminal), so a
/// focused window can never outlive the question it belongs to.
pub fn close_for(app: &AppHandle, question_id: &str) {
    if let Some(controller) = app.try_state::<std::sync::Arc<ReplyController>>() {
        // Only when THIS question is the one on screen: another question's
        // cleanup must not restore a card behind a reply window that is still
        // open for a different question.
        if controller.clear_if_current(question_id) {
            controller.close();
            if let Some(overlay) = app.try_state::<std::sync::Arc<crate::overlay::Overlay>>() {
                overlay.set_reply_open(false);
            }
        }
    }
}

/// The question died while its reply window was open — it ran out of park
/// time, or the agent/shim went away mid-answer.
///
/// Deliberately NOT `close_for`: the user may be mid-sentence, and a window
/// that vanishes with their paragraph in it is the exact failure this path
/// was written for. The prompt is dropped (so a late `submit_reply` is
/// correctly refused) but the window and its text stay on screen, and the
/// webview is told to say so. The user closes it when they have their words
/// back.
pub fn expire_for(app: &AppHandle, question_id: &str) {
    let Some(controller) = app.try_state::<std::sync::Arc<ReplyController>>() else {
        return;
    };
    // False when this question never opened the window, or a newer question
    // already took it over — nothing of this question's is on screen.
    if !controller.clear_if_current(question_id) {
        return;
    }
    if let Err(err) = app.emit_to(
        WINDOW_LABEL,
        "reply-expired",
        serde_json::json!({ "id": question_id }),
    ) {
        // Nothing to tell: the webview is gone, so no draft is at risk. Close
        // rather than leave an unexplained empty window floating.
        log::warn!("reply: expiry notice failed ({err}); closing the window");
        controller.close();
    } else {
        controller.park_expired();
    }
    // The card is being unpinned anyway; clear the flag so the NEXT question's
    // card is not hidden behind a window this one left open.
    if let Some(overlay) = app.try_state::<std::sync::Arc<crate::overlay::Overlay>>() {
        overlay.set_reply_open(false);
    }
}

/// Sit just below the idle sliver so the island never peeks out above the
/// reply window.
pub const TOP_GAP: f64 = 16.0;

/// Clearance above the Dock when an expired draft is moved out of the
/// island's way.
const BOTTOM_GAP: f64 = 96.0;

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
    let y = probe.top_inset + TOP_GAP;
    if let Err(err) = window.set_position(LogicalPosition::new(x, y)) {
        log::warn!("reply: set_position failed: {err}");
    }
}

/// Bottom-centre: nowhere near the notch island, which only ever grows
/// DOWNWARD from the top of the screen. Used for an expired draft the user
/// still has to rescue.
///
/// Must run on the main thread (reads NSScreen).
fn place_out_of_the_way(window: &WebviewWindow) {
    let Ok(Some(monitor)) = window.primary_monitor() else {
        return;
    };
    let scale = monitor.scale_factor();
    let screen = monitor.size().to_logical::<f64>(scale);
    let x = (screen.width - WIDTH) / 2.0;
    let y = (screen.height - HEIGHT - BOTTOM_GAP).max(0.0);
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
