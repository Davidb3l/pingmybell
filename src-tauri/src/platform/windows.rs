//! Windows overlay support (§7): non-activating tool window pinned topmost.
//! WS_EX_NOACTIVATE keeps keyboard focus with the foreground terminal;
//! WS_EX_TOOLWINDOW hides the window from Alt-Tab (AC-5.3).

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE,
    HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
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
