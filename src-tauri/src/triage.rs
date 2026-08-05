//! Triage hotkey (ARCHITECTURE.md §12.2): one chord answers "who needs me
//! next?" and takes you there.
//!
//! With one agent the board is enough. With eight, the only question worth
//! asking is who has waited longest — and the answer has to arrive without
//! finding a window first, or it is not an answer.
//!
//! The decision lives here, in Rust; the hotkey handler is dumb (§project
//! rule). Jumping is not answering, so the same session stays waiting after
//! you have been sent to it — which is why this keeps a short skip list and
//! why pressing again moves you on instead of bouncing you back.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::registry::{Registry, Session};

/// How long a round survives without a press before it is abandoned and the
/// next press starts again at the longest-waiting session.
///
/// Deliberately measured from the LAST PRESS and applied to the round as a
/// whole, not per visited session. A per-session TTL sounds equivalent and is
/// not: the first entry expires while you are still walking the list, and
/// because it is also the oldest, it wins the next lookup outright. At eight
/// parked agents and a realistic three seconds a press — glance at the
/// terminal, press again — that starves everything past the fourth session
/// permanently, which is exactly the scale this feature exists for.
const ROUND_IDLE: Duration = Duration::from_secs(10);

/// Presses closer together than this are one press.
///
/// macOS delivers a Carbon hot-key event once per physical press, but Windows'
/// `RegisterHotKey` repeats `WM_HOTKEY` for as long as the chord is held, and
/// the plugin reports every repeat as a fresh press. Without this, leaning on
/// the key would walk the entire waiting list in a second and shell out to
/// tmux and `ps` for every step of it.
const REPEAT_GUARD: Duration = Duration::from_millis(250);

/// The default chord. `hotkey.next` in `~/.pingmybell/config.json` overrides
/// it; anything Tauri's parser accepts works (`Ctrl+Alt+Space`,
/// `CmdOrCtrl+Shift+J`, …).
pub const DEFAULT_CHORD: &str = "Ctrl+Alt+Space";

/// What the hotkey should do, decided entirely before anything is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Go here.
    Jump(Box<Session>),
    /// Nobody is waiting — say so quietly and speak nothing.
    AllClear,
    /// A key repeat, not a decision. Nothing happens and no state moves.
    Ignored,
}

/// One walk through the sessions that are waiting on the user.
#[derive(Default)]
struct Round {
    visited: HashSet<String>,
    /// None until the first press ever, so startup cannot swallow it.
    last_press: Option<Instant>,
}

#[derive(Default)]
pub struct Triage {
    round: Mutex<Round>,
}

impl Triage {
    /// Pick the next session to visit, remembering it so the following press
    /// moves on.
    pub fn next(&self, registry: &Registry) -> Next {
        self.next_at(registry, Instant::now())
    }

    /// Split out so tests can move the clock instead of sleeping through the
    /// timings.
    fn next_at(&self, registry: &Registry, now: Instant) -> Next {
        let mut round = self
            .round
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(previous) = round.last_press {
            let since = now.duration_since(previous);
            if since < REPEAT_GUARD {
                // Deliberately does NOT restamp `last_press`: a held key then
                // yields at most one jump per guard window rather than being
                // suppressed forever, and a genuine second press still lands.
                return Next::Ignored;
            }
            if since >= ROUND_IDLE {
                round.visited.clear();
            }
        }
        round.last_press = Some(now);

        let skip: Vec<String> = round.visited.iter().cloned().collect();
        let target = registry.oldest_waiting(&skip).or_else(|| {
            // Everyone in this round has been visited. Wrap around to the
            // longest-waiting one instead of reporting all clear: jumping is
            // not answering, so those agents are all still blocked, and
            // saying otherwise would be the status lie this app exists to
            // prevent. With two parked sessions the old behaviour made every
            // third press claim the board was empty.
            round.visited.clear();
            registry.oldest_waiting(&[])
        });

        match target {
            Some(session) => {
                round.visited.insert(session.id.clone());
                Next::Jump(Box::new(session))
            }
            // Nothing waiting at all — the only way to reach this.
            None => Next::AllClear,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AgentKind, EventKind, NormalizedEvent, SessionState};

    fn registry() -> Registry {
        Registry::open_in_memory().expect("registry must open")
    }

    fn park(registry: &Registry, id: &str, at: i64) {
        let event = NormalizedEvent {
            agent: AgentKind::ClaudeCode,
            event: EventKind::PermissionRequest,
            session_id: id.into(),
            cwd: "/tmp/project".into(),
            summary: None,
            transcript_path: None,
            tool: None,
            terminal: None,
        };
        registry.apply(&event, |_| {}).unwrap();
        registry.set_last_event_at_for_test(id, at);
    }

    fn jumped(next: &Next) -> &str {
        match next {
            Next::Jump(session) => &session.id,
            other => panic!("expected a jump, got {other:?}"),
        }
    }

    /// The gate in §12.2: two parked sessions, oldest first, then the other.
    #[test]
    fn presses_walk_the_waiting_sessions_oldest_first() {
        let registry = registry();
        park(&registry, "newer", 2_000);
        park(&registry, "older", 1_000);
        let triage = Triage::default();
        let t0 = Instant::now();

        assert_eq!(jumped(&triage.next_at(&registry, t0)), "older");
        assert_eq!(
            jumped(&triage.next_at(&registry, t0 + Duration::from_secs(1))),
            "newer",
            "jumping does not answer anything, so a second press must move on"
        );
    }

    /// …and the rest of §12.2's gate: "all clear" is what you get once nothing
    /// is waiting, and ONLY then. A press that has run out of unvisited
    /// sessions wraps around, because those agents are all still blocked.
    #[test]
    fn all_clear_means_nothing_is_waiting_not_merely_nothing_new() {
        let registry = registry();
        park(&registry, "a", 1_000);
        park(&registry, "b", 2_000);
        let triage = Triage::default();
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);

        assert_eq!(jumped(&triage.next_at(&registry, at(0))), "a");
        assert_eq!(jumped(&triage.next_at(&registry, at(1))), "b");
        assert_eq!(
            jumped(&triage.next_at(&registry, at(2))),
            "a",
            "both are still parked, so the third press wraps — it does not lie"
        );

        // Answer them, and the same press finally means it.
        for id in ["a", "b"] {
            assert!(registry.clear_attention_state(id));
        }
        assert_eq!(triage.next_at(&registry, at(3)), Next::AllClear);
    }

    /// The bug that made the feature useless at the scale it was written for:
    /// a per-session TTL expires the HEAD of the list while you are still
    /// walking it, and since the head is also the oldest it wins every
    /// subsequent lookup. Everything past the fourth session became
    /// permanently unreachable at a realistic three seconds a press.
    #[test]
    fn every_waiting_session_is_reachable_however_long_the_walk_takes() {
        let registry = registry();
        for i in 0..8 {
            park(&registry, &format!("s{i}"), 1_000 + i as i64);
        }
        let triage = Triage::default();
        let t0 = Instant::now();

        let mut visited = Vec::new();
        for press in 0..8 {
            // Three seconds a press: eight presses span 21 s, well past the
            // 10 s idle window that used to be applied per session.
            let next = triage.next_at(&registry, t0 + Duration::from_secs(press * 3));
            visited.push(jumped(&next).to_string());
        }
        visited.sort();
        let expected: Vec<String> = (0..8).map(|i| format!("s{i}")).collect();
        assert_eq!(visited, expected, "every parked session must be reachable");
    }

    /// A round is abandoned once you stop pressing, so coming back later
    /// starts again at whoever has waited longest.
    #[test]
    fn an_abandoned_round_starts_over() {
        let registry = registry();
        park(&registry, "older", 1_000);
        park(&registry, "newer", 2_000);
        let triage = Triage::default();
        let t0 = Instant::now();

        assert_eq!(jumped(&triage.next_at(&registry, t0)), "older");
        assert_eq!(
            jumped(&triage.next_at(&registry, t0 + ROUND_IDLE)),
            "older",
            "a new round begins at the longest wait, not where the last left off"
        );
    }

    /// Windows repeats `WM_HOTKEY` while the chord is held. Leaning on the key
    /// must not walk the whole list — and must not wedge it either.
    #[test]
    fn a_held_chord_is_one_press() {
        let registry = registry();
        park(&registry, "a", 1_000);
        park(&registry, "b", 2_000);
        let triage = Triage::default();
        let t0 = Instant::now();

        assert_eq!(jumped(&triage.next_at(&registry, t0)), "a");
        for ms in [30, 60, 90, 200] {
            assert_eq!(
                triage.next_at(&registry, t0 + Duration::from_millis(ms)),
                Next::Ignored,
                "repeat at {ms}ms"
            );
        }
        // The guard is a rate limit, not a latch: a real press still lands.
        assert_eq!(
            jumped(&triage.next_at(&registry, t0 + Duration::from_millis(300))),
            "b"
        );
    }

    /// Only sessions that are actually waiting on the user are triage
    /// targets: a working agent needs nothing, and a finished one is where
    /// the board and the toast already point.
    #[test]
    fn only_waiting_sessions_are_targets() {
        let registry = registry();
        park(&registry, "waiting", 1_000);
        for (id, kind) in [
            ("working", EventKind::TurnStart),
            ("done", EventKind::TurnComplete),
        ] {
            let event = NormalizedEvent {
                agent: AgentKind::ClaudeCode,
                event: kind,
                session_id: id.into(),
                cwd: "/tmp/project".into(),
                summary: None,
                transcript_path: None,
                tool: None,
                terminal: None,
            };
            registry.apply(&event, |_| {}).unwrap();
            registry.set_last_event_at_for_test(id, 10);
        }
        let triage = Triage::default();

        let next = triage.next(&registry);
        assert_eq!(jumped(&next), "waiting");
        match next {
            Next::Jump(session) => assert_eq!(session.state, SessionState::NeedsAttention),
            other => panic!("expected a jump, got {other:?}"),
        }
    }
}
