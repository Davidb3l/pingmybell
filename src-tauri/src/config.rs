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
        config["gate_codex_approvals"] = json!(CodexGate::default().as_str());
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

/// When to answer Codex `PermissionRequest` hooks from the overlay. SEPARATE
/// from `gate_tool_calls` (that one is about Claude's PreToolUse latency) and
/// deliberately NOT a boolean: relocating an approval out of the agent — where
/// the surrounding context is — onto a card that must be cleared is only an
/// improvement for a user who has already told Codex to ask them about
/// everything. So the default MIRRORS the user's own Codex approval setting
/// instead of overriding it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodexGate {
    /// Intercept only when Codex itself is set to "Ask for approval" — i.e.
    /// the user opted into being asked about everything (§5.2.3).
    #[default]
    Auto,
    /// Always intercept (the old `gate_codex_approvals: true`).
    Always,
    /// Never intercept (the old `gate_codex_approvals: false`).
    Never,
}

impl CodexGate {
    pub fn as_str(self) -> &'static str {
        match self {
            CodexGate::Auto => "auto",
            CodexGate::Always => "always",
            CodexGate::Never => "never",
        }
    }
}

/// Parse the stored `gate_codex_approvals` value.
///
/// Backwards compatible on purpose: this key shipped as a bool, so an existing
/// config keeps working unchanged — `true` means `always`, `false` means
/// `never`. Absent (or anything unrecognized) falls back to the default,
/// `auto`. The shim carries a byte-identical copy of this mapping; both are
/// unit-tested against the same table.
pub fn parse_codex_gate(value: &Value) -> CodexGate {
    match value {
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

pub fn codex_gate() -> CodexGate {
    parse_codex_gate(&load()["gate_codex_approvals"])
}

pub fn set_codex_gate(gate: CodexGate) {
    let mut config = load();
    config["gate_codex_approvals"] = json!(gate.as_str());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shim has an independent copy of this mapping (it cannot link the
    /// app crate). `shim::codex_gate_from` is tested against the same table;
    /// if you change one, change both.
    #[test]
    fn codex_gate_parses_every_shape() {
        for (raw, want) in [
            // The three real settings.
            (json!("auto"), CodexGate::Auto),
            (json!("always"), CodexGate::Always),
            (json!("never"), CodexGate::Never),
            // Legacy booleans — the key shipped as a bool and live configs
            // still hold one. `false` (what the user's config has today) must
            // keep meaning "never".
            (json!(true), CodexGate::Always),
            (json!(false), CodexGate::Never),
            // Tolerated spellings.
            (json!("Always"), CodexGate::Always),
            (json!(" never "), CodexGate::Never),
            // Absent / unrecognized → the default.
            (Value::Null, CodexGate::Auto),
            (json!("wat"), CodexGate::Auto),
            (json!(3), CodexGate::Auto),
            (json!({}), CodexGate::Auto),
        ] {
            assert_eq!(parse_codex_gate(&raw), want, "{raw}");
        }
    }

    #[test]
    fn codex_gate_round_trips_through_its_string() {
        for gate in [CodexGate::Auto, CodexGate::Always, CodexGate::Never] {
            assert_eq!(parse_codex_gate(&json!(gate.as_str())), gate);
        }
        assert_eq!(CodexGate::default(), CodexGate::Auto);
    }
}
