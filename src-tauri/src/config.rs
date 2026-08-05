//! Tiny shared config at `~/.pingmybell/config.json`, read by the shim on
//! every hook invocation and written by the app (tray toggles now, settings
//! UI in step 7). Only additive keys — the shim treats missing/unknown as
//! defaults, so old shims and new apps stay compatible.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// Serializes the read-modify-write that every setter below performs.
///
/// Each one loads the whole file, changes one key and writes it back, so two
/// landing at once would lose one of the changes — and they genuinely can:
/// the tray menu handlers and the settings window's Tauri commands run on
/// different threads. Poisoning is ignored because the guarded value is `()`;
/// there is no state a panicking writer could have corrupted.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Load, edit, and persist the config as one atomic step. `edit` returns
/// whether it changed anything, so a no-op does not rewrite the file.
fn update(what: &str, edit: impl FnOnce(&mut Value) -> bool) {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut config = load();
    if !edit(&mut config) {
        return;
    }
    if let Err(err) = store(&config) {
        log::error!("could not persist {what}: {err}");
    }
}

/// Write the config as a whole file or not at all.
///
/// The obvious `fs::write` truncates first, and `load` reads anything it
/// cannot parse as "no settings at all" — so a crash or power loss in that
/// window silently drops the voice map and both gate settings on next launch,
/// against AC-9.2 ("settings survive restart"). The shim re-reads this file on
/// every hook invocation, so it can catch a half-written one mid-flight too.
/// Writing a sibling temp file and renaming over the target means a reader
/// only ever sees the old bytes or the new ones.
fn store(config: &Value) -> io::Result<()> {
    let path = config_path()?;
    // Per-process temp name: `WRITE_LOCK` only orders writers inside this
    // process, and a second copy of the app must not scribble on ours.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    write_atomic(&tmp, &path, &serde_json::to_string_pretty(config)?)
}

/// Split out from [`store`] so tests can exercise it without writing to the
/// developer's real `~/.pingmybell`.
fn write_atomic(tmp: &Path, path: &Path, contents: &str) -> io::Result<()> {
    write_then_rename(tmp, path, contents).inspect_err(|_| {
        // A write that failed anywhere past `File::create` must not leave a
        // stray `config.json.tmp.<pid>` sitting in ~/.pingmybell.
        let _ = std::fs::remove_file(tmp);
    })
}

fn write_then_rename(tmp: &Path, path: &Path, contents: &str) -> io::Result<()> {
    let mut file = std::fs::File::create(tmp)?;
    // Mode on the TEMP file, before anyone can reach it under the real name.
    // Setting it after the write (what `fs::write` + `set_permissions` did)
    // leaves the config world-readable for the window in between, against §9
    // invariant 1.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents.as_bytes())?;
    // The rename is atomic, but it only orders against bytes that already
    // reached the disk. Without this a power loss can leave the rename applied
    // and the contents still in the page cache — an atomically empty config.
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)?;
    // The rename is only as durable as the directory entry that records it.
    // Best-effort and unix-only: Windows has no directory handle to sync, and
    // losing this costs at worst the PREVIOUS settings coming back after a
    // power cut — never a torn file, which is what this is all for.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let _ = std::fs::File::open(dir).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

/// Ensure the file exists with defaults so users can discover it.
pub fn ensure_defaults() {
    update("config defaults", |config| {
        let mut changed = false;
        if config.get("gate_tool_calls").is_none() {
            config["gate_tool_calls"] = json!(false);
            changed = true;
        }
        if config.get("gate_codex_approvals").is_none() {
            config["gate_codex_approvals"] = json!(CodexGate::default().as_str());
            changed = true;
        }
        changed
    });
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
    update("gate_codex_approvals", |config| {
        config["gate_codex_approvals"] = json!(gate.as_str());
        true
    });
}

/// User-chosen voice for an agent ("claude-code" / "codex"), by system
/// voice name (AC-4.2). None → the built-in distinct defaults.
pub fn voice_for(agent: &str) -> Option<String> {
    load()["voices"][agent].as_str().map(str::to_string)
}

pub fn set_voice(agent: &str, voice: &str) {
    update("voice choice", |config| {
        if !config["voices"].is_object() {
            config["voices"] = json!({});
        }
        config["voices"][agent] = json!(voice);
        true
    });
}

/// The triage chord (§12.2), from `hotkey.next`. Empty or absent → the
/// built-in default; the string is handed to Tauri's parser, which is the
/// only thing that can say whether it is valid, and a chord it rejects is
/// reported in the board's settings rather than crashing the app.
///
/// Deliberately read-only here: there is no picker to write it back yet, and
/// a key the app never writes is one an app update can never clobber.
pub fn hotkey_next() -> String {
    load()["hotkey"]["next"]
        .as_str()
        .map(str::trim)
        .filter(|chord| !chord.is_empty())
        .unwrap_or(crate::triage::DEFAULT_CHORD)
        .to_string()
}

pub fn set_gate_tool_calls(enabled: bool) {
    update("gate_tool_calls", |config| {
        config["gate_tool_calls"] = json!(enabled);
        true
    });
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

    /// Writing settings must never be able to destroy the ones already there.
    /// The old truncate-then-write could leave an empty file behind a crash,
    /// and `load` reads that as "no settings at all".
    #[test]
    fn a_config_write_replaces_the_old_one_whole_and_stays_user_only() {
        let dir = std::env::temp_dir().join(format!("pmb-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let tmp = dir.join("config.json.tmp");

        write_atomic(&tmp, &path, r#"{"gate_tool_calls":false}"#).unwrap();
        // A second, longer write must not leave any of the first one behind.
        let second = serde_json::to_string_pretty(&json!({
            "gate_tool_calls": true,
            "voices": { "codex": "Daniel" },
        }))
        .unwrap();
        write_atomic(&tmp, &path, &second).unwrap();

        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read, second, "the file is exactly the last write");
        let parsed: Value = serde_json::from_str(&read).unwrap();
        assert_eq!(parsed["voices"]["codex"], json!("Daniel"));
        assert!(!tmp.exists(), "the temp file must not be left behind");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "§9 invariant 1: config is user-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed write leaves the previous settings intact — the whole point of
    /// renaming into place rather than writing over the target — and takes its
    /// temp file with it.
    #[test]
    fn a_failed_write_leaves_the_previous_config_untouched() {
        let dir = std::env::temp_dir().join(format!("pmb-cfg-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"gate_tool_calls":true}"#).unwrap();
        let tmp = dir.join("config.json.tmp");

        // Renaming onto a directory fails, so the failure lands AFTER the temp
        // file was created and written — the case the cleanup exists for.
        let blocked = dir.join("not-a-file");
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_atomic(&tmp, &blocked, "{}").is_err());
        assert!(!tmp.exists(), "a failed write must not strand its temp file");

        // A temp path that cannot even be created (its parent is missing).
        let doomed = dir.join("missing").join("config.json.tmp");
        assert!(write_atomic(&doomed, &path, "{}").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"gate_tool_calls":true}"#,
            "the settings already on disk survive a failed write"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_gate_round_trips_through_its_string() {
        for gate in [CodexGate::Auto, CodexGate::Always, CodexGate::Never] {
            assert_eq!(parse_codex_gate(&json!(gate.as_str())), gate);
        }
        assert_eq!(CodexGate::default(), CodexGate::Auto);
    }
}
