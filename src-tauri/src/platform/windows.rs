//! Windows overlay support (§7): non-activating tool window pinned topmost.
//! WS_EX_NOACTIVATE keeps keyboard focus with the foreground terminal;
//! WS_EX_TOOLWINDOW hides the window from Alt-Tab (AC-5.3).

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    IsWindow, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

/// Is the cursor inside this window's rect (small margin)? GetCursorPos and
/// GetWindowRect share the same physical screen space.
///
/// # Safety
/// `hwnd` must be a valid window handle.
pub unsafe fn cursor_in_window(hwnd: *mut std::ffi::c_void, margin: i32) -> bool {
    let hwnd = HWND(hwnd);
    let mut cursor = POINT::default();
    let mut rect = RECT::default();
    if GetCursorPos(&mut cursor).is_err() || GetWindowRect(hwnd, &mut rect).is_err() {
        return false;
    }
    cursor.x >= rect.left - margin
        && cursor.x <= rect.right + margin
        && cursor.y >= rect.top - margin
        && cursor.y <= rect.bottom + margin
}

/// Which window currently owns the foreground, as a raw handle.
///
/// Captured before the reply window takes focus so it can be handed back on
/// close (`reply.rs`). `0` means "nothing usable": the desktop, or one of our
/// OWN windows — handing focus "back" to ourselves is either a no-op or a
/// re-focus of the window being closed, which is what the macOS side rejects
/// by pid for the same reason.
pub fn foreground_window() -> isize {
    unsafe {
        let hwnd = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return 0;
        }
        hwnd.0 as isize
    }
}

/// Hand the foreground back to a window captured by `foreground_window`.
///
/// Best effort by design: Windows refuses `SetForegroundWindow` from a
/// process that is not itself in the foreground, which is why the caller
/// restores focus BEFORE hiding the reply window rather than after.
///
/// The `IsWindow` check rejects a handle whose window has since been
/// destroyed. It is NOT proof of identity — handles are recycled, so a long
/// enough gap could in principle name somebody else's window — but the gap
/// here is one question's lifetime and the worst case is activating a window
/// that is already on screen.
///
/// # Safety
/// `hwnd` must be a handle previously returned by `foreground_window`.
pub unsafe fn restore_foreground(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    if !IsWindow(Some(hwnd)).as_bool() {
        return;
    }
    let _ = SetForegroundWindow(hwnd);
}

/// Apply the overlay's window styles.
///
/// MUST run AFTER the window is shown. tao recomputes the whole ex-style from
/// scratch inside `set_visible(true)` and writes it with SetWindowLongW; that
/// recomputation emits WS_EX_NOACTIVATE but never WS_EX_TOOLWINDOW, so
/// applying this first meant the Alt-Tab suppression (AC-5.3) was wiped a
/// moment after it was set.
///
/// Note this only ever ORs bits IN. WS_EX_NOACTIVATE is set by both tao and
/// this function, and cleared by neither, so the focus invariant (AC-5.1)
/// holds no matter which order they run in.
///
/// # Safety
/// `hwnd` must be a valid window handle (from tauri's `hwnd()`).
pub unsafe fn apply_overlay_styles(hwnd: *mut std::ffi::c_void) {
    let hwnd = HWND(hwnd);
    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    SetWindowLongPtrW(
        hwnd,
        GWL_EXSTYLE,
        ex_style | WS_EX_TOOLWINDOW.0 as isize | WS_EX_NOACTIVATE.0 as isize,
    );
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}

/// Play a WAV from memory with `PlaySound`, returning the buffer to hold onto.
///
/// `SND_MEMORY | SND_ASYNC` reads the buffer for the whole of playback, so the
/// caller must keep it alive — freeing it early truncates the sound or worse.
/// `SND_NODEFAULT` matters too: without it, a WAV Windows cannot parse plays
/// the system default beep instead, which would turn a broken chime into a
/// startling one.
pub fn play_wav(wav: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Media::Audio::{PlaySoundA, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

    let owned = wav.to_vec();
    let played = unsafe {
        PlaySoundA(
            windows::core::PCSTR(owned.as_ptr()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    played.as_bool().then_some(owned)
}

/// The pid owning the foreground window, or `None` if there isn't one.
///
/// `GetForegroundWindow` returns null when the foreground belongs to another
/// desktop or nothing is active (a lock screen, a UAC prompt); that reads as
/// "not frontmost", which errs toward SPEAKING.
pub fn frontmost_app_pid() -> Option<i32> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid > 0).then_some(pid as i32)
    }
}

/// Is `pid` still the very process a shim recorded, created at `created_ms`?
///
/// Windows recycles pids readily, so a recorded number alone proves nothing
/// minutes later — but a pid AND a creation time name exactly one process.
/// Compared for EQUALITY with what the shim read from the same kernel field
/// and converted the same way, so neither a stepped system clock nor the
/// creation stamp's tick granularity can make a newcomer pass. False when the
/// process cannot be opened or has exited: unknown means "not this session's
/// terminal", which SPEAKS.
pub fn is_recorded_process(pid: i32, created_ms: i64) -> bool {
    process_created_ms(pid) == Some(created_ms)
}

/// Creation time of a RUNNING process in Unix ms, read exactly as the shim
/// reads its ancestors'; `None` if it cannot be opened or has exited.
pub fn process_created_ms(pid: i32) -> Option<i64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid <= 0 {
        return None;
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32).ok()?;
        let (mut creation, mut exit) = (FILETIME::default(), FILETIME::default());
        let (mut kernel, mut user) = (FILETIME::default(), FILETIME::default());
        let read = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        if read.is_err() || exit.dwLowDateTime != 0 || exit.dwHighDateTime != 0 {
            return None;
        }
        filetime_to_unix_ms(
            ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64,
        )
    }
}

/// FILETIME (100 ns ticks since 1601) to Unix milliseconds; `None` before
/// 1970. MUST match the shim's own conversion, or equality never holds.
fn filetime_to_unix_ms(ticks: u64) -> Option<i64> {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    ticks
        .checked_sub(UNIX_EPOCH_TICKS)
        .map(|since| (since / 10_000) as i64)
}

/// Which window each process puts in front of you, from one pass over the
/// top-level windows.
///
/// This is how the Windows side tells a TERMINAL from a LAUNCHER. On macOS
/// the walk above a terminal ends at launchd, which owns no windows; on
/// Windows it continues into explorer.exe — the desktop, the taskbar and
/// every File Explorer window — or sihost/svchost.
#[derive(Debug, Default)]
pub struct Presenters {
    /// Pids owning a visible, unowned, non-tool top-level window.
    owners: Vec<i32>,
    /// `(shell pid, terminal pid, terminal visible)` for each ConPTY
    /// pseudo-console window that has an OWNER: a shell in a Windows
    /// Terminal tab owns no window of its own but this one, and WT owns it
    /// (measured, WT 1.24: every WT tab's shell, adopted ones included).
    /// Unowned pseudo consoles — a desktop app or an editor hosting a CLI —
    /// are not recorded at all: those shells present nothing, and the
    /// search climbs on to the app's own window.
    consoles: Vec<(i32, i32, bool)>,
    /// The desktop shell (`GetShellWindow`'s process), never a terminal.
    shell: Option<i32>,
}

impl Presenters {
    /// What `pid` puts in front of you: its own window; else, if its pseudo
    /// console belongs to a terminal, that terminal — or `Hidden` when the
    /// terminal is not on screen, which ENDS the search (`host_in_chain`).
    /// The desktop shell is never a host — belt and braces, since the search
    /// should reach a terminal first.
    pub fn presented_by(&self, pid: i32) -> crate::focus::Presents {
        use crate::focus::Presents;
        let presents = if self.owners.contains(&pid) {
            Presents::Window(pid)
        } else {
            match self.consoles.iter().find(|(shell, _, _)| *shell == pid) {
                Some((_, terminal, true)) => Presents::Window(*terminal),
                Some((_, _, false)) => Presents::Hidden,
                None => Presents::Nothing,
            }
        };
        match presents {
            Presents::Window(host) if Some(host) == self.shell => Presents::Nothing,
            other => other,
        }
    }
}

pub fn window_presenters() -> Presenters {
    use windows::core::BOOL;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetShellWindow, GetWindow, IsWindowVisible, GW_OWNER,
    };

    unsafe fn pid_of(hwnd: HWND) -> i32 {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid as i32
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Presenters);
        let owner = GetWindow(hwnd, GW_OWNER).ok().filter(|o| !o.0.is_null());
        let pid = pid_of(hwnd);
        if pid <= 0 {
            return true.into();
        }
        // Pseudo consoles are examined whether or not they are visible: what
        // decides is their OWNER, and whether THAT is on screen.
        let mut class = [0u16; 32];
        let len = GetClassNameW(hwnd, &mut class).max(0) as usize;
        if String::from_utf16_lossy(&class[..len]) == "PseudoConsoleWindow" {
            if let Some(owner) = owner {
                let terminal = pid_of(owner);
                if terminal > 0 {
                    out.consoles
                        .push((pid, terminal, IsWindowVisible(owner).as_bool()));
                }
            }
            return true.into();
        }
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }
        let tool = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0;
        if owner.is_none() && !tool && !out.owners.contains(&pid) {
            out.owners.push(pid);
        }
        true.into()
    }

    let mut out = Presenters::default();
    unsafe {
        let shell = GetShellWindow();
        if !shell.0.is_null() {
            out.shell = Some(pid_of(shell)).filter(|pid| *pid > 0);
        }
        // A failed enumeration leaves whatever was gathered; fewer presenters
        // means fewer matches, which errs toward speaking.
        let _ = EnumWindows(Some(collect), LPARAM(&mut out as *mut Presenters as isize));
    }
    out
}

/// Is the cursor within a CENTERED horizontal zone of the window?
///
/// The counterpart of the macOS function of the same name — see it for why
/// the zone that OPENS the island is narrower than the island itself.
/// Uncalled until Windows gets a global hover monitor (see `IDLE_HIT_WIDTH`).
///
/// # Safety
/// `hwnd` must be a valid window handle.
#[allow(dead_code)]
pub unsafe fn cursor_in_centered_zone(hwnd: isize, zone_width: i32, y_margin: i32) -> bool {
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};

    let mut rect = RECT::default();
    if GetWindowRect(HWND(hwnd as _), &mut rect).is_err() {
        return false;
    }
    let mut point = POINT::default();
    if GetCursorPos(&mut point).is_err() {
        return false;
    }
    let width = rect.right - rect.left;
    let half = (zone_width.max(0) / 2).min(width / 2);
    let center_x = rect.left + width / 2;
    point.x >= center_x - half
        && point.x <= center_x + half
        && point.y >= rect.top - y_margin
        && point.y <= rect.bottom + y_margin
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// Our own creation time, read the way the shim reads its ancestors'.
    fn my_created_ms() -> i64 {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut c, mut e) = (FILETIME::default(), FILETIME::default());
        let (mut k, mut u) = (FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).unwrap() };
        filetime_to_unix_ms(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64).unwrap()
    }

    #[test]
    fn a_process_is_the_recorded_one_only_with_the_same_creation_time() {
        let me = std::process::id() as i32;
        let created = my_created_ms();
        assert!(is_recorded_process(me, created));
        // Any other creation time is another process that had our pid —
        // before us or after us, it is not the one recorded.
        assert!(!is_recorded_process(me, created - 1));
        assert!(!is_recorded_process(me, created + 1));
        assert!(!is_recorded_process(me, now_ms()));
        // Nothing to open: never the recorded process.
        for junk in [0, -1, i32::MAX] {
            assert!(!is_recorded_process(junk, created), "{junk}");
        }
    }

    #[test]
    fn filetime_converts_at_the_unix_epoch() {
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), Some(0));
        assert_eq!(
            filetime_to_unix_ms(116_444_736_000_000_000 + 10_000),
            Some(1)
        );
        assert_eq!(filetime_to_unix_ms(0), None);
    }

    #[test]
    fn a_windowless_test_presents_nothing_and_the_desktop_shell_is_never_a_host() {
        use crate::focus::Presents;
        let p = window_presenters();
        assert!(p.owners.iter().all(|pid| *pid > 0));
        assert_eq!(p.presented_by(std::process::id() as i32), Presents::Nothing);
        if let Some(shell) = p.shell {
            assert_eq!(
                p.presented_by(shell),
                Presents::Nothing,
                "explorer is not a terminal"
            );
        }
    }

    #[test]
    fn a_pseudo_console_presents_as_its_terminal() {
        // Synthetic, mirroring what was measured: shell 2396 owns only a
        // pseudo console owned by WindowsTerminal 5892; explorer 5388 owns
        // the desktop.
        use crate::focus::Presents::{Hidden, Nothing, Window};
        let p = Presenters {
            owners: vec![5892, 5388, 777],
            consoles: vec![(2396, 5892, true), (2400, 6000, false)],
            shell: Some(5388),
        };
        assert_eq!(p.presented_by(2396), Window(5892));
        assert_eq!(p.presented_by(5892), Window(5892));
        assert_eq!(p.presented_by(777), Window(777));
        assert_eq!(p.presented_by(5388), Nothing);
        assert_eq!(p.presented_by(1), Nothing);
        // Its terminal hidden to the tray: decisive, not "nothing".
        assert_eq!(p.presented_by(2400), Hidden);
        // A pseudo console owned by the desktop shell still presents nothing.
        let p = Presenters {
            owners: vec![],
            consoles: vec![(10, 5388, true)],
            shell: Some(5388),
        };
        assert_eq!(p.presented_by(10), Nothing);
    }
}
