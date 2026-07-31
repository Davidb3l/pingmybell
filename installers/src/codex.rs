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

    // Codex supports a single notify program. Replacing a user's own notify
    // hook would silently break their setup — refuse instead.
    if let Some(existing) = doc.get("notify") {
        if !existing.to_string().contains(MARKER) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} already has a notify program ({}); remove it first or chain it manually",
                    config_path.display(),
                    existing.to_string().trim()
                ),
            ));
        }
    }

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
    doc["notify"] = value(notify);

    write_atomic(config_path, &doc.to_string())?;
    Ok(InstallReport {
        settings_path: config_path.to_path_buf(),
        backup_path,
        events: vec!["notify"],
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

    let ours = doc
        .get("notify")
        .is_some_and(|n| n.to_string().contains(MARKER));
    if ours {
        doc.remove("notify");
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
    fn foreign_notify_is_refused() {
        let user = "notify = [\"my-notifier\"]\n";
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
