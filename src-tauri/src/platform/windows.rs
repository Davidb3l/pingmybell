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
