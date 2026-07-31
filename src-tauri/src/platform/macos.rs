//! macOS overlay support (§7): raise the window above the menu bar, keep it
//! on every Space and over full-screen apps, and read notch geometry from
//! NSScreen. All values are in AppKit points (logical pixels).

use objc2::runtime::AnyObject;
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSEdgeInsets, NSPoint, NSRect};

use super::ScreenProbe;

/// NSStatusWindowLevel is 25; +1 floats above the menu bar/status items.
const OVERLAY_WINDOW_LEVEL: isize = 26;
/// NSWindowCollectionBehaviorCanJoinAllSpaces | ...FullScreenAuxiliary
const COLLECTION_BEHAVIOR: usize = (1 << 0) | (1 << 8);

/// # Safety
/// `ns_window` must be a valid NSWindow pointer (from tauri's `ns_window()`),
/// called on the main thread.
pub unsafe fn apply_overlay_styles(ns_window: *mut std::ffi::c_void) {
    let window: *mut AnyObject = ns_window.cast();
    let _: () = msg_send![window, setLevel: OVERLAY_WINDOW_LEVEL];
    let _: () = msg_send![window, setCollectionBehavior: COLLECTION_BEHAVIOR];
    let _: () = msg_send![window, setHidesOnDeactivate: false];
}

/// Is the mouse inside this window's frame (small margin)? Both
/// `NSEvent.mouseLocation` and `NSWindow.frame` are AppKit points with a
/// bottom-left origin — the SAME space, so no coordinate conversion can go
/// wrong (comparing tauri's cursor_position against outer_position mixes
/// spaces on retina displays and misfires).
///
/// # Safety
/// `ns_window` must be a valid NSWindow pointer.
pub unsafe fn cursor_in_window(ns_window: *mut std::ffi::c_void, margin: f64) -> bool {
    let window: *mut AnyObject = ns_window.cast();
    let frame: NSRect = msg_send![window, frame];
    let cursor: NSPoint = msg_send![class!(NSEvent), mouseLocation];
    cursor.x >= frame.origin.x - margin
        && cursor.x <= frame.origin.x + frame.size.width + margin
        && cursor.y >= frame.origin.y - margin
        && cursor.y <= frame.origin.y + frame.size.height + margin
}

pub fn probe_primary_screen() -> ScreenProbe {
    // Screens[0] is the primary display (menu bar / origin).
    unsafe {
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        let screen: *mut AnyObject = msg_send![screens, firstObject];
        if screen.is_null() {
            return ScreenProbe {
                top_inset: 24.0,
                notch_width: None,
            };
        }
        let frame: NSRect = msg_send![screen, frame];
        let visible: NSRect = msg_send![screen, visibleFrame];
        // visibleFrame excludes menu bar (top) and dock; bottom-left origin.
        let menu_bar_height =
            frame.size.height - (visible.origin.y - frame.origin.y + visible.size.height);
        // safeAreaInsets/auxiliary areas are macOS 12+; the bundle pins
        // minimumSystemVersion 13.0, but guard anyway so a bare cargo-run on
        // an older host degrades to the menu-bar pill instead of aborting on
        // an unrecognized selector.
        let has_safe_area: bool = msg_send![screen, respondsToSelector: sel!(safeAreaInsets)];
        if !has_safe_area {
            return ScreenProbe {
                top_inset: menu_bar_height.max(24.0),
                notch_width: None,
            };
        }
        let insets: NSEdgeInsets = msg_send![screen, safeAreaInsets];
        if insets.top > 0.0 {
            let left: NSRect = msg_send![screen, auxiliaryTopLeftArea];
            let right: NSRect = msg_send![screen, auxiliaryTopRightArea];
            let notch_width = frame.size.width - left.size.width - right.size.width;
            ScreenProbe {
                top_inset: insets.top,
                notch_width: (notch_width > 0.0).then_some(notch_width),
            }
        } else {
            ScreenProbe {
                top_inset: menu_bar_height.max(24.0),
                notch_width: None,
            }
        }
    }
}
