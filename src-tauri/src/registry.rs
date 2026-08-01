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
    TurnComplete,
    NeedsAttention,
    PermissionRequest,
    SessionEnd,
}

impl EventKind {
    fn as_str(&self) -> &'static str {
        match self {
            EventKind::SessionStart => "session_start",
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
#[derive(Debug, Clone, Serialize)]
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
}

/// Board row: a live session plus its latest summary.
#[derive(Debug, Clone, Serialize)]
pub struct BoardRow {
    #[serde(flatten)]
    pub session: Session,
    pub last_summary: Option<String>,
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
}

struct Inner {
    conn: Connection,
    sessions: HashMap<String, Session>,
}

const RECOVERY_WINDOW_SECS: i64 = 24 * 60 * 60;

impl Registry {
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        let registry = Self::from_conn(conn)?;
        restrict_db_perms(db_path);
        Ok(registry)
    }

    fn from_conn(conn: Connection) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Bound the write-ahead log. SQLite's default autocheckpoint is 1000
        // pages (~4 MB) and a checkpoint RESETS the WAL rather than shrinking
        // it, so the file sits at its high-water mark forever. On macOS that
        // is not free: memory pressure spills to disk and the two budgets are
        // the same budget. Checkpoint more often and let the file be
        // truncated back down.
        conn.pragma_update(None, "wal_autocheckpoint", 256)?;
        conn.pragma_update(None, "journal_size_limit", 1_048_576)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, agent TEXT, cwd TEXT, title TEXT, state TEXT,
                      terminal_json TEXT, started_at INT, last_event_at INT);
             CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY, session_id TEXT, kind TEXT, summary TEXT,
                    decision TEXT NULL, created_at INT);
             CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value_json TEXT);",
        )?;

        let mut registry = Inner {
            conn,
            sessions: HashMap::new(),
        };
        registry.recover(now_unix())?;
        Ok(Self {
            inner: Mutex::new(registry),
        })
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
        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let inner = &mut *inner;

        let new_state = match event.event {
            EventKind::SessionStart => SessionState::Working,
            EventKind::TurnComplete => SessionState::Done,
            EventKind::NeedsAttention | EventKind::PermissionRequest => {
                SessionState::NeedsAttention
            }
            EventKind::SessionEnd => SessionState::Ended,
        };

        let mut title = project_title(&event.cwd);
        // A cwd of "/" or an empty one makes an unreadable board row; fall
        // back to the agent name.
        if title.is_empty() || title == "/" {
            title = event.agent.as_str().to_string();
        }
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
                // and persistence agree.
                let started_at: Option<i64> = inner
                    .conn
                    .query_row(
                        "SELECT started_at FROM sessions WHERE id = ?1",
                        params![event.session_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                Session {
                    id: event.session_id.clone(),
                    agent: event.agent,
                    cwd: event.cwd.clone(),
                    title: title.clone(),
                    state: new_state,
                    terminal_json: None,
                    started_at: started_at.unwrap_or(now),
                    last_event_at: now,
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
        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let inner = &mut *inner;

        inner.conn.execute(
            "UPDATE events SET decision = ?1 WHERE id = ?2",
            params![decision, event_id],
        )?;
        if !resume {
            return Ok(None);
        }

        let Some(session) = inner.sessions.get_mut(session_id) else {
            return Ok(None);
        };
        session.state = SessionState::Working;
        session.last_event_at = now;
        inner.conn.execute(
            "UPDATE sessions SET state = 'working', last_event_at = ?1 WHERE id = ?2",
            params![now, session_id],
        )?;
        Ok(Some(session.clone()))
    }

    /// Live sessions enriched with their most recent summary, for the board
    /// (FR-7 AC-7.1).
    pub fn board_rows(&self) -> Vec<BoardRow> {
        let inner = self.inner.lock().expect("registry mutex poisoned");
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
                    session: session.clone(),
                    last_summary,
                }
            })
            .collect()
    }

    /// Recent history for one session, newest first (FR-7 AC-7.3: last 50).
    pub fn history(&self, session_id: &str, limit: usize) -> rusqlite::Result<Vec<HistoryEvent>> {
        let inner = self.inner.lock().expect("registry mutex poisoned");
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

    /// Look up one live session by id.
    pub fn get(&self, session_id: &str) -> Option<Session> {
        let inner = self.inner.lock().expect("registry mutex poisoned");
        inner.sessions.get(session_id).cloned()
    }

    /// Force a full checkpoint and truncate the WAL back to zero.
    ///
    /// Called on an idle timer: autocheckpoint alone keeps the WAL bounded
    /// but never returns the space, and this app runs for days at a time.
    /// Best-effort — a busy checkpoint just means readers are active and the
    /// next tick will get it.
    pub fn checkpoint(&self) {
        let inner = self.inner.lock().expect("registry mutex poisoned");
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

    /// All live sessions, for rendering a full board snapshot (step 6).
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<Session> {
        let inner = self.inner.lock().expect("registry mutex poisoned");
        inner.sessions.values().cloned().collect()
    }
}

impl Inner {
    /// Startup recovery (§6): load sessions younger than 24 h and mark them
    /// `unknown` until their next event.
    fn recover(&mut self, now: i64) -> rusqlite::Result<()> {
        let cutoff = now - RECOVERY_WINDOW_SECS;
        let mut stmt = self.conn.prepare(
            "SELECT id, agent, cwd, title, terminal_json, started_at, last_event_at
             FROM sessions WHERE last_event_at >= ?1 AND state != 'ended'",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok(Session {
                id: row.get(0)?,
                agent: AgentKind::from_str(&row.get::<_, String>(1)?),
                cwd: row.get(2)?,
                title: row.get(3)?,
                state: SessionState::Unknown,
                terminal_json: row.get(4)?,
                started_at: row.get(5)?,
                last_event_at: row.get(6)?,
            })
        })?;
        for session in rows {
            let session = session?;
            self.sessions.insert(session.id.clone(), session);
        }
        drop(stmt);
        self.conn.execute(
            "UPDATE sessions SET state = 'unknown' WHERE last_event_at >= ?1 AND state != 'ended'",
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
        let registry = Registry::open(&db).unwrap();

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

    use super::*;

    fn test_registry() -> Registry {
        Registry::from_conn(Connection::open_in_memory().unwrap()).unwrap()
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
