//! Overlay controller (FR-5): owns the idle ⇄ toast state machine, window
//! geometry, and emission of `overlay-state` snapshots. The Svelte side only
//! renders what it is told (CLAUDE.md: UI stays dumb).
//!
//! All updates are event-driven — no polling loops (AC-5.5). The window is
//! click-through and never focusable; focus theft is release-blocking
//! (AC-5.1).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewWindow};

use crate::platform::{self, ScreenProbe};
use crate::registry::{AgentKind, EventKind, NormalizedEvent, Registry, Session, SessionState};

const TOAST_SECS: u64 = 6;

/// Window sizes per state, in logical points.
#[derive(Debug, Clone, Copy)]
struct Layout {
    has_notch: bool,
    idle: (f64, f64),
    toast: (f64, f64),
    /// Top offset of the window (0 = flush with screen top for the notch).
    y: f64,
}

impl Layout {
    fn from_probe(probe: &ScreenProbe) -> Self {
        match probe.notch_width {
            // Flush with the notch: idle hugs its width with a thin lip below
            // for the dots; toasts extend below and wider (AC-5.2).
            Some(width) => Layout {
                has_notch: true,
                idle: (width, probe.top_inset + 16.0),
                toast: (width.max(480.0), probe.top_inset + 46.0),
                y: 0.0,
            },
            // Floating pill under the menu bar (or at top on Windows).
            None => Layout {
                has_notch: false,
                idle: (150.0, 30.0),
                toast: (480.0, 58.0),
                y: probe.top_inset + 8.0,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ToastView {
    agent: &'static str,
    title: String,
    state: SessionState,
    summary: String,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
struct Counts {
    working: u32,
    attention: u32,
    done: u32,
}

#[derive(Serialize)]
struct OverlayView<'a> {
    mode: &'static str,
    has_notch: bool,
    counts: Counts,
    toast: Option<&'a ToastView>,
}

enum Mode {
    Idle,
    Toast(ToastView),
}

struct Model {
    mode: Mode,
    /// Monotonic toast id: a collapse timer only fires if no newer toast
    /// replaced the one it was armed for.
    seq: u64,
}

pub struct Overlay {
    app: AppHandle,
    registry: Arc<Registry>,
    layout: Layout,
    model: Mutex<Model>,
    /// Serializes window resize/reposition so a stale collapse timer can't
    /// interleave its geometry with a newer toast's (each sync re-reads the
    /// current mode inside this lock, so geometry converges to the model).
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
        }),
        window_ops: Mutex::new(()),
    });

    overlay.apply_window(layout.idle)?;
    // Click-through: the overlay never participates in mouse or keyboard
    // interaction in step 3 (approval buttons arrive in step 4).
    window.set_ignore_cursor_events(true)?;
    window.show()?;
    overlay.emit();
    Ok(overlay)
}

impl Overlay {
    /// Feed a registry event through the overlay state machine.
    pub fn on_event(self: &Arc<Self>, event: &NormalizedEvent, session: &Session) {
        match event.event {
            EventKind::TurnComplete | EventKind::NeedsAttention | EventKind::PermissionRequest => {
                self.show_toast(event.agent, session, event.summary.as_deref().unwrap_or(""));
            }
            EventKind::SessionStart | EventKind::SessionEnd => {
                // Counts changed; refresh whatever is on screen.
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

    /// Re-emit the current state; used when the overlay webview (re)loads
    /// after the initial setup emit was sent into the void.
    pub fn refresh(&self) {
        self.emit();
    }

    /// Apply the geometry matching the CURRENT mode, serialized so concurrent
    /// state changes can't interleave stale sizes with fresh positions.
    fn sync_window(&self) {
        let _guard = self
            .window_ops
            .lock()
            .expect("overlay window lock poisoned");
        let target = {
            let model = self.model.lock().expect("overlay mutex poisoned");
            match model.mode {
                Mode::Idle => self.layout.idle,
                Mode::Toast(_) => self.layout.toast,
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
            .ok_or_else(|| tauri::Error::WindowNotFound)?;
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
        let counts = self.counts();
        let model = self.model.lock().expect("overlay mutex poisoned");
        let (mode, toast) = match &model.mode {
            Mode::Idle => ("idle", None),
            Mode::Toast(t) => ("toast", Some(t)),
        };
        let view = OverlayView {
            mode,
            has_notch: self.layout.has_notch,
            counts,
            toast,
        };
        if let Err(err) = self.app.emit_to("overlay", "overlay-state", &view) {
            log::warn!("overlay: emit failed: {err}");
        }
    }

    fn counts(&self) -> Counts {
        count_sessions(&self.registry.snapshot())
    }
}

fn count_sessions(sessions: &[Session]) -> Counts {
    sessions.iter().fold(Counts::default(), |mut c, s| {
        match s.state {
            SessionState::NeedsAttention => c.attention += 1,
            SessionState::Done => c.done += 1,
            // Unknown sessions are treated as working until an event
            // proves otherwise (§6).
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
