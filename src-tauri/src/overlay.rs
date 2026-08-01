//! Overlay controller (FR-5): owns the island state machine, window
//! geometry, and emission of `overlay-state` snapshots. The Svelte side only
//! renders what it is told (CLAUDE.md: UI stays dumb).
//!
//! Display precedence: approval (pinned, actionable) > attention (pinned
//! ask-moment) > toast (6 s) > hover-expanded session list > idle sliver.
//! All updates are event-driven — no polling loops (AC-5.5). The window can
//! never take keyboard focus; focus theft is release-blocking (AC-5.1).

use std::sync::{Arc, Mutex};
use std::time::Duration;

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
/// AskUserQuestion allows up to 4 options; clamp anyway so a malformed or
/// future payload can never grow the card past the screen.
const QUESTION_MAX_OPTIONS: usize = 6;

/// Window sizes per state, in logical points.
#[derive(Debug, Clone, Copy)]
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
    attention: Option<&'a AttentionView>,
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
    layout: Layout,
    model: Mutex<Model>,
    /// Serializes window resize/reposition; each sync re-reads the current
    /// display inside this lock, so geometry converges to the model.
    window_ops: Mutex<()>,
    /// Last applied window size + a seq for delayed shrinks: growth snaps the
    /// (transparent) window immediately so the shell can morph inside it;
    /// shrink lets the shell animation finish before the window snaps down.
    win: Mutex<WinState>,
}

struct WinState {
    current: (f64, f64),
    seq: u64,
}

/// How long the shell morph animation runs (see Overlay.svelte transition);
/// window shrinks are deferred past it.
const MORPH_MS: u64 = 280;

pub fn init(app: &AppHandle, registry: Arc<Registry>) -> tauri::Result<Arc<Overlay>> {
    let window = overlay_window(app)?;

    #[cfg(target_os = "macos")]
    unsafe {
        platform::macos::apply_overlay_styles(window.ns_window()?);
    }
    #[cfg(windows)]
    unsafe {
        platform::windows::apply_overlay_styles(window.hwnd()?.0);
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
        layout,
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
            current: layout.idle,
            seq: 0,
        }),
    });

    overlay.apply_window(layout.idle)?;
    // The island is interactive (hover to expand, buttons on cards) but can
    // never take keyboard focus (focusable: false, AC-5.1).
    window.set_ignore_cursor_events(false)?;
    window.show()?;
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
    fn pin_attention(self: &Arc<Self>, attention: AttentionView) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
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

    /// Poll the real cursor position while expanded; force the collapse when
    /// the pointer is genuinely gone, regardless of webview event delivery.
    fn spawn_hover_watchdog(self: &Arc<Self>, seq: u64) {
        let overlay = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                {
                    let model = overlay.model.lock().expect("overlay mutex poisoned");
                    if model.hover_seq != seq || model.display() != Display::Expanded {
                        return; // superseded or no longer expanded
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
    pub fn refresh(&self) {
        self.emit();
    }

    /// Apply the geometry matching the CURRENT display. Growth is applied
    /// immediately (the window is transparent — only the shell is visible,
    /// and it morphs via CSS); shrinks are deferred until the shell's morph
    /// animation has played, so the close feels animated instead of a snap.
    fn sync_window(self: &Arc<Self>) {
        let target = {
            let (display, options) = {
                let model = self.model.lock().expect("overlay mutex poisoned");
                (model.display(), question_options(model.questions.first()))
            };
            // Only the expanded island is row-sized; skip the snapshot (and
            // never nest the registry lock under the model lock) otherwise.
            // Same row set the webview will render, so the window height
            // matches what is actually listed.
            let rows = match display {
                Display::Expanded => self.session_rows().len(),
                _ => 0,
            };
            self.layout.size_for(display, rows, options)
        };

        let (grow_now, seq) = {
            let mut win = self.win.lock().expect("overlay win lock poisoned");
            if win.current == target {
                return;
            }
            let growing = target.0 > win.current.0 || target.1 > win.current.1;
            win.current = target;
            win.seq += 1;
            (growing, win.seq)
        };

        if grow_now {
            let _guard = self
                .window_ops
                .lock()
                .expect("overlay window lock poisoned");
            if let Err(err) = self.apply_window(target) {
                log::warn!("overlay: window resize failed: {err}");
            }
        } else {
            let overlay = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(MORPH_MS)).await;
                {
                    let win = overlay.win.lock().expect("overlay win lock poisoned");
                    if win.seq != seq {
                        return; // superseded by a newer size
                    }
                }
                let _guard = overlay
                    .window_ops
                    .lock()
                    .expect("overlay window lock poisoned");
                if let Err(err) = overlay.apply_window(target) {
                    log::warn!("overlay: window resize failed: {err}");
                }
            });
        }
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
            self.layout.y,
        ))?;
        Ok(())
    }

    fn emit(&self) {
        let counts = count_sessions(&self.registry.snapshot());
        let model = self.model.lock().expect("overlay mutex poisoned");
        let display = model.display();
        let sessions = (display == Display::Expanded).then(|| self.session_rows());
        let shell = self.layout.size_for(
            display,
            sessions.as_ref().map_or(1, Vec::len),
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
            has_notch: self.layout.has_notch,
            shell,
            list_max: EXPANDED_VISIBLE_ROWS as f64 * ROW_H,
            counts,
            toast: match (&model.mode, display) {
                (Mode::Toast(t), Display::Toast) => Some(t),
                _ => None,
            },
            attention: (display == Display::Attention)
                .then(|| model.attentions.first())
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
            sessions,
        };
        if let Err(err) = self.app.emit_to("overlay", "overlay-state", &view) {
            log::warn!("overlay: emit failed: {err}");
        }
    }

    fn session_rows(&self) -> Vec<SessionRow> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        island_rows(self.registry.snapshot(), now)
    }
}

/// The island's session list: live sessions only, sorted attention →
/// working/unknown → done, most recent first within each bucket.
///
/// `Registry::apply` already drops ended sessions from the live map, so the
/// `Ended` filter is defense-in-depth (a recovered or future-sourced snapshot
/// row must never flood a scrollable list); it also mirrors `count_sessions`,
/// which ignores ended sessions.
fn island_rows(sessions: Vec<Session>, now: i64) -> Vec<SessionRow> {
    let mut sessions: Vec<Session> = sessions
        .into_iter()
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
            id: s.id,
            agent: agent_label(s.agent),
            title: s.title,
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
        AttentionView {
            session_id: "s".into(),
            agent: "Claude",
            title: "t".into(),
            summary: "needs you".into(),
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
            vec![
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
        let rows = island_rows(sessions, 0);
        assert_eq!(rows.len(), EXPANDED_MAX_SESSIONS);
        assert!(
            EXPANDED_MAX_SESSIONS > EXPANDED_VISIBLE_ROWS,
            "rows must outnumber the visible window or nothing ever scrolls"
        );
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
