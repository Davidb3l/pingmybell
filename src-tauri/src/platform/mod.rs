//! Platform-specific window styling and screen probing (ARCHITECTURE.md §7).

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(windows)]
pub mod windows;

/// What the overlay needs to know about the primary display, in logical
/// points. Non-macOS values are sensible fixed defaults.
#[derive(Debug, Clone, Copy)]
pub struct ScreenProbe {
    /// Height of the unusable top strip: notch height on notched Macs, menu
    /// bar height otherwise, 0 on Windows.
    pub top_inset: f64,
    /// Physical notch width when present (macOS only).
    pub notch_width: Option<f64>,
}

pub fn probe_primary_screen() -> ScreenProbe {
    #[cfg(target_os = "macos")]
    {
        macos::probe_primary_screen()
    }
    #[cfg(not(target_os = "macos"))]
    {
        ScreenProbe {
            top_inset: 0.0,
            notch_width: None,
        }
    }
}
