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

/// Is `pid` still the process a shim recorded at `captured_ms`?
///
/// Windows recycles pids readily, so a recorded number alone proves nothing
/// minutes later. The shim only records processes alive when it stamped the
/// chain, so the ORIGINAL holder of a pid was created no later than that
/// stamp — and any process created after it is an unrelated one that
/// inherited the number. Also false when the process cannot be opened or has
/// exited: unknown means "not this session's terminal", which SPEAKS.
pub fn is_recorded_process(pid: i32, captured_ms: i64) -> bool {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid <= 0 {
        return false;
    }
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) else {
            return false;
        };
        let (mut creation, mut exit) = (FILETIME::default(), FILETIME::default());
        let (mut kernel, mut user) = (FILETIME::default(), FILETIME::default());
        let read = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        if read.is_err() || exit.dwLowDateTime != 0 || exit.dwHighDateTime != 0 {
            return false;
        }
        let ticks = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
        filetime_to_unix_ms(ticks).is_some_and(|created_ms| created_ms <= captured_ms)
    }
}

/// FILETIME (100 ns ticks since 1601) to Unix milliseconds; `None` before
/// 1970, which no process this app could care about was created in.
fn filetime_to_unix_ms(ticks: u64) -> Option<i64> {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    ticks
        .checked_sub(UNIX_EPOCH_TICKS)
        .map(|since| (since / 10_000) as i64)
}

/// Pids that own a top-level window a person could be looking at: visible,
/// unowned, and not a tool window.
///
/// This is how the Windows side tells a TERMINAL from a LAUNCHER. On macOS
/// the walk above a terminal ends at launchd, which owns no windows; on
/// Windows it continues into explorer.exe — the desktop, the taskbar and
/// every File Explorer window — or sihost/svchost. Without this, a session
/// started from Explorer would be "frontmost" whenever you clicked the
/// desktop.
pub fn window_owner_pids() -> Vec<i32> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindow, IsWindowVisible, GW_OWNER};

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let pids = &mut *(lparam.0 as *mut Vec<i32>);
        let visible = IsWindowVisible(hwnd).as_bool();
        let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.0.is_null());
        let tool = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0;
        if visible && !owned && !tool {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid > 0 && !pids.contains(&(pid as i32)) {
                pids.push(pid as i32);
            }
        }
        true.into()
    }

    let mut pids: Vec<i32> = Vec::new();
    unsafe {
        // A failed enumeration leaves whatever was gathered; fewer owners
        // means fewer matches, which errs toward speaking.
        let _ = EnumWindows(Some(collect), LPARAM(&mut pids as *mut Vec<i32> as isize));
    }
    pids
}

/// Is the cursor within a CENTERED horizontal zone of the window?
///
/// The counterpart of the macOS function of the same name — see it for why
/// the zone that OPENS the island is narrower than the island itself.
///
/// # Safety
/// `hwnd` must be a valid window handle.
/// Uncalled until Windows gets a global hover monitor (see `IDLE_HIT_WIDTH`).
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

    #[test]
    fn a_process_is_the_recorded_one_only_if_it_predates_the_record() {
        let me = std::process::id() as i32;
        // Recorded now: we were running before now, so we are the original.
        assert!(is_recorded_process(me, now_ms()));
        // Recorded before we existed: whoever held our pid then was someone
        // else, and we merely inherited the number.
        assert!(!is_recorded_process(me, 1));
        // Nothing to open: never the recorded process.
        for junk in [0, -1, i32::MAX] {
            assert!(!is_recorded_process(junk, now_ms()), "{junk}");
        }
    }

    #[test]
    fn filetime_converts_at_the_unix_epoch() {
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), Some(0));
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000 + 10_000), Some(1));
        assert_eq!(filetime_to_unix_ms(0), None);
    }

    #[test]
    fn window_owners_are_real_pids_and_exclude_a_windowless_test() {
        // A test harness owns no window; whatever else is on this desktop
        // (possibly nothing, on CI) must be a plausible pid.
        let owners = window_owner_pids();
        assert!(owners.iter().all(|pid| *pid > 0));
        assert!(!owners.contains(&(std::process::id() as i32)));
    }
}
