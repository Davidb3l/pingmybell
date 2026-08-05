//! The suite event spine bridge — the bell as a CONSUMER (ARCHITECTURE §13,
//! SUITE_CONTRACTS §2).
//!
//! Sirius dispatches, Hayven graphs, Ametrite tracks, Catryna documents — and
//! until now nothing told the HUMAN when that fleet needed them. Each repo the
//! suite works in grows an append-only log at
//! `<root>/.suite/events/<YYYY-MM-DD>.jsonl`; this module tails those logs and
//! turns the facts worth hearing into the callouts the app already knows how
//! to make.
//!
//! ## The §2 consumer rules, which are law
//!
//! - Our cursor is ours alone: `<root>/.suite/cursors/pingmybell.json`,
//!   `{file, offset}` — `file` a bare day basename, `offset` a byte position.
//! - Read WHOLE lines only. A trailing line with no `\n` means "no more yet",
//!   never "corrupt": producers append with one atomic sub-4KiB `write(2)`, so
//!   the only way to see a partial line is to catch EOF between the syscall
//!   landing and the bytes being visible. Leave it; it will be complete next
//!   poll.
//! - At EOF, roll to the lexically next day file at offset 0. Older files are
//!   SEALED — producers only ever append to today's.
//! - Never move the cursor backward.
//! - Skip malformed lines, unknown `type`s and unknown `v` SILENTLY. Foreign
//!   events are not errors; they are the normal case.
//! - An absent, empty or unreadable spine is a no-op, never a failure.
//!
//! ## Two rules of our own
//!
//! **We start at the END.** §2 lets a fresh consumer begin at the oldest file
//! or at today's offset 0. Both are wrong for a bell: launching the app at 6pm
//! would replay a day of gate failures out loud. A notifier's history is not
//! news, so first run starts at the end of the newest day file and speaks only
//! what happens next.
//!
//! **Speech is TEMPLATED, never quoted** (§9 invariant 4). A spine line is
//! data written by another tool, and this module treats it as untrusted: the
//! sentence is built from the event TYPE, the repo's own directory name, and
//! — at most — an issue id recognised out of `refs`. Never `data`, never a
//! file path, never a symbol name. "gate failed for AMT-13 in virixia" is the
//! whole vocabulary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ingest::{admit, AppState, Voice};
use crate::registry::{AgentKind, EventKind, NormalizedEvent};

/// Envelope version we understand (§2). Anything else is skipped silently.
const SPINE_VERSION: u64 = 1;

/// How often each watched root is polled. The spine is files on local disk and
/// a poll that finds nothing costs one `readdir` plus one `read`, so this can
/// afford to be brisk — the bell's whole promise is that you hear about a
/// failure while you still care.
const POLL: Duration = Duration::from_millis(1_500);

/// How often the config is re-read while NO repo is watched — the default
/// state. Long, because nothing can happen until the user opts a repo in.
const IDLE_POLL: Duration = Duration::from_secs(30);

/// How long one spoken urgent suppresses the next (§13 burst coalescing).
///
/// A fleet of workers can fail six gates in four seconds; six sentences is a
/// thing you mute, and muting the bell is the only unrecoverable failure this
/// app has.
const COALESCE_WINDOW: Duration = Duration::from_secs(10);

/// Longest repo name we will say. A directory name is chosen by the user, not
/// by a producer, but it still reaches the speaker so it still gets a bound.
const REPO_CHARS: usize = 40;

// ─── Cursor ─────────────────────────────────────────────────────────────────

/// Our position in one repo's spine (§2): a day file and a byte offset in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub file: String,
    pub offset: u64,
}

pub fn events_dir(root: &Path) -> PathBuf {
    root.join(".suite").join("events")
}

fn cursors_dir(root: &Path) -> PathBuf {
    root.join(".suite").join("cursors")
}

/// Our cursor file. The name is reserved for PingMyBell in SUITE_CONTRACTS §2
/// — cursors are consumer-owned and never shared.
fn cursor_path(root: &Path) -> PathBuf {
    cursors_dir(root).join("pingmybell.json")
}

/// A day bucket: exactly `YYYY-MM-DD.jsonl`.
fn is_day_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

/// Read our cursor, or `None` when absent, unreadable or garbage.
///
/// Never fails: a lost cursor degrades to a fresh start, which per this
/// module's own rule means "speak only what happens from now on". That is the
/// right direction to fail — a bell that says nothing is better than one that
/// recites this morning.
pub fn read_cursor(root: &Path) -> Option<Cursor> {
    let raw = std::fs::read_to_string(cursor_path(root)).ok()?;
    let cursor: Cursor = serde_json::from_str(&raw).ok()?;
    is_day_file(&cursor.file).then_some(cursor)
}

/// Persist the cursor. Best-effort and atomic: a torn cursor reads back as
/// garbage, which replays, and replaying is the one thing a notifier must not
/// do. Written via a sibling temp file and a rename, like the app config.
///
/// Returns whether it landed. The caller keeps its own copy in memory either
/// way — a repo on a read-only mount, or one checked out by another uid, must
/// still ring the bell for as long as the app is running. Persistence is a
/// restart optimisation here, not the mechanism.
fn write_cursor(root: &Path, cursor: &Cursor) -> bool {
    let dir = cursors_dir(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let path = cursor_path(root);
    let tmp = dir.join(format!("pingmybell.json.tmp.{}", std::process::id()));
    let Ok(body) = serde_json::to_string(cursor) else {
        return false;
    };
    if std::fs::write(&tmp, body).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Is `next` at or ahead of `current`? Day files sort lexically (§2), so a
/// greater name is a later day and equality falls through to the offset.
///
/// The guard exists because a cursor that goes backward re-speaks events the
/// user has already been told about, and every input here — a file on disk
/// another process can rewrite, a clock, a rolled bucket — is one we do not
/// control.
fn is_forward(current: &Cursor, next: &Cursor) -> bool {
    match next.file.cmp(&current.file) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => next.offset >= current.offset,
    }
}

// ─── Reading ────────────────────────────────────────────────────────────────

/// One spine line, parsed defensively. Only the fields we act on are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpineEvent {
    pub kind: String,
    pub refs: Vec<String>,
}

/// Parse one line into an event we might act on, or `None` to skip it.
///
/// Everything here is a silent skip by §2: malformed JSON, a non-object, an
/// envelope version we do not know, a missing or non-string `type`. `data` is
/// deliberately never read — see the module header.
pub fn parse_line(line: &str) -> Option<SpineEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    if object.get("v").and_then(Value::as_u64) != Some(SPINE_VERSION) {
        return None;
    }
    let kind = object.get("type")?.as_str()?.to_string();
    let refs = object
        .get("refs")
        .and_then(Value::as_array)
        .map(|refs| {
            refs.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(SpineEvent { kind, refs })
}

/// Day buckets in this root, lexically ascending (= chronological).
fn day_files(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(events_dir(root)) else {
        return Vec::new(); // no spine here — not an error (§2)
    };
    let mut files: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_day_file(name))
        .collect();
    files.sort();
    files
}

/// What one pass over a repo's spine produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drain {
    pub events: Vec<SpineEvent>,
    /// Where to resume, or `None` when there is no spine at all.
    pub cursor: Option<Cursor>,
}

/// Where a consumer with no cursor should begin: the END of the newest bucket.
///
/// See the module header — history is not news. `None` when there is no spine
/// yet, so the next poll can find one appearing without having decided
/// anything in the meantime.
pub fn tail_cursor(root: &Path) -> Option<Cursor> {
    let files = day_files(root);
    let newest = files.last()?;
    let offset = std::fs::metadata(events_dir(root).join(newest))
        .map(|meta| meta.len())
        .unwrap_or(0);
    Some(Cursor {
        file: newest.clone(),
        offset,
    })
}

/// Read every whole line after `from`, advancing across the daily rollover.
///
/// Never fails. An unreadable bucket stops the pass with the cursor left
/// BEFORE it, so a transient permission error retries instead of skipping a
/// day of events.
pub fn drain(root: &Path, from: &Cursor) -> Drain {
    let files = day_files(root);
    if files.is_empty() {
        return Drain {
            events: Vec::new(),
            cursor: None,
        };
    }

    // Resolve the starting bucket. A cursor naming a file that is gone (rotated
    // away, or older than anything left) resumes at the earliest bucket after
    // it — never at the beginning, which would replay.
    let start = match files.iter().position(|f| *f == from.file) {
        Some(index) => index,
        None => match files.iter().position(|f| *f > from.file) {
            Some(index) => index,
            None => {
                // Every bucket is older than the cursor: nothing new, and the
                // cursor stays exactly where it is.
                return Drain {
                    events: Vec::new(),
                    cursor: Some(from.clone()),
                };
            }
        },
    };

    let mut events = Vec::new();
    let mut cursor = Cursor {
        file: files[start].clone(),
        offset: if files[start] == from.file {
            from.offset
        } else {
            0
        },
    };

    for (index, name) in files.iter().enumerate().skip(start) {
        let offset = if index == start { cursor.offset } else { 0 };
        let bytes = match std::fs::read(events_dir(root).join(name)) {
            Ok(bytes) => bytes,
            Err(err) if index + 1 < files.len() => {
                // A SEALED bucket we cannot read. It can never grow (producers
                // only append to today's), so waiting on it is waiting
                // forever: an EACCES file, or a directory sharing the name,
                // would leave the cursor parked before it and the bell deaf to
                // every day after. Step over it and say so.
                log::warn!("spine: skipping unreadable sealed bucket {name}: {err}");
                cursor = Cursor {
                    file: files[index + 1].clone(),
                    offset: 0,
                };
                continue;
            }
            Err(_) => {
                // TODAY's file: this one genuinely can grow, and the error is
                // most likely transient (a rotate caught mid-flight). Stop
                // with the cursor before it so the next poll retries.
                break;
            }
        };

        let start_at = (offset as usize).min(bytes.len());
        let region = &bytes[start_at..];
        let last_newline = region.iter().rposition(|b| *b == b'\n');

        let mut next_offset = start_at as u64;
        if let Some(end) = last_newline {
            let complete = String::from_utf8_lossy(&region[..=end]);
            for line in complete.lines() {
                if line.is_empty() {
                    continue;
                }
                if let Some(event) = parse_line(line) {
                    events.push(event);
                }
            }
            next_offset = (start_at + end + 1) as u64;
        }

        if index + 1 < files.len() {
            // Not the newest bucket, so it is SEALED: producers only append to
            // today's file. Any trailing partial line here is a torn append
            // that can never be completed — waiting on it would wedge the
            // cursor forever, so roll past it.
            cursor = Cursor {
                file: files[index + 1].clone(),
                offset: 0,
            };
            continue;
        }

        // Today's file: stop at the last newline and resume there next poll.
        cursor = Cursor {
            file: name.clone(),
            offset: next_offset,
        };
        break;
    }

    Drain {
        events,
        cursor: Some(cursor),
    }
}

// ─── Policy (PMB-8) ─────────────────────────────────────────────────────────

/// How loudly one event type deserves to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Voice callout plus a card. Something is stuck and it needs a human.
    Urgent,
    /// A card, silently. Progress worth seeing, not worth interrupting for.
    Notice,
    /// A card, silently. Ambient state, lowest of the visible tiers.
    Info,
    /// Nothing but a debug line. The overwhelming majority of the spine.
    Log,
}

/// The default mapping (§13). Unknown types land in `Log` by construction,
/// which is also exactly what §2 demands of an unknown type: ignore it,
/// silently, and keep going.
pub fn severity(kind: &str) -> Severity {
    match kind {
        "gate.failed" | "job.blocked" => Severity::Urgent,
        "receipt.filed" | "job.completed" => Severity::Notice,
        "doc.drifted" => Severity::Info,
        _ => Severity::Log,
    }
}

/// The present-tense phrase for one event type, singular then plural.
fn phrase(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "gate.failed" => ("gate failed", "gates failed"),
        "job.blocked" => ("job blocked", "jobs blocked"),
        "receipt.filed" => ("receipt filed", "receipts filed"),
        "job.completed" => ("job completed", "jobs completed"),
        "doc.drifted" => ("docs drifted", "docs drifted"),
        // Unreachable through `severity`, which sends everything else to
        // `Log`. A neutral phrase rather than a panic: this is a notifier.
        _ => ("fleet event", "fleet events"),
    }
}

/// The repo's own directory name, bounded and stripped of anything that is not
/// plainly a name.
pub fn repo_name(root: &Path) -> String {
    let raw = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(REPO_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "a watched repo".to_string()
    } else {
        cleaned
    }
}

/// Longest board prefix we will treat as one: `AMT`, `PMB`, `SIRF`. Bounding
/// it is what keeps `issue_label` a NAMESPACE allow-list rather than a
/// narrow channel for arbitrary producer-chosen words to reach the speaker.
const PREFIX_CHARS: usize = 6;

/// An issue id recognised out of `refs`, e.g. `amt:issue/13` → `AMT-13`.
///
/// This is the ONLY thing we lift out of a spine line, and the allow-list is
/// deliberate. `refs` also carries `hayven:node/daemon/src/auth.ts/mint` and
/// `catryna:doc/architecture/auth-flow` — file paths and symbol names, which
/// are repo CONTENT. Saying those out loud would break §9 invariant 4 just as
/// surely as reading `data` would. An issue id is an opaque handle to
/// something the user already has open in another window.
pub fn issue_label(refs: &[String]) -> Option<String> {
    refs.iter().find_map(|reference| {
        let id = reference.strip_prefix("amt:issue/")?;
        // §1 allows an optional `#fragment` on a suite URI; it names a part of
        // the thing, not the thing, so it is not part of the id.
        let id = id.split('#').next().unwrap_or_default();
        if id.is_empty() || id.len() > 16 {
            return None;
        }
        if id.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("AMT-{id}"));
        }
        // Already prefixed by its own board (`PMB-6`): keep it as it is.
        let mut parts = id.splitn(2, '-');
        let prefix = parts.next()?;
        let number = parts.next()?;
        let prefixed = !prefix.is_empty()
            && prefix.len() <= PREFIX_CHARS
            && prefix.chars().all(|c| c.is_ascii_alphabetic())
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit());
        prefixed.then(|| id.to_ascii_uppercase())
    })
}

/// The sentence for a single event: the event TYPE, and at most an issue id
/// lifted out of `refs`. Nothing else — see the module header.
///
/// Deliberately WITHOUT the repo name. Everything this produces is either
/// wrapped by `speaker::callout`, which already says "… needs you in
/// {project}.", or shown on a card whose title is the project. Including it
/// here produced "The fleet needs you in virixia. gate failed for AMT-13 in
/// virixia." The location belongs to the frame, not to the fact.
pub fn describe(event: &SpineEvent) -> String {
    let (singular, _) = phrase(&event.kind);
    match issue_label(&event.refs) {
        Some(issue) => format!("{singular} for {issue}"),
        None => singular.to_string(),
    }
}

/// Small counts read better as words in a spoken sentence.
fn count_word(n: usize) -> String {
    match n {
        2 => "two".into(),
        3 => "three".into(),
        4 => "four".into(),
        5 => "five".into(),
        6 => "six".into(),
        7 => "seven".into(),
        8 => "eight".into(),
        9 => "nine".into(),
        _ => n.to_string(),
    }
}

/// One sentence for a burst. All of a kind gets that kind's plural; a mixed
/// burst gets a neutral one, because "three gates failed" would be a lie when
/// one of them was a blocked job.
pub fn summarize(events: &[SpineEvent]) -> String {
    let count = count_word(events.len());
    let first = events.first().map(|e| e.kind.as_str()).unwrap_or_default();
    if events.iter().all(|e| e.kind == first) {
        let (_, plural) = phrase(first);
        format!("{count} {plural}")
    } else {
        format!("{count} fleet alerts")
    }
}

// ─── Burst coalescing (PMB-8) ───────────────────────────────────────────────

/// Pacing for one repo's urgent events.
///
/// The shape mirrors `ingest::ActivityThrottle`: speak the leading event
/// immediately, then hold the rest of the window and let ONE sentence cover
/// them. Leading rather than trailing because a lone gate failure — the
/// overwhelmingly common case — must arrive while it still matters, not ten
/// seconds later.
#[derive(Debug, Default)]
pub struct Coalescer {
    /// When the last utterance went out; `None` means nothing is suppressed.
    spoke_at: Option<Instant>,
    /// Urgents seen since then that have not been spoken.
    held: Vec<SpineEvent>,
}

impl Coalescer {
    /// Fold this poll's urgents in and return what should be SAID now — at
    /// most one sentence per call, by construction.
    pub fn admit(&mut self, arriving: Vec<SpineEvent>, now: Instant) -> Option<String> {
        self.held.extend(arriving);

        // A window that has run its course releases whatever it was holding.
        if let Some(spoke_at) = self.spoke_at {
            if now.duration_since(spoke_at) < COALESCE_WINDOW {
                return None; // still inside the quiet window — keep holding
            }
            self.spoke_at = None;
        }

        if self.held.is_empty() {
            return None;
        }

        let sentence = if self.held.len() == 1 {
            describe(&self.held[0])
        } else {
            summarize(&self.held)
        };
        self.held.clear();
        self.spoke_at = Some(now);
        Some(sentence)
    }
}

// ─── The bridge ─────────────────────────────────────────────────────────────

/// Build the normalized event one spine fact becomes.
///
/// `session_id` is per REPO, so a repo's fleet activity is one row on the
/// board rather than a new row per event — the same shape a long-running
/// agent session has.
fn as_normalized(root: &Path, text: String, event: EventKind) -> NormalizedEvent {
    NormalizedEvent {
        agent: AgentKind::Suite,
        event,
        session_id: session_id(root),
        cwd: root.to_string_lossy().into_owned(),
        summary: Some(text),
        transcript_path: None,
        tool: None,
        terminal: None,
    }
}

/// Per-repo state the poll loop carries between ticks.
#[derive(Debug, Default)]
pub struct RootState {
    coalescer: Coalescer,
    /// We have looked at this root and it had no spine at all.
    ///
    /// This is the difference between history and news. A repo that already
    /// has a day file when we first see it is a repo with a past we should
    /// not recite. A repo whose FIRST event arrives while we are watching is
    /// the opposite: skipping to the end there would swallow the very first
    /// `gate.failed` in a newly-watched repo — silently, and exactly once,
    /// which is the worst kind of bug to find in the field.
    watched_empty: bool,
    /// Our position, authoritative while the app runs.
    ///
    /// The file under `.suite/cursors/` is a restart optimisation, not the
    /// mechanism. If it cannot be written — a read-only mount, a repo owned
    /// by another uid — the old code re-derived "no cursor" every poll and
    /// took the `Skip` branch forever, so the bell watched a repo filling
    /// with failures and never said a word, with nothing in the log to say
    /// why. In memory it keeps working; only a restart costs anything.
    cursor: Option<Cursor>,
    /// One complaint per root per run, not one every 1.5 seconds.
    warned_unwritable: bool,
}

impl RootState {
    /// Move to `cursor`, persisting it if the filesystem allows.
    fn advance(&mut self, root: &Path, cursor: Cursor) {
        if !write_cursor(root, &cursor) && !self.warned_unwritable {
            self.warned_unwritable = true;
            log::warn!(
                "spine: cannot persist a cursor under {} — the bridge still \
                 works, but it will re-read from here after a restart",
                root.display()
            );
        }
        self.cursor = Some(cursor);
    }
}

/// Where a repo with no cursor should begin.
#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    /// No spine at all yet. Remember it and wait.
    Empty,
    /// Read from here: the spine appeared while we were already watching, so
    /// everything in it is news.
    Read(Cursor),
    /// Record this and say nothing: the repo had a past before we arrived.
    Skip(Cursor),
}

/// Decide where to start in a repo we hold no cursor for.
pub fn start_at(root: &Path, watched_empty: bool) -> Start {
    let files = day_files(root);
    let Some(newest) = files.last().cloned() else {
        return Start::Empty;
    };
    if watched_empty {
        // The spine appeared while we were watching, so it is news — but only
        // the newest bucket. A whole week arriving at once is a backup being
        // restored or a checkout being copied, not a week of live failures,
        // and replaying it would be the "recites the morning at 6pm" failure
        // this module exists to avoid. Bounding the replay to one day keeps
        // the real case (the first event ever, creating today's file) exact.
        return Start::Read(Cursor {
            file: newest,
            offset: 0,
        });
    }
    Start::Skip(tail_cursor(root).unwrap_or(Cursor {
        file: newest,
        offset: 0,
    }))
}

/// The board row one repo's fleet activity shares.
fn session_id(root: &Path) -> String {
    format!("suite:{}", root.display())
}

/// One poll of one repo: drain, advance the cursor, and admit what matters.
fn poll_root(state: &Arc<AppState>, root: &Path, root_state: &mut RootState, now: Instant) {
    let mut urgent = Vec::new();

    // Where we are, preferring the position we already hold over the file.
    let from = match root_state.cursor.clone().or_else(|| read_cursor(root)) {
        Some(cursor) => Some(cursor),
        None => match start_at(root, root_state.watched_empty) {
            Start::Empty => {
                root_state.watched_empty = true;
                None
            }
            Start::Read(cursor) => {
                root_state.watched_empty = false;
                Some(cursor)
            }
            Start::Skip(cursor) => {
                // Record where the end is; read nothing this tick.
                root_state.advance(root, cursor);
                None
            }
        },
    };

    if let Some(from) = from {
        let drained = drain(root, &from);
        match &drained.cursor {
            Some(next) if is_forward(&from, next) => {
                if *next != from {
                    root_state.advance(root, next.clone());
                }
            }
            Some(next) => {
                // Refusing to go backward is the contract (§2), but doing it
                // silently is how a truncated or rewritten day file turns
                // into a bell that has quietly stopped ringing.
                log::warn!(
                    "spine: {} proposed a backward cursor ({} @{} -> {} @{}); \
                     staying put, so events written below the old offset are missed",
                    root.display(),
                    from.file,
                    from.offset,
                    next.file,
                    next.offset
                );
            }
            None => {}
        }

        for event in drained.events {
            match severity(&event.kind) {
                Severity::Urgent => urgent.push(event),
                // Seen, never said. Both quiet tiers land as `TurnComplete`:
                // it shows a toast that collapses on its own, which is what
                // "a card, not an interruption" means. `NeedsAttention` would
                // PIN a card, making the quietest tier the loudest thing on
                // screen.
                Severity::Notice | Severity::Info => {
                    // Both show a card silently; only Notice earns the chime.
                    // `doc.drifted` is ambient — the lowest visible tier
                    // should not make a sound every time a doc goes stale.
                    let chime = severity(&event.kind) == Severity::Notice;
                    admit_quietly(state, root, describe(&event), chime);
                }
                Severity::Log => log::debug!("spine: {}", event.kind),
            }
        }
    }

    // ALWAYS give the pacer its chance, even on a tick that read nothing and
    // even when the spine has vanished. Held urgents live only in memory, and
    // every early return above used to strand them: a `git clean -xdf` (this
    // directory is gitignored, so that is its EXPECTED fate) between the
    // suppressed event and the window closing meant the second gate failure
    // was never spoken at all.
    if let Some(sentence) = root_state.coalescer.admit(urgent, now) {
        let event = as_normalized(root, sentence, EventKind::NeedsAttention);
        if let Err(err) = admit(state, &event, Voice::Speak) {
            log::warn!("spine: could not admit urgent event: {err}");
        }
    }
}

fn admit_quietly(state: &Arc<AppState>, root: &Path, text: String, chime: bool) {
    let id = session_id(root);
    // A quiet fact must never overwrite a loud one. `TurnComplete` clears the
    // pinned attention card and moves the row to Done, so a receipt filed
    // three seconds after a gate failed would erase every trace of the
    // failure — card, row state, and the armed reminder with it.
    if crate::ingest::needs_attention(state, &id) {
        log::debug!("spine: holding a quiet fact while {id} still needs you");
        return;
    }
    let event = as_normalized(root, text, EventKind::TurnComplete);
    if let Err(err) = admit(state, &event, Voice::Silent) {
        log::warn!("spine: could not admit event: {err}");
        return;
    }
    if chime {
        crate::ingest::chime(state, crate::config::ChimeScenario::Notice);
    }
}

/// Start the bridge. Returns immediately; the work happens on its own thread.
///
/// A plain OS thread rather than a tokio task on purpose: every poll is
/// blocking file IO, and the reactor it would otherwise sit on is the one
/// serving the ingest endpoints the shim depends on. Nothing here may make the
/// app slower to answer a hook.
pub fn spawn(state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("spine-bridge".into())
        .spawn(move || run(state))
        .map(|_| ())
        .unwrap_or_else(|err| {
            // Best-effort to the end: no bridge is a quieter app, not a broken
            // one, and the rest of the process is unaffected.
            log::warn!("spine: bridge not started: {err}");
        });
}

fn run(state: Arc<AppState>) {
    let mut roots_state: HashMap<PathBuf, RootState> = HashMap::new();
    loop {
        // Re-read every tick so editing the config takes effect without a
        // restart, and so a root that appears later is picked up.
        let roots = crate::config::spine_roots();
        for root in &roots {
            let root_state = roots_state.entry(root.clone()).or_default();
            poll_root(&state, root, root_state, Instant::now());
        }
        // Forget state for repos no longer watched, so the map tracks the
        // watch list rather than every root ever configured.
        roots_state.retain(|root, _| roots.contains(root));
        // Watching nothing is the DEFAULT, and a disabled feature has no
        // business re-reading and re-parsing the config file every 1.5 s.
        std::thread::sleep(if roots.is_empty() { IDLE_POLL } else { POLL });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo root with a spine, ready to append to.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(events_dir(dir.path())).unwrap();
        dir
    }

    fn append(root: &Path, day: &str, line: &str) {
        let path = events_dir(root).join(format!("{day}.jsonl"));
        let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
        existing.push_str(line);
        std::fs::write(path, existing).unwrap();
    }

    fn event_line(kind: &str, refs: &str) -> String {
        format!(
            r#"{{"v":1,"id":"b3d1c0ee-0000-4000-8000-000000000000","ts":"2026-08-05T18:42:11Z","source":"sirius","type":"{kind}","refs":[{refs}],"data":{{"issue":"AMT-13","tests":["db::pool"]}}}}
"#
        )
    }

    fn ev(kind: &str) -> SpineEvent {
        SpineEvent {
            kind: kind.into(),
            refs: vec![],
        }
    }

    // ─── Cursor mechanics ───────────────────────────────────────────────

    #[test]
    fn cursor_advances_past_whole_lines_only() {
        let dir = repo();
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));
        // A producer's append caught mid-flight: no trailing newline.
        append(dir.path(), "2026-08-05", r#"{"v":1,"type":"gate.fai"#);

        let start = Cursor {
            file: "2026-08-05.jsonl".into(),
            offset: 0,
        };
        let drained = drain(dir.path(), &start);

        assert_eq!(drained.events.len(), 1, "the partial line must not count");
        let cursor = drained.cursor.unwrap();
        // The cursor stops at the last newline, so the partial line is read
        // again — whole — on the next poll.
        assert_eq!(cursor.offset as usize, event_line("gate.failed", "").len());
    }

    #[test]
    fn partial_line_is_consumed_once_it_completes() {
        let dir = repo();
        append(dir.path(), "2026-08-05", r#"{"v":1,"type":"gate.failed"#);
        let start = Cursor {
            file: "2026-08-05.jsonl".into(),
            offset: 0,
        };
        let first = drain(dir.path(), &start);
        assert!(first.events.is_empty());

        // The rest of the line lands.
        append(dir.path(), "2026-08-05", "\",\"refs\":[]}\n");
        let second = drain(dir.path(), &first.cursor.unwrap());
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].kind, "gate.failed");
    }

    #[test]
    fn rolls_to_the_next_day_at_offset_zero() {
        let dir = repo();
        append(dir.path(), "2026-08-04", &event_line("gate.failed", ""));
        append(dir.path(), "2026-08-05", &event_line("job.blocked", ""));

        let start = Cursor {
            file: "2026-08-04.jsonl".into(),
            offset: 0,
        };
        let drained = drain(dir.path(), &start);

        assert_eq!(drained.events.len(), 2, "both days drain in one pass");
        assert_eq!(drained.cursor.unwrap().file, "2026-08-05.jsonl");
    }

    #[test]
    fn sealed_day_partial_tail_does_not_wedge_the_cursor() {
        let dir = repo();
        // Yesterday ends in a torn append that can never complete.
        append(dir.path(), "2026-08-04", &event_line("gate.failed", ""));
        append(dir.path(), "2026-08-04", r#"{"v":1,"type":"tor"#);
        append(dir.path(), "2026-08-05", &event_line("job.blocked", ""));

        let drained = drain(
            dir.path(),
            &Cursor {
                file: "2026-08-04.jsonl".into(),
                offset: 0,
            },
        );

        assert_eq!(drained.events.len(), 2);
        assert_eq!(
            drained.cursor.unwrap().file,
            "2026-08-05.jsonl",
            "a sealed file's torn tail is rolled past, never waited on"
        );
    }

    #[test]
    fn cursor_never_moves_backward() {
        let ahead = Cursor {
            file: "2026-08-05.jsonl".into(),
            offset: 400,
        };
        assert!(!is_forward(
            &ahead,
            &Cursor {
                file: "2026-08-05.jsonl".into(),
                offset: 399
            }
        ));
        assert!(!is_forward(
            &ahead,
            &Cursor {
                file: "2026-08-04.jsonl".into(),
                offset: 0
            }
        ));
        assert!(is_forward(
            &ahead,
            &Cursor {
                file: "2026-08-06.jsonl".into(),
                offset: 0
            }
        ));
        assert!(is_forward(&ahead, &ahead));
    }

    #[test]
    fn a_cursor_whose_file_vanished_resumes_at_the_next_one() {
        let dir = repo();
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));

        let drained = drain(
            dir.path(),
            &Cursor {
                file: "2026-08-01.jsonl".into(), // rotated away
                offset: 900,
            },
        );

        assert_eq!(drained.events.len(), 1);
        assert_eq!(drained.cursor.unwrap().file, "2026-08-05.jsonl");
    }

    #[test]
    fn a_cursor_ahead_of_every_bucket_reads_nothing() {
        let dir = repo();
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));

        let from = Cursor {
            file: "2026-09-01.jsonl".into(),
            offset: 0,
        };
        let drained = drain(dir.path(), &from);

        assert!(drained.events.is_empty());
        assert_eq!(drained.cursor.unwrap(), from, "and it stays put");
    }

    #[test]
    fn cursor_round_trips_through_disk() {
        let dir = repo();
        let cursor = Cursor {
            file: "2026-08-05.jsonl".into(),
            offset: 128,
        };
        write_cursor(dir.path(), &cursor);
        assert_eq!(read_cursor(dir.path()), Some(cursor));
    }

    #[test]
    fn a_garbage_cursor_reads_as_none() {
        let dir = repo();
        std::fs::create_dir_all(cursors_dir(dir.path())).unwrap();
        std::fs::write(cursor_path(dir.path()), "{ not json").unwrap();
        assert_eq!(read_cursor(dir.path()), None);

        // A well-formed cursor naming something that is not a day file is
        // just as unusable.
        std::fs::write(cursor_path(dir.path()), r#"{"file":"notes.txt","offset":0}"#).unwrap();
        assert_eq!(read_cursor(dir.path()), None);
    }

    #[test]
    fn first_run_starts_at_the_end_so_history_is_never_replayed() {
        let dir = repo();
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));

        let tail = tail_cursor(dir.path()).unwrap();
        let drained = drain(dir.path(), &tail);

        assert!(
            drained.events.is_empty(),
            "launching the app must not recite the day's failures"
        );
    }

    // ─── Absent / hostile spines ────────────────────────────────────────

    #[test]
    fn an_absent_spine_is_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap(); // no .suite/ at all
        assert_eq!(tail_cursor(dir.path()), None);
        let drained = drain(
            dir.path(),
            &Cursor {
                file: "2026-08-05.jsonl".into(),
                offset: 0,
            },
        );
        assert!(drained.events.is_empty());
        assert_eq!(drained.cursor, None);
    }

    #[test]
    fn an_empty_spine_directory_is_a_no_op() {
        let dir = repo(); // .suite/events/ exists, nothing in it
        assert_eq!(tail_cursor(dir.path()), None);
        assert!(day_files(dir.path()).is_empty());
    }

    #[test]
    fn non_day_files_are_ignored() {
        let dir = repo();
        std::fs::write(events_dir(dir.path()).join("README.md"), "hi").unwrap();
        std::fs::write(events_dir(dir.path()).join("2026-8-5.jsonl"), "x").unwrap();
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));
        assert_eq!(day_files(dir.path()), vec!["2026-08-05.jsonl".to_string()]);
    }

    #[test]
    fn malformed_and_unknown_lines_are_skipped_silently() {
        for line in [
            "not json at all",
            "[1,2,3]",                                    // JSON, but not an object
            r#"{"v":2,"type":"gate.failed","refs":[]}"#,  // unknown envelope version
            r#"{"type":"gate.failed"}"#,                  // no v
            r#"{"v":1,"refs":[]}"#,                       // no type
            r#"{"v":1,"type":42}"#,                       // type not a string
            r#"{"v":"1","type":"gate.failed"}"#,          // v not a number
        ] {
            assert_eq!(parse_line(line), None, "should have skipped: {line}");
        }
    }

    #[test]
    fn an_unknown_type_parses_but_maps_to_log_only() {
        let event = parse_line(&event_line("weather.changed", "")).unwrap();
        assert_eq!(severity(&event.kind), Severity::Log);
    }

    #[test]
    fn refs_that_are_not_strings_do_not_break_a_line() {
        let line = r#"{"v":1,"type":"gate.failed","refs":[1,null,"amt:issue/13"],"data":{}}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.refs, vec!["amt:issue/13".to_string()]);
    }

    // ─── The mapping table ──────────────────────────────────────────────

    #[test]
    fn the_default_mapping_is_the_specified_one() {
        assert_eq!(severity("gate.failed"), Severity::Urgent);
        assert_eq!(severity("job.blocked"), Severity::Urgent);
        assert_eq!(severity("receipt.filed"), Severity::Notice);
        assert_eq!(severity("job.completed"), Severity::Notice);
        assert_eq!(severity("doc.drifted"), Severity::Info);
        for quiet in [
            "code.claimed",
            "code.released",
            "code.changed",
            "issue.created",
            "issue.moved",
            "issue.closed",
            "decision.recorded",
            "doc.created",
            "doc.updated",
            "doc.verified",
            "observation.added",
            "job.dispatched",
            "gate.passed",
        ] {
            assert_eq!(severity(quiet), Severity::Log, "{quiet} must stay quiet");
        }
    }

    // ─── Templated speech (§9 invariant 4) ──────────────────────────────

    #[test]
    fn a_sentence_is_built_from_type_refs_and_repo_only() {
        let event = SpineEvent {
            kind: "gate.failed".into(),
            refs: vec!["amt:issue/13".into()],
        };
        assert_eq!(describe(&event), "gate failed for AMT-13");
    }

    #[test]
    fn an_issue_ref_that_carries_its_own_prefix_is_kept() {
        assert_eq!(
            issue_label(&["amt:issue/PMB-6".to_string()]),
            Some("PMB-6".to_string())
        );
    }

    #[test]
    fn refs_that_are_repo_content_are_never_spoken() {
        // Paths and symbol names are content — §9 invariant 4 covers them the
        // same as `data` does.
        for reference in [
            "hayven:node/daemon/src/auth.ts/mint",
            "catryna:doc/architecture/auth-flow",
            "sirius:worker/sirius/oak",
            "guignet:run/2026-07-11-opus-baseline",
        ] {
            assert_eq!(issue_label(&[reference.to_string()]), None, "{reference}");
        }

        let event = SpineEvent {
            kind: "doc.drifted".into(),
            refs: vec!["catryna:doc/architecture/auth-flow".into()],
        };
        let said = describe(&event);
        assert_eq!(said, "docs drifted");
        assert!(!said.contains("auth-flow"), "a doc path must never be said");
    }

    #[test]
    fn a_hostile_issue_ref_is_dropped_rather_than_spoken() {
        for reference in [
            "amt:issue/",                                   // empty
            "amt:issue/../../etc/passwd",                   // traversal
            "amt:issue/13; rm -rf /",                       // punctuation
            "amt:issue/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", // too long
            "amt:issue/drop table sessions",                // prose
        ] {
            assert_eq!(issue_label(&[reference.to_string()]), None, "{reference}");
        }
    }

    #[test]
    fn a_repo_name_is_bounded_and_plain() {
        // CHARS, not bytes: `take(REPO_CHARS)` bounds characters, so a name
        // of multi-byte characters is within budget while `len()` (bytes) is
        // twice it. Asserting on `len()` proved nothing for anything non-ASCII.
        let long = "x".repeat(200);
        assert_eq!(repo_name(Path::new(&long)).chars().count(), REPO_CHARS);
        let wide = "é".repeat(200);
        assert_eq!(repo_name(Path::new(&wide)).chars().count(), REPO_CHARS);
        assert_eq!(repo_name(Path::new("/tmp/Ping My Bell")), "Ping My Bell");
        assert_eq!(repo_name(Path::new("/")), "a watched repo");
    }

    #[test]
    fn no_registry_type_can_make_the_bell_speak_payload_text() {
        // Every type in the SUITE_CONTRACTS §2 registry, with the payloads the
        // golden fixture carries. Nothing from `data` may reach a sentence.
        let secrets = ["db::pool_acquire", "auth-flow", "sirius/oak", "mint"];
        for kind in [
            "code.changed",
            "code.claimed",
            "code.released",
            "issue.created",
            "issue.moved",
            "issue.closed",
            "decision.recorded",
            "doc.created",
            "doc.updated",
            "doc.drifted",
            "doc.verified",
            "observation.added",
            "job.dispatched",
            "job.completed",
            "job.blocked",
            "gate.passed",
            "gate.failed",
            "receipt.filed",
            "suite.mined",
            "task.admitted",
            "run.completed",
            "report.generated",
        ] {
            let event = SpineEvent {
                kind: kind.into(),
                refs: vec![
                    "amt:issue/13".into(),
                    "hayven:node/daemon/src/auth.ts/mint".into(),
                ],
            };
            let said = describe(&event);
            for secret in secrets {
                assert!(!said.contains(secret), "{kind} leaked {secret}: {said}");
            }
        }
    }

    // ─── Burst coalescing ───────────────────────────────────────────────

    #[test]
    fn a_lone_urgent_speaks_itself() {
        let mut pacer = Coalescer::default();
        let said = pacer.admit(vec![ev("gate.failed")], Instant::now());
        assert_eq!(said, Some("gate failed".into()));
    }

    #[test]
    fn a_burst_becomes_exactly_one_sentence() {
        let mut pacer = Coalescer::default();
        let burst = vec![ev("gate.failed"), ev("gate.failed"), ev("gate.failed")];
        let said = pacer.admit(burst, Instant::now());
        assert_eq!(said, Some("three gates failed".into()));
    }

    #[test]
    fn a_mixed_burst_does_not_claim_they_were_all_gates() {
        let mut pacer = Coalescer::default();
        let burst = vec![ev("gate.failed"), ev("job.blocked")];
        let said = pacer.admit(burst, Instant::now());
        assert_eq!(said, Some("two fleet alerts".into()));
    }

    #[test]
    fn urgents_inside_the_window_are_held_then_released_as_one() {
        let mut pacer = Coalescer::default();
        let start = Instant::now();

        // The first one speaks immediately — the common case must be fast.
        assert!(pacer.admit(vec![ev("gate.failed")], start).is_some());

        // Two more land while the window is open: silence, for now.
        let inside = start + Duration::from_secs(2);
        assert_eq!(pacer.admit(vec![ev("gate.failed")], inside), None);
        let later = start + Duration::from_secs(4);
        assert_eq!(pacer.admit(vec![ev("gate.failed")], later), None);

        // When the window closes, they arrive as ONE sentence, not two.
        let after = start + COALESCE_WINDOW + Duration::from_millis(1);
        assert_eq!(
            pacer.admit(vec![], after),
            Some("two gates failed".into())
        );

        // And nothing is left over to say twice.
        let much_later = after + COALESCE_WINDOW * 2;
        assert_eq!(pacer.admit(vec![], much_later), None);
    }

    #[test]
    fn a_quiet_pacer_says_nothing() {
        let mut pacer = Coalescer::default();
        assert_eq!(pacer.admit(vec![], Instant::now()), None);
    }

    // ─── First sight of a repo ──────────────────────────────────────────

    #[test]
    fn a_repo_with_no_spine_yet_is_remembered_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(start_at(dir.path(), false), Start::Empty);
    }

    #[test]
    fn a_repo_that_already_has_a_past_starts_at_the_end() {
        let dir = repo();
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));

        let Start::Skip(cursor) = start_at(dir.path(), false) else {
            panic!("a repo with history must be skipped, not read");
        };
        assert_eq!(drain(dir.path(), &cursor).events.len(), 0);
    }

    #[test]
    fn a_spine_that_appears_while_watching_is_read_from_its_start() {
        // The bug this exists for: watch a repo before the suite has ever
        // written to it, and the FIRST gate.failed lands in a brand-new file.
        // Jumping to the end there swallows it — once, silently, which is the
        // worst way to lose the one event the bell exists for.
        let dir = repo();
        assert_eq!(start_at(dir.path(), false), Start::Empty);

        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));

        let Start::Read(cursor) = start_at(dir.path(), true) else {
            panic!("a spine that appeared under watch must be read");
        };
        let drained = drain(dir.path(), &cursor);
        assert_eq!(drained.events.len(), 1);
        assert_eq!(drained.events[0].kind, "gate.failed");
    }

    // ─── Regressions found in review ────────────────────────────────────

    #[test]
    fn an_unreadable_sealed_bucket_does_not_wedge_the_cursor_forever() {
        // A sealed bucket can never grow, so waiting for it to become
        // readable is waiting forever — and every later day is lost with it.
        // A directory wearing a day file's name reproduces EISDIR portably.
        let dir = repo();
        std::fs::create_dir(events_dir(dir.path()).join("2026-08-04.jsonl")).unwrap();
        append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));

        let drained = drain(
            dir.path(),
            &Cursor {
                file: "2026-08-04.jsonl".into(),
                offset: 0,
            },
        );

        assert_eq!(drained.events.len(), 1, "today's events must still arrive");
        assert_eq!(drained.cursor.unwrap().file, "2026-08-05.jsonl");
    }

    #[test]
    fn todays_unreadable_bucket_is_retried_rather_than_skipped() {
        // The opposite case: the NEWEST file can still grow, so an error
        // reading it is transient and the cursor must stay put.
        let dir = repo();
        std::fs::create_dir(events_dir(dir.path()).join("2026-08-05.jsonl")).unwrap();
        let from = Cursor {
            file: "2026-08-05.jsonl".into(),
            offset: 0,
        };
        let drained = drain(dir.path(), &from);
        assert!(drained.events.is_empty());
        assert_eq!(drained.cursor.unwrap(), from, "stays put to retry");
    }

    #[test]
    fn a_truncated_file_never_drags_the_cursor_backward() {
        let dir = repo();
        for _ in 0..3 {
            append(dir.path(), "2026-08-05", &event_line("gate.failed", ""));
        }
        let long = drain(
            dir.path(),
            &Cursor {
                file: "2026-08-05.jsonl".into(),
                offset: 0,
            },
        );
        let ahead = long.cursor.unwrap();

        // The file is replaced by a shorter one (a restore, or a producer
        // that broke the append-only rule).
        std::fs::write(
            events_dir(dir.path()).join("2026-08-05.jsonl"),
            event_line("job.blocked", ""),
        )
        .unwrap();

        let after = drain(dir.path(), &ahead).cursor.unwrap();
        assert!(
            !is_forward(&ahead, &after),
            "the shrunken file proposes a backward cursor"
        );
        // Which poll_root refuses to take — and now logs, because silently
        // going deaf is how this hides.
    }

    #[test]
    fn a_spine_that_appears_with_a_whole_history_replays_at_most_one_day() {
        // A backup restore or a copied checkout can make a week of buckets
        // appear at once. That is not a week of live failures, and reciting
        // it is exactly what starting at the end exists to prevent.
        let dir = repo();
        assert_eq!(start_at(dir.path(), false), Start::Empty);

        for day in ["2026-08-01", "2026-08-02", "2026-08-05"] {
            append(dir.path(), day, &event_line("gate.failed", ""));
            append(dir.path(), day, &event_line("gate.failed", ""));
        }

        let Start::Read(cursor) = start_at(dir.path(), true) else {
            panic!("a spine that appeared under watch must be read");
        };
        assert_eq!(cursor.file, "2026-08-05.jsonl", "the newest, not the oldest");
        assert_eq!(
            drain(dir.path(), &cursor).events.len(),
            2,
            "one day's worth, not the whole history"
        );
    }

    #[test]
    fn an_issue_ref_fragment_is_not_part_of_the_id() {
        // §1 allows an optional #fragment on a suite URI: it names a part of
        // the thing, not the thing.
        assert_eq!(
            issue_label(&["amt:issue/13#note-2".to_string()]),
            Some("AMT-13".to_string())
        );
    }

    #[test]
    fn a_long_alpha_prefix_is_not_a_board_prefix() {
        // The allow-list is a NAMESPACE, not a channel for producer-chosen
        // words to reach the speaker. Real prefixes are short: AMT, PMB, SIRF.
        assert_eq!(issue_label(&["amt:issue/PASSWORD-1".to_string()]), None);
        assert_eq!(
            issue_label(&["amt:issue/SIRF-42".to_string()]),
            Some("SIRF-42".to_string())
        );
    }

    #[test]
    fn a_sentence_never_carries_the_repo_name_itself() {
        // The frame supplies the location — `callout` says "… needs you in
        // {project}." Repeating it here produced "in virixia. … in virixia."
        let event = SpineEvent {
            kind: "gate.failed".into(),
            refs: vec!["amt:issue/13".into()],
        };
        assert!(!describe(&event).contains("virixia"));
        assert!(!summarize(&[event]).contains("virixia"));
    }

    // ─── The golden fixture ─────────────────────────────────────────────

    /// The canonical one-line-per-type fixture from the docs repo, if it is
    /// on this machine. It lives outside the repo, so its absence skips
    /// rather than fails — CI has no copy of a private docs checkout.
    fn golden_fixture() -> Option<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()?
            .parent()?
            .join("sirius-private-docs/suite/spine/fixture.jsonl");
        std::fs::read_to_string(path).ok()
    }

    #[test]
    fn the_golden_fixture_drains_clean() {
        let Some(fixture) = golden_fixture() else {
            // Absent on any machine without the private docs checkout, which
            // includes CI. Set PMB_REQUIRE_FIXTURE=1 there to make the skip a
            // failure instead of a silent pass.
            assert!(
                std::env::var_os("PMB_REQUIRE_FIXTURE").is_none(),
                "PMB_REQUIRE_FIXTURE is set but the golden fixture was not found"
            );
            return;
        };
        let dir = repo();
        std::fs::write(events_dir(dir.path()).join("2026-08-05.jsonl"), &fixture).unwrap();

        let drained = drain(
            dir.path(),
            &Cursor {
                file: "2026-08-05.jsonl".into(),
                offset: 0,
            },
        );

        let lines = fixture.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(
            drained.events.len(),
            lines,
            "every conformant fixture line must parse"
        );

        // Every event is either mapped or silently logged, and nothing that
        // reaches speech carries a payload.
        let mut spoke = 0;
        for event in &drained.events {
            let said = describe(event);
            assert!(!said.is_empty());
            for leak in ["db::pool", "auth-flow", "x.ts", "sirius/oak", "candidates", "virixia"] {
                assert!(!said.contains(leak), "{} leaked {leak}", event.kind);
            }
            if severity(&event.kind) != Severity::Log {
                spoke += 1;
            }
        }
        assert_eq!(
            spoke, 5,
            "exactly the five mapped types in the registry should be actionable"
        );
    }
}
