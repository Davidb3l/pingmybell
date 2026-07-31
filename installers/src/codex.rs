//! Codex CLI integration (AC-3.1): set `notify = ["<shim>", "codex"]` in
//! `~/.codex/config.toml`, preserving everything else in the file (toml_edit
//! keeps user formatting and comments). Notification-only — never touches
//! `tui.notifications` (§5.2).

use std::io;
use std::path::Path;

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
}
