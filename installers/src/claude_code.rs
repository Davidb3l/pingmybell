//! Claude Code integration: merge our hook entries into
//! `~/.claude/settings.json` (AC-2.1) and remove exactly them on uninstall.
//!
//! Our entries are identified by the shim binary name appearing in the hook
//! command. We only ever add standalone matcher groups, but uninstall is
//! defensive: it strips our commands out of any group, then drops groups and
//! event arrays that end up empty.

use std::io;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::{write_atomic, InstallReport};

/// Marker present in every command we install; never rename the shim binary
/// without a migration for this.
pub const MARKER: &str = "pingmybell-shim";

/// (hook event, shim subcommand, timeout seconds, matcher)
///
/// PreToolUse appears TWICE on purpose, with different budgets. They are two
/// different kinds of wait:
///
/// * `AskUserQuestion` at **600 s** — a typed free-text answer extends its
///   park while the user is actually typing (540 s server ceiling, 570 s in
///   the shim), so the hook has to outlast that. Verified empirically against
///   claude 2.1.198 that a configured timeout above the old 120 s IS honoured:
///   a hook configured at 600 slept 560 s and its deny still reached the model
///   (`claude -p` waited 570 s and obeyed it). Matches Codex's default (§5.1).
/// * everything else at **120 s** — an approval is a two-second yes/no and the
///   shim gives up on it after 115 s regardless. Keeping the old budget here
///   means a wedged shim can never stall a routine tool call for ten minutes;
///   only the path that genuinely waits on a human gets the long rope.
///
/// AskUserQuestion is matched at all so the user can answer the agent's
/// question from the overlay — unlike the other tools it is never suppressed
/// by `gate_tool_calls` (the shim routes it separately).
const EVENTS: [(&str, &str, u64, Option<&str>); 6] = [
    ("SessionStart", "session-start", 10, None),
    ("Stop", "stop", 10, None),
    ("Notification", "notification", 10, None),
    ("SessionEnd", "session-end", 10, None),
    ("PreToolUse", "pretool", 600, Some("AskUserQuestion")),
    (
        "PreToolUse",
        "pretool",
        120,
        Some("Bash|Write|Edit|MultiEdit"),
    ),
];

pub fn install(shim_path: &Path, settings_path: &Path) -> io::Result<InstallReport> {
    let mut root = load_settings(settings_path)?;

    let backup_path = if settings_path.exists() {
        let backup = settings_path.with_file_name(format!(
            "{}.pingmybell.bak",
            settings_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "settings.json".into())
        ));
        // Keep the FIRST backup: it is the pristine pre-PingMyBell snapshot.
        // A reinstall must not overwrite it with a copy containing our hooks.
        if !backup.exists() {
            std::fs::copy(settings_path, &backup)?;
        }
        Some(backup)
    } else {
        None
    };

    let hooks = ensure_object(&mut root, "hooks")?;
    let shim = shell_quote(&shim_path.to_string_lossy());
    // Strip our old entries for EVERY event first. Two rows share PreToolUse,
    // and removing per-row would delete the group the previous row just added.
    for (event, ..) in EVENTS {
        remove_our_hooks(ensure_array(hooks, event)?);
    }
    for (event, subcommand, timeout, matcher) in EVENTS {
        let groups = ensure_array(hooks, event)?;
        let mut group = json!({
            "hooks": [{
                "type": "command",
                "command": format!("{shim} claude {subcommand}"),
                "timeout": timeout,
            }]
        });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        groups.push(group);
    }

    write_atomic(settings_path, &serde_json::to_string_pretty(&root)?)?;
    Ok(InstallReport {
        settings_path: settings_path.to_path_buf(),
        backup_path,
        events: EVENTS.iter().map(|(e, _, _, _)| *e).collect(),
    })
}

pub fn uninstall(settings_path: &Path) -> io::Result<()> {
    if !settings_path.exists() {
        return Ok(());
    }
    let mut root = load_settings(settings_path)?;

    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for (event, _, _, _) in EVENTS {
            if let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) {
                remove_our_hooks(groups);
            }
        }
        // Prune only OUR event keys if they emptied out — an empty array the
        // user created themselves (any other key) is not ours to remove.
        hooks.retain(|key, groups| {
            !EVENTS.iter().any(|(e, _, _, _)| e == key)
                || !matches!(groups.as_array(), Some(a) if a.is_empty())
        });
    }

    write_atomic(settings_path, &serde_json::to_string_pretty(&root)?)
}

fn load_settings(path: &Path) -> io::Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(&raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not valid JSON ({e}); refusing to touch it",
                path.display()
            ),
        )
    })?;
    if !value.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not a JSON object; refusing to touch it",
                path.display()
            ),
        ));
    }
    Ok(value)
}

/// Strip our commands from every group's inner hooks array, preserving any
/// user hooks sharing a group, then drop groups left empty.
fn remove_our_hooks(groups: &mut Vec<Value>) {
    for group in groups.iter_mut() {
        if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            inner.retain(|h| {
                !h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(MARKER))
            });
        }
    }
    groups.retain(|group| {
        !matches!(
            group.get("hooks").and_then(Value::as_array),
            Some(inner) if inner.is_empty()
        )
    });
}

fn ensure_object<'a>(root: &'a mut Value, key: &str) -> io::Result<&'a mut Map<String, Value>> {
    let obj = root
        .as_object_mut()
        .expect("load_settings guarantees an object");
    let entry = obj.entry(key).or_insert_with(|| json!({}));
    entry.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("settings key {key:?} is not an object; refusing to touch it"),
        )
    })
}

fn ensure_array<'a>(obj: &'a mut Map<String, Value>, key: &str) -> io::Result<&'a mut Vec<Value>> {
    let entry = obj.entry(key).or_insert_with(|| json!([]));
    entry.as_array_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("hooks key {key:?} is not an array; refusing to touch it"),
        )
    })
}

/// Quote a path for use in a shell command string (hook commands run via the
/// shell; our own repo path contains spaces).
fn shell_quote(path: &str) -> String {
    if path.contains('"') || path.contains('\\') {
        // Rare on the platforms we target; escape conservatively.
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("\"{path}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_settings(contents: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        if let Some(c) = contents {
            std::fs::write(&path, c).unwrap();
        }
        (dir, path)
    }

    fn shim() -> PathBuf {
        PathBuf::from("/Applications/Ping My Bell.app/Contents/MacOS/pingmybell-shim")
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn install_into_missing_file_creates_all_events() {
        let (_d, path) = tmp_settings(None);
        let report = install(&shim(), &path).unwrap();
        assert!(report.backup_path.is_none());

        let root = read(&path);
        for (event, sub, _, _) in EVENTS {
            let cmd = root["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(cmd.contains(MARKER));
            assert!(cmd.ends_with(&format!("claude {sub}")));
            assert!(
                cmd.starts_with('"'),
                "path with spaces must be quoted: {cmd}"
            );
        }
    }

    #[test]
    fn pretool_matcher_covers_questions_and_gated_tools() {
        let (_d, path) = tmp_settings(None);
        install(&shim(), &path).unwrap();
        let groups = read(&path)["hooks"]["PreToolUse"]
            .as_array()
            .cloned()
            .unwrap();

        // Every tool we care about is still matched, exactly once — a tool in
        // two groups would fire the shim twice for one call.
        let mut budget_for = std::collections::HashMap::new();
        for group in &groups {
            let timeout = group["hooks"][0]["timeout"].as_u64().unwrap();
            for tool in group["matcher"].as_str().unwrap().split('|') {
                assert!(
                    budget_for.insert(tool.to_string(), timeout).is_none(),
                    "{tool} matched by two PreToolUse groups"
                );
            }
        }
        // The question path must outlast the shim's LONGEST park: 570 s on
        // /v1/question, itself outlasting the server's 540 s ceiling. Anything
        // less and the agent kills the hook while the user is still typing.
        assert_eq!(budget_for.get("AskUserQuestion"), Some(&600));
        // The rest stay short: an approval is a two-second decision and the
        // shim gives up after 115 s, so a wedged shim must not be able to
        // stall a routine tool call for ten minutes.
        for tool in ["Bash", "Write", "Edit", "MultiEdit"] {
            assert_eq!(budget_for.get(tool), Some(&120), "{tool}");
        }
    }

    /// Two of our groups share the PreToolUse array, so a reinstall must
    /// replace BOTH rather than stack or cannibalise them.
    #[test]
    fn reinstall_keeps_exactly_two_pretool_groups() {
        let (_d, path) = tmp_settings(None);
        install(&shim(), &path).unwrap();
        install(&shim(), &path).unwrap();
        install(&shim(), &path).unwrap();
        let groups = read(&path)["hooks"]["PreToolUse"]
            .as_array()
            .cloned()
            .unwrap();
        assert_eq!(groups.len(), 2, "{groups:#?}");
    }

    #[test]
    fn install_preserves_user_settings_and_hooks() {
        let user = r#"{
            "model": "opus",
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "my-guard"}]}]
            }
        }"#;
        let (_d, path) = tmp_settings(Some(user));
        let report = install(&shim(), &path).unwrap();
        assert!(report.backup_path.as_ref().unwrap().exists());

        let root = read(&path);
        assert_eq!(root["model"], "opus");
        assert_eq!(root["permissions"]["allow"][0], "Bash(ls:*)");
        // user's Stop hook kept, ours appended
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert!(stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("afplay"));
        assert!(stop[1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(MARKER));
        // untouched event left alone
        assert_eq!(root["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    }

    #[test]
    fn install_is_idempotent() {
        let (_d, path) = tmp_settings(None);
        install(&shim(), &path).unwrap();
        install(&PathBuf::from("/new/location/pingmybell-shim"), &path).unwrap();

        let root = read(&path);
        for (event, ..) in EVENTS {
            let groups = root["hooks"][event].as_array().unwrap();
            // PreToolUse is the one event we install twice (question vs. the
            // gated tools — different budgets); everything else is a single
            // entry. Either way a reinstall must REPLACE, never stack.
            let expected = EVENTS.iter().filter(|(e, ..)| *e == event).count();
            assert_eq!(
                groups.len(),
                expected,
                "{event}: exactly {expected} entries after reinstall"
            );
            for group in groups {
                let cmd = group["hooks"][0]["command"].as_str().unwrap();
                assert!(cmd.contains("/new/location/"), "reinstall updates path");
            }
        }
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let user = r#"{
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "afplay chime.aiff"}]}]
            }
        }"#;
        let (_d, path) = tmp_settings(Some(user));
        install(&shim(), &path).unwrap();
        uninstall(&path).unwrap();

        let root = read(&path);
        assert_eq!(root["model"], "opus");
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("afplay"));
        // events that only held our entry are gone entirely
        assert!(root["hooks"].get("SessionStart").is_none());
    }

    #[test]
    fn uninstall_preserves_user_hook_sharing_a_group() {
        // Defensive: a user may have manually merged their hook into our group.
        let mixed = format!(
            r#"{{"hooks": {{"Stop": [{{"hooks": [
                {{"type": "command", "command": "afplay chime.aiff"}},
                {{"type": "command", "command": "\"/x/{MARKER}\" claude stop"}}
            ]}}]}}}}"#
        );
        let (_d, path) = tmp_settings(Some(&mixed));
        uninstall(&path).unwrap();

        let root = read(&path);
        let inner = root["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert!(inner[0]["command"].as_str().unwrap().contains("afplay"));
    }

    #[test]
    fn uninstall_leaves_foreign_empty_arrays_alone() {
        let user = r#"{"hooks": {"PostToolUse": []}}"#;
        let (_d, path) = tmp_settings(Some(user));
        uninstall(&path).unwrap();
        let root = read(&path);
        assert!(
            root["hooks"]["PostToolUse"].is_array(),
            "user's own empty hook key must survive uninstall"
        );
    }

    #[test]
    fn reinstall_keeps_pristine_backup() {
        let (_d, path) = tmp_settings(Some(r#"{"model": "opus"}"#));
        let report = install(&shim(), &path).unwrap();
        let backup = report.backup_path.unwrap();
        install(&shim(), &path).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&backup).unwrap()).unwrap();
        assert!(
            saved.get("hooks").is_none(),
            "backup must stay the pre-PingMyBell snapshot"
        );
    }

    #[test]
    fn refuses_to_touch_invalid_json() {
        let (_d, path) = tmp_settings(Some("{ not json"));
        assert!(install(&shim(), &path).is_err());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "{ not json", "file untouched on refusal");
    }
}
