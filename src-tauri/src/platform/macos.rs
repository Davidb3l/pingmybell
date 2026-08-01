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

/// The reply window must float ABOVE the island: it is opened from a card
/// that overlaps it, and a focused text box the user cannot see or click is
/// worse than no typed answers at all.
const REPLY_WINDOW_LEVEL: isize = OVERLAY_WINDOW_LEVEL + 1;

/// NSVisualEffectMaterialHUDWindow — a dark blur that suits the instrument
/// palette; `blendingMode` BehindWindow blurs what is BEHIND the window
/// (the question card and the desktop), which is the whole point.
const MATERIAL_HUD_WINDOW: isize = 13;
const BLENDING_MODE_BEHIND_WINDOW: isize = 0;
const VISUAL_EFFECT_STATE_ACTIVE: isize = 1;
/// NSViewWidthSizable | NSViewHeightSizable
const AUTORESIZE_FILL: usize = (1 << 1) | (1 << 4);
/// NSWindowBelow — put the effect view under the webview, not over it.
const WINDOW_BELOW: isize = -1;

/// # Safety
/// `ns_window` must be a valid NSWindow pointer (from tauri's `ns_window()`),
/// called on the main thread.
pub unsafe fn apply_reply_styles(ns_window: *mut std::ffi::c_void) {
    let window: *mut AnyObject = ns_window.cast();
    let _: () = msg_send![window, setLevel: REPLY_WINDOW_LEVEL];
    let _: () = msg_send![window, setCollectionBehavior: COLLECTION_BEHAVIOR];
    // A native shadow separates the reply window from the card it covers.
    let _: () = msg_send![window, setHasShadow: true];

    // Frosted backdrop: without it the reply window reads as a flat slab
    // sitting on the question card instead of a layer above it.
    let content: *mut AnyObject = msg_send![window, contentView];
    if content.is_null() {
        return;
    }
    // IDEMPOTENT: this runs on every reply-window open, and the window is
    // hidden between questions rather than destroyed. Adding a fresh effect
    // view each time stacked them in the view hierarchy — each with its own
    // backing store — and nothing ever released them. Bail if one is already
    // installed.
    let existing: *mut AnyObject = msg_send![content, subviews];
    if !existing.is_null() {
        let count: usize = msg_send![existing, count];
        let class = class!(NSVisualEffectView);
        for i in 0..count {
            let view: *mut AnyObject = msg_send![existing, objectAtIndex: i];
            if !view.is_null() {
                let is_effect: bool = msg_send![view, isKindOfClass: class];
                if is_effect {
                    return;
                }
            }
        }
    }

    let bounds: NSRect = msg_send![content, bounds];
    let effect: *mut AnyObject = msg_send![class!(NSVisualEffectView), alloc];
    let effect: *mut AnyObject = msg_send![effect, initWithFrame: bounds];
    if effect.is_null() {
        return;
    }
    let _: () = msg_send![effect, setMaterial: MATERIAL_HUD_WINDOW];
    let _: () = msg_send![effect, setBlendingMode: BLENDING_MODE_BEHIND_WINDOW];
    let _: () = msg_send![effect, setState: VISUAL_EFFECT_STATE_ACTIVE];
    let _: () = msg_send![effect, setAutoresizingMask: AUTORESIZE_FILL];
    // Round the blur to match the shell's radius, or the frost shows as a
    // square behind the rounded panel.
    let _: () = msg_send![effect, setWantsLayer: true];
    let layer: *mut AnyObject = msg_send![effect, layer];
    if !layer.is_null() {
        let _: () = msg_send![layer, setCornerRadius: 14.0f64];
        let _: () = msg_send![layer, setMasksToBounds: true];
    }
    let _: () = msg_send![content, addSubview: effect, positioned: WINDOW_BELOW, relativeTo: std::ptr::null::<AnyObject>()];
}

/// # Safety
/// `ns_window` must be a valid NSWindow pointer (from tauri's `ns_window()`),
/// called on the main thread.
pub unsafe fn apply_overlay_styles(ns_window: *mut std::ffi::c_void) {
    let window: *mut AnyObject = ns_window.cast();
    let _: () = msg_send![window, setLevel: OVERLAY_WINDOW_LEVEL];
    let _: () = msg_send![window, setCollectionBehavior: COLLECTION_BEHAVIOR];
    let _: () = msg_send![window, setHidesOnDeactivate: false];
}

/// Activate the GUI application owning `pid`, if that pid is a registered
/// application (NSRunningApplication returns nil for plain CLI processes).
/// Needs no Automation/Accessibility permission. Returns false when the pid
/// is not an app or activation was refused.
pub fn activate_app_with_pid(pid: i32) -> bool {
    unsafe {
        let app: *mut AnyObject = msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ];
        if app.is_null() {
            return false;
        }
        // NSApplicationActivateIgnoringOtherApps
        let ok: bool = msg_send![app, activateWithOptions: 2usize];
        ok
    }
}

/// Activate the frontmost running app with this bundle identifier.
///
/// The fallback for jump-to-session when the recorded process is gone: each
/// agent session is its own short-lived process, so by the time the user
/// clicks a finished session its pid is usually dead and the ppid walk finds
/// nothing to activate. The hosting APP is still running, and bringing it
/// forward is what the user actually asked for.
///
/// Returns false when no such app is running (e.g. the agent ran in a
/// terminal, or the app was quit) so the caller can keep degrading.
pub fn activate_app_with_bundle_id(bundle_id: &str) -> bool {
    unsafe {
        let ns_id = objc2_foundation::NSString::from_str(bundle_id);
        let apps: *mut AnyObject = msg_send![
            class!(NSRunningApplication),
            runningApplicationsWithBundleIdentifier: &*ns_id
        ];
        if apps.is_null() {
            return false;
        }
        let count: usize = msg_send![apps, count];
        for i in 0..count {
            let app: *mut AnyObject = msg_send![apps, objectAtIndex: i];
            if app.is_null() {
                continue;
            }
            // NSApplicationActivateAllWindows | ...IgnoringOtherApps
            let ok: bool = msg_send![app, activateWithOptions: 3usize];
            if ok {
                return true;
            }
        }
        false
    }
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

/// Install system mouse-moved monitors that invoke `on_move` for every
/// cursor movement. A window that can never become key (our overlay) gets no
/// hover events from AppKit, so hover-entry must be observed at the OS
/// level. The GLOBAL monitor covers movement over other apps; the LOCAL one
/// covers movement over our own windows. Mouse monitoring needs no special
/// permissions (unlike keyboard). Event-driven — no polling (AC-5.5).
///
/// Must be called on the main thread; the monitors live for the app's
/// lifetime (intentionally leaked).
pub fn install_mouse_moved_monitor(on_move: impl Fn() + Clone + 'static) {
    use block2::RcBlock;

    const MOUSE_MOVED_MASK: u64 = 1 << 5; // NSEventMaskMouseMoved

    unsafe {
        let global_cb = on_move.clone();
        let global_block = RcBlock::new(move |_event: *mut AnyObject| {
            global_cb();
        });
        let global_monitor: *mut AnyObject = msg_send![
            class!(NSEvent),
            addGlobalMonitorForEventsMatchingMask: MOUSE_MOVED_MASK,
            handler: &*global_block,
        ];

        let local_block = RcBlock::new(move |event: *mut AnyObject| -> *mut AnyObject {
            on_move();
            event // pass the event through untouched
        });
        let local_monitor: *mut AnyObject = msg_send![
            class!(NSEvent),
            addLocalMonitorForEventsMatchingMask: MOUSE_MOVED_MASK,
            handler: &*local_block,
        ];

        // App-lifetime observers: keep the blocks and monitor tokens alive.
        std::mem::forget(global_block);
        std::mem::forget(local_block);
        let _ = global_monitor;
        let _ = local_monitor;
    }
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
