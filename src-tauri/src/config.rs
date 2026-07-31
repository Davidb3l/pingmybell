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
    if config.get("gate_tool_calls").is_none() {
        config["gate_tool_calls"] = json!(false);
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

pub fn set_gate_tool_calls(enabled: bool) {
    let mut config = load();
    config["gate_tool_calls"] = json!(enabled);
    if let Err(err) = store(&config) {
        log::error!("could not persist gate_tool_calls: {err}");
    }
}
