//! The weekly dependency review (the "Monday review").
//!
//! Once a week, on a configured day, sweep the user's repo roots for
//! Bun-managed projects, run `bun outdated` in each, and report the fleet's
//! staleness the same way the morning digest reports the day: one spoken
//! sentence and a board card. "Weekly deps review: 38 repos, 12 behind, 3
//! with major updates."
//!
//! OFF BY DEFAULT, and the reason is an invariant: this app promises no
//! outbound network. The sweep itself opens no sockets — but the `bun
//! outdated` child processes it spawns read the npm registry for the repos
//! being checked. That is the user's own toolchain talking to the registry
//! it already installs from, never this app phoning anywhere, and it only
//! happens after the user turns the feature on (ARCHITECTURE.md §9.3a).
//!
//! Shape mirrors digest.rs deliberately: pure window/dueness functions over
//! a clock, a claim against double-running, Priority::Attention so a summary
//! never preempts an agent blocked on a human, and a card that outlives the
//! utterance.

use chrono::{Datelike, NaiveDate, Weekday};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::registry::AgentKind;
use crate::speaker::{Priority, SpeakerHandle, Utterance};

/// The ISO week a run settles, e.g. `2026-W35`. One run per week, keyed on
/// this — a laptop that was closed on Monday still gets its review on
/// Tuesday, and a run on Tuesday does not repeat on Wednesday.
pub fn week_key(today: NaiveDate) -> String {
    let iso = today.iso_week();
    format!("{}-W{:02}", iso.year(), iso.week())
}

/// Whether a review is owed on `today`: the configured day has arrived (or
/// passed — the machine may have been off) and this ISO week has not run.
pub fn due_this_week(today: NaiveDate, review_day: Weekday, last_run_week: Option<&str>) -> bool {
    let arrived = today.weekday().num_days_from_monday() >= review_day.num_days_from_monday();
    arrived && last_run_week != Some(week_key(today).as_str())
}

/// One outdated dependency in one repo, as parsed from `bun outdated`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Outdated {
    pub name: String,
    pub current: String,
    pub latest: String,
    /// Latest is a different major — the update that needs a human.
    pub major: bool,
}

/// One swept repo.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RepoReport {
    pub name: String,
    pub path: String,
    pub outdated: Vec<Outdated>,
    /// `bun outdated` failed or timed out; the repo counts as unswept, not
    /// as current — a review that hides its failures is worse than none.
    pub error: Option<String>,
}

/// A whole week's sweep, persisted as JSON so the card survives a restart.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Review {
    pub week: String,
    pub ran_at: i64,
    pub repos: Vec<RepoReport>,
}

impl Review {
    pub fn scanned(&self) -> usize {
        self.repos.len()
    }
    pub fn behind(&self) -> Vec<&RepoReport> {
        self.repos.iter().filter(|r| !r.outdated.is_empty()).collect()
    }
    pub fn majors(&self) -> usize {
        self.repos
            .iter()
            .flat_map(|r| &r.outdated)
            .filter(|o| o.major)
            .count()
    }
    pub fn errors(&self) -> usize {
        self.repos.iter().filter(|r| r.error.is_some()).count()
    }
}

/// Find Bun repos under the configured roots: a directory holding both a
/// package.json and a bun lockfile. Nested finds are workspaces of a repo
/// already found and are skipped — `bun outdated` at the root covers them.
pub fn find_repos(roots: &[PathBuf]) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        "node_modules", ".git", "dist", "build", "target", ".astro", ".claude",
        ".wrangler", "coverage", ".venv", "venv",
    ];
    fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let is_repo = dir.join("package.json").exists()
            && (dir.join("bun.lock").exists() || dir.join("bun.lockb").exists());
        if is_repo {
            out.push(dir.to_path_buf());
            return; // workspaces below are covered from here
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| !n.starts_with('.') && !SKIP.contains(&n))
                    .unwrap_or(false)
            })
            .collect();
        children.sort();
        for child in children {
            walk(&child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for root in roots {
        walk(root, 0, &mut out);
    }
    out
}

/// Parse `bun outdated` table output. The table is box-drawn pipes:
/// `| satori (dev)     | 0.19.3  | 0.19.3 | 0.33.3 |` — name (with an
/// optional `(dev)`/`(peer)` marker), current, update-within-range, latest.
/// No rows (or the all-current message) parses to an empty list.
pub fn parse_outdated(stdout: &str) -> Vec<Outdated> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with('|') {
                return None;
            }
            let cells: Vec<&str> = line
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect();
            if cells.len() != 4 {
                return None;
            }
            let name = cells[0].split_whitespace().next()?.to_string();
            if name == "Package" || name.is_empty() {
                return None; // header row
            }
            if cells.iter().any(|c| !c.is_empty() && c.chars().all(|ch| ch == '-')) {
                return None; // divider row
            }
            let (current, latest) = (cells[1].to_string(), cells[3].to_string());
            if current.is_empty() || latest.is_empty() || current == latest {
                return None;
            }
            let major = major_of(&current) != major_of(&latest);
            Some(Outdated { name, current, latest, major })
        })
        .collect()
}

fn major_of(version: &str) -> Option<&str> {
    version.split('.').next()
}

/// Run `bun outdated` in one repo. A timeout or spawn failure is an error on
/// the report, never a panic and never "current".
fn sweep_one(repo: &Path) -> RepoReport {
    let name = repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    let report = |outdated, error| RepoReport {
        name: name.clone(),
        path: repo.display().to_string(),
        outdated,
        error,
    };
    match Command::new("bun").arg("outdated").current_dir(repo).output() {
        Ok(out) if out.status.success() => {
            report(parse_outdated(&String::from_utf8_lossy(&out.stdout)), None)
        }
        Ok(out) => report(
            Vec::new(),
            Some(format!(
                "bun outdated exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>()
            )),
        ),
        Err(err) => report(Vec::new(), Some(format!("could not run bun: {err}"))),
    }
}

/// The whole sweep. Sequential on purpose: this runs behind the user's back
/// on a Monday morning, and forty concurrent `bun` processes hammering the
/// registry is not a background task, it is a fan spinning up.
pub fn sweep(roots: &[PathBuf], week: String) -> Review {
    let repos = find_repos(roots);
    log::info!("deps review: sweeping {} repos", repos.len());
    Review {
        week,
        ran_at: chrono::Utc::now().timestamp(),
        repos: repos.iter().map(|r| sweep_one(r)).collect(),
    }
}

/// The sentence body — digest's rule applies: only what happened. All
/// current is one short clause, not a recitation of zeros.
pub fn body(review: &Review) -> String {
    let scanned = review.scanned();
    let behind = review.behind();
    if scanned == 0 {
        return "No Bun repos found under the configured roots.".to_string();
    }
    if behind.is_empty() && review.errors() == 0 {
        return format!(
            "All {scanned} {} current.",
            if scanned == 1 { "repo" } else { "repos" }
        );
    }
    let mut text = format!(
        "{scanned} {} scanned, {} behind",
        if scanned == 1 { "repo" } else { "repos" },
        behind.len()
    );
    let majors = review.majors();
    if majors > 0 {
        text.push_str(&format!(
            ", {majors} major {}",
            if majors == 1 { "update" } else { "updates" }
        ));
    }
    text.push('.');
    // The most-behind repo is the actionable half of the number, exactly as
    // the digest names the longest wait.
    if let Some(worst) = behind.iter().max_by_key(|r| r.outdated.len()) {
        if worst.outdated.len() > 1 {
            text.push_str(&format!(
                " Most behind, {} with {}.",
                worst.name,
                worst.outdated.len()
            ));
        }
    }
    if review.errors() > 0 {
        text.push_str(&format!(" {} could not be checked.", review.errors()));
    }
    text
}

/// Run the review if it is owed, speak the summary, persist the result.
/// Blocking (child processes + disk); call from a blocking task. Answers
/// whether a review ran.
pub fn run_if_due(claim: &crate::digest::Claim, speaker: &SpeakerHandle) -> bool {
    if !crate::config::deps_review_enabled() {
        return false;
    }
    let today = crate::digest::today_local();
    let week = week_key(today);
    if !due_this_week(today, crate::config::deps_review_day(), crate::config::deps_review_last_run_week().as_deref()) {
        return false;
    }
    // One runner per week across the launch catch-up and the periodic timer,
    // same discipline as the digest's day claim.
    if !claim.take(&week, crate::config::deps_review_last_run_week) {
        return false;
    }

    let roots = crate::config::deps_review_roots();
    let review = sweep(&roots, week.clone());

    // Persist BEFORE speaking, exactly as the digest does: a muted speaker
    // is not a reason to sweep again tomorrow.
    match serde_json::to_string(&review) {
        Ok(json) => {
            crate::config::set_deps_review_last_result(&json);
            crate::config::set_deps_review_last_run_week(&week);
        }
        Err(err) => {
            log::warn!("deps review: could not serialise the result: {err}");
            claim.release();
            return false;
        }
    }

    let text = crate::speaker::callout(
        crate::config::speech_style(),
        crate::speaker::Callout::DepsReview { body: &body(&review) },
        AgentKind::ClaudeCode,
        "",
    );
    speaker.enqueue(Utterance {
        // Attention, NEVER Approval — a weekly summary must not preempt an
        // agent blocked on a human.
        priority: Priority::Attention,
        session_id: format!("deps-review-{week}"),
        agent: AgentKind::ClaudeCode,
        text,
        voice_override: None,
        audition: false,
        terminal_pid: None,
    });
    true
}

/// The board card, or None — disabled, dismissed this week, or never run.
pub fn card() -> Option<Card> {
    if !crate::config::deps_review_enabled() {
        return None;
    }
    let json = crate::config::deps_review_last_result()?;
    let review: Review = serde_json::from_str(&json).ok()?;
    let this_week = week_key(crate::digest::today_local());
    // Only this week's sweep is news; a stale result is not a card.
    if review.week != this_week {
        return None;
    }
    if crate::config::deps_review_dismissed_week().as_deref() == Some(this_week.as_str()) {
        return None;
    }
    let mut worst: Vec<(String, usize)> = review
        .behind()
        .iter()
        .map(|r| (r.name.clone(), r.outdated.len()))
        .collect();
    worst.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    worst.truncate(5);
    Some(Card {
        lead: "Deps review",
        body: body(&review),
        worst,
    })
}

/// What the board draws. Rust renders the numbers (§project rule).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Card {
    pub lead: &'static str,
    pub body: String,
    /// Up to five (repo, outdated-count) rows, most behind first.
    pub worst: Vec<(String, usize)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // 2026-08-24 is a Monday.
    #[test]
    fn due_on_the_configured_day() {
        assert!(due_this_week(date(2026, 8, 24), Weekday::Mon, None));
    }

    #[test]
    fn due_later_in_the_week_when_the_machine_was_off() {
        assert!(due_this_week(date(2026, 8, 26), Weekday::Mon, Some("2026-W34")));
    }

    #[test]
    fn not_due_before_the_configured_day() {
        // Monday the 24th, review configured for Wednesday: not owed yet.
        assert!(!due_this_week(date(2026, 8, 24), Weekday::Wed, None));
    }

    #[test]
    fn first_enable_late_in_the_week_catches_up() {
        // Sunday the 23rd is AFTER Monday in ISO week 34 — a never-run
        // Monday review is owed as a catch-up, same philosophy as the
        // digest's launch catch-up.
        assert!(due_this_week(date(2026, 8, 23), Weekday::Mon, None));
    }

    #[test]
    fn not_due_twice_in_one_week() {
        assert!(!due_this_week(date(2026, 8, 26), Weekday::Mon, Some("2026-W35")));
    }

    #[test]
    fn week_keys_are_iso() {
        assert_eq!(week_key(date(2026, 8, 24)), "2026-W35");
        // ISO year differs from the calendar year at the boundary.
        assert_eq!(week_key(date(2027, 1, 1)), "2026-W53");
    }

    #[test]
    fn parses_real_bun_outdated_output() {
        let out = "bun outdated v1.3.14 (0d9b296a)\n\
| Package          | Current | Update | Latest |\n\
|------------------|---------|--------|--------|\n\
| satori (dev)     | 0.19.3  | 0.19.3 | 0.33.3 |\n\
|------------------|---------|--------|--------|\n\
| sharp (dev)      | 0.34.5  | 0.34.5 | 0.35.3 |\n\
|------------------|---------|--------|--------|\n\
| typescript (dev) | 5.9.3   | 5.9.3  | 7.0.2  |\n\
|----------------------------------------------|\n";
        let parsed = parse_outdated(out);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name, "satori");
        assert_eq!(parsed[0].current, "0.19.3");
        assert_eq!(parsed[0].latest, "0.33.3");
        assert_eq!(parsed[2].name, "typescript");
        assert!(parsed[2].major);
    }

    #[test]
    fn zero_x_series_counts_majors_on_the_leading_digit() {
        // 0.19 -> 0.33 has the same leading digit; not flagged as major.
        let parsed = parse_outdated(
            "| a | 0.19.3 | 0.19.3 | 0.33.3 |\n| b | 5.9.3 | 5.9.3 | 7.0.2 |\n",
        );
        assert!(!parsed[0].major);
        assert!(parsed[1].major);
    }

    #[test]
    fn up_to_date_output_parses_empty() {
        assert!(parse_outdated("").is_empty());
        assert!(parse_outdated("All dependencies are up to date!\n").is_empty());
    }

    #[test]
    fn body_all_current() {
        let review = Review {
            week: "2026-W35".into(),
            ran_at: 0,
            repos: vec![RepoReport {
                name: "a".into(),
                path: "/a".into(),
                outdated: vec![],
                error: None,
            }],
        };
        assert_eq!(body(&review), "All 1 repo current.");
    }

    #[test]
    fn body_names_the_most_behind_repo_and_counts_majors() {
        let od = |major| Outdated {
            name: "x".into(),
            current: "1.0.0".into(),
            latest: if major { "2.0.0" } else { "1.1.0" }.into(),
            major,
        };
        let review = Review {
            week: "2026-W35".into(),
            ran_at: 0,
            repos: vec![
                RepoReport { name: "virixia".into(), path: "/v".into(), outdated: vec![od(true), od(false), od(false)], error: None },
                RepoReport { name: "bryy".into(), path: "/b".into(), outdated: vec![od(false)], error: None },
                RepoReport { name: "ok".into(), path: "/o".into(), outdated: vec![], error: None },
            ],
        };
        let text = body(&review);
        assert!(text.contains("3 repos scanned, 2 behind"), "{text}");
        assert!(text.contains("1 major update"), "{text}");
        assert!(text.contains("Most behind, virixia with 3."), "{text}");
    }

    #[test]
    fn body_surfaces_errors() {
        let review = Review {
            week: "2026-W35".into(),
            ran_at: 0,
            repos: vec![RepoReport { name: "a".into(), path: "/a".into(), outdated: vec![], error: Some("boom".into()) }],
        };
        assert!(body(&review).contains("1 could not be checked."));
    }
}
