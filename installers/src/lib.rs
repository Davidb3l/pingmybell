//! Config installers: merge PingMyBell hook entries into agent config files
//! and remove exactly those entries again (PRD AC-2.1: parse → merge → write,
//! never blind-overwrite).

pub mod claude_code;
pub mod codex;

use std::io;
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
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
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
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
