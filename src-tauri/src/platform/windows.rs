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

/// Parent pid from a Toolhelp snapshot — the Windows half of the ancestor
/// walk (`tmux::parent_pid` shells out to `ps` and is unix-only).
///
/// A snapshot is O(processes) and this runs at most 16 times per explicit
/// user action, so the cost never lands on the idle path.
pub fn parent_pid(pid: i32) -> Option<i32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    if pid <= 0 {
        return None;
    }
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut parent = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid as u32 {
                    parent = Some(entry.th32ParentProcessID as i32);
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
        // A parent that is itself, 0, or the idle process is the top of the
        // walk, not another step.
        parent.filter(|p| *p > 0 && *p != pid)
    }
}

/// The first visible, unowned, titled top-level window belonging to `pid`.
///
/// Owned windows (tool palettes, dialogs) and untitled ones are skipped: the
/// target is the thing a user would call "the app's window".
unsafe fn main_window_for_pid(pid: u32) -> Option<HWND> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowTextLengthW, IsWindowVisible, GW_OWNER,
    };

    struct Target {
        pid: u32,
        found: Option<HWND>,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let target = &mut *(lparam.0 as *mut Target);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let unowned = GetWindow(hwnd, GW_OWNER).map(|o| o.0.is_null()).unwrap_or(true);
        if pid == target.pid
            && IsWindowVisible(hwnd).as_bool()
            && unowned
            && GetWindowTextLengthW(hwnd) > 0
        {
            target.found = Some(hwnd);
            return BOOL(0); // found it — stop enumerating
        }
        BOOL(1)
    }

    let mut target = Target { pid, found: None };
    let _ = EnumWindows(Some(visit), LPARAM(&mut target as *mut Target as isize));
    target.found
}

/// Bring `hwnd` to the foreground from a process that is not itself in the
/// foreground.
///
/// Windows refuses a bare `SetForegroundWindow` in exactly our situation: the
/// overlay is `WS_EX_NOACTIVATE` and never receives input, so it can never
/// satisfy the foreground-rights rule on its own. Attaching to the current
/// foreground thread's input queue for the duration of the call is the
/// documented way through, and it is detached again immediately — leaving the
/// queues attached would make our message loop hostage to another app's.
unsafe fn force_foreground(hwnd: HWND) -> bool {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, IsIconic, SetForegroundWindow, ShowWindow,
        SW_RESTORE,
    };

    // A minimized window that is "brought forward" without this stays
    // minimized and the click reads as having done nothing.
    if IsIconic(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    if SetForegroundWindow(hwnd).as_bool() {
        return true;
    }
    let foreground = GetForegroundWindow();
    let mut fg_pid = 0u32;
    let fg_thread = GetWindowThreadProcessId(foreground, Some(&mut fg_pid));
    let our_thread = GetCurrentThreadId();
    if fg_thread == 0 || fg_thread == our_thread {
        return false;
    }
    let attached = AttachThreadInput(our_thread, fg_thread, true).as_bool();
    let raised = SetForegroundWindow(hwnd).as_bool();
    let _ = BringWindowToTop(hwnd);
    if attached {
        let _ = AttachThreadInput(our_thread, fg_thread, false);
    }
    raised
}

/// Activate the app owning `pid`. True when a window actually came forward.
///
/// Never activates OUR OWN process: raising the overlay would break the focus
/// invariant (AC-5.1) that the whole window is built around.
pub fn activate_app_with_pid(pid: i32) -> bool {
    if pid <= 0 || pid as u32 == std::process::id() {
        return false;
    }
    unsafe {
        match main_window_for_pid(pid as u32) {
            Some(hwnd) => force_foreground(hwnd),
            None => false,
        }
    }
}

/// Activate the first visible window whose process image is `exe` (e.g.
/// `Claude.exe`), for when the recorded process is already gone.
///
/// Matched case-insensitively on the file name alone — the full path differs
/// between a Store install, a per-user install and a dev build.
pub fn activate_app_with_exe(exe: &str) -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let wanted = exe.to_ascii_lowercase();
    let mut candidates = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name: String = String::from_utf16_lossy(
                    &entry.szExeFile[..entry
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(entry.szExeFile.len())],
                );
                if name.to_ascii_lowercase() == wanted && entry.th32ProcessID != std::process::id()
                {
                    candidates.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
        // An Electron app is many processes and only one owns the window;
        // take the first that actually has one.
        candidates
            .into_iter()
            .any(|pid| match main_window_for_pid(pid) {
                Some(hwnd) => force_foreground(hwnd),
                None => false,
            })
    }
}

/// Re-assert the overlay's topmost rank.
///
/// The taskbar is itself a topmost window, and Explorer re-asserts its
/// position whenever it feels like it — a badge update, a shell refresh —
/// which buries a window parked in the taskbar band. The bell then keeps
/// ringing with no pill anywhere: sound but no UI. Calling this lifts the
/// overlay back above the bar; it is cheap enough to run on a timer for as
/// long as the placement dips below the work area.
///
/// # Safety
/// `hwnd` must be a valid window handle (from tauri's `hwnd()`).
pub unsafe fn assert_topmost(hwnd: *mut std::ffi::c_void) {
    let _ = SetWindowPos(
        HWND(hwnd),
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

/// Is the cursor within a CENTERED horizontal zone of the window?
///
/// The counterpart of the macOS function of the same name — see it for why
/// the zone that OPENS the island is narrower than the island itself.
///
/// # Safety
/// `hwnd` must be a valid window handle.
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

    /// Runs against THIS process, so it fails if the Toolhelp walk is broken
    /// rather than merely if the guards are: a test process always has a
    /// real parent, and reporting `None` for it would mean the ancestor walk
    /// never leaves the ground — which is exactly how jump-to-session and
    /// focus-aware quieting silently do nothing.
    #[test]
    fn the_ancestor_walk_finds_a_real_parent_and_terminates() {
        let own = std::process::id() as i32;
        let parent = parent_pid(own).expect("this process has a parent");
        assert!(parent > 0 && parent != own, "parent {parent} of {own}");

        // Sixteen is the walk budget `focus_host_app` spends; a chain that
        // does not end inside it would spin on every click.
        let mut current = own;
        let mut steps = 0;
        while let Some(next) = parent_pid(current) {
            assert_ne!(next, current, "a process cannot be its own parent");
            current = next;
            steps += 1;
            assert!(steps <= 64, "ancestor chain did not terminate");
        }
    }

    #[test]
    fn absurd_pids_are_not_walked() {
        assert_eq!(parent_pid(0), None);
        assert_eq!(parent_pid(-1), None);
    }

    /// The overlay must never be what comes forward (AC-5.1): a jump that
    /// raised our own window would steal focus from the very terminal the
    /// user is being sent to.
    #[test]
    fn we_never_activate_ourselves() {
        assert!(!activate_app_with_pid(std::process::id() as i32));
        assert!(!activate_app_with_pid(0));
        assert!(!activate_app_with_pid(-5));
    }
}
