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

use crate::registry::{NormalizedEvent, Registry};

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
}

pub async fn serve(
    app: AppHandle,
    registry: Arc<Registry>,
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

    let event: NormalizedEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(err) => {
            log::warn!("dropping malformed event payload: {err}");
            return StatusCode::BAD_REQUEST;
        }
    };

    // The emit happens inside apply's notify callback, under the registry
    // lock, so concurrent events for one session reach the UI in order.
    match state.registry.apply(&event, |snapshot| {
        if let Err(err) = state.app.emit("session-updated", snapshot) {
            log::warn!("failed to emit session-updated: {err}");
        }
    }) {
        Ok(_) => StatusCode::ACCEPTED,
        Err(err) => {
            log::error!("registry failed to apply event: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
