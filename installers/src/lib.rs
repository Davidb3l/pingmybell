//! Config installers: merge PingMyBell hook entries into agent config files
//! and remove exactly those entries again (PRD AC-2.1: parse → merge → write,
//! never blind-overwrite).
//!
//! Everything here edits files the USER owns and that routinely hold secrets
//! (`env.ANTHROPIC_API_KEY`, an `apiKeyHelper`, hooks they wrote themselves).
//! Two rules follow from that and are enforced by the shared helpers below:
//! a write never widens permissions or loses bytes on a crash
//! (`write_atomic`), and an uninstall that owns nothing in a file does not
//! rewrite it at all.

pub mod claude_code;
pub mod codex;

use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// What an install touched, for UI reporting.
#[derive(Debug)]
pub struct InstallReport {
    pub settings_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub events: Vec<&'static str>,
}

/// Write `contents` to `path` atomically (temp file + rename in the same
/// directory), creating parent directories as needed. Symlinks are resolved
/// first so a dotfiles-managed settings file keeps its link.
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let resolved = resolve_symlink(path);
    let path = resolved.as_path();
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.pingmybell.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "settings".into())
    ));
    write_then_rename(&tmp, path, contents).inspect_err(|_| {
        // A failed rename (read-only filesystem, no write permission on the
        // directory) must not leave `.settings.json.pingmybell.tmp` sitting in
        // the user's ~/.claude forever — and that temp file holds a full copy
        // of their config, secrets included.
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Follow a symlinked config to the real file, so we replace what the link
/// points AT and the user's dotfiles link survives the install.
///
/// `canonicalize` does the job for a live link but fails on a DANGLING one
/// (the dotfiles repo is not checked out yet, say) — and falling back to the
/// literal path there would rename a regular file over the link, quietly
/// turning a managed symlink into a plain file. One manual hop covers that;
/// a chain of dangling links is not a real configuration.
fn resolve_symlink(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match std::fs::read_link(path) {
        Ok(target) if target.is_absolute() => target,
        Ok(target) => match path.parent() {
            Some(dir) => dir.join(target),
            None => target,
        },
        // Not a link, or nothing there at all: the common fresh-install case.
        Err(_) => path.to_path_buf(),
    }
}

fn write_then_rename(tmp: &Path, path: &Path, contents: &str) -> io::Result<()> {
    let mut file = create_as_private_as(tmp, path)?;
    file.write_all(contents.as_bytes())?;
    // Durability before visibility. Without this, a crash just after the
    // rename can leave a ZERO-LENGTH settings.json on a delayed-allocation
    // filesystem — the rename is atomic but the data behind it is not yet on
    // disk. The other order is harmless (losing only the rename leaves the old
    // file intact), so the directory itself needs no fsync.
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)
}

/// Create `path` fresh, carrying `like`'s permissions — or owner-only when
/// `like` does not exist — BEFORE a single byte is written to it.
///
/// Every file this crate creates is either a temp file that a rename will turn
/// INTO the user's config (the rename carries the temp file's mode, so a
/// default `0666 & ~umask` would silently widen a `chmod 600 settings.json`
/// the user set because the file holds an API key) or a backup copy of that
/// same config. Both hold the user's secrets in full, so neither may exist
/// even briefly at a wider mode than the original.
fn create_as_private_as(path: &Path, like: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::File::create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(like)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o600);
        // fchmod on the handle we already hold, and explicitly rather than via
        // the open mode, which the umask would only ever mask down.
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    // Windows has no mode to carry: the new file inherits the directory's ACL,
    // which is what its neighbour in the same directory has too.
    #[cfg(not(unix))]
    let _ = like;
    Ok(file)
}

/// Where the pre-PingMyBell snapshot of `path` lives.
fn backup_path_for(path: &Path, fallback: &str) -> PathBuf {
    path.with_file_name(format!(
        "{}.pingmybell.bak",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| fallback.into())
    ))
}

/// Snapshot `path` next to itself before the first install touches it.
///
/// First backup only: a reinstall must not overwrite the pristine
/// pre-PingMyBell copy with one that already contains our entries. `None`
/// means there was no file yet, so there is nothing to restore to.
fn back_up(path: &Path, fallback: &str) -> io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let backup = backup_path_for(path, fallback);
    if !backup.exists() {
        snapshot(path, &backup).inspect_err(|_| {
            // A half-written snapshot is worse than none at all: the next
            // install keeps the first backup it finds and would present this
            // one to the user as their pristine config.
            let _ = std::fs::remove_file(&backup);
        })?;
    }
    Ok(Some(backup))
}

/// Copy `from` to `to`, privately and durably.
///
/// Not `fs::copy`: that creates the destination at the default mode and only
/// fixes the permissions up afterwards, so a snapshot of a 0600 config holding
/// an API key would be world-readable for as long as the bytes take to land.
fn snapshot(from: &Path, to: &Path) -> io::Result<()> {
    let contents = std::fs::read(from)?;
    let mut file = create_as_private_as(to, from)?;
    file.write_all(&contents)?;
    file.sync_all()
}

/// Drop the snapshot on the way out of an uninstall.
///
/// The invariant is "a `.pingmybell.bak` exists exactly while PingMyBell is
/// installed into that file". Leaving one behind is worse than untidy:
/// `back_up` deliberately keeps the FIRST backup it finds, so a snapshot from
/// an uninstall a year ago would masquerade as the pristine pre-install copy
/// of a config the user has rewritten many times since — and the install
/// message points them straight at it.
///
/// Every uninstall that COMPLETES calls this, including one that found nothing
/// of ours to remove: that is exactly the state a half-finished install (we
/// took the backup, then the write failed) leaves behind, and it is also where
/// a user who restored from the backup by hand ends up. The `.bak` is
/// unambiguously ours — we named it — so removing it is not the same as
/// touching the user's config, which an uninstall owning nothing still leaves
/// byte-identical.
///
/// Best effort on purpose: a leftover file must never be the reason an
/// otherwise successful uninstall reports failure.
fn discard_backup(path: &Path, fallback: &str) {
    let _ = std::fs::remove_file(backup_path_for(path, fallback));
}

/// Quote a path for use inside a hook's command STRING (both agents run
/// command hooks through a shell — `$SHELL -lc`, ARCHITECTURE §5.2.1 — and our
/// own bundle path contains spaces).
#[cfg(not(windows))]
fn shell_quote(path: &str) -> String {
    shell_quote_posix(path)
}

#[cfg(windows)]
fn shell_quote(path: &str) -> String {
    shell_quote_windows(path)
}

/// POSIX quoting. Double quotes are NOT enough: inside them `$`, backticks and
/// `$(…)` are all still live. An app installed under a path containing
/// `$HOME` would resolve to a completely different, nonexistent path — and
/// because the shim fails open (exit 0, no stdout), the entire integration
/// would go silently dead with no error anywhere. A path segment containing
/// backticks would be EXECUTED by the login shell on every agent event.
///
/// Single quotes are literal for every character except `'` itself, which
/// cannot be escaped inside them — the standard dance is to close the quote,
/// emit an escaped quote, and reopen: `'\''`.
#[cfg(any(not(windows), test))]
fn shell_quote_posix(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Windows quoting, which is a different language: `cmd.exe` has no `$`
/// expansion, no backticks and no backslash escapes — a backslash is just a
/// path separator, so escaping it (as the old shared implementation did) would
/// corrupt `C:\Users\…`. Double quotes are the only grouping construct, and a
/// `"` cannot appear in a Windows path at all. `%VAR%` still expands and has
/// no escape available on a command line; a path containing `%` is left as-is
/// rather than mangled.
#[cfg(any(windows, test))]
fn shell_quote_windows(path: &str) -> String {
    format!("\"{path}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per process so concurrent test binaries cannot collide on a
    /// shared temp name.
    fn tmp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("pingmybell-{}-", std::process::id()))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn write_atomic_replaces_contents() {
        let dir = tmp_dir();
        let path = dir.path().join("settings.json");
        write_atomic(&path, "{\"a\":1}").unwrap();
        write_atomic(&path, "{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        // The temp file is an implementation detail, never a leftover.
        assert!(!dir.path().join(".settings.json.pingmybell.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_the_targets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        // The user chmod'ed it because it holds an API key.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&path, "{\"hooks\":{}}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "install must not widen a private config");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_creates_new_files_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let path = dir.path().join("nested").join("settings.json");
        write_atomic(&path, "{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a config we create starts private");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_an_unusual_but_deliberate_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        // Group-readable on purpose (a shared build account, say). Preserving
        // means preserving, not clamping to our own default.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&path, "{\"a\":1}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn write_atomic_cleans_up_its_temp_file_when_the_rename_fails() {
        let dir = tmp_dir();
        // A directory in the target's place: the write succeeds, the rename
        // cannot, and the temp file must not survive holding a full copy of
        // the user's config.
        let path = dir.path().join("settings.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), "x").unwrap();

        assert!(write_atomic(&path, "{\"secret\":\"sk-live\"}").is_err());
        assert!(
            !dir.path().join(".settings.json.pingmybell.tmp").exists(),
            "temp file leaked into the user's config directory"
        );
    }

    #[test]
    fn backups_are_taken_once_and_discarded_on_removal() {
        let dir = tmp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "original").unwrap();

        let backup = back_up(&path, "settings.json").unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "original");

        // Reinstall: the snapshot stays pristine, never re-taken from a file
        // that already has our entries in it.
        std::fs::write(&path, "installed").unwrap();
        back_up(&path, "settings.json").unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "original");

        discard_backup(&path, "settings.json");
        assert!(!backup.exists());
        // Idempotent: uninstalling twice is not an error.
        discard_backup(&path, "settings.json");
    }

    #[test]
    fn back_up_of_a_missing_file_reports_nothing_to_restore() {
        let dir = tmp_dir();
        let path = dir.path().join("settings.json");
        assert!(back_up(&path, "settings.json").unwrap().is_none());
        assert!(!dir.path().join("settings.json.pingmybell.bak").exists());
    }

    // ─── shell quoting ──────────────────────────────────────────────────────

    #[test]
    fn posix_quoting_neutralises_shell_metacharacters() {
        // The measured failure: `$HOME` expanded, so the hook pointed at a
        // path that does not exist and the integration was silently dead.
        assert_eq!(shell_quote_posix("/opt/$HOME/shim"), "'/opt/$HOME/shim'");
        assert_eq!(shell_quote_posix("/opt/`id`/shim"), "'/opt/`id`/shim'");
        assert_eq!(shell_quote_posix("/opt/$(id)/shim"), "'/opt/$(id)/shim'");
        assert_eq!(shell_quote_posix("/o/a b/shim"), "'/o/a b/shim'");
        assert_eq!(shell_quote_posix("/o/a\"b/shim"), "'/o/a\"b/shim'");
        // The one character single quotes cannot contain: close, escape,
        // reopen.
        assert_eq!(
            shell_quote_posix("/Users/o'brien/shim"),
            r"'/Users/o'\''brien/shim'"
        );
        // A path needing no quoting at all is still quoted — one code path,
        // and nothing downstream has to know which kind it got.
        assert_eq!(
            shell_quote_posix("/usr/local/bin/pingmybell-shim"),
            "'/usr/local/bin/pingmybell-shim'"
        );
    }

    /// The assertion that actually matters: a real shell, given our quoted
    /// string, must hand the shim back the exact path we installed. Every
    /// case here round-trips through `sh -c` the way the agents run hooks.
    #[cfg(unix)]
    #[test]
    fn posix_quoting_round_trips_through_a_real_shell() {
        for path in [
            "/Applications/Ping My Bell.app/Contents/MacOS/pingmybell-shim",
            "/opt/$HOME/pingmybell-shim",
            "/opt/${HOME}/pingmybell-shim",
            "/opt/`whoami`/pingmybell-shim",
            "/opt/$(whoami)/pingmybell-shim",
            "/Users/o'brien/pingmybell-shim",
            "/opt/say \"hi\"/pingmybell-shim",
            "/opt/a;rm -rf b/pingmybell-shim",
            "/opt/back\\slash/pingmybell-shim",
            "/usr/local/bin/pingmybell-shim",
        ] {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote_posix(path)))
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                path,
                "shell mangled {path:?}"
            );
        }
    }

    #[test]
    fn windows_quoting_leaves_backslashes_alone() {
        // cmd.exe has no backslash escape: doubling them (what the old shared
        // implementation did) corrupts every Windows path.
        assert_eq!(
            shell_quote_windows(r"C:\Program Files\Ping My Bell\pingmybell-shim.exe"),
            "\"C:\\Program Files\\Ping My Bell\\pingmybell-shim.exe\""
        );
        // `$` and backticks are inert there, so they need no special handling.
        assert_eq!(
            shell_quote_windows(r"C:\$HOME\shim.exe"),
            "\"C:\\$HOME\\shim.exe\""
        );
    }
}
