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

#[cfg(target_os = "macos")]
use crate::registry::AgentKind;
use crate::registry::Session;
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
        // A fleet event has no window to raise: it came from a file another
        // tool appended to, possibly with nothing interactive running at all.
        // `None` is the honest answer and the caller already handles it.
        AgentKind::Suite => None,
    }
}

/// The session's recorded terminal, parsed. `None` when nothing was recorded
/// or the record does not parse — both of which mean "do not know", and every
/// consumer treats that as SPEAK / do nothing.
pub fn terminal_of(session: &Session) -> Option<Value> {
    serde_json::from_str(session.terminal_json.as_ref()?).ok()
}

/// The pid the session's agent was launched from, if it was recorded.
///
/// The SAME extraction `jump` uses, deliberately shared: quieting asks "is
/// that terminal in front of me" and jump asks "bring that terminal forward",
/// and the two must never disagree about which process they mean.
///
/// A pid that is not positive is ABSENT, not an answer. Windows shims before
/// PMB-12 recorded `ppid_chain: [0]`, and `Some(0)` used to win over the
/// `pid` fallback simply because it parsed — so the fallback was dead code on
/// exactly the platform it existed for, and the speaker was handed a pid no
/// process can have.
pub fn terminal_pid_in(terminal: &Value) -> Option<i32> {
    let usable = |pid: i64| (pid > 0 && pid <= i32::MAX as i64).then_some(pid as i32);
    terminal["ppid_chain"]
        .as_array()
        .and_then(|chain| chain.first())
        .and_then(Value::as_i64)
        .and_then(usable)
        .or_else(|| terminal["pid"].as_i64().and_then(usable))
}

/// An ancestry the SHIM walked itself, while every process in it was alive.
///
/// Only the Windows shim records one (PMB-12): Windows has no `getppid`, the
/// app has no live walk there, and the shim exits within milliseconds — so
/// the chain has to be captured at hook time or not at all. Each entry
/// carries its creation time, which is that process's IDENTITY alongside its
/// pid: Windows recycles pids readily, and a pid alone minutes later proves
/// nothing, but a pid with the same creation time is the same process.
///
/// A unix shim records one hop and no creation times, and gets `None` here —
/// its chain is walked live by the app, as it always was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedChain {
    /// `(pid, created_ms)`, nearest ancestor first, as recorded.
    pub entries: Vec<(i32, i64)>,
}

pub fn recorded_chain(terminal: &Value) -> Option<RecordedChain> {
    let pids = terminal["ppid_chain"].as_array()?;
    let created = terminal["ppid_chain_created_ms"].as_array()?;
    // Misaligned lists are a corrupt record, not a shorter one: pairing them
    // up anyway could attach one process's identity to another's pid.
    if pids.len() != created.len() {
        return None;
    }
    let entries: Vec<(i32, i64)> = pids
        .iter()
        .zip(created)
        .filter_map(|(pid, ms)| Some((pid.as_i64()?, ms.as_i64()?)))
        .filter(|(pid, ms)| *pid > 0 && *pid <= i32::MAX as i64 && *ms > 0)
        .map(|(pid, ms)| (pid as i32, ms))
        .collect();
    (!entries.is_empty()).then_some(RecordedChain { entries })
}

/// What one chain entry puts in front of you (see [`host_in_chain`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presents {
    /// A window belonging to this pid: the host, if the entry is genuine.
    Window(i32),
    /// The entry DOES belong to a terminal, but that terminal is not on
    /// screen (a Windows Terminal hidden to the tray). Decisive: nothing
    /// above it is the terminal either, so the search stops and it SPEAKS.
    Hidden,
    /// No window of its own: keep climbing.
    Nothing,
}

/// The application hosting a recorded chain: for the nearest entry that is
/// still the recorded process AND presents something, the pid of the window
/// it presents — or no host at all if what it presents is hidden.
///
/// "Presents" is injected because on Windows it is not always the entry's
/// own window. A shell in a Windows Terminal tab owns only a ConPTY
/// pseudo-console window, which WT OWNS — the window you are looking at is
/// WT's, so that is the host. And "nearest", not "anything in the chain",
/// because the chain does not stop at the terminal: it climbs into whatever
/// launched it, and explorer.exe owns the desktop and the taskbar. (Review
/// finding: a shell WT ADOPTED from a Start-menu launch is explorer's child,
/// and before the pseudo-console rule explorer was the nearest window owner
/// — quieting the session while you looked at the desktop, and speaking
/// while you looked at its terminal.)
///
/// A HIDDEN terminal stops the search rather than being stepped over (second
/// review round): otherwise a WT window minimised to the tray let the climb
/// continue to whatever launched the shell — a second explorer.exe folder
/// window, a file manager — and quieted the session while you looked at
/// that.
///
/// An entry that fails `is_recorded` (exited, or its pid recycled) is
/// SKIPPED rather than ending the search: the hook's wrapper shells are gone
/// within milliseconds, while the agent and its terminal above them are
/// alive for the whole session.
///
/// Pure (both lookups injected) so the rule is tested on every platform.
pub fn host_in_chain(
    entries: &[(i32, i64)],
    is_recorded: impl Fn(i32, i64) -> bool,
    presented_by: impl Fn(i32) -> Presents,
) -> Option<i32> {
    for &(pid, created_ms) in entries {
        let presents = presented_by(pid);
        if presents == Presents::Nothing || !is_recorded(pid, created_ms) {
            continue;
        }
        return match presents {
            Presents::Window(host) => Some(host),
            Presents::Hidden | Presents::Nothing => None,
        };
    }
    None
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
    // RE-CHECKED 2026-08-05 against Claude.app 1.25927.0 (built 08-04), by
    // firing each form and reading `~/Library/Logs/Claude/main.log`:
    //
    //   claude://resume/<uuid>          → "missing or invalid session
    //                                      { sessionId: null }" (path form
    //                                      is not read at all)
    //   claude://resume?sessionId=<uuid> → same warning; wrong param name
    //   claude://resume?session=<uuid>   → "importing CLI session <uuid>",
    //                                      and a SECOND desktop session
    //                                      appeared for a cliSessionId that
    //                                      already had one
    //
    // So the behaviour is unchanged: still an import, still duplicates, even
    // when the target already exists in the app. The app DOES have the route
    // we want — `setFocusedSession(sessionId)` and
    // `signalSessionIntent({kind:"resume", sessionId})` on its
    // `LocalAgentModeSessions` interface — but only as internal Electron IPC
    // behind origin validation, with no URL or CLI surface. Until one exists,
    // jump brings the right APP forward and no further.

    #[cfg(target_os = "macos")]
    {
        // The same extraction quieting uses — see `terminal_pid_in`.
        if let Some(pid) = terminal_pid_in(&terminal) {
            if focus_host_app(pid) {
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
    use serde_json::json;

    #[test]
    fn a_zero_seed_is_absent_so_the_fallback_is_reached() {
        // The pre-PMB-12 Windows record. `Some(0)` used to come back because
        // it PARSED, which made the `pid` fallback unreachable.
        let windows_before = json!({"pid": 4242, "ppid_chain": [0]});
        assert_eq!(terminal_pid_in(&windows_before), Some(4242));
        for junk in [json!([-1]), json!([1i64 << 40]), json!(["x"]), json!([]), json!(null)] {
            let t = json!({"pid": 77, "ppid_chain": junk});
            assert_eq!(terminal_pid_in(&t), Some(77), "{junk}");
        }
        // And junk in BOTH places is absent, not zero.
        assert_eq!(terminal_pid_in(&json!({"pid": 0, "ppid_chain": [0]})), None);
        assert_eq!(terminal_pid_in(&json!({})), None);
        // A real seed still wins over the shim's own pid.
        assert_eq!(terminal_pid_in(&json!({"pid": 9, "ppid_chain": [8, 7]})), Some(8));
    }

    #[test]
    fn only_a_chain_with_identities_counts_as_recorded() {
        // Unix: one hop, no creation times — walked live by the app.
        assert_eq!(recorded_chain(&json!({"ppid_chain": [812]})), None);
        // Windows after PMB-12: the whole chain, each with its identity.
        assert_eq!(
            recorded_chain(&json!({"ppid_chain": [30, 20], "ppid_chain_created_ms": [300, 200]})),
            Some(RecordedChain { entries: vec![(30, 300), (20, 200)] })
        );
        // Junk entries drop out; nothing usable at all is no chain.
        assert_eq!(
            recorded_chain(&json!({"ppid_chain": [0, 30, -4], "ppid_chain_created_ms": [1, 300, 1]})),
            Some(RecordedChain { entries: vec![(30, 300)] })
        );
        assert_eq!(recorded_chain(&json!({"ppid_chain": [0], "ppid_chain_created_ms": [5]})), None);
        assert_eq!(recorded_chain(&json!({"ppid_chain": [30], "ppid_chain_created_ms": [0]})), None);
        // Misaligned lists are corrupt, never re-paired.
        assert_eq!(
            recorded_chain(&json!({"ppid_chain": [30, 20], "ppid_chain_created_ms": [200]})),
            None
        );
    }

    #[test]
    fn the_host_is_the_terminal_presenting_the_nearest_live_entry() {
        use Presents::{Hidden, Nothing, Window};
        // Recorded on this machine, 2026-10-11: a `claude -p` in a WT tab.
        // hook bash 41016 ← bash 35896 ← claude 35488 ← powershell 32508
        // ← WindowsTerminal 5892. The shell owns only a pseudo-console
        // window, which WT owns, so it PRESENTS as 5892.
        let chain = [(41016, 9), (35896, 8), (35488, 7), (32508, 6), (5892, 5)];
        let alive = |pid: i32, _| pid != 41016 && pid != 35896; // wrappers exited
        let presents = |pid: i32| match pid {
            32508 | 5892 => Window(5892),
            _ => Nothing,
        };
        assert_eq!(host_in_chain(&chain, alive, presents), Some(5892));

        // The first review's case: a shell WT ADOPTED from a Start-menu
        // launch is explorer's child (2396 ← explorer 5388, measured). Its
        // pseudo console presents as WT, so WT is the host — never explorer.
        let chain = [(35488, 7), (2396, 6), (5388, 5)];
        let presents = |pid: i32| match pid {
            2396 => Window(5892),
            5388 => Window(5388), // were explorer ever asked, it would answer
            _ => Nothing,
        };
        assert_eq!(host_in_chain(&chain, |_, _| true, presents), Some(5892));

        // The second review's case: that WT is hidden to the tray. The shell
        // still belongs to it, so the search STOPS — it must not climb on to
        // a launcher with a window of its own (a second explorer, a file
        // manager) and quiet the session while you look at that.
        let presents = |pid: i32| match pid {
            2396 => Hidden,
            5388 => Window(5388),
            _ => Nothing,
        };
        assert_eq!(host_in_chain(&chain, |_, _| true, presents), None);

        // A dead wrapper whose pid now belongs to an app with a window is
        // SKIPPED, not taken as the host — and not where the search stops,
        // even when what that stranger presents is hidden.
        let chain = [(41016, 9), (35488, 7), (5892, 5)];
        for stranger in [Window(41016), Hidden] {
            let presents = |pid: i32| match pid {
                41016 => stranger,
                5892 => Window(5892),
                _ => Nothing,
            };
            let recorded = |pid: i32, _| pid != 41016;
            assert_eq!(host_in_chain(&chain, recorded, presents), Some(5892));
        }

        // Identity is checked with the RECORDED creation time.
        let chain = [(5892, 5)];
        let presents = |pid: i32| Window(pid);
        assert_eq!(host_in_chain(&chain, |_, ms| ms == 5, presents), Some(5892));
        assert_eq!(host_in_chain(&chain, |_, ms| ms == 6, presents), None);

        // Nothing presents a window, or nothing is still recorded: no host,
        // so nothing can be frontmost, so it SPEAKS.
        assert_eq!(host_in_chain(&chain, |_, _| true, |_| Nothing), None);
        assert_eq!(host_in_chain(&chain, |_, _| false, presents), None);
        assert_eq!(host_in_chain(&[], |_, _| true, presents), None);
    }

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
