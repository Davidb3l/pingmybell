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
    /// Speak in THIS voice regardless of what the agent is configured to
    /// use. Only auditioning sets it: choosing a voice has to be hearable
    /// before it is saved, or the picker is guesswork.
    pub voice_override: Option<String>,
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

/// What the worker accepts. Enumerating voices has to be a MESSAGE rather
/// than a free function, because the platform allows exactly one speech
/// engine per process and the worker owns it: every other
/// `tts::Tts::default()` in this process fails, and the old code turned that
/// failure into an empty list, so the settings picker showed nothing at all.
enum Command {
    Speak(Box<Utterance>),
    /// Reply with the enumerated voices. The reply channel is bounded to one
    /// value and the caller may give up waiting, so a send error here is
    /// normal and ignored.
    Voices(mpsc::Sender<Vec<VoiceOption>>),
}

#[derive(Clone)]
pub struct SpeakerHandle {
    tx: mpsc::Sender<Command>,
    muted: Arc<AtomicBool>,
}

impl SpeakerHandle {
    /// Voices as the ONE live engine sees them.
    ///
    /// Blocking, with a deadline: the worker answers between utterances, and
    /// a caller must not hang the settings panel if the speaker is wedged in
    /// a platform call.
    pub fn voices(&self) -> Vec<VoiceOption> {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Command::Voices(reply)).is_err() {
            return Vec::new();
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default()
    }

    pub fn enqueue(&self, utterance: Utterance) {
        if self.tx.send(Command::Speak(Box::new(utterance))).is_err() {
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
    let (tx, rx) = mpsc::channel::<Command>();
    let muted = Arc::new(AtomicBool::new(false));
    let worker_muted = muted.clone();
    std::thread::Builder::new()
        .name("speaker".into())
        .spawn(move || worker(rx, worker_muted))
        .expect("failed to spawn speaker thread");
    SpeakerHandle { tx, muted }
}

fn worker(rx: mpsc::Receiver<Command>, muted: Arc<AtomicBool>) {
    let mut tts = match tts::Tts::default() {
        Ok(t) => t,
        Err(err) => {
            log::error!("TTS unavailable; voice callouts disabled: {err}");
            // Drain forever so senders never block or error — and answer
            // voice queries with an empty list rather than letting the
            // caller wait out its deadline.
            while let Ok(cmd) = rx.recv() {
                if let Command::Voices(reply) = cmd {
                    let _ = reply.send(Vec::new());
                }
            }
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
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::Speak(u) => enqueue_pending(&mut pending, *u, Instant::now()),
                Command::Voices(reply) => {
                    let _ = reply.send(options_from(&tts.voices().unwrap_or_default()));
                }
            }
        }
        drop_stale(&mut pending, Instant::now());
        if pending.is_empty() {
            // BLOCK. This used to be a 200 ms `recv_timeout` whose only
            // outcome on an idle machine was Timeout → continue: five
            // wakeups a second for the life of the tray process, which is
            // what AC-5.5 rules out and what keeps App Nap from ever
            // parking us. A plain `recv` is exactly equivalent and free.
            match rx.recv() {
                Ok(Command::Speak(u)) => enqueue_pending(&mut pending, *u, Instant::now()),
                Ok(Command::Voices(reply)) => {
                    let _ = reply.send(options_from(&tts.voices().unwrap_or_default()));
                }
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
        let wanted = utterance
            .voice_override
            .clone()
            .or_else(|| crate::config::voice_for(agent_key))
            .or_else(|| match utterance.agent {
            AgentKind::ClaudeCode => defaults.0.clone(),
            AgentKind::Codex => defaults.1.clone(),
        });
        let spoke = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(name) = &wanted {
                // Best variant, not first match: one name can mean a
                // compact voice, an enhanced one, and a foreign Siri bundle
                // that merely shares the name (there is a French "Daniel").
                let best = voices
                    .iter()
                    .filter(|v| v.name().eq_ignore_ascii_case(name))
                    .max_by(|a, b| rank(&VoiceRef::of(a)).cmp(&rank(&VoiceRef::of(b))));
                if let Some(voice) = best {
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
/// One enumerated system voice, reduced to what choosing one needs. Kept
/// separate from `tts::Voice` so the selection rules are testable without a
/// speech engine on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VoiceRef {
    name: String,
    id: String,
    language: String,
    english: bool,
}

impl VoiceRef {
    fn of(v: &tts::Voice) -> Self {
        let lang = v.language();
        Self {
            name: v.name(),
            id: v.id(),
            language: lang.to_string(),
            english: lang.primary_language().starts_with("en"),
        }
    }
}

/// Quality tier, read out of the voice identifier.
///
/// macOS ships SEVERAL distinct voices under one display name: `Samantha` is
/// both `com.apple.voice.compact.en-US.Samantha` and
/// `com.apple.voice.enhanced.en-US.Samantha` once the user downloads the
/// better one, and Siri's bundles mark the tier with a trailing suffix
/// instead (`com.apple.ttsbundle.siri_nicky_en-US_premium`). Nothing in the
/// NAME distinguishes them, so matching on the name alone lands on whichever
/// the system happened to enumerate first — and the compact tier is the one
/// that sounds like a 2005 screen reader.
fn quality_rank(id: &str) -> u8 {
    let id = id.to_ascii_lowercase();
    if id.contains("premium") {
        3
    } else if id.contains("enhanced") {
        2
    } else {
        1
    }
}

/// Sort key: English first, then the best tier available, then the id, so a
/// tie resolves the same way on every launch instead of following whatever
/// order the speech engine enumerated.
fn rank(v: &VoiceRef) -> (bool, u8, &str) {
    (v.english, quality_rank(&v.id), v.id.as_str())
}

/// The best voice going by this display name — the whole point being that
/// one name can mean several voices of very different quality.
fn best_named<'a>(voices: &'a [VoiceRef], name: &str) -> Option<&'a VoiceRef> {
    voices
        .iter()
        .filter(|v| v.name.eq_ignore_ascii_case(name))
        .max_by(|a, b| rank(a).cmp(&rank(b)))
}

/// The best voice on the machine, optionally avoiding one already taken.
fn best_overall<'a>(voices: &'a [VoiceRef], taken: Option<&str>) -> Option<&'a VoiceRef> {
    voices
        .iter()
        .filter(|v| Some(v.id.as_str()) != taken)
        .max_by(|a, b| rank(a).cmp(&rank(b)))
}

/// Distinct default voice names per agent (AC-4.2).
///
/// Samantha and Daniel stay the preferred identities, but only if the
/// machine has them at a decent tier. A user who has downloaded better
/// voices should hear them without first finding the settings panel, so a
/// preferred name that exists ONLY in the compact tier loses to the best
/// voice actually installed.
fn pick_defaults(voices: &[VoiceRef]) -> (Option<String>, Option<String>) {
    let upgrade = |preferred: Option<&VoiceRef>, taken: Option<&str>| -> Option<VoiceRef> {
        let best = best_overall(voices, taken);
        match (preferred, best) {
            // Keep the preferred identity unless it is the BOTTOM tier and
            // something genuinely better is installed. Deliberately not
            // "whenever anything ranks higher": Samantha (Enhanced) is a
            // good voice, and swapping it for a premium one the moment a
            // premium one appears would change the voice a user already
            // knows for no audible gain.
            (Some(p), Some(b)) if quality_rank(&p.id) == 1 && quality_rank(&b.id) > 1 => {
                Some(b.clone())
            }
            (Some(p), _) => Some(p.clone()),
            (None, b) => b.cloned(),
        }
    };
    let claude = upgrade(best_named(voices, "Samantha"), None);
    let codex = upgrade(
        best_named(voices, "Daniel"),
        claude.as_ref().map(|c| c.id.as_str()),
    );
    // Distinctness is the requirement, not the names: if both landed on the
    // same voice, move the second one off it.
    let codex = match (&claude, codex) {
        (Some(c), Some(x)) if x.id == c.id => best_overall(voices, Some(&c.id)).cloned(),
        (_, other) => other,
    };
    (claude.map(|v| v.name), codex.map(|v| v.name))
}

fn pick_default_names(voices: &[tts::Voice]) -> (Option<String>, Option<String>) {
    let refs: Vec<VoiceRef> = voices.iter().map(VoiceRef::of).collect();
    pick_defaults(&refs)
}

#[cfg(test)]
mod voice_choice_tests {
    use super::*;

    fn v(name: &str, id: &str) -> VoiceRef {
        VoiceRef {
            name: name.into(),
            id: id.into(),
            language: if id.contains("fr-FR") { "fr-FR".into() } else { "en-US".into() },
            english: id.contains("en-US") || id.contains("en-GB"),
        }
    }

    /// The real shapes off a macOS 15.5 machine, including the two traps:
    /// one name covering several tiers, and a FRENCH Siri voice that also
    /// answers to "Daniel".
    fn machine() -> Vec<VoiceRef> {
        vec![
            v("Samantha", "com.apple.voice.compact.en-US.Samantha"),
            v("Samantha", "com.apple.voice.enhanced.en-US.Samantha"),
            v("Daniel", "com.apple.voice.compact.en-GB.Daniel"),
            v("Daniel", "com.apple.ttsbundle.siri_dan_fr-FR_compact"),
            v("Nicky", "com.apple.ttsbundle.siri_nicky_en-US_premium"),
            v("Ava", "com.apple.voice.enhanced.en-US.Ava"),
        ]
    }

    #[test]
    fn one_name_many_voices_resolves_to_the_best_tier() {
        let m = machine();
        assert_eq!(
            best_named(&m, "Samantha").unwrap().id,
            "com.apple.voice.enhanced.en-US.Samantha"
        );
        // Case-insensitive, like the lookup it replaces.
        assert_eq!(best_named(&m, "samantha"), best_named(&m, "Samantha"));
        assert_eq!(best_named(&m, "Nobody"), None);
    }

    #[test]
    fn a_foreign_voice_sharing_a_name_never_wins() {
        // Both are compact, so only the English test separates them — and
        // announcing an English summary in a French voice is the bug.
        assert_eq!(
            best_named(&machine(), "Daniel").unwrap().id,
            "com.apple.voice.compact.en-GB.Daniel"
        );
    }

    #[test]
    fn siri_premium_outranks_enhanced_outranks_compact() {
        assert!(
            quality_rank("com.apple.ttsbundle.siri_nicky_en-US_premium")
                > quality_rank("com.apple.voice.enhanced.en-US.Ava")
        );
        assert!(
            quality_rank("com.apple.voice.enhanced.en-US.Ava")
                > quality_rank("com.apple.voice.compact.en-US.Samantha")
        );
    }

    #[test]
    fn defaults_keep_their_identity_when_the_machine_has_them_decently() {
        let (claude, codex) = pick_defaults(&machine());
        assert_eq!(claude.as_deref(), Some("Samantha"));
        // Daniel is compact-only here while a premium voice exists, so the
        // second slot upgrades rather than sounding like 2005.
        assert_eq!(codex.as_deref(), Some("Nicky"));
    }

    #[test]
    fn defaults_stay_distinct_and_survive_a_bare_machine() {
        // Only one voice on the box: the first slot takes it, the second has
        // nothing left rather than doubling up.
        let bare = vec![v("Samantha", "com.apple.voice.compact.en-US.Samantha")];
        let (claude, codex) = pick_defaults(&bare);
        assert_eq!(claude.as_deref(), Some("Samantha"));
        assert_eq!(codex, None);
        // No voices at all must not panic.
        assert_eq!(pick_defaults(&[]), (None, None));
    }

    #[test]
    fn selection_does_not_depend_on_enumeration_order() {
        let mut reversed = machine();
        reversed.reverse();
        assert_eq!(pick_defaults(&machine()), pick_defaults(&reversed));
        assert_eq!(
            best_named(&machine(), "Samantha"),
            best_named(&reversed, "Samantha")
        );
    }
}

/// A voice as the settings UI needs to show it.
///
/// The name alone is not enough to choose with: macOS lists several distinct
/// voices under one name, and the picker previously showed ~100 bare strings
/// with nothing to say which were worth having.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VoiceOption {
    pub name: String,
    /// BCP-47-ish tag as reported by the engine, e.g. `en-US`.
    pub language: String,
    /// `premium` | `enhanced` | `standard`.
    pub quality: &'static str,
    /// `siri` | `standard` | `eloquence` | `novelty`, read off the id family.
    /// Novelty covers the joke voices (Bells, Zarvox, Boing) which are useless
    /// for announcements and would otherwise sit among the real ones.
    pub family: &'static str,
    pub english: bool,
}

fn family_of(id: &str) -> &'static str {
    if id.starts_with("com.apple.ttsbundle") {
        "siri"
    } else if id.starts_with("com.apple.speech.synthesis.voice") {
        "novelty"
    } else if id.starts_with("com.apple.eloquence") {
        "eloquence"
    } else {
        "standard"
    }
}

fn quality_label(id: &str) -> &'static str {
    match quality_rank(id) {
        3 => "premium",
        2 => "enhanced",
        _ => "standard",
    }
}

/// One entry per NAME — the best variant of it — because two rows reading
/// "Samantha" with no visible difference is worse than one that is simply
/// the good one. Ordered the way a chooser wants it: English first, best
/// quality first, then alphabetical.
/// Build the picker's view of a voice list.
///
/// Takes the voices rather than enumerating them: only the worker's engine
/// can enumerate (one per process), so this is the pure half and the worker
/// supplies the input.
fn options_from(voices: &[tts::Voice]) -> Vec<VoiceOption> {
    let refs: Vec<VoiceRef> = voices.iter().map(VoiceRef::of).collect();
    let mut best: std::collections::HashMap<String, &VoiceRef> = std::collections::HashMap::new();
    for v in &refs {
        let key = v.name.to_lowercase();
        match best.get(&key) {
            Some(existing) if rank(existing) >= rank(v) => {}
            _ => {
                best.insert(key, v);
            }
        }
    }
    let mut out: Vec<VoiceOption> = best
        .into_values()
        .map(|v| VoiceOption {
            name: v.name.clone(),
            language: v.language.clone(),
            quality: quality_label(&v.id),
            family: family_of(&v.id),
            english: v.english,
        })
        .collect();
    out.sort_by(|a, b| {
        b.english
            .cmp(&a.english)
            .then_with(|| quality_rank_label(b.quality).cmp(&quality_rank_label(a.quality)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

fn quality_rank_label(q: &str) -> u8 {
    match q {
        "premium" => 3,
        "enhanced" => 2,
        _ => 1,
    }
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
            voice_override: None,
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




