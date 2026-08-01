//! Loopback-only ingest server (ARCHITECTURE.md §4, PRD FR-1).
//!
//! Binds 127.0.0.1 on a random free port, writes the port and a per-install
//! bearer token to `~/.pingmybell/` with user-only permissions, and accepts
//! normalized events on `POST /v1/event`. Bad input is logged and dropped —
//! never a crash (AC-1.3).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use rand::RngCore;
use subtle::ConstantTimeEq;
use tauri::{AppHandle, Emitter};

use crate::broker::{ApprovalInfo, Broker, QuestionInfo, QuestionOutcome, QuestionSpec};
use crate::overlay::Overlay;
use crate::registry::{AgentKind, EventKind, NormalizedEvent, Registry, Session};
use crate::speaker::{self, Priority, SpeakerHandle, Utterance};
use crate::{adapters, summarize};

/// Broker wait before answering 204 — safely inside the 120 s hook timeout
/// (§4). Env override exists for integration testing only.
const APPROVAL_TIMEOUT_SECS: u64 = 110;

/// The decide/answer-vs-timeout race window: when the UI grabbed the entry
/// just as the timer fired, its send is imminent — wait briefly so the click
/// still counts.
const RACE_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

fn approval_timeout() -> std::time::Duration {
    let secs = std::env::var("PINGMYBELL_APPROVAL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(APPROVAL_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// `~/.pingmybell/`, created 0700 on unix (AC-1.1). On Windows this relies on
/// the default `%USERPROFILE%` ACL inheritance; tighten explicitly when the
/// Windows work lands.
pub fn data_dir() -> io::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
    let dir = home.join(".pingmybell");
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

struct AppState {
    token: String,
    app: AppHandle,
    registry: Arc<Registry>,
    speaker: SpeakerHandle,
    overlay: Option<Arc<Overlay>>,
    broker: Arc<Broker>,
}

pub async fn serve(
    app: AppHandle,
    registry: Arc<Registry>,
    speaker: SpeakerHandle,
    overlay: Option<Arc<Overlay>>,
    broker: Arc<Broker>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dir = data_dir()?;

    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();

    write_private(&dir.join("token"), token.as_bytes())?;
    write_private(&dir.join("port"), port.to_string().as_bytes())?;
    log::info!("ingest server listening on 127.0.0.1:{port}");

    let state = Arc::new(AppState {
        token,
        app,
        registry,
        speaker,
        overlay,
        broker,
    });
    let router = Router::new()
        .route("/v1/event", post(post_event))
        .route("/v1/approval", post(post_approval))
        .route("/v1/question", post(post_question))
        .with_state(state);

    axum::serve(listener, router).await?;
    Ok(())
}

/// Write a file with 0600 permissions on unix (AC-1.1); Windows inherits the
/// user-profile ACLs (see `data_dir`).
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    // mode() only applies on create; re-assert for pre-existing files
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    Ok(())
}

/// Constant-time bearer-token check (§9 invariant 1).
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };
    let a = presented.as_bytes();
    let b = token.as_bytes();
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

async fn post_event(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED;
    }

    let mut event: NormalizedEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(err) => {
            log::warn!("dropping malformed event payload: {err}");
            return StatusCode::BAD_REQUEST;
        }
    };

    // Normalize the summary up front: cleanup happens in core (§5.1), and raw
    // assistant text must never be persisted or logged (§9 invariant 4) — the
    // registry stores exactly what we'd speak, ≤220 chars.
    event.summary = match event.summary.take() {
        Some(raw) => non_empty(summarize::clean(&raw)),
        None if event.event == EventKind::TurnComplete && event.agent == AgentKind::ClaudeCode => {
            event
                .transcript_path
                .as_deref()
                .and_then(|p| adapters::claude_code::last_assistant_message(Path::new(p)))
                .and_then(|raw| non_empty(summarize::clean(&raw)))
        }
        None => None,
    };

    // The emit happens inside apply's notify callback, under the registry
    // lock, so concurrent events for one session reach the UI in order.
    match state.registry.apply(&event, |snapshot| {
        if let Err(err) = state.app.emit("session-updated", snapshot) {
            log::warn!("failed to emit session-updated: {err}");
        }
    }) {
        Ok((session, _)) => {
            dispatch_callout(&state.speaker, &event, &session);
            if let Some(overlay) = &state.overlay {
                overlay.on_event(&event, &session);
            }
            StatusCode::ACCEPTED
        }
        Err(err) => {
            log::error!("registry failed to apply event: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Cleanup that also runs when the parked handler future is CANCELLED (the
/// shim's connection died mid-wait: user hit Esc, closed the terminal, agent
/// crashed). Without it the pinned card would be stranded — clickable, on
/// top of the screen, forever. Both calls are idempotent, so running after a
/// normal decision/timeout is harmless.
struct ApprovalCleanup {
    id: String,
    broker: Arc<Broker>,
    overlay: Option<Arc<Overlay>>,
}

impl Drop for ApprovalCleanup {
    fn drop(&mut self) {
        self.broker.expire(&self.id);
        if let Some(overlay) = &self.overlay {
            overlay.unpin_approval(&self.id);
        }
    }
}

/// Blocking long-poll for PreToolUse (§4, FR-6). Registers the request with
/// the broker, pins the overlay card, announces it (preempting the speaker
/// queue), and parks until the user decides or the timeout hits (→ 204, and
/// Claude Code falls back to its own terminal prompt — AC-6.3).
async fn post_approval(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let event: NormalizedEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(err) => {
            log::warn!("dropping malformed approval payload: {err}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let Some(tool) = &event.tool else {
        log::warn!("approval request without tool payload dropped");
        return StatusCode::BAD_REQUEST.into_response();
    };

    // Voice-only degraded mode (overlay failed to init): there is no way to
    // decide, so parking would just stall the agent for 110 s. Fall straight
    // through to Claude Code's own prompt.
    if state.overlay.is_none() {
        log::info!("approval skipped: overlay unavailable, deferring to terminal");
        return StatusCode::NO_CONTENT.into_response();
    }

    // Record the permission_request in the registry (state → NeedsAttention).
    let (session, event_id) = match state.registry.apply(&event, |snapshot| {
        if let Err(err) = state.app.emit("session-updated", snapshot) {
            log::warn!("failed to emit session-updated: {err}");
        }
    }) {
        Ok(applied) => applied,
        Err(err) => {
            log::error!("registry failed to apply approval event: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let (info, rx) = state.broker.register(ApprovalInfo {
        id: String::new(),
        session_id: session.id.clone(),
        event_id,
        agent: event.agent,
        title: session.title.clone(),
        tool_name: tool.name.clone(),
        tool_summary: tool_summary(&tool.name, &tool.input),
    });
    let _cleanup = ApprovalCleanup {
        id: info.id.clone(),
        broker: state.broker.clone(),
        overlay: state.overlay.clone(),
    };
    log::info!(
        "approval {} pending: {} in session {}",
        info.id,
        info.tool_name,
        info.session_id
    );

    if let Some(overlay) = &state.overlay {
        overlay.pin_approval(info.clone());
    }
    // Preempting announcement on insert (§6).
    state.speaker.enqueue(Utterance {
        priority: Priority::Approval,
        session_id: session.id.clone(),
        agent: event.agent,
        text: speaker::approval_request_text(event.agent, &info.title, &info.tool_name),
    });

    let decision = park_until(rx, approval_timeout(), || {
        state.broker.expire(&info.id).is_some()
    })
    .await;

    match decision {
        Some(decision) => {
            log::info!("approval {} decided: {}", info.id, decision.as_str());
            axum::Json(serde_json::json!({ "decision": decision.as_str() })).into_response()
        }
        None => {
            log::info!("approval {} timed out; falling back to terminal", info.id);
            if let Some(overlay) = &state.overlay {
                overlay.unpin_approval(&info.id);
            }
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

// ─── AskUserQuestion ────────────────────────────────────────────────────────
//
// A sibling of the approval path: the shim parks an `AskUserQuestion`
// PreToolUse call here so the user can answer from the overlay instead of the
// TUI selector. Unlike approvals this ignores the `gate_tool_calls` flag —
// the agent is already blocked waiting for a human, so there is no latency to
// protect (decided in the shim, see run_question).

/// Caps on the questions we accept from a hook payload. Claude's own limits
/// are ~4 questions × 2–4 options; anything wildly past that is junk, and
/// junk must fall back to the terminal rather than render a monster card.
const MAX_QUESTIONS: usize = 8;
const MAX_OPTIONS: usize = 12;
const MAX_QUESTION_CHARS: usize = 500;
const MAX_HEADER_CHARS: usize = 80;
const MAX_LABEL_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 500;

/// Guards the park against there being no way to answer: without a card,
/// every AskUserQuestion would stall for 110 s before falling through. True
/// since `Overlay::pin_question` and the Svelte question card exist.
const QUESTION_UI_READY: bool = true;

/// The question-specific half of a `/v1/question` body (the rest is a plain
/// normalized event).
#[derive(serde::Deserialize)]
struct QuestionPayload {
    #[serde(default)]
    tool_use_id: Option<String>,
    questions: Vec<QuestionSpec>,
}

/// Parse and sanity-check a `/v1/question` body. `Err` → 400 (AC-1.3: bad
/// input is rejected, never fatal). Strings are trimmed and capped and
/// over-long option lists truncated, but a structurally wrong or unanswerable
/// payload is refused outright so the shim falls open to the TUI selector.
fn parse_question_body(
    body: &[u8],
) -> Result<(NormalizedEvent, Vec<QuestionSpec>, Option<String>), String> {
    let event: NormalizedEvent =
        serde_json::from_slice(body).map_err(|e| format!("event fields: {e}"))?;
    let payload: QuestionPayload =
        serde_json::from_slice(body).map_err(|e| format!("question fields: {e}"))?;

    if payload.questions.is_empty() {
        return Err("no questions in payload".into());
    }
    if payload.questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "{} questions exceeds the {MAX_QUESTIONS} cap",
            payload.questions.len()
        ));
    }

    let mut questions = Vec::with_capacity(payload.questions.len());
    for mut q in payload.questions {
        q.question = cap(&q.question, MAX_QUESTION_CHARS);
        if q.question.is_empty() {
            return Err("question with empty text".into());
        }
        q.header = cap(&q.header, MAX_HEADER_CHARS);
        // Drop blanks BEFORE capping the count: truncating first could keep
        // twelve empty labels and leave a card with nothing to click.
        q.options.retain(|o| !o.label.trim().is_empty());
        q.options.truncate(MAX_OPTIONS);
        for o in &mut q.options {
            o.label = cap(&o.label, MAX_LABEL_CHARS);
            o.description = cap(&o.description, MAX_DESCRIPTION_CHARS);
        }
        if q.options.is_empty() {
            // Claude always offers 2–4 options; none means a drifted or junk
            // payload. Refuse rather than park an unanswerable card.
            return Err("question with no usable options".into());
        }
        questions.push(q);
    }

    let tool_use_id = payload
        .tool_use_id
        .map(|id| cap(&id, 128))
        .filter(|id| !id.is_empty());
    Ok((event, questions, tool_use_id))
}

fn cap(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/// Cleanup that also runs when the parked handler future is CANCELLED (shim
/// connection death: Esc, closed terminal, `kill -9`). Same guarantee the
/// approval path already needed — a pinned question card must never outlive
/// the request that created it. Idempotent.
struct QuestionCleanup {
    id: String,
    /// None only in unit tests, which have no Tauri app to reach.
    app: Option<AppHandle>,
    broker: Arc<Broker>,
    overlay: Option<Arc<Overlay>>,
}

impl Drop for QuestionCleanup {
    fn drop(&mut self) {
        self.broker.expire_question(&self.id);
        // A question that stops being answerable must not leave a focused
        // reply window floating over everything, silently eating whatever the
        // user was still typing into it.
        if let Some(app) = &self.app {
            crate::reply::close_for(app, &self.id);
        }
        if let Some(overlay) = &self.overlay {
            overlay.unpin_question(&self.id);
        }
    }
}

/// Blocking long-poll for an `AskUserQuestion` PreToolUse call. Registers the
/// question with the broker, marks the session as needing attention, pins the
/// card, announces it, and parks until the user answers (200 + answers), taps
/// "answer in terminal" (204), or the timeout hits (204 → the TUI selector
/// renders as if PingMyBell were not installed).
async fn post_question(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (mut event, questions, tool_use_id) = match parse_question_body(&body) {
        Ok(parsed) => parsed,
        Err(err) => {
            log::warn!("dropping malformed question payload: {err}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // No card, no answer: never park a request nobody can resolve.
    if !QUESTION_UI_READY || state.overlay.is_none() {
        log::info!("question skipped: no overlay card available, deferring to terminal");
        return StatusCode::NO_CONTENT.into_response();
    }

    // The question text is what the board/history and the callout show; clean
    // it like any other summary (§9.4 — bounded, derived data only).
    event.summary = event
        .summary
        .take()
        .and_then(|raw| non_empty(summarize::clean(&raw)))
        .or_else(|| non_empty(summarize::clean(&questions[0].question)));
    let spoken = event.summary.clone().unwrap_or_default();

    let (session, event_id) = match state.registry.apply(&event, |snapshot| {
        if let Err(err) = state.app.emit("session-updated", snapshot) {
            log::warn!("failed to emit session-updated: {err}");
        }
    }) {
        Ok(applied) => applied,
        Err(err) => {
            log::error!("registry failed to apply question event: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let (info, rx) = state.broker.register_question(QuestionInfo {
        id: String::new(),
        session_id: session.id.clone(),
        event_id,
        agent: event.agent,
        title: session.title.clone(),
        tool_use_id,
        questions,
    });
    let _cleanup = QuestionCleanup {
        id: info.id.clone(),
        app: Some(state.app.clone()),
        broker: state.broker.clone(),
        overlay: state.overlay.clone(),
    };
    log::info!(
        "question {} pending: {} question(s) in session {}",
        info.id,
        info.questions.len(),
        info.session_id
    );

    if let Some(overlay) = &state.overlay {
        overlay.pin_question(info.clone());
    }
    // Preempting announcement, same priority as an approval: the agent is
    // blocked until this is answered.
    state.speaker.enqueue(Utterance {
        priority: Priority::Approval,
        session_id: session.id.clone(),
        agent: event.agent,
        text: speaker::attention_text(event.agent, &info.title, &spoken),
    });

    let outcome = park_until(rx, approval_timeout(), || {
        state.broker.expire_question(&info.id).is_some()
    })
    .await;

    match outcome {
        Some(QuestionOutcome::Answered(answer)) => {
            log::info!(
                "question {} answered ({} of {} question(s))",
                info.id,
                answer.answers.len(),
                info.questions.len()
            );
            axum::Json(answer).into_response()
        }
        Some(QuestionOutcome::Deferred) | None => {
            log::info!("question {} unanswered; falling back to terminal", info.id);
            if let Some(overlay) = &state.overlay {
                overlay.unpin_question(&info.id);
            }
            crate::reply::close_for(&state.app, &info.id);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Park on a broker receiver until the UI resolves it or the timeout fires.
///
/// `expire` runs when the timer wins and must report whether the entry was
/// still pending (i.e. WE won). If the UI grabbed it in the race window its
/// send is already on the way, so we wait one grace period rather than
/// throwing away a click the user already made. Shared by the approval and
/// question paths so the subtle part exists exactly once.
async fn park_until<T>(
    rx: tokio::sync::oneshot::Receiver<T>,
    timeout: std::time::Duration,
    expire: impl FnOnce() -> bool,
) -> Option<T> {
    let mut rx = rx;
    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    tokio::select! {
        res = &mut rx => res.ok(),
        _ = &mut sleep => {
            if expire() {
                None
            } else {
                (tokio::time::timeout(RACE_GRACE, &mut rx).await)
                    .ok()
                    .and_then(Result::ok)
            }
        }
    }
}

/// The primary input worth showing for a tool call (AC-6.1), truncated for
/// the card. Derived data — never logged (§9).
fn tool_summary(tool_name: &str, input: &serde_json::Value) -> String {
    let primary = match tool_name {
        "Bash" => input["command"].as_str().unwrap_or_default().to_string(),
        "Write" | "Edit" | "MultiEdit" => {
            input["file_path"].as_str().unwrap_or_default().to_string()
        }
        _ => serde_json::to_string(input).unwrap_or_default(),
    };
    let mut out: String = primary.chars().take(160).collect();
    if out.len() < primary.len() {
        out.push('…');
    }
    out
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// Turn a registry event into a voice callout (AC-2.2, AC-2.3). The summary
/// was already cleaned at ingress; it is derived data — never logged (§9).
fn dispatch_callout(speaker: &SpeakerHandle, event: &NormalizedEvent, session: &Session) {
    let summary = event.summary.as_deref().unwrap_or_default();
    let (priority, text) = match event.event {
        EventKind::TurnComplete => (
            Priority::Completion,
            speaker::completion_text(event.agent, &session.title, summary),
        ),
        EventKind::NeedsAttention | EventKind::PermissionRequest => (
            Priority::Attention,
            speaker::attention_text(event.agent, &session.title, summary),
        ),
        EventKind::SessionStart | EventKind::SessionEnd => return,
    };
    speaker.enqueue(Utterance {
        priority,
        session_id: session.id.clone(),
        agent: event.agent,
        text,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{Answer, AnswerResult, QuestionAnswer, QuestionInfo};

    /// The verified AskUserQuestion PreToolUse payload (claude 2.1.198),
    /// wrapped in the normalized-event envelope the shim sends.
    fn question_body(questions: serde_json::Value) -> Vec<u8> {
        serde_json::json!({
            "agent": "claude-code",
            "event": "needs_attention",
            "session_id": "4018aba2-7f8c-4733-bcfa-d1ab7e41033c",
            "cwd": "/tmp/ask-spike",
            "summary": "Do you prefer option A (fast) or option B (thorough)?",
            "transcript_path": "/tmp/t.jsonl",
            "tool": null,
            "terminal": null,
            "tool_use_id": "toolu_01Cvj9QuwXtaGg2Yezug3EQ1",
            "questions": questions,
        })
        .to_string()
        .into_bytes()
    }

    fn one_question() -> serde_json::Value {
        serde_json::json!([{
            "question": "Do you prefer option A (fast) or option B (thorough)?",
            "header": "Approach",
            "options": [
                {"label": "Option A (fast)", "description": "Quicker turnaround, less depth."},
                {"label": "Option B (thorough)", "description": "Slower but more comprehensive."}
            ],
            "multiSelect": false
        }])
    }

    #[test]
    fn parses_the_verified_payload() {
        let (event, questions, tool_use_id) = parse_question_body(&question_body(one_question()))
            .expect("verified payload must parse");
        assert_eq!(event.agent, AgentKind::ClaudeCode);
        assert_eq!(event.event, EventKind::NeedsAttention);
        assert_eq!(event.session_id, "4018aba2-7f8c-4733-bcfa-d1ab7e41033c");
        assert_eq!(
            tool_use_id.as_deref(),
            Some("toolu_01Cvj9QuwXtaGg2Yezug3EQ1")
        );
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].header, "Approach");
        assert_eq!(questions[0].options.len(), 2);
        assert!(!questions[0].multi_select);
    }

    #[test]
    fn rejects_schema_invalid_bodies() {
        // Not JSON / not an event at all.
        assert!(parse_question_body(b"not json").is_err());
        assert!(parse_question_body(b"{}").is_err());
        // Event envelope fine, questions missing or empty.
        assert!(parse_question_body(&question_body(serde_json::json!([]))).is_err());
        assert!(parse_question_body(&question_body(serde_json::json!("nope"))).is_err());
        // Question object without the required text.
        assert!(parse_question_body(&question_body(serde_json::json!([{"header": "h"}]))).is_err());
        assert!(parse_question_body(&question_body(
            serde_json::json!([{"question": "   ", "options": [{"label": "a"}]}])
        ))
        .is_err());
        // A question nobody can answer (no options, or only blank ones) falls
        // back to the terminal instead of parking a dead card.
        assert!(
            parse_question_body(&question_body(serde_json::json!([{"question": "q?"}]))).is_err()
        );
        assert!(parse_question_body(&question_body(serde_json::json!([
            {"question": "q?", "options": [{"label": "  "}, {"label": ""}]}
        ])))
        .is_err());
        // Absurd question count → terminal fallback rather than a monster card.
        let many: Vec<_> = (0..MAX_QUESTIONS + 1)
            .map(|i| serde_json::json!({"question": format!("q{i}"), "options": [{"label": "a"}]}))
            .collect();
        assert!(parse_question_body(&question_body(serde_json::json!(many))).is_err());
    }

    #[test]
    fn caps_and_trims_hostile_strings() {
        let long = "x".repeat(5000);
        let mut options: Vec<_> = (0..MAX_OPTIONS)
            .map(|_| serde_json::json!({"label": "   ", "description": ""}))
            .collect();
        options.extend(
            (0..40).map(
                |i| serde_json::json!({"label": format!("opt{i}"), "description": long.clone()}),
            ),
        );
        let body = question_body(serde_json::json!([{
            "question": long.clone(),
            "header": long.clone(),
            "options": options,
            "multiSelect": true
        }]));
        let (_, questions, _) = parse_question_body(&body).unwrap();
        assert_eq!(questions[0].question.chars().count(), MAX_QUESTION_CHARS);
        assert_eq!(questions[0].header.chars().count(), MAX_HEADER_CHARS);
        assert_eq!(questions[0].options.len(), MAX_OPTIONS);
        assert_eq!(
            questions[0].options[0].label, "opt0",
            "blank options are dropped before the count cap, not after"
        );
        assert_eq!(
            questions[0].options[0].description.chars().count(),
            MAX_DESCRIPTION_CHARS
        );
        assert!(questions[0].multi_select);
    }

    #[test]
    fn missing_optional_fields_are_tolerated() {
        // No header, no descriptions, no multiSelect, no tool_use_id.
        let mut body: serde_json::Value =
            serde_json::from_slice(&question_body(serde_json::json!([{
                "question": "Ship it?",
                "options": [{"label": "Yes"}, {"label": "No"}]
            }])))
            .unwrap();
        body.as_object_mut().unwrap().remove("tool_use_id");
        let (_, questions, tool_use_id) = parse_question_body(body.to_string().as_bytes()).unwrap();
        assert!(tool_use_id.is_none());
        assert_eq!(questions[0].header, "");
        assert!(!questions[0].multi_select);
        assert_eq!(questions[0].options[0].description, "");
    }

    fn parked_question() -> QuestionInfo {
        QuestionInfo {
            id: String::new(),
            session_id: "s1".into(),
            event_id: 1,
            agent: AgentKind::ClaudeCode,
            title: "ask-spike".into(),
            tool_use_id: None,
            questions: parse_question_body(&question_body(one_question()))
                .unwrap()
                .1,
        }
    }

    #[tokio::test]
    async fn parked_question_times_out_into_terminal_fallback() {
        let broker = Arc::new(Broker::default());
        let (info, rx) = broker.register_question(parked_question());

        // Same code path as the 110 s park, compressed for the test.
        let outcome = park_until(rx, std::time::Duration::from_millis(20), || {
            broker.expire_question(&info.id).is_some()
        })
        .await;
        assert!(outcome.is_none(), "timeout must yield 204, not an answer");
        assert!(
            !broker.has_pending_for_session("s1"),
            "the timed-out question is gone from the broker"
        );
    }

    #[tokio::test]
    async fn answered_question_resolves_the_park() {
        let broker = Arc::new(Broker::default());
        let (info, rx) = broker.register_question(parked_question());

        let answering = {
            let broker = broker.clone();
            let id = info.id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                broker.answer(
                    &id,
                    QuestionAnswer {
                        answers: vec![Answer {
                            question_index: 0,
                            labels: vec!["Option B (thorough)".into()],
                            free_text: None,
                        }],
                    },
                )
            })
        };
        let outcome = park_until(rx, std::time::Duration::from_secs(5), || {
            broker.expire_question(&info.id).is_some()
        })
        .await;
        assert!(matches!(
            answering.await.unwrap(),
            AnswerResult::Accepted(_)
        ));
        match outcome {
            Some(QuestionOutcome::Answered(answer)) => {
                assert_eq!(answer.answers[0].labels, vec!["Option B (thorough)"]);
                // This is exactly the 200 body the shim parses.
                assert_eq!(
                    serde_json::to_string(&answer).unwrap(),
                    r#"{"answers":[{"question_index":0,"labels":["Option B (thorough)"],"free_text":null}]}"#
                );
            }
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deferred_question_falls_through_immediately() {
        let broker = Arc::new(Broker::default());
        let (info, rx) = broker.register_question(parked_question());
        assert!(broker.defer_question(&info.id).is_some());

        let outcome = park_until(rx, std::time::Duration::from_secs(5), || {
            broker.expire_question(&info.id).is_some()
        })
        .await;
        assert_eq!(outcome, Some(QuestionOutcome::Deferred));
    }

    #[tokio::test]
    async fn cleanup_guard_expires_a_park_abandoned_by_the_shim() {
        // Simulates `kill -9` on the agent: the handler future is dropped
        // mid-park, so the guard must free the broker entry (and, once wired,
        // unpin the card) instead of stranding it.
        let broker = Arc::new(Broker::default());
        let (info, _rx) = broker.register_question(parked_question());
        {
            let _cleanup = QuestionCleanup {
                id: info.id.clone(),
                app: None,
                broker: broker.clone(),
                overlay: None,
            };
        }
        assert!(!broker.has_pending_for_session("s1"));
        assert!(broker.expire_question(&info.id).is_none());
    }
}
