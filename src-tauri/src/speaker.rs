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
    let voices = pick_voices(&tts);
    let mut pending: Vec<Utterance> = Vec::new();
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
            pending.push(u);
        }
        if pending.is_empty() {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(u) => pending.push(u),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        let speaking = tts.is_speaking().unwrap_or(false);
        let has_approval = pending.iter().any(|u| u.priority == Priority::Approval);
        if speaking && !has_approval {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        // Highest priority first, FIFO within a priority (stable min search).
        let best = pending
            .iter()
            .enumerate()
            .min_by_key(|(i, u)| (u.priority, *i))
            .map(|(i, _)| i)
            .expect("pending is non-empty");
        let utterance = pending.remove(best);

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
        let spoke = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(voice) = voices.for_agent(utterance.agent) {
                let _ = tts.set_voice(voice);
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

struct VoiceMap {
    claude: Option<tts::Voice>,
    codex: Option<tts::Voice>,
}

impl VoiceMap {
    fn for_agent(&self, agent: AgentKind) -> Option<&tts::Voice> {
        match agent {
            AgentKind::ClaudeCode => self.claude.as_ref(),
            AgentKind::Codex => self.codex.as_ref(),
        }
    }
}

/// Distinct default voices per agent (AC-4.2); settings UI comes in step 7.
fn pick_voices(tts: &tts::Tts) -> VoiceMap {
    let voices = tts.voices().unwrap_or_default();
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
    let claude = by_name("Samantha")
        .or_else(|| pool.first().copied())
        .cloned();
    let codex = by_name("Daniel")
        .or_else(|| {
            pool.iter()
                .find(|v| Some(v.id()) != claude.as_ref().map(|c| c.id()))
                .copied()
        })
        .cloned();

    log::info!(
        "voice defaults: claude-code={:?} codex={:?} ({} system voices)",
        claude.as_ref().map(|v| v.name()),
        codex.as_ref().map(|v| v.name()),
        voices.len()
    );
    VoiceMap { claude, codex }
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
}
