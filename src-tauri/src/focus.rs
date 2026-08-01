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


/// The app's handler rejects anything that is not a UUID, and our Codex
/// sessions are keyed `codex-<hash>` — so check the shape before spending an
/// `open` on it.
///
/// Retained (unused) because the deep-link investigation was expensive and
/// the guard is the safe half of it: if a focus-without-import route ever
/// turns up, this is what gates it.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
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
#[allow(dead_code)]
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

    // NO DEEP LINK HERE. `claude://resume?session=<uuid>` works, but what it
    // does is IMPORT the CLI session — every invocation mints ANOTHER desktop
    // session ("General coding session" x N in Recents) instead of focusing
    // the existing conversation. It is a bring-a-CLI-session-into-the-app
    // feature, not a navigation one, and using it for jump duplicated a
    // user's sessions three times before this was caught. The app's own log
    // says "importing"; that was the tell.
    //
    // Landing on the exact conversation needs a focus/select route the app
    // does not appear to expose to URL handlers. Until one is found, jump
    // brings the right APP forward and no further.

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
