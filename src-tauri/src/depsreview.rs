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
use std::sync::atomic::Ordering;
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

/// Live reader threads spawned by [`run_bounded`], for the regression test
/// that a timed-out check does not leak them.
///
/// Counting the THING UNDER TEST rather than the process: the first version
/// of that test sampled this binary's total thread count, which drifts
/// constantly because `cargo test` runs hundreds of tests in parallel
/// threads of one process. It failed roughly half the time under load while
/// the code was behaving perfectly — a test that lies in both directions.
static LIVE_READERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Cap on captured stdout per repo. `bun outdated`'s table for a very large
/// workspace is still tens of kilobytes; a megabyte means something has gone
/// wrong, and buffering it unbounded is how a background task eats memory.
const MAX_OUTPUT: usize = 1 << 20;

/// Executable suffixes to try for a bare name. On Windows a bare `bun` is
/// not a file — `CreateProcess` resolves `bun.exe` through `PATHEXT`, which
/// is what `Command::new("bun")` used to do for us before this module
/// resolved the binary itself. Resolving by hand means doing that part too,
/// or the whole feature reports every repo as uncheckable on Windows.
/// `.ps1` and the bare name are deliberately ABSENT on Windows.
/// `CreateProcess` can execute neither a PowerShell script nor an
/// extensionless sh shim, and `is_executable` there is only `is_file()` —
/// so either would be a HIT. Worse than useless: `manager_bin` returns on
/// the first directory that matches, so a directory holding only
/// `pnpm.ps1` (or an npm-generated extensionless `pnpm`) would SHADOW the
/// real `pnpm.exe` further down `PATH`, turning a working install into
/// "%1 is not a valid Win32 application" on every repo. pnpm ships
/// `pnpm.cmd` beside `pnpm.ps1`, so `.cmd` covers it.
#[cfg(windows)]
const EXE_SUFFIXES: &[&str] = &[".exe", ".cmd", ".bat"];
#[cfg(not(windows))]
const EXE_SUFFIXES: &[&str] = &[""];

/// Places a GUI-launched app has to look for `bun` and `pnpm`. An app bundle
/// started from Finder/LaunchServices inherits a bare `PATH`
/// (`/usr/bin:/bin:/usr/sbin:/sbin`) that contains neither Homebrew nor a
/// package manager's own install directory, so resolving by `PATH` alone
/// reported every repo as uncheckable for exactly the users who launch the
/// app normally. Same hazard, and the same remedy, as `tmux::FALLBACK_BINS`.
///
/// Returned WITHOUT an executable suffix; `first_executable` applies those.
///
/// This cannot be exhaustive and does not try to be: a binary installed
/// through a Node version manager lives under a version-specific path
/// (`~/.nvm/versions/node/v25.8.0/bin/pnpm`) that no static list can name.
/// That case is what [`login_shell_path`] exists for; this list is the
/// backstop for when asking the shell fails.
fn fallback_bins(manager: Manager) -> Vec<PathBuf> {
    let name = manager.bin();
    let mut out = Vec::new();
    // An explicitly configured install location wins — set by both
    // managers' own installers, and present whenever the app inherited a
    // real environment.
    let env_home = match manager {
        Manager::Bun => "BUN_INSTALL",
        Manager::Pnpm => "PNPM_HOME",
    };
    if let Some(home) = std::env::var_os(env_home) {
        let home = PathBuf::from(home);
        // `BUN_INSTALL` is bun's prefix (binary under `bin/`); `PNPM_HOME`
        // IS the directory holding the binary.
        out.push(home.join("bin").join(name));
        out.push(home.join(name));
    }
    if let Some(home) = dirs::home_dir() {
        match manager {
            Manager::Bun => out.push(home.join(".bun").join("bin").join(name)),
            Manager::Pnpm => {
                // pnpm's standalone installer picks a per-platform home:
                // ~/Library/pnpm on macOS, ~/.local/share/pnpm on Linux.
                // Listing only the Linux one meant this fallback never
                // fired on the primary platform.
                out.push(home.join("Library").join("pnpm").join(name));
                out.push(home.join(".local").join("share").join("pnpm").join(name));
            }
        }
    }
    // Unix system locations. NOT compiled on Windows: there a literal like
    // `/usr/local/bin` is rooted but has no drive prefix, so it is not
    // absolute and resolves against the CURRENT drive — a planted
    // `C:\usr\local\bin\bun.exe` would be found and executed.
    #[cfg(not(windows))]
    out.extend(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/opt/local/bin",
            "/usr/bin",
        ]
        .iter()
        .map(|d| Path::new(d).join(name)),
    );
    // pnpm's standalone installer on Windows puts itself in
    // %LOCALAPPDATA%\pnpm (bun's `~\.bun\bin` is the home entry above).
    #[cfg(windows)]
    if manager == Manager::Pnpm {
        if let Some(local) = dirs::data_local_dir() {
            out.push(local.join("pnpm").join(name));
        }
    }
    absolute_only(out)
}

/// Keep only absolute candidates. Everything `resolve_manager` finds gets
/// EXECUTED, so a candidate that is merely rooted (`/usr/bin/bun` on Windows)
/// or relative (an env var holding `.`) must never reach `first_executable`:
/// it would resolve against the app's current directory or drive. This is the
/// rule `scan_path` applies to PATH entries, applied to the static fallbacks
/// (step 3) as well.
fn absolute_only(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    candidates.into_iter().filter(|p| p.is_absolute()).collect()
}

/// Resolve a package manager's binary, in three steps: this process's
/// `PATH`, then the `PATH`s a shell reports, then static fallbacks.
/// `None` means it could not be found, which the caller reports as an error
/// on the repo rather than as a clean sweep.
fn manager_bin(manager: Manager) -> Option<PathBuf> {
    resolve_manager(
        manager,
        std::env::var_os("PATH").as_deref(),
        login_shell_paths(),
    )
}

/// The resolution ORDER, with every input injected.
///
/// `login_paths` is a LAZY iterator, and that is the point twice over.
/// It must not be advanced at all when the process's own `PATH` already
/// answered, because advancing it sources the user's shell configuration.
/// And each step must be advanced only until the BINARY is found, not
/// until some PATH comes back — the first version stopped at the first
/// usable PATH, which on a machine whose `.zprofile` sets a perfectly good
/// PATH that simply lacks pnpm meant never escalating to the interactive
/// shell where nvm lives. That is the exact bug PMB-18 is about, so the
/// stop condition has to be "found it", not "got an answer".
fn resolve_manager(
    manager: Manager,
    process_path: Option<&std::ffi::OsStr>,
    login_paths: impl IntoIterator<Item = std::ffi::OsString>,
) -> Option<PathBuf> {
    // 1. The PATH this process actually has. Correct and free whenever the
    //    app was started from a terminal.
    if let Some(found) = scan_path(manager, process_path) {
        return Some(found);
    }
    // 2. The PATHs a shell reports, least invasive first. The authoritative
    //    answer to "what would the user get if they typed `pnpm`", and the
    //    only one that can find a version-manager install, whose path is
    //    version-specific (`~/.nvm/versions/node/v25.8.0/bin/pnpm`) and so
    //    cannot be guessed by any static list. Reached only when step 1
    //    missed — i.e. under a GUI launch, the broken case.
    for path in login_paths {
        if let Some(found) = scan_path(manager, Some(&path)) {
            return Some(found);
        }
    }
    // 3. Static guesses, last. Still worth having: a shell probe that fails
    //    or is misconfigured should not take the common installs with it.
    fallback_bins(manager)
        .iter()
        .find_map(|base| first_executable(base))
}

/// Scan one PATH-shaped value for the manager's binary. No fallbacks.
fn scan_path(manager: Manager, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let name = manager.bin();
    let path = path?;
    for dir in std::env::split_paths(path) {
        // ABSOLUTE entries only. An empty entry and a `.` both mean "the
        // current directory" in PATH, and the current directory here is
        // the APP's, not the user's shell's — so a tree the user merely
        // opened could put a `bun` next to whatever this process happens
        // to be sitting in. Everything this resolves gets EXECUTED, and a
        // relative entry is never the thing the user meant.
        if !dir.is_absolute() {
            continue;
        }
        if let Some(found) = first_executable(&dir.join(name)) {
            return Some(found);
        }
    }
    None
}

/// The `PATH` a login shell would hand the user, probed once per process.
///
/// WHY THIS EXISTS. A macOS app bundle launched from Finder inherits
/// `/usr/bin:/bin:/usr/sbin:/sbin` and almost no environment — no
/// `PNPM_HOME`, no `BUN_INSTALL`. A binary installed by a Node version
/// manager then lives somewhere no static list can name, because the path
/// carries the version in it. Asking the user's own shell is how a GUI app
/// recovers the environment the user actually works in.
///
/// WHAT IT COSTS, stated plainly because it is the user's shell: this runs
/// `$SHELL -ilc` and therefore SOURCES THEIR SHELL CONFIG — an interactive
/// login shell, because that is where `nvm`, `fnm` and friends install
/// themselves, and a non-interactive one would miss the very setups this
/// exists for. Measured at ~10 ms here. It is bounded by [`run_bounded`]
/// like every other child, killed by process group on timeout, and runs at
/// most once per app process, only when the binary was not already on
/// `PATH`, and only after the user opted into the deps review at all.
///
/// Windows has no `$SHELL` and GUI processes there inherit the user's real
/// `PATH` anyway, so this is a no-op outside unix.
fn login_shell_paths() -> impl Iterator<Item = std::ffi::OsString> {
    #[cfg(unix)]
    {
        // LAZY on purpose: `filter_map` over a range probes stage `n` only
        // when the caller actually asks for it, so a manager found by the
        // first shell never causes the second to run.
        (0..SHELL_FLAGS.len()).filter_map(login_shell_path_stage)
    }
    #[cfg(not(unix))]
    {
        std::iter::empty()
    }
}

/// One stage of the ladder, probed at most once per process.
///
/// Each flag set gets its own cache slot, so escalating to the interactive
/// shell does not re-run the non-interactive one, and a second manager
/// reuses both.
///
/// A FAILED probe is cached too, deliberately: re-probing would multiply
/// the one thing most worth limiting — sourcing the user's configuration —
/// and the static fallbacks still apply. The cost is that a transient
/// failure (a 10s timeout under load) disables that stage until the app
/// restarts, which for a desktop app is a real remedy.
#[cfg(unix)]
fn login_shell_path_stage(stage: usize) -> Option<std::ffi::OsString> {
    static CACHE: [std::sync::OnceLock<Option<std::ffi::OsString>>; SHELL_FLAGS.len()] = [
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
    ];
    CACHE
        .get(stage)?
        .get_or_init(|| probe_login_shell_path(SHELL_FLAGS[stage]))
        .clone()
}

/// How long the user's shell config gets, per attempt. Generous — `nvm`
/// initialisation alone is routinely a few hundred milliseconds — but
/// finite, because an rc file that blocks must not wedge the sweep.
#[cfg(unix)]
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// Flag sets to try, least invasive FIRST.
///
/// `-lc` is a NON-interactive login shell: it reads `.zprofile` /
/// `.bash_profile` / `.zshenv`, where a great many people export `PATH`,
/// and it does not read `.zshrc` / `.bashrc`. When that is enough, the
/// user's interactive configuration is never sourced at all.
///
/// `-ilc` is the escalation, and it has to exist because `nvm` and `fnm`
/// install themselves into `.zshrc` / `.bashrc` — the very setups PMB-18 is
/// about — which a non-interactive shell never reads.
///
/// `-ic` is for `csh`/`tcsh`, which reject `-l` unless it is the only flag
/// and so fail both of the above outright.
#[cfg(unix)]
const SHELL_FLAGS: [&str; 3] = ["-lc", "-ilc", "-ic"];

#[cfg(unix)]
fn probe_login_shell_path(flags: &str) -> Option<std::ffi::OsString> {
    let shell = PathBuf::from(std::env::var_os("SHELL")?);
    if !is_executable(&shell) {
        log::debug!("deps review: $SHELL is not executable; not probing it for a PATH");
        return None;
    }
    // The answer goes to a FILE, and the shell's own stdio is discarded.
    // That is not a style choice — it is the difference between the probe
    // working and not.
    //
    // Reading the answer from a pipe requires EOF, and EOF requires every
    // holder of the write end to be gone. An rc file that backgrounds
    // anything keeping stdout — a watcher, a daemon, `something &` —
    // therefore held the pipe open, the read hit its deadline, and a PATH
    // that had ALREADY been printed was thrown away. Worse, a descendant
    // escaping the process group via `setsid` parked both reader threads
    // for the life of the app. The rc patterns most likely to do this are
    // exactly the ones that make this probe worth running.
    //
    // The file lives in `~/.pingmybell/`, created 0700 and ours, rather
    // than in a world-writable temp directory where another account could
    // plant a symlink under a predictable name. Named per flag set so two
    // stages cannot collide.
    let answer = crate::ingest::data_dir()
        .ok()?
        .join(format!("path-probe{flags}"));
    let script = format!("printenv PATH > {}", shell_quote(&answer));
    let _ = std::fs::remove_file(&answer);

    let outcome = run_quiet(&shell, &[flags, &script], SHELL_TIMEOUT);
    let path = match outcome {
        Ok(()) => read_probe_answer(&answer),
        Err(err) => {
            log::debug!("deps review: {} {flags} failed: {err}", shell.display());
            None
        }
    };
    let _ = std::fs::remove_file(&answer);

    match &path {
        Some(found) => log::info!(
            "deps review: recovered a PATH from {} {flags} ({} entries)",
            shell.display(),
            std::env::split_paths(found).count()
        ),
        None => log::debug!(
            "deps review: no usable PATH from {} {flags}",
            shell.display()
        ),
    }
    path
}

/// The probe's answer, or `None` when the shell never wrote a usable one.
#[cfg(unix)]
fn read_probe_answer(path: &Path) -> Option<std::ffi::OsString> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    // A usable PATH has at least one ABSOLUTE entry. This rejects an empty
    // file (no `printenv` on the system), a shell that wrote an error
    // there, and a lone newline — all of which would otherwise read as a
    // successful probe.
    if !std::env::split_paths(trimmed).any(|dir| dir.is_absolute()) {
        return None;
    }
    Some(std::ffi::OsString::from(trimmed))
}

/// Single-quote a path for a shell command line.
///
/// Every shell this can run — sh, bash, zsh, fish, csh — treats a
/// single-quoted string literally, and the only character needing care is
/// the quote itself. `$HOME` can contain spaces; on this very machine the
/// project path does.
#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Run a child with its stdio discarded, bounded, killing the whole process
/// group on expiry.
///
/// Deliberately hands the child NO pipe: see the note in
/// [`probe_login_shell_path`] for why that is the point.
#[cfg(unix)]
fn run_quiet(bin: &Path, args: &[&str], timeout: Duration) -> Result<(), String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let spawned = cmd
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", bin.display()))?;
    let pgid = spawned.id() as i32;
    let mut child = GroupedChild {
        child: spawned,
        pgid,
        reaped: false,
    };
    match wait_bounded(&mut child, Instant::now() + timeout) {
        Some(_) => Ok(()),
        None => Err(format!("timed out after {timeout:?}")),
    }
}

#[cfg(test)]
fn manager_bin_in(manager: Manager, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    scan_path(manager, path).or_else(|| {
        fallback_bins(manager)
            .iter()
            .find_map(|base| first_executable(base))
    })
}

/// `base` plus each [`EXE_SUFFIXES`] entry, first one that is executable.
fn first_executable(base: &Path) -> Option<PathBuf> {
    for suffix in EXE_SUFFIXES {
        let candidate = if suffix.is_empty() {
            base.to_path_buf()
        } else {
            // `continue`, not `?`: a `?` here abandoned the WHOLE loop on a
            // path with no final component, so the bare-name candidate was
            // never tried either.
            let Some(name) = base.file_name() else {
                continue;
            };
            let mut name = name.to_os_string();
            name.push(suffix);
            base.with_file_name(name)
        };
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
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
    /// stdout hit [`MAX_OUTPUT`] and what we hold is a PREFIX of the real
    /// output. A truncated `bun outdated` table still parses — it just
    /// parses into an answer that is quietly incomplete, which is the same
    /// class of lie as reporting a failure as "current".
    truncated: bool,
}

/// Run `bin args…` in `cwd` with stdin closed, giving up after `timeout`.
///
/// Mirrors `tmux::run`: the read happens on a helper thread so a wedged
/// child cannot pin the caller, and the child is killed and reaped on
/// expiry. `Err` is a message fit for `RepoReport::error`; a timeout is an
/// error, never a result, because reporting a hung check as "current" is
/// the one thing this feature must not do.
fn run_bounded(
    bin: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<Captured, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so `reap` can kill the WHOLE tree. Killing the
    // direct child alone leaves any descendant holding the pipe write-ends
    // open, and the reader threads below then block in `read_to_end`
    // forever — two threads leaked per abandoned check.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let spawned = cmd
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", bin.display()))?;
    #[cfg(unix)]
    let pgid = spawned.id() as i32;
    #[cfg(windows)]
    let job = win_job::Job::adopt(&spawned);
    let mut child = GroupedChild {
        child: spawned,
        #[cfg(unix)]
        pgid,
        #[cfg(windows)]
        job,
        reaped: false,
    };

    let (Some(mut out), Some(mut err)) = (child.child.stdout.take(), child.child.stderr.take())
    else {
        reap(&mut child);
        return Err("could not capture the output of the outdated check".to_string());
    };

    // Both pipes are drained concurrently. Reading one to the end first can
    // deadlock: a child that fills the other pipe's buffer blocks forever
    // while we wait on the one it has stopped writing to.
    let (tx, rx) = mpsc::channel();
    let tx_err = tx.clone();
    // `take(MAX_OUTPUT + 1)`: one byte past the cap, so a complete output of
    // exactly MAX_OUTPUT bytes is not mistaken for a truncated one.
    let budget = MAX_OUTPUT as u64 + 1;
    LIVE_READERS.fetch_add(2, Ordering::Relaxed);
    thread::spawn(move || {
        let mut buf = Vec::new();
        let r = out.by_ref().take(budget).read_to_end(&mut buf);
        let _ = tx.send(("stdout", r.map(|_| buf)));
        LIVE_READERS.fetch_sub(1, Ordering::Relaxed);
    });
    thread::spawn(move || {
        let mut buf = Vec::new();
        let r = err.by_ref().take(budget).read_to_end(&mut buf);
        let _ = tx_err.send(("stderr", r.map(|_| buf)));
        LIVE_READERS.fetch_sub(1, Ordering::Relaxed);
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
    // Both readers take one byte past the cap, so trim both; only stdout's
    // overflow is reported, because stderr feeds a 200-char detail string
    // where a prefix is all that was ever used.
    stderr.truncate(MAX_OUTPUT);
    let truncated = stdout.len() > MAX_OUTPUT;
    if truncated {
        stdout.truncate(MAX_OUTPUT);
        log::warn!(
            "deps review: {} produced more than {MAX_OUTPUT} bytes; the result is incomplete",
            bin.display()
        );
    }
    Ok(Captured {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        status,
        truncated,
    })
}

/// Reap the child by `deadline`. A process that closes its pipes without
/// exiting must not pin the sweep, so the wait is polled. Normally returns
/// on the first iteration.
fn wait_bounded(child: &mut GroupedChild, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.child.try_wait() {
            Ok(Some(status)) => {
                // `try_wait` reaps on success, so the pid is no longer ours
                // and `reap` must not later signal whatever inherits it.
                child.reaped = true;
                return Some(status);
            }
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

/// A child spawned by [`run_bounded`], and therefore — on unix — one that
/// leads its own process group.
///
/// A newtype rather than a bare `Child` because [`reap`] signals the
/// NEGATED pid, i.e. the whole group, and that is only correct for a group
/// leader. A child spawned without `process_group(0)` inherits the parent's
/// group, so the identical call would SIGKILL this app and, on a dev
/// machine, the shell that launched it. (Verified: without the flag a
/// child's pgid equals the parent's; with it, its own pid.)
///
/// Why a type rather than a runtime check: asking `getpgid` at kill time
/// cannot answer for a child that has already exited, and a manager that
/// backgrounds something and returns is exactly the case the group kill
/// exists for. An earlier attempt did check `getpgid` and silently stopped
/// group-killing anything, because by then the child was a zombie — the
/// thread-leak test caught it. Making the type unconstructable outside the
/// one spawn path moves the guarantee to where it can be kept.
struct GroupedChild {
    child: std::process::Child,
    /// The group id, captured at spawn. `reap` uses THIS rather than
    /// re-reading `child.id()`: std's own `Child::kill` refuses to signal
    /// after a `wait` precisely because the pid may have been recycled, and
    /// re-deriving the group from a possibly-stale pid is the same hazard
    /// one step removed. No current path reaps after waiting; this makes a
    /// future one that does harmless rather than catastrophic.
    #[cfg(unix)]
    pgid: i32,
    /// The Job Object holding the child and everything it spawns, set to
    /// kill its members when the last handle closes. The Windows analogue of
    /// the process group: `None` only if creating or assigning the job
    /// failed, in which case `reap` falls back to killing the direct child.
    #[cfg(windows)]
    job: Option<win_job::Job>,
    /// Set once the child has been waited on, after which the pid is no
    /// longer ours to signal.
    reaped: bool,
}

/// Kill the child's whole PROCESS GROUP, then reap it.
///
/// The group, not just the child, and the reason is the reader threads: they
/// sit in `read_to_end` until the pipes reach EOF, which only happens once
/// every holder of a write-end has gone. A manager that leaves a background
/// descendant would otherwise park both threads for as long as that
/// descendant lives — two leaked threads per abandoned check, in a process
/// that runs from login.
///
/// On Windows the equivalent is the child's Job Object: `pnpm.cmd` runs as
/// `cmd.exe` -> `node.exe`, and killing only `cmd.exe` leaves node holding
/// the pipes.
fn reap(child: &mut GroupedChild) {
    if child.reaped {
        return;
    }
    child.reaped = true;
    #[cfg(unix)]
    {
        // The NEGATED pid names the whole group, and that is only correct
        // for a group leader — which is what [`GroupedChild`] guarantees.
        // `pgid > 1` matters: `kill(0, …)` would signal OUR own group.
        if child.pgid > 1 {
            unsafe {
                libc::kill(-child.pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    if let Some(job) = &child.job {
        job.terminate();
    }
    let _ = child.child.kill();
    let _ = child.child.wait();
}

/// A Windows Job Object that kills every process in it when terminated or
/// when its last handle closes (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
///
/// Processes spawned by a member join the job automatically, so assigning
/// the direct child covers its whole tree. Assignment happens just AFTER
/// `spawn` (std offers no suspended-create), so a descendant started in that
/// first instant would escape; a package manager's startup is far slower than
/// the gap, and this is a best-effort bound on a weekly background check.
///
/// Nesting is fine: Windows 8+ allows a process to be in several jobs, so a
/// parent that already put this app in a job does not make assignment fail.
#[cfg(windows)]
mod win_job {
    use std::os::windows::io::AsRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(HANDLE);

    impl Job {
        /// Create a kill-on-close job and put `child` in it. `None` (logged)
        /// on any failure; the caller then degrades to killing the child.
        pub fn adopt(child: &std::process::Child) -> Option<Job> {
            unsafe {
                let handle = match CreateJobObjectW(None, PCWSTR::null()) {
                    Ok(h) => h,
                    Err(err) => {
                        log::debug!("deps review: CreateJobObject failed: {err}");
                        return None;
                    }
                };
                // From here the handle is owned by `job`, so every early
                // return closes it (which, with no members yet, is harmless).
                let job = Job(handle);
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if let Err(err) = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) {
                    log::debug!("deps review: SetInformationJobObject failed: {err}");
                    return None;
                }
                if let Err(err) =
                    AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle() as _))
                {
                    log::debug!("deps review: AssignProcessToJobObject failed: {err}");
                    return None;
                }
                Some(job)
            }
        }

        /// Kill every process in the job.
        pub fn terminate(&self) {
            unsafe {
                let _ = TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
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
        self.repos
            .iter()
            .filter(|r| !r.outdated.is_empty())
            .collect()
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
        "node_modules",
        ".git",
        "dist",
        "build",
        "target",
        ".astro",
        ".claude",
        ".wrangler",
        "coverage",
        ".venv",
        "venv",
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
                out.push(Repo {
                    path: dir.to_path_buf(),
                    manager,
                });
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
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() != 4 {
                return None;
            }
            let name = cells[0].split_whitespace().next()?.to_string();
            if name == "Package" || name.is_empty() {
                return None; // header row
            }
            if cells
                .iter()
                .any(|c| !c.is_empty() && c.chars().all(|ch| ch == '-'))
            {
                return None; // divider row
            }
            let (current, latest) = (cells[1].to_string(), cells[3].to_string());
            if current.is_empty() || latest.is_empty() || current == latest {
                return None;
            }
            let major = major_of(&current) != major_of(&latest);
            Some(Outdated {
                name,
                current,
                latest,
                major,
            })
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
            Some(Outdated {
                name: name.clone(),
                current,
                latest,
                major,
            })
        })
        .collect()
}

/// The repo's display name: its directory name, which is what the sentence
/// and the card say. Shared with the sweep's budget path so an unswept repo
/// is named the same way a swept one is.
fn repo_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string()
}

/// What the card says when a manager could not be resolved at all. One
/// definition, so the message and the test that checks it cannot drift.
fn not_installed(manager: Manager) -> String {
    // Says what to DO about it. The app looks on its own PATH, then asks
    // the login shell, then tries the usual install locations — so if all
    // three missed, the useful next step is making it visible to the shell
    // the user actually logs in with.
    format!(
        "{0} was not found — not on this app's PATH, not in your login shell's PATH, \
         and not in the usual install locations. If `{0}` works in your terminal, \
         check that your shell config exports it for login shells too.",
        manager.bin()
    )
}

/// Run the manager's outdated command in one repo. A spawn failure is an
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
        return report(Vec::new(), Some(not_installed(repo.manager)));
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
        Ok(out) => match judge(repo.manager, &out) {
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
fn judge(manager: Manager, out: &Captured) -> Verdict {
    // Truncated output is an ERROR, not a short answer. A `bun outdated`
    // table cut off at MAX_OUTPUT still parses perfectly — into a row count
    // that is silently a prefix of the truth. Reporting that as the repo's
    // dependency state is the same class of lie as reporting a failure as
    // "current", so it gets the same treatment. (pnpm is incidentally safe:
    // truncated JSON does not parse. Bun's table is the exposed one.)
    if out.truncated {
        return Verdict::Failed(format!(
            "outdated check produced more than {MAX_OUTPUT} bytes; the result would be incomplete"
        ));
    }
    let succeeded = out.status.map(|s| s.success());
    let outdated = match manager {
        Manager::Bun => parse_outdated(&out.stdout),
        Manager::Pnpm => parse_pnpm_outdated(&out.stdout),
    };
    if succeeded == Some(true) || !outdated.is_empty() || looks_like_result(manager, &out.stdout) {
        return Verdict::Result(outdated);
    }
    let detail: String = out.stderr.trim().chars().take(200).collect();
    let detail = if detail.is_empty() {
        out.stdout.trim().chars().take(200).collect::<String>()
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
    crate::ingest::data_dir()
        .ok()
        .map(|dir| dir.join("deps-review.json"))
}

/// The last sweep, if one is stored and still parses. A corrupt or
/// half-written file reads as "no review", which is the same as never having
/// run — never a panic, and never a crash on a file a user hand-edited.
pub fn stored_result() -> Option<Review> {
    if let Some(path) = result_path() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            return serde_json::from_str(&raw).ok();
        }
    }
    // Nothing in the new location: an install upgraded from the build that
    // kept the review inside `config.json` still has this week's result
    // there, and `last_run_week` already says the week is settled — so
    // without this the board would go blank until the NEXT sweep, days
    // later.
    //
    // MOVE, in that order: store first, prune only on success. Pruning as
    // part of the read lost the value outright whenever the write failed,
    // and the likeliest cause of that failure — no usable data directory —
    // is exactly what sent us down this path.
    let legacy = crate::config::deps_review_legacy_result()?;
    let review: Review = serde_json::from_str(&legacy).ok()?;
    if store_result(&review) {
        log::info!("deps review: migrated the stored result out of config.json");
    } else {
        log::warn!("deps review: could not migrate the stored result; leaving it in config.json");
    }
    Some(review)
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
            // The result is now durable in its own file, so the legacy copy
            // can go: an install upgraded from the build that kept it in
            // `config.json` stops paying the shim's hot path for it. This is
            // the ONLY place the legacy key is dropped, and it runs strictly
            // after a successful write — which is what makes the migration
            // in `stored_result` a move rather than a gamble. No-op once
            // clean.
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
    sweep_until(repos, week, Instant::now() + SWEEP_TIMEOUT)
}

/// The sweep with its deadline passed in, so the budget path is testable
/// without waiting out [`SWEEP_TIMEOUT`].
fn sweep_until(repos: Vec<Repo>, week: String, deadline: Instant) -> Review {
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

impl Drop for GroupedChild {
    /// Last resort, so no path can leave a zombie plus a live descendant.
    ///
    /// `wait_bounded`'s `try_wait` error arm (ECHILD/EINVAL — effectively
    /// unreachable) neither reaps nor marks the child, and a future early
    /// return could do the same. `reap` is idempotent via `reaped`, so this
    /// is a no-op on every normal path.
    fn drop(&mut self) {
        reap(self);
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
    if !due_this_week(
        today,
        crate::config::deps_review_day(),
        crate::config::deps_review_last_run_week().as_deref(),
    ) {
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
        crate::speaker::Callout::DepsReview {
            body: &body(review),
        },
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
        terminal: None,
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
pub fn run_now(
    gate: &SweepGate,
    claim: &crate::digest::Claim,
    speaker: &SpeakerHandle,
) -> Option<Card> {
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
        assert!(due_this_week(
            date(2026, 8, 26),
            Weekday::Mon,
            Some("2026-W34")
        ));
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
        assert!(!due_this_week(
            date(2026, 8, 26),
            Weekday::Mon,
            Some("2026-W35")
        ));
    }

    // ---- a failure is NEVER "current" (fresh-eyes review, 2026-10-10) ----

    /// Serialises tests that OBSERVE or MUTATE process-global state.
    ///
    /// `cargo test` runs tests in parallel threads of one process, so a test
    /// that counts live threads must not run beside one that spawns them,
    /// and a test that replaces `PATH` must not run beside one that resolves
    /// a binary through it. Both would flake, and intermittently — the worst
    /// kind of test to debug later.
    static PROCESS_GLOBAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_GLOBAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn cap(status: Option<bool>, stdout: &str, stderr: &str) -> Captured {
        Captured {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            status: status.map(exit_status),
            truncated: false,
        }
    }

    /// An `ExitStatus` with the given success-ness, built from a raw value
    /// rather than by shelling out.
    ///
    /// The first version ran `/bin/sh -c 'exit 0'`, which made every
    /// `judge` test unix-only — so the pure-logic coverage (a failure is
    /// never "current") vanished on Windows, which is where the rest of
    /// this module is least exercised. `ExitStatusExt` is per-platform but
    /// available on both, so the tests do not have to be.
    fn exit_status(success: bool) -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            // A unix wait status, not an exit code: the code lives in the
            // high byte, so exit(1) is 1 << 8.
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 })
        }
    }

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
            &cap(
                Some(false),
                "",
                "error: lockfile had changes, but lockfile is frozen",
            ),
        ));
        let why = why.expect("a non-zero bun exit with no rows must be an error");
        assert!(
            why.contains("lockfile is frozen"),
            "the error should carry bun's own reason: {why}"
        );
    }

    #[test]
    fn a_killed_check_is_an_error_not_a_current_repo() {
        let why = failed(judge(Manager::Bun, &cap(None, "", "")))
            .expect("a killed check must be an error");
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
        match judge(Manager::Bun, &cap(Some(false), table, "some warning")) {
            Verdict::Result(rows) => assert_eq!(rows.len(), 1, "the parsed row should survive"),
            Verdict::Failed(why) => panic!("usable output must not be an error: {why}"),
        }
    }

    #[test]
    fn a_clean_bun_run_is_a_result_with_no_rows() {
        match judge(Manager::Bun, &cap(Some(true), "", "")) {
            Verdict::Result(rows) => assert!(rows.is_empty()),
            Verdict::Failed(why) => panic!("exit 0 is a result: {why}"),
        }
    }

    #[test]
    fn a_failing_pnpm_with_json_is_still_a_result() {
        // pnpm exits non-zero WHENEVER anything is outdated, so its
        // all-current case is a non-zero exit with `{}` on stdout. This is
        // the case `looks_like_result` exists for, and it must keep working.
        match judge(Manager::Pnpm, &cap(Some(false), "{}", "")) {
            Verdict::Result(rows) => assert!(rows.is_empty()),
            Verdict::Failed(why) => panic!("pnpm's empty object is a clean result: {why}"),
        }
    }

    #[test]
    fn a_failing_pnpm_without_json_is_an_error() {
        assert!(failed(judge(
            Manager::Pnpm,
            &cap(Some(false), "not json at all", "ERR_PNPM_NO_LOCKFILE")
        ))
        .is_some());
    }

    #[test]
    fn an_error_without_stderr_still_says_something() {
        // A manager that writes its complaint to stdout must not produce a
        // blank error string, which would read as a mystery in the card.
        let why = failed(judge(
            Manager::Bun,
            &cap(Some(false), "could not resolve registry", ""),
        ))
        .unwrap();
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
                RepoReport {
                    name: "a".into(),
                    path: "/a".into(),
                    outdated: vec![],
                    error: None,
                },
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
        assert!(
            !text.contains("All 2 repos current"),
            "a failure must not read as current: {text}"
        );
        assert!(text.contains("could not be checked"), "{text}");
    }

    // ---- a hung child cannot park the sweep (review finding 2) ------------

    #[test]
    #[cfg(unix)]
    fn a_hung_check_times_out_rather_than_blocking_forever() {
        let _serial = serial();
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
    fn an_abandoned_check_does_not_leak_its_reader_threads() {
        // Holds the lock the other `run_bounded` tests take, because they
        // move LIVE_READERS too: a concurrent check would add 2 to the
        // counter mid-measurement and this test's baseline comparison would
        // read it as a leak. Counting the readers instead of the process
        // removed the interference from the REST of the suite; it does not
        // remove it from tests that drive the same code path.
        let _serial = serial();
        // THE BUG: `reap` killed the direct child only. A manager that
        // leaves a background descendant leaves that descendant holding the
        // pipe write-ends, so both reader threads stay blocked in
        // `read_to_end` after `run_bounded` has already returned an error —
        // two threads leaked per abandoned check, every week, in a
        // launch-at-login process. Killing the process GROUP closes those
        // write ends, so the readers see EOF and exit.
        //
        // `sh -c "...; sleep 30 & exit 0"`: the shell exits immediately and
        // the `sleep` lives on holding the pipes. Exactly the shape of a
        // manager that daemonises something.
        //
        // Asserts on LIVE_READERS — the reader threads themselves — not on
        // this process's total thread count. The first version of this test
        // sampled the process, which drifts constantly because cargo runs
        // hundreds of tests in parallel threads of one binary: it failed
        // about half the time under load while the code was perfectly
        // correct, and would have failed in CI. Counting the thing under
        // test is immune to every other test in the suite.
        // DRAIN to zero before sampling. The lock excludes the other
        // `run_bounded` tests' main threads, but their readers `fetch_sub`
        // after `tx.send` — microseconds AFTER the test that owned them
        // returned and released the lock. So a baseline taken immediately
        // can be a stale 2 that is about to become 0, and then the final
        // read legitimately differs from it. Draining first makes `before`
        // a true zero and the final condition an exact equality, so the
        // assertion measures this test's readers and nothing else.
        let settle = Instant::now() + Duration::from_secs(5);
        while LIVE_READERS.load(Ordering::Relaxed) != 0 && Instant::now() < settle {
            thread::sleep(Duration::from_millis(10));
        }
        let before = LIVE_READERS.load(Ordering::Relaxed);
        assert_eq!(before, 0, "the counter must be drained before measuring");
        for _ in 0..6 {
            let err = run_bounded(
                Path::new("/bin/sh"),
                &["-c", "printf 'x'; sleep 30 & exit 0"],
                Path::new("/tmp"),
                Duration::from_millis(200),
            );
            // The descendant holds the pipes past the deadline, so this must
            // time out rather than complete — otherwise the test is not
            // exercising the abandoned path at all.
            assert!(
                err.is_err(),
                "a child whose descendant holds the pipes must time out"
            );
        }

        // Each reader decrements as it exits, so this returns to the
        // baseline only if all twelve actually finished.
        // Exact equality, not `>`: with `before` a true zero, `> before`
        // would exit the moment the count dipped, including while this
        // test's own last-iteration readers were still live.
        let deadline = Instant::now() + Duration::from_secs(10);
        while LIVE_READERS.load(Ordering::Relaxed) != before && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        let after = LIVE_READERS.load(Ordering::Relaxed);
        assert_eq!(
            after,
            before,
            "six abandoned checks leaked {} reader threads",
            after.saturating_sub(before)
        );
    }

    #[test]
    #[cfg(unix)]
    fn process_group_is_what_makes_the_group_kill_safe() {
        let _serial = serial();
        // The invariant `GroupedChild` exists to encode, proven directly: a
        // child spawned WITH process_group(0) leads its own group (pgid ==
        // its own pid), and one spawned WITHOUT it inherits OURS — in which
        // case `reap`'s `kill(-pgid)` would SIGKILL this process and the
        // shell that started it. That is why `reap` takes a `GroupedChild`
        // and not a bare `Child`.
        let mine = unsafe { libc::getpgid(0) };

        let mut with = Command::new("/bin/sh");
        with.args(["-c", "exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        {
            use std::os::unix::process::CommandExt;
            with.process_group(0);
        }
        let mut child = with.spawn().expect("spawn");
        let pid = child.id() as i32;
        let pgid = unsafe { libc::getpgid(pid) };
        assert_eq!(
            pgid, pid,
            "process_group(0) must make the child a group leader"
        );
        assert_ne!(pgid, mine, "and that group must not be ours");
        let _ = child.wait();

        let mut plain = Command::new("/bin/sh");
        plain
            .args(["-c", "exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = plain.spawn().expect("spawn");
        let pid = child.id() as i32;
        // Read it before reaping; after `wait` the pid is no longer ours.
        let pgid = unsafe { libc::getpgid(pid) };
        let _ = child.wait();
        assert_eq!(
            pgid, mine,
            "a child spawned without process_group shares OUR group — exactly what the guard refuses"
        );
        assert_ne!(
            pgid, pid,
            "so it is not a group leader and must not be group-killed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_bounded_run_captures_stdout_and_the_exit_status() {
        let _serial = serial();
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
        let _serial = serial();
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
        let why = failed(judge(Manager::Bun, &out)).unwrap();
        assert!(why.contains("boom"), "{why}");
    }

    #[test]
    #[cfg(unix)]
    fn a_chatty_child_cannot_deadlock_the_reader() {
        let _serial = serial();
        // Both pipes are drained concurrently. Reading one to the end first
        // deadlocks once the child fills the OTHER pipe's buffer.
        //
        // The volume matters and the first version of this test got it
        // wrong: it wrote 4000 lines (~56 KB) assuming a 16 KiB pipe, but
        // macOS pipes grow to 64 KiB, so it all fit and nothing ever
        // blocked — the test passed with a naive sequential reader too, and
        // therefore proved nothing. Measured on this platform: 4000 and
        // 5000 lines complete naively, 6000 deadlock. 20 000 lines
        // (~280 KB) is past any platform's pipe capacity by a wide margin.
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "i=0; while [ $i -lt 20000 ]; do echo stderr-noise >&2; i=$((i+1)); done; printf 'done'"],
            Path::new("/tmp"),
            Duration::from_secs(60),
        )
        .expect("a chatty child must still complete");
        assert_eq!(out.stdout, "done", "stdout must survive a flood on stderr");
        assert!(
            out.stderr.len() > 200_000,
            "the flood must be large enough that a sequential reader would deadlock (got {})",
            out.stderr.len()
        );
        assert!(!out.truncated, "280 KB is well under MAX_OUTPUT");
    }

    #[test]
    fn an_unsweepable_repo_is_an_error_never_a_clean_sweep() {
        let _serial = serial();
        // A nonexistent cwd: the spawn itself fails. This asserts the
        // invariant that matters — never a panic, never a repo that reads
        // as current — but note it does NOT exercise the `manager_bin ==
        // None` branch, because `bun` is on PATH on a dev machine. That
        // branch has its own test below.
        let repo = Repo {
            path: PathBuf::from("/nonexistent-repo"),
            manager: Manager::Bun,
        };
        let report = sweep_one(&repo);
        assert!(report.outdated.is_empty());
        assert!(
            report.error.is_some(),
            "an unsweepable repo must carry an error"
        );
        assert_eq!(report.name, "nonexistent-repo");
    }

    // ---- login-shell PATH recovery (PMB-18) -------------------------------

    #[test]
    #[cfg(unix)]
    fn a_probe_answer_needs_at_least_one_absolute_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let answer = dir.path().join("path-probe");

        // What a working probe writes.
        std::fs::write(&answer, "/usr/local/bin:/usr/bin\n").unwrap();
        assert_eq!(
            read_probe_answer(&answer),
            Some(std::ffi::OsString::from("/usr/local/bin:/usr/bin")),
            "the trailing newline printenv leaves must be trimmed"
        );

        // What a BROKEN probe writes, none of which may read as success.
        for junk in [
            "",
            "\n",
            "   \n",
            "printenv: command not found",
            "relative:also/relative",
        ] {
            std::fs::write(&answer, junk).unwrap();
            assert_eq!(read_probe_answer(&answer), None, "{junk:?} is not a PATH");
        }

        // A file the shell never created at all.
        assert_eq!(read_probe_answer(&dir.path().join("nope")), None);
    }

    #[test]
    #[cfg(unix)]
    fn the_probe_path_is_quoted_so_a_home_with_spaces_still_works() {
        // This machine's own project path has spaces in it, so an unquoted
        // redirect target would have split and written to the wrong place.
        assert_eq!(shell_quote(Path::new("/tmp/a b/c")), "'/tmp/a b/c'");
        // And a quote in the path cannot end the quoting.
        assert_eq!(shell_quote(Path::new("/tmp/it's")), r"'/tmp/it'\''s'");
    }

    #[test]
    #[cfg(unix)]
    fn the_quoted_path_survives_a_round_trip_through_a_real_shell() {
        // The quoting is only worth anything if a shell agrees with it.
        let dir = tempfile::tempdir().expect("tempdir");
        let awkward = dir.path().join("a b's dir");
        std::fs::create_dir(&awkward).expect("mkdir");
        let answer = awkward.join("path-probe");
        let script = format!("printenv PATH > {}", shell_quote(&answer));
        run_quiet(
            Path::new("/bin/sh"),
            &["-c", &script],
            Duration::from_secs(10),
        )
        .expect("the shell should accept the quoted path");
        assert!(
            read_probe_answer(&answer).is_some(),
            "the PATH should have landed in the file"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_backgrounded_rc_job_no_longer_defeats_the_probe() {
        let _serial = serial();
        // THE BUG this rewrite is about. Reading the answer from a pipe
        // needed EOF, which needs every write-end holder gone — so an rc
        // file that backgrounds anything keeping stdout discarded a PATH
        // that had already been printed, and the setsid variant parked two
        // reader threads forever.
        //
        // Here the child both backgrounds a long sleep AND writes the
        // answer. With stdio discarded and the answer in a file, the probe
        // must not care that the descendant outlives it.
        let dir = tempfile::tempdir().expect("tempdir");
        let answer = dir.path().join("path-probe");
        let script = format!("sleep 45 & printenv PATH > {}", shell_quote(&answer));

        let started = Instant::now();
        run_quiet(
            Path::new("/bin/sh"),
            &["-c", &script],
            Duration::from_secs(10),
        )
        .expect("the shell itself exits promptly");
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(5),
            "must not wait for the backgrounded job (waited {waited:?})"
        );
        assert!(
            read_probe_answer(&answer).is_some(),
            "the PATH must survive the background job"
        );
        // Deliberately NOT asserting on LIVE_READERS here. `run_quiet`
        // hands out no pipes at all, so it cannot leak a reader by
        // construction — and that counter is process-global, so asserting
        // on it from a test that does not own the whole process is the
        // flake this file has been bitten by before. The dedicated leak
        // test owns that property.
    }

    #[test]
    #[cfg(unix)]
    fn the_login_shell_probe_recovers_a_real_path_from_this_machine() {
        // The end-to-end claim, run for real: ask this machine's $SHELL and
        // get back something that parses as a PATH containing /usr/bin.
        // Skipped rather than failed where $SHELL is absent (some CI
        // containers), because that is a property of the runner, not a bug.
        let _serial = serial();
        let Some(shell) = std::env::var_os("SHELL") else {
            eprintln!("no $SHELL; skipping");
            return;
        };
        if !is_executable(Path::new(&shell)) {
            eprintln!("$SHELL is not executable; skipping");
            return;
        }
        // Every stage in turn — a machine whose profile sets PATH answers
        // at the first, one whose .zshrc does at the second.
        let Some(path) = SHELL_FLAGS.iter().find_map(|f| probe_login_shell_path(f)) else {
            eprintln!("the login shell produced no PATH; skipping (shell config dependent)");
            return;
        };
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert!(dirs.len() > 1, "a real PATH has several entries: {dirs:?}");
        assert!(
            dirs.iter().any(|d| d == Path::new("/usr/bin")),
            "every sane login shell has /usr/bin: {dirs:?}"
        );
    }

    /// Set in the re-executed child below. Its presence means "you are the
    /// child: do the real check".
    #[cfg(unix)]
    const BARE_PATH_CHILD: &str = "PMB_BARE_PATH_CHILD";

    #[test]
    #[cfg(unix)]
    fn a_version_manager_install_is_found_under_a_finder_style_bare_path() {
        // THE BUG PMB-18 IS ABOUT, reproduced for real rather than argued.
        //
        // A Finder-launched bundle gets PATH=/usr/bin:/bin:/usr/sbin:/sbin
        // and almost no environment — no PNPM_HOME, no BUN_INSTALL. A pnpm
        // installed by a version manager then sits at a version-specific
        // path no static list can name, so before this change every pnpm
        // repo reported "not installed".
        //
        // PATH is process-global, so rather than mutate it (which raced the
        // rest of this binary), this re-executes THIS TEST in a child with
        // the stripped environment and lets the child run the real
        // `manager_bin`.
        if std::env::var_os(BARE_PATH_CHILD).is_some() {
            // We are the child. `bun` and `pnpm` are both unreachable via
            // the bare PATH; only the login-shell probe can find them.
            assert_eq!(
                scan_path(Manager::Pnpm, std::env::var_os("PATH").as_deref()),
                None,
                "the bare PATH must NOT contain pnpm, or this proves nothing"
            );
            // Which STEP answered matters, not just that one did. A pnpm
            // sitting in a static fallback location (Homebrew,
            // /usr/local/bin — the common case, and what CI would be)
            // satisfies `manager_bin` with the shell step contributing
            // nothing, so asserting only "it was found" would advertise
            // PMB-18 as proven while the new code lay dead.
            let via_shell = login_shell_paths().find_map(|p| scan_path(Manager::Pnpm, Some(&p)));
            let via_fallback = fallback_bins(Manager::Pnpm)
                .iter()
                .find_map(|b| first_executable(b));
            match (via_shell, via_fallback) {
                (Some(found), _) => {
                    assert!(is_executable(&found), "{found:?} must be runnable");
                    assert_eq!(
                        manager_bin(Manager::Pnpm).as_ref(),
                        Some(&found),
                        "the shell's answer must be the one resolution returns"
                    );
                    eprintln!("child: recovered pnpm VIA THE SHELL at {}", found.display());
                }
                (None, Some(fallback)) => eprintln!(
                    "child: pnpm only reachable via a static fallback at {}",
                    fallback.display()
                ),
                (None, None) => eprintln!("child: pnpm not installed anywhere on this machine"),
            }
            return;
        }

        let _serial = serial();
        let Some(shell) = std::env::var_os("SHELL") else {
            eprintln!("no $SHELL; skipping");
            return;
        };
        let exe = std::env::current_exe().expect("test binary");
        let mut cmd = Command::new(exe);
        cmd.args([
            "--exact",
            "depsreview::tests::a_version_manager_install_is_found_under_a_finder_style_bare_path",
            "--nocapture",
        ]);
        // As close to a LaunchServices environment as a test can get.
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("SHELL", &shell)
            .env(BARE_PATH_CHILD, "1");
        if let Some(home) = std::env::var_os("HOME") {
            cmd.env("HOME", home); // Finder-launched apps do get HOME
        }
        let out = cmd.output().expect("re-exec the test binary");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "child failed:\n{stderr}");

        // If this machine has pnpm at all, the child must have found it
        // from the bare PATH — which is only possible via the login shell.
        // Echo whatever the child concluded, always.
        for line in stderr.lines().filter(|l| l.starts_with("child:")) {
            eprintln!("{line}");
        }
        if stderr.contains("child: pnpm not installed anywhere") {
            eprintln!("pnpm is not installed here; nothing to recover");
            return;
        }
        if stderr.contains("child: pnpm only reachable via a static fallback") {
            // Honest skip rather than a green tick: on this machine the
            // new code was not what found it, so this run proves nothing
            // about PMB-18.
            eprintln!("pnpm lives in a fallback location here; the shell step was not exercised");
            return;
        }
        assert!(
            stderr.contains("child: recovered pnpm VIA THE SHELL at"),
            "pnpm is reachable, so the bare-PATH child had to recover it through the shell:\n{stderr}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn scan_path_finds_a_binary_only_on_the_path_it_was_given() {
        // `scan_path` is the half with no fallbacks, so it is the one that
        // proves a recovered PATH is really what found the binary.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("bun");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        assert_eq!(
            scan_path(Manager::Bun, Some(dir.path().as_os_str())),
            Some(fake)
        );
        assert_eq!(
            scan_path(Manager::Pnpm, Some(dir.path().as_os_str())),
            None,
            "wrong manager"
        );
        assert_eq!(scan_path(Manager::Bun, None), None, "no path, no answer");
        assert_eq!(
            scan_path(Manager::Bun, Some(std::ffi::OsStr::new(""))),
            None,
            "an empty PATH entry must not resolve to the cwd"
        );
    }

    /// An executable `name` inside a fresh temp dir, for PATH tests.
    #[cfg(unix)]
    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// A lazy stand-in for the shell ladder that records how far it got.
    #[cfg(unix)]
    fn counting_paths(
        paths: Vec<std::ffi::OsString>,
        probed: std::rc::Rc<std::cell::Cell<usize>>,
    ) -> impl Iterator<Item = std::ffi::OsString> {
        paths
            .into_iter()
            .inspect(move |_| probed.set(probed.get() + 1))
    }

    #[test]
    #[cfg(unix)]
    fn the_login_shell_is_not_consulted_when_path_already_answered() {
        // The probe SOURCES the user's shell configuration, so WHEN it runs
        // is a correctness property, not a performance one. A
        // terminal-launched app, whose PATH already has the binary, must
        // never reach it.
        let dir = tempfile::tempdir().expect("tempdir");
        let bun = fake_bin(dir.path(), "bun");
        let probed = std::rc::Rc::new(std::cell::Cell::new(0));

        let found = resolve_manager(
            Manager::Bun,
            Some(dir.path().as_os_str()),
            counting_paths(vec![std::ffi::OsString::from("/nope")], probed.clone()),
        );
        assert_eq!(found, Some(bun));
        assert_eq!(
            probed.get(),
            0,
            "the shell must not be run when PATH already answered"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_ladder_escalates_only_until_the_binary_is_found() {
        // THE BUG the staged ladder exists for. The first version stopped
        // at the first shell that returned a usable PATH — but a
        // `.zprofile` can set a perfectly good PATH that simply lacks the
        // manager, and then the interactive shell where nvm lives was
        // never reached. Measured on this machine: `zsh -lc` yields a real
        // PATH with no pnpm on it, `zsh -ilc` yields one with it.
        let early = tempfile::tempdir().expect("tempdir"); // a PATH, but no pnpm
        let late = tempfile::tempdir().expect("tempdir"); // where pnpm actually is
        let pnpm = fake_bin(late.path(), "pnpm");
        let probed = std::rc::Rc::new(std::cell::Cell::new(0));

        let found = resolve_manager(
            Manager::Pnpm,
            Some(std::ffi::OsStr::new("/nonexistent-bin-dir")),
            counting_paths(
                vec![
                    early.path().as_os_str().to_os_string(),
                    late.path().as_os_str().to_os_string(),
                ],
                probed.clone(),
            ),
        );
        assert_eq!(
            found,
            Some(pnpm),
            "it must keep going past a PATH that lacks the binary"
        );
        assert_eq!(probed.get(), 2, "and must have escalated exactly once");
    }

    #[test]
    #[cfg(unix)]
    fn the_ladder_stops_at_the_first_stage_that_has_the_binary() {
        let early = tempfile::tempdir().expect("tempdir");
        let bun = fake_bin(early.path(), "bun");
        let probed = std::rc::Rc::new(std::cell::Cell::new(0));

        let found = resolve_manager(
            Manager::Bun,
            Some(std::ffi::OsStr::new("/nonexistent-bin-dir")),
            counting_paths(
                vec![
                    early.path().as_os_str().to_os_string(),
                    std::ffi::OsString::from("/never-reached"),
                ],
                probed.clone(),
            ),
        );
        assert_eq!(found, Some(bun));
        assert_eq!(
            probed.get(),
            1,
            "the second shell must not run once the first answered"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_shell_probe_still_leaves_the_static_fallbacks() {
        // Step 3 is a backstop, not decoration: a shell that cannot be read
        // must not take the ordinary installs down with it.
        let found = resolve_manager(
            Manager::Bun,
            Some(std::ffi::OsStr::new("/nonexistent")),
            Vec::<std::ffi::OsString>::new(),
        );
        assert_eq!(
            found,
            fallback_bins(Manager::Bun)
                .iter()
                .find_map(|b| first_executable(b)),
            "with no PATH and no shell, the answer is exactly the fallback list's"
        );
    }

    #[test]
    #[cfg(unix)]
    fn scan_path_refuses_relative_entries() {
        // A PATH entry of `.` (or an empty one, which means the same) is
        // resolved against THIS process's cwd, not the user's. Since
        // whatever resolves here is then executed, a relative entry must
        // never win — a recovered PATH comes from the user's shell config
        // and can legitimately contain one.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("bun");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        // Absolute: found. Relative to the same place: refused.
        assert_eq!(
            scan_path(Manager::Bun, Some(dir.path().as_os_str())),
            Some(fake)
        );
        for relative in [".", "./bin", "relative/dir", ""] {
            assert_eq!(
                scan_path(Manager::Bun, Some(std::ffi::OsStr::new(relative))),
                None,
                "{relative:?} is relative and must be skipped"
            );
        }
    }

    #[test]
    fn a_manager_is_not_found_on_an_empty_search_path() {
        // The branch the test above cannot reach, without touching the
        // process's PATH: that is global state other tests depend on.
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            manager_bin_in(Manager::Pnpm, Some(empty.path().as_os_str())),
            fallback_bins(Manager::Pnpm)
                .iter()
                .find_map(|b| first_executable(b)),
            "an empty search path must fall through to the fallback list and nothing else"
        );
        // With no search path at all AND no fallback hit, it is None. On a
        // machine that happens to have pnpm in a fallback location this is
        // correctly Some, so assert the shape rather than the value.
        if let Some(found) = manager_bin_in(Manager::Pnpm, None) {
            assert!(is_executable(&found), "a resolved path must be executable");
        }
    }

    #[test]
    fn the_not_installed_error_names_the_manager() {
        // Asserts on the PRODUCTION string, via the helper `sweep_one`
        // itself uses. The first version of this test built the message
        // locally and then checked its own `format!` — so dropping the
        // manager's name from `sweep_one` would not have failed it.
        for manager in [Manager::Bun, Manager::Pnpm] {
            let msg = not_installed(manager);
            assert!(msg.contains(manager.bin()), "must name the manager: {msg}");
            assert!(msg.contains("not found"), "{msg}");
            // It should tell the user what to DO, not just what failed.
            assert!(
                msg.contains("login shell"),
                "must point at the likely cause: {msg}"
            );
        }
    }

    #[test]
    fn truncated_output_is_an_error_not_a_short_answer() {
        // A `bun outdated` table cut off at MAX_OUTPUT still PARSES — into
        // a row count that is a silent prefix of the truth. That is the
        // same class of lie as reporting a failure as "current", so it gets
        // the same treatment.
        let mut table = String::from("| Package | Current | Update | Latest |\n");
        table.push_str("| svelte | 5.0.0 | 5.1.0 | 6.0.0 |\n");
        let out = Captured {
            stdout: table,
            stderr: String::new(),
            status: Some(exit_status(true)),
            truncated: true,
        };
        let why = failed(judge(Manager::Bun, &out))
            .expect("truncation must be an error even with a clean exit and parseable rows");
        assert!(why.contains("incomplete"), "{why}");
    }

    #[test]
    #[cfg(unix)]
    fn a_resolved_binary_is_a_file_not_a_directory() {
        // `is_executable` must reject a directory that happens to be named
        // like the binary, or `manager_bin` would hand `Command` a path it
        // cannot spawn.
        let dir = tempfile::tempdir().expect("tempdir");
        let decoy = dir.path().join("bun");
        std::fs::create_dir(&decoy).expect("mkdir");
        assert!(!is_executable(&decoy));
        assert_eq!(first_executable(&decoy), None);
    }

    #[test]
    fn every_fallback_candidate_is_a_bare_name_for_the_suffix_pass() {
        // `first_executable` is what appends `.exe` on Windows, so the
        // fallback list must NOT carry an extension of its own — otherwise
        // the Windows pass would be looking for `bun.exe.exe`.
        for manager in [Manager::Bun, Manager::Pnpm] {
            for candidate in fallback_bins(manager) {
                assert_eq!(
                    candidate.file_name().and_then(|n| n.to_str()),
                    Some(manager.bin()),
                    "{candidate:?} should end in the bare binary name"
                );
            }
        }
    }

    #[test]
    fn absolute_only_drops_relative_and_rooted_candidates() {
        // Step 3 executes what it finds, so a candidate that resolves
        // against the app's cwd (or, on Windows, its current drive) must be
        // dropped — the same rule `scan_path` applies to PATH entries. This
        // is also what neutralizes a relative `BUN_INSTALL`/`PNPM_HOME`.
        let abs = std::env::temp_dir().join("bun");
        let kept = absolute_only(vec![
            PathBuf::from("bun"),
            PathBuf::from("./bin/bun"),
            PathBuf::from("relative/dir/bun"),
            PathBuf::new(),
            abs.clone(),
        ]);
        assert_eq!(kept, vec![abs]);
    }

    #[test]
    #[cfg(windows)]
    fn unix_literals_are_not_absolute_on_windows_and_are_filtered() {
        // The bug: `/usr/local/bin/bun` is rooted but drive-less, so on
        // Windows it is NOT absolute and would resolve against the current
        // drive (`C:\usr\local\bin\bun.exe`).
        for literal in [
            "/usr/local/bin/bun",
            "/opt/homebrew/bin/bun",
            "\\usr\\bin\\bun",
        ] {
            assert!(!Path::new(literal).is_absolute(), "{literal}");
        }
        assert_eq!(
            absolute_only(vec![PathBuf::from("/usr/local/bin/bun")]),
            Vec::<PathBuf>::new()
        );
        for manager in [Manager::Bun, Manager::Pnpm] {
            for candidate in fallback_bins(manager) {
                assert!(
                    candidate.is_absolute(),
                    "{candidate:?} must be absolute on Windows"
                );
                assert!(
                    !candidate.starts_with("/usr") && !candidate.starts_with("/opt"),
                    "{candidate:?}: Unix literals must not be compiled on Windows"
                );
            }
        }
    }

    /// Is `pid` a running process? (`OpenProcess` fails for a pid that is
    /// gone; an exited-but-not-yet-closed one is told apart by a zero-timeout
    /// wait.)
    #[cfg(windows)]
    fn pid_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
                return false;
            };
            let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
            let _ = CloseHandle(h);
            alive
        }
    }

    #[test]
    #[cfg(windows)]
    fn reap_kills_the_whole_tree_on_windows() {
        let _serial = serial();
        // cmd.exe (the direct child) runs ping.exe (the grandchild), the
        // same shape as `pnpm.cmd` -> `node.exe`. Killing only cmd.exe
        // would leave ping alive, holding the pipes.
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "ping -n 30 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let spawned = cmd.spawn().expect("spawn cmd");
        let parent = spawned.id();
        let job = win_job::Job::adopt(&spawned);
        assert!(job.is_some(), "the child must be assignable to a job");
        let mut child = GroupedChild {
            child: spawned,
            job,
            reaped: false,
        };

        // Find the grandchild. cmd takes a moment to start ping.
        let query =
            format!("(Get-CimInstance Win32_Process -Filter 'ParentProcessId={parent}').ProcessId");
        let mut grandchild = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        while grandchild.is_none() && Instant::now() < deadline {
            let out = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &query])
                .output()
                .expect("run powershell");
            grandchild = String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.trim().parse::<u32>().ok());
            if grandchild.is_none() {
                thread::sleep(Duration::from_millis(200));
            }
        }
        let grandchild = grandchild.expect("cmd.exe must have started a ping child");
        assert!(
            pid_alive(parent),
            "the parent must be alive before the reap"
        );
        assert!(
            pid_alive(grandchild),
            "the grandchild must be alive before the reap"
        );

        let mut out = child.child.stdout.take().expect("stdout");
        let reader = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
        });

        reap(&mut child);

        let deadline = Instant::now() + Duration::from_secs(10);
        while pid_alive(grandchild) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!pid_alive(parent), "the direct child must be gone");
        assert!(
            !pid_alive(grandchild),
            "the grandchild must be killed with the job, not left holding the pipes"
        );
        // With every holder of the write end gone, the reader reaches EOF.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reader.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(reader.is_finished(), "the pipe reader leaked");
    }

    #[test]
    fn the_pnpm_fallbacks_cover_the_macos_install_location() {
        // The first draft listed only pnpm's LINUX home
        // (~/.local/share/pnpm), so this fallback never fired on the
        // primary platform — the exact case it exists for.
        let home = dirs::home_dir().expect("home");
        let candidates = fallback_bins(Manager::Pnpm);
        assert!(
            candidates.contains(&home.join("Library").join("pnpm").join("pnpm")),
            "macOS: ~/Library/pnpm must be listed; got {candidates:?}"
        );
        assert!(
            candidates.contains(&home.join(".local").join("share").join("pnpm").join("pnpm")),
            "Linux: ~/.local/share/pnpm must still be listed"
        );
        assert!(fallback_bins(Manager::Bun).contains(&home.join(".bun").join("bin").join("bun")));
    }

    #[test]
    fn repos_not_reached_before_the_budget_are_errors_named_after_themselves() {
        // An already-spent deadline means NOTHING is swept, so every repo
        // must come back as its own unswept entry. The earlier draft of this
        // loop risked naming them all after whichever repo it stopped on.
        let repos = vec![
            Repo {
                path: PathBuf::from("/tmp/alpha"),
                manager: Manager::Bun,
            },
            Repo {
                path: PathBuf::from("/tmp/beta"),
                manager: Manager::Pnpm,
            },
            Repo {
                path: PathBuf::from("/tmp/gamma"),
                manager: Manager::Bun,
            },
        ];
        let spent = Instant::now() - Duration::from_secs(1);
        let review = sweep_until(repos, "2026-W35".into(), spent);

        // Every repo is still counted — a truncated sweep must not look like
        // a smaller, healthier fleet.
        assert_eq!(review.scanned(), 3);
        assert_eq!(review.errors(), 3);
        assert!(review.behind().is_empty());
        let names: Vec<&str> = review.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "beta", "gamma"],
            "each must be named after itself"
        );
        let paths: Vec<&str> = review.repos.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec!["/tmp/alpha", "/tmp/beta", "/tmp/gamma"]);
        // And the sentence says so rather than claiming they are current.
        let text = body(&review);
        assert!(text.contains("could not be checked"), "{text}");
        assert!(!text.contains("current"), "{text}");
    }

    #[test]
    fn an_empty_sweep_is_not_an_error() {
        let review = sweep_until(
            Vec::new(),
            "2026-W35".into(),
            Instant::now() - Duration::from_secs(1),
        );
        assert_eq!(review.scanned(), 0);
        assert_eq!(review.errors(), 0);
        assert!(body(&review).contains("No Bun or pnpm repos"));
    }

    // ---- the sweep gate ---------------------------------------------------

    #[test]
    fn the_sweep_gate_admits_one_sweep_at_a_time() {
        let gate = SweepGate::default();
        let first = gate.enter().expect("the first sweep gets in");
        assert!(
            gate.enter().is_none(),
            "a second concurrent sweep must be refused"
        );
        drop(first);
        assert!(
            gate.enter().is_some(),
            "and the gate reopens once the sweep is done"
        );
    }

    #[test]
    fn the_sweep_gate_reopens_after_a_panic() {
        let gate = SweepGate::default();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = gate.enter().expect("in");
            panic!("the sweep blew up");
        }));
        assert!(
            gate.enter().is_some(),
            "a panicking sweep must not wedge the gate shut"
        );
    }

    // ---- the Friday-never-fires bug (fresh-eyes review, 2026-10-10) -------

    #[test]
    fn a_non_monday_review_catches_up_after_the_week_rolls_over() {
        // `day = "fri"` and the machine sleeps from Friday evening through
        // Sunday. Monday the 24th is EARLIER in the week than Friday, and
        // week 34 is now unreachable. Before the fix this returned false
        // every day forever and the review never fired again.
        assert!(due_this_week(
            date(2026, 8, 24),
            Weekday::Fri,
            Some("2026-W33")
        ));
    }

    #[test]
    fn a_non_monday_review_still_waits_its_turn_in_a_reviewed_week() {
        // Same Monday, but LAST week was reviewed. Nothing is overdue, so
        // Friday's review waits for Friday rather than firing early.
        assert!(!due_this_week(
            date(2026, 8, 24),
            Weekday::Fri,
            Some("2026-W34")
        ));
    }

    #[test]
    fn a_never_run_review_waits_for_its_configured_day() {
        // No catch-up to perform: enabling the feature on a Monday with
        // `day = "fri"` must not fire immediately.
        assert!(!due_this_week(date(2026, 8, 24), Weekday::Fri, None));
    }

    #[test]
    fn the_configured_day_itself_is_due_whatever_the_history() {
        assert!(due_this_week(
            date(2026, 8, 28),
            Weekday::Fri,
            Some("2026-W34")
        ));
        assert!(due_this_week(date(2026, 8, 28), Weekday::Fri, None));
    }

    #[test]
    fn a_week_already_run_is_never_due_however_overdue_the_last_one() {
        assert!(!due_this_week(
            date(2026, 8, 24),
            Weekday::Fri,
            Some("2026-W35")
        ));
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
        let parsed =
            parse_outdated("| a | 0.19.3 | 0.19.3 | 0.33.3 |\n| b | 5.9.3 | 5.9.3 | 7.0.2 |\n");
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
                RepoReport {
                    name: "virixia".into(),
                    path: "/v".into(),
                    outdated: vec![od(true), od(false), od(false)],
                    error: None,
                },
                RepoReport {
                    name: "bryy".into(),
                    path: "/b".into(),
                    outdated: vec![od(false)],
                    error: None,
                },
                RepoReport {
                    name: "ok".into(),
                    path: "/o".into(),
                    outdated: vec![],
                    error: None,
                },
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
            repos: vec![RepoReport {
                name: "a".into(),
                path: "/a".into(),
                outdated: vec![],
                error: Some("boom".into()),
            }],
        };
        assert!(body(&review).contains("1 could not be checked."));
    }
}
