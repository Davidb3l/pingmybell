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

use crate::{back_up, discard_backup, shell_quote, write_atomic, InstallReport};

/// Marker present in every command we install; never rename the shim binary
/// without a migration for this.
pub const MARKER: &str = "pingmybell-shim";

/// The argument tails we install, and therefore the only ones we will ever
/// delete. Derived from EVENTS so the two cannot drift apart.
fn our_tails() -> Vec<String> {
    EVENTS
        .iter()
        .map(|(_, sub, _, _)| format!("claude {sub}"))
        .collect()
}

fn is_ours(command: &str) -> bool {
    let tails = our_tails();
    let refs: Vec<&str> = tails.iter().map(String::as_str).collect();
    crate::is_our_command(command, &refs)
}

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
const EVENTS: [(&str, &str, u64, Option<&str>); 8] = [
    ("SessionStart", "session-start", 10, None),
    // Verified present in the locally installed claude 2.1.219 binary, per
    // the rule in CLAUDE.md — this is what puts a session back to working
    // when a turn begins.
    ("UserPromptSubmit", "prompt-submit", 10, None),
    ("Stop", "stop", 10, None),
    ("Notification", "notification", 10, None),
    ("SessionEnd", "session-end", 10, None),
    // The activity ticker (§12.1). NO matcher on purpose: the ticker exists
    // for the tools we do NOT gate, and a matcher-less row is what makes it
    // narrate them all (verified against claude 2.1.198 — `Read`,
    // `ToolSearch` and `TaskCreate` all arrived through it). Five seconds
    // because this fires after every single tool call and its whole budget is
    // a loopback round trip: 300 ms to connect plus 400 ms waiting for a 202
    // (`ACTIVITY_READ_TIMEOUT`), so a wedged app costs a fraction of a second
    // per tool call rather than the ten a wedged shim could hold a real park.
    ("PostToolUse", "posttool", 5, None),
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
    let before = root.clone();

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

    // Already exactly right? Touch nothing.
    //
    // Rewriting an identical file is not harmless: the agents re-review a
    // hook whose text changed, and a needless rewrite also churns the backup
    // and the file's mtime. Reinstalling after an app update — the common
    // case, and the one that was annoying — now costs nothing when the shim
    // path has not moved.
    if root == before {
        return Ok(InstallReport {
            settings_path: settings_path.to_path_buf(),
            backup_path: None,
            events: EVENTS.iter().map(|(e, _, _, _)| *e).collect(),
        });
    }

    let backup_path = back_up(settings_path, "settings.json")?;
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

    let mut removed = false;
    let mut prune_hooks_key = false;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        // Which event keys WE emptied — an empty array the user wrote
        // themselves is not ours to delete, so pruning is driven by what this
        // call removed and never by the final state.
        let mut emptied: Vec<&str> = Vec::new();
        for (event, _, _, _) in EVENTS {
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
            .expect("load_settings guarantees an object")
            .remove("hooks");
    }
    // Own nothing here? Leave the file completely alone. Rewriting it would
    // reorder and reformat a config we never installed into — serde_json's map
    // is a BTreeMap, so a plain round-trip alphabetizes the user's top-level
    // keys — and, unlike install, uninstall takes no backup. "Uninstall" is an
    // always-available tray action, so someone who never installed can click
    // it.
    if removed {
        write_atomic(settings_path, &serde_json::to_string_pretty(&root)?)?;
    }
    // Our entries are gone either way, so the pre-install snapshot has done
    // its job — including when this call found nothing to remove, which is
    // where a failed install or a hand-restore leaves things.
    discard_backup(settings_path, "settings.json");
    Ok(())
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
/// user hooks sharing a group, then drop groups WE emptied. Returns whether
/// anything of ours was actually there.
///
/// The "we emptied" part is load-bearing: a group the user wrote with an empty
/// `hooks` array — a hook they disabled and mean to fill back in — is not ours
/// to delete, so pruning has to be driven by what this call removed rather
/// than by the final state. Install calls this too, so getting it wrong
/// destroys user config on the path every first-time user runs.
fn remove_our_hooks(groups: &mut Vec<Value>) -> bool {
    let mut removed = false;
    let mut i = 0;
    while i < groups.len() {
        let mut drop_group = false;
        if let Some(inner) = groups[i].get_mut("hooks").and_then(Value::as_array_mut) {
            let before = inner.len();
            inner.retain(|h| {
                !h.get("command").and_then(Value::as_str).is_some_and(is_ours)
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
                cmd.starts_with(&shell_quote(&shim().to_string_lossy())),
                "path with spaces must reach the shell as one word: {cmd}"
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
    fn uninstall_does_not_touch_a_file_we_own_nothing_in() {
        // "Uninstall Claude Code Integration" is an always-available tray
        // action, so someone who never installed can click it. Rewriting then
        // would reorder and reformat a config we never installed into —
        // serde_json's map is a BTreeMap, so a round-trip alphabetizes the
        // user's top-level keys and their own empty `SessionStart` would be
        // pruned away — and uninstall takes no backup to restore from.
        let user = "{\"zzz\":1,\"aaa\":2,\"hooks\":{\"SessionStart\":[],\"PostToolUse\":[]}}";
        let (_d, path) = tmp_settings(Some(user));
        uninstall(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            user,
            "byte-identical: not even key order may change"
        );
    }

    #[test]
    fn uninstall_on_missing_file_is_a_noop() {
        let (_d, path) = tmp_settings(None);
        uninstall(&path).unwrap();
        assert!(!path.exists(), "must not create the file just to empty it");
    }

    #[test]
    fn a_users_own_empty_group_is_never_deleted() {
        // Regression: pruning must be driven by what WE removed, not by the
        // final state — otherwise an empty group the user wrote themselves (a
        // hook they disabled and mean to fill back in) silently vanishes. The
        // destructive path here is INSTALL, which every first-time user runs.
        let user = r#"{"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": []},
            {"matcher": "Read", "hooks": [{"type": "command", "command": "my-guard"}]}
        ]}}"#;
        let (_d, path) = tmp_settings(Some(user));

        install(&shim(), &path).unwrap();
        let groups = read(&path)["hooks"]["PreToolUse"]
            .as_array()
            .cloned()
            .unwrap();
        assert_eq!(groups.len(), 4, "two foreign groups survive install");
        assert_eq!(groups[0]["matcher"], "Bash");
        assert!(groups[0]["hooks"].as_array().unwrap().is_empty());

        uninstall(&path).unwrap();
        let groups = read(&path)["hooks"]["PreToolUse"]
            .as_array()
            .cloned()
            .unwrap();
        assert_eq!(groups.len(), 2, "only ours removed");
        assert_eq!(groups[0]["matcher"], "Bash");
        assert!(groups[0]["hooks"].as_array().unwrap().is_empty());
        assert_eq!(groups[1]["matcher"], "Read");
    }

    #[cfg(unix)]
    #[test]
    fn install_and_uninstall_keep_a_private_settings_file_private() {
        // The file holds `env.ANTHROPIC_API_KEY`, so the user chmod'ed it.
        // Widening it to 0644 on a multi-user machine is the worst thing this
        // crate can do.
        use std::os::unix::fs::PermissionsExt;
        let (_d, path) = tmp_settings(Some(r#"{"env": {"ANTHROPIC_API_KEY": "sk-live"}}"#));
        // 0640, not 0600: no umask can synthesise it, so this test fails on
        // the unfixed code whatever the machine's umask happens to be (0600
        // is exactly what `umask 077` hands a fresh file anyway).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let report = install(&shim(), &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "install rewrote the file's permissions");
        // The snapshot is a full copy of the same secrets.
        let backup = report.backup_path.unwrap();
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "the backup is as readable as the original");

        uninstall(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "uninstall rewrote them");
    }

    #[test]
    fn install_then_uninstall_is_a_round_trip() {
        // Whatever we added, we take away — including the `hooks` object
        // itself, which the user never wrote.
        let user = r#"{"model": "opus", "permissions": {"allow": ["Bash(ls:*)"]}}"#;
        let (_d, path) = tmp_settings(Some(user));
        install(&shim(), &path).unwrap();
        uninstall(&path).unwrap();

        let root = read(&path);
        assert_eq!(root, serde_json::from_str::<Value>(user).unwrap(), "{root}");
    }

    #[test]
    fn uninstall_clears_a_stale_snapshot_even_when_it_owns_nothing() {
        // Where a failed install (backup taken, write failed) or a hand
        // restore leaves things. The snapshot must not survive to be adopted
        // as "pristine" by an install years later — but the user's own file is
        // still not ours to rewrite.
        let user = "{\"zzz\":1,\"aaa\":2}";
        let (_d, path) = tmp_settings(Some(user));
        let stray = path.with_file_name("settings.json.pingmybell.bak");
        std::fs::write(&stray, "{\"model\":\"from-2024\"}").unwrap();

        uninstall(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), user);
        assert!(!stray.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_stays_a_symlink() {
        // Dotfiles-managed configs are the norm. We must write THROUGH the
        // link, both when its target exists and when it does not — replacing
        // the link with a regular file would quietly detach the user's config
        // from the repo they manage it in.
        let dir = tempfile::Builder::new()
            .prefix(&format!("pingmybell-{}-", std::process::id()))
            .tempdir()
            .unwrap();
        let real = dir.path().join("dotfiles").join("claude.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Dangling: the dotfiles repo is not checked out yet.
        install(&shim(), &link).unwrap();
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "install replaced the link with a regular file"
        );
        assert!(
            read(&real)["hooks"]["Stop"].is_array(),
            "wrote the wrong file"
        );

        // And once it points at a real file.
        uninstall(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(read(&real).get("hooks").is_none());
    }

    #[test]
    fn uninstall_discards_the_backup_it_is_responsible_for() {
        let (_d, path) = tmp_settings(Some(r#"{"model": "opus"}"#));
        let backup = install(&shim(), &path).unwrap().backup_path.unwrap();
        assert!(backup.exists());

        uninstall(&path).unwrap();
        assert!(
            !backup.exists(),
            "a stale snapshot would masquerade as the pristine copy on the \
             next install"
        );

        // And a fresh install takes a new one, of the config as it is NOW.
        std::fs::write(&path, r#"{"model": "sonnet"}"#).unwrap();
        let backup = install(&shim(), &path).unwrap().backup_path.unwrap();
        assert_eq!(read(&backup)["model"], "sonnet");
    }

    #[test]
    fn shell_metacharacters_in_the_app_path_survive_installation() {
        // A bundle under a path containing `$HOME` used to resolve to a
        // different, nonexistent path — and because the shim fails open, the
        // whole integration went silently dead with no error anywhere.
        let (_d, path) = tmp_settings(None);
        let weird = PathBuf::from("/opt/$HOME/`id`/o'brien/Ping My Bell/pingmybell-shim");
        install(&weird, &path).unwrap();

        let cmd = read(&path)["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(cmd.ends_with(" claude stop"));
        #[cfg(unix)]
        {
            // What the agent will actually do with that string.
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "printf %s {}",
                    cmd.trim_end_matches(" claude stop")
                ))
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                weird.to_string_lossy(),
                "the shell must hand the shim the path we installed"
            );
        }
    }

    #[test]
    fn refuses_to_touch_invalid_json() {
        let (_d, path) = tmp_settings(Some("{ not json"));
        assert!(install(&shim(), &path).is_err());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "{ not json", "file untouched on refusal");
    }
}
