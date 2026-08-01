//! Jump-to-session (FR-8, first slice): strategy chain per §6 —
//! tmux pane → host-application focus → logged no-op.
//!
//! The hook shim rarely sees a tty (hooks run with pipes), but it always
//! records the parent pid of the agent process. Walking that process tree
//! upward until a pid belongs to a registered GUI application finds the
//! hosting terminal/editor/app regardless of which one it is — Terminal,
//! iTerm2, WezTerm, VS Code, or the Claude desktop app — and activating it
//! needs no Automation permission. In-app tab selection stays best-effort
//! work for the rest of step 6.

use serde_json::Value;

use crate::registry::Session;
use crate::tmux;

pub fn jump(session: &Session) {
    let Some(raw) = &session.terminal_json else {
        log::info!("focus: session {} has no terminal info", session.id);
        return;
    };
    let terminal: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(err) => {
            log::warn!("focus: bad terminal_json for {}: {err}", session.id);
            return;
        }
    };

    // tmux first (AC-8.1). `pane_for_terminal` also recovers the pane by pid
    // when `$TMUX_PANE` was never recorded, and yields None on machines with
    // no tmux at all — in which case this step is simply skipped, as before.
    if let Some(pane) = tmux::pane_for_terminal(&terminal) {
        tmux::focus_pane(&pane);
        // Fall through: the hosting terminal app still needs to come forward.
    }

    #[cfg(target_os = "macos")]
    {
        let start = terminal["ppid_chain"]
            .as_array()
            .and_then(|c| c.first())
            .and_then(Value::as_i64)
            .or_else(|| terminal["pid"].as_i64());
        if let Some(pid) = start {
            if focus_host_app(pid as i32) {
                return;
            }
        }
        log::info!("focus: no GUI ancestor found for session {}", session.id);
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Windows HWND focus lands with the rest of step 6.
        log::info!("focus: platform focus not implemented yet");
    }
}

/// Walk up the process tree from `pid` until a pid is a registered GUI
/// application; activate it. Returns true when something came forward.
#[cfg(target_os = "macos")]
fn focus_host_app(pid: i32) -> bool {
    let mut current = pid;
    for _ in 0..16 {
        if current <= 1 {
            break;
        }
        if crate::platform::macos::activate_app_with_pid(current) {
            log::info!("focus: activated host app (pid {current})");
            return true;
        }
        // Shared with the tmux pane walk (timeout-bounded `ps`).
        match tmux::parent_pid(current) {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    false
}
