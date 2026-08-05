//! Session registry: in-memory session map with SQLite write-through
//! (ARCHITECTURE.md §6, schema §8).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
}

impl AgentKind {
    fn as_str(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "codex" => AgentKind::Codex,
            _ => AgentKind::ClaudeCode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    /// A turn began. Without this the board is wrong for the whole length of
    /// a turn: `Stop` moves a session to Done, and nothing moved it back
    /// until the NEXT `Stop` — so a session actively working showed DONE.
    TurnStart,
    TurnComplete,
    NeedsAttention,
    PermissionRequest,
    SessionEnd,
}

impl EventKind {
    fn as_str(&self) -> &'static str {
        match self {
            EventKind::SessionStart => "session_start",
            EventKind::TurnStart => "turn_start",
            EventKind::TurnComplete => "turn_complete",
            EventKind::NeedsAttention => "needs_attention",
            EventKind::PermissionRequest => "permission_request",
            EventKind::SessionEnd => "session_end",
        }
    }
}

// Fields read from step 4 (approval card rendering).
#[derive(Debug, Deserialize)]
pub struct ToolCall {
    #[allow(dead_code)]
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub input: serde_json::Value,
}

/// Normalized event as defined by the wire protocol (ARCHITECTURE.md §4).
#[derive(Debug, Deserialize)]
pub struct NormalizedEvent {
    pub agent: AgentKind,
    pub event: EventKind,
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    // Read from step 4 (approvals); part of the wire protocol from day one.
    #[serde(default)]
    #[allow(dead_code)]
    pub tool: Option<ToolCall>,
    #[serde(default)]
    pub terminal: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Working,
    NeedsAttention,
    Done,
    Ended,
    /// Recovered from SQLite after a restart; real state unknown until the
    /// next event arrives (ARCHITECTURE.md §6).
    Unknown,
}

impl SessionState {
    fn as_str(&self) -> &'static str {
        match self {
            SessionState::Working => "working",
            SessionState::NeedsAttention => "needs_attention",
            SessionState::Done => "done",
            SessionState::Ended => "ended",
            SessionState::Unknown => "unknown",
        }
    }
}

/// Snapshot emitted to the UI via the `session-updated` Tauri event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    pub id: String,
    pub agent: AgentKind,
    pub cwd: String,
    pub title: String,
    pub state: SessionState,
    #[serde(skip)]
    pub terminal_json: Option<String>,
    pub started_at: i64,
    pub last_event_at: i64,
    /// What this session's agent is doing RIGHT NOW — the activity ticker
    /// (§12.1). Ephemeral by construction: memory only, never a row in
    /// `events`, and dropped by the next lifecycle event.
    ///
    /// `serde(skip)` because whether it is worth SHOWING is a decision, and
    /// decisions live in Rust (§project rule): the view builders
    /// (`board_rows`, `overlay::island_rows`) hand the UI a label only while
    /// the session is actually running, so no consumer of a raw `Session`
    /// snapshot can render a ticker for a session that has since finished.
    #[serde(skip)]
    pub last_activity: Option<String>,
}

impl Session {
    /// The ticker label, but only when this session is live enough for it to
    /// mean anything. `Unknown` counts: everywhere else in the app a recovered
    /// session is treated as working until an event proves otherwise (§6), and
    /// a tool call arriving for one is exactly that kind of evidence.
    pub fn activity_label(&self) -> Option<String> {
        matches!(self.state, SessionState::Working | SessionState::Unknown)
            .then(|| self.last_activity.clone())
            .flatten()
    }

    /// Un-park a session: the thing it was waiting on is resolved.
    ///
    /// The ticker is retired here for the same reason `apply` retires it —
    /// and this is the path that would otherwise sneak one past. A label
    /// recorded while the session was parked is HIDDEN, not dropped
    /// (`activity_label` gates on state), so flipping the state back without
    /// clearing it would republish a label from before the park under a live
    /// dot, and for an approved long-running command nothing would replace it
    /// for minutes.
    fn resume_working(&mut self, now: i64) {
        self.state = SessionState::Working;
        self.last_event_at = now;
        self.last_activity = None;
    }
}

/// What `Registry::record_activity` did with a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ticked {
    /// No live session with that id. A PostToolUse can arrive before its
    /// `session_start` (a hook installed mid-session) or long after recovery
    /// dropped the row, and neither is a reason to invent a board row that no
    /// lifecycle event vouches for.
    Unknown,
    /// Recorded, but nothing would render it — the session is finished or
    /// parked on the user, and a breathing "live" dot there would be a lie.
    Hidden,
    Shown,
}

/// Board row: a live session plus its latest summary.
#[derive(Debug, Clone, Serialize)]
pub struct BoardRow {
    #[serde(flatten)]
    pub session: Session,
    pub last_summary: Option<String>,
    /// Live activity label (§12.1), or None when there is nothing to tick —
    /// the row falls back to `last_summary` then.
    pub activity: Option<String>,
}

/// One history entry for the per-session drawer.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEvent {
    pub kind: String,
    pub summary: Option<String>,
    pub decision: Option<String>,
    pub created_at: i64,
}

pub struct Registry {
    inner: Mutex<Inner>,
    /// Read-only view of the desktop app's session names. Lookups are memory
    /// reads; the scanning that fills it runs on a background timer, because
    /// nothing on this path may wait on a disk.
    titles: crate::titles::TitleIndex,
}

struct Inner {
    conn: Connection,
    sessions: HashMap<String, Session>,
}

const RECOVERY_WINDOW_SECS: i64 = 24 * 60 * 60;

/// How long a session is kept after its last event.
///
/// Not a disk-space measure — the database is ~150 KB after a few days and
/// would be tens of MB after a year. It is that the table would otherwise
/// grow without bound and stand as a permanent record of every folder the
/// user has ever worked in, which sits badly next to the rest of §9. The
/// board only ever shows the last 24 h, so 30 days is already generous for
/// the history drawer, the only thing that reads back this far.
const RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

impl Registry {
    pub fn open(db_path: &Path, titles: crate::titles::TitleIndex) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        let registry = Self::from_conn(conn, titles)?;
        restrict_db_perms(db_path);
        Ok(registry)
    }

    fn from_conn(conn: Connection, titles: crate::titles::TitleIndex) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Bound the write-ahead log. SQLite's default autocheckpoint is 1000
        // pages (~4 MB) and a checkpoint RESETS the WAL rather than shrinking
        // it, so the file sits at its high-water mark forever. On macOS that
        // is not free: memory pressure spills to disk and the two budgets are
        // the same budget. Checkpoint more often and let the file be
        // truncated back down.
        conn.pragma_update(None, "wal_autocheckpoint", 256)?;
        conn.pragma_update(None, "journal_size_limit", 1_048_576)?;
        // Runs on every open, not just on a fresh file, so an existing
        // database picks up the index below without a migration step.
        //
        // There is deliberately no `settings` table: settings live in
        // ~/.pingmybell/config.json, because the shim re-reads them on every
        // hook invocation and cannot afford to link SQLite (config.rs).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, agent TEXT, cwd TEXT, title TEXT, state TEXT,
                      terminal_json TEXT, started_at INT, last_event_at INT);
             CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY, session_id TEXT, kind TEXT, summary TEXT,
                    decision TEXT NULL, created_at INT);
             -- Every read of `events` keys on session_id: the board's summary
             -- lookup (once PER LIVE SESSION, under the registry lock, and it
             -- walks the whole table backwards when a session has no non-empty
             -- summary), the history drawer, and the retention sweep. Without
             -- this they all scan 30 days of rows.
             CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);",
        )?;

        let mut registry = Inner {
            conn,
            sessions: HashMap::new(),
        };
        registry.recover(now_unix())?;
        Ok(Self {
            inner: Mutex::new(registry),
            titles,
        })
    }

    /// The registry lock, taken without treating poisoning as fatal.
    ///
    /// `apply` runs the caller's `notify` closure while still holding this
    /// lock — on purpose, so concurrent events for one session reach the UI
    /// in order — and a panic in there is the one realistic way this mutex
    /// gets poisoned. Propagating that would turn every later registry call
    /// into a panic of its own and take the ingest server down for good.
    /// Recovering is safe because nothing here can leave `Inner` structurally
    /// broken as it unwinds: the session map is only ever touched after
    /// SQLite has committed, and an in-flight transaction rolls itself back.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Apply a normalized event: update the session state machine, write
    /// through to SQLite, and return the fresh snapshot plus the inserted
    /// event row id (used to attribute approval decisions to the exact row).
    ///
    /// `notify` runs under the registry lock after a successful commit, so
    /// per-session notifications observe state transitions in order.
    pub fn apply<F>(&self, event: &NormalizedEvent, notify: F) -> rusqlite::Result<(Session, i64)>
    where
        F: FnOnce(&Session),
    {
        let now = now_unix();

        // Only Claude Code sessions can join: our id for one IS the CLI
        // session id the desktop app records. Codex ids are `codex-<hash>`.
        let desktop_title = (event.agent == AgentKind::ClaudeCode)
            .then(|| self.titles.lookup(&event.session_id))
            .flatten();

        let mut inner = self.lock();
        let inner = &mut *inner;

        let new_state = match event.event {
            EventKind::SessionStart | EventKind::TurnStart => SessionState::Working,
            EventKind::TurnComplete => SessionState::Done,
            EventKind::NeedsAttention | EventKind::PermissionRequest => {
                SessionState::NeedsAttention
            }
            EventKind::SessionEnd => SessionState::Ended,
        };

        let title = display_title(event.agent, &event.cwd, desktop_title);
        let terminal_json = event
            .terminal
            .as_ref()
            .filter(|t| !t.is_null())
            .map(|t| t.to_string());

        // Build the next snapshot without touching the map yet: memory must
        // never get ahead of what SQLite has durably recorded.
        let mut session = match inner.sessions.get(&event.session_id) {
            Some(existing) => existing.clone(),
            None => {
                // Brand new — or older than the recovery window with a row
                // still on disk. Keep the DB's original started_at so memory
                // and persistence agree, and its terminal_json too: the upsert
                // below COALESCEs that column, so the row keeps its value
                // whatever we do, and only `session-start` ever carries
                // terminal data — dropping it here would leave memory
                // permanently blank while disk still knows, and `focus_session`
                // reads memory.
                let stored: Option<(i64, Option<String>)> = inner
                    .conn
                    .query_row(
                        "SELECT started_at, terminal_json FROM sessions WHERE id = ?1",
                        params![event.session_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let (started_at, stored_terminal) = match stored {
                    Some((started_at, terminal)) => (started_at, terminal),
                    None => (now, None),
                };
                Session {
                    id: event.session_id.clone(),
                    agent: event.agent,
                    cwd: event.cwd.clone(),
                    title: title.clone(),
                    state: new_state,
                    terminal_json: stored_terminal,
                    started_at,
                    last_event_at: now,
                    last_activity: None,
                }
            }
        };
        session.agent = event.agent;
        session.cwd = event.cwd.clone();
        session.title = title;
        session.state = new_state;
        if terminal_json.is_some() {
            session.terminal_json = terminal_json;
        }
        session.last_event_at = now;
        // Every lifecycle event retires the ticker (§12.1). A label describes
        // one moment inside one turn: after `turn_complete` the row must fall
        // back to the summary, and after `turn_start` it must not show what the
        // PREVIOUS turn was doing until the first tool call of this one lands.
        session.last_activity = None;

        let tx = inner.conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions(id, agent, cwd, title, state, terminal_json, started_at, last_event_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
               agent = excluded.agent, cwd = excluded.cwd, title = excluded.title,
               state = excluded.state,
               terminal_json = COALESCE(excluded.terminal_json, sessions.terminal_json),
               last_event_at = excluded.last_event_at",
            params![
                session.id,
                session.agent.as_str(),
                session.cwd,
                session.title,
                session.state.as_str(),
                session.terminal_json,
                session.started_at,
                session.last_event_at,
            ],
        )?;
        tx.execute(
            "INSERT INTO events(session_id, kind, summary, decision, created_at)
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![session.id, event.event.as_str(), event.summary, now],
        )?;
        let event_id = tx.last_insert_rowid();
        tx.commit()?;

        // Commit succeeded — now it is safe to update memory. Ended sessions
        // leave the live map (history stays in SQLite); recovery also skips
        // them, so the board only ever sees live sessions.
        if event.event == EventKind::SessionEnd {
            inner.sessions.remove(&event.session_id);
        } else {
            inner
                .sessions
                .insert(event.session_id.clone(), session.clone());
        }
        notify(&session);

        Ok((session, event_id))
    }

    /// Record an approval decision on its exact permission_request event row
    /// (AC-6.4). When `resume` is set (no sibling approvals still pending),
    /// the session moves back to Working; the returned snapshot, if any,
    /// should be emitted to the UI.
    pub fn record_decision(
        &self,
        session_id: &str,
        event_id: i64,
        decision: &str,
        resume: bool,
    ) -> rusqlite::Result<Option<Session>> {
        let now = now_unix();
        let mut guard = self.lock();
        let Inner { conn, sessions } = &mut *guard;

        // One transaction: the decision on the event row and the state it
        // releases are the same fact, and a crash between them would leave a
        // decided approval on a session still reading "waiting on you".
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE events SET decision = ?1 WHERE id = ?2",
            params![decision, event_id],
        )?;
        let resuming = resume && sessions.contains_key(session_id);
        if resuming {
            tx.execute(
                "UPDATE sessions SET state = 'working', last_event_at = ?1 WHERE id = ?2",
                params![now, session_id],
            )?;
        }
        tx.commit()?;

        // Memory only after the commit — the same order the rest of this type
        // keeps. It used to move first, so a failed UPDATE left memory saying
        // working while disk still said needs_attention.
        if !resuming {
            return Ok(None);
        }
        let Some(session) = sessions.get_mut(session_id) else {
            return Ok(None);
        };
        session.resume_working(now);
        Ok(Some(session.clone()))
    }

    /// Record what a session's agent is doing right now (§12.1, step 12).
    ///
    /// Deliberately NOT `apply`: this writes NOTHING to SQLite, moves no state
    /// machine, and does not touch `last_event_at` — a busy turn is hundreds of
    /// tool calls, and persisting them would swamp a table sized for lifecycle
    /// events, distort every age the board renders, and (via `last_event_at`)
    /// keep sessions alive past retention. Activity is also worthless after a
    /// restart, which is why recovery leaves it None.
    ///
    /// Returns whether there is anything for a surface to draw — deliberately
    /// NOT the session snapshot. This runs hundreds of times a
    /// turn on the registry mutex, and cloning five heap strings per tool call
    /// to answer a yes/no question is work done under the lock that guards
    /// every SQLite write in the app.
    pub fn record_activity(&self, session_id: &str, label: &str) -> Ticked {
        let mut guard = self.lock();
        let Some(session) = guard.sessions.get_mut(session_id) else {
            return Ticked::Unknown;
        };
        session.last_activity = Some(label.to_string());
        // `activity_label` is the one place that decides whether a label is
        // worth showing; asking it here means a parked or finished session
        // that keeps receiving tool calls costs no emits at all.
        match session.activity_label() {
            Some(_) => Ticked::Shown,
            None => Ticked::Hidden,
        }
    }

    /// Return a session that is waiting on the user back to Working, for when
    /// the thing it was waiting for ended WITHOUT a decision — an approval that
    /// timed out, a deferred question, a shim that died while parked. Returns
    /// whether anything changed.
    ///
    /// `record_decision` covers the one path that ends in an answer. Every
    /// other way a park can end used to leave the row reading "waiting on you"
    /// until the app restarted: harmless-looking but wrong for the rest of the
    /// turn when the user just approved in the terminal instead, and a stranded
    /// session when the agent was killed.
    pub fn clear_attention_state(&self, session_id: &str) -> bool {
        let now = now_unix();
        let mut guard = self.lock();
        let Inner { conn, sessions } = &mut *guard;

        // Only ever unsticks a park. A session that has since moved on — done,
        // ended, working again — must not be dragged backwards by a late
        // timeout for an approval it already answered.
        match sessions.get(session_id) {
            Some(session) if session.state == SessionState::NeedsAttention => {}
            _ => return false,
        }

        match conn.execute(
            "UPDATE sessions SET state = 'working', last_event_at = ?1 WHERE id = ?2",
            params![now, session_id],
        ) {
            // No row on disk to correct (a `delete` raced us): leaving memory
            // alone keeps the two in step, which is the whole discipline here.
            Ok(0) => return false,
            Ok(_) => {}
            Err(err) => {
                // Nothing the caller can do about it, and memory must not run
                // ahead of disk: leave the session parked and say so.
                log::warn!("registry: could not clear attention state for {session_id}: {err}");
                return false;
            }
        }

        let Some(session) = sessions.get_mut(session_id) else {
            return false;
        };
        session.resume_working(now);
        true
    }

    /// Live sessions enriched with their most recent summary, for the board
    /// (FR-7 AC-7.1).
    pub fn board_rows(&self) -> Vec<BoardRow> {
        let inner = self.lock();
        inner
            .sessions
            .values()
            .map(|session| {
                let last_summary = inner
                    .conn
                    .query_row(
                        "SELECT summary FROM events
                         WHERE session_id = ?1 AND summary IS NOT NULL AND summary != ''
                         ORDER BY id DESC LIMIT 1",
                        params![session.id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten();
                BoardRow {
                    activity: session.activity_label(),
                    session: session.clone(),
                    last_summary,
                }
            })
            .collect()
    }

    /// Recent history for one session, newest first (FR-7 AC-7.3: last 50).
    pub fn history(&self, session_id: &str, limit: usize) -> rusqlite::Result<Vec<HistoryEvent>> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare(
            "SELECT kind, summary, decision, created_at FROM events
             WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], |row| {
            Ok(HistoryEvent {
                kind: row.get(0)?,
                summary: row.get(1)?,
                decision: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// The session that has been waiting on the user longest, skipping ids the
    /// caller has already sent them to (§12.2, the triage hotkey).
    ///
    /// "Longest" is by `last_event_at`, which for a parked session is the
    /// moment it started waiting — nothing moves it again until the wait ends,
    /// so the oldest timestamp IS the longest wait. The skip list is what makes
    /// repeated presses cycle: jumping does not answer anything, so without it
    /// every press would land on the same session forever.
    pub fn oldest_waiting(&self, skip: &[String]) -> Option<Session> {
        let inner = self.lock();
        inner
            .sessions
            .values()
            .filter(|session| session.state == SessionState::NeedsAttention)
            .filter(|session| !skip.iter().any(|id| id == &session.id))
            // `id` breaks ties: two sessions parked in the same second must
            // not swap places between presses, or the cycle can revisit one
            // and never reach the other.
            .min_by(|a, b| {
                a.last_event_at
                    .cmp(&b.last_event_at)
                    .then_with(|| a.id.cmp(&b.id))
            })
            .cloned()
    }

    /// How many live sessions have not said anything since the app started.
    ///
    /// Recovery restores sessions as `Unknown` (§6), and an `Unknown` session
    /// is not a triage target — it may be parked, working, or long dead, and
    /// jumping to it on a guess is worse than not. But it is also the one case
    /// where "all clear" would be claiming more than we know, so the caller
    /// can say so instead.
    pub fn unreported_count(&self) -> usize {
        let inner = self.lock();
        inner
            .sessions
            .values()
            .filter(|session| session.state == SessionState::Unknown)
            .count()
    }

    /// Look up one live session by id.
    pub fn get(&self, session_id: &str) -> Option<Session> {
        let inner = self.lock();
        inner.sessions.get(session_id).cloned()
    }

    /// A registry on an in-memory database, for unit tests in sibling
    /// modules (`from_conn` is private, and a test must never open the
    /// developer's real one).
    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::from_conn(
            Connection::open_in_memory()?,
            crate::titles::TitleIndex::empty(),
        )
    }

    /// Move one session's clock. Tests that care about ORDER need it fixed;
    /// every real event stamps `now`, so without this two sessions parked in
    /// the same second are indistinguishable.
    #[cfg(test)]
    pub fn set_last_event_at_for_test(&self, session_id: &str, when: i64) {
        let mut guard = self.lock();
        let Inner { conn, sessions } = &mut *guard;
        let session = sessions
            .get_mut(session_id)
            .expect("test session must exist — a typo'd id would silently keep now()");
        session.last_event_at = when;
        conn.execute(
            "UPDATE sessions SET last_event_at = ?1 WHERE id = ?2",
            params![when, session_id],
        )
        .unwrap();
    }

    /// Force a full checkpoint and truncate the WAL back to zero.
    ///
    /// Called on an idle timer: autocheckpoint alone keeps the WAL bounded
    /// but never returns the space, and this app runs for days at a time.
    /// Best-effort — a busy checkpoint just means readers are active and the
    /// next tick will get it.
    pub fn checkpoint(&self) {
        let inner = self.lock();
        match inner
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            }) {
            Ok(0) => log::debug!("registry: wal checkpointed"),
            Ok(_) => log::debug!("registry: wal checkpoint busy, will retry"),
            Err(err) => log::debug!("registry: wal checkpoint failed: {err}"),
        }
    }

    /// Drop sessions whose last event is older than [`RETENTION_SECS`],
    /// along with their history. Returns how many sessions went.
    ///
    /// A session this old is not live under any definition — the board stops
    /// loading anything past 24 h — so this also drops it from memory, which
    /// matters for an app that stays up for weeks.
    pub fn prune(&self) -> rusqlite::Result<usize> {
        self.prune_at(now_unix())
    }

    /// Split out so tests can move the clock instead of waiting a month.
    fn prune_at(&self, now: i64) -> rusqlite::Result<usize> {
        let cutoff = now - RETENTION_SECS;
        let mut guard = self.lock();
        let Inner { conn, sessions } = &mut *guard;
        let tx = conn.transaction()?;

        // Sessions this process is tracking are NEVER swept, however old the
        // clock says they are. Two things fall out of that:
        //
        // - The board renders from this map, so nothing can vanish from
        //   underneath it and leave a row that opens an empty drawer.
        // - A wrong clock — a VM restored from a snapshot, a dead RTC, a
        //   dual-boot machine writing local time — cannot take out the work
        //   in front of the user. It can still reach genuinely idle history,
        //   which is the bounded version of that failure.
        //
        // A session left in memory by a process that has been up for weeks
        // is therefore immortal until the next restart, when recovery drops
        // anything past its 24 h window and the following sweep collects it.
        let candidates: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM sessions WHERE last_event_at < ?1")?;
            let rows = stmt.query_map(params![cutoff], |row| row.get::<_, String>(0))?;
            rows.flatten()
                .filter(|id| !sessions.contains_key(id))
                .collect()
        };
        for id in &candidates {
            // History first: a crash between the two must not strand events
            // pointing at a session that no longer exists.
            tx.execute("DELETE FROM events WHERE session_id = ?1", params![id])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        }
        // History that outlived its session — from a crash mid-delete, or
        // from any past bug. Nothing can ever read it. NOT EXISTS rather
        // than NOT IN: the latter silently matches nothing at all if any id
        // is NULL, and would report success while doing none of this.
        let orphans = tx.execute(
            "DELETE FROM events WHERE NOT EXISTS
               (SELECT 1 FROM sessions WHERE sessions.id = events.session_id)",
            [],
        )?;
        tx.commit()?;

        let removed = candidates.len();
        if removed > 0 || orphans > 0 {
            // Deleted pages are reused, not returned, so without this the
            // file only ever sits at its high-water mark — which is the one
            // thing a retention sweep is supposed to prevent. Orphans count
            // too: a sweep can free real space without removing a session.
            // Cheap here: this runs daily and almost always frees nothing.
            if let Err(err) = conn.execute_batch("VACUUM") {
                log::warn!("registry: vacuum after prune failed: {err}");
            }
            log::info!("registry: pruned {removed} sessions, {orphans} orphaned events");
        }
        Ok(removed)
    }

    /// Forget a session completely: the row, and every event recorded
    /// against it. Returns whether there was anything to delete.
    ///
    /// This is a hard delete, and the only one in the app. Nothing else ever
    /// removes a session — rows simply stop being loaded once they age out
    /// of the recovery window — so a session the user wants GONE (a stale
    /// import, a mistake) had no way to leave the board at all.
    ///
    /// Deleting a session that is still running is allowed and is not
    /// destructive in the same way: its next event re-creates the row. The
    /// UI says so rather than blocking it.
    pub fn delete(&self, session_id: &str) -> rusqlite::Result<bool> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let tx = inner.conn.transaction()?;
        // Events first: a crash between the two must not strand history
        // pointing at a session that no longer exists.
        tx.execute(
            "DELETE FROM events WHERE session_id = ?1",
            params![session_id],
        )?;
        let rows = tx.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
        tx.commit()?;
        // Memory only after the commit, matching the write-through order the
        // rest of this type keeps.
        let was_live = inner.sessions.remove(session_id).is_some();
        Ok(rows > 0 || was_live)
    }

    /// Re-label sessions from the title index, and report whether anything
    /// moved so the caller can skip a re-render.
    ///
    /// The event path already titles sessions as they report in, but that
    /// only ever covers sessions that are still talking. This is what gives
    /// a name to rows restored at startup, and what lets a rename in the
    /// desktop app reach a session that has already finished.
    pub fn retitle(&self) -> bool {
        let mut guard = self.lock();
        let Inner { conn, sessions } = &mut *guard;
        let mut changed = false;
        for session in sessions.values_mut() {
            let desktop = (session.agent == AgentKind::ClaudeCode)
                .then(|| self.titles.lookup(&session.id))
                .flatten();
            let next = display_title(session.agent, &session.cwd, desktop);
            if session.title == next {
                continue;
            }
            session.title = next;
            // Keep SQLite in step: a restart must not resurrect the old
            // label from a row we already corrected in memory.
            let _ = conn.execute(
                "UPDATE sessions SET title = ?1 WHERE id = ?2",
                params![session.title, session.id],
            );
            changed = true;
        }
        changed
    }

    /// All live sessions, for rendering a full board snapshot (step 6).
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<Session> {
        let inner = self.lock();
        inner.sessions.values().cloned().collect()
    }
}

impl Inner {
    /// Startup recovery (§6): load sessions younger than 24 h and mark them
    /// `unknown` until their next event.
    fn recover(&mut self, now: i64) -> rusqlite::Result<()> {
        let cutoff = now - RECOVERY_WINDOW_SECS;
        let mut stmt = self.conn.prepare(
            "SELECT id, agent, cwd, title, terminal_json, started_at, last_event_at, state
             FROM sessions WHERE last_event_at >= ?1 AND state != 'ended'",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok(Session {
                id: row.get(0)?,
                agent: AgentKind::from_str(&row.get::<_, String>(1)?),
                cwd: row.get(2)?,
                title: row.get(3)?,
                // A finished turn STAYS finished. Nothing about our restart
                // makes it un-complete, and blanking it was actively wrong:
                // `working` and `needs_attention` genuinely become unknowable
                // while we are down (the agent may have died), but `done` is
                // a fact. Codex made the cost visible — it only ever emits on
                // turn completion, so its rows sat at the faint "…" for hours
                // after every restart instead of reading "done".
                state: match row.get::<_, String>(7)?.as_str() {
                    "done" => SessionState::Done,
                    _ => SessionState::Unknown,
                },
                terminal_json: row.get(4)?,
                started_at: row.get(5)?,
                last_event_at: row.get(6)?,
                // Nothing survives a restart here on purpose (§12.1): a label
                // says what an agent is doing at this instant, and we were not
                // running for that instant.
                last_activity: None,
            })
        })?;
        // Titles are NOT resolved here: recovery runs on the main thread
        // during setup. The first background scan re-labels these rows a
        // moment later via `retitle`.
        for session in rows {
            let session = session?;
            self.sessions.insert(session.id.clone(), session);
        }
        drop(stmt);
        self.conn.execute(
            "UPDATE sessions SET state = 'unknown'
             WHERE last_event_at >= ?1 AND state NOT IN ('ended', 'done')",
            params![cutoff],
        )?;
        Ok(())
    }
}

/// All files in `~/.pingmybell/` must be user-only (§9 invariant 1). SQLite
/// creates the db 0644 by default; -wal/-shm inherit the main db's mode, so
/// tightening the base file covers files created later.
fn restrict_db_perms(db_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let base = db_path.as_os_str().to_owned();
        for suffix in ["", "-wal", "-shm"] {
            let mut name = base.clone();
            name.push(suffix);
            let path = std::path::PathBuf::from(name);
            if path.exists() {
                if let Err(err) =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                {
                    log::warn!(
                        "could not restrict permissions on {}: {err}",
                        path.display()
                    );
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = db_path;
}

/// The label a board row carries: the name the user gave the conversation
/// when the desktop app knows one, else the project it is running in.
///
/// A cwd basename cannot tell two sessions in one repo apart, and names a
/// home-directory session after the user's account — which is why the
/// desktop title wins whenever there is one.
fn display_title(agent: AgentKind, cwd: &str, desktop: Option<String>) -> String {
    let title = desktop.unwrap_or_else(|| project_title(cwd));
    let title = title.trim();
    // A cwd of "/" or an empty one makes an unreadable board row; fall back
    // to the agent name. Whitespace-only counts as empty: the board's delete
    // gate asks the user to TYPE the title back, so a row labelled with
    // nothing but spaces could never be confirmed and so could never be
    // removed.
    if title.is_empty() || title == "/" {
        return agent.as_str().to_string();
    }
    title.to_string()
}

fn project_title(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write-ahead log must stay bounded: this app runs for days, and on
    /// macOS disk pressure IS memory pressure (the compressor spills to swap),
    /// so a WAL parked at its high-water mark is not free.
    #[test]
    fn wal_is_bounded_and_checkpoint_truncates() {
        let dir = std::env::temp_dir().join(format!("pmb-wal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        let registry = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();

        {
            let inner = registry.inner.lock().unwrap();
            let mode: String = inner
                .conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode.to_lowercase(), "wal");
            let limit: i64 = inner
                .conn
                .query_row("PRAGMA journal_size_limit", [], |r| r.get(0))
                .unwrap();
            assert_eq!(limit, 1_048_576, "WAL must be truncated back after checkpoint");
            let autockpt: i64 = inner
                .conn
                .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
                .unwrap();
            assert_eq!(autockpt, 256, "checkpoint far more often than the 1000-page default");
        }

        // Write enough to create a WAL, then prove the checkpoint clears it.
        for i in 0..200 {
            let event = NormalizedEvent {
                agent: AgentKind::ClaudeCode,
                event: EventKind::TurnComplete,
                session_id: format!("s{i}"),
                cwd: "/tmp/x".into(),
                summary: Some("x".into()),
                transcript_path: None,
                tool: None,
                terminal: None,
            };
            registry.apply(&event, |_| {}).unwrap();
        }
        let wal = dir.join("t.db-wal");
        registry.checkpoint();
        let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(
            after <= 1_048_576,
            "checkpoint must truncate the WAL, got {after} bytes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_registry() -> Registry {
        // Empty index: the unit tests must not read the developer's real
        // desktop-app store, or their expected titles would depend on it.
        Registry::from_conn(
            Connection::open_in_memory().unwrap(),
            crate::titles::TitleIndex::empty(),
        )
        .unwrap()
    }

    /// Age a session on disk AND drop it from the live map — the state a
    /// row is in after a restart, when recovery skipped it for being past
    /// the 24 h window. This is the only state prune can act on.
    fn age_on_disk(registry: &Registry, id: &str, secs_ago: i64) {
        let when = now_unix() - secs_ago;
        let mut inner = registry.inner.lock().unwrap();
        inner
            .conn
            .execute(
                "UPDATE sessions SET last_event_at = ?1 WHERE id = ?2",
                params![when, id],
            )
            .unwrap();
        inner.sessions.remove(id);
    }

    fn started(registry: &Registry, id: &str) {
        registry
            .apply(&event(EventKind::SessionStart, id), |_| {})
            .unwrap();
        registry
            .apply(&event(EventKind::TurnComplete, id), |_| {})
            .unwrap();
    }

    #[test]
    fn recovery_keeps_a_finished_session_finished() {
        let dir = std::env::temp_dir().join(format!("pmb-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("registry.db");
        {
            let r = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
            r.apply(&event(EventKind::TurnComplete, "finished"), |_| {})
                .unwrap();
            r.apply(&event(EventKind::SessionStart, "busy"), |_| {})
                .unwrap();
            r.apply(&event(EventKind::NeedsAttention, "waiting"), |_| {})
                .unwrap();
        }
        let reopened = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
        // A completed turn is a fact that survives our restart.
        assert_eq!(reopened.get("finished").unwrap().state, SessionState::Done);
        // These two genuinely became unknowable while we were down: the agent
        // may have died, and we must not claim it is still working or still
        // waiting on the user.
        assert_eq!(reopened.get("busy").unwrap().state, SessionState::Unknown);
        assert_eq!(
            reopened.get("waiting").unwrap().state,
            SessionState::Unknown
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_turn_moves_a_finished_session_back_to_working() {
        let registry = test_registry();
        let (done, _) = registry
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        assert_eq!(done.state, SessionState::Done);
        // Without this the row read DONE for the entire length of the next
        // turn — the whole point of the event.
        let (working, _) = registry
            .apply(&event(EventKind::TurnStart, "s1"), |_| {})
            .unwrap();
        assert_eq!(working.state, SessionState::Working);
    }

    #[test]
    fn prune_sweeps_sessions_past_retention_and_leaves_the_rest_alone() {
        let registry = test_registry();
        for id in ["old", "recent", "boundary"] {
            started(&registry, id);
        }
        age_on_disk(&registry, "old", RETENTION_SECS + 60);
        age_on_disk(&registry, "recent", RETENTION_SECS - 3600);
        // Exactly at the cutoff: the one value where < and <= differ.
        age_on_disk(&registry, "boundary", RETENTION_SECS);

        assert_eq!(registry.prune_at(now_unix()).unwrap(), 1);
        assert!(registry.history("old", 50).unwrap().is_empty());
        assert!(!registry.history("recent", 50).unwrap().is_empty());
        assert!(
            !registry.history("boundary", 50).unwrap().is_empty(),
            "a session exactly at the cutoff must survive"
        );

        // Nothing left to sweep: a second pass is a no-op.
        assert_eq!(registry.prune_at(now_unix()).unwrap(), 0);
    }

    #[test]
    fn prune_never_touches_a_session_this_process_is_tracking() {
        let registry = test_registry();
        started(&registry, "live");
        // Ancient on disk, but still in the live map — a clock that jumped
        // must not be able to delete the work in front of the user.
        {
            let inner = registry.inner.lock().unwrap();
            inner
                .conn
                .execute(
                    "UPDATE sessions SET last_event_at = 0 WHERE id = 'live'",
                    [],
                )
                .unwrap();
        }
        assert_eq!(registry.prune_at(now_unix()).unwrap(), 0);
        assert!(registry.get("live").is_some());
        assert!(!registry.history("live", 50).unwrap().is_empty());
    }

    #[test]
    fn prune_keys_on_the_session_row_not_on_event_timestamps() {
        let registry = test_registry();
        started(&registry, "s1");
        registry.inner.lock().unwrap().sessions.remove("s1");
        // Ancient history under a session that is otherwise current.
        {
            let inner = registry.inner.lock().unwrap();
            inner
                .conn
                .execute("UPDATE events SET created_at = 0", [])
                .unwrap();
        }
        assert_eq!(registry.prune_at(now_unix()).unwrap(), 0);
        assert!(!registry.history("s1", 50).unwrap().is_empty());
    }

    #[test]
    fn a_pruned_session_stays_gone_and_the_db_stays_user_only() {
        let dir = std::env::temp_dir().join(format!("pmb-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("registry.db");
        {
            // A real file in WAL mode, so the VACUUM actually runs against
            // one — the in-memory connection never exercises that path.
            let registry = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
            started(&registry, "old");
            started(&registry, "keep");
            age_on_disk(&registry, "old", RETENTION_SECS + 60);
            age_on_disk(&registry, "keep", 60);
            assert_eq!(registry.prune_at(now_unix()).unwrap(), 1);
        }
        // VACUUM rewrites the main database file; §9 invariant 1 says every
        // file in ~/.pingmybell must stay user-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "vacuum must not loosen the db file mode");
        }
        let reopened = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
        assert!(reopened.history("old", 50).unwrap().is_empty());
        assert!(!reopened.history("keep", 50).unwrap().is_empty());
        assert_eq!(reopened.board_rows().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_collects_history_whose_session_is_already_gone() {
        let registry = test_registry();
        registry
            .apply(&event(EventKind::SessionStart, "s1"), |_| {})
            .unwrap();
        // Orphan the events the way a crash mid-delete would.
        {
            let inner = registry.inner.lock().unwrap();
            inner
                .conn
                .execute("DELETE FROM sessions WHERE id = 's1'", [])
                .unwrap();
        }
        registry.inner.lock().unwrap().sessions.remove("s1");
        assert!(!registry.history("s1", 50).unwrap().is_empty());

        registry.prune_at(now_unix()).unwrap();
        assert!(registry.history("s1", 50).unwrap().is_empty());
    }

    #[test]
    fn delete_removes_the_row_its_history_and_the_live_snapshot() {
        let registry = test_registry();
        registry
            .apply(&event(EventKind::SessionStart, "s1"), |_| {})
            .unwrap();
        registry
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        // A second session must be untouched by the first one's deletion.
        registry
            .apply(&event(EventKind::SessionStart, "s2"), |_| {})
            .unwrap();
        assert!(!registry.history("s1", 50).unwrap().is_empty());

        assert!(registry.delete("s1").unwrap());
        assert!(registry.get("s1").is_none());
        assert!(registry.history("s1", 50).unwrap().is_empty());
        assert_eq!(registry.board_rows().len(), 1);
        assert!(registry.get("s2").is_some());
        assert!(!registry.history("s2", 50).unwrap().is_empty());

        // Idempotent: deleting what is already gone is not an error.
        assert!(!registry.delete("s1").unwrap());
        assert!(!registry.delete("never-existed").unwrap());
    }

    #[test]
    fn a_deleted_session_does_not_come_back_from_disk() {
        let dir = std::env::temp_dir().join(format!("pmb-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("registry.db");
        {
            let registry = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
            registry
                .apply(&event(EventKind::SessionStart, "s1"), |_| {})
                .unwrap();
            assert!(registry.delete("s1").unwrap());
        }
        let reopened = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
        assert!(reopened.board_rows().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_live_session_lets_its_next_event_recreate_it() {
        let registry = test_registry();
        registry
            .apply(&event(EventKind::SessionStart, "s1"), |_| {})
            .unwrap();
        assert!(registry.delete("s1").unwrap());
        let (session, _) = registry
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        assert_eq!(session.id, "s1");
        assert_eq!(registry.board_rows().len(), 1);
    }

    fn titled_registry(pairs: &[(&str, &str)]) -> Registry {
        Registry::from_conn(
            Connection::open_in_memory().unwrap(),
            crate::titles::TitleIndex::from_pairs(pairs),
        )
        .unwrap()
    }

    #[test]
    fn a_claude_session_is_labelled_with_the_name_the_user_gave_it() {
        let registry = titled_registry(&[("s1", "bc9 website - coding session")]);
        let (session, _) = registry
            .apply(&event(EventKind::SessionStart, "s1"), |_| {})
            .unwrap();
        assert_eq!(session.title, "bc9 website - coding session");
    }

    #[test]
    fn a_session_the_desktop_app_does_not_know_keeps_its_project_name() {
        let registry = titled_registry(&[("someone-else", "not this one")]);
        let (session, _) = registry
            .apply(&event(EventKind::SessionStart, "s1"), |_| {})
            .unwrap();
        assert_eq!(session.title, "my-project");
    }

    #[test]
    fn a_codex_session_never_joins_against_a_cli_session_id() {
        // Codex ids are `codex-<cwd hash>` and could never be a cliSessionId,
        // but a collision must not be able to mislabel one.
        let registry = titled_registry(&[("codex-1", "a claude session name")]);
        let mut ev = event(EventKind::SessionStart, "codex-1");
        ev.agent = AgentKind::Codex;
        let (session, _) = registry.apply(&ev, |_| {}).unwrap();
        assert_eq!(session.title, "my-project");
    }

    #[test]
    fn retitle_renames_a_finished_session_and_reports_whether_it_moved() {
        let registry = titled_registry(&[("s1", "the new name")]);
        // Arrives before the first scan published anything for it.
        let empty = Registry::from_conn(
            Connection::open_in_memory().unwrap(),
            crate::titles::TitleIndex::empty(),
        )
        .unwrap();
        let (session, _) = empty
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        assert_eq!(session.title, "my-project");

        registry
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        // Already correct, so nothing moves.
        assert!(!registry.retitle());

        // Now the desktop title goes away (session deleted there): the row
        // must fall back rather than keep a name that no longer exists.
        let registry = titled_registry(&[("s1", "the new name")]);
        registry
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        let stale = Registry::from_conn(
            Connection::open_in_memory().unwrap(),
            crate::titles::TitleIndex::empty(),
        )
        .unwrap();
        stale
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        assert!(!stale.retitle());
    }

    #[test]
    fn a_rename_reaches_a_session_that_already_reported_in() {
        let index = crate::titles::TitleIndex::from_pairs(&[("s1", "before")]);
        let registry =
            Registry::from_conn(Connection::open_in_memory().unwrap(), index.clone()).unwrap();
        registry
            .apply(&event(EventKind::TurnComplete, "s1"), |_| {})
            .unwrap();
        assert_eq!(registry.get("s1").unwrap().title, "before");

        index.replace_for_test(&[("s1", "after")]);
        assert!(registry.retitle());
        assert_eq!(registry.get("s1").unwrap().title, "after");
        // And SQLite agrees, so a restart does not resurrect the old label.
        let stored: String = registry
            .inner
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT title FROM sessions WHERE id = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, "after");
    }

    fn event(kind: EventKind, session_id: &str) -> NormalizedEvent {
        NormalizedEvent {
            agent: AgentKind::ClaudeCode,
            event: kind,
            session_id: session_id.into(),
            cwd: "/tmp/my-project".into(),
            summary: Some("did a thing".into()),
            transcript_path: None,
            tool: None,
            terminal: None,
        }
    }

    fn apply(registry: &Registry, e: &NormalizedEvent) -> Session {
        registry.apply(e, |_| {}).unwrap().0
    }

    #[test]
    fn session_start_creates_working_session() {
        let registry = test_registry();
        let s = apply(&registry, &event(EventKind::SessionStart, "s1"));
        assert_eq!(s.state, SessionState::Working);
        assert_eq!(s.title, "my-project");

        let inner = registry.inner.lock().unwrap();
        let n: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let n: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn state_machine_transitions() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::SessionStart, "s1"));
        let s = apply(&registry, &event(EventKind::TurnComplete, "s1"));
        assert_eq!(s.state, SessionState::Done);
        let s = apply(&registry, &event(EventKind::PermissionRequest, "s1"));
        assert_eq!(s.state, SessionState::NeedsAttention);
        let s = apply(&registry, &event(EventKind::SessionEnd, "s1"));
        assert_eq!(s.state, SessionState::Ended);

        let inner = registry.inner.lock().unwrap();
        let n: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "one session row despite four events");
        let n: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 4);
    }

    /// The ticker is memory-only (§12.1). If this ever writes a row, a busy
    /// turn buries the lifecycle events the whole app is built on.
    #[test]
    fn activity_writes_nothing_and_moves_nothing() {
        let registry = test_registry();
        let started = apply(&registry, &event(EventKind::SessionStart, "s1"));

        for label in ["Bash: cargo", "Edit: registry.rs", "Read: PRD.md"] {
            assert_eq!(registry.record_activity("s1", label), Ticked::Shown);
            let s = registry.get("s1").unwrap();
            assert_eq!(s.last_activity.as_deref(), Some(label));
            assert_eq!(s.state, started.state, "activity never moves the state machine");
            assert_eq!(
                s.last_event_at, started.last_event_at,
                "activity is not an event: ages, retention and waiting spans must not see it"
            );
        }

        let inner = registry.inner.lock().unwrap();
        let n: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "the session_start row and nothing else");
    }

    /// A label belongs to one moment of one turn. The next lifecycle event —
    /// including the `turn_complete` that puts the summary back on the row —
    /// must retire it.
    #[test]
    fn a_lifecycle_event_retires_the_ticker() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::SessionStart, "s1"));
        registry.record_activity("s1", "Bash: cargo");

        let done = apply(&registry, &event(EventKind::TurnComplete, "s1"));
        assert_eq!(done.last_activity, None);
        assert_eq!(registry.get("s1").unwrap().last_activity, None);
    }

    /// A PostToolUse can arrive for a session no lifecycle event vouches for
    /// (hook installed mid-session, or a row recovery dropped). It must not
    /// conjure a board row.
    #[test]
    fn activity_for_an_unknown_session_is_dropped() {
        let registry = test_registry();
        assert_eq!(
            registry.record_activity("ghost", "Bash: cargo"),
            Ticked::Unknown
        );
        assert!(registry.get("ghost").is_none());
        assert!(registry.board_rows().is_empty());
    }

    /// Rust decides whether a ticker is worth drawing, not the webview.
    #[test]
    fn board_rows_only_carry_a_ticker_while_the_session_is_live() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::SessionStart, "s1"));
        assert_eq!(
            registry.record_activity("s1", "Edit: registry.rs"),
            Ticked::Shown
        );
        let row = |registry: &Registry| registry.board_rows().pop().unwrap();
        assert_eq!(row(&registry).activity.as_deref(), Some("Edit: registry.rs"));

        // Waiting on the user is not "doing something", and a stale label
        // under an approval card reads as an agent still running. `Hidden`
        // is also what stops a parked session paying for an emit per window
        // to redraw nothing.
        apply(&registry, &event(EventKind::PermissionRequest, "s1"));
        assert_eq!(
            registry.record_activity("s1", "Edit: registry.rs"),
            Ticked::Hidden
        );
        assert_eq!(row(&registry).activity, None);
        assert!(row(&registry).last_summary.is_some(), "the summary still shows");

        // …and un-parking must not republish the label that was hidden. This
        // is the path that does NOT go through `apply`, so it clears the
        // ticker itself; without that, approving a long `Bash` puts a label
        // from before the park under a live dot for as long as it runs.
        assert!(registry.clear_attention_state("s1"));
        assert_eq!(row(&registry).activity, None);
        assert_eq!(registry.get("s1").unwrap().last_activity, None);
    }

    /// The other non-`apply` route back to Working: an answered approval.
    #[test]
    fn recording_a_decision_retires_the_ticker_too() {
        let registry = test_registry();
        let (_, event_id) = registry
            .apply(&event(EventKind::PermissionRequest, "s1"), |_| {})
            .unwrap();
        assert_eq!(
            registry.record_activity("s1", "Bash: sleep"),
            Ticked::Hidden
        );
        let resumed = registry
            .record_decision("s1", event_id, "allow", true)
            .unwrap()
            .expect("resumed");
        assert_eq!(resumed.state, SessionState::Working);
        assert_eq!(resumed.last_activity, None);
    }

    /// Two sessions parked in the same second are ordered by id, not by
    /// whatever order the map happens to iterate in — otherwise the triage
    /// cycle can revisit one and never reach the other.
    #[test]
    fn oldest_waiting_is_deterministic_when_waits_are_the_same_age() {
        let registry = test_registry();
        for id in ["b-second", "a-first", "c-third"] {
            apply(&registry, &event(EventKind::PermissionRequest, id));
            registry.set_last_event_at_for_test(id, 1_000);
        }
        for _ in 0..10 {
            assert_eq!(
                registry.oldest_waiting(&[]).map(|s| s.id),
                Some("a-first".to_string())
            );
        }
        assert_eq!(
            registry
                .oldest_waiting(&["a-first".to_string()])
                .map(|s| s.id),
            Some("b-second".to_string())
        );
    }

    /// Recovery has no business restoring a label describing an instant we
    /// were not running for.
    #[test]
    fn recovery_leaves_the_ticker_empty() {
        let dir = std::env::temp_dir().join(format!("pmb-activity-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("registry.db");
        {
            let r = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
            r.apply(&event(EventKind::SessionStart, "busy"), |_| {})
                .unwrap();
            r.record_activity("busy", "Bash: cargo");
        }
        let reopened = Registry::open(&db, crate::titles::TitleIndex::empty()).unwrap();
        assert_eq!(reopened.get("busy").unwrap().last_activity, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_event_kind_is_rejected_by_serde() {
        let raw = r#"{"agent":"claude-code","event":"mystery","session_id":"x","cwd":"/tmp"}"#;
        assert!(serde_json::from_str::<NormalizedEvent>(raw).is_err());
    }

    #[test]
    fn ended_sessions_leave_the_live_map_but_stay_in_sqlite() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::SessionStart, "s1"));
        apply(&registry, &event(EventKind::SessionEnd, "s1"));

        assert!(registry.snapshot().is_empty(), "no live sessions after end");
        let inner = registry.inner.lock().unwrap();
        let state: String = inner
            .conn
            .query_row("SELECT state FROM sessions WHERE id='s1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "ended", "history row kept");
    }

    #[test]
    fn resurrected_session_keeps_db_started_at() {
        let registry = test_registry();
        {
            // A session last seen 48 h ago: on disk, outside the recovery
            // window, so not loaded into memory.
            let inner = registry.inner.lock().unwrap();
            let old = now_unix() - 48 * 60 * 60;
            inner
                .conn
                .execute(
                    "INSERT INTO sessions(id, agent, cwd, title, state, terminal_json, started_at, last_event_at)
                     VALUES ('s1', 'claude-code', '/tmp/my-project', 'my-project', 'done', NULL, ?1, ?1)",
                    params![old],
                )
                .unwrap();
        }

        let s = apply(&registry, &event(EventKind::TurnComplete, "s1"));
        let db_started: i64 = {
            let inner = registry.inner.lock().unwrap();
            inner
                .conn
                .query_row("SELECT started_at FROM sessions WHERE id='s1'", [], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        assert_eq!(s.started_at, db_started, "memory and DB agree");
        assert!(
            s.started_at <= now_unix() - 48 * 60 * 60 + 5,
            "original start kept"
        );
    }

    #[test]
    fn board_rows_carry_latest_summary_and_history_is_newest_first() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::SessionStart, "s1"));
        let mut done = event(EventKind::TurnComplete, "s1");
        done.summary = Some("First finish.".into());
        apply(&registry, &done);
        let mut done2 = event(EventKind::TurnComplete, "s1");
        done2.summary = Some("Second finish.".into());
        apply(&registry, &done2);

        let rows = registry.board_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].last_summary.as_deref(), Some("Second finish."));

        let history = registry.history("s1", 50).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].kind, "turn_complete");
        assert_eq!(history[0].summary.as_deref(), Some("Second finish."));
        assert_eq!(history[2].kind, "session_start");

        let capped = registry.history("s1", 2).unwrap();
        assert_eq!(capped.len(), 2);
    }

    fn stored_state(registry: &Registry, id: &str) -> String {
        registry
            .inner
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT state FROM sessions WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// The common case this exists for: the user approved in the terminal
    /// instead, the card timed out, and the board must stop claiming the
    /// session is waiting on them.
    #[test]
    fn clearing_attention_unsticks_a_park_that_ended_without_a_decision() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::PermissionRequest, "s1"));
        assert_eq!(
            registry.get("s1").unwrap().state,
            SessionState::NeedsAttention
        );

        assert!(registry.clear_attention_state("s1"));
        assert_eq!(registry.get("s1").unwrap().state, SessionState::Working);
        assert_eq!(stored_state(&registry, "s1"), "working", "disk agrees");

        // Idempotent, and an unknown session is not an error: the cleanup
        // guards that call this run on every path, decided or not.
        assert!(!registry.clear_attention_state("s1"));
        assert!(!registry.clear_attention_state("never-existed"));
    }

    #[test]
    fn clearing_attention_never_drags_a_session_backwards() {
        let registry = test_registry();
        // A late timeout, arriving after the turn already finished, must not
        // turn a DONE row back into a working one.
        apply(&registry, &event(EventKind::TurnComplete, "s1"));
        assert!(!registry.clear_attention_state("s1"));
        assert_eq!(registry.get("s1").unwrap().state, SessionState::Done);
        assert_eq!(stored_state(&registry, "s1"), "done");
    }

    #[test]
    fn clearing_attention_does_not_move_memory_when_the_row_is_gone_from_disk() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::PermissionRequest, "s1"));
        // What a `delete` racing this call would leave behind.
        registry
            .inner
            .lock()
            .unwrap()
            .conn
            .execute("DELETE FROM sessions WHERE id = 's1'", [])
            .unwrap();

        assert!(!registry.clear_attention_state("s1"));
        assert_eq!(
            registry.get("s1").unwrap().state,
            SessionState::NeedsAttention,
            "nothing was written, so memory must not move either"
        );
    }

    #[test]
    fn a_decision_moves_memory_and_disk_together() {
        let registry = test_registry();
        apply(&registry, &event(EventKind::SessionStart, "s1"));
        let (_, event_id) = registry
            .apply(&event(EventKind::PermissionRequest, "s1"), |_| {})
            .unwrap();

        // Siblings still pending: the decision lands, the park stays.
        assert!(registry
            .record_decision("s1", event_id, "approve", false)
            .unwrap()
            .is_none());
        assert_eq!(
            registry.get("s1").unwrap().state,
            SessionState::NeedsAttention
        );
        assert_eq!(stored_state(&registry, "s1"), "needs_attention");
        assert_eq!(
            registry.history("s1", 1).unwrap()[0].decision.as_deref(),
            Some("approve")
        );

        let resumed = registry
            .record_decision("s1", event_id, "approve", true)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.state, SessionState::Working);
        assert_eq!(
            stored_state(&registry, "s1"),
            "working",
            "memory must never get ahead of what SQLite recorded"
        );
    }

    #[test]
    fn a_decision_on_a_session_that_is_no_longer_live_is_still_recorded() {
        let registry = test_registry();
        let (_, event_id) = registry
            .apply(&event(EventKind::PermissionRequest, "s1"), |_| {})
            .unwrap();
        registry.inner.lock().unwrap().sessions.remove("s1");

        assert!(registry
            .record_decision("s1", event_id, "deny", true)
            .unwrap()
            .is_none());
        assert_eq!(
            registry.history("s1", 1).unwrap()[0].decision.as_deref(),
            Some("deny"),
            "the audit trail does not depend on the session still being live"
        );
    }

    /// Only `session-start` carries terminal data, so a resurrected session
    /// that dropped it would never get it back — and `focus_session` reads
    /// memory, so jump-to-session would silently do nothing.
    #[test]
    fn a_resurrected_session_keeps_the_terminal_sqlite_still_has() {
        let registry = test_registry();
        {
            let inner = registry.inner.lock().unwrap();
            let old = now_unix() - 48 * 60 * 60;
            inner
                .conn
                .execute(
                    "INSERT INTO sessions(id, agent, cwd, title, state, terminal_json, started_at, last_event_at)
                     VALUES ('s1', 'claude-code', '/tmp/my-project', 'my-project', 'done', '{\"app\":\"iTerm2\"}', ?1, ?1)",
                    params![old],
                )
                .unwrap();
        }

        // A plain turn event, carrying no terminal of its own.
        let s = apply(&registry, &event(EventKind::TurnComplete, "s1"));
        assert_eq!(s.terminal_json.as_deref(), Some(r#"{"app":"iTerm2"}"#));
        assert_eq!(
            registry.get("s1").unwrap().terminal_json.as_deref(),
            Some(r#"{"app":"iTerm2"}"#),
            "memory and SQLite must not diverge on the terminal"
        );
    }

    /// `board_rows` runs one of these PER LIVE SESSION under the registry
    /// lock, and walks the table backwards when a session has no non-empty
    /// summary. It must not be a scan of 30 days of events.
    #[test]
    fn events_are_indexed_by_session_even_on_a_database_that_predates_the_index() {
        let conn = Connection::open_in_memory().unwrap();
        // The schema as it shipped before: tables, no secondary index.
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, agent TEXT, cwd TEXT, title TEXT, state TEXT,
                      terminal_json TEXT, started_at INT, last_event_at INT);
             CREATE TABLE events(id INTEGER PRIMARY KEY, session_id TEXT, kind TEXT, summary TEXT,
                      decision TEXT NULL, created_at INT);",
        )
        .unwrap();
        let registry =
            Registry::from_conn(conn, crate::titles::TitleIndex::empty()).unwrap();

        let inner = registry.inner.lock().unwrap();
        let plan: String = inner
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT summary FROM events
                 WHERE session_id = 'x' AND summary IS NOT NULL AND summary != ''
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get("detail"),
            )
            .unwrap();
        assert!(
            plan.contains("idx_events_session"),
            "board_rows must not scan the events table: {plan}"
        );
    }

    #[test]
    fn notify_runs_with_committed_snapshot() {
        let registry = test_registry();
        let mut seen = None;
        registry
            .apply(&event(EventKind::SessionStart, "s1"), |s| {
                seen = Some(s.state);
            })
            .unwrap();
        assert_eq!(seen, Some(SessionState::Working));
    }
}
