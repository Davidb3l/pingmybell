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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long one repo's `outdated` call may take before it is abandoned and
/// recorded as an error. Generous — a cold registry round-trip over a slow
/// link is normal — but finite, because the alternative is what this bounds:
/// one hung child parking the 4-hourly loop in `lib.rs` for the life of the
/// process, which permanently stops the weekly review.
const REPO_TIMEOUT: Duration = Duration::from_secs(60);

/// The whole sweep's budget. `find_repos` has no cap on how many repos it
/// returns, so a root pointed at a large tree could otherwise run for
/// hours. Repos not reached before this are recorded as unswept rather than
/// silently dropped.
const SWEEP_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Cap on captured stdout per repo. `bun outdated`'s table for a very large
/// workspace is still tens of kilobytes; a megabyte means something has gone
/// wrong, and buffering it unbounded is how a background task eats memory.
const MAX_OUTPUT: usize = 1 << 20;

/// Places a GUI-launched app has to look for `bun` and `pnpm`. An app bundle
/// started from Finder/LaunchServices inherits a bare `PATH`
/// (`/usr/bin:/bin:/usr/sbin:/sbin`) that contains neither Homebrew nor
/// `~/.bun/bin`, so resolving by `PATH` alone reported every repo as
/// uncheckable for exactly the users who launch the app normally. Same
/// hazard, and the same remedy, as `tmux::FALLBACK_BINS`.
fn fallback_bins(manager: Manager) -> Vec<PathBuf> {
    let name = manager.bin();
    let mut out = Vec::new();
    if manager == Manager::Bun {
        if let Some(home) = dirs::home_dir() {
            out.push(home.join(".bun").join("bin").join(name));
        }
    }
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".local").join("share").join("pnpm").join(name));
        out.push(home.join(".volta").join("bin").join(name));
    }
    out.extend(
        ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin", "/usr/bin"]
            .iter()
            .map(|d| Path::new(d).join(name)),
    );
    out
}

/// Resolve a package manager's binary: `PATH` first, then the fallbacks.
/// `None` means it is genuinely not installed, which the caller reports once
/// for the whole sweep instead of once per repo.
fn manager_bin(manager: Manager) -> Option<PathBuf> {
    let name = manager.bin();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    fallback_bins(manager).into_iter().find(|c| is_executable(c))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// What one bounded `outdated` invocation produced.
#[derive(Debug)]
struct Captured {
    stdout: String,
    stderr: String,
    status: Option<std::process::ExitStatus>,
}

/// Run `bin args…` in `cwd` with stdin closed, giving up after `timeout`.
///
/// Mirrors `tmux::run`: the read happens on a helper thread so a wedged
/// child cannot pin the caller, and the child is killed and reaped on
/// expiry. `Err` is a message fit for `RepoReport::error`; a timeout is an
/// error, never a result, because reporting a hung check as "current" is
/// the one thing this feature must not do.
fn run_bounded(bin: &Path, args: &[&str], cwd: &Path, timeout: Duration) -> Result<Captured, String> {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", bin.display()))?;

    let (Some(mut out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
        reap(&mut child);
        return Err("could not capture the output of the outdated check".to_string());
    };

    // Both pipes are drained concurrently. Reading one to the end first can
    // deadlock: a child that fills the other pipe's buffer blocks forever
    // while we wait on the one it has stopped writing to.
    let (tx, rx) = mpsc::channel();
    let tx_err = tx.clone();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let r = out.by_ref().take(MAX_OUTPUT as u64).read_to_end(&mut buf);
        let _ = tx.send(("stdout", r.map(|_| buf)));
    });
    thread::spawn(move || {
        let mut buf = Vec::new();
        let r = err.by_ref().take(MAX_OUTPUT as u64).read_to_end(&mut buf);
        let _ = tx_err.send(("stderr", r.map(|_| buf)));
    });

    let deadline = Instant::now() + timeout;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((which, Ok(buf))) => {
                if which == "stdout" {
                    stdout = buf;
                } else {
                    stderr = buf;
                }
            }
            Ok((which, Err(e))) => {
                log::debug!("deps review: reading {which} failed: {e}");
            }
            Err(_) => {
                reap(&mut child);
                return Err(format!("outdated check timed out after {timeout:?}"));
            }
        }
    }

    let status = wait_bounded(&mut child, deadline);
    Ok(Captured {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        status,
    })
}

/// Reap the child by `deadline`. A process that closes its pipes without
/// exiting must not pin the sweep, so the wait is polled. Normally returns
/// on the first iteration.
fn wait_bounded(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    reap(child);
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                log::debug!("deps review: waiting on the outdated check failed: {err}");
                return None;
            }
        }
    }
}

/// Kill the direct child only. `bun`/`pnpm` may have spawned their own
/// children, which can outlive this — bounded and leaked is still strictly
/// better than unbounded and blocking, which is what this replaces.
fn reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

use crate::registry::AgentKind;
use crate::speaker::{Priority, SpeakerHandle, Utterance};

/// The ISO week a run settles, e.g. `2026-W35`. One run per week, keyed on
/// this — a laptop that was closed on Monday still gets its review on
/// Tuesday, and a run on Tuesday does not repeat on Wednesday.
pub fn week_key(today: NaiveDate) -> String {
    let iso = today.iso_week();
    format!("{}-W{:02}", iso.year(), iso.week())
}

/// Whether a review is owed on `today`: this ISO week has not run, and
/// either the configured day has arrived (or passed — the machine may have
/// been off) or a previous week went entirely unreviewed.
///
/// That second clause is why this is not just a weekday comparison. With
/// `day = "fri"`, a machine that is off from Friday evening to Monday would
/// otherwise never be owed a review ever again: Monday is before Friday in
/// the week, and by then the ISO week has rolled over, so the missed week is
/// unreachable. Falling through on the first run of a week that follows an
/// unreviewed one keeps "catch up if the machine was off" true for every
/// configured day, not just the Monday default.
pub fn due_this_week(today: NaiveDate, review_day: Weekday, last_run_week: Option<&str>) -> bool {
    let this_week = week_key(today);
    if last_run_week == Some(this_week.as_str()) {
        return false;
    }
    let arrived = today.weekday().num_days_from_monday() >= review_day.num_days_from_monday();
    // A review has never run at all: wait for the configured day rather than
    // firing the moment the feature is switched on mid-week.
    let Some(last) = last_run_week else {
        return arrived;
    };
    arrived || last != previous_week_key(today)
}

/// The ISO week before the one containing `today`.
fn previous_week_key(today: NaiveDate) -> String {
    week_key(today - chrono::Duration::days(7))
}

/// Which package manager a repo is driven by — decides the sweep command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Manager {
    Bun,
    Pnpm,
}

impl Manager {
    fn bin(self) -> &'static str {
        match self {
            Manager::Bun => "bun",
            Manager::Pnpm => "pnpm",
        }
    }
}

/// A discovered repo and how to ask it about its dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub path: PathBuf,
    pub manager: Manager,
}

/// One outdated dependency in one repo, as parsed from `bun outdated`
/// or `pnpm outdated --format json`.
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

/// Find Bun and pnpm repos under the configured roots: a directory holding
/// a package.json plus that manager's lockfile. Nested finds are workspaces
/// of a repo already found and are skipped — the outdated command at the
/// root covers them. A repo with both lockfiles counts as Bun: in this
/// fleet a stray pnpm-lock.yaml beside bun.lock is drift, not intent.
pub fn find_repos(roots: &[PathBuf]) -> Vec<Repo> {
    const SKIP: &[&str] = &[
        "node_modules", ".git", "dist", "build", "target", ".astro", ".claude",
        ".wrangler", "coverage", ".venv", "venv",
    ];
    fn walk(dir: &Path, depth: u8, out: &mut Vec<Repo>) {
        if depth > 4 {
            return;
        }
        if dir.join("package.json").exists() {
            let manager = if dir.join("bun.lock").exists() || dir.join("bun.lockb").exists() {
                Some(Manager::Bun)
            } else if dir.join("pnpm-lock.yaml").exists() {
                Some(Manager::Pnpm)
            } else {
                None
            };
            if let Some(manager) = manager {
                out.push(Repo { path: dir.to_path_buf(), manager });
                return; // workspaces below are covered from here
            }
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

/// Parse `pnpm outdated --format json`: an object keyed by package name.
/// Deprecated-but-current entries (current == latest, isDeprecated true)
/// are NOT outdated and are skipped — deprecation is a different problem
/// than staleness, and counting it here would cry wolf weekly.
pub fn parse_pnpm_outdated(stdout: &str) -> Vec<Outdated> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, entry)| {
            let current = entry.get("current")?.as_str()?.to_string();
            let latest = entry.get("latest")?.as_str()?.to_string();
            if current == latest || current.is_empty() || latest.is_empty() {
                return None;
            }
            let major = major_of(&current) != major_of(&latest);
            Some(Outdated { name: name.clone(), current, latest, major })
        })
        .collect()
}

/// Run the manager's outdated command in one repo. A spawn failure is an
/// The repo's display name: its directory name, which is what the sentence
/// and the card say. Shared with the sweep's budget path so an unswept repo
/// is named the same way a swept one is.
fn repo_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string()
}

/// error on the report, never a panic and never "current".
fn sweep_one(repo: &Repo) -> RepoReport {
    let path = repo.path.as_path();
    let name = repo_name(path);
    let report = |outdated, error| RepoReport {
        name: name.clone(),
        path: path.display().to_string(),
        outdated,
        error,
    };
    let Some(bin) = manager_bin(repo.manager) else {
        return report(
            Vec::new(),
            Some(format!("{} is not installed, or not on this app's PATH", repo.manager.bin())),
        );
    };
    let workspace = repo.manager == Manager::Pnpm && path.join("pnpm-workspace.yaml").exists();
    let args: &[&str] = match repo.manager {
        Manager::Bun => &["outdated"],
        // Workspace repos keep their deps in member packages; the root
        // query answers `{}` (verified empirically). `-r` aggregates.
        Manager::Pnpm if workspace => &["-r", "outdated", "--format", "json"],
        Manager::Pnpm => &["outdated", "--format", "json"],
    };
    match run_bounded(&bin, args, path, REPO_TIMEOUT) {
        Ok(out) => match judge(repo.manager, out.status.map(|s| s.success()), &out.stdout, &out.stderr) {
            Verdict::Result(outdated) => report(outdated, None),
            Verdict::Failed(why) => report(Vec::new(), Some(why)),
        },
        Err(err) => report(Vec::new(), Some(err)),
    }
}

/// What one finished `outdated` run amounts to.
enum Verdict {
    /// A genuine answer, possibly "nothing is behind".
    Result(Vec<Outdated>),
    /// The check could not be made. Counts as unswept, NEVER as current.
    Failed(String),
}

/// Judge a finished run, as a pure function of what it produced.
///
/// Extracted from [`sweep_one`] so the rule that matters most here — a
/// failure is never reported as "current" — can be tested without spawning
/// anything. `succeeded` is `None` when the child never exited and was
/// killed.
///
/// pnpm exits non-zero when outdated deps exist, and both managers may have
/// produced parseable stdout anyway, so: parse first, judge after. A failure
/// with usable output is a result; a failure without it is an error.
fn judge(manager: Manager, succeeded: Option<bool>, stdout: &str, stderr: &str) -> Verdict {
    let outdated = match manager {
        Manager::Bun => parse_outdated(stdout),
        Manager::Pnpm => parse_pnpm_outdated(stdout),
    };
    if succeeded == Some(true) || !outdated.is_empty() || looks_like_result(manager, stdout) {
        return Verdict::Result(outdated);
    }
    let detail: String = stderr.trim().chars().take(200).collect();
    let detail = if detail.is_empty() {
        stdout.trim().chars().take(200).collect::<String>()
    } else {
        detail
    };
    Verdict::Failed(match succeeded {
        Some(_) => format!("outdated check failed: {detail}"),
        None => "outdated check never exited and was killed".to_string(),
    })
}

/// Whether stdout is a genuine (possibly empty) result rather than noise —
/// guards the exit-code judgement above for the all-current case.
fn looks_like_result(manager: Manager, stdout: &str) -> bool {
    match manager {
        // `bun outdated` exits 0 whether or not anything is behind, so a
        // NON-ZERO exit means bun itself failed: a stale lockfile, a missing
        // node_modules, a registry that would not answer. This arm is only
        // ever consulted after such a failure, and it used to answer `true`
        // unconditionally — which made the error branch below unreachable
        // for Bun and reported every failing repo as CURRENT. A whole fleet
        // of failures came back as "All 38 repos current." Nothing here is
        // worth rescuing: if bun failed and parsed no rows, it is an error.
        Manager::Bun => false,
        Manager::Pnpm => serde_json::from_str::<serde_json::Value>(stdout)
            .map(|v| v.is_object())
            .unwrap_or(false),
    }
}

/// Where the sweep result lives: its OWN file beside the config, never
/// inside it.
///
/// `config.json` is re-read and re-parsed by the shim on every hook
/// invocation, twice, against NFR-2's 10 ms shim-startup budget. A realistic
/// fleet (dozens of repos, a dozen outdated deps each) serialises to tens of
/// kilobytes, so keeping the review in there made the agent's hot path pay
/// for it on every tool call and made every unrelated settings write rewrite
/// and fsync the whole thing. The review is derived data that the shim never
/// looks at, so it belongs next to the config rather than in it.
fn result_path() -> Option<PathBuf> {
    crate::ingest::data_dir().ok().map(|dir| dir.join("deps-review.json"))
}

/// The last sweep, if one is stored and still parses. A corrupt or
/// half-written file reads as "no review", which is the same as never having
/// run — never a panic, and never a crash on a file a user hand-edited.
pub fn stored_result() -> Option<Review> {
    let raw = std::fs::read_to_string(result_path()?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the sweep. Written atomically and user-only, the same discipline
/// as the config, because it carries the names of every repo in the fleet.
fn store_result(review: &Review) -> bool {
    let Some(path) = result_path() else {
        log::warn!("deps review: no data directory; the result was not stored");
        return false;
    };
    let json = match serde_json::to_string(review) {
        Ok(json) => json,
        Err(err) => {
            log::warn!("deps review: could not serialise the result: {err}");
            return false;
        }
    };
    match crate::config::write_private_atomic(&path, &json) {
        Ok(()) => {
            // An install upgraded from the build that kept this in the config
            // stops paying the shim's hot path for it as soon as one sweep runs.
            crate::config::prune_deps_review_last_result();
            true
        }
        Err(err) => {
            log::warn!("deps review: could not write the result: {err}");
            false
        }
    }
}

/// The whole sweep. Sequential on purpose: this runs behind the user's back
/// on a Monday morning, and forty concurrent `bun` processes hammering the
/// registry is not a background task, it is a fan spinning up.
pub fn sweep(roots: &[PathBuf], week: String) -> Review {
    let repos = find_repos(roots);
    log::info!("deps review: sweeping {} repos", repos.len());
    let deadline = Instant::now() + SWEEP_TIMEOUT;
    let mut reports = Vec::with_capacity(repos.len());
    for repo in &repos {
        if Instant::now() >= deadline {
            // Recorded, not dropped: `RepoReport::error` means "could not be
            // checked", and a repo we never reached is exactly that. Silently
            // shortening the sweep would shrink `scanned()` and make a
            // truncated review look like a smaller, healthier fleet.
            log::warn!(
                "deps review: sweep budget of {SWEEP_TIMEOUT:?} spent; {} repos unswept",
                repos.len() - reports.len()
            );
            reports.extend(repos[reports.len()..].iter().map(|r| RepoReport {
                name: repo_name(&r.path),
                path: r.path.display().to_string(),
                outdated: Vec::new(),
                error: Some("not reached before the sweep's time budget ran out".to_string()),
            }));
            break;
        }
        reports.push(sweep_one(repo));
    }
    Review {
        week,
        ran_at: chrono::Utc::now().timestamp(),
        repos: reports,
    }
}

/// The sentence body — digest's rule applies: only what happened. All
/// current is one short clause, not a recitation of zeros.
pub fn body(review: &Review) -> String {
    let scanned = review.scanned();
    let behind = review.behind();
    if scanned == 0 {
        return "No Bun or pnpm repos found under the configured roots.".to_string();
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

/// At most one sweep at a time, across every path that can start one.
///
/// Distinct from the week-claim on purpose: the claim answers "has this week
/// been settled", which is a fact about the calendar and survives restarts,
/// while this answers "is a sweep running right now", which is a fact about
/// this process. The scheduled run and the board's "Run now" can be owed and
/// un-run simultaneously, so the first question alone does not keep them
/// from both sweeping at once.
#[derive(Default)]
pub struct SweepGate(std::sync::Mutex<bool>);

/// Releases the gate on every exit path, including an early `return` or a
/// panic inside the sweep.
pub struct SweepGuard<'a>(&'a SweepGate);

impl SweepGate {
    /// Claim the right to sweep, or `None` if one is already under way.
    /// Never blocks: a UI command must not sit behind a sweep that can take
    /// minutes, and the caller has something better to do with a `None`
    /// (stand down, or return the existing card) than wait.
    pub fn enter(&self) -> Option<SweepGuard<'_>> {
        let mut busy = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if *busy {
            return None;
        }
        *busy = true;
        Some(SweepGuard(self))
    }
}

impl Drop for SweepGuard<'_> {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap_or_else(|p| p.into_inner()) = false;
    }
}

/// Run the review if it is owed, speak the summary, persist the result.
/// Blocking (child processes + disk); call from a blocking task. Answers
/// whether a review ran.
pub fn run_if_due(gate: &SweepGate, claim: &crate::digest::Claim, speaker: &SpeakerHandle) -> bool {
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
    // And one SWEEP at a time across every path that can start one. The week
    // claim alone does not give this: the board's "Run now" is a different
    // trigger and can land mid-sweep.
    let Some(_sweeping) = gate.enter() else {
        log::info!("deps review: a sweep is already running; the scheduled run stands down");
        claim.release();
        return false;
    };

    let roots = crate::config::deps_review_roots();
    let review = sweep(&roots, week.clone());

    // Persist BEFORE speaking, exactly as the digest does: a muted speaker
    // is not a reason to sweep again tomorrow.
    if !store_result(&review) {
        claim.release();
        return false;
    }
    crate::config::set_deps_review_last_run_week(&week);
    speak_summary(speaker, &review, &week);
    true
}

/// Say the week's summary. Shared by the scheduled run and the board's
/// "Run now" so the user hears the review exactly once however it was
/// started.
fn speak_summary(speaker: &SpeakerHandle, review: &Review, week: &str) {
    let text = crate::speaker::callout(
        crate::config::speech_style(),
        crate::speaker::Callout::DepsReview { body: &body(review) },
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
}

/// The board's "Run now": sweep regardless of the schedule, and return the
/// refreshed card.
///
/// Goes through the same gate and the same week-claim as the scheduled run,
/// which it previously bypassed entirely. Two bugs came out of that bypass:
///
/// 1. Clicking it while the launch catch-up was mid-sweep started a SECOND
///    full sweep, so twice as many `bun` children ran at once and whichever
///    finished last won the stored result.
/// 2. Clicking it in the seconds before the catch-up fired wrote
///    `last_run_week`, which made `due_this_week` false — so the week's
///    review was never SPOKEN at all. The card appeared in silence and the
///    user simply never heard that week's summary.
///
/// The gate fixes the first. The second is fixed by speaking here when this
/// run is the one that settles the week: "Run now" counts as the week's run,
/// as its tooltip says, so it has to deliver what the week's run delivers.
/// A second click in the same week refreshes the card silently, which is
/// what you want from a button you pressed on purpose.
pub fn run_now(gate: &SweepGate, claim: &crate::digest::Claim, speaker: &SpeakerHandle) -> Option<Card> {
    if !crate::config::deps_review_enabled() {
        return None;
    }
    let Some(_sweeping) = gate.enter() else {
        log::info!("deps review: a sweep is already running; Run now returns its result");
        return card();
    };
    let week = week_key(crate::digest::today_local());
    let owed = crate::config::deps_review_last_run_week().as_deref() != Some(week.as_str());

    let review = sweep(&crate::config::deps_review_roots(), week.clone());
    if !store_result(&review) {
        return card();
    }
    crate::config::set_deps_review_last_run_week(&week);
    // Keep the in-memory claim in step with disk, so the scheduled path does
    // not turn around and sweep again on top of this.
    claim.take(&week, || Some(week.clone()));
    if owed {
        speak_summary(speaker, &review, &week);
    }
    card()
}

/// The board card, or None — disabled, dismissed this week, or never run.
pub fn card() -> Option<Card> {
    if !crate::config::deps_review_enabled() {
        return None;
    }
    let review = stored_result()?;
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

    // ---- a failure is NEVER "current" (fresh-eyes review, 2026-10-10) ----

    fn failed(verdict: Verdict) -> Option<String> {
        match verdict {
            Verdict::Failed(why) => Some(why),
            Verdict::Result(_) => None,
        }
    }

    #[test]
    fn a_failing_bun_is_an_error_not_a_current_repo() {
        // THE BUG: `looks_like_result(Bun, _)` answered true unconditionally,
        // so this arm was unreachable and a repo whose `bun outdated` exited
        // non-zero with nothing on stdout came back `outdated: [], error:
        // None` — counted as CURRENT. A whole fleet of failures reported as
        // "All 38 repos current." and spoke it aloud.
        let why = failed(judge(
            Manager::Bun,
            Some(false),
            "",
            "error: lockfile had changes, but lockfile is frozen",
        ));
        let why = why.expect("a non-zero bun exit with no rows must be an error");
        assert!(why.contains("lockfile is frozen"), "the error should carry bun's own reason: {why}");
    }

    #[test]
    fn a_killed_check_is_an_error_not_a_current_repo() {
        let why = failed(judge(Manager::Bun, None, "", "")).expect("a killed check must be an error");
        assert!(why.contains("never exited"), "{why}");
    }

    #[test]
    fn a_failing_bun_that_still_printed_its_table_is_a_result() {
        // Parse first, judge after: usable output outranks the exit code.
        let table = "\
| Package | Current | Update | Latest |
|---------|---------|--------|--------|
| svelte  | 5.0.0   | 5.1.0  | 6.0.0  |
";
        match judge(Manager::Bun, Some(false), table, "some warning") {
            Verdict::Result(rows) => assert_eq!(rows.len(), 1, "the parsed row should survive"),
            Verdict::Failed(why) => panic!("usable output must not be an error: {why}"),
        }
    }

    #[test]
    fn a_clean_bun_run_is_a_result_with_no_rows() {
        match judge(Manager::Bun, Some(true), "", "") {
            Verdict::Result(rows) => assert!(rows.is_empty()),
            Verdict::Failed(why) => panic!("exit 0 is a result: {why}"),
        }
    }

    #[test]
    fn a_failing_pnpm_with_json_is_still_a_result() {
        // pnpm exits non-zero WHENEVER anything is outdated, so its
        // all-current case is a non-zero exit with `{}` on stdout. This is
        // the case `looks_like_result` exists for, and it must keep working.
        match judge(Manager::Pnpm, Some(false), "{}", "") {
            Verdict::Result(rows) => assert!(rows.is_empty()),
            Verdict::Failed(why) => panic!("pnpm's empty object is a clean result: {why}"),
        }
    }

    #[test]
    fn a_failing_pnpm_without_json_is_an_error() {
        assert!(failed(judge(Manager::Pnpm, Some(false), "not json at all", "ERR_PNPM_NO_LOCKFILE")).is_some());
    }

    #[test]
    fn an_error_without_stderr_still_says_something() {
        // A manager that writes its complaint to stdout must not produce a
        // blank error string, which would read as a mystery in the card.
        let why = failed(judge(Manager::Bun, Some(false), "could not resolve registry", "")).unwrap();
        assert!(why.contains("could not resolve registry"), "{why}");
    }

    #[test]
    fn errors_are_counted_separately_from_current_repos() {
        // The whole point of the above: `scanned` counts it, `behind` does
        // not, and the sentence says so out loud.
        let review = Review {
            week: "2026-W35".into(),
            ran_at: 0,
            repos: vec![
                RepoReport { name: "a".into(), path: "/a".into(), outdated: vec![], error: None },
                RepoReport {
                    name: "b".into(),
                    path: "/b".into(),
                    outdated: vec![],
                    error: Some("outdated check failed: boom".into()),
                },
            ],
        };
        assert_eq!(review.scanned(), 2);
        assert_eq!(review.errors(), 1);
        let text = body(&review);
        assert!(!text.contains("All 2 repos current"), "a failure must not read as current: {text}");
        assert!(text.contains("could not be checked"), "{text}");
    }

    // ---- a hung child cannot park the sweep (review finding 2) ------------

    #[test]
    #[cfg(unix)]
    fn a_hung_check_times_out_rather_than_blocking_forever() {
        // The real failure this guards: `Command::output()` waits as long as
        // the child lives, and the sweep runs inside the 4-hourly loop — so
        // one hung `bun` parked that loop for the life of the process and
        // silently ended the weekly review. Proven here with `sleep`, which
        // behaves exactly like the hung manager did.
        let started = Instant::now();
        let err = run_bounded(
            Path::new("/bin/sleep"),
            &["30"],
            Path::new("/tmp"),
            Duration::from_millis(400),
        )
        .expect_err("a child that outlives the timeout must be an error");
        let waited = started.elapsed();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            waited < Duration::from_secs(5),
            "it should give up at the timeout, not wait out the child (waited {waited:?})"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_bounded_run_captures_stdout_and_the_exit_status() {
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "printf 'hello'; exit 0"],
            Path::new("/tmp"),
            Duration::from_secs(10),
        )
        .expect("a quick child succeeds");
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.status.map(|s| s.success()), Some(true));
    }

    #[test]
    #[cfg(unix)]
    fn a_bounded_run_captures_stderr_for_the_error_message() {
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "printf 'boom' >&2; exit 1"],
            Path::new("/tmp"),
            Duration::from_secs(10),
        )
        .expect("a quick child succeeds");
        assert_eq!(out.status.map(|s| s.success()), Some(false));
        // And the judgement turns that into an error carrying the reason,
        // rather than into a repo that reads as current.
        let why = failed(judge(Manager::Bun, Some(false), &out.stdout, &out.stderr)).unwrap();
        assert!(why.contains("boom"), "{why}");
    }

    #[test]
    #[cfg(unix)]
    fn a_chatty_child_cannot_deadlock_the_reader() {
        // Both pipes are drained concurrently. Reading one to the end first
        // deadlocks when the child fills the other's buffer — far more than
        // a pipe's worth of stderr here.
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "i=0; while [ $i -lt 4000 ]; do echo stderr-noise >&2; i=$((i+1)); done; printf 'done'"],
            Path::new("/tmp"),
            Duration::from_secs(20),
        )
        .expect("a chatty child must still complete");
        assert_eq!(out.stdout, "done");
        assert!(out.stderr.len() > 16_384, "stderr should be larger than one pipe buffer");
    }

    #[test]
    fn a_missing_manager_is_an_error_on_the_report() {
        // `manager_bin` returning None is reported per repo rather than
        // panicking or looking like a clean sweep.
        let repo = Repo { path: PathBuf::from("/nonexistent-repo"), manager: Manager::Bun };
        let _ = &repo; // the real assertion is that sweep_one never panics
        let report = sweep_one(&repo);
        assert!(report.outdated.is_empty());
        assert!(report.error.is_some(), "an unsweepable repo must carry an error");
    }

    // ---- the sweep gate ---------------------------------------------------

    #[test]
    fn the_sweep_gate_admits_one_sweep_at_a_time() {
        let gate = SweepGate::default();
        let first = gate.enter().expect("the first sweep gets in");
        assert!(gate.enter().is_none(), "a second concurrent sweep must be refused");
        drop(first);
        assert!(gate.enter().is_some(), "and the gate reopens once the sweep is done");
    }

    #[test]
    fn the_sweep_gate_reopens_after_a_panic() {
        let gate = SweepGate::default();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = gate.enter().expect("in");
            panic!("the sweep blew up");
        }));
        assert!(gate.enter().is_some(), "a panicking sweep must not wedge the gate shut");
    }

    // ---- the Friday-never-fires bug (fresh-eyes review, 2026-10-10) -------

    #[test]
    fn a_non_monday_review_catches_up_after_the_week_rolls_over() {
        // `day = "fri"` and the machine sleeps from Friday evening through
        // Sunday. Monday the 24th is EARLIER in the week than Friday, and
        // week 34 is now unreachable. Before the fix this returned false
        // every day forever and the review never fired again.
        assert!(due_this_week(date(2026, 8, 24), Weekday::Fri, Some("2026-W33")));
    }

    #[test]
    fn a_non_monday_review_still_waits_its_turn_in_a_reviewed_week() {
        // Same Monday, but LAST week was reviewed. Nothing is overdue, so
        // Friday's review waits for Friday rather than firing early.
        assert!(!due_this_week(date(2026, 8, 24), Weekday::Fri, Some("2026-W34")));
    }

    #[test]
    fn a_never_run_review_waits_for_its_configured_day() {
        // No catch-up to perform: enabling the feature on a Monday with
        // `day = "fri"` must not fire immediately.
        assert!(!due_this_week(date(2026, 8, 24), Weekday::Fri, None));
    }

    #[test]
    fn the_configured_day_itself_is_due_whatever_the_history() {
        assert!(due_this_week(date(2026, 8, 28), Weekday::Fri, Some("2026-W34")));
        assert!(due_this_week(date(2026, 8, 28), Weekday::Fri, None));
    }

    #[test]
    fn a_week_already_run_is_never_due_however_overdue_the_last_one() {
        assert!(!due_this_week(date(2026, 8, 24), Weekday::Fri, Some("2026-W35")));
    }

    #[test]
    fn previous_week_key_crosses_the_iso_year_boundary() {
        // 2027-01-01 is in ISO week 2026-W53; the week before it is W52.
        assert_eq!(previous_week_key(date(2027, 1, 1)), "2026-W52");
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
    fn parses_real_pnpm_json() {
        // Captured from `pnpm -r outdated --format json` on a workspace repo.
        // The deprecated-but-current entry must NOT count as outdated.
        let out = r#"{
  "@react-email/components": {
    "current": "1.0.12",
    "latest": "1.0.12",
    "wanted": "1.0.12",
    "isDeprecated": true,
    "dependencyType": "dependencies"
  },
  "@pothos/core": {
    "current": "4.13.0",
    "latest": "4.13.1",
    "wanted": "4.13.0",
    "isDeprecated": false,
    "dependencyType": "dependencies"
  },
  "typescript": {
    "current": "6.0.3",
    "latest": "7.0.2",
    "wanted": "6.0.3",
    "isDeprecated": false,
    "dependencyType": "devDependencies"
  }
}"#;
        let mut parsed = parse_pnpm_outdated(out);
        parsed.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(parsed.len(), 2, "deprecated-but-current is not outdated");
        assert_eq!(parsed[0].name, "@pothos/core");
        assert!(!parsed[0].major);
        assert_eq!(parsed[1].name, "typescript");
        assert!(parsed[1].major);
    }

    #[test]
    fn pnpm_empty_object_is_a_clean_result() {
        assert!(parse_pnpm_outdated("{}").is_empty());
        assert!(looks_like_result(Manager::Pnpm, "{}"));
        assert!(!looks_like_result(Manager::Pnpm, "ERR_PNPM_SOMETHING"));
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

