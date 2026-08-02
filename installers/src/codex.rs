//! Codex CLI integration (AC-3.1, §5.2). Two independent, separately
//! installable pieces:
//!
//! 1. **Notifications** — `notify = ["<shim>", "codex"]` in
//!    `~/.codex/config.toml`, preserving everything else in the file
//!    (toml_edit keeps user formatting and comments). Never touches
//!    `tui.notifications`.
//! 2. **Hooks** — `$CODEX_HOME/hooks.json` (JSON, Claude-Code-shaped, NOT
//!    config.toml), installed and removed as one unit:
//!    * a `PreToolUse` hook on `request_user_input`, so the user can answer a
//!      Codex question from the overlay (§5.2.1);
//!    * a `PermissionRequest` hook on `Bash|apply_patch`, so the user can
//!      approve or deny a Codex command or file change from the overlay
//!      (§5.2.2). Only ever consulted when `gate_tool_calls` is on — the shim
//!      decides that, not the installer, so turning the toggle off does not
//!      require rewriting hooks.json and re-triggering Codex's trust review.
//!
//! The two pieces know nothing about each other: installing or removing one
//! leaves the other exactly as it was.

use std::io;
use std::path::Path;

use serde_json::{json, Map, Value};
use toml_edit::{value, Array, DocumentMut};

use crate::{back_up, discard_backup, shell_quote, write_atomic, InstallReport};

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
            if crate::is_shim_path(&items[0]) {
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

    let backup_path = back_up(config_path, "config.toml")?;

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
    if items.first().is_some_and(|p| crate::is_shim_path(p)) {
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
    // Our entry is gone either way, so the pre-install snapshot has done its
    // job — including when this call found nothing of ours, which is where a
    // failed install or a hand-restore leaves things.
    discard_backup(config_path, "config.toml");
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

/// (hook event, matcher, shim subcommand, timeout seconds, report label)
///
/// TWO different events, and the difference is not cosmetic (§5.2.1, §5.2.2):
///
/// * **`PreToolUse` / `request_user_input` at 600 s** — questions. Fires for
///   the question tool like any other tool call. The shim parks up to 570 s on
///   `/v1/question` (server side: a 110 s base park extended to a 540 s
///   ceiling while the user is actually typing), so the hook must outlast
///   that. 600 s is also Codex's own default and matches the Claude entry.
/// * **`PermissionRequest` / `Bash|apply_patch` at 120 s** — exec and
///   file-change approvals. A different event with a different output schema,
///   fired only where Codex was already going to block and ask a human. An
///   approval is a two-second yes/no and the shim abandons it after 115 s
///   regardless, so the long rope would buy nothing and a wedged shim could
///   stall a command for ten minutes. Same budget as the Claude approval
///   matcher, for the same reason.
///
/// `Bash|apply_patch` is an EXACT matcher, not a regex: Codex treats a pattern
/// of only `[A-Za-z0-9_|]` as a literal alternation list (verified — the hook
/// fired for both). `Bash` is Codex's hook-facing name for every exec flavour
/// and `apply_patch` for file edits; Codex additionally accepts `Write`/`Edit`
/// as aliases for the latter, which we do not need and do not list.
pub const HOOKS: [(&str, &str, &str, u64, &str); 2] = [
    (
        "PreToolUse",
        "request_user_input",
        "codex-ask",
        600,
        "PreToolUse:request_user_input",
    ),
    (
        "PermissionRequest",
        "Bash|apply_patch",
        "codex-approve",
        120,
        "PermissionRequest:Bash|apply_patch",
    ),
];

/// Codex hook lines carry no agent token — `<shim> codex-ask` — so the tails
/// come straight from HOOKS.
fn is_ours(command: &str) -> bool {
    let tails: Vec<&str> = HOOKS.iter().map(|(_, _, sub, _, _)| *sub).collect();
    crate::is_our_command(command, &tails)
}

pub fn install_hooks(shim_path: &Path, hooks_path: &Path) -> io::Result<InstallReport> {
    let mut root = load_json(hooks_path)?;
    let before = root.clone();

    let hooks = ensure_object(&mut root, "hooks")?;
    let shim = shell_quote(&shim_path.to_string_lossy());
    // Strip our old entries for EVERY event first, then add. If two rows ever
    // share an event key, removing per-row would delete the group the previous
    // row just added (the bug the Claude installer already guards against).
    for (event, ..) in HOOKS {
        remove_our_hooks(ensure_array(hooks, event)?);
    }
    for (event, matcher, subcommand, timeout, _) in HOOKS {
        // Reinstall (or a moved app bundle) replaces our entry rather than
        // stacking a second one; foreign entries in the array are untouched.
        ensure_array(hooks, event)?.push(json!({
            "matcher": matcher,
            "hooks": [{
                "type": "command",
                "command": format!("{shim} {subcommand}"),
                "timeout": timeout,
            }]
        }));
    }

    // Unchanged? Do not touch the file. Codex starts every NEW OR CHANGED
    // hook untrusted and makes the user re-approve it in its own review UI,
    // and the trust key is a hash over the hook's text — so rewriting an
    // identical entry would send them back through that dialog after every
    // app update for no reason at all.
    if root == before {
        return Ok(InstallReport {
            settings_path: hooks_path.to_path_buf(),
            backup_path: None,
            events: HOOKS.iter().map(|(.., label)| *label).collect(),
        });
    }

    let backup_path = back_up(hooks_path, "hooks.json")?;
    write_atomic(hooks_path, &serde_json::to_string_pretty(&root)?)?;
    Ok(InstallReport {
        settings_path: hooks_path.to_path_buf(),
        backup_path,
        events: HOOKS.iter().map(|(.., label)| *label).collect(),
    })
}

pub fn uninstall_hooks(hooks_path: &Path) -> io::Result<()> {
    if !hooks_path.exists() {
        return Ok(());
    }
    let mut root = load_json(hooks_path)?;

    let mut removed = false;
    let mut prune_hooks_key = false;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        // Which event keys WE emptied — an empty array the user wrote
        // themselves is not ours to delete, so pruning is driven by what this
        // call removed and never by the final state.
        let mut emptied: Vec<&str> = Vec::new();
        for (event, ..) in HOOKS {
            if let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) {
                if remove_our_hooks(groups) {
                    removed = true;
                    emptied.push(event);
                }
            }
        }
        hooks.retain(|key, groups| {
            !emptied.contains(&key.as_str())
                || !matches!(groups.as_array(), Some(a) if a.is_empty())
        });
        // Same rule one level up: a `hooks` object we emptied is one WE
        // created on install, so install → uninstall is a round trip rather
        // than something that leaves behind a key the user never wrote.
        prune_hooks_key = removed && hooks.is_empty();
    }
    if prune_hooks_key {
        root.as_object_mut()
            .expect("load_json guarantees an object")
            .remove("hooks");
    }
    // Own nothing here? Leave the file completely alone. Rewriting it would
    // reorder and reformat a config we never installed into — and, unlike
    // install, uninstall takes no backup.
    if removed {
        write_atomic(hooks_path, &serde_json::to_string_pretty(&root)?)?;
    }
    // Our entries are gone either way, so the pre-install snapshot has done
    // its job — including when this call found nothing of ours, which is where
    // a failed install or a hand-restore leaves things.
    discard_backup(hooks_path, "hooks.json");
    Ok(())
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
                    .is_some_and(|c| is_ours(c))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::MARKER;
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
        assert!(
            cmd.starts_with(&shell_quote(&shim().to_string_lossy())),
            "path with spaces must reach the shell as one word: {cmd}"
        );
    }

    #[test]
    fn install_hooks_writes_the_approval_hook_on_its_own_event() {
        let (_d, path) = tmp_hooks(None);
        let report = install_hooks(&shim(), &path).unwrap();
        assert_eq!(
            report.events,
            vec![
                "PreToolUse:request_user_input",
                "PermissionRequest:Bash|apply_patch"
            ]
        );

        let root = read(&path);
        let groups = root["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0]["matcher"], "Bash|apply_patch",
            "exact-matcher alternation: Bash covers every exec flavour, \
             apply_patch every file change"
        );
        let hook = &groups[0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(
            hook["timeout"], 120,
            "an approval is a two-second yes/no; the shim abandons it at 115 s \
             regardless, so the question path's 600 s rope would only let a \
             wedged shim stall a command"
        );
        let cmd = hook["command"].as_str().unwrap();
        assert!(cmd.contains(MARKER));
        assert!(cmd.ends_with(" codex-approve"), "{cmd}");
        assert!(
            cmd.starts_with(&shell_quote(&shim().to_string_lossy())),
            "path with spaces must reach the shell as one word: {cmd}"
        );

        // The two hooks live on DIFFERENT events; neither may leak into the
        // other's array (the output schemas are incompatible).
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert!(pre[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with(" codex-ask"));
    }

    #[test]
    fn reinstall_replaces_both_hooks_without_stacking() {
        let (_d, path) = tmp_hooks(None);
        install_hooks(&shim(), &path).unwrap();
        install_hooks(&PathBuf::from("/new/location/pingmybell-shim"), &path).unwrap();
        install_hooks(&PathBuf::from("/new/location/pingmybell-shim"), &path).unwrap();

        let root = read(&path);
        for event in ["PreToolUse", "PermissionRequest"] {
            let groups = root["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event} stacked");
            assert!(groups[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("/new/location/"));
        }
    }

    #[test]
    fn uninstall_hooks_removes_the_approval_hook_too() {
        let user = r#"{
            "hooks": {
                "PermissionRequest": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "my-guard"}]}]
            }
        }"#;
        let (_d, path) = tmp_hooks(Some(user));
        install_hooks(&shim(), &path).unwrap();
        uninstall_hooks(&path).unwrap();

        let root = read(&path);
        // Ours gone from both events; the user's own guard survives.
        assert!(root["hooks"].get("PreToolUse").is_none());
        let groups = root["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "my-guard");

        // Nothing of ours left anywhere → both event keys disappear.
        let (_d2, path2) = tmp_hooks(None);
        install_hooks(&shim(), &path2).unwrap();
        uninstall_hooks(&path2).unwrap();
        let root2 = read(&path2);
        assert!(root2["hooks"].get("PreToolUse").is_none());
        assert!(root2["hooks"].get("PermissionRequest").is_none());
    }

    #[test]
    fn uninstall_prunes_per_event_and_spares_arrays_it_did_not_empty() {
        // The case the two-event pruning actually exists for: we remove our
        // entry from ONE event while a sibling event holds an empty array the
        // user wrote and we never touched. Deleting that array would be us
        // tidying away someone else's disabled hook.
        //
        // Built by hand rather than via install_hooks, because install would
        // put an entry of ours in BOTH events and the interesting asymmetry
        // would never arise.
        let mixed = format!(
            r#"{{"hooks": {{
                "PreToolUse": [{{"matcher": "request_user_input", "hooks": [
                    {{"type": "command", "command": "\"/x/{MARKER}\" codex-ask"}}
                ]}}],
                "PermissionRequest": [],
                "PostToolUse": []
            }}}}"#
        );
        let (_d, path) = tmp_hooks(Some(&mixed));
        uninstall_hooks(&path).unwrap();

        let root = read(&path);
        assert!(
            root["hooks"].get("PreToolUse").is_none(),
            "we emptied this one, so it goes"
        );
        assert!(
            root["hooks"]["PermissionRequest"].is_array()
                && root["hooks"]["PermissionRequest"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
            "an empty array under an event WE also install into, but did not \
             empty on this run, must survive: {root}"
        );
        assert!(
            root["hooks"]["PostToolUse"].is_array(),
            "an event we never touch at all must survive"
        );

        // Mirror image: our approval entry removed while an empty PreToolUse
        // the user wrote survives.
        let mirrored = format!(
            r#"{{"hooks": {{
                "PreToolUse": [],
                "PermissionRequest": [{{"matcher": "Bash|apply_patch", "hooks": [
                    {{"type": "command", "command": "\"/x/{MARKER}\" codex-approve"}}
                ]}}]
            }}}}"#
        );
        let (_d2, path2) = tmp_hooks(Some(&mirrored));
        uninstall_hooks(&path2).unwrap();
        let root2 = read(&path2);
        assert!(root2["hooks"].get("PermissionRequest").is_none());
        assert!(
            root2["hooks"]["PreToolUse"].is_array(),
            "an empty PreToolUse we did not empty must survive: {root2}"
        );
    }

    #[test]
    fn uninstall_preserves_a_user_hook_sharing_our_approval_group() {
        // Same protection the question group already has: a user hook that
        // happens to live in OUR group survives, and the group with it.
        let mixed = format!(
            r#"{{"hooks": {{"PermissionRequest": [{{"matcher": "Bash|apply_patch", "hooks": [
                {{"type": "command", "command": "afplay chime.aiff"}},
                {{"type": "command", "command": "\"/x/{MARKER}\" codex-approve"}}
            ]}}]}}}}"#
        );
        let (_d, path) = tmp_hooks(Some(&mixed));
        uninstall_hooks(&path).unwrap();

        let inner = read(&path)["hooks"]["PermissionRequest"][0]["hooks"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(inner.len(), 1);
        assert!(inner[0]["command"].as_str().unwrap().contains("afplay"));
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
    fn reinstalling_an_unchanged_file_does_not_rewrite_it() {
        // Codex re-reviews any hook whose text changed, so a needless
        // rewrite drags the user back through its approval dialog after
        // every app update.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        install_hooks(&shim(), &path).unwrap();
        let first = std::fs::read(&path).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let report = install_hooks(&shim(), &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first, "bytes must be identical");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "an unchanged install must not even touch the file"
        );
        assert!(report.backup_path.is_none(), "nothing changed, nothing to back up");

        // A MOVED bundle is a real change and must still be written.
        let moved = install_hooks(&PathBuf::from("/new/place/pingmybell-shim"), &path).unwrap();
        assert_ne!(std::fs::read(&path).unwrap(), first);
        assert!(moved.backup_path.is_some());
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
    fn uninstall_discards_the_backup_it_is_responsible_for() {
        // Backups are ours and exist exactly while we are installed: a stale
        // one would masquerade as the pristine pre-install snapshot next time,
        // because install keeps the FIRST backup it finds.
        let (_d, config) = tmp_config(Some("model = \"o3\"\n"));
        let backup = install(&shim(), &config).unwrap().backup_path.unwrap();
        uninstall(&config).unwrap();
        assert!(!backup.exists());

        let (_d2, hooks) = tmp_hooks(Some(r#"{"hooks": {"SessionStart": []}}"#));
        let backup = install_hooks(&shim(), &hooks).unwrap().backup_path.unwrap();
        uninstall_hooks(&hooks).unwrap();
        assert!(!backup.exists());
    }

    #[test]
    fn an_uninstall_that_owns_nothing_still_clears_a_stale_snapshot() {
        // The state a half-finished install leaves (backup taken, write
        // failed), and where a user who restored by hand ends up. The
        // snapshot must not survive to be adopted as "pristine" by an install
        // years later — but the user's own file is still not ours to rewrite.
        let user = "notify = [\"my-notifier\"]\n";
        let (_d, config) = tmp_config(Some(user));
        let stray = config.with_file_name("config.toml.pingmybell.bak");
        std::fs::write(&stray, "old snapshot").unwrap();

        uninstall(&config).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), user);
        assert!(!stray.exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_keeps_a_private_config_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, config) = tmp_config(Some("model = \"o3\"\n"));
        // 0640, not 0600: no umask can synthesise it, so this fails on the
        // unfixed code whatever the machine's umask happens to be.
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
        install(&shim(), &config).unwrap();
        let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "install rewrote the config's permissions");
        let backup = config.with_file_name("config.toml.pingmybell.bak");
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "the snapshot holds the same credentials");

        uninstall(&config).unwrap();
        let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "uninstall rewrote them");
    }

    #[test]
    fn shell_metacharacters_in_the_app_path_survive_installation() {
        let (_d, path) = tmp_hooks(None);
        let weird = PathBuf::from("/opt/$HOME/`id`/o'brien/Ping My Bell/pingmybell-shim");
        install_hooks(&weird, &path).unwrap();

        let cmd = read(&path)["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(cmd.ends_with(" codex-ask"));
        #[cfg(unix)]
        {
            // Codex runs this through `$SHELL -lc`; the shim must receive the
            // path we installed, not whatever the shell expands it to.
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", cmd.trim_end_matches(" codex-ask")))
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                weird.to_string_lossy()
            );
        }
    }

    #[test]
    fn notify_argv_needs_no_quoting_even_for_a_metacharacter_path() {
        // config.toml's `notify` is an argv ARRAY, executed directly — the
        // quoting that hooks.json needs would become part of the filename.
        let (_d, config) = tmp_config(None);
        let weird = PathBuf::from("/opt/$HOME/Ping My Bell/pingmybell-shim");
        install(&weird, &config).unwrap();
        let raw = std::fs::read_to_string(&config).unwrap();
        assert!(
            raw.contains("\"/opt/$HOME/Ping My Bell/pingmybell-shim\""),
            "notify must hold the raw path: {raw}"
        );
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
