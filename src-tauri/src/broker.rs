//! Approval broker (§6): pending PreToolUse requests parked on oneshot
//! channels. The ingest handler registers a request and awaits the receiver
//! with a timeout; the UI's `decide` command completes it. Whoever takes the
//! entry first (decision vs. timeout) wins — the loser sees None.
//!
//! Two kinds of request park here, with identical race semantics:
//!
//! * approvals — `Bash`/`Write`/… PreToolUse gating (allow/deny/ask)
//! * questions — `AskUserQuestion` PreToolUse calls, answered from the
//!   overlay instead of the TUI selector
//!
//! (§6 sketches a DashMap; a Mutex<HashMap> is equivalent at this load and
//! avoids a dependency.)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};

use crate::registry::AgentKind;

/// A park deadline the UI can push FORWARD while the user is demonstrably
/// working on the answer (the reply window is open, keys are being pressed).
///
/// Two numbers, and the difference between them is the whole point:
///
/// * the base deadline is what an UNTOUCHED request gets — a question nobody
///   is answering must still fall through promptly so the agent can render
///   its own selector;
/// * the ceiling is a hard cap measured from the same start, sized so the
///   shim always answers (or gives up) INSIDE the agent's hook timeout. Past
///   it the agent would kill the hook itself, and while that is still
///   fail-open (no stdout, exit 0 — PRD AC-2.4) it costs the user their turn.
///
/// `extend` never moves the deadline backwards and never past the ceiling, so
/// a buggy or hostile caller can only ever be ignored.
#[derive(Debug)]
pub struct Deadline {
    at: Mutex<Instant>,
    ceiling: Instant,
}

impl Deadline {
    /// `base` is how long an untouched request parks; `ceiling` is the hard
    /// cap from the same instant. A ceiling below the base is raised to it —
    /// the base park is a floor, never something an extension policy shortens.
    pub fn new(base: Duration, ceiling: Duration) -> Self {
        let now = Instant::now();
        let ceiling = now + ceiling.max(base);
        Self {
            at: Mutex::new((now + base).min(ceiling)),
            ceiling,
        }
    }

    /// A deadline that cannot be extended: an approval is a two-second
    /// decision, not something the user types.
    pub fn fixed(base: Duration) -> Self {
        Self::new(base, base)
    }

    pub fn at(&self) -> Instant {
        *self.at.lock().expect("deadline mutex poisoned")
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ceiling(&self) -> Instant {
        self.ceiling
    }

    /// Push the deadline to `now + by`, clamped to the ceiling. Returns the
    /// effective deadline.
    pub fn extend(&self, by: Duration) -> Instant {
        let want = (Instant::now() + by).min(self.ceiling);
        let mut at = self.at.lock().expect("deadline mutex poisoned");
        if want > *at {
            *at = want;
        }
        *at
    }

    /// True once the deadline has been pushed as far as it can go. Callers
    /// learn this indirectly, from the shrinking `remaining` that
    /// `Broker::extend_question` hands back — the UI's job is to warn the
    /// user, not to know about ceilings.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn at_ceiling(&self) -> bool {
        self.at() >= self.ceiling
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Decision::Allow),
            "deny" => Some(Decision::Deny),
            "ask" => Some(Decision::Ask),
            _ => None,
        }
    }
}

/// Everything the overlay card and the voicing need about one request.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub session_id: String,
    /// The permission_request row this approval belongs to, so the decision
    /// lands on the right history entry even with concurrent approvals.
    #[serde(skip)]
    pub event_id: i64,
    pub agent: AgentKind,
    /// Project title (cwd basename).
    pub title: String,
    pub tool_name: String,
    /// Primary tool input, already truncated for display (e.g. the bash
    /// command line) — derived data, never logged (§9).
    pub tool_summary: String,
}

/// One question from an `AskUserQuestion` tool call, mirrored verbatim from
/// the hook payload (verified shape, claude 2.1.198) so the overlay can offer
/// exactly the choices the TUI selector would have.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionSpec {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Claude's own camelCase key, kept as-is on the wire in both directions.
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// Everything the overlay card needs to render one parked AskUserQuestion.
#[derive(Debug, Clone, Serialize)]
pub struct QuestionInfo {
    pub id: String,
    pub session_id: String,
    /// The event row this question belongs to, so an answer lands on the
    /// right history entry even with concurrent questions. Read by the
    /// `answer_question` command (overlay workstream).
    #[serde(skip)]
    #[allow(dead_code)]
    pub event_id: i64,
    pub agent: AgentKind,
    /// Project title (cwd basename).
    pub title: String,
    /// Claude's `tool_use_id` for the AskUserQuestion call — carried through
    /// for correlation/debugging; never required to answer.
    pub tool_use_id: Option<String>,
    pub questions: Vec<QuestionSpec>,
}

/// The user's answer to one question of the call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// Index into `QuestionInfo::questions`.
    pub question_index: usize,
    /// Chosen option labels (one for single-select, several for multiSelect).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Free text the user typed instead of / alongside picking (the TUI's
    /// "Type something" affordance).
    #[serde(default)]
    pub free_text: Option<String>,
}

/// A complete answer to an AskUserQuestion call: zero or more answered
/// questions, in question order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub answers: Vec<Answer>,
}

/// Result of `Broker::answer`, for the `answer_question` command.
#[derive(Debug)]
#[allow(dead_code)] // TODO(overlay): matched by the `answer_question` command
pub enum AnswerResult {
    /// Answer accepted; the parked HTTP handler is completing. Record the
    /// decision on `info.event_id`, voice it, and unpin the card.
    Accepted(QuestionInfo),
    /// Nothing usable in the answer — the question is STILL PARKED. Keep the
    /// card pinned and let the user try again.
    Rejected,
    /// No longer pending (answered already, deferred, timed out, or the shim
    /// died). Unpin the card; there is nobody left to answer.
    Gone,
}

/// What a parked request's handler found when its timer fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// The timer won: the entry is gone and the park ends in the terminal
    /// fallback (204 → the shim prints nothing → the agent asks its own way).
    Expired,
    /// The UI took the entry first and its send is already in flight — wait a
    /// grace period rather than throw away a click the user already made.
    Raced,
    /// Not due after all: more time was bought while the timer was firing.
    /// Re-arm on the new deadline.
    Extended,
}

/// How a parked question ended. `Deferred` is the overlay's "I'll answer in
/// the terminal" escape hatch: the handler answers 204 immediately and Claude
/// Code renders its own selector (same fallback as a timeout).
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // constructed by Broker::answer / defer_question
pub enum QuestionOutcome {
    Answered(QuestionAnswer),
    Deferred,
}

/// Caps on answer text accepted from the UI (defensive: the reason string
/// ends up in the agent's context).
#[allow(dead_code)]
const MAX_LABEL_CHARS: usize = 500;
#[allow(dead_code)]
const MAX_FREE_TEXT_CHARS: usize = 2000;

impl QuestionInfo {
    /// Normalize an incoming answer against this call's questions: drop
    /// out-of-range indices, duplicates, and empty entries; trim and cap
    /// text; collapse a multi-label answer to one label when the question is
    /// not `multiSelect`. `None` when nothing usable is left — the caller
    /// must then leave the park untouched (a garbage answer must never
    /// strand a waiting agent).
    #[allow(dead_code)] // reached via Broker::answer, wired with the UI
    pub fn normalize_answer(&self, answer: QuestionAnswer) -> Option<QuestionAnswer> {
        let mut out: Vec<Answer> = Vec::new();
        for a in answer.answers {
            let Some(spec) = self.questions.get(a.question_index) else {
                continue;
            };
            if out.iter().any(|e| e.question_index == a.question_index) {
                continue; // first answer per question wins
            }
            let mut labels: Vec<String> = a
                .labels
                .into_iter()
                .filter_map(|l| trim_cap(&l, MAX_LABEL_CHARS))
                .collect();
            if !spec.multi_select {
                labels.truncate(1);
            }
            let free_text = a.free_text.and_then(|t| trim_cap(&t, MAX_FREE_TEXT_CHARS));
            if labels.is_empty() && free_text.is_none() {
                continue;
            }
            out.push(Answer {
                question_index: a.question_index,
                labels,
                free_text,
            });
        }
        out.sort_by_key(|a| a.question_index);
        (!out.is_empty()).then_some(QuestionAnswer { answers: out })
    }
}

#[allow(dead_code)]
fn trim_cap(s: &str, max: usize) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(max).collect())
}

struct Pending {
    tx: oneshot::Sender<Decision>,
    info: ApprovalInfo,
}

struct PendingQuestion {
    #[allow(dead_code)] // sent on by Broker::answer / defer_question
    tx: oneshot::Sender<QuestionOutcome>,
    info: QuestionInfo,
    /// Shared with the parked HTTP handler, which sleeps to it and re-reads
    /// it every time it fires.
    deadline: Arc<Deadline>,
}

#[derive(Default)]
pub struct Broker {
    pending: Mutex<HashMap<String, Pending>>,
    questions: Mutex<HashMap<String, PendingQuestion>>,
}

impl Broker {
    /// Park a new approval; returns its id and the receiver the HTTP handler
    /// awaits.
    pub fn register(&self, mut info: ApprovalInfo) -> (ApprovalInfo, oneshot::Receiver<Decision>) {
        let id = new_id();
        info.id = id.clone();

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("broker mutex poisoned").insert(
            id,
            Pending {
                tx,
                info: info.clone(),
            },
        );
        (info, rx)
    }

    /// Complete a pending approval. Returns its info if it was still pending
    /// (i.e. this call won the race against the timeout).
    pub fn decide(&self, id: &str, decision: Decision) -> Option<ApprovalInfo> {
        let entry = self
            .pending
            .lock()
            .expect("broker mutex poisoned")
            .remove(id)?;
        // If the receiver was dropped (handler already timed out and gave up
        // between our remove and this send), treat as not-pending.
        match entry.tx.send(decision) {
            Ok(()) => Some(entry.info),
            Err(_) => None,
        }
    }

    /// Drop a pending approval after the handler's timeout. Returns info if
    /// it was still pending.
    pub fn expire(&self, id: &str) -> Option<ApprovalInfo> {
        self.pending
            .lock()
            .expect("broker mutex poisoned")
            .remove(id)
            .map(|p| p.info)
    }

    /// Park a new question; returns its id and the receiver the HTTP handler
    /// awaits. Same race semantics as `register`.
    ///
    /// `deadline` is shared with that handler: the caller parks on it, and
    /// `extend_question` pushes it forward while the user is answering.
    pub fn register_question(
        &self,
        mut info: QuestionInfo,
        deadline: Arc<Deadline>,
    ) -> (QuestionInfo, oneshot::Receiver<QuestionOutcome>) {
        let id = new_id();
        info.id = id.clone();

        let (tx, rx) = oneshot::channel();
        self.questions
            .lock()
            .expect("broker mutex poisoned")
            .insert(
                id,
                PendingQuestion {
                    tx,
                    info: info.clone(),
                    deadline,
                },
            );
        (info, rx)
    }

    /// Push a parked question's deadline out by `by`, because the user is
    /// demonstrably still working on the answer (they opened the typed-reply
    /// window, they are pressing keys in it).
    ///
    /// `None` means the question is no longer parked — the caller must treat
    /// that as "this is over", never as a failed retry. `Some(remaining)` is
    /// how much time the extension actually bought, which is zero once the
    /// ceiling is reached: the UI needs to know it can no longer promise the
    /// user more time rather than silently letting the agent time out.
    pub fn extend_question(&self, id: &str, by: Duration) -> Option<Duration> {
        let map = self.questions.lock().expect("broker mutex poisoned");
        let entry = map.get(id)?;
        let at = entry.deadline.extend(by);
        Some(at.saturating_duration_since(Instant::now()))
    }

    /// Expire a parked question, but ONLY if its deadline really has passed.
    ///
    /// `armed_for` is the deadline the handler slept to. The comparison
    /// happens under the same lock `extend_question` takes, which is what
    /// makes an extension landing in the microseconds around the timer firing
    /// impossible to lose. Without it there is a hairline window where a user
    /// mid-keystroke still loses the question — the original bug, just
    /// narrower, and "narrower" is not the same as fixed.
    pub fn expire_question_if_due(&self, id: &str, armed_for: Instant) -> Expiry {
        let mut map = self.questions.lock().expect("broker mutex poisoned");
        let Some(entry) = map.get(id) else {
            return Expiry::Raced;
        };
        if entry.deadline.at() > armed_for {
            return Expiry::Extended;
        }
        map.remove(id);
        Expiry::Expired
    }


    /// Answer a parked question. The answer is normalized against the parked
    /// questions FIRST, so an unusable answer is `Rejected` and the park is
    /// left untouched — a UI bug must never strand a waiting agent.
    ///
    /// The three outcomes are deliberately distinct: `Rejected` means KEEP the
    /// card pinned (the question is still live), while `Gone` means unpin it.
    /// Collapsing them into one "nothing happened" answer would silently drop
    /// the user's only chance to reply.
    #[allow(dead_code)] // TODO(overlay): called by the `answer_question` command
    pub fn answer(&self, id: &str, answer: QuestionAnswer) -> AnswerResult {
        let mut map = self.questions.lock().expect("broker mutex poisoned");
        let Some(entry) = map.get(id) else {
            return AnswerResult::Gone;
        };
        let Some(normalized) = entry.info.normalize_answer(answer) else {
            return AnswerResult::Rejected;
        };
        let entry = match map.remove(id) {
            Some(entry) => entry,
            None => return AnswerResult::Gone,
        };
        drop(map);
        match entry.tx.send(QuestionOutcome::Answered(normalized)) {
            // The handler already gave up between our lookup and this send.
            Err(_) => AnswerResult::Gone,
            Ok(()) => AnswerResult::Accepted(entry.info),
        }
    }

    /// Hand a parked question back to the terminal (overlay dismiss): the
    /// handler answers 204 at once and Claude Code renders its own selector.
    #[allow(dead_code)] // TODO(overlay): called by the `defer_question` command
    pub fn defer_question(&self, id: &str) -> Option<QuestionInfo> {
        let entry = self
            .questions
            .lock()
            .expect("broker mutex poisoned")
            .remove(id)?;
        match entry.tx.send(QuestionOutcome::Deferred) {
            Ok(()) => Some(entry.info),
            Err(_) => None,
        }
    }

    /// Drop a parked question after the handler's timeout (or when the shim's
    /// connection died). Returns info if it was still pending.
    pub fn expire_question(&self, id: &str) -> Option<QuestionInfo> {
        self.questions
            .lock()
            .expect("broker mutex poisoned")
            .remove(id)
            .map(|p| p.info)
    }

    /// Whether the session still has approvals OR questions parked (used to
    /// decide if a resolved request should flip the session back to Working).
    pub fn has_pending_for_session(&self, session_id: &str) -> bool {
        let approvals = self
            .pending
            .lock()
            .expect("broker mutex poisoned")
            .values()
            .any(|p| p.info.session_id == session_id);
        approvals
            || self
                .questions
                .lock()
                .expect("broker mutex poisoned")
                .values()
                .any(|p| p.info.session_id == session_id)
    }
}

fn new_id() -> String {
    let mut raw = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ApprovalInfo {
        ApprovalInfo {
            id: String::new(),
            session_id: "s1".into(),
            event_id: 1,
            agent: AgentKind::ClaudeCode,
            title: "api-server".into(),
            tool_name: "Bash".into(),
            tool_summary: "cargo test".into(),
        }
    }

    #[tokio::test]
    async fn decide_resolves_parked_receiver() {
        let broker = Broker::default();
        let (registered, rx) = broker.register(info());
        assert_eq!(registered.id.len(), 32);

        let resolved = broker.decide(&registered.id, Decision::Allow);
        assert!(resolved.is_some());
        assert_eq!(rx.await.unwrap(), Decision::Allow);
    }

    #[tokio::test]
    async fn expire_wins_over_late_decision() {
        let broker = Broker::default();
        let (registered, rx) = broker.register(info());

        assert!(broker.expire(&registered.id).is_some());
        assert!(
            broker.decide(&registered.id, Decision::Allow).is_none(),
            "late decision must not report success"
        );
        assert!(rx.await.is_err(), "receiver sees the channel closed");
    }

    #[tokio::test]
    async fn unknown_id_is_none() {
        let broker = Broker::default();
        assert!(broker.decide("nope", Decision::Deny).is_none());
        assert!(broker.expire("nope").is_none());
        assert!(matches!(
            broker.answer("nope", QuestionAnswer::default()),
            AnswerResult::Gone
        ));
        assert!(broker.defer_question("nope").is_none());
        assert!(broker.expire_question("nope").is_none());
    }

    fn spec(header: &str, multi: bool, labels: &[&str]) -> QuestionSpec {
        QuestionSpec {
            question: format!("Which {header}?"),
            header: header.into(),
            options: labels
                .iter()
                .map(|l| QuestionOption {
                    label: (*l).into(),
                    description: String::new(),
                })
                .collect(),
            multi_select: multi,
        }
    }

    /// Long enough that no test races it; extension behaviour is tested
    /// directly on `Deadline` and via `extend_question`.
    fn test_deadline() -> Arc<Deadline> {
        Arc::new(Deadline::new(
            Duration::from_secs(60),
            Duration::from_secs(600),
        ))
    }

    fn question(specs: Vec<QuestionSpec>) -> QuestionInfo {
        QuestionInfo {
            id: String::new(),
            session_id: "s1".into(),
            event_id: 7,
            agent: AgentKind::ClaudeCode,
            title: "api-server".into(),
            tool_use_id: Some("toolu_01".into()),
            questions: specs,
        }
    }

    fn picked(index: usize, labels: &[&str]) -> QuestionAnswer {
        QuestionAnswer {
            answers: vec![Answer {
                question_index: index,
                labels: labels.iter().map(|s| (*s).to_string()).collect(),
                free_text: None,
            }],
        }
    }

    #[tokio::test]
    async fn answer_resolves_parked_question() {
        let broker = Broker::default();
        let (registered, rx) = broker.register_question(
            question(vec![spec(
                "Approach",
                false,
                &["Option A (fast)", "Option B (thorough)"],
            )]),
            test_deadline(),
        );
        assert_eq!(registered.id.len(), 32);
        assert!(broker.has_pending_for_session("s1"));

        let AnswerResult::Accepted(resolved) =
            broker.answer(&registered.id, picked(0, &["Option B (thorough)"]))
        else {
            panic!("a valid answer must be accepted");
        };
        assert_eq!(resolved.event_id, 7);
        assert_eq!(
            rx.await.unwrap(),
            QuestionOutcome::Answered(picked(0, &["Option B (thorough)"]))
        );
        assert!(!broker.has_pending_for_session("s1"));
    }

    #[tokio::test]
    async fn defer_and_expire_race_like_approvals() {
        let broker = Broker::default();
        let (a, rx_a) = broker.register_question(question(vec![spec("Approach", false, &["A"])]), test_deadline());
        assert!(broker.defer_question(&a.id).is_some());
        assert_eq!(rx_a.await.unwrap(), QuestionOutcome::Deferred);
        assert!(
            matches!(broker.answer(&a.id, picked(0, &["A"])), AnswerResult::Gone),
            "answering a deferred question must report Gone, not success"
        );

        let (b, rx_b) = broker.register_question(question(vec![spec("Approach", false, &["A"])]), test_deadline());
        assert!(broker.expire_question(&b.id).is_some());
        assert!(matches!(
            broker.answer(&b.id, picked(0, &["A"])),
            AnswerResult::Gone
        ));
        assert!(rx_b.await.is_err(), "receiver sees the channel closed");
    }

    #[tokio::test]
    async fn garbage_answer_leaves_the_park_alive() {
        let broker = Broker::default();
        let (q, rx) = broker.register_question(question(vec![spec("Approach", false, &["A"])]), test_deadline());

        // Out-of-range index, empty labels, whitespace free text: nothing
        // usable → Rejected, and the question stays parked and answerable.
        for junk in [
            QuestionAnswer::default(),
            picked(9, &["A"]),
            picked(0, &["   "]),
        ] {
            assert!(matches!(broker.answer(&q.id, junk), AnswerResult::Rejected));
            assert!(
                broker.has_pending_for_session("s1"),
                "a rejected answer must leave the park alive"
            );
        }
        assert!(
            matches!(
                broker.answer(&q.id, picked(0, &["A"])),
                AnswerResult::Accepted(_)
            ),
            "a real answer still lands after garbage was rejected"
        );
        assert!(matches!(rx.await.unwrap(), QuestionOutcome::Answered(_)));
    }

    #[test]
    fn normalize_enforces_single_select_dedup_and_order() {
        let info = question(vec![
            spec("Approach", false, &["A", "B"]),
            spec("Scope", true, &["X", "Y"]),
        ]);

        // Single-select keeps only the first label; multiSelect keeps all;
        // answers come back in question order; duplicates are dropped.
        let raw = QuestionAnswer {
            answers: vec![
                Answer {
                    question_index: 1,
                    labels: vec!["X".into(), "Y".into()],
                    free_text: Some("  ".into()),
                },
                Answer {
                    question_index: 0,
                    labels: vec!["A".into(), "B".into()],
                    free_text: None,
                },
                Answer {
                    question_index: 0,
                    labels: vec!["B".into()],
                    free_text: None,
                },
            ],
        };
        let out = info.normalize_answer(raw).unwrap();
        assert_eq!(out.answers.len(), 2);
        assert_eq!(out.answers[0].question_index, 0);
        assert_eq!(out.answers[0].labels, vec!["A".to_string()]);
        assert_eq!(out.answers[1].question_index, 1);
        assert_eq!(
            out.answers[1].labels,
            vec!["X".to_string(), "Y".to_string()]
        );
        assert!(out.answers[1].free_text.is_none());

        // Free text alone is a valid answer; long text is capped.
        let long = "x".repeat(MAX_FREE_TEXT_CHARS + 100);
        let out = info
            .normalize_answer(QuestionAnswer {
                answers: vec![Answer {
                    question_index: 0,
                    labels: vec![],
                    free_text: Some(long),
                }],
            })
            .unwrap();
        assert_eq!(
            out.answers[0].free_text.as_ref().unwrap().chars().count(),
            MAX_FREE_TEXT_CHARS
        );
    }

    #[test]
    fn question_spec_round_trips_claude_wire_shape() {
        let raw = r#"{"question":"A or B?","header":"Approach",
            "options":[{"label":"Option A","description":"fast"},{"label":"Option B"}],
            "multiSelect":true}"#;
        let spec: QuestionSpec = serde_json::from_str(raw).unwrap();
        assert!(spec.multi_select);
        assert_eq!(spec.options[1].label, "Option B");
        assert_eq!(spec.options[1].description, "");
        let back = serde_json::to_value(&spec).unwrap();
        assert_eq!(back["multiSelect"], true, "camelCase key preserved");
    }

    #[test]
    fn deadline_extends_forward_but_never_past_the_ceiling() {
        let d = Deadline::new(Duration::from_secs(110), Duration::from_secs(540));
        let base = d.at();
        assert!(!d.at_ceiling());

        // A shorter extension than what is already granted must not SHORTEN
        // the park: the base wait is a floor.
        assert_eq!(d.extend(Duration::from_secs(5)), base);

        let extended = d.extend(Duration::from_secs(300));
        assert!(extended > base, "a real extension moves the deadline out");
        assert!(extended < d.ceiling());

        // The ceiling is hard: no amount of typing buys past it, and once
        // there the deadline stops moving entirely.
        let capped = d.extend(Duration::from_secs(86_400));
        assert_eq!(capped, d.ceiling());
        assert!(d.at_ceiling());
        assert_eq!(d.extend(Duration::from_secs(86_400)), d.ceiling());
    }

    #[test]
    fn fixed_deadline_ignores_extensions() {
        // Approvals must not be extendable: a stalled approval card would
        // hold up a tool call the user never looked at.
        let d = Deadline::fixed(Duration::from_secs(110));
        let base = d.at();
        assert_eq!(d.extend(Duration::from_secs(600)), base);
        assert!(d.at_ceiling());
    }

    #[test]
    fn ceiling_below_base_is_raised_to_it() {
        let d = Deadline::new(Duration::from_secs(110), Duration::from_secs(1));
        assert_eq!(d.at(), d.ceiling());
        assert!(d.at().saturating_duration_since(Instant::now()) > Duration::from_secs(100));
    }

    #[tokio::test]
    async fn extend_question_only_works_while_parked() {
        let broker = Broker::default();
        let deadline = Arc::new(Deadline::new(
            Duration::from_secs(110),
            Duration::from_secs(540),
        ));
        let (q, _rx) = broker.register_question(
            question(vec![spec("Approach", false, &["A"])]),
            deadline.clone(),
        );
        let before = deadline.at();

        let remaining = broker
            .extend_question(&q.id, Duration::from_secs(300))
            .expect("a parked question can be extended");
        assert!(remaining > Duration::from_secs(200));
        assert!(deadline.at() > before);

        // At the ceiling the extension still succeeds but buys nothing more,
        // and `remaining` is what tells the UI to stop promising time.
        let capped = broker
            .extend_question(&q.id, Duration::from_secs(86_400))
            .unwrap();
        assert!(capped <= Duration::from_secs(540));
        assert!(deadline.at_ceiling());

        // Gone means gone: a heartbeat must never resurrect a dead park.
        assert!(broker.expire_question(&q.id).is_some());
        assert!(broker
            .extend_question(&q.id, Duration::from_secs(300))
            .is_none());
        assert!(broker
            .extend_question("never-existed", Duration::from_secs(300))
            .is_none());
    }

    #[tokio::test]
    async fn expire_if_due_cannot_lose_an_extension_that_lands_on_the_timer() {
        let broker = Broker::default();
        let deadline = Arc::new(Deadline::new(
            Duration::from_millis(1),
            Duration::from_secs(540),
        ));
        let (q, _rx) = broker.register_question(
            question(vec![spec("Approach", false, &["A"])]),
            deadline.clone(),
        );
        let armed_for = deadline.at();

        // The heartbeat lands in the instant the handler's timer fires. The
        // deadline check happens under the same lock the extension took, so
        // the park must survive rather than be expired out from under it.
        broker.extend_question(&q.id, Duration::from_secs(120));
        assert_eq!(
            broker.expire_question_if_due(&q.id, armed_for),
            Expiry::Extended
        );
        assert!(broker.has_pending_for_session("s1"));

        // Re-armed on the new deadline, nothing further arrives: it expires.
        let armed_for = deadline.at();
        assert_eq!(
            broker.expire_question_if_due(&q.id, armed_for),
            Expiry::Expired
        );
        assert!(!broker.has_pending_for_session("s1"));
        // And a question someone else already took reports Raced, so the
        // handler waits out the grace window instead of dropping their click.
        assert_eq!(
            broker.expire_question_if_due(&q.id, armed_for),
            Expiry::Raced
        );
    }

    #[tokio::test]
    async fn approvals_and_questions_share_session_pending_state() {
        let broker = Broker::default();
        let (a, _rx_a) = broker.register(info());
        let (q, _rx_q) = broker.register_question(question(vec![spec("Approach", false, &["A"])]), test_deadline());

        assert!(broker.decide(&a.id, Decision::Allow).is_some());
        assert!(
            broker.has_pending_for_session("s1"),
            "a parked question must keep the session in NeedsAttention"
        );
        assert!(matches!(
            broker.answer(&q.id, picked(0, &["A"])),
            AnswerResult::Accepted(_)
        ));
        assert!(!broker.has_pending_for_session("s1"));
    }
}
