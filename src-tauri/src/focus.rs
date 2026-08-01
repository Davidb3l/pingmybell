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

use crate::registry::{AgentKind, Session};
use crate::tmux;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn only_real_cli_uuids_are_deep_linked() {
        // Claude CLI session ids are UUIDs and are what the app expects.
        assert!(is_cli_session_uuid("40dc6488-8413-4bb0-86ff-c9a8227c90a5"));
        // Codex sessions are keyed by cwd hash — never send these.
        assert!(!is_cli_session_uuid("codex-e8d4cef2ad747306"));
        // Shape guards: length, separator positions, hex only.
        assert!(!is_cli_session_uuid(""));
        assert!(!is_cli_session_uuid("40dc6488-8413-4bb0-86ff-c9a8227c90a"));
        assert!(!is_cli_session_uuid("40dc64888413-4bb0-86ff-c9a8227c90a5"));
        assert!(!is_cli_session_uuid("40dc6488-8413-4bb0-86ff-c9a8227c90az"));
        // A path-traversal-ish payload must never reach `open`.
        assert!(!is_cli_session_uuid("../../etc/passwd"));
    }
}

/// The app's handler rejects anything that is not a UUID, and our Codex
/// sessions are keyed `codex-<hash>` — so check the shape before spending an
/// `open` on it.
#[cfg(target_os = "macos")]
fn is_cli_session_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Hand a URL to LaunchServices. `open` exits 0 whenever the scheme is
/// registered — it cannot tell us the app then rejected the payload — which
/// is exactly why `is_cli_session_uuid` gates this.
#[cfg(target_os = "macos")]
fn open_url(url: &str) -> bool {
    std::process::Command::new("open")
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The desktop app that hosts each agent, for the fallback above. Verified on
/// this machine via `osascript -e 'id of app "…"'`. A terminal-run agent
/// simply will not match a running app, and the caller degrades to a log.
#[cfg(target_os = "macos")]
fn host_bundle_id(agent: AgentKind) -> Option<&'static str> {
    match agent {
        AgentKind::ClaudeCode => Some("com.anthropic.claudefordesktop"),
        AgentKind::Codex => Some("com.openai.codex"),
    }
}

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

    // Claude Code: the deep link is the ONLY strategy that lands on the right
    // conversation rather than merely the right app, so it goes first.
    //
    // Shape verified against Claude.app on 2026-08-01 by reading its URL
    // handler and watching its log: the parameter is `session` (NOT
    // `sessionId`, which parses as null), and the value must match the app's
    // own UUID regex or it is rejected outright. On success the app logs
    // "Resume deep link: importing CLI session <id>".
    #[cfg(target_os = "macos")]
    if session.agent == AgentKind::ClaudeCode && is_cli_session_uuid(&session.id) {
        if open_url(&format!("claude://resume?session={}", session.id)) {
            log::info!("focus: deep-linked to claude session {}", session.id);
            return;
        }
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
        // The recorded process is gone. This is the COMMON case, not an edge
        // one: each agent session is its own short-lived process, so by the
        // time a user clicks a finished session in the island the pid is dead
        // and the walk above finds nothing. Bring the hosting app forward
        // instead — that is what "jump to this session" means to the user.
        if let Some(bundle_id) = host_bundle_id(session.agent) {
            if crate::platform::macos::activate_app_with_bundle_id(bundle_id) {
                log::info!(
                    "focus: recorded process gone; activated {bundle_id} for session {}",
                    session.id
                );
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
