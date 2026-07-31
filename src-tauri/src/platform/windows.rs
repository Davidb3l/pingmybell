//! Windows overlay support (§7): non-activating tool window pinned topmost.
//! WS_EX_NOACTIVATE keeps keyboard focus with the foreground terminal;
//! WS_EX_TOOLWINDOW hides the window from Alt-Tab (AC-5.3).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

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
