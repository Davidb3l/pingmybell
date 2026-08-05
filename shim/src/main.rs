//! PingMyBell hook shim.
//!
//! Invoked by agent hooks: `pingmybell-shim claude <session-start|stop|notification|session-end>`.
//! Reads the hook JSON from stdin, maps it to a normalized event
//! (ARCHITECTURE.md §4), and POSTs it to the local ingest server discovered
//! via `~/.pingmybell/{port,token}`.
//!
//! Hard invariant (PRD AC-2.4): fail open. Any error — missing files, app not
//! running, bad JSON, network trouble, panic — must end with exit 0 and no
//! stdout, so a broken or absent PingMyBell never blocks or alters agent
//! behavior. Only step-4 `pretool` decisions will ever print to stdout.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

const STDIN_CAP_BYTES: u64 = 5 * 1024 * 1024;
/// Plenty for a basename or a command's first word; anything longer is not a
/// label, and the wire is not the place to find that out.
const MAX_ACTIVITY_LABEL_CHARS: usize = 120;
/// The ticker runs after EVERY tool call, so it gets the shortest budget in
/// the shim: long enough for a loopback round trip against a healthy app,
/// short enough that a wedged one cannot hold up an agent's work.
const ACTIVITY_READ_TIMEOUT: Duration = Duration::from_millis(400);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const IO_TIMEOUT: Duration = Duration::from_millis(1500);
/// /v1/approval parks up to 110 s server-side; this must exceed that and stay
/// well inside the PreToolUse hook timeout. Approvals are NOT extendable —
/// deciding on a tool call is a two-second yes/no, so the budget stays where
/// it always was even though the hook now allows far more.
const APPROVAL_READ_TIMEOUT: Duration = Duration::from_secs(115);

/// /v1/question is different: the server EXTENDS its park while the user is
/// actually typing a free-text answer, up to a 540 s ceiling. This must
/// outlast that ceiling and still leave the hook room to receive our stdout.
///
/// Budgets, outermost first: 600 s hook timeout on the AskUserQuestion matcher
/// (what the installers write — verified empirically that claude 2.1.198
/// honours a configured 600 s hook for a full 560 s, so the old 120 s was our
/// config and not a cap) > 570 s here > 540 s server ceiling > 110 s base
/// park. If a user has NOT re-run `install-claude`, their hook is still at
/// 120 s and the agent kills us first: no stdout, exit 0, its own selector
/// renders. Degraded, never broken (PRD AC-2.4).
const QUESTION_READ_TIMEOUT: Duration = Duration::from_secs(570);

/// The tool Claude Code uses to ask the user a question. It arrives through
/// PreToolUse like any other tool (verified against claude 2.1.198), which is
/// what lets PingMyBell answer it from the overlay.
const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// Codex's equivalent (verified against codex-cli 0.146.0-alpha.9.2). Codex
/// grew Claude-shaped hooks: a `PreToolUse` hook on this tool receives the
/// same envelope and can answer through the same deny channel (§5.2).
const REQUEST_USER_INPUT: &str = "request_user_input";

/// Codex's approval hook event. NOT `PreToolUse`: verified against codex-cli
/// 0.146.0-alpha.9.2 that exec and file-change approvals arrive on a separate
/// `PermissionRequest` event, which is the only one whose decision can say
/// "allow" and have the command actually run (§5.2.2).
const PERMISSION_REQUEST: &str = "PermissionRequest";

/// The two tools our `PermissionRequest` matcher selects. Codex deliberately
/// reuses Claude Code's `Bash` for EVERY exec flavour (shell and unified_exec
/// both report it) and `apply_patch` for file edits.
const CODEX_EXEC_TOOL: &str = "Bash";
const CODEX_PATCH_TOOL: &str = "apply_patch";

/// What the model is told when the user denies from the overlay. Unlike the
/// `PreToolUse` deny channel, Codex does NOT wrap this — it reaches the model
/// verbatim as `Rejected("<message>")` — so it is written for the model.
const CODEX_DENY_MESSAGE: &str =
    "The user denied this from PingMyBell. Do not retry it; ask what they would like instead.";

fn main() {
    // Swallow panic output too: nothing we do may leak onto the hook's
    // stdout/stderr or produce a nonzero exit.
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(run);
    // Implicit exit 0 on every path.
}

fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("claude") if args.len() >= 2 => run_claude(&args[1]),
        Some("codex") => run_codex(&args[1..]),
        // Distinct subcommand, not a `codex` mode: notify delivers its payload
        // as argv while the hook delivers it on stdin, and the two must never
        // be confused for one another.
        Some("codex-ask") => run_codex_ask(),
        // Codex's exec / file-change approvals. A THIRD distinct channel:
        // different hook event (`PermissionRequest`), different output shape,
        // and — unlike `codex-ask` — gated behind `gate_codex_approvals`.
        Some("codex-approve") => run_codex_approve(),
        // Codex lifecycle (§11.1): the same Claude-shaped hook envelope, on
        // stdin. Fire-and-forget posts — nothing parks, nothing prints.
        Some(
            sub @ ("codex-session-start" | "codex-prompt-submit" | "codex-stop"
            | "codex-session-end"),
        ) => run_codex_lifecycle(sub),
        _ => {}
    }
}

fn run_claude(subcommand: &str) {
    let mut input = String::new();
    if std::io::stdin()
        .take(STDIN_CAP_BYTES)
        .read_to_string(&mut input)
        .is_err()
    {
        return;
    }
    let hook: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };

    if subcommand == "pretool" {
        run_pretool(&hook);
        return;
    }
    if subcommand == "posttool" {
        run_posttool(&hook);
        return;
    }

    let Some(body) = map_claude_hook(subcommand, &hook) else {
        return;
    };
    post_event(&body.to_string(), "/v1/event", IO_TIMEOUT);
}

/// `PostToolUse` → the activity ticker (§12.1).
///
/// Verified against claude 2.1.198 by capturing real payloads (four tool
/// calls through a throwaway `--settings` file, 2026-08-05): the envelope is
/// the same `session_id` / `cwd` / `hook_event_name` one `PreToolUse` uses,
/// plus `tool_name`, `tool_input`, `tool_response`, `tool_use_id`,
/// `duration_ms`, `effort` and `prompt_id`. Registered with NO matcher, so it
/// fires for every tool — `Read`, `ToolSearch` and `TaskCreate` all arrived,
/// none of which our PreToolUse matcher lists.
///
/// `tool_response` carries stdout and whole file contents. It never leaves
/// this function, and neither does `tool_input` beyond ONE label: §9
/// invariant 4 applies to a ticker exactly as it does to a summary.
fn run_posttool(hook: &Value) {
    let Some(body) = map_posttool(hook) else {
        return;
    };
    // The response IS read, even though a 202 tells us nothing.
    //
    // Writing and walking away looks free and is not: axum drops a handler
    // whose client has gone, which is the same cancellation the approval path
    // relies on deliberately. A shim that exited the instant the bytes were
    // written raced the server into that cancellation and the ticker never
    // fired at all — every unit test still passed, because they cover the
    // mapper and not the wire. The whole saving was 0.7 ms.
    //
    // A short read budget instead: the handler does a lock and an emit, so a
    // server that has not answered in 400 ms is wedged, and no tool call
    // should wait on it.
    post_event(&body.to_string(), "/v1/activity", ACTIVITY_READ_TIMEOUT);
}

/// What the ticker is allowed to know: the tool, and one label.
fn map_posttool(hook: &Value) -> Option<Value> {
    let session_id = hook["session_id"].as_str().filter(|s| !s.is_empty())?;
    let tool = hook["tool_name"].as_str().filter(|t| !t.is_empty())?;
    Some(json!({
        "session_id": session_id,
        "tool": tool,
        "label": activity_label(tool, &hook["tool_input"]),
    }))
}

/// One label per tool call, or none.
///
/// Never arguments and never content: the first WORD of a command (`cargo`,
/// not `cargo test --workspace -- --nocapture`) and the basename of a path
/// (`registry.rs`, not the tree it sits in, which is also a good deal of
/// somebody's private directory structure). Every other tool shows its name
/// alone — captured payloads put a query in `query`, a subject in `subject`
/// and a description in `description`, none of which is ours to narrate.
fn activity_label(tool: &str, input: &Value) -> Option<String> {
    let raw = match tool {
        "Bash" | "BashOutput" => input["command"]
            .as_str()?
            .split_whitespace()
            .next()?
            .to_string(),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            basename(input["file_path"].as_str()?).to_string()
        }
        _ => return None,
    };
    // Bounded before it reaches the wire; the core caps it again for display.
    // A path or command is not user prose and has no business being long.
    Some(raw.chars().take(MAX_ACTIVITY_LABEL_CHARS).collect())
}

/// Last path segment, for either separator — the shim ships on Windows too,
/// where `\` is the separator and `/` still turns up in tool arguments.
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
}

/// Codex notify (§5.2): the payload arrives as a single JSON argv (with a
/// stdin fallback in case the delivery mechanism changes). Notification-only
/// — Codex has no blocking hooks.
///
/// Codex supports exactly ONE notify program, and other tools (e.g. the
/// ChatGPT desktop app) may already own the slot — so the shim can act as a
/// multiplexer: `<shim> codex --chain <prog> <args...>` forwards the payload
/// to the previous notify program untouched, then rings PingMyBell. The
/// chained program runs FIRST and unconditionally: breaking someone else's
/// notifier would violate the fail-open spirit.
fn run_codex(args: &[String]) {
    let (chain, payload) = split_codex_args(args);

    let raw = match payload {
        Some(p) => p.to_string(),
        None => {
            let mut input = String::new();
            if std::io::stdin()
                .take(STDIN_CAP_BYTES)
                .read_to_string(&mut input)
                .is_err()
            {
                return;
            }
            input
        }
    };

    if let Some(chain) = chain {
        if let Some((program, chain_args)) = chain.split_first() {
            // Fire and forget; the child outlives us.
            let _ = std::process::Command::new(program)
                .args(chain_args)
                .arg(&raw)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    let Ok(notification) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let Some(body) = map_codex_notify(&notification) else {
        return;
    };
    post_event(&body.to_string(), "/v1/event", IO_TIMEOUT);
}

/// Split codex argv into (chained notify program+args, JSON payload). Codex
/// appends the payload as the final argument; everything after `--chain` and
/// before that payload is the wrapped program's own command line.
fn split_codex_args(args: &[String]) -> (Option<&[String]>, Option<&str>) {
    let looks_like_json = |s: &String| {
        let t = s.trim_start();
        t.starts_with('{') || t.starts_with('[')
    };
    if args.first().map(String::as_str) == Some("--chain") {
        let rest = &args[1..];
        match rest.last() {
            Some(last) if looks_like_json(last) => {
                (Some(&rest[..rest.len() - 1]), Some(last.as_str()))
            }
            // No trailing JSON (delivery changed to stdin?): whole rest is
            // the chain, payload comes from stdin.
            _ => (Some(rest), None),
        }
    } else {
        (None, args.first().map(String::as_str))
    }
}

/// Map a Codex agent-turn-complete notification to a normalized event.
/// Tolerates kebab-case and snake_case keys across Codex versions.
fn map_codex_notify(notification: &Value) -> Option<Value> {
    // Accept the completion event across naming eras: agent-turn-complete,
    // turn-ended, agent_turn_completed, ...
    let ty = notification["type"]
        .as_str()
        .unwrap_or_default()
        .replace('_', "-");
    if !(ty.contains("turn") && (ty.contains("complete") || ty.contains("ended"))) {
        return None; // other notification types are not events we track
    }
    let field = |kebab: &str, snake: &str| {
        notification[kebab]
            .as_str()
            .or_else(|| notification[snake].as_str())
            .filter(|s| !s.is_empty())
    };
    let cwd = field("cwd", "cwd").unwrap_or_default();
    let session_id = codex_session_id(cwd);

    Some(json!({
        "agent": "codex",
        "event": "turn_complete",
        "session_id": session_id,
        "cwd": cwd,
        "summary": field("last-assistant-message", "last_assistant_message"),
        "transcript_path": null,
        "tool": null,
        // Captured here (Codex has no session-start): parent pid is the
        // codex process, which lets jump-to-session find the host app.
        "terminal": terminal_info(),
    }))
}

/// Codex session identity, keyed by working directory — shared by the notify
/// path and the question hook so both land on the SAME board session.
///
/// Observed in practice (ChatGPT desktop app, codex 2026-07): the notify
/// payload's ids rotate PER TURN, so using them minted a new "session" for
/// every step. The hook envelope does carry a `session_id` that is stable
/// within a turn, but it is NOT the same identifier the notify payload
/// reports — keying questions by it would split one Codex session into two
/// board rows, so a question and that session's turn-complete callouts would
/// never share a card. A directory IS the session for board purposes, and it
/// cannot drift, so both paths hash the cwd. (Cost: two Codex sessions in one
/// directory share a row — the same trade the notify path already makes.)
fn codex_session_id(cwd: &str) -> String {
    format!(
        "codex-{}",
        stable_id(if cwd.is_empty() { "global" } else { cwd })
    )
}

/// FNV-1a: tiny, dependency-free, stable across runs (not security-relevant
/// — just a compact session key).
fn stable_id(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Blocking approval flow (FR-6): POST to /v1/approval and wait for the
/// user's decision. On a decision, print the PreToolUse output JSON to
/// stdout — the ONLY case where the shim ever writes to stdout. On 204,
/// timeout, or any error: print nothing, exit 0, and Claude Code falls
/// through to its own terminal prompt (AC-6.3).
///
/// Gating is OPT-IN (`gate_tool_calls` in ~/.pingmybell/config.json): most
/// users run in auto modes where tool calls must flow with zero added
/// latency; the ask-moments they care about surface via Notification
/// events instead.
fn run_pretool(hook: &Value) {
    // Questions take their own path BEFORE any gating check: `gate_tool_calls`
    // exists to keep routine tool calls flowing with zero latency, but an
    // AskUserQuestion is by definition a moment where the agent is already
    // blocked on the user — there is no latency to protect, so PingMyBell
    // always offers to answer it. The permission-mode early exits are skipped
    // for the same reason (they are about auto-approving actions, and a
    // question is not an action).
    if is_question(hook) {
        run_question(hook);
        return;
    }
    if !should_gate(hook) {
        return;
    }
    let Some(session_id) = hook["session_id"].as_str().filter(|s| !s.is_empty()) else {
        return;
    };
    let tool_name = hook["tool_name"].as_str().unwrap_or_default();
    if tool_name.is_empty() {
        return;
    }
    let body = json!({
        "agent": "claude-code",
        "event": "permission_request",
        "session_id": session_id,
        "cwd": hook["cwd"].as_str().unwrap_or_default(),
        "summary": null,
        "transcript_path": hook["transcript_path"].as_str(),
        "tool": { "name": tool_name, "input": hook["tool_input"] },
        "terminal": null,
    });

    // The server parks this request for up to 110 s; stay comfortably inside
    // the 120 s hook timeout.
    let Some(response) = post_event(&body.to_string(), "/v1/approval", APPROVAL_READ_TIMEOUT)
    else {
        return;
    };
    let Some(decision) = parse_decision(&response) else {
        return;
    };

    let output = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": format!("PingMyBell: {decision} from overlay"),
        }
    });
    println!("{output}");
}

/// Whether this PreToolUse should park for an overlay decision.
fn should_gate(hook: &Value) -> bool {
    should_gate_with(gating_enabled(), hook)
}

fn should_gate_with(enabled: bool, hook: &Value) -> bool {
    if !enabled {
        return false;
    }
    match hook["permission_mode"].as_str().unwrap_or_default() {
        // Everything auto-runs in bypass mode; a card is pure friction.
        "bypassPermissions" => false,
        // acceptEdits auto-approves file edits — don't gate what the mode
        // already waves through.
        "acceptEdits" => !matches!(
            hook["tool_name"].as_str().unwrap_or_default(),
            "Write" | "Edit" | "MultiEdit"
        ),
        _ => true,
    }
}

/// Opt-in flag written by the app (tray toggle). Missing/unreadable/absent
/// key → false: fail open means fail FAST, never fail into a 110 s park.
fn gating_enabled() -> bool {
    flag("gate_tool_calls", false)
}

/// When to intercept a Codex `PermissionRequest`. A SEPARATE setting from
/// Claude tool gating, and deliberately three-state rather than a bool.
///
/// The two gates look alike but cost completely different things. Gating
/// Claude's PreToolUse inserts a wait into calls that were about to run
/// unattended. A Codex `PermissionRequest` fires only where Codex has ALREADY
/// stopped and is waiting for a human, so there is no latency to protect —
/// but taking that ask OUT of the agent (where the surrounding context is) and
/// putting it on a card that must be cleared is still a downgrade for a user
/// who told Codex to bother them only about genuinely unsafe things. So the
/// default is neither on nor off: it MIRRORS whatever the user already told
/// Codex (§5.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexGate {
    /// Intercept only when Codex itself is set to ask the user about
    /// everything ("Ask for approval"). The DEFAULT.
    Auto,
    /// Always intercept (the legacy `gate_codex_approvals: true`).
    Always,
    /// Never intercept (the legacy `gate_codex_approvals: false`).
    Never,
}

/// Read the gate. Missing/unreadable config → `Never`: fail open means fail
/// FAST — with no app there is nobody to answer, so never park.
fn codex_gate() -> CodexGate {
    let Some(home) = home_dir() else {
        return CodexGate::Never;
    };
    let Ok(raw) = std::fs::read_to_string(home.join(".pingmybell").join("config.json")) else {
        return CodexGate::Never;
    };
    let Ok(config) = serde_json::from_str::<Value>(&raw) else {
        return CodexGate::Never;
    };
    codex_gate_from(&config)
}

/// Backwards compatible on purpose: this key shipped as a bool, so existing
/// configs keep working — `true` is `always`, `false` is `never`. Absent (or
/// anything unrecognized) is the default, `auto`. `src-tauri/src/config.rs`
/// carries the same mapping and the same table of cases; change both together.
fn codex_gate_from(config: &Value) -> CodexGate {
    match &config["gate_codex_approvals"] {
        Value::Bool(true) => CodexGate::Always,
        Value::Bool(false) => CodexGate::Never,
        Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "always" => CodexGate::Always,
            "never" => CodexGate::Never,
            _ => CodexGate::Auto,
        },
        _ => CodexGate::Auto,
    }
}

fn flag(key: &str, default_when_present: bool) -> bool {
    let Some(home) = home_dir() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(home.join(".pingmybell").join("config.json")) else {
        return false;
    };
    let Ok(config) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    flag_from(&config, key, default_when_present)
}

fn flag_from(config: &Value, key: &str, default_when_present: bool) -> bool {
    config[key].as_bool().unwrap_or(default_when_present)
}

// ─── Mirroring the user's Codex approval setting (CodexGate::Auto) ──────────
//
// Established empirically against codex-cli 0.146.0-alpha.9.2 in a throwaway
// CODEX_HOME (deleted afterwards) and cross-read against the binary's own
// `hook_permission_mode`:
//
//   approval_policy | PermissionRequest fires? | payload permission_mode
//   ----------------|--------------------------|------------------------
//   untrusted       | yes                      | "default"
//   on-request      | yes                      | "default"
//   on-failure      | yes (alias of on-request)| "default"
//   never           | NO — never fires         | "bypassPermissions"
//
// So `permission_mode` CANNOT tell "ask me about everything" apart from
// "bother me only when it's unsafe" — it collapses every asking policy to
// "default". Nor does the payload carry the axis that actually distinguishes
// the ChatGPT app's two settings: that is `approvals_reviewer`
// ("Ask for approval" = `user`, "Approve for me" = `auto_review`), and the
// PermissionRequest payload struct has no such field.
//
// What the payload DOES carry is `transcript_path`, the session rollout, whose
// `turn_context` records hold BOTH `approval_policy` and `approvals_reviewer`
// verbatim — and are already on disk when the hook fires (verified: a hook
// that read the file at hook time saw the right values for every run).
// So `auto` reads the rollout.

/// Marker used to find candidate rollout lines without parsing every one.
const TURN_CONTEXT_MARKER: &str = "\"turn_context\"";

/// How much of the rollout tail to scan. `turn_context` is written once per
/// user turn, so the newest one is at the end; the largest rollout on this
/// machine is ~6 MB across dozens of turns. If a single turn somehow buries it
/// deeper than this we find nothing and fail open (do not park), which is the
/// same stance every other unknown takes here.
const ROLLOUT_TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// Codex approval policies under which Codex stops and asks a human at all.
/// `never` is absent deliberately — the hook does not fire in that mode, and
/// an unrecognized value (a future policy, `granular`, drift) must fail open.
const CODEX_ASKING_POLICIES: [&str; 2] = ["untrusted", "on-request"];

/// `approvals_reviewer` value meaning the human reviews escalations
/// themselves. The ChatGPT app labels this "Ask for approval"; its sibling
/// `auto_review` (alias `guardian_subagent`) is "Approve for me".
const CODEX_REVIEWER_USER: &str = "user";

/// Does the payload's session say the user asked to be consulted about
/// everything? False for any doubt whatsoever — unreadable rollout, missing
/// fields, a policy or reviewer we have not verified.
fn codex_asks_user_about_everything(hook: &Value) -> bool {
    let Some(path) = hook["transcript_path"]
        .as_str()
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    match last_turn_context(std::path::Path::new(path)) {
        Some(ctx) => codex_ask_everything_from(&ctx),
        None => false,
    }
}

/// The rule itself, over a rollout `turn_context` payload.
fn codex_ask_everything_from(ctx: &Value) -> bool {
    let policy = ctx["approval_policy"].as_str().unwrap_or_default();
    if !CODEX_ASKING_POLICIES.contains(&policy) {
        return false;
    }
    match ctx.get("approvals_reviewer") {
        // The axis the ChatGPT app's two settings actually move.
        Some(Value::String(reviewer)) => reviewer == CODEX_REVIEWER_USER,
        // A Codex old enough to predate the reviewer axis. `untrusted` is by
        // itself the "ask me about everything" policy (only known-safe
        // read-only commands are auto-approved), so it still answers the
        // question; `on-request` does not, and stays off.
        None | Some(Value::Null) => policy == "untrusted",
        // Present but not a string we can read: that is drift, not consent.
        // Falling into the legacy branch here would let a future structured
        // reviewer field turn "Approve for me" back into an interception --
        // the exact regression this rule exists to prevent.
        Some(_) => false,
    }
}

/// Newest `turn_context` payload in a rollout, or None. Bounded tail read:
/// this runs on the path where `auto` decides NOT to park, so it must not cost
/// more than the decision is worth.
fn last_turn_context(path: &std::path::Path) -> Option<Value> {
    use std::io::Seek;

    // Stat BEFORE opening: a regular file is the only thing safe to open here.
    // Opening a FIFO blocks in `open()` itself until a writer appears, and a
    // hung hook is strictly worse than not parking. Everything on this path is
    // bounded or it does not run.
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    let mut file = std::fs::File::open(path).ok()?;
    let truncated = len > ROLLOUT_TAIL_BYTES;
    if truncated {
        file.seek(std::io::SeekFrom::Start(len - ROLLOUT_TAIL_BYTES))
            .ok()?;
    }
    let mut bytes = Vec::new();
    file.take(ROLLOUT_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    // Lossy: a rollout is UTF-8, but a seek landing mid-character must not
    // turn into a hard error on a path whose whole job is to be cheap.
    let text = String::from_utf8_lossy(&bytes);
    let text = if truncated {
        // The first line is a fragment of whatever record we cut into.
        &text[text.find('\n')? + 1..]
    } else {
        &text
    };
    last_turn_context_in(text)
}

fn last_turn_context_in(text: &str) -> Option<Value> {
    let mut end = text.len();
    while let Some(hit) = text[..end].rfind(TURN_CONTEXT_MARKER) {
        let start = text[..hit].rfind('\n').map_or(0, |nl| nl + 1);
        let stop = text[hit..]
            .find('\n')
            .map_or(text.len(), |offset| hit + offset);
        if let Some(ctx) = turn_context_payload(&text[start..stop]) {
            return Some(ctx);
        }
        end = hit;
    }
    None
}

/// A rollout line, if it is a `turn_context` record with an object payload.
fn turn_context_payload(line: &str) -> Option<Value> {
    let record: Value = serde_json::from_str(line).ok()?;
    if record["type"].as_str() != Some("turn_context") {
        return None;
    }
    let payload = record.get("payload")?;
    payload.is_object().then(|| payload.clone())
}

// ─── AskUserQuestion ────────────────────────────────────────────────────────
//
// Claude Code fires PreToolUse for `AskUserQuestion` before rendering its
// selector. A PreToolUse hook can answer the question by DENYING the call
// with a reason carrying the user's choice (verified against claude 2.1.198:
// the selector never renders, the reason reaches the model as the tool
// result, and it proceeds on that answer). Every failure path prints nothing
// and exits 0, so the TUI selector appears exactly as if PingMyBell were not
// installed (AC-2.4 / AC-6.3).

/// One question of an AskUserQuestion call — only what the reason encoder
/// needs (the full payload, options included, goes to the app verbatim).
#[derive(Debug, Clone, PartialEq)]
struct QuestionSpec {
    header: String,
    question: String,
}

/// The user's answer to one question, already flattened to display values
/// (chosen labels first, then any free text).
#[derive(Debug, Clone, PartialEq)]
struct AnswerSpec {
    index: usize,
    values: Vec<String>,
}

/// Guard rails on what we will echo back into the agent's context. The label
/// and free-text caps mirror the app's (`broker.rs`) so an answer the UI
/// accepted is never silently truncated on its way to the model.
const MAX_QUESTIONS: usize = 8;
const MAX_OPTION_LABEL_CHARS: usize = 500;
const MAX_FREE_TEXT_CHARS: usize = 2000;
const MAX_REASON_CHARS: usize = 8000;
/// How much of a question's header/text is used to name it when several
/// questions were asked at once.
const MAX_QUESTION_LABEL_CHARS: usize = 80;

/// Route a PreToolUse payload to the question path. Deliberately independent
/// of `gate_tool_calls` and `permission_mode`.
fn is_question(hook: &Value) -> bool {
    hook["tool_name"].as_str() == Some(ASK_USER_QUESTION)
}

fn run_question(hook: &Value) {
    let Some(session_id) = hook["session_id"].as_str().filter(|s| !s.is_empty()) else {
        return;
    };
    let tool_input = &hook["tool_input"];
    // Unusable/drifted payload → fail open immediately rather than park.
    let Some(questions) = parse_questions(tool_input) else {
        return;
    };

    let body = json!({
        "agent": "claude-code",
        "event": "needs_attention",
        "session_id": session_id,
        "cwd": hook["cwd"].as_str().unwrap_or_default(),
        "summary": questions[0].question,
        "transcript_path": hook["transcript_path"].as_str(),
        "tool": null,
        // Captured here too: a session started inside a host app (no tty) may
        // never have sent a session_start, and jump-to-session needs a pid.
        "terminal": terminal_info(),
        "tool_use_id": hook["tool_use_id"].as_str(),
        // Verbatim, so the overlay can offer exactly the TUI's choices.
        "questions": tool_input["questions"],
    });

    let Some(response) = post_event(&body.to_string(), "/v1/question", QUESTION_READ_TIMEOUT)
    else {
        return;
    };
    if let Some(output) = question_output(&questions, &response) {
        println!("{output}");
    }
}

// ─── Codex request_user_input ───────────────────────────────────────────────
//
// Codex (codex-cli 0.146.0-alpha.9.2) grew Claude-shaped hooks. A `PreToolUse`
// hook on `request_user_input` receives the same envelope and answers through
// the same channel: `permissionDecision: "deny"` with the user's choice in
// `permissionDecisionReason`, which Codex hands to the model as the tool
// result (wrapped in "Tool call blocked by PreToolUse hook: {reason}").
// Verified end to end: the model treats it as the answer and does not re-ask.
//
// DESIGN: everything Codex-specific is normalized HERE, at the shim boundary,
// into the exact `/v1/question` body the Claude path already sends — so the
// app, the broker, the overlay card and the reply window are agent-agnostic
// and needed no changes at all.
//
// Wire differences from Claude's AskUserQuestion, all absorbed by the mapping:
//   * each question carries a REQUIRED snake_case `id`, which we DROP: the
//     deny channel carries prose, not an id-keyed map, so nothing downstream
//     could use it (`encode_reason` names questions by header/text);
//   * there is no `multiSelect` on the wire — Codex questions are
//     single-select, and Codex renders its own free-form "Other" affordance —
//     so we pin `multiSelect: false`;
//   * `options[].description` is required; Codex caps calls at 3 questions
//     with 2–3 options each (our own caps are looser and stay the guard).
// Envelope extras (`turn_id`, `model`, `agent_id`/`agent_type`, different
// `permission_mode` values) are simply not read.

/// Codex lifecycle events, delivered as Claude-shaped hook payloads on stdin.
///
/// These exist because the notify slot is single-occupancy and CONTESTED —
/// the Codex Computer Use app evicted us from it on 2026-08-04 — and because
/// notify never had a turn START, so a working Codex session wore its last
/// turn's "done". Hooks compose as arrays; nothing can evict these.
fn run_codex_lifecycle(subcommand: &str) {
    let mut input = String::new();
    if std::io::stdin()
        .take(STDIN_CAP_BYTES)
        .read_to_string(&mut input)
        .is_err()
    {
        return;
    }
    let Ok(hook) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    if let Some(event) = map_codex_lifecycle(subcommand, &hook) {
        post_event(&event.to_string(), "/v1/event", IO_TIMEOUT);
    }
}

/// Field names verified against payloads CAPTURED from the installed build
/// (ChatGPT.app, framework 150.0.7871.182) on 2026-08-04, not against the
/// strings in its binary: `hook_event_name`, `session_id`, `cwd`,
/// `transcript_path`, plus `prompt` on UserPromptSubmit and `turn_id` from
/// the second event on.
///
/// Identity stays `codex_session_id(cwd)` — the same hash the question and
/// approval paths use, so every channel lands on one board row. The envelope
/// `session_id` IS a UUID that our capture shows stable across SessionStart
/// and UserPromptSubmit, but that was a single turn, and the 2026-07
/// observation on this file records it as only turn-stable back then.
/// Migrating identity on one turn of evidence would split sessions if that
/// observation still holds; two captured turns settle it (§11.1 follow-up).
fn map_codex_lifecycle(subcommand: &str, hook: &Value) -> Option<Value> {
    let cwd = hook["cwd"].as_str().unwrap_or_default();
    // No cwd means no identity on any codex channel: fail open.
    if cwd.is_empty() {
        return None;
    }
    let (event, summary, terminal) = match subcommand {
        "codex-session-start" => ("session_start", None, Some(terminal_info())),
        // The payload carries the user's raw prompt text. It stays here: §9
        // invariant 4 keeps it out of the registry and the logs, exactly as
        // the Claude prompt-submit path does.
        "codex-prompt-submit" => ("turn_start", None, None),
        "codex-stop" => (
            "turn_complete",
            hook["last_assistant_message"].as_str().map(str::to_string),
            None,
        ),
        "codex-session-end" => ("session_end", None, None),
        _ => return None,
    };
    Some(json!({
        "agent": "codex",
        "event": event,
        "session_id": codex_session_id(cwd),
        "cwd": cwd,
        "summary": summary,
        "transcript_path": hook["transcript_path"].as_str(),
        "tool": null,
        "terminal": terminal,
    }))
}

fn run_codex_ask() {
    let mut input = String::new();
    if std::io::stdin()
        .take(STDIN_CAP_BYTES)
        .read_to_string(&mut input)
        .is_err()
    {
        return;
    }
    let Ok(hook) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    run_codex_question(&hook);
}

fn run_codex_question(hook: &Value) {
    // Matcher-independent: even if the hook were installed with a wider
    // matcher, only the question tool takes this path.
    if hook["tool_name"].as_str() != Some(REQUEST_USER_INPUT) {
        return;
    }
    // Unusable/drifted payload → fail open immediately rather than park.
    let Some((specs, questions)) = map_codex_questions(&hook["tool_input"]) else {
        return;
    };
    let cwd = hook["cwd"].as_str().unwrap_or_default();

    let body = json!({
        "agent": "codex",
        "event": "needs_attention",
        "session_id": codex_session_id(cwd),
        "cwd": cwd,
        "summary": specs[0].question,
        "transcript_path": hook["transcript_path"].as_str(),
        "tool": null,
        // Codex has no session-start hook, so this is our only chance to
        // record a pid for jump-to-session — same reason as the notify path.
        //
        // Caveat, deliberate: Codex runs command hooks through `$SHELL -lc`,
        // so our parent is a shell that exits when the hook returns. The pid
        // is live for the whole park (exactly when the card is clickable, so
        // jumping FROM the card works), but the registry keeps the last
        // terminal it saw — so between answering and this session's next
        // turn-complete a board jump may find nothing. `focus::jump` treats a
        // vanished pid as a no-op, and the next notify event restores the
        // longer-lived codex pid, so this self-heals.
        "terminal": terminal_info(),
        "tool_use_id": hook["tool_use_id"].as_str(),
        "questions": questions,
    });

    let Some(response) = post_event(&body.to_string(), "/v1/question", QUESTION_READ_TIMEOUT)
    else {
        return;
    };
    // Byte-identical to the Claude path: the phrasing is what demonstrably
    // overcomes Codex's "Tool call blocked by…" wrapper.
    if let Some(output) = question_output(&specs, &response) {
        println!("{output}");
    }
}

/// Map Codex's `tool_input.questions[]` into (reason specs, the normalized
/// `questions` array the app already accepts). None whenever the payload is
/// not the shape we verified — a schema drift must fail open, never park.
fn map_codex_questions(tool_input: &Value) -> Option<(Vec<QuestionSpec>, Value)> {
    let raw = tool_input["questions"].as_array()?;
    if raw.is_empty() || raw.len() > MAX_QUESTIONS {
        return None;
    }
    let mut specs = Vec::with_capacity(raw.len());
    let mut normalized = Vec::with_capacity(raw.len());
    for q in raw {
        let question = sanitize(q["question"].as_str().unwrap_or_default());
        if question.is_empty() {
            return None;
        }
        let header = sanitize(q["header"].as_str().unwrap_or_default());

        let mut options = Vec::new();
        for o in q["options"].as_array().into_iter().flatten() {
            let label = sanitize(o["label"].as_str().unwrap_or_default());
            if label.is_empty() {
                continue; // an unclickable button is worse than one fewer
            }
            options.push(json!({
                "label": label,
                "description": sanitize(o["description"].as_str().unwrap_or_default()),
            }));
        }
        // The app refuses a card nobody can click (400); bail here so we never
        // pay a round trip to learn that.
        if options.is_empty() {
            return None;
        }

        normalized.push(json!({
            "question": question.clone(),
            "header": header.clone(),
            "options": options,
            // Codex sends no multiSelect: its questions are single-select.
            "multiSelect": false,
        }));
        specs.push(QuestionSpec { header, question });
    }
    Some((specs, Value::Array(normalized)))
}

// ─── Codex exec / file-change approvals ─────────────────────────────────────
//
// Verified empirically against codex-cli 0.146.0-alpha.9.2 (bundled in
// ChatGPT.app) on 2026-07-31, in a throwaway CODEX_HOME, and cross-read
// against the binary's own hook sources:
//
//   * approvals do NOT arrive on `PreToolUse`. They arrive on a separate
//     `PermissionRequest` event that fires ONLY where Codex was already about
//     to block and ask a human, and whose payload has no `tool_use_id`;
//   * `tool_name` is `Bash` for every exec flavour (shell / unified_exec) and
//     `apply_patch` for file edits — Codex deliberately uses Claude Code's
//     names, so `tool_summary` in the core needed no `Bash` special-casing;
//   * `tool_input` is `{"command": …}` in BOTH cases: a shell command line for
//     `Bash`, the raw `*** Begin Patch …` text for `apply_patch`;
//   * the decision shape is NOT the PreToolUse one. It is
//     `hookSpecificOutput.decision.{behavior, message}`, and **`allow` needs
//     no `updatedInput`** — in fact `updatedInput`, `updatedPermissions` and
//     `interrupt` all make Codex fail the hook closed. Confirmed by running
//     it: a `behavior: "allow"` reply turned "command execution approval is
//     not supported in exec mode" into the command actually executing.
//
// So Approve genuinely approves here, unlike the question path where only
// `deny` is expressible. Deny's `message` is NOT wrapped by Codex; it reaches
// the model as `Rejected("<message>")`.

/// Codex approval hook (`PermissionRequest`). Gated behind
/// `gate_codex_approvals` — this channel can GRANT permission without the user
/// ever seeing Codex's own prompt, and it moves the ask away from the context
/// that explains it, so it needs more than a default "on".
fn run_codex_approve() {
    // Fast path FIRST, before touching stdin: `never` must cost about as
    // little as a process can. Codex explicitly tolerates the resulting broken
    // pipe (its hook runner ignores `ErrorKind::BrokenPipe` when writing hook
    // stdin), so exiting here is safe and simply leaves the approval to Codex.
    // `auto` cannot take this exit — its decision needs the payload — so it
    // pays for stdin plus one bounded rollout read, and nothing else.
    let gate = codex_gate();
    if gate == CodexGate::Never {
        return;
    }
    let mut input = String::new();
    if std::io::stdin()
        .take(STDIN_CAP_BYTES)
        .read_to_string(&mut input)
        .is_err()
    {
        return;
    }
    let Ok(hook) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    // The gate is threaded through rather than re-read or assumed: the early
    // return above is an optimisation, and `codex_approval_body` is what
    // actually enforces it.
    run_codex_approval(gate, &hook);
}

fn run_codex_approval(gate: CodexGate, hook: &Value) {
    let Some(body) = codex_approval_body(gate, hook) else {
        return;
    };
    let Some(response) = post_event(&body.to_string(), "/v1/approval", APPROVAL_READ_TIMEOUT)
    else {
        return;
    };
    let Some(decision) = parse_decision(&response) else {
        return;
    };
    if let Some(output) = codex_approval_output(decision) {
        println!("{output}");
    }
}

/// Map a `PermissionRequest` payload to the `/v1/approval` body. None whenever
/// the payload is not the shape we verified — a drift must fail open
/// immediately rather than park a card on something we cannot describe.
fn codex_approval_body(gate: CodexGate, hook: &Value) -> Option<Value> {
    // Event-guarded, not matcher-guarded: a `PreToolUse` payload reaching this
    // subcommand would produce output Codex fails closed on, so only the event
    // we verified takes this path.
    if hook["hook_event_name"].as_str() != Some(PERMISSION_REQUEST) {
        return None;
    }
    // `never` is enforced HERE, not only by the fast-path return in the caller
    // — this path can GRANT permission, so the opt-out must not rest on one
    // early return. The permission_mode arm is belt and braces: Codex only
    // reports `bypassPermissions` when its approval policy is `never`, and in
    // that mode this hook never fires at all.
    if !should_gate_with(gate != CodexGate::Never, hook) {
        return None;
    }
    // Allowlisted, like the question path pins `request_user_input`. Our
    // installed matcher only selects these two, but a widened matcher (or a
    // future Codex routing another tool through `PermissionRequest`) must not
    // be able to make us approve something we cannot even describe on the
    // card — `tool_summary` would fall through to raw JSON. Falling through to
    // Codex's own prompt is the right answer for anything unrecognized.
    let tool_name = hook["tool_name"]
        .as_str()
        .filter(|name| matches!(*name, CODEX_EXEC_TOOL | CODEX_PATCH_TOOL))?;
    // No usable input → fail open rather than pin a card nobody can read.
    if !hook["tool_input"].is_object() {
        return None;
    }
    // Deliberately LAST: the only check here that touches the filesystem, so
    // every cheap reason to bail has already had its chance. `auto` mirrors
    // the user's own Codex setting — intercept only where they told Codex to
    // ask them about everything; where they told it to handle the safe stuff,
    // PingMyBell must not override that and drag the ask onto a card.
    if gate == CodexGate::Auto && !codex_asks_user_about_everything(hook) {
        return None;
    }
    let cwd = hook["cwd"].as_str().unwrap_or_default();

    Some(json!({
        "agent": "codex",
        "event": "permission_request",
        // Same cwd hash the notify and question paths use, so an approval
        // lands on the SAME board row as that project's questions and
        // turn-completes. The hook's own `session_id` is unrelated to the ids
        // the notify payload rotates through.
        "session_id": codex_session_id(cwd),
        "cwd": cwd,
        "summary": null,
        "transcript_path": hook["transcript_path"].as_str(),
        // Verbatim: `tool_summary` in the core reads `command` for both
        // `Bash` and `apply_patch`, and Codex's names are Claude's.
        "tool": { "name": tool_name, "input": hook["tool_input"] },
        // Codex has no session-start hook; this is our only chance to record a
        // pid for jump-to-session (same caveat as the question path: our
        // parent is the `$SHELL -lc` wrapper, alive for exactly as long as the
        // card is up).
        "terminal": terminal_info(),
    }))
}

/// Encode an overlay decision as Codex's `PermissionRequest` hook output.
///
/// `ask` ("Terminal" on the card) deliberately prints NOTHING: Codex then runs
/// its own approval flow, which is the whole point of that button and is the
/// same shape as every other fail-open path here.
fn codex_approval_output(decision: &str) -> Option<String> {
    let decision = match decision {
        // No `updatedInput`: Codex fails the hook CLOSED if that key is
        // present, and needs nothing beyond the behavior to let it run.
        "allow" => json!({ "behavior": "allow" }),
        "deny" => json!({ "behavior": "deny", "message": CODEX_DENY_MESSAGE }),
        _ => return None,
    };
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": PERMISSION_REQUEST,
                "decision": decision,
            }
        })
        .to_string(),
    )
}

/// Build the PreToolUse output for a raw HTTP response, or None when there is
/// no usable answer (204, timeout, garbage, unknown shape) — the caller then
/// prints NOTHING.
fn question_output(questions: &[QuestionSpec], response: &str) -> Option<String> {
    let answers = parse_answers(response)?;
    let reason = encode_reason(questions, &answers)?;
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
        .to_string(),
    )
}

/// Read the questions out of `tool_input`. None whenever the payload is not
/// the shape we verified — a schema drift must fail open, never park.
fn parse_questions(tool_input: &Value) -> Option<Vec<QuestionSpec>> {
    let raw = tool_input["questions"].as_array()?;
    if raw.is_empty() || raw.len() > MAX_QUESTIONS {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len());
    for q in raw {
        let question = sanitize(q["question"].as_str().unwrap_or_default());
        if question.is_empty() {
            return None;
        }
        out.push(QuestionSpec {
            header: sanitize(q["header"].as_str().unwrap_or_default()),
            question,
        });
    }
    Some(out)
}

/// Extract answers from a raw HTTP response. None for 204/non-200/anything
/// that is not the documented `{"answers":[…]}` body with usable content.
fn parse_answers(raw: &str) -> Option<Vec<AnswerSpec>> {
    let status_line = raw.lines().next()?;
    if !status_line.contains("200") {
        return None;
    }
    let body = raw.split("\r\n\r\n").nth(1)?;
    let parsed: Value = serde_json::from_str(body.trim()).ok()?;
    let answers = parsed["answers"].as_array()?;

    let mut out = Vec::new();
    for a in answers {
        let Some(index) = a["question_index"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
        else {
            continue;
        };
        let mut values = Vec::new();
        if let Some(labels) = a["labels"].as_array() {
            for label in labels {
                let value =
                    sanitize_value(label.as_str().unwrap_or_default(), MAX_OPTION_LABEL_CHARS);
                if !value.is_empty() {
                    values.push(value);
                }
            }
        }
        // Free text gets the roomier cap: it is the user's own sentence, and
        // truncating it would change the instruction the agent acts on.
        let free_text = sanitize_value(
            a["free_text"].as_str().unwrap_or_default(),
            MAX_FREE_TEXT_CHARS,
        );
        if !free_text.is_empty() {
            values.push(free_text);
        }
        if !values.is_empty() {
            out.push(AnswerSpec { index, values });
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Turn the user's answer into the `permissionDecisionReason` Claude reads.
///
/// The single-question phrasing is the empirically verified string — it
/// demonstrably makes Claude accept the answer and move on without re-asking,
/// so keep it stable:
///
/// ```text
/// The user answered via PingMyBell: "Option B (thorough)". Treat this as
/// their answer to your question; do not ask again.
/// ```
///
/// Multi-select adds the extra labels to the same quoted list; a call with
/// several questions prefixes each answer with that question's header (or the
/// question text when it has none) so the mapping is unambiguous. Answers for
/// questions that do not exist are dropped; if nothing usable remains the
/// caller prints nothing and the TUI selector takes over.
fn encode_reason(questions: &[QuestionSpec], answers: &[AnswerSpec]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for answer in answers {
        let Some(spec) = questions.get(answer.index) else {
            continue;
        };
        let values = answer
            .values
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(", ");
        if questions.len() == 1 {
            parts.push(values);
        } else {
            parts.push(format!("{} — {}", label_for(spec), values));
        }
    }
    if parts.is_empty() {
        return None;
    }

    // Budget the reason by whole parts: slicing the joined string could cut
    // inside a "quoted value" and hand the model an unbalanced quote.
    let mut used = 0usize;
    let mut kept: Vec<String> = Vec::new();
    for part in parts {
        let cost = part.chars().count() + 2; // "; " separator
        if !kept.is_empty() && used + cost > MAX_REASON_CHARS {
            break;
        }
        used += cost;
        kept.push(part);
    }
    // Defensive: a single oversized part is trimmed at a value boundary and
    // re-closed so the quoting stays balanced.
    if kept.len() == 1 && kept[0].chars().count() > MAX_REASON_CHARS {
        let trimmed: String = kept[0].chars().take(MAX_REASON_CHARS).collect();
        kept[0] = format!("{}…\"", trimmed.trim_end_matches('"'));
    }
    let (pronoun, noun) = if kept.len() == 1 {
        ("this", "their answer to your question")
    } else {
        ("these", "their answers to your questions")
    };
    let body = kept.join("; ");
    Some(format!(
        "The user answered via PingMyBell: {body}. \
         Treat {pronoun} as {noun}; do not ask again."
    ))
}

/// How one question is named when several were asked at once.
fn label_for(spec: &QuestionSpec) -> String {
    let source = if spec.header.is_empty() {
        &spec.question
    } else {
        &spec.header
    };
    let mut label: String = source.chars().take(MAX_QUESTION_LABEL_CHARS).collect();
    if label.chars().count() < source.chars().count() {
        label.push('…');
    }
    label
}

/// Flatten to a single line of prose: control characters (ESC, BEL, zero
/// width, newlines) become spaces and runs of whitespace collapse. The reason
/// is prose inside a JSON string read by the model.
fn sanitize(s: &str) -> String {
    let neutral: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    neutral.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Same, plus a length cap and quote flattening — an answer value is rendered
/// inside "quotes", and nested double quotes read badly to the model.
fn sanitize_value(s: &str, max: usize) -> String {
    let cleaned = sanitize(s).replace('"', "'");
    let mut out: String = cleaned.chars().take(max).collect();
    if out.chars().count() < cleaned.chars().count() {
        out.push('…');
    }
    out
}

/// Extract the decision from a raw HTTP response; None for 204/anything odd.
fn parse_decision(raw: &str) -> Option<&'static str> {
    let status_line = raw.lines().next()?;
    if !status_line.contains("200") {
        return None;
    }
    let body = raw.split("\r\n\r\n").nth(1)?;
    let parsed: Value = serde_json::from_str(body.trim()).ok()?;
    match parsed["decision"].as_str()? {
        "allow" => Some("allow"),
        "deny" => Some("deny"),
        "ask" => Some("ask"),
        _ => None,
    }
}

/// Map a Claude Code hook payload to a normalized event. Returns None when
/// the payload is unusable (fail open).
fn map_claude_hook(subcommand: &str, hook: &Value) -> Option<Value> {
    let session_id = hook["session_id"].as_str().filter(|s| !s.is_empty())?;
    let cwd = hook["cwd"].as_str().unwrap_or_default();
    let transcript_path = hook["transcript_path"].as_str();

    let (event, summary, terminal) = match subcommand {
        "session-start" => ("session_start", None, Some(terminal_info())),
        // A prompt was submitted, so the agent is about to work. Carries no
        // summary on purpose: the prompt is the user's own text and §9
        // invariant 4 keeps it out of the registry and the logs.
        "prompt-submit" => ("turn_start", None, None),
        // Stop carries last_assistant_message (verified against claude
        // 2.1.198); core falls back to transcript_path when absent.
        "stop" => (
            "turn_complete",
            hook["last_assistant_message"].as_str(),
            None,
        ),
        "notification" => {
            // Notifications fire for more than permission/idle prompts (e.g.
            // auth success). Only attention-worthy ones become events; when
            // the type field is absent we keep the notification (AC-2.3 over
            // false negatives).
            if !notification_needs_attention(hook) {
                return None;
            }
            ("needs_attention", hook["message"].as_str(), None)
        }
        "session-end" => ("session_end", None, None),
        _ => return None,
    };

    Some(json!({
        "agent": "claude-code",
        "event": event,
        "session_id": session_id,
        "cwd": cwd,
        "summary": summary,
        "transcript_path": transcript_path,
        "tool": null,
        "terminal": terminal,
    }))
}

fn notification_needs_attention(hook: &Value) -> bool {
    match hook["notification_type"].as_str() {
        // Unknown/absent type: err toward speaking (missing a permission
        // prompt is worse than an extra callout).
        None => true,
        Some(t) => t.contains("permission") || t.contains("idle") || t.contains("elicitation"),
    }
}

fn terminal_info() -> Value {
    #[cfg(unix)]
    let ppid = unsafe { libc::getppid() } as i64;
    #[cfg(not(unix))]
    let ppid = 0i64;

    json!({
        "pid": std::process::id(),
        "ppid_chain": [ppid],
        "tty": tty_name(),
        "tmux_pane": std::env::var("TMUX_PANE").ok(),
        "term_program": std::env::var("TERM_PROGRAM").ok(),
        "hwnd": null,
    })
}

#[cfg(unix)]
fn tty_name() -> Option<String> {
    // Hook stdin is a pipe; stderr (then stdout) may still be the terminal.
    for fd in [2, 1, 0] {
        let ptr = unsafe { libc::ttyname(fd) };
        if !ptr.is_null() {
            let name = unsafe { std::ffi::CStr::from_ptr(ptr) };
            return Some(name.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(not(unix))]
fn tty_name() -> Option<String> {
    None
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    let home = std::env::var_os("HOME");
    #[cfg(not(unix))]
    let home = std::env::var_os("USERPROFILE");
    home.map(PathBuf::from)
}

/// Discover the ingest server, POST the body to `path`, and return the raw
/// HTTP response. Every failure returns None — the caller ignores it (fail
/// open). `read_timeout` bounds how long we wait for the response: short for
/// fire-and-forget events, long for the blocking approval poll.
fn post_event(body: &str, path: &str, read_timeout: Duration) -> Option<String> {
    let (mut stream, port, token) = connect()?;
    stream.set_read_timeout(Some(read_timeout)).ok()?;
    write_request(&mut stream, &port, &token, path, body)?;
    read_response(stream, read_timeout)
}

/// Discover the ingest server and open a socket to it.
fn connect() -> Option<(TcpStream, u16, String)> {
    let dir = home_dir()?.join(".pingmybell");
    let port: u16 = std::fs::read_to_string(dir.join("port"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let token = std::fs::read_to_string(dir.join("token")).ok()?;
    let token = token.trim();

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    Some((stream, port, token.to_string()))
}

fn write_request(
    stream: &mut TcpStream,
    port: &u16,
    token: &str,
    path: &str,
    body: &str,
) -> Option<()> {
    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()
}

fn read_response(mut stream: TcpStream, read_timeout: Duration) -> Option<String> {
    // Read to connection close (the server sends Connection: close); capped
    // small — responses are a status line, headers, and a tiny JSON body.
    //
    // `read_timeout` is a PER-READ socket timeout, so a peer that dribbles
    // one byte at a time could keep us alive past the 120 s hook timeout —
    // and after an unclean shutdown the port file can point at a port some
    // other process now owns. Enforce a TOTAL deadline instead: whatever the
    // peer does, this returns within `read_timeout`.
    let deadline = std::time::Instant::now() + read_timeout;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        // A zero/sub-millisecond timeout means "block forever" to the OS —
        // stop instead of arming an infinite read.
        if remaining < Duration::from_millis(1) || stream.set_read_timeout(Some(remaining)).is_err()
        {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() >= 64 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let raw = String::from_utf8_lossy(&raw).into_owned();
    (!raw.is_empty()).then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `PostToolUse` payloads, captured from claude 2.1.198 on
    /// 2026-08-05 through a throwaway `--settings` file (§12.1 forbids
    /// building this mapper from anything else). `tool_response` is kept in
    /// the Bash fixture precisely so a test can prove it never escapes.
    fn posttool_bash() -> Value {
        json!({
            "session_id": "b1e0e0a4-0000-4000-8000-000000000001",
            "transcript_path": "/Users/x/.claude/projects/p/s.jsonl",
            "cwd": "/tmp/work",
            "permission_mode": "bypassPermissions",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "toolu_01",
            "duration_ms": 12,
            "effort": "low",
            "prompt_id": "p1",
            "tool_input": { "command": "echo hello world", "description": "Print hello world" },
            "tool_response": {
                "stdout": "hello world",
                "stderr": "",
                "interrupted": false,
                "isImage": false,
                "noOutputExpected": false
            }
        })
    }

    fn posttool_edit() -> Value {
        json!({
            "session_id": "b1e0e0a4-0000-4000-8000-000000000001",
            "cwd": "/tmp/work",
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "/tmp/work/src-tauri/src/registry.rs",
                "old_string": "alpha",
                "new_string": "gamma",
                "replace_all": false
            },
            "tool_response": { "originalFile": "alpha\nbeta\n", "structuredPatch": [] }
        })
    }

    /// The ticker narrates the tool and ONE label — never arguments, never
    /// content (§9 invariant 4).
    #[test]
    fn posttool_reduces_a_payload_to_a_tool_and_one_label() {
        let body = map_posttool(&posttool_bash()).expect("bash produces a label");
        assert_eq!(body["tool"], "Bash");
        assert_eq!(body["label"], "echo", "the first WORD, not the command line");
        assert_eq!(body["session_id"], "b1e0e0a4-0000-4000-8000-000000000001");

        let body = map_posttool(&posttool_edit()).expect("edit produces a label");
        assert_eq!(body["tool"], "Edit");
        assert_eq!(
            body["label"], "registry.rs",
            "the basename, not the tree it sits in"
        );
    }

    /// The payload carries stdout and whole file contents. None of it may
    /// reach the wire — this is the test that would fail if somebody ever
    /// forwarded `tool_input` or `tool_response` wholesale.
    #[test]
    fn no_argument_or_output_can_reach_the_wire() {
        for hook in [posttool_bash(), posttool_edit()] {
            let wire = map_posttool(&hook).unwrap().to_string();
            for forbidden in [
                "hello world",   // stdout, and the command's own arguments
                "Print hello",   // the model's description of the call
                "alpha",         // the file's previous contents
                "gamma",         // and its new ones
                "/tmp/work",     // the directory the user is working in
                "structuredPatch",
            ] {
                assert!(
                    !wire.contains(forbidden),
                    "{forbidden:?} leaked into {wire}"
                );
            }
        }
    }

    /// Every other tool shows its name and nothing else. Captured payloads
    /// put a query in `query`, a subject in `subject` and a description in
    /// `description` — none of it ours to narrate.
    #[test]
    fn tools_without_a_safe_label_show_only_their_name() {
        for (tool, input) in [
            ("ToolSearch", json!({ "query": "select:Read", "max_results": 5 })),
            ("TaskCreate", json!({ "subject": "ship the ticker", "description": "…" })),
            ("Glob", json!({ "pattern": "**/*.rs" })),
        ] {
            let hook = json!({
                "session_id": "s1",
                "tool_name": tool,
                "tool_input": input,
            });
            let body = map_posttool(&hook).unwrap();
            assert_eq!(body["tool"], tool);
            assert!(body["label"].is_null(), "{tool} must not carry a label");
        }
    }

    /// Drift and junk fail open, like every other shim path: no session, no
    /// tool, or a payload of the wrong shape produces nothing to send.
    #[test]
    fn a_payload_we_do_not_understand_sends_nothing() {
        for hook in [
            json!({}),
            json!({ "tool_name": "Bash" }),
            json!({ "session_id": "", "tool_name": "Bash" }),
            json!({ "session_id": "s1" }),
            json!({ "session_id": "s1", "tool_name": "" }),
            json!({ "session_id": "s1", "tool_name": 42 }),
        ] {
            assert!(map_posttool(&hook).is_none(), "{hook}");
        }
        // A Bash call whose input is missing or oddly shaped is still worth
        // narrating — the TOOL is the news; the label is a bonus.
        let body = map_posttool(&json!({
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_input": { "command": "" }
        }))
        .unwrap();
        assert!(body["label"].is_null());
    }

    /// Labels are bounded before they reach the wire, and a path is reduced
    /// to its last segment on either platform's separator.
    #[test]
    fn labels_are_bounded_and_platform_agnostic() {
        assert_eq!(basename("/a/b/c.rs"), "c.rs");
        assert_eq!(basename(r"C:\\Users\\x\\notes.md"), "notes.md");
        assert_eq!(basename("/a/b/"), "b", "a trailing separator is not a name");
        assert_eq!(basename("bare.txt"), "bare.txt");

        let long = "x".repeat(5_000);
        let label = activity_label("Bash", &json!({ "command": long })).unwrap();
        assert_eq!(label.chars().count(), MAX_ACTIVITY_LABEL_CHARS);
    }

    fn stop_hook() -> Value {
        json!({
            "session_id": "sess-1",
            "cwd": "/tmp/proj",
            "transcript_path": "/tmp/t.jsonl",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "All done."
        })
    }

    #[test]
    fn stop_maps_to_turn_complete_with_summary() {
        let e = map_claude_hook("stop", &stop_hook()).unwrap();
        assert_eq!(e["event"], "turn_complete");
        assert_eq!(e["agent"], "claude-code");
        assert_eq!(e["summary"], "All done.");
        assert_eq!(e["transcript_path"], "/tmp/t.jsonl");
        assert_eq!(e["session_id"], "sess-1");
    }

    #[test]
    fn session_start_captures_terminal() {
        let hook = json!({"session_id": "s", "cwd": "/tmp", "hook_event_name": "SessionStart"});
        let e = map_claude_hook("session-start", &hook).unwrap();
        assert_eq!(e["event"], "session_start");
        assert!(e["terminal"].is_object());
        assert!(e["terminal"]["pid"].is_number());
    }

    #[test]
    fn prompt_submit_maps_to_turn_start_and_carries_no_prompt_text() {
        let hook = json!({
            "session_id": "s", "cwd": "/tmp",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "delete the production database"
        });
        let e = map_claude_hook("prompt-submit", &hook).unwrap();
        assert_eq!(e["event"], "turn_start");
        assert_eq!(e["agent"], "claude-code");
        assert_eq!(e["session_id"], "s");
        // §9 invariant 4: the user's own prompt must not travel with it.
        assert!(e["summary"].is_null());
        assert!(!e.to_string().contains("production database"));
    }

    /// Fixtures below are the payloads CAPTURED from the installed Codex on
    /// 2026-08-04, fields verbatim (prompt text and paths shortened).
    #[test]
    fn codex_session_start_maps_and_captures_the_terminal() {
        let hook = json!({
            "cwd": "/Users/x/MoodScene",
            "hook_event_name": "SessionStart",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "session_id": "019fcffb-63bb-7fe3-a0c7-57d8f4075251",
            "source": "startup",
            "transcript_path": "/Users/x/.codex/sessions/rollout.jsonl"
        });
        let e = map_codex_lifecycle("codex-session-start", &hook).unwrap();
        assert_eq!(e["event"], "session_start");
        assert_eq!(e["agent"], "codex");
        // Identity is the cwd hash — the same row every other channel uses.
        assert_eq!(e["session_id"], codex_session_id("/Users/x/MoodScene"));
        assert!(e["terminal"].is_object(), "first chance to record a pid");
    }

    #[test]
    fn codex_prompt_submit_maps_to_turn_start_and_drops_the_prompt() {
        let hook = json!({
            "cwd": "/Users/x/MoodScene",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "hey whats up\n",
            "session_id": "019fcffb-63bb-7fe3-a0c7-57d8f4075251",
            "turn_id": "019fcffb-6d0b-7443-9ec5-a29cec96a073",
            "transcript_path": "/Users/x/.codex/sessions/rollout.jsonl"
        });
        let e = map_codex_lifecycle("codex-prompt-submit", &hook).unwrap();
        assert_eq!(e["event"], "turn_start");
        assert!(e["summary"].is_null());
        // §9 invariant 4: the user's own text must not travel with it.
        assert!(!e.to_string().contains("whats up"));
    }

    #[test]
    fn codex_stop_maps_to_turn_complete_with_whatever_summary_exists() {
        let with = json!({
            "cwd": "/x", "hook_event_name": "Stop",
            "last_assistant_message": "All done."
        });
        let e = map_codex_lifecycle("codex-stop", &with).unwrap();
        assert_eq!(e["event"], "turn_complete");
        assert_eq!(e["summary"], "All done.");
        // Unconfirmed on this build whether an ERRORED turn carries the
        // field; its absence must cost the sentence, not the event.
        let without = json!({"cwd": "/x", "hook_event_name": "Stop"});
        let e = map_codex_lifecycle("codex-stop", &without).unwrap();
        assert_eq!(e["event"], "turn_complete");
        assert!(e["summary"].is_null());
    }

    #[test]
    fn codex_lifecycle_without_a_cwd_fails_open() {
        // No cwd means no identity on ANY codex channel.
        for sub in [
            "codex-session-start",
            "codex-prompt-submit",
            "codex-stop",
            "codex-session-end",
        ] {
            assert!(map_codex_lifecycle(sub, &json!({"session_id": "u1"})).is_none());
        }
        assert!(
            map_codex_lifecycle("codex-session-end", &json!({"cwd": "/x"})).is_some(),
            "session_end needs only the cwd"
        );
    }

    #[test]
    fn notification_maps_message_to_summary() {
        let hook = json!({"session_id": "s", "cwd": "/tmp", "message": "Claude needs your permission to use Bash"});
        let e = map_claude_hook("notification", &hook).unwrap();
        assert_eq!(e["event"], "needs_attention");
        assert_eq!(e["summary"], "Claude needs your permission to use Bash");
    }

    #[test]
    fn missing_session_id_or_unknown_subcommand_fails_open() {
        assert!(map_claude_hook("stop", &json!({"cwd": "/tmp"})).is_none());
        assert!(map_claude_hook("mystery", &stop_hook()).is_none());
    }

    #[test]
    fn the_two_gates_are_independent() {
        // Claude gating inserts a wait into calls that were about to run
        // unattended, so it stays opt-in and boolean. The Codex gate is a
        // different question entirely (WHERE the ask belongs, not how fast),
        // so it is three-state — and moving one must never move the other.
        let codex_only = json!({"gate_tool_calls": false, "gate_codex_approvals": "always"});
        assert!(!flag_from(&codex_only, "gate_tool_calls", false));
        assert_eq!(codex_gate_from(&codex_only), CodexGate::Always);

        let claude_only = json!({"gate_tool_calls": true, "gate_codex_approvals": "never"});
        assert!(flag_from(&claude_only, "gate_tool_calls", false));
        assert_eq!(codex_gate_from(&claude_only), CodexGate::Never);
    }

    /// Mirror of `config::tests::codex_gate_parses_every_shape` in the app
    /// crate. The shim cannot link that crate, so the mapping exists twice and
    /// is pinned twice; change both together.
    #[test]
    fn codex_gate_parses_every_shape_including_the_legacy_booleans() {
        for (raw, want) in [
            (json!("auto"), CodexGate::Auto),
            (json!("always"), CodexGate::Always),
            (json!("never"), CodexGate::Never),
            // The key shipped as a bool. The user's live config holds `false`
            // TODAY and that must keep meaning "never" until they change it.
            (json!(true), CodexGate::Always),
            (json!(false), CodexGate::Never),
            (json!("Always"), CodexGate::Always),
            (json!(" never "), CodexGate::Never),
            // Absent / unrecognized → the default.
            (Value::Null, CodexGate::Auto),
            (json!("wat"), CodexGate::Auto),
            (json!(3), CodexGate::Auto),
        ] {
            let config = json!({ "gate_codex_approvals": raw });
            assert_eq!(codex_gate_from(&config), want, "{raw}");
        }
        // Key entirely absent (a config written by an older app).
        assert_eq!(codex_gate_from(&json!({})), CodexGate::Auto);
        assert_eq!(
            codex_gate_from(&json!({"gate_tool_calls": true})),
            CodexGate::Auto
        );
    }

    #[test]
    fn gating_respects_flag_and_permission_mode() {
        let bash = |mode: &str| json!({"tool_name": "Bash", "permission_mode": mode});
        let edit = |mode: &str| json!({"tool_name": "Edit", "permission_mode": mode});

        // Disabled flag short-circuits everything.
        assert!(!should_gate_with(false, &bash("default")));
        // Bypass mode never gates.
        assert!(!should_gate_with(true, &bash("bypassPermissions")));
        // acceptEdits: edits flow, bash still gates.
        assert!(!should_gate_with(true, &edit("acceptEdits")));
        assert!(should_gate_with(true, &bash("acceptEdits")));
        // Default/plan/missing mode gates when enabled.
        assert!(should_gate_with(true, &bash("default")));
        assert!(should_gate_with(true, &json!({"tool_name": "Bash"})));
    }

    #[test]
    fn parse_decision_handles_200_204_and_garbage() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{\"decision\":\"allow\"}";
        assert_eq!(parse_decision(ok), Some("allow"));
        let deny = "HTTP/1.1 200 OK\r\n\r\n{\"decision\":\"deny\"}";
        assert_eq!(parse_decision(deny), Some("deny"));
        let no_content = "HTTP/1.1 204 No Content\r\n\r\n";
        assert_eq!(parse_decision(no_content), None);
        assert_eq!(parse_decision("HTTP/1.1 200 OK\r\n\r\nnot json"), None);
        assert_eq!(
            parse_decision("HTTP/1.1 200 OK\r\n\r\n{\"decision\":\"self-destruct\"}"),
            None
        );
        assert_eq!(parse_decision(""), None);
    }

    #[test]
    fn codex_sessions_are_keyed_by_cwd_not_rotating_ids() {
        let turn = |turn_id: &str, msg: &str| {
            json!({"type": "agent-turn-complete", "thread-id": turn_id, "turn-id": turn_id,
                   "cwd": "/tmp/p", "last-assistant-message": msg})
        };
        let a = map_codex_notify(&turn("uuid-1", "Step one.")).unwrap();
        let b = map_codex_notify(&turn("uuid-2", "Step two.")).unwrap();
        assert_eq!(
            a["session_id"], b["session_id"],
            "rotating per-turn ids must not mint new sessions"
        );
        assert_eq!(a["agent"], "codex");
        assert_eq!(a["event"], "turn_complete");
        assert_eq!(a["summary"], "Step one.");
        assert!(a["terminal"].is_object());

        // Different directory → different session; empty cwd → global bucket.
        let c = map_codex_notify(
            &json!({"type": "agent_turn_complete", "thread_id": "x", "cwd": "/other"}),
        )
        .unwrap();
        assert_ne!(a["session_id"], c["session_id"]);
        let d = map_codex_notify(&json!({"type": "agent-turn-complete", "turn-id": "u3"})).unwrap();
        assert_eq!(
            d["session_id"],
            format!("codex-{}", stable_id("global")).as_str()
        );
    }

    #[test]
    fn codex_args_split_chain_and_payload() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // plain: payload only
        let args = s(&["{\"type\":\"x\"}"]);
        let (chain, payload) = split_codex_args(&args);
        assert!(chain.is_none());
        assert_eq!(payload, Some("{\"type\":\"x\"}"));

        // chained: prog + its arg + payload
        let args = s(&[
            "--chain",
            "/Apps/SkyClient",
            "turn-ended",
            "{\"type\":\"x\"}",
        ]);
        let (chain, payload) = split_codex_args(&args);
        assert_eq!(chain.unwrap(), &s(&["/Apps/SkyClient", "turn-ended"])[..]);
        assert_eq!(payload, Some("{\"type\":\"x\"}"));

        // chained but no trailing JSON: payload must come from stdin
        let args = s(&["--chain", "/Apps/SkyClient", "turn-ended"]);
        let (chain, payload) = split_codex_args(&args);
        assert_eq!(chain.unwrap().len(), 2);
        assert!(payload.is_none());
    }

    #[test]
    fn codex_turn_ended_type_is_accepted() {
        assert!(
            map_codex_notify(&json!({"type": "turn-ended", "thread-id": "t9", "cwd": "/x"}))
                .is_some()
        );
        assert!(map_codex_notify(
            &json!({"type": "agent-turn-complete", "session-id": "s1", "cwd": "/x"})
        )
        .is_some());
    }

    #[test]
    fn codex_other_notification_types_fail_open() {
        assert!(
            map_codex_notify(&json!({"type": "session-configured", "thread-id": "t"})).is_none()
        );
        assert!(map_codex_notify(&json!({"type": "session-start"})).is_none());
        assert!(map_codex_notify(&json!({})).is_none());
        // A turn-complete with no ids/cwd is still a real completion: it
        // lands in the global bucket rather than being dropped.
        assert!(map_codex_notify(&json!({"type": "agent-turn-complete"})).is_some());
    }

    /// The empirically verified AskUserQuestion PreToolUse payload
    /// (claude 2.1.198).
    fn ask_hook() -> Value {
        json!({
            "session_id": "4018aba2-7f8c-4733-bcfa-d1ab7e41033c",
            "transcript_path": "/tmp/4018aba2.jsonl",
            "cwd": "/private/tmp/ask-spike",
            "prompt_id": "9639d3cb",
            "permission_mode": "acceptEdits",
            "hook_event_name": "PreToolUse",
            "tool_name": "AskUserQuestion",
            "tool_use_id": "toolu_01Cvj9QuwXtaGg2Yezug3EQ1",
            "tool_input": {
                "questions": [{
                    "question": "Do you prefer option A (fast) or option B (thorough)?",
                    "header": "Approach",
                    "options": [
                        {"label": "Option A (fast)", "description": "Quicker turnaround, less depth."},
                        {"label": "Option B (thorough)", "description": "Slower but more comprehensive."}
                    ],
                    "multiSelect": false
                }]
            }
        })
    }

    fn http_200(body: &str) -> String {
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{body}")
    }

    fn answered(json_body: serde_json::Value) -> Option<String> {
        question_output(
            &parse_questions(&ask_hook()["tool_input"]).unwrap(),
            &http_200(&json_body.to_string()),
        )
    }

    #[test]
    fn single_answer_reproduces_the_verified_hook_output() {
        let out = answered(json!({"answers": [
            {"question_index": 0, "labels": ["Option B (thorough)"], "free_text": null}
        ]}))
        .expect("a real answer must produce hook output");

        assert_eq!(
            out,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"The user answered via PingMyBell: \"Option B (thorough)\". Treat this as their answer to your question; do not ask again."}}"#,
            "this exact shape+wording was verified to make Claude accept the answer"
        );
    }

    #[test]
    fn multi_select_and_free_text_join_into_one_answer() {
        let two = |q: &str, h: &str| json!({"question": q, "header": h, "options": [], "multiSelect": true});
        let questions =
            parse_questions(&json!({"questions": [two("Which files?", "Scope")]})).unwrap();

        let out = encode_reason(
            &questions,
            &parse_answers(&http_200(
                &json!({"answers": [{"question_index": 0,
                        "labels": ["src/main.rs", "src/lib.rs"], "free_text": "and the tests"}]})
                .to_string(),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            out,
            "The user answered via PingMyBell: \"src/main.rs\", \"src/lib.rs\", \"and the tests\". \
             Treat this as their answer to your question; do not ask again."
        );

        // Free text alone (the TUI's "Type something") is a complete answer.
        let out = encode_reason(
            &questions,
            &parse_answers(&http_200(
                &json!({"answers": [{"question_index": 0, "labels": [], "free_text": "Neither — use the config file"}]})
                    .to_string(),
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(out.contains("\"Neither — use the config file\""));
        assert!(out.ends_with("do not ask again."));
    }

    #[test]
    fn multiple_questions_are_labelled_per_question() {
        let input = json!({"questions": [
            {"question": "Fast or thorough?", "header": "Approach", "options": []},
            {"question": "Which directory should I start in, given the layout?", "options": []},
            {"question": "Ship it?", "header": "Release", "options": []}
        ]});
        let questions = parse_questions(&input).unwrap();
        let answers = parse_answers(&http_200(
            &json!({"answers": [
                {"question_index": 0, "labels": ["Thorough"]},
                {"question_index": 1, "labels": [], "free_text": "src/"}
            ]})
            .to_string(),
        ))
        .unwrap();

        let out = encode_reason(&questions, &answers).unwrap();
        assert_eq!(
            out,
            "The user answered via PingMyBell: Approach — \"Thorough\"; \
             Which directory should I start in, given the layout? — \"src/\". \
             Treat these as their answers to your questions; do not ask again.",
            "header when present, question text as the fallback label"
        );

        // A single answer inside a multi-question call still gets its label
        // (otherwise Claude cannot tell which question was answered).
        let one = encode_reason(&questions, &answers[..1]).unwrap();
        assert!(one.starts_with("The user answered via PingMyBell: Approach — \"Thorough\"."));
        assert!(one.contains("Treat this as their answer to your question;"));
    }

    #[test]
    fn answers_for_unknown_questions_are_dropped() {
        let questions = parse_questions(&ask_hook()["tool_input"]).unwrap();
        let answers = parse_answers(&http_200(
            &json!({"answers": [
                {"question_index": 9, "labels": ["ghost"]},
                {"question_index": 0, "labels": ["Option A (fast)"]}
            ]})
            .to_string(),
        ))
        .unwrap();
        let out = encode_reason(&questions, &answers).unwrap();
        assert!(out.contains("\"Option A (fast)\""));
        assert!(!out.contains("ghost"));

        // Nothing left after dropping → no stdout at all.
        let only_ghost = parse_answers(&http_200(
            &json!({"answers": [{"question_index": 4, "labels": ["ghost"]}]}).to_string(),
        ))
        .unwrap();
        assert!(encode_reason(&questions, &only_ghost).is_none());
    }

    #[test]
    fn answer_text_is_sanitized_and_bounded() {
        let questions = parse_questions(&ask_hook()["tool_input"]).unwrap();
        let hostile = format!(
            "say \"hi\"\u{1b}[31m\n\nthen {}",
            "x".repeat(MAX_OPTION_LABEL_CHARS + 50)
        );
        let answers = parse_answers(&http_200(
            &json!({"answers": [{"question_index": 0, "labels": [hostile]}]}).to_string(),
        ))
        .unwrap();
        let value = &answers[0].values[0];
        assert!(!value.contains('"'), "nested quotes flattened: {value}");
        assert!(
            !value.chars().any(char::is_control),
            "control characters neutralized: {value:?}"
        );
        assert_eq!(
            value.chars().count(),
            MAX_OPTION_LABEL_CHARS + 1,
            "capped + ellipsis"
        );

        let out = encode_reason(&questions, &answers).unwrap();
        assert!(serde_json::from_str::<Value>(
            &question_output(
                &questions,
                &http_200(
                    &json!({"answers": [{"question_index": 0, "labels": ["a"]}]}).to_string()
                )
            )
            .unwrap()
        )
        .is_ok());
        assert!(out.starts_with("The user answered via PingMyBell: "));
    }

    #[test]
    fn oversized_reasons_stay_whole_and_quoted() {
        // Eight questions each answered with a max-length free text: the
        // reason must be budgeted by dropping whole answers, never by slicing
        // mid-value (an unbalanced quote confuses the model).
        let specs: Vec<Value> = (0..MAX_QUESTIONS)
            .map(|i| json!({"question": format!("Question {i}?"), "header": format!("H{i}")}))
            .collect();
        let questions = parse_questions(&json!({"questions": specs})).unwrap();
        let answers: Vec<Value> = (0..MAX_QUESTIONS)
            .map(|i| json!({"question_index": i, "free_text": "y".repeat(MAX_FREE_TEXT_CHARS)}))
            .collect();
        let parsed = parse_answers(&http_200(&json!({"answers": answers}).to_string())).unwrap();

        let out = encode_reason(&questions, &parsed).unwrap();
        assert!(out.chars().count() <= MAX_REASON_CHARS + 200, "budgeted");
        assert_eq!(
            out.matches('"').count() % 2,
            0,
            "every value stays balanced: {out:.120}"
        );
        assert!(out.ends_with("do not ask again."));
        // Truncation drops later answers rather than corrupting earlier ones.
        assert!(out.contains("H0 — \""));
        assert!(!out.contains("H7 — \""));
    }

    #[test]
    fn question_payloads_that_drifted_fail_open() {
        // No questions array, empty array, absurd count, empty question text.
        assert!(parse_questions(&json!({})).is_none());
        assert!(parse_questions(&json!({"questions": []})).is_none());
        assert!(parse_questions(&json!({"questions": "surprise"})).is_none());
        assert!(parse_questions(&json!({"questions": [{"header": "h"}]})).is_none());
        assert!(parse_questions(&json!({"questions": [{"question": "  \n "}]})).is_none());
        let many: Vec<Value> = (0..MAX_QUESTIONS + 1)
            .map(|i| json!({"question": format!("q{i}")}))
            .collect();
        assert!(parse_questions(&json!({"questions": many})).is_none());
    }

    #[test]
    fn malformed_answer_responses_print_nothing() {
        let questions = parse_questions(&ask_hook()["tool_input"]).unwrap();
        let none = |raw: &str| {
            assert!(
                question_output(&questions, raw).is_none(),
                "must fail open on: {raw:?}"
            );
        };
        none("");
        none("HTTP/1.1 204 No Content\r\n\r\n");
        none("HTTP/1.1 500 Internal Server Error\r\n\r\n{\"answers\":[{\"question_index\":0,\"labels\":[\"x\"]}]}");
        none("HTTP/1.1 401 Unauthorized\r\n\r\n");
        none(&http_200("not json"));
        none(&http_200("{}"));
        none(&http_200(r#"{"answers": "yes"}"#));
        none(&http_200(r#"{"answers": []}"#));
        none(&http_200(r#"{"answers": [{"question_index": 0}]}"#));
        none(&http_200(
            r#"{"answers": [{"question_index": 0, "labels": []}]}"#,
        ));
        none(&http_200(
            r#"{"answers": [{"question_index": 0, "labels": ["  "], "free_text": ""}]}"#,
        ));
        none(&http_200(r#"{"answers": [{"labels": ["no index"]}]}"#));
        none(&http_200(
            r#"{"answers": [{"question_index": -1, "labels": ["x"]}]}"#,
        ));
        none(&http_200(r#"{"decision": "allow"}"#));
        // Headerless / truncated responses.
        none("HTTP/1.1 200 OK");
        none("garbage");
    }

    #[test]
    fn questions_ignore_gating_and_permission_modes() {
        // run_pretool routes on the tool name BEFORE consulting gating or the
        // permission mode, so a question survives every combination that
        // would have suppressed an approval.
        for mode in ["bypassPermissions", "acceptEdits", "default", "plan", ""] {
            let hook = json!({"tool_name": ASK_USER_QUESTION, "permission_mode": mode});
            assert!(is_question(&hook), "mode {mode:?} must not hide a question");
        }
        // Two of those modes, and the default-off flag, WOULD have suppressed
        // an approval — which is exactly why the routing happens first.
        assert!(!should_gate_with(
            true,
            &json!({"tool_name": ASK_USER_QUESTION, "permission_mode": "bypassPermissions"})
        ));
        assert!(!should_gate_with(
            false,
            &json!({"tool_name": ASK_USER_QUESTION, "permission_mode": "default"})
        ));
        // Other tools keep the approval path.
        assert!(!is_question(&json!({"tool_name": "Bash"})));
        assert!(!is_question(&json!({})));
    }

    /// The empirically verified Codex `request_user_input` PreToolUse payload
    /// (codex-cli 0.146.0-alpha.9.2, dumped live on 2026-07-31).
    fn codex_ask_hook() -> Value {
        json!({
            "session_id": "019fbb08-600e-7dd3-a757-85196a541b24",
            "turn_id": "019fbb08-6045-72e2-828c-bcbc207ad9ca",
            "transcript_path": "/Users/x/.codex/sessions/2026/07/31/rollout-abc.jsonl",
            "cwd": "/Users/x/proj",
            "hook_event_name": "PreToolUse",
            "model": "gpt-5.6-sol",
            "permission_mode": "bypassPermissions",
            "tool_name": "request_user_input",
            "tool_use_id": "call_mxZtKQ9NIQrRQcgRNrc5DhHR",
            "tool_input": {"questions": [{
                "header": "Target",
                "id": "deployment_target",
                "options": [
                    {"description": "Deploy to the staging environment.", "label": "Staging (Recommended)"},
                    {"description": "Deploy to the live production environment.", "label": "Production"}
                ],
                "question": "Which deployment target should I use?"
            }]}
        })
    }

    #[test]
    fn codex_questions_normalize_to_the_shape_the_app_accepts() {
        let (specs, questions) =
            map_codex_questions(&codex_ask_hook()["tool_input"]).expect("verified payload maps");

        assert_eq!(
            questions,
            json!([{
                "question": "Which deployment target should I use?",
                "header": "Target",
                "options": [
                    {"label": "Staging (Recommended)", "description": "Deploy to the staging environment."},
                    {"label": "Production", "description": "Deploy to the live production environment."}
                ],
                "multiSelect": false
            }]),
            "Codex's per-question `id` is dropped and multiSelect pinned false"
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].header, "Target");
        assert_eq!(specs[0].question, "Which deployment target should I use?");
    }

    #[test]
    fn codex_three_question_payload_maps_in_order() {
        // Codex's documented maximum: 3 questions, 2–3 options each.
        let input = json!({"questions": [
            {"id": "a", "header": "Scope", "question": "How wide?",
             "options": [{"label": "Narrow", "description": "d1"}, {"label": "Wide", "description": "d2"}]},
            {"id": "b", "header": "Speed", "question": "How fast?",
             "options": [{"label": "Now", "description": "d3"}, {"label": "Later", "description": "d4"},
                         {"label": "Never", "description": "d5"}]},
            // No header, no descriptions: both are optional to US even though
            // Codex says it always sends them.
            {"id": "c", "question": "Ship it?", "options": [{"label": "Yes"}, {"label": "No"}]}
        ]});
        let (specs, questions) = map_codex_questions(&input).unwrap();
        assert_eq!(specs.len(), 3);
        assert_eq!(questions.as_array().unwrap().len(), 3);
        assert_eq!(questions[1]["options"].as_array().unwrap().len(), 3);
        assert_eq!(questions[2]["header"], "");
        assert_eq!(questions[2]["options"][0]["description"], "");
        for q in questions.as_array().unwrap() {
            assert_eq!(q["multiSelect"], false);
            assert!(q["id"].is_null(), "the Codex id must not leak downstream");
        }

        // Labelled per question, exactly like a multi-question Claude call.
        let answers = parse_answers(&http_200(
            &json!({"answers": [{"question_index": 1, "labels": ["Later"]}]}).to_string(),
        ))
        .unwrap();
        let reason = encode_reason(&specs, &answers).unwrap();
        assert_eq!(
            reason,
            "The user answered via PingMyBell: Speed — \"Later\". \
             Treat this as their answer to your question; do not ask again."
        );
    }

    #[test]
    fn codex_answer_reproduces_the_verified_deny_output() {
        let (specs, _) = map_codex_questions(&codex_ask_hook()["tool_input"]).unwrap();
        let out = question_output(
            &specs,
            &http_200(
                &json!({"answers": [{"question_index": 0, "labels": ["Production"]}]}).to_string(),
            ),
        )
        .expect("a real answer must produce hook output");
        assert_eq!(
            out,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"The user answered via PingMyBell: \"Production\". Treat this as their answer to your question; do not ask again."}}"#,
            "phrasing is load-bearing: it overcomes Codex's 'Tool call blocked by…' wrapper"
        );
    }

    #[test]
    fn codex_question_payloads_that_drifted_fail_open() {
        let none = |input: Value| {
            assert!(
                map_codex_questions(&input).is_none(),
                "must fail open on: {input}"
            );
        };
        none(json!({}));
        none(json!({"questions": []}));
        none(json!({"questions": "surprise"}));
        none(json!({"questions": {"id": "x"}}));
        // Missing / blank question text.
        none(json!({"questions": [{"id": "x", "options": [{"label": "a"}]}]}));
        none(json!({"questions": [{"question": " \n ", "options": [{"label": "a"}]}]}));
        none(json!({"questions": [{"question": 42, "options": [{"label": "a"}]}]}));
        // Nothing clickable: no options key, empty list, or only blank labels.
        none(json!({"questions": [{"question": "q?"}]}));
        none(json!({"questions": [{"question": "q?", "options": []}]}));
        none(
            json!({"questions": [{"question": "q?", "options": [{"label": "  "}, {"label": ""}]}]}),
        );
        none(json!({"questions": [{"question": "q?", "options": "two"}]}));
        // One bad question poisons the whole call (the answer must map 1:1).
        none(json!({"questions": [
            {"question": "ok?", "options": [{"label": "a"}]},
            {"question": "", "options": [{"label": "b"}]}
        ]}));
        // Absurd count.
        let many: Vec<Value> = (0..MAX_QUESTIONS + 1)
            .map(|i| json!({"question": format!("q{i}"), "options": [{"label": "a"}]}))
            .collect();
        none(json!({"questions": many}));
    }

    #[test]
    fn codex_question_text_is_sanitized() {
        let (specs, questions) = map_codex_questions(&json!({"questions": [{
            "question": "Deploy\u{1b}[31m  now\nor later?",
            "header": "  Target\u{7} ",
            "options": [{"label": "  Ship\tit  ", "description": "line1\nline2"}]
        }]}))
        .unwrap();
        assert_eq!(specs[0].question, "Deploy [31m now or later?");
        assert_eq!(questions[0]["header"], "Target");
        assert_eq!(questions[0]["options"][0]["label"], "Ship it");
        assert_eq!(questions[0]["options"][0]["description"], "line1 line2");
    }

    #[test]
    fn codex_questions_share_the_cwd_keyed_session_of_notify_events() {
        // The whole point of cwd keying: a question and that session's
        // turn-complete callouts must land on the SAME board row, even though
        // the hook envelope's session_id has nothing to do with the notify
        // payload's rotating ids.
        let hook = codex_ask_hook();
        let cwd = hook["cwd"].as_str().unwrap();
        let notify = map_codex_notify(&json!({
            "type": "agent-turn-complete", "thread-id": "rotates-per-turn",
            "cwd": cwd, "last-assistant-message": "Deployed."
        }))
        .unwrap();

        assert_eq!(
            codex_session_id(cwd),
            notify["session_id"].as_str().unwrap()
        );
        assert_ne!(
            codex_session_id(cwd),
            hook["session_id"].as_str().unwrap(),
            "we deliberately do NOT key on the hook's own session_id"
        );
        assert_eq!(
            codex_session_id(""),
            format!("codex-{}", stable_id("global"))
        );
    }

    // ─── Codex approvals (PermissionRequest) ────────────────────────────────

    /// The empirically verified Codex `PermissionRequest` payload for an exec
    /// approval (codex-cli 0.146.0-alpha.9.2, dumped live on 2026-07-31 in a
    /// throwaway CODEX_HOME). Note what is NOT here: no `tool_use_id`, and
    /// `hook_event_name` is `PermissionRequest`, not `PreToolUse`.
    fn codex_exec_approval_hook() -> Value {
        json!({
            "session_id": "019fbb4f-ac81-72d2-b93a-03ae4bddfce4",
            "turn_id": "019fbb4f-acba-7f22-96fa-470eecda2c4d",
            "transcript_path": "/Users/x/.codex/sessions/2026/07/31/rollout-abc.jsonl",
            "cwd": "/Users/x/proj",
            "hook_event_name": "PermissionRequest",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": "curl -sS https://example.com -o /dev/null"}
        })
    }

    /// Same event, file-change flavour. `tool_input.command` is the raw patch.
    fn codex_patch_approval_hook() -> Value {
        json!({
            "session_id": "019fbb51-31c4-7401-b51b-8a7d4ab7c9f4",
            "turn_id": "019fbb51-31fa-7d70-bca4-625abbaee8b4",
            "transcript_path": "/Users/x/.codex/sessions/2026/07/31/rollout-def.jsonl",
            "cwd": "/Users/x/proj",
            "hook_event_name": "PermissionRequest",
            "model": "gpt-5.6-sol",
            "permission_mode": "default",
            "tool_name": "apply_patch",
            "tool_input": {"command": "*** Begin Patch\n*** Add File: canary.txt\n+PMB_PATCH_CANARY\n*** End Patch"}
        })
    }

    #[test]
    fn codex_exec_approval_maps_to_the_shared_approval_body() {
        let body = codex_approval_body(CodexGate::Always, &codex_exec_approval_hook())
            .expect("verified payload maps");
        assert_eq!(body["agent"], "codex");
        assert_eq!(body["event"], "permission_request");
        assert_eq!(body["cwd"], "/Users/x/proj");
        assert_eq!(body["tool"]["name"], "Bash");
        assert_eq!(
            body["tool"]["input"]["command"], "curl -sS https://example.com -o /dev/null",
            "the command line is what the card must show"
        );
        assert!(body["summary"].is_null());
    }

    #[test]
    fn codex_file_change_approval_carries_the_patch_verbatim() {
        let body = codex_approval_body(CodexGate::Always, &codex_patch_approval_hook())
            .expect("verified payload maps");
        assert_eq!(body["tool"]["name"], "apply_patch");
        assert!(body["tool"]["input"]["command"]
            .as_str()
            .unwrap()
            .contains("*** Add File: canary.txt"));
    }

    #[test]
    fn codex_approvals_share_the_cwd_keyed_session_of_questions_and_notify() {
        // The board row must be the same one this project's questions and
        // turn-completes land on, or an approval opens a second session.
        let hook = codex_exec_approval_hook();
        let cwd = hook["cwd"].as_str().unwrap();
        let body = codex_approval_body(CodexGate::Always, &hook).unwrap();
        let notify = map_codex_notify(&json!({
            "type": "agent-turn-complete", "thread-id": "rotates-per-turn",
            "cwd": cwd, "last-assistant-message": "Done."
        }))
        .unwrap();

        assert_eq!(body["session_id"], notify["session_id"]);
        assert_eq!(body["session_id"], json!(codex_session_id(cwd)));
        assert_ne!(
            body["session_id"].as_str().unwrap(),
            hook["session_id"].as_str().unwrap(),
            "we deliberately do NOT key on the hook's own session_id"
        );
    }

    #[test]
    fn codex_approval_allow_needs_no_updated_input() {
        // The load-bearing difference from the question path: Approve here
        // genuinely lets the command RUN. Verified against the real binary —
        // `behavior: "allow"` turned "command execution approval is not
        // supported in exec mode" into the command executing. Adding
        // `updatedInput`/`updatedPermissions`/`interrupt` would make Codex
        // fail the hook CLOSED, so the encoded object must stay exactly this.
        let out = codex_approval_output("allow").expect("allow is expressible");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed,
            json!({"hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"}
            }})
        );
        let decision = &parsed["hookSpecificOutput"]["decision"];
        for reserved in ["updatedInput", "updatedPermissions", "interrupt"] {
            assert!(
                decision.get(reserved).is_none(),
                "{reserved} makes Codex fail the hook closed"
            );
        }
    }

    #[test]
    fn codex_approval_deny_carries_a_message_the_model_reads() {
        let out = codex_approval_output("deny").expect("deny is expressible");
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(parsed["hookSpecificOutput"]["decision"]["behavior"], "deny");
        let message = parsed["hookSpecificOutput"]["decision"]["message"]
            .as_str()
            .expect("deny must carry a message");
        assert!(
            !message.trim().is_empty(),
            "an empty message makes Codex substitute its own boilerplate"
        );
    }

    #[test]
    fn codex_approval_ask_defers_to_codex_and_prints_nothing() {
        // "Terminal" on the card. Codex's PermissionRequest schema has no
        // third behavior, so the only way to say "you decide" is silence —
        // which is the same shape as every fail-open path here.
        assert!(codex_approval_output("ask").is_none());
        assert!(codex_approval_output("").is_none());
        assert!(codex_approval_output("approve").is_none());
    }

    #[test]
    fn codex_approval_payloads_that_drifted_fail_open() {
        let base = codex_exec_approval_hook();
        let mut wrong_event = base.clone();
        wrong_event["hook_event_name"] = json!("PreToolUse");
        assert!(
            codex_approval_body(CodexGate::Always, &wrong_event).is_none(),
            "a PreToolUse payload here would produce output Codex fails closed on"
        );

        let mut no_tool = base.clone();
        no_tool["tool_name"] = json!("");
        assert!(codex_approval_body(CodexGate::Always, &no_tool).is_none());

        let mut no_input = base.clone();
        no_input["tool_input"] = json!("a string");
        assert!(codex_approval_body(CodexGate::Always, &no_input).is_none());

        assert!(codex_approval_body(CodexGate::Always, &json!({})).is_none());
        // The question payload must never be mistaken for an approval.
        assert!(codex_approval_body(CodexGate::Always, &codex_ask_hook()).is_none());
    }

    #[test]
    fn codex_approvals_respect_gating_unlike_questions() {
        // Approvals ARE gated, because this channel can GRANT permission
        // without the user ever seeing Codex's own prompt, and because it
        // relocates the ask away from the context that explains it. Questions
        // are not, because the agent is already blocked on a human.
        //
        // The gate is enforced INSIDE the body builder, not only by the
        // fast-path return in `run_codex_approve` — so deleting that return
        // (an optimisation) cannot silently make approvals always-on.
        let hook = codex_exec_approval_hook();
        assert!(codex_approval_body(CodexGate::Always, &hook).is_some());
        assert!(
            codex_approval_body(CodexGate::Never, &hook).is_none(),
            "gate_codex_approvals=never must never produce an approval request"
        );
        // `auto` on a payload whose transcript_path does not exist has no way
        // to know the user's setting, so it declines — fail open, not park.
        let mut missing = hook.clone();
        missing["transcript_path"] = json!("/nonexistent/pmb/rollout.jsonl");
        assert!(
            !std::path::Path::new("/nonexistent/pmb/rollout.jsonl").exists(),
            "this assertion is only meaningful while the path is absent"
        );
        assert!(
            codex_approval_body(CodexGate::Auto, &missing).is_none(),
            "auto must not park when it cannot read the session's settings"
        );
        // The question path is deliberately the opposite.
        assert!(is_question(&json!({"tool_name": ASK_USER_QUESTION})));

        // Codex reports this only under approval_policy = never, where the
        // hook cannot fire anyway — but the guard costs nothing.
        let mut bypass = hook.clone();
        bypass["permission_mode"] = json!("bypassPermissions");
        assert!(codex_approval_body(CodexGate::Always, &bypass).is_none());
    }

    // ─── CodexGate::Auto — mirroring the user's own Codex setting ───────────

    /// A rollout `turn_context` record, the shape verified live on
    /// codex-cli 0.146.0-alpha.9.2 (fields the rule does not read are elided).
    fn turn_context(policy: &str, reviewer: Option<&str>) -> String {
        let mut payload = json!({
            "turn_id": "019fbc80-d1c6-7f52-9e18-9a3e0b8b1f0a",
            "cwd": "/Users/x/proj",
            "approval_policy": policy,
            "sandbox_policy": {"type": "workspace-write", "writable_roots": []},
            "model": "gpt-5.6-sol",
        });
        if let Some(reviewer) = reviewer {
            payload["approvals_reviewer"] = json!(reviewer);
        }
        json!({"timestamp": "2026-08-01T04:52:00.000Z", "type": "turn_context", "payload": payload})
            .to_string()
    }

    fn write_rollout(dir: &std::path::Path, name: &str, lines: &[String]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        path
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pmb-shim-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every mode combination we observed, and what `auto` must do with it.
    ///
    /// The empirical table (codex-cli 0.146.0-alpha.9.2, isolated CODEX_HOME):
    /// `permission_mode` is "default" for untrusted, on-request AND on-failure
    /// alike and "bypassPermissions" only for `never` — where the hook does not
    /// fire at all. So the payload alone cannot answer this; the rollout can,
    /// because it records `approval_policy` and `approvals_reviewer` verbatim.
    #[test]
    fn auto_intercepts_only_when_codex_asks_the_user_about_everything() {
        for (policy, reviewer, want, why) in [
            // "Ask for approval": the user opted into being asked about
            // everything, so answering from the notch is strictly better.
            ("on-request", Some("user"), true, "Ask for approval"),
            ("untrusted", Some("user"), true, "untrusted + user reviews"),
            // "Approve for me": the guardian handles the safe things and only
            // escalates the unsafe ones. The user already said do not
            // interrupt me — PingMyBell must not override that.
            ("on-request", Some("auto_review"), false, "Approve for me"),
            ("untrusted", Some("auto_review"), false, "guardian reviews"),
            (
                "on-request",
                Some("guardian_subagent"),
                false,
                "auto_review's alias",
            ),
            // Full auto: the hook does not even fire here, but the rule must
            // still say no if it ever did.
            ("never", Some("user"), false, "full auto"),
            ("never", Some("auto_review"), false, "full auto"),
            // Drift: an unknown policy or reviewer is not evidence of consent.
            ("granular", Some("user"), false, "unverified policy"),
            (
                "on-request",
                Some("something_new"),
                false,
                "unknown reviewer",
            ),
            ("", Some("user"), false, "empty policy"),
            // A Codex predating the reviewer axis: `untrusted` is itself the
            // ask-me-about-everything policy; `on-request` is not.
            ("untrusted", None, true, "legacy untrusted"),
            ("on-request", None, false, "legacy on-request"),
            ("never", None, false, "legacy never"),
        ] {
            let mut ctx: Value = serde_json::from_str(&turn_context(policy, reviewer)).unwrap();
            let ctx = ctx["payload"].take();
            assert_eq!(
                codex_ask_everything_from(&ctx),
                want,
                "{policy}/{reviewer:?} ({why})"
            );
        }
        // A turn_context missing the policy entirely.
        assert!(!codex_ask_everything_from(&json!({})));
        assert!(!codex_ask_everything_from(&json!({"approval_policy": 7})));

        // A reviewer that is PRESENT but not a string we can read is drift,
        // not consent — and must not fall into the "legacy Codex" branch,
        // which under `untrusted` would flip an "Approve for me" user back
        // into being interrupted. `null` is the one non-string that does mean
        // "no reviewer recorded".
        for shape in [
            json!({"kind": "auto_review"}),
            json!(["auto_review"]),
            json!(7),
            json!(true),
        ] {
            for policy in ["untrusted", "on-request"] {
                let ctx = json!({"approval_policy": policy, "approvals_reviewer": shape});
                assert!(!codex_ask_everything_from(&ctx), "{policy} / {shape}");
            }
        }
        assert!(codex_ask_everything_from(
            &json!({"approval_policy": "untrusted", "approvals_reviewer": null})
        ));
        assert!(!codex_ask_everything_from(
            &json!({"approval_policy": "on-request", "approvals_reviewer": null})
        ));
    }

    #[test]
    fn auto_reads_the_newest_turn_context_from_the_rollout() {
        let dir = scratch_dir("rollout");
        // A session that started under "Approve for me" and was switched to
        // "Ask for approval" mid-session must be read as the latter.
        let path = write_rollout(
            &dir,
            "rollout-switch.jsonl",
            &[
                json!({"type": "session_meta", "payload": {"id": "x"}}).to_string(),
                turn_context("on-request", Some("auto_review")),
                json!({"type": "response_item", "payload": {"type": "message"}}).to_string(),
                turn_context("on-request", Some("user")),
                json!({"type": "response_item", "payload": {"type": "message"}}).to_string(),
            ],
        );
        let hook = json!({"transcript_path": path.to_string_lossy()});
        assert!(codex_asks_user_about_everything(&hook));

        // ...and the other way round.
        let path = write_rollout(
            &dir,
            "rollout-back.jsonl",
            &[
                turn_context("on-request", Some("user")),
                turn_context("on-request", Some("auto_review")),
            ],
        );
        let hook = json!({"transcript_path": path.to_string_lossy()});
        assert!(!codex_asks_user_about_everything(&hook));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_fails_open_on_every_unreadable_rollout() {
        let dir = scratch_dir("rollout-bad");

        // No transcript_path at all, empty, or pointing nowhere.
        assert!(!codex_asks_user_about_everything(&json!({})));
        assert!(!codex_asks_user_about_everything(
            &json!({"transcript_path": ""})
        ));
        assert!(!codex_asks_user_about_everything(
            &json!({"transcript_path": "/nonexistent/rollout.jsonl"})
        ));
        assert!(!codex_asks_user_about_everything(
            &json!({"transcript_path": 42})
        ));
        // A directory, not a file.
        assert!(!codex_asks_user_about_everything(
            &json!({"transcript_path": dir.to_string_lossy()})
        ));

        // Present but useless: empty, garbage, truncated JSON, no turn_context,
        // a turn_context whose payload is not an object, and raw bytes.
        for (name, body) in [
            ("empty.jsonl", String::new()),
            ("garbage.jsonl", "not json at all\n".repeat(50)),
            (
                "truncated.jsonl",
                "{\"type\":\"turn_context\",\"payload\":{\"approval_policy\":\"untru".to_string(),
            ),
            (
                "no-ctx.jsonl",
                json!({"type": "response_item", "payload": {}}).to_string(),
            ),
            (
                "bad-payload.jsonl",
                json!({"type": "turn_context", "payload": "untrusted"}).to_string(),
            ),
            (
                "decoy.jsonl",
                // The marker appears, but only inside someone's message text.
                json!({"type": "response_item",
                       "payload": {"text": "the \"turn_context\" is approval_policy untrusted"}})
                .to_string(),
            ),
            ("binary.jsonl", "\u{0}\u{1}\u{2}".repeat(500)),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, &body).unwrap();
            let hook = json!({"transcript_path": path.to_string_lossy()});
            assert!(
                !codex_asks_user_about_everything(&hook),
                "{name} must fail open"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_ignores_records_that_merely_look_like_a_turn_context() {
        let dir = scratch_dir("rollout-lookalike");

        // A rollout containing the marker inside some OTHER record — an agent
        // that read a rollout, or tool output echoing one. Only the `type`
        // check stops this being read as the user's setting. (Note the marker
        // has to appear unescaped to be found at all, which is why this is a
        // JSON *value* rather than quoted prose.)
        let decoy = json!({"type": "response_item", "payload": {
            "kind": "turn_context",
            "approval_policy": "untrusted", "approvals_reviewer": "user"}})
        .to_string();
        assert!(
            decoy.contains(TURN_CONTEXT_MARKER),
            "the decoy must actually reach the type check"
        );
        let path = write_rollout(&dir, "decoy.jsonl", &[decoy]);
        assert!(!codex_asks_user_about_everything(
            &json!({"transcript_path": path.to_string_lossy()})
        ));

        // ...and a real record later in the same file still wins, so the
        // guard above rejects the decoy rather than the whole file.
        let decoy = json!({"type": "response_item",
                           "payload": {"kind": "turn_context"}})
        .to_string();
        let path = write_rollout(
            &dir,
            "decoy-then-real.jsonl",
            &[turn_context("on-request", Some("user")), decoy],
        );
        assert!(codex_asks_user_about_everything(
            &json!({"transcript_path": path.to_string_lossy()})
        ));

        // Shape guards below this line are defensive, not behavioural: a
        // non-object payload and an unparseable fragment both read as absent
        // fields and so already fail open. They are asserted for the contract,
        // not because removing them would change a decision.
        let path = write_rollout(
            &dir,
            "string-payload.jsonl",
            &[json!({"type": "turn_context", "payload": "untrusted"}).to_string()],
        );
        assert!(last_turn_context(&path).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A FIFO reports length 0, so without the `is_file` guard `read_to_end`
    /// blocks until a writer that never comes — a hung hook, which is worse
    /// than not parking. Run off-thread so a regression FAILS instead of
    /// wedging the suite.
    #[cfg(unix)]
    #[test]
    fn auto_never_blocks_on_a_transcript_path_that_is_not_a_regular_file() {
        let dir = scratch_dir("rollout-fifo");
        let fifo = dir.join("rollout.jsonl");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(made, "mkfifo is needed for this test");

        let (tx, rx) = std::sync::mpsc::channel();
        let probe = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(last_turn_context(&probe).is_some());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(found) => assert!(!found, "a FIFO can never yield a turn_context"),
            Err(_) => panic!("last_turn_context blocked on a FIFO — the is_file guard is gone"),
        }
        // A directory is refused by the same guard.
        assert!(last_turn_context(&dir).is_none());

        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_reads_a_bounded_tail_of_a_huge_rollout() {
        let dir = scratch_dir("rollout-huge");
        let filler = json!({"type": "response_item",
                            "payload": {"text": "x".repeat(4000)}})
        .to_string();

        // The newest turn_context sits well inside the tail window: found.
        let mut lines = vec![turn_context("on-request", Some("auto_review"))];
        // ~4 MB of filler, i.e. more than ROLLOUT_TAIL_BYTES, before it.
        lines.extend(std::iter::repeat_n(filler.clone(), 1000));
        lines.push(turn_context("on-request", Some("user")));
        lines.extend(std::iter::repeat_n(filler.clone(), 10));
        let path = write_rollout(&dir, "huge-ok.jsonl", &lines);
        assert!(std::fs::metadata(&path).unwrap().len() > ROLLOUT_TAIL_BYTES);
        assert!(codex_asks_user_about_everything(
            &json!({"transcript_path": path.to_string_lossy()})
        ));

        // Buried deeper than the window: no evidence → do not park.
        let mut lines = vec![turn_context("untrusted", Some("user"))];
        lines.extend(std::iter::repeat_n(filler, 1000));
        let path = write_rollout(&dir, "huge-buried.jsonl", &lines);
        assert!(std::fs::metadata(&path).unwrap().len() > ROLLOUT_TAIL_BYTES);
        assert!(!codex_asks_user_about_everything(
            &json!({"transcript_path": path.to_string_lossy()})
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_three_settings_decide_the_same_payload_differently() {
        let dir = scratch_dir("rollout-gate");
        let mut hook = codex_exec_approval_hook();

        // Session in "Approve for me": always parks, auto does not, never does not.
        let path = write_rollout(
            &dir,
            "approve-for-me.jsonl",
            &[turn_context("on-request", Some("auto_review"))],
        );
        hook["transcript_path"] = json!(path.to_string_lossy());
        assert!(codex_approval_body(CodexGate::Always, &hook).is_some());
        assert!(codex_approval_body(CodexGate::Auto, &hook).is_none());
        assert!(codex_approval_body(CodexGate::Never, &hook).is_none());

        // Session in "Ask for approval": auto now parks too — but never still
        // wins, because an explicit off is an explicit off.
        let path = write_rollout(
            &dir,
            "ask-for-approval.jsonl",
            &[turn_context("on-request", Some("user"))],
        );
        hook["transcript_path"] = json!(path.to_string_lossy());
        assert!(codex_approval_body(CodexGate::Always, &hook).is_some());
        assert!(codex_approval_body(CodexGate::Auto, &hook).is_some());
        assert!(codex_approval_body(CodexGate::Never, &hook).is_none());

        // The cheap guards still run first under auto: a tool we cannot
        // describe is refused even in a session that asks about everything.
        let mut unknown = hook.clone();
        unknown["tool_name"] = json!("some_mcp__tool");
        assert!(codex_approval_body(CodexGate::Auto, &unknown).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_approvals_only_act_on_tools_we_can_describe() {
        // Our matcher selects exactly these two, but a widened matcher — or a
        // future Codex routing something else through PermissionRequest —
        // must fall through to Codex's own prompt rather than let the overlay
        // grant permission for a tool the card cannot summarize.
        for tool in ["Bash", "apply_patch"] {
            let hook = json!({
                "hook_event_name": "PermissionRequest", "cwd": "/x",
                "permission_mode": "default", "tool_name": tool,
                "tool_input": {"command": "ls"},
            });
            assert!(
                codex_approval_body(CodexGate::Always, &hook).is_some(),
                "{tool}"
            );
        }
        for tool in [
            "shell",
            "unified_exec",
            "Write",
            "Edit",
            "read_file",
            "some_mcp__tool",
            "bash",
            "",
        ] {
            let hook = json!({
                "hook_event_name": "PermissionRequest", "cwd": "/x",
                "permission_mode": "default", "tool_name": tool,
                "tool_input": {"command": "ls"},
            });
            assert!(
                codex_approval_body(CodexGate::Always, &hook).is_none(),
                "{tool} must fall through to Codex"
            );
        }
    }

    #[test]
    fn benign_notification_types_are_dropped() {
        let benign = json!({"session_id": "s", "cwd": "/tmp", "message": "Signed in", "notification_type": "auth_success"});
        assert!(map_claude_hook("notification", &benign).is_none());

        let perm = json!({"session_id": "s", "cwd": "/tmp", "message": "Needs approval", "notification_type": "permission_prompt"});
        assert!(map_claude_hook("notification", &perm).is_some());

        let untyped = json!({"session_id": "s", "cwd": "/tmp", "message": "Waiting for input"});
        assert!(
            map_claude_hook("notification", &untyped).is_some(),
            "absent type must err toward speaking"
        );
    }
}
