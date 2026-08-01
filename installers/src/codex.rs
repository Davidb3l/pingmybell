//! Codex CLI integration (AC-3.1, §5.2). Two independent, separately
//! installable pieces:
//!
//! 1. **Notifications** — `notify = ["<shim>", "codex"]` in
//!    `~/.codex/config.toml`, preserving everything else in the file
//!    (toml_edit keeps user formatting and comments). Never touches
//!    `tui.notifications`.
//! 2. **Questions** — a `PreToolUse` hook on `request_user_input` in
//!    `$CODEX_HOME/hooks.json` (JSON, Claude-Code-shaped, NOT config.toml),
//!    which lets the user answer a Codex question from the overlay.
//!
//! Neither knows about the other: installing or removing one leaves the other
//! exactly as it was.

use std::io;
use std::path::Path;

use serde_json::{json, Map, Value};
use toml_edit::{value, Array, DocumentMut};

use crate::claude_code::MARKER;
use crate::{write_atomic, InstallReport};

pub fn install(shim_path: &Path, config_path: &Path) -> io::Result<InstallReport> {
    let raw = if config_path.exists() {
        std::fs::read_to_string(config_path)?
    } else {
        String::new()
    };
    let mut doc: DocumentMut = raw.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not valid TOML ({e}); refusing to touch it",
                config_path.display()
            ),
        )
    })?;

    // Codex supports a single notify program, and something else may already
    // own the slot (the ChatGPT desktop app installs its own). We wrap it:
    // our shim multiplexes, forwarding the payload to the previous program
    // via `--chain <prog> <args...>` before ringing PingMyBell.
    let chain: Vec<String> = match doc.get("notify") {
        None => Vec::new(),
        Some(existing) => {
            let items: Option<Vec<String>> = existing.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect()
            });
            let Some(items) = items.filter(|i| !i.is_empty()) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} has a notify value that is not an array of strings; refusing to touch it",
                        config_path.display()
                    ),
                ));
            };
            if items[0].contains(MARKER) {
                // Reinstall: keep whatever we were already chaining.
                items
                    .iter()
                    .skip_while(|s| s.as_str() != "--chain")
                    .skip(1)
                    .cloned()
                    .collect()
            } else {
                // Foreign notify program becomes the chain.
                items
            }
        }
    };

    let backup_path = if config_path.exists() {
        let backup = config_path.with_file_name(format!(
            "{}.pingmybell.bak",
            config_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "config.toml".into())
        ));
        // First backup only: the pristine pre-PingMyBell snapshot.
        if !backup.exists() {
            std::fs::copy(config_path, &backup)?;
        }
        Some(backup)
    } else {
        None
    };

    let mut notify = Array::new();
    // argv array, not a shell string — no quoting needed.
    notify.push(shim_path.to_string_lossy().as_ref());
    notify.push("codex");
    if !chain.is_empty() {
        notify.push("--chain");
        for item in &chain {
            notify.push(item.as_str());
        }
    }
    doc["notify"] = value(notify);

    write_atomic(config_path, &doc.to_string())?;
    Ok(InstallReport {
        settings_path: config_path.to_path_buf(),
        backup_path,
        events: if chain.is_empty() {
            vec!["notify"]
        } else {
            vec!["notify", "chained existing notify program"]
        },
    })
}

pub fn uninstall(config_path: &Path) -> io::Result<()> {
    if !config_path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(config_path)?;
    let mut doc: DocumentMut = raw.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is not valid TOML ({e}); refusing to touch it",
                config_path.display()
            ),
        )
    })?;

    let items: Vec<String> = doc
        .get("notify")
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if items.first().is_some_and(|p| p.contains(MARKER)) {
        // Restore whatever we were chaining; remove the key if nothing was.
        let chain: Vec<String> = items
            .iter()
            .skip_while(|s| s.as_str() != "--chain")
            .skip(1)
            .cloned()
            .collect();
        if chain.is_empty() {
            doc.remove("notify");
        } else {
            let mut restored = Array::new();
            for item in &chain {
                restored.push(item.as_str());
            }
            doc["notify"] = value(restored);
        }
        write_atomic(config_path, &doc.to_string())?;
    }
    Ok(())
}

// ─── hooks.json (question answering) ────────────────────────────────────────
//
// Codex reads hook config from `$CODEX_HOME/hooks.json` in the same shape
// Claude Code uses inside settings.json: `hooks.<Event>[] = { matcher, hooks:
// [{ type, command, timeout }] }`. Commands run through `$SHELL -lc`, so the
// path is shell-quoted exactly as in claude_code.rs.
//
// TRUST GATE: a new or changed hook starts UNTRUSTED and does nothing until
// the user approves it in Codex's own hook-review UI (ChatGPT app → Settings
// → Hooks). The trust key is a sha256 over the hook's normalized identity,
// which we cannot precompute — so we write the entry and the human approves
// it once. Any later change to the command string re-triggers the review.

/// The hook event Codex fires before a tool call.
const HOOK_EVENT: &str = "PreToolUse";
/// Codex's question tool — the only thing we ever want to intercept.
const HOOK_MATCHER: &str = "request_user_input";
/// The shim parks up to 570 s on `/v1/question` (server side: a 110 s base
/// park, extended up to a 540 s ceiling while the user is typing a free-text
/// answer), so the hook must outlast that. 600 s is both Codex's own default
/// and what the Claude Code entry now uses, so the two agents behave alike.
///
/// A stalled app cannot actually freeze a turn for ten minutes: the shim's
/// own read timeout gives up first, and the park only ever reaches the
/// ceiling when a human is demonstrably still typing into the reply window.
const HOOK_TIMEOUT: u64 = 600;

pub fn install_hooks(shim_path: &Path, hooks_path: &Path) -> io::Result<InstallReport> {
    let mut root = load_json(hooks_path)?;

    let backup_path = if hooks_path.exists() {
        let backup = backup_path_for(hooks_path, "hooks.json");
        // First backup only: the pristine pre-PingMyBell snapshot.
        if !backup.exists() {
            std::fs::copy(hooks_path, &backup)?;
        }
        Some(backup)
    } else {
        None
    };

    let hooks = ensure_object(&mut root, "hooks")?;
    let groups = ensure_array(hooks, HOOK_EVENT)?;
    // Reinstall (or a moved app bundle) replaces our entry rather than
    // stacking a second one; foreign entries in the same array are untouched.
    remove_our_hooks(groups);
    groups.push(json!({
        "matcher": HOOK_MATCHER,
        "hooks": [{
            "type": "command",
            "command": format!("{} codex-ask", shell_quote(&shim_path.to_string_lossy())),
            "timeout": HOOK_TIMEOUT,
        }]
    }));

    write_atomic(hooks_path, &serde_json::to_string_pretty(&root)?)?;
    Ok(InstallReport {
        settings_path: hooks_path.to_path_buf(),
        backup_path,
        events: vec!["PreToolUse:request_user_input"],
    })
}

pub fn uninstall_hooks(hooks_path: &Path) -> io::Result<()> {
    if !hooks_path.exists() {
        return Ok(());
    }
    let mut root = load_json(hooks_path)?;

    let mut removed = false;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        if let Some(groups) = hooks.get_mut(HOOK_EVENT).and_then(Value::as_array_mut) {
            removed = remove_our_hooks(groups);
        }
        // Prune the event key only if OUR removal is what emptied it — an
        // empty array the user wrote themselves is not ours to delete.
        if removed {
            hooks.retain(|key, groups| {
                key != HOOK_EVENT || !matches!(groups.as_array(), Some(a) if a.is_empty())
            });
        }
    }
    // Own nothing here? Leave the file completely alone. Rewriting it would
    // reorder and reformat a config we never installed into — and, unlike
    // install, uninstall takes no backup.
    if !removed {
        return Ok(());
    }

    write_atomic(hooks_path, &serde_json::to_string_pretty(&root)?)
}

fn backup_path_for(path: &Path, fallback: &str) -> std::path::PathBuf {
    path.with_file_name(format!(
        "{}.pingmybell.bak",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| fallback.into())
    ))
}

/// Read a JSON config as an object. A missing or empty file is `{}`; anything
/// unparseable or non-object is refused so we never clobber a file we do not
/// understand.
fn load_json(path: &Path) -> io::Result<Value> {
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
/// user hooks sharing a group, then drop groups WE emptied. Returns whether
/// anything of ours was actually there.
///
/// The "we emptied" part is load-bearing: a group the user wrote with an
/// empty `hooks` array is not ours to delete, so pruning has to be driven by
/// what this call removed rather than by the final state.
fn remove_our_hooks(groups: &mut Vec<Value>) -> bool {
    let mut removed = false;
    let mut i = 0;
    while i < groups.len() {
        let mut drop_group = false;
        if let Some(inner) = groups[i].get_mut("hooks").and_then(Value::as_array_mut) {
            let before = inner.len();
            inner.retain(|h| {
                !h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(MARKER))
            });
            if inner.len() != before {
                removed = true;
                drop_group = inner.is_empty();
            }
        }
        if drop_group {
            groups.remove(i);
        } else {
            i += 1;
        }
    }
    removed
}

fn ensure_object<'a>(root: &'a mut Value, key: &str) -> io::Result<&'a mut Map<String, Value>> {
    let obj = root.as_object_mut().expect("load_json guarantees an object");
    let entry = obj.entry(key).or_insert_with(|| json!({}));
    entry.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("hooks.json key {key:?} is not an object; refusing to touch it"),
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

/// Quote a path for a shell command string (Codex runs command hooks through
/// `$SHELL -lc`, and our own bundle path contains spaces).
fn shell_quote(path: &str) -> String {
    if path.contains('"') || path.contains('\\') {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("\"{path}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_config(contents: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        if let Some(c) = contents {
            std::fs::write(&path, c).unwrap();
        }
        (dir, path)
    }

    fn shim() -> PathBuf {
        PathBuf::from("/Applications/Ping My Bell.app/Contents/MacOS/pingmybell-shim")
    }

    #[test]
    fn install_into_missing_file_sets_notify() {
        let (_d, path) = tmp_config(None);
        let report = install(&shim(), &path).unwrap();
        assert!(report.backup_path.is_none());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("notify = "));
        assert!(raw.contains(MARKER));
        assert!(raw.contains("\"codex\""));
    }

    #[test]
    fn install_preserves_user_config_and_comments() {
        let user = "# my codex setup\nmodel = \"o3\"\n\n[tui]\nnotifications = true\n";
        let (_d, path) = tmp_config(Some(user));
        install(&shim(), &path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# my codex setup"), "comments kept");
        assert!(raw.contains("model = \"o3\""));
        assert!(raw.contains("notifications = true"), "tui untouched");
        assert!(raw.contains(MARKER));
    }

    #[test]
    fn install_is_idempotent_and_updates_path() {
        let (_d, path) = tmp_config(None);
        install(&shim(), &path).unwrap();
        install(&PathBuf::from("/new/pingmybell-shim"), &path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.matches("notify").count(), 1);
        assert!(raw.contains("/new/pingmybell-shim"));
    }

    #[test]
    fn foreign_notify_is_chained_and_restored() {
        // e.g. the ChatGPT desktop app's own notify hook
        let user = "notify = [\"/Apps/SkyClient\", \"turn-ended\"]\n";
        let (_d, path) = tmp_config(Some(user));
        install(&shim(), &path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(MARKER), "we own the slot");
        assert!(raw.contains("--chain"));
        assert!(raw.contains("/Apps/SkyClient"));
        assert!(raw.contains("turn-ended"));

        // Reinstall keeps the chain exactly once.
        install(&PathBuf::from("/new/pingmybell-shim"), &path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.matches("SkyClient").count(), 1);
        assert!(raw.contains("/new/pingmybell-shim"));

        // Uninstall restores the original program.
        uninstall(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(MARKER));
        assert!(raw.contains("/Apps/SkyClient"));
        assert!(raw.contains("turn-ended"));
    }

    #[test]
    fn malformed_notify_is_refused() {
        let user = "notify = \"not-an-array\"\n";
        let (_d, path) = tmp_config(Some(user));
        assert!(install(&shim(), &path).is_err());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, user, "file untouched on refusal");
    }

    #[test]
    fn uninstall_removes_only_our_notify() {
        let (_d, path) = tmp_config(Some("model = \"o3\"\n"));
        install(&shim(), &path).unwrap();
        uninstall(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("notify"));
        assert!(raw.contains("model = \"o3\""));

        let (_d2, path2) = tmp_config(Some("notify = [\"my-notifier\"]\n"));
        uninstall(&path2).unwrap();
        let raw2 = std::fs::read_to_string(&path2).unwrap();
        assert!(raw2.contains("my-notifier"), "foreign notify untouched");
    }

    // ─── hooks.json ─────────────────────────────────────────────────────────

    fn tmp_hooks(contents: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        if let Some(c) = contents {
            std::fs::write(&path, c).unwrap();
        }
        (dir, path)
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn install_hooks_into_missing_file_writes_the_question_hook() {
        let (_d, path) = tmp_hooks(None);
        let report = install_hooks(&shim(), &path).unwrap();
        assert!(report.backup_path.is_none());

        let root = read(&path);
        let groups = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["matcher"], "request_user_input");
        let hook = &groups[0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(
            hook["timeout"], 600,
            "must outlast the shim's 570 s /v1/question park"
        );
        let cmd = hook["command"].as_str().unwrap();
        assert!(cmd.contains(MARKER));
        assert!(cmd.ends_with(" codex-ask"), "{cmd}");
        assert!(cmd.starts_with('"'), "path with spaces must be quoted: {cmd}");
    }

    #[test]
    fn install_hooks_preserves_foreign_hooks_and_keys() {
        let user = r#"{
            "hooks": {
                "PreToolUse": [{"matcher": "shell", "hooks": [{"type": "command", "command": "my-guard"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "say hi"}]}]
            },
            "somethingElse": {"keep": true}
        }"#;
        let (_d, path) = tmp_hooks(Some(user));
        let report = install_hooks(&shim(), &path).unwrap();
        assert!(report.backup_path.as_ref().unwrap().exists());

        let root = read(&path);
        assert_eq!(root["somethingElse"]["keep"], true);
        assert_eq!(root["hooks"]["SessionStart"][0]["hooks"][0]["command"], "say hi");
        let groups = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "foreign PreToolUse entry kept, ours appended");
        assert_eq!(groups[0]["matcher"], "shell");
        assert!(groups[1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(MARKER));
    }

    #[test]
    fn install_hooks_is_idempotent_and_updates_path() {
        let (_d, path) = tmp_hooks(None);
        install_hooks(&shim(), &path).unwrap();
        install_hooks(&PathBuf::from("/new/location/pingmybell-shim"), &path).unwrap();

        let groups = read(&path)["hooks"]["PreToolUse"].as_array().unwrap().clone();
        assert_eq!(groups.len(), 1, "exactly one entry after reinstall");
        assert!(groups[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("/new/location/"));
    }

    #[test]
    fn reinstall_hooks_keeps_pristine_backup() {
        let (_d, path) = tmp_hooks(Some(r#"{"hooks": {"SessionStart": []}}"#));
        let report = install_hooks(&shim(), &path).unwrap();
        let backup = report.backup_path.unwrap();
        install_hooks(&shim(), &path).unwrap();
        let saved = read(&backup);
        assert!(
            saved["hooks"].get("PreToolUse").is_none(),
            "backup must stay the pre-PingMyBell snapshot"
        );
    }

    #[test]
    fn uninstall_hooks_removes_only_ours() {
        let user = r#"{
            "hooks": {
                "PreToolUse": [{"matcher": "shell", "hooks": [{"type": "command", "command": "my-guard"}]}]
            }
        }"#;
        let (_d, path) = tmp_hooks(Some(user));
        install_hooks(&shim(), &path).unwrap();
        uninstall_hooks(&path).unwrap();

        let root = read(&path);
        let groups = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["matcher"], "shell");

        // Nothing of ours left → the event key disappears entirely.
        let (_d2, path2) = tmp_hooks(None);
        install_hooks(&shim(), &path2).unwrap();
        uninstall_hooks(&path2).unwrap();
        assert!(read(&path2)["hooks"].get("PreToolUse").is_none());
    }

    #[test]
    fn uninstall_hooks_preserves_a_user_hook_sharing_our_group() {
        let mixed = format!(
            r#"{{"hooks": {{"PreToolUse": [{{"matcher": "request_user_input", "hooks": [
                {{"type": "command", "command": "afplay chime.aiff"}},
                {{"type": "command", "command": "\"/x/{MARKER}\" codex-ask"}}
            ]}}]}}}}"#
        );
        let (_d, path) = tmp_hooks(Some(&mixed));
        uninstall_hooks(&path).unwrap();

        let inner = read(&path)["hooks"]["PreToolUse"][0]["hooks"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(inner.len(), 1);
        assert!(inner[0]["command"].as_str().unwrap().contains("afplay"));
    }

    #[test]
    fn uninstall_hooks_leaves_a_foreign_empty_array_alone() {
        let (_d, path) = tmp_hooks(Some(r#"{"hooks": {"PostToolUse": [], "PreToolUse": []}}"#));
        uninstall_hooks(&path).unwrap();
        let root = read(&path);
        assert!(root["hooks"]["PostToolUse"].is_array());
        assert!(
            root["hooks"]["PreToolUse"].is_array(),
            "an empty PreToolUse we did not empty must survive"
        );
    }

    #[test]
    fn a_users_own_empty_group_is_never_deleted() {
        // Regression: pruning must be driven by what WE removed, not by the
        // final state — otherwise an empty group the user wrote themselves
        // (a disabled hook they mean to fill back in) silently vanishes on
        // both install and uninstall.
        let user = r#"{"hooks": {"PreToolUse": [
            {"matcher": "shell", "hooks": []},
            {"matcher": "apply_patch", "hooks": [{"type": "command", "command": "my-guard"}]}
        ]}}"#;
        let (_d, path) = tmp_hooks(Some(user));

        install_hooks(&shim(), &path).unwrap();
        let groups = read(&path)["hooks"]["PreToolUse"].as_array().unwrap().clone();
        assert_eq!(groups.len(), 3, "empty foreign group survives install");
        assert_eq!(groups[0]["matcher"], "shell");

        uninstall_hooks(&path).unwrap();
        let groups = read(&path)["hooks"]["PreToolUse"].as_array().unwrap().clone();
        assert_eq!(groups.len(), 2, "only ours removed");
        assert_eq!(groups[0]["matcher"], "shell");
        assert!(groups[0]["hooks"].as_array().unwrap().is_empty());
        assert_eq!(groups[1]["matcher"], "apply_patch");
    }

    #[test]
    fn uninstall_hooks_does_not_touch_a_file_we_own_nothing_in() {
        // Rewriting would reorder/reformat a config we never installed into,
        // and uninstall takes no backup.
        let user = "{\"hooks\":{\"PreToolUse\":[{\"matcher\":\"shell\",\"hooks\":[]}]},\"zzz\":1,\"aaa\":2}";
        let (_d, path) = tmp_hooks(Some(user));
        uninstall_hooks(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            user,
            "byte-identical: not even key order may change"
        );
    }

    #[test]
    fn uninstall_hooks_on_missing_file_is_a_noop() {
        let (_d, path) = tmp_hooks(None);
        uninstall_hooks(&path).unwrap();
        assert!(!path.exists(), "must not create the file just to empty it");
    }

    #[test]
    fn hooks_install_refuses_files_it_does_not_understand() {
        for bad in ["{ not json", "[]", "\"a string\""] {
            let (_d, path) = tmp_hooks(Some(bad));
            assert!(install_hooks(&shim(), &path).is_err(), "accepted {bad:?}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                bad,
                "file untouched on refusal"
            );
        }
        // Right file, wrong shape inside.
        let (_d, path) = tmp_hooks(Some(r#"{"hooks": "off"}"#));
        assert!(install_hooks(&shim(), &path).is_err());
        let (_d2, path2) = tmp_hooks(Some(r#"{"hooks": {"PreToolUse": "off"}}"#));
        assert!(install_hooks(&shim(), &path2).is_err());
        // An empty file is a fresh start, not a refusal.
        let (_d3, path3) = tmp_hooks(Some("   \n"));
        assert!(install_hooks(&shim(), &path3).is_ok());
    }

    #[test]
    fn hooks_and_notify_installs_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let hooks = dir.path().join("hooks.json");
        std::fs::write(&config, "model = \"o3\"\n").unwrap();

        install(&shim(), &config).unwrap();
        install_hooks(&shim(), &hooks).unwrap();

        // Removing the hook leaves notify alone, and vice versa.
        uninstall_hooks(&hooks).unwrap();
        assert!(std::fs::read_to_string(&config).unwrap().contains(MARKER));
        install_hooks(&shim(), &hooks).unwrap();
        uninstall(&config).unwrap();
        assert!(read(&hooks)["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(MARKER));
    }
}
