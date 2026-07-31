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

use crate::overlay::Overlay;
use crate::registry::{AgentKind, EventKind, NormalizedEvent, Registry, Session};
use crate::speaker::{self, Priority, SpeakerHandle, Utterance};
use crate::{adapters, summarize};

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
}

pub async fn serve(
    app: AppHandle,
    registry: Arc<Registry>,
    speaker: SpeakerHandle,
    overlay: Option<Arc<Overlay>>,
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
    });
    let router = Router::new()
        .route("/v1/event", post(post_event))
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
        Ok(session) => {
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
