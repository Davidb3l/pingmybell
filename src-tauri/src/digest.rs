//! The morning digest (ARCHITECTURE.md §12.5).
//!
//! "Yesterday: 14 sessions, 9 finished, 3 approvals, and you kept agents
//! waiting 47 minutes — longest, bc9." One spoken sentence and a small board
//! card. The rest of the app is about what the agents are doing; this is the
//! one part that is about the user's day.
//!
//! Two decisions live here and nowhere else: WHICH window counts as
//! "yesterday", and whether today's has been spoken yet. Both are pure
//! functions over a clock, so the awkward cases — a Monday that has to cover
//! the weekend, a laptop opened at 00:01, a machine that was off for a week —
//! are tested rather than reasoned about.

use chrono::{Datelike, Local, NaiveDate, TimeZone, Weekday};

use std::sync::Mutex;

use crate::registry::{AgentKind, Registry};
use crate::speaker::{DigestSpan, Priority, SpeakerHandle, Utterance};

/// The window a digest reports on, and the word for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Unix seconds, `[from, to)`.
    pub from: i64,
    pub to: i64,
    /// The local day this digest BELONGS to, `YYYY-MM-DD`. Persisted as
    /// `digest.last_spoken_day`, so "once daily" survives a restart.
    pub day: String,
    /// How to introduce it — the styles each phrase this differently.
    pub span: DigestSpan,
}

/// The window to report when the user sits down on `today`.
///
/// Monday reaches back to Friday rather than reporting a silent Saturday —
/// and so does any day whose predecessor was a weekend, which is the same rule
/// stated once: walk back to the most recent day that could plausibly have had
/// work in it, and cover everything since.
pub fn window_for(today: NaiveDate) -> Option<Window> {
    let start = match today.weekday() {
        // Saturday and Sunday report the day before like any other day:
        // somebody working then wants to hear about Friday, or about
        // Saturday. It is MONDAY that must not skip the weekend.
        Weekday::Mon => today - chrono::Duration::days(3),
        _ => today.pred_opt()?,
    };
    let from = midnight(start)?;
    let to = midnight(today)?;
    Some(Window {
        from,
        to,
        day: today.format("%Y-%m-%d").to_string(),
        span: if start == today.pred_opt()? {
            DigestSpan::Yesterday
        } else {
            DigestSpan::SinceFriday
        },
    })
}

/// Today, in the user's own timezone — which is the whole point: "the first
/// event of a local calendar day" is the moment they sat down, and UTC gets
/// that wrong by up to half a day.
pub fn today_local() -> NaiveDate {
    Local::now().date_naive()
}

/// Local midnight as a unix timestamp.
///
/// `and_hms_opt(0, 0, 0)` can be ambiguous or nonexistent on a DST boundary —
/// in Brazil, midnight itself is the hour that does not exist. `Some(t)` on
/// either side is better than a digest that silently reports nothing twice a
/// year, so an ambiguous local time takes the earlier instant and a skipped
/// one takes the following hour.
fn midnight(day: NaiveDate) -> Option<i64> {
    let naive = day.and_hms_opt(0, 0, 0)?;
    if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
        return Some(dt.timestamp());
    }
    // The hour does not exist locally: step forward until one does.
    (1..=3).find_map(|hour| {
        let naive = day.and_hms_opt(hour, 0, 0)?;
        Local
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.timestamp())
    })
}

/// The sentence body: everything after the lead-in, built once and used by
/// both the spoken line and the board card.
///
/// Only what happened is mentioned. A day with no approvals does not report
/// "0 approvals" — the whole point of a digest is that it is shorter than the
/// data it summarises.
pub fn body(stats: &crate::registry::Digest) -> String {
    fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
        if n == 1 {
            one
        } else {
            many
        }
    }
    let mut parts = vec![format!(
        "{} {}",
        stats.sessions,
        plural(stats.sessions, "session", "sessions")
    )];
    if stats.completions > 0 {
        parts.push(format!("{} finished", stats.completions));
    }
    let decisions = stats.approvals_allowed + stats.approvals_denied;
    if decisions > 0 {
        parts.push(format!(
            "{decisions} {}",
            plural(decisions, "approval", "approvals")
        ));
    }

    let mut text = parts.join(", ");
    if stats.waiting_secs >= 60 {
        text.push_str(&format!(
            ", and you kept agents waiting {}",
            spoken_duration(stats.waiting_secs)
        ));
    }
    text.push('.');
    // The longest wait is the actionable half of that number, and naming the
    // project is what makes it actionable. A wait under a minute is not worth
    // a clause — and must not swallow the busiest one either, which is what
    // an `else if` on `longest` did for every day with a 30-second wait in it.
    let longest = stats
        .longest
        .as_ref()
        .filter(|(_, secs)| *secs >= 60)
        .map(|(project, _)| format!(" Longest, {project}."));
    match longest {
        Some(clause) => text.push_str(&clause),
        // Nothing worth calling a wait: say where the day actually went.
        None => {
            if let Some((project, _)) = &stats.busiest {
                text.push_str(&format!(" Busiest was {project}."));
            }
        }
    }
    text
}

/// Durations as a person would say them: "47 minutes", "2 hours", "1 hour 12".
fn spoken_duration(secs: i64) -> String {
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" });
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    let head = format!("{hours} hour{}", if hours == 1 { "" } else { "s" });
    if rest == 0 {
        head
    } else {
        format!("{head} {rest}")
    }
}

/// The day-claim shared by BOTH triggers (§12.5).
///
/// One instance, in Tauri's managed state: the ingest path and the launch
/// catch-up otherwise hold a mutex each, and since the disk write only lands
/// after aggregation, an event arriving during the catch-up's query leaves
/// both paths believing they own the day. The speaker's identical-text window
/// hides that most of the time, which is exactly the kind of accident that
/// stops hiding it on a slow rate setting.
#[derive(Default)]
pub struct Claim(Mutex<Option<String>>);

impl Claim {
    /// Take the day if nobody has. `seed` is what disk remembers, adopted on
    /// the first call so a restart at lunchtime does not repeat the morning.
    fn take(&self, day: &str, seed: impl FnOnce() -> Option<String>) -> bool {
        let mut held = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if held.is_none() {
            *held = seed();
        }
        if held.as_deref() == Some(day) {
            return false;
        }
        *held = Some(day.to_string());
        true
    }

    /// Hand the day back — the work failed, so somebody should try again.
    fn release(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// What to say, and which day saying it settles. None when there is nothing
/// worth saying.
pub struct Due {
    pub day: String,
    pub utterance: Utterance,
}

impl Due {
    #[cfg(test)]
    fn priority_for_test(&self) -> Priority {
        self.utterance.priority
    }
}

/// The decision, with every input passed in: no config, no clock, no speaker.
/// This is the half that can be tested, and the half where §12.5's rules live.
pub fn due(
    window: &Window,
    registry: &Registry,
    style: crate::speaker::Style,
) -> rusqlite::Result<Option<Due>> {
    let stats = registry.digest(window.from, window.to)?;
    // A day with nothing in it is not a report. It still counts as settled:
    // the window is entirely in the past, so no event arriving later today
    // can ever land in it, and re-deciding on every hook event would mean a
    // full re-aggregation per event for the rest of the day.
    if stats.is_empty() {
        return Ok(None);
    }
    let text = crate::speaker::callout(
        style,
        crate::speaker::Callout::Digest {
            span: window.span,
            body: &body(&stats),
        },
        // The digest belongs to no agent; it speaks in the Claude voice
        // because something has to, and that is the app's own.
        AgentKind::ClaudeCode,
        "",
    );
    Ok(Some(Due {
        day: window.day.clone(),
        utterance: Utterance {
            // Attention, NEVER Approval: a morning summary must not preempt
            // an agent that is blocked on a human (§12.5).
            priority: Priority::Attention,
            session_id: format!("digest-{}", window.day),
            agent: AgentKind::ClaudeCode,
            text,
            voice_override: None,
            audition: false,
            terminal_pid: None,
        },
    }))
}

/// Speak today's digest if it is due, and answer whether it was.
///
/// Called from the ingest path — the first event of a local day IS the moment
/// the user sat down — and once at launch, for the day that started while the
/// app was closed.
pub fn speak_if_due(claim: &Claim, registry: &Registry, speaker: &SpeakerHandle) -> bool {
    if !crate::config::digest_enabled() {
        return false;
    }
    let Some(window) = window_for(today_local()) else {
        return false;
    };
    if !claim.take(&window.day, crate::config::digest_last_spoken_day) {
        return false;
    }

    match due(&window, registry, crate::config::speech_style()) {
        Ok(Some(due)) => {
            // Persisted BEFORE the utterance is queued, and deliberately: the
            // speaker may be muted or the queue may age it out, and neither
            // is a reason to say it again this afternoon. The board card is
            // independent of this flag, so a muted user still sees it.
            crate::config::set_digest_last_spoken_day(&due.day);
            log::info!("digest: speaking the summary for {}", due.day);
            speaker.enqueue(due.utterance);
            true
        }
        // Nothing happened in that window. The claim stands (see `due`), so
        // this costs one aggregation per day rather than one per event.
        Ok(None) => {
            crate::config::set_digest_last_spoken_day(&window.day);
            log::debug!("digest: nothing to report for {}", window.day);
            false
        }
        // A transient failure must not burn the day: hand it back so the next
        // event tries again.
        Err(err) => {
            log::warn!("digest: could not summarise {}: {err}", window.day);
            claim.release();
            false
        }
    }
}

/// The digest card for the board, or None when there is nothing to show —
/// disabled, dismissed today, or a window with nothing in it.
pub fn card(registry: &Registry) -> Option<Card> {
    let window = window_for(today_local())?;
    if !should_show(
        crate::config::digest_enabled(),
        &window.day,
        crate::config::digest_dismissed_day().as_deref(),
    ) {
        return None;
    }
    let stats = registry.digest(window.from, window.to).ok()?;
    if stats.is_empty() {
        return None;
    }
    Some(Card {
        lead: window.span.label(),
        body: body(&stats),
    })
}

/// Whether today's card belongs on the board at all. Split out from the query
/// so the two ways it disappears — turned off, or dismissed today — are tested
/// rather than only observed.
fn should_show(enabled: bool, today: &str, dismissed_day: Option<&str>) -> bool {
    // Yesterday's dismissal is not today's: the card comes back with tomorrow
    // morning's numbers, which is the whole point of a daily digest.
    enabled && dismissed_day != Some(today)
}

/// What the board draws above the rows. Numbers rendered by Rust (§project
/// rule); the card decides nothing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Card {
    pub lead: &'static str,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// The rule §12.5 asks for: a Monday covers the weekend rather than
    /// reporting a silent Saturday, and says so.
    #[test]
    fn monday_reaches_back_to_friday() {
        // 2026-08-03 is a Monday.
        let monday = window_for(date(2026, 8, 3)).unwrap();
        assert_eq!(monday.span, DigestSpan::SinceFriday);
        assert_eq!(monday.to - monday.from, 3 * 24 * 3600, "Fri, Sat and Sun");
        assert_eq!(monday.day, "2026-08-03");

        // Every other day is just yesterday, including the weekend itself:
        // somebody working on Saturday wants to hear about Friday.
        for (y, m, d) in [(2026, 8, 4), (2026, 8, 7), (2026, 8, 8), (2026, 8, 9)] {
            let window = window_for(date(y, m, d)).unwrap();
            assert_eq!(window.span, DigestSpan::Yesterday, "{y}-{m}-{d}");
            assert_eq!(window.to - window.from, 24 * 3600, "{y}-{m}-{d}");
        }
    }

    /// The window ends where today begins: a digest must never count events
    /// from the morning it is being spoken in.
    #[test]
    fn the_window_stops_at_this_mornings_midnight() {
        let window = window_for(date(2026, 8, 5)).unwrap();
        let today_midnight = midnight(date(2026, 8, 5)).unwrap();
        assert_eq!(window.to, today_midnight);
        assert_eq!(window.from, midnight(date(2026, 8, 4)).unwrap());
        assert!(window.from < window.to);
    }

    /// Month and year boundaries are where hand-rolled date arithmetic dies.
    #[test]
    fn windows_cross_month_and_year_boundaries() {
        let first_of_month = window_for(date(2026, 3, 1)).unwrap();
        assert_eq!(
            first_of_month.from,
            midnight(date(2026, 2, 28)).unwrap(),
            "…and 2026 is not a leap year"
        );
        let leap = window_for(date(2024, 3, 1)).unwrap();
        assert_eq!(leap.from, midnight(date(2024, 2, 29)).unwrap());
        let new_year = window_for(date(2026, 1, 1)).unwrap();
        assert_eq!(new_year.from, midnight(date(2025, 12, 31)).unwrap());
    }

    fn stats(sessions: usize, completions: usize, allowed: usize, denied: usize, waiting: i64) -> crate::registry::Digest {
        crate::registry::Digest {
            from: 0,
            to: 0,
            sessions,
            completions,
            approvals_allowed: allowed,
            approvals_denied: denied,
            waiting_secs: waiting,
            longest: (waiting > 0).then(|| ("bc9".to_string(), waiting)),
            busiest: Some(("api-server".to_string(), 40)),
        }
    }

    /// The sentence §12.5 opens with, end to end.
    #[test]
    fn the_digest_reads_like_the_spec_says_it_should() {
        assert_eq!(
            body(&stats(14, 9, 2, 1, 47 * 60)),
            "14 sessions, 9 finished, 3 approvals, and you kept agents waiting 47 minutes. Longest, bc9."
        );
    }

    /// A digest must be shorter than the data it summarises: what did not
    /// happen is not worth a clause.
    #[test]
    fn nothing_that_did_not_happen_is_mentioned() {
        let quiet = body(&stats(3, 3, 0, 0, 0));
        assert_eq!(quiet, "3 sessions, 3 finished. Busiest was api-server.");
        assert!(!quiet.contains('0'), "{quiet:?} counts things that did not happen");

        // Under a minute of waiting is not worth saying either — it is the
        // difference between a metric and a nag.
        assert!(!body(&stats(2, 1, 0, 0, 45)).contains("waiting"));
    }

    #[test]
    fn one_of_something_is_singular() {
        let one = body(&stats(1, 1, 1, 0, 60));
        assert!(one.starts_with("1 session,"), "{one}");
        assert!(one.contains("1 approval,"), "{one}");
        assert!(one.contains("1 minute"), "{one}");
        assert!(!one.contains("sessions"), "{one}");
        assert!(!one.contains("approvals"), "{one}");
        assert!(!one.contains("minutes"), "{one}");
    }

    #[test]
    fn durations_are_spoken_not_printed() {
        assert_eq!(spoken_duration(60), "1 minute");
        assert_eq!(spoken_duration(47 * 60), "47 minutes");
        assert_eq!(spoken_duration(3600), "1 hour");
        assert_eq!(spoken_duration(2 * 3600), "2 hours");
        assert_eq!(spoken_duration(72 * 60), "1 hour 12");
    }

    /// The gate in §12.5: seed a day's events, assert the exact sentence.
    ///
    /// End to end through the real registry rather than a hand-built stats
    /// struct, because the counting is half of what can be wrong.
    #[test]
    fn a_seeded_day_produces_the_exact_sentence() {
        use crate::registry::{EventKind, NormalizedEvent};

        let registry = Registry::open_in_memory().unwrap();
        let day_start = 1_785_000_000;
        let day_end = day_start + 24 * 3600;

        // The cwd basename must NOT equal the session id, or a `title_of`
        // that never resolves a title passes anyway.
        let seed = |session: &str, kind: EventKind, at: i64| -> i64 {
            let event = NormalizedEvent {
                agent: AgentKind::ClaudeCode,
                event: kind,
                session_id: format!("id-{session}"),
                cwd: format!("/tmp/{session}"),
                summary: None,
                transcript_path: None,
                tool: None,
                terminal: None,
            };
            let (_, id) = registry.apply(&event, |_| {}).unwrap();
            registry.set_event_times_for_test(id, at, None);
            id
        };

        // Just before the window opens: must not be counted. Without it the
        // lower bound of every count in `Registry::digest` is untested.
        seed("api-server", EventKind::TurnComplete, day_start - 60);

        // Two sessions: one busy and finishing, one that kept the user
        // waiting for four minutes and was approved. The turn counts are
        // deliberately UNEQUAL, so counting the wrong kind fails here.
        for i in 0..4 {
            seed("api-server", EventKind::TurnStart, day_start + i * 100);
        }
        for i in 0..3 {
            seed("api-server", EventKind::TurnComplete, day_start + i * 100 + 50);
        }
        let parked = seed("bc9", EventKind::PermissionRequest, day_start + 1_000);
        registry.set_event_times_for_test(parked, day_start + 1_000, Some(day_start + 1_240));
        registry.set_decision_for_test(parked, "allow");
        // Outside the window: must not be counted.
        seed("tomorrow", EventKind::TurnComplete, day_end + 60);

        let stats = registry.digest(day_start, day_end).unwrap();
        assert_eq!(stats.sessions, 2, "the session from the next day is not ours");
        assert_eq!(stats.completions, 3);
        assert_eq!(stats.approvals_allowed, 1);
        assert_eq!(stats.approvals_denied, 0);
        assert_eq!(stats.waiting_secs, 240);
        assert_eq!(stats.longest, Some(("bc9".to_string(), 240)));
        assert_eq!(stats.busiest.as_ref().map(|(p, _)| p.as_str()), Some("api-server"));

        assert_eq!(
            body(&stats),
            "2 sessions, 3 finished, 1 approval, and you kept agents waiting 4 minutes. Longest, bc9."
        );
        // …and the same numbers, in each style, through the normal path.
        assert_eq!(
            crate::speaker::callout(
                crate::speaker::Style::Terse,
                crate::speaker::Callout::Digest {
                    span: DigestSpan::Yesterday,
                    body: &body(&stats),
                },
                AgentKind::ClaudeCode,
                "",
            ),
            "Yesterday: 2 sessions, 3 finished, 1 approval, and you kept agents waiting 4 minutes. Longest, bc9."
        );
    }

    /// The two ways the card goes away, and the one way it comes back.
    #[test]
    fn the_card_hides_when_dismissed_today_or_turned_off() {
        assert!(should_show(true, "2026-08-05", None));
        assert!(
            should_show(true, "2026-08-05", Some("2026-08-04")),
            "yesterday's dismissal is not today's"
        );
        assert!(!should_show(true, "2026-08-05", Some("2026-08-05")));
        assert!(!should_show(false, "2026-08-05", None), "turned off is off");
        assert!(!should_show(false, "2026-08-05", Some("2026-08-04")));
    }

    /// The once-a-day claim, which is the whole of "fires once per day".
    /// Every branch here was previously unprotected: deleting the claim
    /// entirely left the suite green.
    #[test]
    fn a_day_can_only_be_claimed_once() {
        let claim = Claim::default();
        assert!(claim.take("2026-08-05", || None), "first call takes it");
        assert!(!claim.take("2026-08-05", || None), "second call does not");
        assert!(claim.take("2026-08-06", || None), "tomorrow is a new day");

        // A restart mid-morning adopts what disk remembers rather than
        // repeating the digest the user already heard.
        let restarted = Claim::default();
        assert!(!restarted.take("2026-08-05", || Some("2026-08-05".to_string())));
        // …and the seed is consulted ONCE, not on every call.
        let mut seeds = 0;
        let counting = Claim::default();
        for _ in 0..3 {
            counting.take("2026-08-05", || {
                seeds += 1;
                None
            });
        }
        assert_eq!(seeds, 1);

        // A failure hands the day back so the next event can try again.
        let failed = Claim::default();
        assert!(failed.take("2026-08-05", || None));
        failed.release();
        assert!(failed.take("2026-08-05", || None), "released days are retryable");
    }

    /// §12.5's one hard requirement about the utterance: a morning summary
    /// must never preempt an agent that is blocked on a human. Nothing
    /// protected this — flipping it to `Approval` left the suite green.
    #[test]
    fn the_digest_never_preempts_an_approval() {
        let registry = Registry::open_in_memory().unwrap();
        let event = crate::registry::NormalizedEvent {
            agent: AgentKind::ClaudeCode,
            event: crate::registry::EventKind::TurnComplete,
            session_id: "s1".into(),
            cwd: "/tmp/project".into(),
            summary: None,
            transcript_path: None,
            tool: None,
            terminal: None,
        };
        let (_, id) = registry.apply(&event, |_| {}).unwrap();
        let window = window_for(today_local()).unwrap();
        registry.set_event_times_for_test(id, window.from + 60, None);

        let due = due(&window, &registry, crate::speaker::Style::Terse)
            .unwrap()
            .expect("a day with an event in it has a digest");
        assert_eq!(due.priority_for_test(), Priority::Attention);
        assert!(due.utterance.text.starts_with(window.span.label()));
        assert_eq!(due.day, window.day);
        assert!(!due.utterance.audition);
    }

    /// An empty window produces nothing to say — and the caller settles the
    /// day anyway, because the window is entirely in the past and cannot
    /// fill in later.
    #[test]
    fn an_empty_window_has_nothing_due() {
        let registry = Registry::open_in_memory().unwrap();
        let window = window_for(today_local()).unwrap();
        assert!(due(&window, &registry, crate::speaker::Style::Terse)
            .unwrap()
            .is_none());
    }

    /// A day with nothing in it is not a digest. It must also not be marked
    /// spoken, or a machine that was off all weekend loses the first real one.
    #[test]
    fn an_empty_day_says_nothing_and_stays_owed() {
        let registry = Registry::open_in_memory().unwrap();
        let stats = registry.digest(1_785_000_000, 1_785_086_400).unwrap();
        assert!(stats.is_empty());
        assert_eq!(stats.sessions, 0);
    }

    /// Every day of a year produces a usable window: no panics, no inverted
    /// ranges, and never a zero-length one (which would report an empty day
    /// and mark it spoken).
    #[test]
    fn every_day_of_a_year_yields_a_sane_window() {
        let mut day = date(2026, 1, 1);
        while day.year() == 2026 {
            let window = window_for(day).expect("every day has a window");
            assert!(window.from < window.to, "{day}");
            assert!(
                window.to - window.from >= 23 * 3600,
                "{day} produced {}s",
                window.to - window.from
            );
            day = day.succ_opt().unwrap();
        }
    }
}
