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
    /// A settings audition rather than a notification.
    ///
    /// It exists BECAUSE the user just asked for it, so none of the
    /// suppression that protects them from a chatty agent applies: not the
    /// per-session window, not the identical-text one, and it leaves neither
    /// behind. Without this, dragging a slider is silent after the first
    /// move — every sample says the same sentence, and the 10 s identical-text
    /// guard swallows it — which is exactly the feedback the sliders exist
    /// for. It also interrupts, so the newest setting is what you hear.
    pub audition: bool,
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
    // Probed ONCE: these are properties of the backend, not of an utterance.
    let features = tts.supported_features();
    let ranges = SpeechRanges {
        rate: features
            .rate
            .then(|| (tts.min_rate(), tts.normal_rate(), tts.max_rate())),
        volume: features
            .volume
            .then(|| (tts.min_volume(), tts.normal_volume(), tts.max_volume())),
    };
    log::info!("speech ranges: rate={:?} volume={:?}", ranges.rate, ranges.volume);
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
        // Per-session dedup window; approvals and auditions are never
        // suppressed.
        let dedup_key = (utterance.session_id.clone(), utterance.priority);
        if utterance.priority != Priority::Approval && !utterance.audition {
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
            // Deliberately NOT "suite". The settings UI only ever writes
            // `claude-code` and `codex`, so a key of its own would mean the
            // fleet ignored the voice, rate and volume the user actually
            // chose — full blast in a shared office, with no control anywhere
            // to turn it down. It speaks with the app's own settings, which
            // is the same call `digest.rs` makes for the same reason.
            //
            // The distinction lives where it belongs: on the board, where the
            // row wears the fleet's own mark and label.
            AgentKind::Suite => "claude-code",
        };
        // ONE config read per utterance for all three settings: they live in
        // the same file and each `load()` re-reads and re-parses it.
        let speech = crate::config::speech_settings(agent_key);
        let wanted = utterance
            .voice_override
            .clone()
            .or(speech.voice)
            .or_else(|| match utterance.agent {
                AgentKind::ClaudeCode => defaults.0.clone(),
                AgentKind::Codex => defaults.1.clone(),
                // The fleet is the app talking about itself, so it borrows the
                // app's own voice — the same reasoning the daily digest uses.
                // `pick_defaults`' never-collide rule exists to tell two AGENTS
                // apart by ear; the bell is not a third agent.
                AgentKind::Suite => defaults.0.clone(),
            });
        // Rate and volume are read per utterance, like the voice above, so a
        // slider move applies to the very next callout instead of the next
        // launch. Mapped through the ranges the ENGINE reports rather than
        // any constant: the crate's normal/min/max differ per backend (this
        // machine: rate 0.1/0.5/2.0, volume 0.0/1.0/1.0) and a hard-coded
        // number would mean something different on Windows.
        let spoke = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Computed in HERE, not outside: `clamp` panics if a backend ever
            // reports min > max or a NaN bound, and losing one utterance is
            // the failure this guard exists to convert a dead thread into.
            let rate = engine_rate(&ranges, speech.rate);
            let volume = engine_volume(&ranges, speech.volume);
            // Failures are logged and ignored: a backend that will not take a
            // rate must still speak the sentence.
            if let Some(rate) = rate {
                if let Err(err) = tts.set_rate(rate) {
                    log::debug!("TTS set_rate({rate}) failed: {err}");
                }
            }
            if let Some(volume) = volume {
                if let Err(err) = tts.set_volume(volume) {
                    log::debug!("TTS set_volume({volume}) failed: {err}");
                }
            }
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
            // An audition interrupts for the same reason an approval does:
            // what is being spoken is already out of date the moment the user
            // moves the slider again.
            let interrupt = utterance.priority == Priority::Approval || utterance.audition;
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
            // An audition leaves no trace: recording it would let a slider
            // sample suppress the REAL callout that follows it.
            Ok(true) if utterance.audition => {}
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

/// What the backend says it can do with rate and volume: `(min, normal, max)`
/// each, or None when the feature is unsupported (then we never call the
/// setter at all rather than guessing).
struct SpeechRanges {
    rate: Option<(f32, f32, f32)>,
    volume: Option<(f32, f32, f32)>,
}

/// A user multiplier (1.0 = the engine's normal) turned into an engine value.
///
/// Multiplication, not interpolation between min and max: "1.5×" has to mean
/// half again as fast, and on this machine's ranges (0.1 / 0.5 / 2.0)
/// interpolating 0.5× would land on 0.1 — a fifth of normal, wearing a label
/// that says half. The clamp is what keeps a backend whose max sits below
/// `normal × 2` honest.
fn engine_rate(ranges: &SpeechRanges, multiplier: f64) -> Option<f32> {
    let (min, normal, max) = ranges.rate?;
    Some(((normal as f64 * multiplier) as f32).clamp(min, max))
}

/// Volume is a FRACTION of normal, not a multiplier: 1.0 is as loud as the app
/// has ever been, and there is nothing above it to ask for.
fn engine_volume(ranges: &SpeechRanges, fraction: f64) -> Option<f32> {
    let (min, normal, max) = ranges.volume?;
    Some(((normal as f64 * fraction) as f32).clamp(min, max))
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


/// Which shape the spoken line takes (AC-4.3). One global choice, because a
/// user who wants short lines wants them from every agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Style {
    /// What the app has always said: a full sentence, then the summary.
    #[default]
    Terse,
    /// Longer sentences, addressed to a person rather than reporting a fact.
    Conversational,
    /// The ping without the essay: agent, project, state, and NOTHING else.
    /// Summaries are dropped entirely — that is the point of choosing it.
    StatusOnly,
}

impl Style {
    pub fn as_str(self) -> &'static str {
        match self {
            Style::Terse => "terse",
            Style::Conversational => "conversational",
            Style::StatusOnly => "status_only",
        }
    }

    /// Tolerant, like every other config read: an unrecognized value is the
    /// default rather than an error the user cannot see.
    pub fn parse(raw: &str) -> Style {
        match raw.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "conversational" => Style::Conversational,
            "status_only" | "status" => Style::StatusOnly,
            _ => Style::Terse,
        }
    }
}

/// What is being announced. Every arm carries exactly what its sentence needs,
/// so the style table below is the ONLY place wording lives (AC-4.3).
#[derive(Debug, Clone, Copy)]
pub enum Callout<'a> {
    Completion { summary: &'a str },
    Attention { summary: &'a str },
    /// §6: preempts the queue on insert.
    ApprovalRequest { tool: &'a str },
    /// AC-6.4: every decision is voiced.
    Decision { decision: &'a str, tool: &'a str },
    /// The morning digest (§12.5). Carries the finished sentence body rather
    /// than the numbers: the counting belongs to the registry, and the style
    /// table's job here is only how to introduce it.
    Digest { span: DigestSpan, body: &'a str },
}

/// Which stretch of time a digest covers. A type rather than a phrase,
/// because the three styles introduce it three different ways and
/// lower-casing a label produced "Here is how since friday went."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestSpan {
    Yesterday,
    /// A Monday, covering the weekend it would otherwise skip.
    SinceFriday,
}

impl DigestSpan {
    /// The label the board card and the terse line both use.
    pub fn label(self) -> &'static str {
        match self {
            DigestSpan::Yesterday => "Yesterday",
            DigestSpan::SinceFriday => "Since Friday",
        }
    }
}

/// The one place a spoken line is composed.
///
/// Pure: no config read, no clock, no engine. Every caller passes the style it
/// was handed, which is what makes the whole table testable and what lets the
/// voice preview say exactly what a real callout would.
pub fn callout(style: Style, kind: Callout<'_>, agent: AgentKind, project: &str) -> String {
    let name = agent_label(agent);
    // Status-only never speaks a summary, so it is the only arm that ignores
    // one; the other two append it when there is one to append.
    let with = |lead: String, summary: &str| {
        if summary.is_empty() {
            lead
        } else {
            format!("{lead} {summary}")
        }
    };
    match (style, kind) {
        (Style::Terse, Callout::Completion { summary }) => {
            with(format!("{name} finished in {project}."), summary)
        }
        (Style::Conversational, Callout::Completion { summary }) => {
            with(format!("{name} has finished up in {project}."), summary)
        }
        (Style::StatusOnly, Callout::Completion { .. }) => format!("{name}, {project}: done."),

        (Style::Terse, Callout::Attention { summary }) => {
            with(format!("{name} needs you in {project}."), summary)
        }
        (Style::Conversational, Callout::Attention { summary }) => {
            with(format!("{name} is waiting on you over in {project}."), summary)
        }
        (Style::StatusOnly, Callout::Attention { .. }) => {
            format!("{name}, {project}: waiting on you.")
        }

        (Style::Terse, Callout::ApprovalRequest { tool }) => {
            format!("{name} wants to run {} in {project}.", speakable_tool(tool))
        }
        (Style::Conversational, Callout::ApprovalRequest { tool }) => format!(
            "{name} is asking to run {} in {project}.",
            speakable_tool(tool)
        ),
        (Style::StatusOnly, Callout::ApprovalRequest { tool }) => {
            format!("{name}, {project}: approve {}?", speakable_tool(tool))
        }

        (Style::Terse, Callout::Decision { decision, tool }) => {
            format!("{} {} in {project}.", verb(decision), speakable_tool(tool))
        }
        // NOT `verb()` lowercased: the fall-through verb is "Sent to
        // terminal:", and reading a colon out mid-sentence ("You sent to
        // terminal: a bash command") is the kind of thing only a template
        // table notices. Every style spells its own decision out.
        (Style::Conversational, Callout::Decision { decision, tool }) => match decision {
            "allow" => format!("You approved {} in {project}.", speakable_tool(tool)),
            "deny" => format!("You denied {} in {project}.", speakable_tool(tool)),
            _ => format!(
                "You sent {} to the terminal in {project}.",
                speakable_tool(tool)
            ),
        },
        (Style::StatusOnly, Callout::Decision { decision, tool }) => {
            format!("{project}: {} {}.", speakable_tool(tool), past(decision))
        }

        // The digest names no session, so `project` is not part of it — the
        // one callout that is about the user's day rather than one agent's.
        (Style::Terse | Style::StatusOnly, Callout::Digest { span, body }) => {
            format!("{}: {body}", span.label())
        }
        (Style::Conversational, Callout::Digest { span, body }) => match span {
            DigestSpan::Yesterday => format!("Here's how yesterday went. {body}"),
            DigestSpan::SinceFriday => format!("Here's how things have gone since Friday. {body}"),
        },
    }
}

fn verb(decision: &str) -> &'static str {
    match decision {
        "allow" => "Approved",
        "deny" => "Denied",
        _ => "Sent to terminal:",
    }
}

fn past(decision: &str) -> &'static str {
    match decision {
        "allow" => "approved",
        "deny" => "denied",
        _ => "sent to the terminal",
    }
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
        AgentKind::Suite => "the fleet",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say(style: Style, kind: Callout<'_>) -> String {
        callout(style, kind, AgentKind::ClaudeCode, "api-server")
    }

    #[test]
    fn templates_read_naturally() {
        assert_eq!(
            say(
                Style::Terse,
                Callout::Completion {
                    summary: "Fixed the bug."
                }
            ),
            "Claude finished in api-server. Fixed the bug."
        );
        assert_eq!(
            callout(
                Style::Terse,
                Callout::Attention { summary: "" },
                AgentKind::Codex,
                "web"
            ),
            "Codex needs you in web."
        );
    }

    /// Every style says something different for every kind of callout — the
    /// promise the setting makes (AC-4.3). Without this a style could quietly
    /// fall back to terse for one kind and nobody would notice until they had
    /// chosen it and heard the old wording.
    #[test]
    fn every_style_changes_the_shape_of_every_callout() {
        let kinds: [Callout<'_>; 5] = [
            Callout::Completion { summary: "Done." },
            Callout::Attention { summary: "Done." },
            Callout::ApprovalRequest { tool: "Bash" },
            Callout::Decision {
                decision: "allow",
                tool: "Bash",
            },
            // The fall-through decision, which is where the wording is
            // easiest to get wrong: the terse verb ends in a colon, and
            // lower-casing it into a sentence produced "You sent to terminal:
            // a bash command in api-server."
            Callout::Decision {
                decision: "ask",
                tool: "Bash",
            },
        ];
        // The digest is deliberately excluded from the matrix below: it names
        // no project, because it is about the day rather than a session.
        assert_eq!(
            say(
                Style::Terse,
                Callout::Digest {
                    span: DigestSpan::Yesterday,
                    body: "3 sessions.",
                }
            ),
            "Yesterday: 3 sessions."
        );
        // A Monday must not read "Here's how since friday went."
        assert_eq!(
            say(
                Style::Conversational,
                Callout::Digest {
                    span: DigestSpan::SinceFriday,
                    body: "3 sessions.",
                }
            ),
            "Here's how things have gone since Friday. 3 sessions."
        );
        for kind in kinds {
            let lines = [
                say(Style::Terse, kind),
                say(Style::Conversational, kind),
                say(Style::StatusOnly, kind),
            ];
            for line in &lines {
                assert!(line.contains("api-server"), "{line:?} names no project");
                assert!(!line.is_empty());
            }
            // Conversational is prose — it must read as a sentence someone
            // would say. Terse and status-only are labels and may punctuate
            // like labels ("Claude, api-server: done."), but reusing the
            // terse verb here produced "You sent to terminal: a bash command
            // in api-server.", which is the bug this pins.
            assert!(
                !lines[1].contains(':'),
                "conversational must not read a colon mid-sentence: {:?}",
                lines[1]
            );
            assert!(
                lines[0] != lines[1] && lines[1] != lines[2] && lines[0] != lines[2],
                "styles must differ: {lines:?}"
            );
        }
    }

    /// Status-only exists to be the ping without the essay. A summary leaking
    /// into it would silently undo the one thing the user asked for.
    #[test]
    fn status_only_never_speaks_the_summary() {
        let secret = "the model said something long and specific";
        for kind in [
            Callout::Completion { summary: secret },
            Callout::Attention { summary: secret },
        ] {
            let line = say(Style::StatusOnly, kind);
            assert!(!line.contains(secret), "{line:?}");
            assert!(line.chars().count() < 40, "{line:?} is not a status line");
        }
    }

    /// A missing summary must never leave a dangling space or a stray full
    /// stop — the two styles that DO speak summaries have to survive an empty
    /// one, which is the common case for Codex.
    #[test]
    fn an_empty_summary_leaves_a_clean_sentence() {
        for style in [Style::Terse, Style::Conversational, Style::StatusOnly] {
            for kind in [
                Callout::Completion { summary: "" },
                Callout::Attention { summary: "" },
            ] {
                let line = say(style, kind);
                assert!(line.ends_with('.'), "{line:?}");
                assert!(!line.contains("  "), "{line:?}");
                assert!(!line.ends_with(" ."), "{line:?}");
            }
        }
    }

    #[test]
    fn style_parses_tolerantly_and_round_trips() {
        for (raw, want) in [
            ("terse", Style::Terse),
            ("Conversational", Style::Conversational),
            ("status_only", Style::StatusOnly),
            ("status-only", Style::StatusOnly),
            (" STATUS ", Style::StatusOnly),
            // Anything unrecognized is the default, not an error the user
            // cannot see.
            ("wat", Style::Terse),
            ("", Style::Terse),
        ] {
            assert_eq!(Style::parse(raw), want, "{raw:?}");
        }
        for style in [Style::Terse, Style::Conversational, Style::StatusOnly] {
            assert_eq!(Style::parse(style.as_str()), style);
        }
    }

    /// The mapping from "1.5×" to an engine number, against the ranges THIS
    /// machine reports (rate 0.1 / 0.5 / 2.0, volume 0.0 / 1.0 / 1.0 —
    /// measured, see `print_backend_speech_ranges`).
    #[test]
    fn rate_and_volume_map_through_the_backends_own_ranges() {
        let mac = SpeechRanges {
            rate: Some((0.1, 0.5, 2.0)),
            volume: Some((0.0, 1.0, 1.0)),
        };
        assert_eq!(engine_rate(&mac, 1.0), Some(0.5), "normal stays normal");
        assert_eq!(engine_rate(&mac, 2.0), Some(1.0), "twice as fast");
        assert_eq!(engine_rate(&mac, 0.5), Some(0.25), "half as fast");
        assert_eq!(engine_volume(&mac, 1.0), Some(1.0));
        assert_eq!(engine_volume(&mac, 0.25), Some(0.25));

        // A backend whose ceiling sits below normal × 2 must be clamped, not
        // handed a number it will reject (or worse, accept).
        let tight = SpeechRanges {
            rate: Some((0.5, 1.0, 1.2)),
            volume: Some((0.2, 0.8, 0.9)),
        };
        assert_eq!(engine_rate(&tight, 2.0), Some(1.2));
        assert_eq!(engine_rate(&tight, 0.5), Some(0.5), "floor holds too");
        assert_eq!(engine_volume(&tight, 0.0), Some(0.2));

        // Unsupported: never call the setter at all rather than guess.
        let none = SpeechRanges {
            rate: None,
            volume: None,
        };
        assert_eq!(engine_rate(&none, 1.5), None);
        assert_eq!(engine_volume(&none, 0.5), None);
    }

    /// Probe, not a guarantee: prints what THIS machine's speech backend
    /// reports for rate and volume. §11.2 requires the numbers to be measured
    /// rather than assumed — the crate's normal/min/max differ per backend —
    /// and the mapping in `engine_rate`/`engine_volume` is written against
    /// whatever comes out of here. Ignored by default: CI has no speech engine.
    #[test]
    #[ignore = "requires a speech engine (prints this machine's ranges)"]
    fn print_backend_speech_ranges() {
        let tts = tts::Tts::default().expect("speech engine");
        let features = tts.supported_features();
        println!(
            "rate: min={} normal={} max={} supported={}",
            tts.min_rate(),
            tts.normal_rate(),
            tts.max_rate(),
            features.rate
        );
        println!(
            "volume: min={} normal={} max={} supported={}",
            tts.min_volume(),
            tts.normal_volume(),
            tts.max_volume(),
            features.volume
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
            audition: false,
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




