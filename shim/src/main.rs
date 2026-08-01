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

    let Some(body) = map_claude_hook(subcommand, &hook) else {
        return;
    };
    post_event(&body.to_string(), "/v1/event", IO_TIMEOUT);
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
    let Some(home) = home_dir() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(home.join(".pingmybell").join("config.json")) else {
        return false;
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|c| c["gate_tool_calls"].as_bool())
        .unwrap_or(false)
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
    let dir = home_dir()?.join(".pingmybell");
    let port: u16 = std::fs::read_to_string(dir.join("port"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let token = std::fs::read_to_string(dir.join("token")).ok()?;
    let token = token.trim();

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_read_timeout(Some(read_timeout)).ok()?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;

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
        none(json!({"questions": [{"question": "q?", "options": [{"label": "  "}, {"label": ""}]}]}));
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

        assert_eq!(codex_session_id(cwd), notify["session_id"].as_str().unwrap());
        assert_ne!(
            codex_session_id(cwd),
            hook["session_id"].as_str().unwrap(),
            "we deliberately do NOT key on the hook's own session_id"
        );
        assert_eq!(codex_session_id(""), format!("codex-{}", stable_id("global")));
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
