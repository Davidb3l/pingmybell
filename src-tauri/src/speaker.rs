//! Voice engine (FR-4): OS-native TTS behind a queue on a dedicated thread.
//! One utterance at a time; higher priorities first (approvals will preempt
//! from step 4); per-session 5 s dedup; global mute.
//!
//! §9 invariant 2: spoken text is derived from transcripts — never log it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::registry::AgentKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Approval = 0,
    Attention = 1,
    Completion = 2,
}

#[derive(Debug)]
pub struct Utterance {
    pub priority: Priority,
    pub session_id: String,
    pub agent: AgentKind,
    pub text: String,
}

const DEDUP_WINDOW: Duration = Duration::from_secs(5);

/// An utterance waiting its turn, stamped with when the worker took it off
/// the channel.
///
/// The timestamp lives here rather than on `Utterance` so callers keep
/// building plain utterances; the worker drains the channel at the top of
/// every iteration, so this is within milliseconds of the enqueue anyway.
struct Queued {
    at: Instant,
    utterance: Utterance,
}

/// Hard cap on the backlog. The queue is drained one utterance per iteration
/// but was refilled without limit: if `is_speaking()` ever sticks true (a
/// wedged platform synthesizer), the worker loops at 100 ms and the vector
/// grows for as long as events keep arriving.
const MAX_PENDING: usize = 24;

/// How long a callout is still worth saying. Speech is a notification about
/// *now* — "Claude finished in api-server" half a minute late is noise, and
/// by then the island has said it silently anyway. Also the release valve for
/// a stuck synthesizer: the backlog ages out instead of accumulating.
const MAX_QUEUE_AGE: Duration = Duration::from_secs(30);

/// Add to the backlog, enforcing `MAX_PENDING`.
///
/// A full queue sheds the LEAST urgent, OLDEST entry — and only if the
/// newcomer is more urgent than it. Otherwise the newcomer is dropped, which
/// keeps the queue FIFO within a priority instead of letting a flood of
/// completions rotate each other out.
fn enqueue_pending(pending: &mut Vec<Queued>, utterance: Utterance, at: Instant) {
    if pending.len() >= MAX_PENDING {
        let worst = pending
            .iter()
            .enumerate()
            // Highest priority VALUE is the least urgent; `Reverse` on the
            // index picks the oldest of that bucket.
            .max_by_key(|(i, q)| (q.utterance.priority, std::cmp::Reverse(*i)))
            .map(|(i, _)| i);
        match worst {
            Some(i) if pending[i].utterance.priority > utterance.priority => {
                pending.remove(i);
            }
            _ => {
                log::warn!(
                    "speaker queue full ({MAX_PENDING}); dropping a {:?} callout",
                    utterance.priority
                );
                return;
            }
        }
    }
    pending.push(Queued { at, utterance });
}

/// Forget anything that has been waiting longer than `MAX_QUEUE_AGE`.
fn drop_stale(pending: &mut Vec<Queued>, now: Instant) {
    let before = pending.len();
    pending.retain(|q| now.duration_since(q.at) < MAX_QUEUE_AGE);
    if pending.len() != before {
        log::debug!(
            "speaker: dropped {} stale callout(s)",
            before - pending.len()
        );
    }
}

/// Index of the next utterance to speak: highest priority first, FIFO within
/// a priority (stable min search).
fn next_index(pending: &[Queued]) -> Option<usize> {
    pending
        .iter()
        .enumerate()
        .min_by_key(|(i, q)| (q.utterance.priority, *i))
        .map(|(i, _)| i)
}

#[derive(Clone)]
pub struct SpeakerHandle {
    tx: mpsc::Sender<Utterance>,
    muted: Arc<AtomicBool>,
}

impl SpeakerHandle {
    pub fn enqueue(&self, utterance: Utterance) {
        if self.tx.send(utterance).is_err() {
            // Worker thread is gone (panic in a platform TTS call): voice is
            // dead for this process — make that visible.
            log::error!("speaker thread is dead; callout dropped");
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Read back for the settings UI (step 7).
    #[allow(dead_code)]
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

pub fn spawn() -> SpeakerHandle {
    let (tx, rx) = mpsc::channel::<Utterance>();
    let muted = Arc::new(AtomicBool::new(false));
    let worker_muted = muted.clone();
    std::thread::Builder::new()
        .name("speaker".into())
        .spawn(move || worker(rx, worker_muted))
        .expect("failed to spawn speaker thread");
    SpeakerHandle { tx, muted }
}

fn worker(rx: mpsc::Receiver<Utterance>, muted: Arc<AtomicBool>) {
    let mut tts = match tts::Tts::default() {
        Ok(t) => t,
        Err(err) => {
            log::error!("TTS unavailable; voice callouts disabled: {err}");
            // Drain forever so senders never block or error.
            while rx.recv().is_ok() {}
            return;
        }
    };
    let voices = tts.voices().unwrap_or_default();
    let defaults = pick_default_names(&voices);
    log::info!(
        "voice defaults: claude-code={:?} codex={:?} ({} system voices)",
        defaults.0,
        defaults.1,
        voices.len()
    );
    let mut pending: Vec<Queued> = Vec::new();
    // Dedup is per (session, priority): repeated completions within the
    // window collapse, but an attention callout is never suppressed by a
    // completion that just spoke (AC-4.4).
    let mut last_spoken: HashMap<(String, Priority), Instant> = HashMap::new();
    // Multi-turn bursts (e.g. Codex firing one notify per internal step) can
    // repeat the exact same sentence — never say the same thing twice in
    // quick succession.
    let mut last_text: Option<(String, Instant)> = None;
    const SAME_TEXT_WINDOW: Duration = Duration::from_secs(10);

    loop {
        // Collect everything currently queued.
        while let Ok(u) = rx.try_recv() {
            enqueue_pending(&mut pending, u, Instant::now());
        }
        drop_stale(&mut pending, Instant::now());
        if pending.is_empty() {
            // BLOCK. This used to be a 200 ms `recv_timeout` whose only
            // outcome on an idle machine was Timeout → continue: five
            // wakeups a second for the life of the tray process, which is
            // what AC-5.5 rules out and what keeps App Nap from ever
            // parking us. A plain `recv` is exactly equivalent and free.
            match rx.recv() {
                Ok(u) => enqueue_pending(&mut pending, u, Instant::now()),
                // Every sender is gone: the app is shutting down.
                Err(mpsc::RecvError) => return,
            }
            continue;
        }

        let speaking = tts.is_speaking().unwrap_or(false);
        let has_approval = pending
            .iter()
            .any(|q| q.utterance.priority == Priority::Approval);
        if speaking && !has_approval {
            // Approvals still bypass this wait entirely (they speak with
            // `interrupt = true` below). If `is_speaking` ever sticks true
            // this is a 10 Hz wait rather than a spin that grows: each pass
            // re-runs `drop_stale`, so the backlog ages out and the loop
            // falls back to the blocking `recv` above.
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        let best = next_index(&pending).expect("pending is non-empty");
        let utterance = pending.remove(best).utterance;

        if muted.load(Ordering::Relaxed) {
            continue;
        }
        // Per-session dedup window; approvals are never suppressed.
        let dedup_key = (utterance.session_id.clone(), utterance.priority);
        if utterance.priority != Priority::Approval {
            if let Some(at) = last_spoken.get(&dedup_key) {
                if at.elapsed() < DEDUP_WINDOW {
                    log::debug!("deduped callout for session {}", utterance.session_id);
                    continue;
                }
            }
            if let Some((text, at)) = &last_text {
                if *text == utterance.text && at.elapsed() < SAME_TEXT_WINDOW {
                    log::debug!("deduped identical callout text");
                    continue;
                }
            }
        }

        // Contain panics from platform TTS calls: losing one utterance beats
        // silently killing voice for the rest of the process.
        // User-configured voice wins (checked per utterance so settings
        // changes apply immediately); otherwise the distinct defaults.
        let agent_key = match utterance.agent {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
        };
        let wanted = crate::config::voice_for(agent_key).or_else(|| match utterance.agent {
            AgentKind::ClaudeCode => defaults.0.clone(),
            AgentKind::Codex => defaults.1.clone(),
        });
        let spoke = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(name) = &wanted {
                if let Some(voice) = voices.iter().find(|v| v.name().eq_ignore_ascii_case(name)) {
                    let _ = tts.set_voice(voice);
                }
            }
            let interrupt = utterance.priority == Priority::Approval;
            match tts.speak(&utterance.text, interrupt) {
                Ok(_) => {
                    log::info!(
                        "speaking {:?} callout for session {} ({} chars)",
                        utterance.priority,
                        utterance.session_id,
                        utterance.text.chars().count()
                    );
                    true
                }
                Err(err) => {
                    log::warn!("TTS speak failed: {err}");
                    false
                }
            }
        }));
        match spoke {
            Ok(true) => {
                last_spoken.insert(dedup_key, Instant::now());
                last_text = Some((utterance.text.clone(), Instant::now()));
            }
            Ok(false) => {}
            Err(_) => log::error!("TTS backend panicked; utterance dropped"),
        }
        // The dedup map only matters within the window; keep it from growing
        // for the lifetime of the tray process.
        last_spoken.retain(|_, at| at.elapsed() < DEDUP_WINDOW);
    }
}

/// Distinct default voice names per agent (AC-4.2).
fn pick_default_names(voices: &[tts::Voice]) -> (Option<String>, Option<String>) {
    let english: Vec<&tts::Voice> = voices
        .iter()
        .filter(|v| v.language().primary_language().starts_with("en"))
        .collect();
    let pool: Vec<&tts::Voice> = if english.is_empty() {
        voices.iter().collect()
    } else {
        english
    };

    let by_name = |name: &str| {
        pool.iter()
            .find(|v| v.name().eq_ignore_ascii_case(name))
            .copied()
    };
    let claude = by_name("Samantha").or_else(|| pool.first().copied());
    let codex = by_name("Daniel").or_else(|| {
        pool.iter()
            .find(|v| Some(v.id()) != claude.map(|c| c.id()))
            .copied()
    });
    (claude.map(|v| v.name()), codex.map(|v| v.name()))
}

/// Enumerate system voice names for the settings UI (English first, then
/// the rest, deduped).
pub fn available_voices() -> Vec<String> {
    let Ok(tts) = tts::Tts::default() else {
        return Vec::new();
    };
    let voices = tts.voices().unwrap_or_default();
    let mut english: Vec<String> = Vec::new();
    let mut other: Vec<String> = Vec::new();
    for voice in &voices {
        let bucket = if voice.language().primary_language().starts_with("en") {
            &mut english
        } else {
            &mut other
        };
        let name = voice.name();
        if !bucket.contains(&name) {
            bucket.push(name);
        }
    }
    english.sort();
    other.sort();
    english.extend(other);
    english
}

/// Callout templates ("terse" style; more styles in step 7).
pub fn completion_text(agent: AgentKind, project: &str, summary: &str) -> String {
    let agent = agent_label(agent);
    if summary.is_empty() {
        format!("{agent} finished in {project}.")
    } else {
        format!("{agent} finished in {project}. {summary}")
    }
}

pub fn attention_text(agent: AgentKind, project: &str, summary: &str) -> String {
    let agent = agent_label(agent);
    if summary.is_empty() {
        format!("{agent} needs you in {project}.")
    } else {
        format!("{agent} needs you in {project}. {summary}")
    }
}

/// Approval request announcement (§6: preempts the queue on insert).
pub fn approval_request_text(agent: AgentKind, project: &str, tool_name: &str) -> String {
    format!(
        "{} wants to run {} in {project}.",
        agent_label(agent),
        speakable_tool(tool_name)
    )
}

/// Decision announcement (AC-6.4: every decision is voiced).
pub fn decision_text(decision: &str, tool_name: &str, project: &str) -> String {
    let verb = match decision {
        "allow" => "Approved",
        "deny" => "Denied",
        _ => "Sent to terminal:",
    };
    format!("{verb} {} in {project}.", speakable_tool(tool_name))
}

fn speakable_tool(tool_name: &str) -> String {
    match tool_name {
        "Bash" => "a bash command".into(),
        "Write" => "a file write".into(),
        "Edit" | "MultiEdit" => "a file edit".into(),
        // Codex's file-change tool. Same idea as Write/Edit above: the raw
        // name is what the agent called it, but nobody wants to hear
        // "apply underscore patch" read aloud.
        "apply_patch" => "a file edit".into(),
        other => other.to_string(),
    }
}

fn agent_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::ClaudeCode => "Claude",
        AgentKind::Codex => "Codex",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_read_naturally() {
        assert_eq!(
            completion_text(AgentKind::ClaudeCode, "api-server", "Fixed the bug."),
            "Claude finished in api-server. Fixed the bug."
        );
        assert_eq!(
            attention_text(AgentKind::Codex, "web", ""),
            "Codex needs you in web."
        );
    }

    #[test]
    fn priority_orders_approval_first() {
        assert!(Priority::Approval < Priority::Attention);
        assert!(Priority::Attention < Priority::Completion);
    }

    fn utterance(priority: Priority, text: &str) -> Utterance {
        Utterance {
            priority,
            session_id: "s".into(),
            agent: AgentKind::ClaudeCode,
            text: text.into(),
        }
    }

    #[test]
    fn queue_is_bounded_and_sheds_the_least_urgent_first() {
        let now = Instant::now();
        let mut pending: Vec<Queued> = Vec::new();
        for i in 0..MAX_PENDING {
            enqueue_pending(
                &mut pending,
                utterance(Priority::Completion, &i.to_string()),
                now,
            );
        }
        assert_eq!(pending.len(), MAX_PENDING);

        // A more urgent callout displaces the OLDEST least-urgent one...
        enqueue_pending(&mut pending, utterance(Priority::Approval, "approve"), now);
        assert_eq!(pending.len(), MAX_PENDING, "the cap holds");
        assert_eq!(pending[0].utterance.text, "1", "the oldest completion went");
        assert_eq!(pending[MAX_PENDING - 1].utterance.text, "approve");

        // ...and the approval is what gets spoken next, cap or no cap.
        assert_eq!(next_index(&pending), Some(MAX_PENDING - 1));

        // A newcomer with nothing less urgent than itself in the queue is
        // dropped rather than rotating it: FIFO within a priority survives.
        let mut approvals: Vec<Queued> = Vec::new();
        for i in 0..MAX_PENDING {
            enqueue_pending(
                &mut approvals,
                utterance(Priority::Approval, &format!("a{i}")),
                now,
            );
        }
        enqueue_pending(
            &mut approvals,
            utterance(Priority::Approval, "newcomer"),
            now,
        );
        assert_eq!(approvals.len(), MAX_PENDING);
        assert!(approvals.iter().all(|q| q.utterance.text != "newcomer"));
        assert_eq!(
            approvals[0].utterance.text, "a0",
            "the oldest is still first out"
        );
    }

    #[test]
    fn stale_utterances_are_dropped_but_fresh_ones_survive() {
        // Built forwards from `start`, never backwards from `now`: Instant is
        // monotonic-since-boot and subtracting past it panics.
        let start = Instant::now();
        let later = start + MAX_QUEUE_AGE + Duration::from_secs(1);
        let mut pending = vec![
            Queued {
                at: start,
                utterance: utterance(Priority::Completion, "ancient"),
            },
            Queued {
                at: start,
                // Even an approval: a request nobody heard about for half a
                // minute has long since fallen through to the terminal.
                utterance: utterance(Priority::Approval, "stale approval"),
            },
            Queued {
                at: later,
                utterance: utterance(Priority::Attention, "fresh"),
            },
        ];
        drop_stale(&mut pending, later);
        let texts: Vec<&str> = pending.iter().map(|q| q.utterance.text.as_str()).collect();
        assert_eq!(texts, vec!["fresh"]);

        // The whole point: a wedged synthesizer drains the queue to empty,
        // which is what lets the worker go back to blocking on `recv`.
        drop_stale(&mut pending, later + MAX_QUEUE_AGE);
        assert!(pending.is_empty());
    }

    #[test]
    fn next_index_prefers_priority_then_arrival() {
        let now = Instant::now();
        let mut pending: Vec<Queued> = Vec::new();
        enqueue_pending(&mut pending, utterance(Priority::Completion, "done-1"), now);
        enqueue_pending(&mut pending, utterance(Priority::Attention, "attn-1"), now);
        enqueue_pending(&mut pending, utterance(Priority::Attention, "attn-2"), now);
        enqueue_pending(&mut pending, utterance(Priority::Approval, "approval"), now);

        assert_eq!(
            pending[next_index(&pending).unwrap()].utterance.text,
            "approval"
        );
        pending.remove(next_index(&pending).unwrap());
        assert_eq!(
            pending[next_index(&pending).unwrap()].utterance.text,
            "attn-1",
            "FIFO within a priority"
        );
        assert_eq!(next_index(&[]), None);
    }
}
