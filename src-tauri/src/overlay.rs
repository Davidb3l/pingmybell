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

use crate::broker::ApprovalInfo;
use crate::platform::{self, ScreenProbe};
use crate::registry::{AgentKind, EventKind, NormalizedEvent, Registry, Session, SessionState};

const TOAST_SECS: u64 = 6;
const HOVER_COLLAPSE_MS: u64 = 300;
const EXPANDED_MAX_ROWS: usize = 6;

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

    fn expanded(&self, rows: usize) -> (f64, f64) {
        let rows = rows.clamp(1, EXPANDED_MAX_ROWS) as f64;
        (440.0, self.expanded_base + rows * 34.0 + 10.0)
    }
}

#[derive(Debug, Clone, Serialize)]
struct ToastView {
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
struct OverlayView<'a> {
    mode: &'static str,
    has_notch: bool,
    counts: Counts,
    toast: Option<&'a ToastView>,
    attention: Option<&'a AttentionView>,
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
    registry: Arc<Registry>,
    layout: Layout,
    model: Mutex<Model>,
    /// Serializes window resize/reposition; each sync re-reads the current
    /// display inside this lock, so geometry converges to the model.
    window_ops: Mutex<()>,
}

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

    let overlay = Arc::new(Overlay {
        app: app.clone(),
        registry,
        layout,
        model: Mutex::new(Model {
            mode: Mode::Idle,
            seq: 0,
            approvals: Vec::new(),
            attentions: Vec::new(),
            hovered: false,
            hover_seq: 0,
        }),
        window_ops: Mutex::new(()),
    });

    overlay.apply_window(layout.idle)?;
    // The island is interactive (hover to expand, buttons on cards) but can
    // never take keyboard focus (focusable: false, AC-5.1).
    window.set_ignore_cursor_events(false)?;
    window.show()?;
    overlay.emit();
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
            EventKind::SessionStart | EventKind::SessionEnd => {
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

    fn collapse_if(&self, seq: u64) {
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
    fn pin_attention(&self, attention: AttentionView) {
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
    pub fn clear_attention(&self, session_id: &str) {
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
    pub fn pin_approval(&self, info: ApprovalInfo) {
        {
            let mut model = self.model.lock().expect("overlay mutex poisoned");
            model.approvals.push(info);
        }
        self.sync_window();
        self.emit();
    }

    /// Remove an approval (decided or expired).
    pub fn unpin_approval(&self, id: &str) {
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

    /// Is the global cursor within the overlay window frame (small margin)?
    fn cursor_inside(&self) -> Option<bool> {
        let window = overlay_window(&self.app).ok()?;
        let cursor = window.cursor_position().ok()?;
        let origin = window.outer_position().ok()?;
        let size = window.outer_size().ok()?;
        const MARGIN: f64 = 8.0;
        Some(
            cursor.x >= origin.x as f64 - MARGIN
                && cursor.x <= origin.x as f64 + size.width as f64 + MARGIN
                && cursor.y >= origin.y as f64 - MARGIN
                && cursor.y <= origin.y as f64 + size.height as f64 + MARGIN,
        )
    }

    /// Re-emit the current state; used when the overlay webview (re)loads
    /// after the initial setup emit was sent into the void.
    pub fn refresh(&self) {
        self.emit();
    }

    /// Apply the geometry matching the CURRENT display, serialized so
    /// concurrent state changes can't interleave stale sizes.
    fn sync_window(&self) {
        let _guard = self
            .window_ops
            .lock()
            .expect("overlay window lock poisoned");
        let target = {
            let model = self.model.lock().expect("overlay mutex poisoned");
            match model.display() {
                Display::Approval => self.layout.approval,
                Display::Attention => self.layout.attention,
                Display::Toast => self.layout.toast,
                Display::Expanded => self.layout.expanded(self.registry.snapshot().len()),
                Display::Idle => self.layout.idle,
            }
        };
        if let Err(err) = self.apply_window(target) {
            log::warn!("overlay: window resize failed: {err}");
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
        let view = OverlayView {
            mode: match display {
                Display::Approval => "approval",
                Display::Attention => "attention",
                Display::Toast => "toast",
                Display::Expanded => "expanded",
                Display::Idle => "idle",
            },
            has_notch: self.layout.has_notch,
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
        let mut sessions = self.registry.snapshot();
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
            .take(EXPANDED_MAX_ROWS)
            .map(|s| SessionRow {
                agent: agent_label(s.agent),
                title: s.title,
                state: s.state,
                minutes: ((now - s.last_event_at).max(0)) / 60,
            })
            .collect()
    }
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

    #[test]
    fn display_precedence() {
        let mut m = model();
        assert_eq!(m.display(), Display::Idle);
        m.hovered = true;
        assert_eq!(m.display(), Display::Expanded);
        m.mode = Mode::Toast(ToastView {
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
        assert!(layout.expanded(100).1 <= layout.expanded(EXPANDED_MAX_ROWS).1);
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
