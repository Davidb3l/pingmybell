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
    load_checked().unwrap_or_else(|_| json!({}))
}

/// The config as parsed, or `Err` when the file EXISTS and could not be read
/// as a settings object.
///
/// Readers treat that as "no settings" and carry on with defaults, which is
/// right. Writers must not: a setter loads, edits one key and stores the
/// result, so writing after a failed parse replaces a file we could not
/// understand with `{}` plus that one key — silently destroying every voice,
/// gate and speech setting the user had. Sliders write often, which is what
/// turned a theoretical hazard into a likely one.
fn load_checked() -> Result<Value, ()> {
    let path = config_path().map_err(|_| ())?;
    read_config(&path)
}

/// Split out from [`load_checked`] so it can be tested against a file that is
/// not the developer's real `~/.pingmybell/config.json`.
fn read_config(path: &Path) -> Result<Value, ()> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        // No file yet is not a failure: that is a fresh install.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(_) => return Err(()),
    };
    serde_json::from_str(&raw)
        .ok()
        .filter(Value::is_object)
        .ok_or(())
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
    let Ok(mut config) = load_checked() else {
        log::error!(
            "refusing to write {what}: ~/.pingmybell/config.json is unreadable \
             or not a JSON object — fix or remove it and the setting will save"
        );
        return;
    };
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

/// How rate is expressed to the user and stored: a MULTIPLE of the backend's
/// normal speaking rate, so 1.5 means half again as fast whatever the engine's
/// own scale happens to be (this machine's AVFoundation backend reports
/// min 0.1 / normal 0.5 / max 2.0; Windows differs).
const RATE_MIN: f64 = 0.5;
const RATE_MAX: f64 = 2.0;

/// Everything the speaker needs for one utterance, from ONE read of the file.
pub struct SpeechSettings {
    pub voice: Option<String>,
    pub rate: f64,
    pub volume: f64,
}

/// The per-agent speech settings, read once.
///
/// The worker used to call `voice_for` + `speech_rate` + `speech_volume`, and
/// each of those re-reads and re-parses `config.json` — three loads per
/// spoken line for three keys of the same object.
pub fn speech_settings(agent: &str) -> SpeechSettings {
    let config = load();
    SpeechSettings {
        voice: config["voices"][agent].as_str().map(str::to_string),
        rate: clamped(&config["speech"][agent]["rate"], RATE_MIN, RATE_MAX, 1.0),
        volume: clamped(&config["speech"][agent]["volume"], 0.0, 1.0, 1.0),
    }
}

/// Callout shape (AC-4.3), from `speech.style`. Global: someone who wants
/// short lines wants them from every agent.
pub fn speech_style() -> crate::speaker::Style {
    load()["speech"]["style"]
        .as_str()
        .map(crate::speaker::Style::parse)
        .unwrap_or_default()
}

pub fn set_speech_style(style: crate::speaker::Style) {
    update("speech style", |config| {
        ensure_speech(config)["style"] = json!(style.as_str());
        true
    });
}

/// Speaking rate for one agent, as a multiple of normal (AC-4.2).
///
/// Clamped at READ time, not only at write: the file is user-editable and a
/// hand-typed `40` must not produce an utterance nobody can understand, or a
/// `0` that never finishes.
pub fn speech_rate(agent: &str) -> f64 {
    clamped(&load()["speech"][agent]["rate"], RATE_MIN, RATE_MAX, 1.0)
}

pub fn set_speech_rate(agent: &str, rate: f64) {
    let rate = rate.clamp(RATE_MIN, RATE_MAX);
    update("speech rate", |config| {
        ensure_agent(config, agent)["rate"] = json!(rate);
        true
    });
}

/// Volume for one agent as a fraction of the backend's normal (AC-4.2).
pub fn speech_volume(agent: &str) -> f64 {
    clamped(&load()["speech"][agent]["volume"], 0.0, 1.0, 1.0)
}

pub fn set_speech_volume(agent: &str, volume: f64) {
    let volume = volume.clamp(0.0, 1.0);
    update("speech volume", |config| {
        ensure_agent(config, agent)["volume"] = json!(volume);
        true
    });
}

/// A finite number inside the range, or the default. NaN and infinity are
/// rejected rather than clamped: both survive `clamp` on one side and would
/// reach the speech engine as a rate.
fn clamped(value: &Value, min: f64, max: f64, default: f64) -> f64 {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(min, max))
        .unwrap_or(default)
}

fn ensure_speech(config: &mut Value) -> &mut Value {
    if !config["speech"].is_object() {
        config["speech"] = json!({});
    }
    &mut config["speech"]
}

fn ensure_agent<'a>(config: &'a mut Value, agent: &str) -> &'a mut Value {
    let speech = ensure_speech(config);
    if !speech[agent].is_object() {
        speech[agent] = json!({});
    }
    &mut speech[agent]
}

/// The morning digest (§12.5): on unless the user turns it off. It is the
/// one part of the app that is about their day rather than the agents'.
pub fn digest_enabled() -> bool {
    load()["digest"]["enabled"].as_bool().unwrap_or(true)
}

pub fn set_digest_enabled(enabled: bool) {
    update("digest setting", |config| {
        if !config["digest"].is_object() {
            config["digest"] = json!({});
        }
        config["digest"]["enabled"] = json!(enabled);
        true
    });
}

/// The local day whose digest has already been spoken (`YYYY-MM-DD`), so
/// "once daily" survives a restart.
pub fn digest_last_spoken_day() -> Option<String> {
    load()["digest"]["last_spoken_day"]
        .as_str()
        .map(str::to_string)
}

pub fn set_digest_last_spoken_day(day: &str) {
    update("digest day", |config| {
        if !config["digest"].is_object() {
            config["digest"] = json!({});
        }
        config["digest"]["last_spoken_day"] = json!(day);
        true
    });
}

/// The local day whose digest CARD the user dismissed. Separate from the
/// spoken day: hearing it and being done with the card are different acts.
pub fn digest_dismissed_day() -> Option<String> {
    load()["digest"]["dismissed_day"].as_str().map(str::to_string)
}

pub fn set_digest_dismissed_day(day: &str) {
    update("digest dismissal", |config| {
        if !config["digest"].is_object() {
            config["digest"] = json!({});
        }
        config["digest"]["dismissed_day"] = json!(day);
        true
    });
}

/// How long a session may sit waiting on the user before ONE reminder is
/// spoken (§11.4), from `quiet.remind_after_secs`. 0 or absent = off, which is
/// the default: a notifier that repeats itself is the thing people mute.
///
/// Floored well above zero when enabled, because the reminder is spoken and a
/// two-second reminder is just an echo of the callout that raised it.
pub fn remind_after_secs() -> Option<u64> {
    remind_after_from(&load())
}

fn remind_after_from(config: &Value) -> Option<u64> {
    let secs = config["quiet"]["remind_after_secs"].as_f64()?;
    (secs.is_finite() && secs >= 1.0).then(|| (secs as u64).max(30))
}

/// Downgrade callouts to a chime when the session's own terminal is already
/// frontmost (§11.3). ON by default: the app's core risk is becoming the
/// thing you mute, and announcing what you are already staring at is the
/// fastest route there.
pub fn quiet_focus_aware() -> bool {
    load()["quiet"]["focus_aware"].as_bool().unwrap_or(true)
}

pub fn set_quiet_focus_aware(enabled: bool) {
    update("focus-aware quieting", |config| {
        if !config["quiet"].is_object() {
            config["quiet"] = json!({});
        }
        config["quiet"]["focus_aware"] = json!(enabled);
        true
    });
}

/// Is the clock inside the user's quiet window right now?
///
/// OFF unless `quiet.hours` is set, and read fresh each time so a change
/// applies to the very next callout. LOCAL time on purpose — "after ten at
/// night" means the user's night, not UTC's.
pub fn in_quiet_hours_now() -> bool {
    let Some(range) = load()["quiet"]["hours"].as_str().map(str::to_string) else {
        return false;
    };
    let now = chrono::Local::now();
    let minutes = chrono::Timelike::hour(&now) * 60 + chrono::Timelike::minute(&now);
    crate::speaker::in_quiet_hours(&range, minutes)
}

/// Which chime plays for which moment (§11.3).
///
/// TWO scenarios, chosen independently, because they mean opposite things.
/// `attention` fires when a callout is DOWNGRADED — you are already looking
/// at the terminal, so the app declines to say a sentence and just marks the
/// moment. `notice` fires for quiet fleet progress that was never going to be
/// spoken at all. One user wants the same sound for both; another wants to
/// tell them apart without looking. Both are reasonable, so both are settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChimeScenario {
    Attention,
    Notice,
}

impl ChimeScenario {
    fn key(self) -> &'static str {
        match self {
            ChimeScenario::Attention => "attention",
            ChimeScenario::Notice => "notice",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "attention" => Some(ChimeScenario::Attention),
            "notice" => Some(ChimeScenario::Notice),
            _ => None,
        }
    }
}

pub fn chime_for(scenario: ChimeScenario) -> crate::speaker::Chime {
    let config = load();
    match config["chime"][scenario.key()].as_str() {
        Some(name) => crate::speaker::Chime::from_str(name),
        // Absent means the default, not silence — a fresh install should be
        // audible without visiting settings first.
        None => crate::speaker::Chime::Ding,
    }
}

pub fn set_chime(scenario: ChimeScenario, chime: crate::speaker::Chime) {
    update("chime choice", |config| {
        if !config["chime"].is_object() {
            config["chime"] = json!({});
        }
        config["chime"][scenario.key()] = json!(chime.as_str());
        true
    });
}

/// Repo roots whose suite event spine the bell tails (§13), from
/// `spine.roots`. Absent or empty means the bridge does nothing at all —
/// which is the default, and the honest one: hearing another tool's events
/// is something you opt into per repo.
///
/// Read-only here, like `hotkey.next`: v1 is a hand-edited list, and the
/// zero-config path where the shim registers the root it is running in is
/// its own piece of work (PMB-7). A key the app never writes is a key an app
/// update can never clobber.
pub fn spine_roots() -> Vec<PathBuf> {
    spine_roots_from(&load())
}

fn spine_roots_from(config: &Value) -> Vec<PathBuf> {
    let Some(roots) = config["spine"]["roots"].as_array() else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    roots
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        // Two spellings of one repo would tail it twice and say everything
        // twice, so the de-dup is on the path, not the string.
        .filter(|root| seen.insert(root.clone()))
        .collect()
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

    /// Off unless the user asked for it, and never so eager that the
    /// "reminder" is just an echo of the callout that raised it (§11.4).
    #[test]
    fn the_reminder_is_off_by_default_and_floored_when_on() {
        for (raw, want) in [
            (json!({}), None),
            (json!({ "quiet": {} }), None),
            (json!({ "quiet": { "remind_after_secs": 0 } }), None),
            (json!({ "quiet": { "remind_after_secs": -60 } }), None),
            // Sub-second is somebody typing a number they did not mean.
            (json!({ "quiet": { "remind_after_secs": 0.5 } }), None),
            (json!({ "quiet": { "remind_after_secs": "300" } }), None),
            (json!({ "quiet": { "remind_after_secs": true } }), None),
            // Floored: a two-second reminder is an echo, not a reminder.
            (json!({ "quiet": { "remind_after_secs": 1 } }), Some(30)),
            (json!({ "quiet": { "remind_after_secs": 29 } }), Some(30)),
            (json!({ "quiet": { "remind_after_secs": 300 } }), Some(300)),
            (json!({ "quiet": { "remind_after_secs": 3600.9 } }), Some(3600)),
        ] {
            assert_eq!(remind_after_from(&raw), want, "{raw}");
        }
    }

    /// Rate and volume are clamped where they are READ, not only where they
    /// are written: this file is user-editable and a hand-typed `40` must not
    /// produce an utterance nobody can follow, nor a `0` that never ends.
    #[test]
    fn speech_numbers_are_clamped_and_junk_falls_back() {
        let cases = [
            (json!(1.5), 1.5),
            // Both ends of the range, and past both ends.
            (json!(0.5), 0.5),
            (json!(2.0), 2.0),
            (json!(40), RATE_MAX),
            (json!(-5), RATE_MIN),
            (json!(0), RATE_MIN),
            // Wrong types are the default, not a panic and not a coercion:
            // a string "1.5" is somebody editing by hand and getting it
            // wrong, and guessing at it is how a config starts lying.
            (json!("1.5"), 1.0),
            (json!(true), 1.0),
            (json!(null), 1.0),
            (json!({}), 1.0),
        ];
        for (raw, want) in cases {
            assert_eq!(clamped(&raw, RATE_MIN, RATE_MAX, 1.0), want, "{raw}");
        }
        assert_eq!(clamped(&json!(2.0), 0.0, 1.0, 1.0), 1.0, "volume ceiling");
        assert_eq!(clamped(&json!(-1), 0.0, 1.0, 1.0), 0.0, "volume floor");
        // NaN survives `clamp` on both sides and would reach the speech
        // engine as a rate; it has to be rejected rather than clamped.
        let nan: Value = serde_json::from_str("1e999").unwrap_or(json!(f64::NAN));
        assert_eq!(clamped(&nan, RATE_MIN, RATE_MAX, 1.0), 1.0);
    }

    /// Writing one setting must never cost another. `speech` is nested two
    /// levels deep, so its helpers are the ones that could plausibly clobber
    /// a sibling.
    #[test]
    fn speech_writes_preserve_everything_else() {
        let mut config = json!({
            "gate_tool_calls": true,
            "voices": { "codex": "Daniel" },
            "speech": { "style": "status_only", "codex": { "rate": 1.4 } }
        });
        ensure_agent(&mut config, "claude-code")["rate"] = json!(1.5);
        ensure_speech(&mut config)["style"] = json!("terse");

        assert_eq!(config["gate_tool_calls"], json!(true));
        assert_eq!(config["voices"]["codex"], json!("Daniel"));
        assert_eq!(config["speech"]["codex"]["rate"], json!(1.4));
        assert_eq!(config["speech"]["claude-code"]["rate"], json!(1.5));
        assert_eq!(config["speech"]["style"], json!("terse"));

        // A `speech` key of the wrong shape (hand-edited, or from a future
        // version) is replaced rather than crashing an index — and taking the
        // unrelated keys with it is not on the table.
        for wrong in [json!("terse"), json!([1, 2]), json!(null), json!(3)] {
            let mut config = json!({ "voices": { "codex": "Daniel" }, "speech": wrong });
            ensure_agent(&mut config, "codex")["volume"] = json!(0.5);
            assert_eq!(config["speech"]["codex"]["volume"], json!(0.5));
            assert_eq!(config["voices"]["codex"], json!("Daniel"));
        }
    }

    /// A config we cannot parse is NOT an empty config to a writer.
    ///
    /// Every setter loads, edits one key and stores the result, so treating an
    /// unreadable file as `{}` would replace it with that one key and destroy
    /// every voice, gate and speech setting the user had. Sliders write often,
    /// which turned this from theory into something a bad editor save could
    /// trigger any afternoon.
    #[test]
    fn a_config_that_does_not_parse_is_not_silently_replaced() {
        let dir = std::env::temp_dir().join(format!("pmb-config-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let tmp = dir.join("config.json.tmp");

        // Truncated mid-write, and a valid JSON scalar that is not settings.
        for junk in [r#"{"voices": {"codex": "Dan"#, r#""just a string""#, "[]"] {
            write_atomic(&tmp, &path, junk).unwrap();
            assert_eq!(
                read_config(&path),
                Err(()),
                "a writer must refuse to build on {junk:?}"
            );
        }

        // Real settings load, and so does a machine that has never had a
        // config file — a fresh install must still be able to save.
        write_atomic(&tmp, &path, r#"{"gate_tool_calls": true}"#).unwrap();
        assert_eq!(
            read_config(&path),
            Ok(json!({ "gate_tool_calls": true })),
            "valid settings still load"
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_config(&path), Ok(json!({})), "no file yet is not a failure");

        let _ = std::fs::remove_dir_all(&dir);
    }

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
