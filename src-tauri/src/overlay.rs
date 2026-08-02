//! Overlay controller (FR-5): owns the island state machine, window
//! geometry, and emission of `overlay-state` snapshots. The Svelte side only
//! renders what it is told (CLAUDE.md: UI stays dumb).
//!
//! Display precedence: approval (pinned, actionable) > attention (pinned
//! ask-moment) > toast (6 s) > hover-expanded session list > idle sliver.
//! All updates are event-driven — no polling loops (AC-5.5). The window can
//! never take keyboard focus; focus theft is release-blocking (AC-5.1).
//!
//! LOCK ORDER, and the reason for it. The registry mutex is held across
//! blocking disk work elsewhere (WAL checkpoint, the daily prune's VACUUM),
//! so it must never be taken while an overlay lock is held — a caller would
//! otherwise block on SQLite while holding the state every UI update needs.
//! Every path therefore snapshots the registry FIRST, with nothing held, and
//! then acquires, in this order and never the reverse:
//!
//!   `layout` (leaf, copied out immediately) → `model` → `win`
//!   `window_ops` → `win`
//!
//! `model` → `win` is not incidental: taking `win` while still holding
//! `model` is what makes the sync that computed its target LAST also the one
//! that commits it, so geometry cannot end up describing a state the model
//! never had.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewWindow};

use crate::broker::{ApprovalInfo, QuestionInfo};
use crate::platform::{self, ScreenProbe};
use crate::registry::{AgentKind, EventKind, NormalizedEvent, Registry, Session, SessionState};

const TOAST_SECS: u64 = 6;
const HOVER_COLLAPSE_MS: u64 = 300;
/// How many live sessions the expanded island will list. Generous on purpose:
/// the list scrolls, so nothing is silently dropped off the bottom (a real
/// session went missing when this doubled as the height clamp).
const EXPANDED_MAX_SESSIONS: usize = 40;
/// How many rows fit in the island before it scrolls. This — not the row
/// count — bounds the WINDOW height, so the island never grows toward the
/// bottom of a 13" screen.
const EXPANDED_VISIBLE_ROWS: usize = 8;
/// Row / header metrics shared by the window math and the webview's scroll
/// viewport (emitted as `list_max`); keep in sync with `.row`/`.header` in
/// Overlay.svelte.
const ROW_H: f64 = 34.0;
const HEADER_H: f64 = 26.0;
/// Rows must outnumber the visible window or the list never scrolls and the
/// overflow is silently lost. Asserted at compile time — it was a runtime
/// assertion inside a test, which clippy rightly pointed out can only ever
/// have one answer.
const _: () = assert!(EXPANDED_MAX_SESSIONS > EXPANDED_VISIBLE_ROWS);
/// AskUserQuestion allows up to 4 options; clamp anyway so a malformed or
/// future payload can never grow the card past the screen.
/// How many options the question card is SIZED for. Ingest clamps incoming
/// questions to this, so the two cannot drift: they did, and a 9-option
/// question rendered its Send button below the window edge, unanswerable.
pub const QUESTION_MAX_OPTIONS: usize = 6;

/// Window sizes per state, in logical points.
///
/// Re-derived whenever the display configuration changes (see
/// `on_screen_change`): every number here comes from the screen the island
/// sits on, and docking a notched laptop to an external monitor changes all
/// of them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    has_notch: bool,
    idle: (f64, f64),
    toast: (f64, f64),
    attention: (f64, f64),
    approval: (f64, f64),
    /// Top offset of the window (0 = flush with screen top for the notch).
    y: f64,
    /// Base height above the expanded rows (notch inset or pill padding).
    expanded_base: f64,
}

impl Layout {
    fn from_probe(probe: &ScreenProbe) -> Self {
        match probe.notch_width {
            // Flush with the notch: idle hugs its width with a thin lip below
            // for the dots; everything else extends below and wider (AC-5.2).
            Some(width) => Layout {
                has_notch: true,
                idle: (width, probe.top_inset + 16.0),
                toast: (width.max(480.0), probe.top_inset + 46.0),
                attention: (width.max(500.0), probe.top_inset + 64.0),
                approval: (width.max(540.0), probe.top_inset + 88.0),
                y: 0.0,
                expanded_base: probe.top_inset + 16.0,
            },
            // Floating pill under the menu bar (or at top on Windows).
            None => Layout {
                has_notch: false,
                idle: (150.0, 30.0),
                toast: (480.0, 58.0),
                attention: (500.0, 76.0),
                approval: (540.0, 100.0),
                y: probe.top_inset + 8.0,
                expanded_base: 20.0,
            },
        }
    }

    /// A question card sizes to the WIDEST question in the call (the most
    /// options), so advancing from question 1 to 2 never resizes the window
    /// under the user's cursor mid-answer.
    fn question(&self, options: usize) -> (f64, f64) {
        let options = options.clamp(1, QUESTION_MAX_OPTIONS) as f64;
        let base = if self.has_notch {
            self.expanded_base
        } else {
            10.0
        };
        // header + question text + option buttons + the type/defer footer.
        // Options are two lines (label above description): one-lining them
        // truncated real descriptions mid-word, which is what the user saw.
        (620.0, base + 58.0 + options * 58.0 + 34.0)
    }

    fn expanded(&self, rows: usize) -> (f64, f64) {
        let rows = rows.clamp(1, EXPANDED_VISIBLE_ROWS) as f64;
        // header row + visible session rows + padding; extra rows scroll
        // inside `list_max` rather than growing the window.
        (440.0, self.expanded_base + HEADER_H + rows * ROW_H + 10.0)
    }

    fn size_for(&self, display: Display, rows: usize, options: usize) -> (f64, f64) {
        match display {
            Display::Question => self.question(options),
            Display::Approval => self.approval,
            Display::Attention => self.attention,
            Display::Toast => self.toast,
            Display::Expanded => self.expanded(rows),
            Display::Idle => self.idle,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ToastView {
    /// Which session the toast is about — a toast is a pointer to work that
    /// just finished, so clicking it must be able to take you there.
    session_id: String,
    agent: &'static str,
    title: String,
    state: SessionState,
    summary: String,
}

#[derive(Debug, Clone, Serialize)]
struct AttentionView {
    session_id: String,
    agent: &'static str,
    title: String,
    summary: String,
    /// When this pin was made, on the monotonic clock. Read by
    /// `Model::prune_attentions` to tell a pin that a registry snapshot is
    /// authoritative about from one made after that snapshot was taken.
    #[serde(skip)]
    pinned_at: Instant,
}

#[derive(Debug, Clone, Serialize)]
struct SessionRow {
    id: String,
    agent: &'static str,
    title: String,
    state: SessionState,
    minutes: i64,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
struct Counts {
    working: u32,
    attention: u32,
    done: u32,
}

#[derive(Serialize)]
struct ApprovalCard<'a> {
    #[serde(flatten)]
    info: &'a ApprovalInfo,
    /// How many more approvals are queued behind this one.
    queued: usize,
}

#[derive(Serialize)]
struct QuestionCard<'a> {
    #[serde(flatten)]
    info: &'a QuestionInfo,
    /// How many more questions are queued behind this one.
    queued: usize,
}

#[derive(Serialize)]
struct AttentionCard<'a> {
    #[serde(flatten)]
    info: &'a AttentionView,
    /// How many OTHER sessions are also waiting on the user. Only the oldest
    /// card is drawn, so without this a second session needing attention is
    /// invisible rather than merely behind — the same count approvals and
    /// questions already carry.
    queued: usize,
}

#[derive(Serialize)]
struct OverlayView<'a> {
    mode: &'static str,
    has_notch: bool,
    /// Target island size in logical px — the shell div animates to this
    /// (the window itself snaps invisibly around it).
    shell: (f64, f64),
    /// Max height of the expanded session list's scroll viewport, in logical
    /// px. Rust owns sizing; the webview just clamps its scroller to this.
    list_max: f64,
    counts: Counts,
    toast: Option<&'a ToastView>,
    attention: Option<AttentionCard<'a>>,
    question: Option<QuestionCard<'a>>,
    approval: Option<ApprovalCard<'a>>,
    sessions: Option<Vec<SessionRow>>,
}

enum Mode {
    Idle,
    Toast(ToastView),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Display {
    Approval,
    Question,
    Attention,
    Toast,
    Expanded,
    Idle,
}

struct Model {
    mode: Mode,
    /// Monotonic toast id: a collapse timer only fires if no newer toast
    /// replaced the one it was armed for.
    seq: u64,
    /// Pinned approvals, oldest first (AC-5.4).
    approvals: Vec<ApprovalInfo>,
    /// Parked AskUserQuestion calls, oldest first. Like approvals these are
    /// actionable and block the agent, so they outrank every passive state.
    questions: Vec<QuestionInfo>,
    /// True while the focusable reply window is open for a question. The
    /// card is SUPPRESSED for the duration: the reply window repeats the
    /// question, and a wider card peeking out around it looks broken.
    reply_open: bool,
    /// Pinned ask-moments (permission/idle/question prompts), one per
    /// session; persist until the session moves on or the user dismisses.
    attentions: Vec<AttentionView>,
    hovered: bool,
    hover_seq: u64,
}

impl Model {
    fn display(&self) -> Display {
        if !self.approvals.is_empty() {
            Display::Approval
        } else if self.reply_open {
            // Typing an answer: everything passive collapses to the sliver.
            // The reply window is narrower than the card and shorter than the
            // expanded list, so anything still drawn peeks out around it.
            Display::Idle
        } else if !self.questions.is_empty() {
            Display::Question
        } else if !self.attentions.is_empty() {
            Display::Attention
        } else if matches!(self.mode, Mode::Toast(_)) {
            Display::Toast
        } else if self.hovered {
            Display::Expanded
        } else {
            Display::Idle
        }
    }

    /// A pin must not be able to outlive the need.
    ///
    /// `attentions` used to be drained only by a LATER event for the same
    /// session, so an agent killed between raising a notification and
    /// answering it left its amber card pinned forever — and since Attention
    /// outranks Expanded, one stale card disabled hover-to-expand entirely
    /// until somebody noticed the ✕. The registry is the source of truth for
    /// session state, so a pin whose session is no longer `NeedsAttention` —
    /// or has left the live map altogether — is dropped here.
    ///
    /// `taken_at` is the moment `sessions` was sampled, and a pin made after
    /// that is deliberately left alone. Ingest applies the event to the
    /// registry BEFORE calling `Overlay::on_event`, so the state that
    /// justifies a pin is always recorded before the pin exists; but only a
    /// snapshot taken after the pin is guaranteed to have seen it. Without
    /// this check an `emit` that sampled the registry microseconds earlier
    /// could drop a pin in the instant before its justification became
    /// visible — the pin would flicker away and never come back.
    fn prune_attentions(&mut self, sessions: &[Session], taken_at: Instant) -> bool {
        let before = self.attentions.len();
        self.attentions.retain(|pinned| {
            pinned.pinned_at >= taken_at
                || sessions.iter().any(|session| {
                    session.id == pinned.session_id && session.state == SessionState::NeedsAttention
                })
        });
        before != self.attentions.len()
    }

    /// Does the hover watchdog armed for `seq` still own the hover state?
    ///
    /// Deliberately NOT "is the display still Expanded". A higher-priority
    /// card can appear while the island is expanded and snap the window to a
    /// different size under the pointer, which swallows the webview's
    /// `mouseleave` (see `set_hover`) — precisely the case the watchdog
    /// exists for. Standing down there left `hovered` latched true with
    /// nothing able to clear it, and the session list unfolded minutes later
    /// with the pointer nowhere near the island.
    fn hover_watch_active(&self, seq: u64) -> bool {
        self.hover_seq == seq && self.hovered
    }
}

pub struct Overlay {
    app: AppHandle,
    /// Native window handle, resolved ONCE at init on the main thread.
    ///
    /// The hover fast path runs on the main thread for EVERY cursor movement
    /// system-wide. Calling back into tauri's window accessors there
    /// (`ns_window()`/`hwnd()`) re-enters the runtime from inside AppKit
    /// event dispatch, and an app wedged that way stalls window switching for
    /// the whole machine — observed, not theoretical. The hot path must be
    /// pure geometry against a handle we already hold.
    native_window: std::sync::atomic::AtomicUsize,
    registry: Arc<Registry>,
    /// Screen-derived geometry. Behind a lock because it is re-probed on
    /// display reconfiguration; treat it as a leaf — copy it out, never hold
    /// it across another lock.
    layout: Mutex<Layout>,
    model: Mutex<Model>,
    /// Serializes window resize/reposition; each sync re-reads the current
    /// display inside this lock, so geometry converges to the model.
    window_ops: Mutex<()>,
    /// What the window should be, what it demonstrably IS, and a seq for
    /// delayed shrinks: growth snaps the (transparent) window immediately so
    /// the shell can morph inside it; shrink lets the shell animation finish
    /// before the window snaps down.
    win: Mutex<WinState>,
}

struct WinState {
    /// Geometry that was ACTUALLY applied to the window — `None` until an
    /// apply has succeeded.
    ///
    /// This used to record the last size *requested*, which made a failed
    /// apply sticky: `apply_window` fails whenever `primary_monitor()` yields
    /// nothing (clamshell close, resolution change), and every later sync to
    /// that same logical size then returned early because the request had
    /// already been recorded. The window kept its old physical size forever —
    /// not cosmetically, either: the stage is `100vw/100vh` with cursor
    /// events enabled, so an oversized transparent window eats every click in
    /// its rect while rendering only a small island.
    applied: Option<(f64, f64)>,
    /// Geometry the model wants. Written under the model lock so the sync
    /// that computed its target last is also the one that commits it.
    desired: (f64, f64),
    seq: u64,
    /// Consecutive failed applies; reset by success and by a new sync.
    retries: u32,
    /// Bumped whenever everything already applied stops meaning anything —
    /// today only a display reconfiguration, where the same logical size can
    /// need a different physical placement. An apply that started before the
    /// bump must not record itself as current, or the early-return in
    /// `sync_window` would skip the re-centre it was invalidated for.
    generation: u64,
}

/// How long the shell morph animation runs (see Overlay.svelte transition);
/// window shrinks are deferred past it.
const MORPH_MS: u64 = 280;

/// How many times a failed apply retries itself before waiting for the next
/// state change. Bounded so a display that is genuinely gone (lid shut) does
/// not leave a task rescheduling forever; any later event, hover, or screen
/// reconfiguration starts a fresh budget.
const MAX_APPLY_RETRIES: u32 = 5;
const APPLY_RETRY_MS: u64 = 400;

pub fn init(app: &AppHandle, registry: Arc<Registry>) -> tauri::Result<Arc<Overlay>> {
    let window = overlay_window(app)?;

    #[cfg(target_os = "macos")]
    unsafe {
        // NSWindow level/collection behaviour survive `show()`, so these can
        // go on before the window is on screen (and should: it must never
        // appear at the default level, even for a frame).
        platform::macos::apply_overlay_styles(window.ns_window()?);
    }

    let probe = platform::probe_primary_screen();
    let layout = Layout::from_probe(&probe);
    log::info!(
        "overlay: notch={:?} top_inset={} idle={:?}",
        probe.notch_width,
        probe.top_inset,
        layout.idle
    );

    // Resolve the native handle once, here on the main thread during setup,
    // so the per-mouse-move hover check never calls back into tauri.
    #[cfg(target_os = "macos")]
    let native_window = window.ns_window().map(|p| p as usize).unwrap_or(0);
    #[cfg(windows)]
    let native_window = window.hwnd().map(|h| h.0 as usize).unwrap_or(0);
    #[cfg(not(any(target_os = "macos", windows)))]
    let native_window = 0usize;

    let overlay = Arc::new(Overlay {
        app: app.clone(),
        native_window: std::sync::atomic::AtomicUsize::new(native_window),
        registry,
        layout: Mutex::new(layout),
        model: Mutex::new(Model {
            mode: Mode::Idle,
            seq: 0,
            approvals: Vec::new(),
            questions: Vec::new(),
            reply_open: false,
            attentions: Vec::new(),
            hovered: false,
            hover_seq: 0,
        }),
        window_ops: Mutex::new(()),
        win: Mutex::new(WinState {
            // Nothing has been applied yet — not "we asked for idle".
            applied: None,
            desired: layout.idle,
            seq: 0,
            retries: 0,
            generation: 0,
        }),
    });

    // Deliberately not fatal, and deliberately not `?`. A missing primary
    // monitor at startup (a Mac booting with the lid shut into a display that
    // has not woken yet) is a transient condition; retrying is strictly
    // better than disabling the overlay for the session, which is what
    // propagating the error did.
    //
    // This is the one place the main thread may take `window_ops`: the Arc has
    // not been shared with anything yet, so there is nobody to wait for.
    overlay.apply_pending();
    // The island is interactive (hover to expand, buttons on cards) but can
    // never take keyboard focus (focusable: false, AC-5.1).
    window.set_ignore_cursor_events(false)?;
    window.show()?;

    // AFTER `show()`, and that ordering is the whole point on Windows.
    // `show()` → tao `set_visible(true)` → `apply_diff`, which recomputes the
    // ex-style from scratch with `to_window_styles()` and SetWindowLongW's it
    // over whatever is there. That recomputation emits WS_EX_NOACTIVATE (so
    // the focus invariant survives either way) but never WS_EX_TOOLWINDOW, so
    // applying our styles first meant they were wiped a moment later and the
    // overlay showed up in Alt-Tab (AC-5.3). `skipTaskbar` does not cover it:
    // tao implements that with ITaskbarList::DeleteTab, which removes the
    // taskbar button only — Alt-Tab suppression is WS_EX_TOOLWINDOW and
    // nothing else. Applying afterwards only ever ORs bits IN, so
    // WS_EX_NOACTIVATE stays set and the window still cannot be activated.
    #[cfg(windows)]
    unsafe {
        platform::windows::apply_overlay_styles(window.hwnd()?.0);
    }

    overlay.emit();

    // A never-key window receives no hover events from AppKit, so hover
    // ENTRY is detected from OS-level mouse-moved events (exit is already
    // covered by the cursor watchdog). Webview mouseenter stays as the
    // cross-platform fast path.
    #[cfg(target_os = "macos")]
    {
        let monitor_overlay = Arc::clone(&overlay);
        platform::macos::install_mouse_moved_monitor(move || {
            monitor_overlay.on_global_mouse_move();
        });

        // Every number in `Layout` comes from the screen the island sits on,
        // and it used to be frozen at boot: dock a notched MacBook to an
        // external 27" and the island re-centred onto the external display
        // but kept y=0 and notch-hugging sizes, drawing itself over a real
        // menu bar. AppKit posts a notification for exactly this, so no
        // polling is needed (AC-5.5).
        let screen_overlay = Arc::clone(&overlay);
        platform::macos::install_screen_change_observer(move || {
            screen_overlay.on_screen_change();
        });
    }

    Ok(overlay)
}

impl Overlay {
    /// Feed a registry event through the overlay state machine.
    pub fn on_event(self: &Arc<Self>, event: &NormalizedEvent, session: &Session) {
        match event.event {
            EventKind::TurnComplete => {
                // The session moved on: any pinned ask-moment is stale.
                self.clear_attention(&session.id);
                self.show_toast(event.agent, session, event.summary.as_deref().unwrap_or(""));
            }
            EventKind::NeedsAttention | EventKind::PermissionRequest => {
                self.pin_attention(AttentionView {
                    session_id: session.id.clone(),
                    agent: agent_label(event.agent),
                    title: session.title.clone(),
                    summary: event.summary.clone().unwrap_or_default(),
                    // Re-stamped under the model lock by `pin_attention`.
                    pinned_at: Instant::now(),
                });
            }
            // A turn starting means the user has moved on — typically by
            // answering the very thing that was pinned — so the same
            // handling as a session boundary is what is wanted: drop the
            // stale card, then re-render at the new state.
            EventKind::SessionStart | EventKind::TurnStart | EventKind::SessionEnd => {
                self.clear_attention(&session.id);
                self.sync_window();
                self.emit();
            }
        }
    }

    fn show_toast(self: &Arc<Self>, agent: AgentKind, session: &Session, summary: &str) {
        let seq = {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.seq += 1;
            model.mode = Mode::Toast(ToastView {
                session_id: session.id.clone(),
                agent: agent_label(agent),
                title: session.title.clone(),
                state: session.state,
                summary: summary.to_string(),
            });
            model.seq
        };
        log::info!("overlay: toast for session {} (seq {seq})", session.id);
        self.sync_window();
        self.emit();

        let overlay = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(TOAST_SECS)).await;
            overlay.collapse_if(seq);
        });
    }

    fn collapse_if(self: &Arc<Self>, seq: u64) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            if model.seq != seq || !matches!(model.mode, Mode::Toast(_)) {
                return;
            }
            model.mode = Mode::Idle;
        }
        log::info!("overlay: collapsed to idle (seq {seq})");
        self.sync_window();
        self.emit();
    }

    /// Pin an ask-moment card: Claude is genuinely waiting on the user.
    ///
    /// The pin is not immortal — `Model::prune_attentions` drops it again as
    /// soon as the registry stops saying this session needs attention.
    fn pin_attention(self: &Arc<Self>, mut attention: AttentionView) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            // Stamped here, under the lock, so it is ordered against the
            // registry snapshots that prune it.
            attention.pinned_at = Instant::now();
            model
                .attentions
                .retain(|a| a.session_id != attention.session_id);
            model.attentions.push(attention);
        }
        self.sync_window();
        self.emit();
    }

    /// Drop pinned attention for a session (it resumed, ended, or the user
    /// dismissed the card).
    pub fn clear_attention(self: &Arc<Self>, session_id: &str) {
        let removed = {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            let before = model.attentions.len();
            model.attentions.retain(|a| a.session_id != session_id);
            before != model.attentions.len()
        };
        if removed {
            self.sync_window();
            self.emit();
        }
    }

    /// Pin an approval card (AC-5.4/AC-6.1): overrides everything until
    /// resolved or expired.
    pub fn pin_approval(self: &Arc<Self>, info: ApprovalInfo) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.approvals.push(info);
        }
        self.sync_window();
        self.emit();
    }

    /// Pin a parked AskUserQuestion: the agent is blocked until it is
    /// answered here or the park times out into its own selector.
    pub fn pin_question(self: &Arc<Self>, info: QuestionInfo) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.questions.push(info);
        }
        self.sync_window();
        self.emit();
    }

    /// Hide/restore the question card while the typed-answer window is up.
    pub fn set_reply_open(self: &Arc<Self>, open: bool) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            if model.reply_open == open {
                return;
            }
            model.reply_open = open;
        }
        self.sync_window();
        self.emit();
    }

    /// Remove a question (answered, deferred, or expired).
    pub fn unpin_question(self: &Arc<Self>, id: &str) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.questions.retain(|q| q.id != id);
        }
        self.sync_window();
        self.emit();
    }

    /// Remove an approval (decided or expired).
    pub fn unpin_approval(self: &Arc<Self>, id: &str) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.approvals.retain(|a| a.id != id);
        }
        self.sync_window();
        self.emit();
    }

    /// Hover from the webview: expand the idle island into the session list;
    /// collapse shortly after the pointer leaves.
    ///
    /// The webview's mouseleave is only the FAST path — it can be swallowed
    /// while the window resizes under the pointer, so expansion also arms a
    /// cursor-position watchdog that guarantees the collapse (the "island
    /// stuck open" bug). The watchdog only runs while expanded, so idle CPU
    /// stays at zero (AC-5.5).
    pub fn set_hover(self: &Arc<Self>, hovering: bool) {
        let (collapse_seq, watchdog_seq) = {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.hover_seq += 1;
            model.hovered = hovering;
            if hovering {
                (None, Some(model.hover_seq))
            } else {
                (Some(model.hover_seq), None)
            }
        };
        match collapse_seq {
            None => {
                self.sync_window();
                self.emit();
                if let Some(seq) = watchdog_seq {
                    self.spawn_hover_watchdog(seq);
                }
            }
            Some(seq) => {
                let overlay = Arc::clone(self);
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(HOVER_COLLAPSE_MS)).await;
                    let still_out = {
                        let model = overlay.model.lock().expect("overlay mutex poisoned");
                        model.hover_seq == seq && !model.hovered
                    };
                    if still_out {
                        overlay.sync_window();
                        overlay.emit();
                    }
                });
            }
        }
    }

    /// OS-level mouse movement (runs on the MAIN thread): expand when the
    /// pointer genuinely enters the idle island. Only cheap checks happen
    /// here — a brief model-lock peek and a native point-in-rect — and the
    /// actual expansion is dispatched to the async runtime. The main thread
    /// must never wait on `window_ops`: a background task can hold it while
    /// blocked on a main-thread window getter, which would deadlock the app.
    pub fn on_global_mouse_move(self: &Arc<Self>) {
        // This runs ON THE MAIN THREAD for every cursor movement anywhere in
        // the system, so it must be cheap and must never wait on anything.
        //
        // Two rules, both learned the hard way from a report of the beachball
        // on tray hover:
        //   1. Do the lock-free AppKit geometry check FIRST. It is two
        //      msg_sends and rejects ~every movement, since the island is a
        //      sliver at the top of one screen.
        //   2. NEVER block on the model mutex here. `try_lock` and bail if it
        //      is contended — another movement event is milliseconds away, so
        //      a skipped sample costs nothing, while a blocked main thread
        //      freezes the whole UI including the tray menu.
        if !self.cursor_inside().unwrap_or(false) {
            return;
        }
        let Ok(model) = self.model.try_lock() else {
            return;
        };
        let idle_and_unhovered = !model.hovered && model.display() == Display::Idle;
        drop(model);
        if !idle_and_unhovered {
            return;
        }
        let overlay = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            overlay.set_hover(true);
        });
    }

    /// Poll the real cursor position while the island believes it is hovered;
    /// force the collapse when the pointer is genuinely gone, regardless of
    /// webview event delivery.
    ///
    /// It watches `hovered`, NOT `display() == Expanded` (see
    /// `Model::hover_watch_active`). Standing down the moment a
    /// higher-priority card appeared is what let `hovered` latch: nothing
    /// re-arms it — `set_hover(true)` is the only arming path, and
    /// `on_global_mouse_move` refuses to act unless the island is already
    /// idle and unhovered — so the flag stayed true until the app restarted.
    /// The cost of watching through a card is bounded the same way as before:
    /// the loop ends the moment the pointer is outside the window, so it
    /// still cannot poll while the user is elsewhere (AC-5.5).
    fn spawn_hover_watchdog(self: &Arc<Self>, seq: u64) {
        let overlay = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                {
                    let model = overlay.model.lock().expect("overlay mutex poisoned");
                    if !model.hover_watch_active(seq) {
                        return; // superseded, or already collapsed by someone else
                    }
                }
                // Unknown cursor position → treat as outside (collapse is
                // the safe direction; hovering again re-expands).
                if overlay.cursor_inside().unwrap_or(false) {
                    continue;
                }
                let collapsed = {
                    let mut model = overlay.model.lock().expect("overlay mutex poisoned");
                    if model.hover_seq == seq {
                        model.hovered = false;
                        model.hover_seq += 1;
                        true
                    } else {
                        false
                    }
                };
                if collapsed {
                    log::debug!("overlay: hover watchdog forced collapse");
                    overlay.sync_window();
                    overlay.emit();
                }
                return;
            }
        });
    }

    /// Is the global cursor within the overlay window frame? Uses native
    /// same-space APIs per platform — mixing tauri's cursor_position with
    /// outer_position compares different coordinate spaces on retina macOS
    /// and always reads "outside".
    fn cursor_inside(&self) -> Option<bool> {
        // Cached handle only — never ask tauri for the window here (see
        // `native_window`).
        let handle = self
            .native_window
            .load(std::sync::atomic::Ordering::Relaxed);
        if handle == 0 {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            Some(unsafe {
                platform::macos::cursor_in_window(handle as *mut std::ffi::c_void, 8.0)
            })
        }
        #[cfg(windows)]
        {
            Some(unsafe { platform::windows::cursor_in_window(handle as _, 8) })
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            None
        }
    }

    /// Re-emit the current state; used when the overlay webview (re)loads
    /// after the initial setup emit was sent into the void.
    pub fn refresh(self: &Arc<Self>) {
        self.emit();
    }

    /// The display configuration changed: re-derive the layout from the screen
    /// the island now lives on and re-place the window.
    ///
    /// Runs ON THE MAIN THREAD (NSScreen is a main-thread API), so it does the
    /// cheap probe here and hands every window operation to the async runtime.
    /// The main thread must never wait on `window_ops` — a background task can
    /// hold it while blocked on a main-thread window getter, which deadlocks
    /// the app (same rule as `on_global_mouse_move`).
    #[cfg(target_os = "macos")]
    pub fn on_screen_change(self: &Arc<Self>) {
        let probe = platform::probe_primary_screen();
        let next = Layout::from_probe(&probe);
        let changed = {
            let mut layout = self.layout.lock().expect("overlay layout lock poisoned");
            let changed = *layout != next;
            *layout = next;
            changed
        };
        if changed {
            log::info!(
                "overlay: display changed; notch={:?} top_inset={} idle={:?}",
                probe.notch_width,
                probe.top_inset,
                next.idle
            );
        }
        {
            // Force a re-apply even when the size is unchanged: a pure
            // resolution change moves the centre and nothing else, and only
            // SIZE changes used to reach `apply_window` — which left the
            // island off-centre until some unrelated state change.
            //
            // The generation bump is what makes that stick. Clearing
            // `applied` alone was not enough: an apply already in flight
            // writes its own target back a moment later, and the next sync
            // then early-returns on geometry computed for the OLD screen.
            let mut win = self.win.lock().expect("overlay win lock poisoned");
            win.applied = None;
            win.retries = 0;
            win.generation = win.generation.wrapping_add(1);
        }
        let overlay = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            overlay.sync_window();
            overlay.emit();
        });
    }

    /// Apply the geometry matching the CURRENT display. Growth is applied
    /// immediately (the window is transparent — only the shell is visible,
    /// and it morphs via CSS); shrinks are deferred until the shell's morph
    /// animation has played, so the close feels animated instead of a snap.
    fn sync_window(self: &Arc<Self>) {
        // Registry first, with nothing held (see the lock order at the top).
        let taken_at = Instant::now();
        let sessions = self.registry.snapshot();
        let layout = self.layout();

        let (grow_now, seq) = {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.prune_attentions(&sessions, taken_at);
            let display = model.display();
            let options = question_options(model.questions.first());
            // Same row set the webview will render, so the window height
            // matches what is actually listed — counted rather than built,
            // since only the count is wanted here.
            let rows = match display {
                Display::Expanded => sessions
                    .iter()
                    .filter(|s| !matches!(s.state, SessionState::Ended))
                    .count()
                    .min(EXPANDED_MAX_SESSIONS),
                _ => 0,
            };
            let target = layout.size_for(display, rows, options);

            // `win` is taken while `model` is still held on purpose: it makes
            // whichever sync computed its target LAST also the one that
            // records it. Releasing the model lock first let two concurrent
            // syncs compute in one order and commit in the other, leaving the
            // model idle and the window at approval size — an invisible
            // click-eating rectangle across the top of the screen.
            let mut win = self.win.lock().expect("overlay win lock poisoned");
            if win.desired == target && win.applied == Some(target) {
                return; // already there, and demonstrably so
            }
            win.desired = target;
            // A fresh budget per sync, so a spent one cannot make every later
            // state change a single-shot attempt. Syncs are event-driven, so
            // this cannot become a retry treadmill on an idle machine.
            win.retries = 0;
            win.seq += 1;
            let growing = match win.applied {
                Some((w, h)) => target.0 > w || target.1 > h,
                // Nothing applied yet (startup, or the last apply failed):
                // snap immediately rather than sitting wrong for the morph.
                None => true,
            };
            (growing, win.seq)
        };

        if grow_now {
            self.apply_pending();
        } else {
            let overlay = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(MORPH_MS)).await;
                {
                    let win = overlay.win.lock().expect("overlay win lock poisoned");
                    if win.seq != seq {
                        return; // superseded by a newer size, which applied itself
                    }
                }
                overlay.apply_pending();
            });
        }
    }

    /// Drive the window to whatever `win.desired` currently is, and record
    /// what was ACTUALLY applied.
    ///
    /// Two properties matter here and neither is optional:
    ///   * the target is re-read from `desired` INSIDE `window_ops`, so
    ///     concurrent syncs converge on the newest desire instead of the one
    ///     whichever thread happened to be holding;
    ///   * a failed apply leaves `applied` alone, so it is retryable rather
    ///     than sticky — the old code recorded the request and then short-
    ///     circuited every later sync to the same size forever.
    fn apply_pending(self: &Arc<Self>) {
        let _guard = self
            .window_ops
            .lock()
            .expect("overlay window lock poisoned");
        // Bounded: a desire that keeps moving under us is chased a few times
        // and then left to the sync that is already queued behind it.
        for _ in 0..4 {
            let (target, generation) = {
                let win = self.win.lock().expect("overlay win lock poisoned");
                if win.applied == Some(win.desired) {
                    return;
                }
                (win.desired, win.generation)
            };
            match self.apply_window(target) {
                Ok(()) => {
                    let mut win = self.win.lock().expect("overlay win lock poisoned");
                    win.retries = 0;
                    // Only claim this is what the window IS if the screen it
                    // was measured against is still the current one; a
                    // display change mid-apply invalidates the placement and
                    // the loop takes another pass at it.
                    if win.generation == generation {
                        win.applied = Some(target);
                    }
                }
                Err(err) => {
                    log::warn!("overlay: window resize failed: {err}");
                    self.schedule_apply_retry();
                    return;
                }
            }
        }
    }

    /// Re-attempt a failed apply. `primary_monitor()` yields `None` while the
    /// lid is shutting or a display is being reconfigured — transient states
    /// that resolve on their own, but only if something looks again.
    fn schedule_apply_retry(self: &Arc<Self>) {
        let attempt = {
            let mut win = self.win.lock().expect("overlay win lock poisoned");
            win.retries = win.retries.saturating_add(1);
            win.retries
        };
        if attempt > MAX_APPLY_RETRIES {
            log::warn!(
                "overlay: giving up on the resize after {attempt} tries; \
                 the next state change or display reconfiguration retries it"
            );
            return;
        }
        let overlay = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(APPLY_RETRY_MS * attempt as u64)).await;
            overlay.apply_pending();
        });
    }

    fn layout(&self) -> Layout {
        *self.layout.lock().expect("overlay layout lock poisoned")
    }

    /// Resize and re-center on the primary display (§7 fallback; per-terminal
    /// display tracking arrives with the focus work in step 6).
    fn apply_window(&self, (w, h): (f64, f64)) -> tauri::Result<()> {
        let window = overlay_window(&self.app)?;
        let monitor = window
            .primary_monitor()?
            .ok_or(tauri::Error::WindowNotFound)?;
        let scale = monitor.scale_factor();
        let screen_w = monitor.size().width as f64 / scale;
        let screen_x = monitor.position().x as f64 / scale;
        window.set_size(LogicalSize::new(w, h))?;
        window.set_position(LogicalPosition::new(
            screen_x + (screen_w - w) / 2.0,
            self.layout().y,
        ))?;
        Ok(())
    }

    fn emit(self: &Arc<Self>) {
        // ONE registry snapshot, taken with no overlay lock held. The registry
        // mutex is the same one held across blocking disk work (WAL
        // checkpoint, the daily VACUUM), so nesting it under the model lock —
        // which is what `session_rows()` used to do from in here — let any
        // caller block on SQLite while holding the state every update needs.
        // Sampling once also means the header counts and the listed rows can
        // no longer disagree: they used to be two independent reads.
        let taken_at = Instant::now();
        let sessions = self.registry.snapshot();
        let counts = count_sessions(&sessions);
        let now = unix_now();
        let layout = self.layout();

        let pruned = {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            let pruned = model.prune_attentions(&sessions, taken_at);
            let display = model.display();
            let rows = (display == Display::Expanded).then(|| island_rows(&sessions, now));
            let shell = layout.size_for(
                display,
                rows.as_ref().map_or(1, Vec::len),
                question_options(model.questions.first()),
            );
            let view = OverlayView {
                mode: match display {
                    Display::Approval => "approval",
                    Display::Question => "question",
                    Display::Attention => "attention",
                    Display::Toast => "toast",
                    Display::Expanded => "expanded",
                    Display::Idle => "idle",
                },
                has_notch: layout.has_notch,
                shell,
                list_max: EXPANDED_VISIBLE_ROWS as f64 * ROW_H,
                counts,
                toast: match (&model.mode, display) {
                    (Mode::Toast(t), Display::Toast) => Some(t),
                    _ => None,
                },
                attention: (display == Display::Attention)
                    .then(|| {
                        model.attentions.first().map(|info| AttentionCard {
                            info,
                            queued: model.attentions.len() - 1,
                        })
                    })
                    .flatten(),
                approval: (display == Display::Approval)
                    .then(|| {
                        model.approvals.first().map(|info| ApprovalCard {
                            info,
                            queued: model.approvals.len() - 1,
                        })
                    })
                    .flatten(),
                question: (display == Display::Question)
                    .then(|| {
                        model.questions.first().map(|info| QuestionCard {
                            info,
                            queued: model.questions.len() - 1,
                        })
                    })
                    .flatten(),
                sessions: rows,
            };
            if let Err(err) = self.app.emit_to("overlay", "overlay-state", &view) {
                log::warn!("overlay: emit failed: {err}");
            }
            pruned
        };

        // A pin died of old age just now, so the display changed and the
        // window has to follow it — and the view that was just sent is one
        // render behind, so re-emit after.
        //
        // Off this thread, and that is not tidiness. `emit` is reachable ON
        // THE MAIN THREAD (`refresh` from `on_page_load`), `sync_window` can
        // block on `window_ops`, and a background task can hold `window_ops`
        // while parked inside a main-thread window getter — the deadlock this
        // file warns about twice. It cannot recurse either: `attentions` only
        // ever shrinks here, so the follow-up emit prunes nothing.
        if pruned {
            log::debug!("overlay: dropped an attention its session no longer justifies");
            let overlay = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                overlay.sync_window();
                overlay.emit();
            });
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The island's session list: live sessions only, sorted attention →
/// working/unknown → done, most recent first within each bucket.
///
/// `Registry::apply` already drops ended sessions from the live map, so the
/// `Ended` filter is defense-in-depth (a recovered or future-sourced snapshot
/// row must never flood a scrollable list); it also mirrors `count_sessions`,
/// which ignores ended sessions.
fn island_rows(sessions: &[Session], now: i64) -> Vec<SessionRow> {
    // Borrowed, not consumed: callers hold ONE registry snapshot and use it
    // for the counts, the pruning, and these rows (see `emit`).
    let mut sessions: Vec<&Session> = sessions
        .iter()
        .filter(|s| !matches!(s.state, SessionState::Ended))
        .collect();
    sessions.sort_by_key(|s| {
        let priority = match s.state {
            SessionState::NeedsAttention => 0,
            SessionState::Working | SessionState::Unknown => 1,
            SessionState::Done => 2,
            SessionState::Ended => 3,
        };
        (priority, -s.last_event_at)
    });
    sessions
        .into_iter()
        .take(EXPANDED_MAX_SESSIONS)
        .map(|s| SessionRow {
            id: s.id.clone(),
            agent: agent_label(s.agent),
            title: s.title.clone(),
            state: s.state,
            minutes: ((now - s.last_event_at).max(0)) / 60,
        })
        .collect()
}

/// Height budget for a question card: the most options any one question in
/// the call offers, so stepping through a multi-question call never resizes
/// the window mid-answer.
fn question_options(info: Option<&QuestionInfo>) -> usize {
    info.map_or(1, |q| {
        q.questions
            .iter()
            .map(|spec| spec.options.len())
            .max()
            .unwrap_or(1)
            .max(1)
    })
}

fn count_sessions(sessions: &[Session]) -> Counts {
    sessions.iter().fold(Counts::default(), |mut c, s| {
        match s.state {
            SessionState::NeedsAttention => c.attention += 1,
            SessionState::Done => c.done += 1,
            // Unknown sessions are treated as working until an event proves
            // otherwise (§6).
            SessionState::Working | SessionState::Unknown => c.working += 1,
            SessionState::Ended => {}
        }
        c
    })
}

fn overlay_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    app.get_webview_window("overlay")
        .ok_or(tauri::Error::WindowNotFound)
}

fn agent_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::ClaudeCode => "Claude",
        AgentKind::Codex => "Codex",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model {
            mode: Mode::Idle,
            seq: 0,
            approvals: Vec::new(),
            questions: Vec::new(),
            reply_open: false,
            attentions: Vec::new(),
            hovered: false,
            hover_seq: 0,
        }
    }

    fn approval() -> ApprovalInfo {
        ApprovalInfo {
            id: "a".into(),
            session_id: "s".into(),
            event_id: 1,
            agent: AgentKind::ClaudeCode,
            title: "t".into(),
            tool_name: "Bash".into(),
            tool_summary: "x".into(),
        }
    }

    fn attention() -> AttentionView {
        attention_for("s", Instant::now())
    }

    fn attention_for(session_id: &str, pinned_at: Instant) -> AttentionView {
        AttentionView {
            session_id: session_id.into(),
            agent: "Claude",
            title: "t".into(),
            summary: "needs you".into(),
            pinned_at,
        }
    }

    fn session(id: &str, state: SessionState) -> Session {
        Session {
            id: id.into(),
            agent: AgentKind::ClaudeCode,
            cwd: "/tmp".into(),
            title: id.into(),
            state,
            terminal_json: None,
            started_at: 0,
            last_event_at: 0,
        }
    }

    fn question(options: usize) -> QuestionInfo {
        use crate::broker::{QuestionOption, QuestionSpec};
        QuestionInfo {
            id: "q".into(),
            session_id: "s".into(),
            event_id: 1,
            agent: AgentKind::ClaudeCode,
            title: "t".into(),
            tool_use_id: None,
            questions: vec![QuestionSpec {
                question: "which?".into(),
                header: "Pick".into(),
                options: (0..options)
                    .map(|i| QuestionOption {
                        label: format!("opt {i}"),
                        description: String::new(),
                    })
                    .collect(),
                multi_select: false,
            }],
        }
    }

    #[test]
    fn question_card_outranks_passive_states_but_not_approvals() {
        let mut m = model();
        m.attentions.push(attention());
        assert_eq!(m.display(), Display::Attention);
        m.questions.push(question(2));
        assert_eq!(m.display(), Display::Question, "a parked question beats an ask-moment");
        m.approvals.push(approval());
        assert_eq!(
            m.display(),
            Display::Approval,
            "an approval still wins: it is the older parked request"
        );
    }

    #[test]
    fn question_card_sizes_to_the_widest_question_and_is_bounded() {
        let layout = Layout::from_probe(&ScreenProbe {
            top_inset: 32.0,
            notch_width: Some(179.0),
        });
        // Height grows with the option count...
        assert!(layout.question(4).1 > layout.question(2).1);
        // ...but a malformed payload cannot grow the card off the screen.
        assert_eq!(layout.question(99).1, layout.question(QUESTION_MAX_OPTIONS).1);
        // Worst case is the 6-option clamp; AskUserQuestion allows 4 and
        // Codex 3, so the realistic ceiling is the second assertion.
        assert!(layout.question(99).1 <= 500.0);
        assert!(layout.question(4).1 < 400.0);

        // The budget is the WIDEST question in the call, so stepping from
        // question 1 to 2 never resizes the window mid-answer.
        use crate::broker::{QuestionOption, QuestionSpec};
        let mut info = question(2);
        info.questions.push(QuestionSpec {
            question: "and then?".into(),
            header: "Next".into(),
            options: (0..4)
                .map(|i| QuestionOption {
                    label: format!("b{i}"),
                    description: String::new(),
                })
                .collect(),
            multi_select: false,
        });
        assert_eq!(question_options(Some(&info)), 4);
        // An empty/absent question never yields a zero-height card.
        assert_eq!(question_options(None), 1);
    }

    #[test]
    fn open_reply_window_suppresses_the_card_behind_it() {
        let mut m = model();
        m.questions.push(question(2));
        assert_eq!(m.display(), Display::Question);

        // While the typed-answer window is up the card must vanish entirely:
        // it is wider than the reply window, so any part still drawn peeks
        // out around its edges.
        m.reply_open = true;
        assert_eq!(m.display(), Display::Idle, "card hidden while typing");

        // Nothing passive may peek out around the reply window either: not a
        // toast, not an ask-moment, not the hover-expanded list.
        m.hovered = true;
        m.attentions.push(attention());
        assert_eq!(m.display(), Display::Idle, "island stays collapsed while typing");
        m.attentions.clear();
        m.hovered = false;

        // Cancelling the typed answer brings the still-parked question back.
        m.reply_open = false;
        assert_eq!(m.display(), Display::Question);

        // An approval is a different parked request and still outranks it.
        m.reply_open = true;
        m.approvals.push(approval());
        assert_eq!(m.display(), Display::Approval);
    }

    #[test]
    fn a_pin_dies_with_the_need_that_justified_it() {
        let mut m = model();
        let pinned_at = Instant::now();
        let taken_at = pinned_at + Duration::from_millis(1); // snapshot is newer
        m.attentions.push(attention_for("waiting", pinned_at));
        m.attentions.push(attention_for("resumed", pinned_at));
        m.attentions.push(attention_for("killed", pinned_at));

        // "waiting" still needs the user; "resumed" answered in the terminal;
        // "killed" was force-quit and is gone from the live map entirely —
        // the case where the card used to stay pinned forever, taking
        // hover-to-expand down with it.
        let sessions = vec![
            session("waiting", SessionState::NeedsAttention),
            session("resumed", SessionState::Working),
        ];
        assert!(m.prune_attentions(&sessions, taken_at));
        let ids: Vec<&str> = m.attentions.iter().map(|a| a.session_id.as_str()).collect();
        assert_eq!(ids, vec!["waiting"]);
        assert_eq!(m.display(), Display::Attention);

        // With the last live one done too, the island falls back through to
        // hover — which is exactly what one stale pin used to make impossible.
        m.hovered = true;
        assert!(m.prune_attentions(&[session("waiting", SessionState::Done)], taken_at));
        assert!(m.attentions.is_empty());
        assert_eq!(m.display(), Display::Expanded);

        // Idempotent: nothing left to drop is not a change.
        assert!(!m.prune_attentions(&[], taken_at));
    }

    #[test]
    fn a_pin_newer_than_the_snapshot_is_never_pruned() {
        let mut m = model();
        let taken_at = Instant::now();
        // Ingest writes the registry BEFORE pinning, so a snapshot taken
        // after the pin always justifies it. A snapshot taken BEFORE cannot
        // speak for it — pruning on that evidence would drop the card in the
        // instant between the event landing and the registry being read.
        m.attentions
            .push(attention_for("fresh", taken_at + Duration::from_millis(1)));
        assert!(!m.prune_attentions(&[], taken_at));
        assert_eq!(m.attentions.len(), 1);

        // The next snapshot is taken after the pin and does decide it.
        assert!(m.prune_attentions(&[], taken_at + Duration::from_millis(2)));
        assert!(m.attentions.is_empty());
    }

    #[test]
    fn hover_watchdog_keeps_watching_through_a_higher_priority_card() {
        let mut m = model();
        m.hovered = true;
        m.hover_seq = 7;
        assert_eq!(m.display(), Display::Expanded);
        assert!(m.hover_watch_active(7));

        // A toast (or approval, or attention) covers the island and snaps the
        // window to a different size under the pointer, swallowing the
        // webview's mouseleave. The watchdog MUST stay armed: it is the only
        // thing that can clear `hovered`, and nothing re-arms it.
        m.mode = Mode::Toast(ToastView {
            session_id: "s".into(),
            agent: "Claude",
            title: "t".into(),
            state: SessionState::Done,
            summary: String::new(),
        });
        assert_eq!(m.display(), Display::Toast);
        assert!(
            m.hover_watch_active(7),
            "standing down here is what let `hovered` latch true forever"
        );
        m.approvals.push(approval());
        assert!(m.hover_watch_active(7));

        // It stands down only for the two reasons that are actually safe:
        // somebody else already collapsed the hover, or a newer watchdog owns it.
        assert!(!m.hover_watch_active(8));
        m.hovered = false;
        assert!(!m.hover_watch_active(7));
    }

    #[test]
    fn display_precedence() {
        let mut m = model();
        assert_eq!(m.display(), Display::Idle);
        m.hovered = true;
        assert_eq!(m.display(), Display::Expanded);
        m.mode = Mode::Toast(ToastView {
            session_id: "s".into(),
            agent: "Claude",
            title: "t".into(),
            state: SessionState::Done,
            summary: String::new(),
        });
        assert_eq!(m.display(), Display::Toast, "toast beats hover");
        m.attentions.push(attention());
        assert_eq!(m.display(), Display::Attention, "attention beats toast");
        m.approvals.push(approval());
        assert_eq!(m.display(), Display::Approval, "approval beats all");
    }

    #[test]
    fn notch_layout_hugs_notch_width_flush_with_top() {
        let layout = Layout::from_probe(&ScreenProbe {
            top_inset: 32.0,
            notch_width: Some(179.0),
        });
        assert!(layout.has_notch);
        assert_eq!(layout.idle.0, 179.0);
        assert_eq!(layout.y, 0.0);
        assert!(layout.toast.0 >= 480.0);
        assert!(layout.idle.1 > 32.0, "lip below the notch for the dots");
        let (w, h) = layout.expanded(3);
        assert_eq!(w, 440.0);
        assert!(h > layout.idle.1);
        assert!(layout.expanded(100).1 <= layout.expanded(EXPANDED_VISIBLE_ROWS).1);
    }

    #[test]
    fn expanded_height_is_clamped_to_the_visible_rows() {
        let layout = Layout::from_probe(&ScreenProbe {
            top_inset: 32.0,
            notch_width: Some(179.0),
        });
        // Every row count at or beyond the visible cap yields the same window
        // height — overflow scrolls inside the island instead of growing it.
        let capped = layout.expanded(EXPANDED_VISIBLE_ROWS).1;
        assert_eq!(layout.expanded(EXPANDED_MAX_SESSIONS).1, capped);
        assert_eq!(layout.expanded(usize::MAX).1, capped);
        assert!(
            layout.expanded(EXPANDED_VISIBLE_ROWS - 1).1 < capped,
            "under the cap the island still grows per row"
        );
        // 13" MacBook (notched) is ~956 logical pt tall; keep the island well
        // clear of the bottom of the screen.
        assert!(capped < 400.0, "island got too tall: {capped}");
        // The scroll viewport the webview is told to use matches the rows the
        // window height budgets for.
        assert_eq!(
            EXPANDED_VISIBLE_ROWS as f64 * ROW_H,
            capped - layout.expanded_base - HEADER_H - 10.0
        );
    }

    #[test]
    fn island_rows_drop_ended_sessions_and_sort_by_urgency() {
        let mk = |id: &str, state, last_event_at| Session {
            id: id.into(),
            agent: AgentKind::ClaudeCode,
            cwd: "/tmp".into(),
            title: id.into(),
            state,
            terminal_json: None,
            started_at: 0,
            last_event_at,
        };
        let rows = island_rows(
            &[
                mk("old-done", SessionState::Done, 10),
                mk("ended", SessionState::Ended, 900),
                mk("working", SessionState::Working, 20),
                mk("attention", SessionState::NeedsAttention, 5),
                mk("new-done", SessionState::Done, 50),
                mk("unknown", SessionState::Unknown, 30),
            ],
            1_000,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["attention", "unknown", "working", "new-done", "old-done"],
            "ended filtered out; attention > working/unknown > done, newest first"
        );
        assert_eq!(rows[0].minutes, (1_000 - 5) / 60);
    }

    #[test]
    fn island_rows_are_generous_but_bounded() {
        let sessions: Vec<Session> = (0..EXPANDED_MAX_SESSIONS + 25)
            .map(|i| Session {
                id: format!("s{i}"),
                agent: AgentKind::Codex,
                cwd: "/tmp".into(),
                title: "t".into(),
                state: SessionState::Working,
                terminal_json: None,
                started_at: 0,
                last_event_at: i as i64,
            })
            .collect();
        let rows = island_rows(&sessions, 0);
        assert_eq!(rows.len(), EXPANDED_MAX_SESSIONS);
    }

    #[test]
    fn no_notch_layout_floats_below_menu_bar() {
        let layout = Layout::from_probe(&ScreenProbe {
            top_inset: 24.0,
            notch_width: None,
        });
        assert!(!layout.has_notch);
        assert_eq!(layout.y, 32.0, "menu bar height + 8");
    }

    #[test]
    fn counts_bucket_states() {
        let mk = |state| Session {
            id: "x".into(),
            agent: AgentKind::ClaudeCode,
            cwd: "/tmp".into(),
            title: "t".into(),
            state,
            terminal_json: None,
            started_at: 0,
            last_event_at: 0,
        };
        let c = count_sessions(&[
            mk(SessionState::Working),
            mk(SessionState::Unknown),
            mk(SessionState::NeedsAttention),
            mk(SessionState::Done),
        ]);
        assert_eq!((c.working, c.attention, c.done), (2, 1, 1));
    }
}
