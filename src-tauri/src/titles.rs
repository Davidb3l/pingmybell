//! The user's own name for a Claude Code session.
//!
//! A cwd basename is not an identity. Two sessions in the same repo get the
//! same row label, and a session started from the home directory shows up as
//! the user's account name — neither of which tells you which conversation
//! you are looking at.
//!
//! The Claude desktop app already stores the name each session goes by, in
//! `<data dir>/Claude/claude-code-sessions/<account>/<workspace>/local_<uuid>.json`:
//!
//! ```text
//! { "cliSessionId": "5eacf01f-…", "title": "bc9 website - coding session",
//!   "titleSource": "user", "lastActivityAt": 1785615073794, … }
//! ```
//!
//! `cliSessionId` is exactly the id our hooks report, so the join needs no
//! new plumbing on our side. Sessions run from a terminal have no file here
//! and keep the cwd basename.
//!
//! Split in two on purpose:
//!
//! - [`TitleIndex`] is the read side. A lookup is a map read behind an
//!   `RwLock` and NEVER touches the disk, because it is called from the
//!   ingest handlers that agents are parked against.
//! - [`TitleScanner`] is the write side, driven from a background timer.
//!   All file I/O happens there.
//!
//! This is another app's private on-disk format and it can change without
//! warning, so every layer degrades to `None` rather than failing: a missing
//! directory, an unparseable file, or a renamed field costs us the nicer
//! label and nothing else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Deserialize;

/// Skip anything implausibly large. These files are a few KB; a huge one is
/// not the file we think it is, and reading it would be the expensive way to
/// find that out.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Titles are displayed in a notch-width row and are also SPOKEN aloud. This
/// is foreign, user-authored text arriving from another app's files, and it
/// is the only such string that reaches the speech queue — where one absurd
/// utterance would stall every announcement behind it. Cap it here, at the
/// boundary, the way `summarize` and `ingest` cap everything else external.
const MAX_TITLE_CHARS: usize = 80;

/// The fields we need. Everything else in the file is ignored, so unknown or
/// newly-added keys do not break the parse. Every field is optional: an
/// explicit `null` must cost us one title, not the whole record.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSession {
    session_id: Option<String>,
    cli_session_id: Option<String>,
    title: Option<String>,
    title_source: Option<String>,
    last_activity_at: Option<i64>,
    is_archived: Option<bool>,
}

/// A title plus enough context to settle a collision.
struct Candidate {
    title: String,
    /// Archived means the user filed this conversation away. Ranked above
    /// authorship: a live auto-named session is a better label for what is
    /// running right now than the name of one that was put away.
    live: bool,
    /// A name the user typed beats one the app generated.
    user_named: bool,
    last_activity_at: i64,
    /// Final tiebreak, so a tie resolves the same way on every scan instead
    /// of following `read_dir` order, which APFS does not define.
    session_id: String,
}

impl Candidate {
    /// Two desktop sessions CAN claim the same `cliSessionId` — importing a
    /// CLI session twice produces exactly that — so pick deliberately.
    fn beats(&self, other: &Self) -> bool {
        (
            self.live,
            self.user_named,
            self.last_activity_at,
            self.session_id.as_str(),
        ) > (
            other.live,
            other.user_named,
            other.last_activity_at,
            other.session_id.as_str(),
        )
    }
}

/// Read side: cheap to clone, safe to call from anywhere.
#[derive(Clone)]
pub struct TitleIndex {
    titles: Arc<RwLock<HashMap<String, String>>>,
}

impl TitleIndex {
    pub fn empty() -> Self {
        Self {
            titles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The desktop app's name for this CLI session, if it has one.
    ///
    /// Pure memory read. A poisoned lock yields `None` rather than a panic —
    /// losing a label must never take the ingest server down.
    pub fn lookup(&self, cli_session_id: &str) -> Option<String> {
        self.titles
            .read()
            .ok()?
            .get(cli_session_id)
            .cloned()
    }

    #[cfg(test)]
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let index = Self::empty();
        {
            let mut map = index.titles.write().unwrap();
            for (id, title) in pairs {
                map.insert((*id).to_string(), (*title).to_string());
            }
        }
        index
    }

    /// Stand in for a background scan having published new names.
    #[cfg(test)]
    pub fn replace_for_test(&self, pairs: &[(&str, &str)]) {
        let mut map = self.titles.write().unwrap();
        map.clear();
        for (id, title) in pairs {
            map.insert((*id).to_string(), (*title).to_string());
        }
    }
}

/// Write side: owns all of the file I/O. Drive it from a background thread.
pub struct TitleScanner {
    root: Option<PathBuf>,
    titles: Arc<RwLock<HashMap<String, String>>>,
}

impl TitleScanner {
    /// Scans the store behind `index`.
    ///
    /// NOTE: the Windows path is inferred, not verified — `dirs::data_dir()`
    /// is `%APPDATA%` (Roaming) there, and whether the desktop app keeps
    /// `claude-code-sessions` under Roaming or Local has not been checked on
    /// a Windows machine. A wrong guess silently costs the nicer labels and
    /// nothing else.
    pub fn new(index: &TitleIndex) -> Self {
        Self {
            root: dirs::data_dir().map(|d| d.join("Claude").join("claude-code-sessions")),
            titles: Arc::clone(&index.titles),
        }
    }

    /// One full pass. Returns whether anything actually changed, so the
    /// caller can skip re-rendering the board when nothing did.
    pub fn scan_once(&self) -> bool {
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        let mut best: HashMap<String, Candidate> = HashMap::new();
        // <root>/<account>/<workspace>/local_*.json — walk the two opaque id
        // levels rather than hardcoding them; they differ per install.
        for account in read_dir_entries(root) {
            for workspace in read_dir_entries(&account) {
                for file in read_dir_entries(&workspace) {
                    let Some(session) = read_session(&file) else {
                        continue;
                    };
                    let (Some(id), Some(title)) = (session.cli_session_id, session.title) else {
                        continue;
                    };
                    let Some(title) = sanitize(&title) else {
                        continue;
                    };
                    if id.is_empty() {
                        continue;
                    }
                    let candidate = Candidate {
                        title,
                        live: !session.is_archived.unwrap_or(false),
                        user_named: session.title_source.as_deref() == Some("user"),
                        last_activity_at: session.last_activity_at.unwrap_or(0),
                        session_id: session.session_id.unwrap_or_default(),
                    };
                    match best.get(&id) {
                        Some(existing) if !candidate.beats(existing) => {}
                        _ => {
                            best.insert(id, candidate);
                        }
                    }
                }
            }
        }
        let next: HashMap<String, String> =
            best.into_iter().map(|(id, c)| (id, c.title)).collect();
        // A store that momentarily reads as empty is far more likely to be a
        // transient (permissions, a directory being rewritten) than the user
        // deleting every session at once. Refusing to publish an empty map
        // over a populated one keeps labels from flapping back to cwd
        // basenames and returning a cycle later.
        if next.is_empty() {
            return false;
        }
        let Ok(mut current) = self.titles.write() else {
            return false;
        };
        // Replace wholesale, so a deleted session loses its title.
        if *current == next {
            return false;
        }
        *current = next;
        true
    }
}

/// Fold foreign text into something safe to render in a one-line row and to
/// hand to a speech synthesizer.
fn sanitize(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        // Invisible formatting characters would otherwise pass every
        // emptiness check below and render as a blank row: drop them
        // outright. Control characters become spaces rather than vanishing,
        // so a newline separates the words it sat between instead of
        // welding them together.
        .filter(|c| !matches!(c, '\u{200b}'..='\u{200f}' | '\u{2060}' | '\u{feff}' | '\u{00ad}'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    // Collapse runs of whitespace: a title is one line.
    let mut out = String::new();
    for word in cleaned.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        return None;
    }
    if out.chars().count() > MAX_TITLE_CHARS {
        out = out.chars().take(MAX_TITLE_CHARS - 1).collect::<String>();
        out.push('…');
    }
    Some(out)
}

/// Directory entries, or nothing at all. A missing or unreadable directory is
/// the normal case on a machine without the desktop app.
fn read_dir_entries(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn read_session(path: &Path) -> Option<DesktopSession> {
    // `local_` is the live-session prefix. The same directory holds records
    // with other lifecycles (`deleted_…`), whose titles must not resurface.
    let name = path.file_name()?.to_str()?;
    if !name.starts_with("local_") || !name.ends_with(".json") {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), json).unwrap();
    }

    /// Unique per test AND per concurrently running suite: two `cargo test`
    /// runs on one machine must not share a directory.
    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pmb-titles-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// Scanner over a fixed root, with the index it publishes into.
    fn scanner(root: &Path) -> (TitleScanner, TitleIndex) {
        let index = TitleIndex::empty();
        let scanner = TitleScanner {
            root: Some(root.to_path_buf()),
            titles: Arc::clone(&index.titles),
        };
        (scanner, index)
    }

    #[test]
    fn finds_the_title_two_directory_levels_down() {
        let root = temp_root("basic");
        write(
            &root.join("account-uuid").join("workspace-uuid"),
            "local_abc.json",
            r#"{"cliSessionId":"cli-1","title":"bc9 website - coding session","titleSource":"user"}"#,
        );
        let (scanner, index) = scanner(&root);
        assert!(scanner.scan_once());
        assert_eq!(
            index.lookup("cli-1").as_deref(),
            Some("bc9 website - coding session")
        );
        assert_eq!(index.lookup("cli-unknown"), None);
        // Nothing changed on disk, so a second pass reports no change.
        assert!(!scanner.scan_once());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_live_session_outranks_an_archived_one_even_if_the_archived_one_was_renamed() {
        let root = temp_root("archived");
        let nested = root.join("a").join("b");
        write(
            &nested,
            "local_archived.json",
            r#"{"sessionId":"s1","cliSessionId":"cli-2","title":"old spike","titleSource":"user","isArchived":true,"lastActivityAt":99}"#,
        );
        write(
            &nested,
            "local_live.json",
            r#"{"sessionId":"s2","cliSessionId":"cli-2","title":"Refactoring the parser","titleSource":"auto","lastActivityAt":1}"#,
        );
        let (scanner, index) = scanner(&root);
        scanner.scan_once();
        assert_eq!(
            index.lookup("cli-2").as_deref(),
            Some("Refactoring the parser")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn among_live_sessions_a_user_name_beats_a_generated_one() {
        let root = temp_root("collision");
        let nested = root.join("a").join("b");
        // What a duplicate import looks like: same CLI session, two files.
        write(
            &nested,
            "local_auto.json",
            r#"{"sessionId":"s1","cliSessionId":"cli-3","title":"General coding session","titleSource":"auto","lastActivityAt":99}"#,
        );
        write(
            &nested,
            "local_user.json",
            r#"{"sessionId":"s2","cliSessionId":"cli-3","title":"bc9 website","titleSource":"user","lastActivityAt":1}"#,
        );
        let (scanner, index) = scanner(&root);
        scanner.scan_once();
        assert_eq!(index.lookup("cli-3").as_deref(), Some("bc9 website"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn otherwise_the_most_recently_active_wins_and_exact_ties_are_deterministic() {
        let root = temp_root("recency");
        let nested = root.join("a").join("b");
        write(
            &nested,
            "local_old.json",
            r#"{"sessionId":"s1","cliSessionId":"cli-4","title":"stale name","titleSource":"auto","lastActivityAt":10}"#,
        );
        write(
            &nested,
            "local_new.json",
            r#"{"sessionId":"s2","cliSessionId":"cli-4","title":"current name","titleSource":"auto","lastActivityAt":20}"#,
        );
        // Identical on every ranked field except the desktop session id.
        write(
            &nested,
            "local_tie_a.json",
            r#"{"sessionId":"aaa","cliSessionId":"cli-5","title":"tie A","titleSource":"auto"}"#,
        );
        write(
            &nested,
            "local_tie_b.json",
            r#"{"sessionId":"bbb","cliSessionId":"cli-5","title":"tie B","titleSource":"auto"}"#,
        );
        let (scanner, index) = scanner(&root);
        scanner.scan_once();
        assert_eq!(index.lookup("cli-4").as_deref(), Some("current name"));
        // Whichever way `read_dir` ordered them, the higher id wins — and a
        // rescan must not flip it.
        assert_eq!(index.lookup("cli-5").as_deref(), Some("tie B"));
        assert!(!scanner.scan_once());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn garbage_and_gaps_cost_only_the_nicer_label() {
        let root = temp_root("garbage");
        let nested = root.join("a").join("b");
        write(&nested, "local_broken.json", "{not json at all");
        write(&nested, "local_blank.json", r#"{"cliSessionId":"cli-6","title":"   "}"#);
        // Invisible-but-not-whitespace: must not become a blank row.
        write(
            &nested,
            "local_zerowidth.json",
            "{\"cliSessionId\":\"cli-7\",\"title\":\"\u{200b}\u{feff}\"}",
        );
        write(&nested, "local_nocli.json", r#"{"title":"desktop only session"}"#);
        // Not a live-session record: its title must not resurface.
        write(&nested, "deleted_x.json", r#"{"cliSessionId":"cli-8","title":"tombstone"}"#);
        write(&nested, "local_notes.txt", r#"{"cliSessionId":"cli-9","title":"ignored"}"#);
        // An explicit null on the one non-Option-shaped field.
        write(
            &nested,
            "local_null.json",
            r#"{"cliSessionId":"cli-10","title":"null archived flag","isArchived":null}"#,
        );
        write(
            &nested,
            "local_good.json",
            r#"{"cliSessionId":"cli-11","title":"survivor","titleSource":"user"}"#,
        );
        let (scanner, index) = scanner(&root);
        scanner.scan_once();
        for missing in ["cli-6", "cli-7", "cli-8", "cli-9"] {
            assert_eq!(index.lookup(missing), None, "{missing} must not be titled");
        }
        assert_eq!(index.lookup("cli-10").as_deref(), Some("null archived flag"));
        // Proves the bad files above did not abort the scan, regardless of
        // the order the directory happened to yield them in.
        assert_eq!(index.lookup("cli-11").as_deref(), Some("survivor"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rename_is_picked_up_and_a_deletion_drops_the_title() {
        let root = temp_root("rename");
        let nested = root.join("a").join("b");
        write(
            &nested,
            "local_one.json",
            r#"{"cliSessionId":"cli-12","title":"before","titleSource":"user"}"#,
        );
        write(
            &nested,
            "local_two.json",
            r#"{"cliSessionId":"cli-13","title":"keeper","titleSource":"user"}"#,
        );
        let (scanner, index) = scanner(&root);
        scanner.scan_once();
        assert_eq!(index.lookup("cli-12").as_deref(), Some("before"));

        write(
            &nested,
            "local_one.json",
            r#"{"cliSessionId":"cli-12","title":"after","titleSource":"user"}"#,
        );
        assert!(scanner.scan_once());
        assert_eq!(index.lookup("cli-12").as_deref(), Some("after"));

        std::fs::remove_file(nested.join("local_one.json")).unwrap();
        assert!(scanner.scan_once());
        assert_eq!(index.lookup("cli-12"), None);
        assert_eq!(index.lookup("cli-13").as_deref(), Some("keeper"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_read_never_wipes_titles_we_already_have() {
        let root = temp_root("empty-guard");
        let nested = root.join("a").join("b");
        write(
            &nested,
            "local_one.json",
            r#"{"cliSessionId":"cli-14","title":"keep me","titleSource":"user"}"#,
        );
        let (scanner, index) = scanner(&root);
        scanner.scan_once();
        // The whole store vanishes mid-flight (permissions, a rewrite).
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!scanner.scan_once());
        assert_eq!(index.lookup("cli-14").as_deref(), Some("keep me"));
    }

    #[test]
    fn an_oversized_file_is_skipped_and_a_long_title_is_capped() {
        let root = temp_root("limits");
        let nested = root.join("a").join("b");
        let huge = "x".repeat(MAX_FILE_BYTES as usize + 1);
        write(
            &nested,
            "local_huge.json",
            &format!(r#"{{"cliSessionId":"cli-15","title":"{huge}"}}"#),
        );
        let long = "y".repeat(400);
        write(
            &nested,
            "local_long.json",
            &format!(r#"{{"cliSessionId":"cli-16","title":"{long}"}}"#),
        );
        // Newlines must not survive into a one-line row or an utterance.
        write(
            &nested,
            "local_multiline.json",
            r#"{"cliSessionId":"cli-17","title":"first line\n\nsecond line"}"#,
        );
        let (scanner, index) = scanner(&root);
        scanner.scan_once();
        assert_eq!(index.lookup("cli-15"), None, "oversized file must be skipped");
        let capped = index.lookup("cli-16").expect("long title still yields a label");
        assert_eq!(capped.chars().count(), MAX_TITLE_CHARS);
        assert!(capped.ends_with('…'));
        assert_eq!(
            index.lookup("cli-17").as_deref(),
            Some("first line second line")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_store_is_not_an_error() {
        let root = temp_root("absent");
        let (scanner, index) = scanner(&root);
        assert!(!scanner.scan_once());
        assert_eq!(index.lookup("cli-18"), None);
        // And no root at all (no home directory).
        let index = TitleIndex::empty();
        let rootless = TitleScanner {
            root: None,
            titles: Arc::clone(&index.titles),
        };
        assert!(!rootless.scan_once());
        assert_eq!(index.lookup("cli-18"), None);
    }
}
