//! Tiny shared config at `~/.pingmybell/config.json`, read by the shim on
//! every hook invocation and written by the app (tray toggles now, settings
//! UI in step 7). Only additive keys — the shim treats missing/unknown as
//! defaults, so old shims and new apps stay compatible.

use std::io;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::ingest::data_dir;

fn config_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join("config.json"))
}

fn load() -> Value {
    config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn store(config: &Value) -> io::Result<()> {
    let path = config_path()?;
    std::fs::write(&path, serde_json::to_string_pretty(config)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Ensure the file exists with defaults so users can discover it.
pub fn ensure_defaults() {
    let mut config = load();
    let mut changed = false;
    if config.get("gate_tool_calls").is_none() {
        config["gate_tool_calls"] = json!(false);
        changed = true;
    }
    if config.get("gate_codex_approvals").is_none() {
        config["gate_codex_approvals"] = json!(true);
        changed = true;
    }
    if changed {
        if let Err(err) = store(&config) {
            log::warn!("could not write config defaults: {err}");
        }
    }
}

/// Opt-in: park PreToolUse calls for overlay approval (FR-6). Defaults to
/// false — in auto workflows tool calls must flow with zero latency.
pub fn gate_tool_calls() -> bool {
    load()["gate_tool_calls"].as_bool().unwrap_or(false)
}

/// Answer Codex `PermissionRequest` hooks from the overlay. SEPARATE from
/// `gate_tool_calls` and defaulting to TRUE: Codex has already stopped and is
/// waiting for a human when this fires, so there is no latency to protect —
/// unlike Claude PreToolUse gating, which inserts a wait into calls that were
/// about to run unattended.
pub fn gate_codex_approvals() -> bool {
    load()["gate_codex_approvals"].as_bool().unwrap_or(true)
}

pub fn set_gate_codex_approvals(enabled: bool) {
    let mut config = load();
    config["gate_codex_approvals"] = json!(enabled);
    if let Err(err) = store(&config) {
        log::error!("could not persist gate_codex_approvals: {err}");
    }
}

/// User-chosen voice for an agent ("claude-code" / "codex"), by system
/// voice name (AC-4.2). None → the built-in distinct defaults.
pub fn voice_for(agent: &str) -> Option<String> {
    load()["voices"][agent].as_str().map(str::to_string)
}

pub fn set_voice(agent: &str, voice: &str) {
    let mut config = load();
    if !config["voices"].is_object() {
        config["voices"] = json!({});
    }
    config["voices"][agent] = json!(voice);
    if let Err(err) = store(&config) {
        log::error!("could not persist voice choice: {err}");
    }
}

pub fn set_gate_tool_calls(enabled: bool) {
    let mut config = load();
    config["gate_tool_calls"] = json!(enabled);
    if let Err(err) = store(&config) {
        log::error!("could not persist gate_tool_calls: {err}");
    }
}
