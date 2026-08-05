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

/// Plays the built-in chimes (§11.3) through whatever the OS already has.
///
/// No audio crate. Both platforms can play a WAV out of a memory buffer with
/// an API the app is already linked against — `NSSound` on macOS, `PlaySound`
/// with `SND_MEMORY` on Windows — and `PlaySound` accepts WAV and nothing
/// else, which is precisely why the chimes ship as WAV rather than as
/// something compressed. Trading ~1 MB of samples for a decoder crate plus
/// its dependency tree was the worse deal; this way costs neither.
///
/// The player HOLDS the sound that is currently playing. Both APIs are
/// asynchronous and both stop the moment their backing object goes away: the
/// `NSSound` if it is released, the buffer if it is freed under `SND_ASYNC`.
/// Keeping the most recent one is enough: a new chime supersedes the last,
/// which is the right behaviour anyway when two arrive close together.
#[derive(Default)]
pub struct Player {
    #[cfg(target_os = "macos")]
    current: Option<objc2::rc::Retained<objc2::runtime::AnyObject>>,
    #[cfg(windows)]
    current: Option<Vec<u8>>,
}

impl Player {
    /// Play one 16-bit PCM WAV. Best-effort: any failure is silence, never a
    /// panic and never an error the caller has to handle.
    #[allow(unused_variables)]
    pub fn play(&mut self, wav: &[u8]) {
        #[cfg(target_os = "macos")]
        {
            self.current = macos::play_wav(wav);
        }
        #[cfg(windows)]
        {
            self.current = windows::play_wav(wav);
        }
        // Both platform paths return None only when the OS refused the sound.
        // Saying so matters: a chime is a NEGATIVE signal — you notice its
        // absence, not its presence — so a silent failure here looks exactly
        // like "nothing happened", which is the message it was replacing.
        #[cfg(any(target_os = "macos", windows))]
        if self.current.is_none() {
            log::warn!("chime: the system declined to play it");
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            log::debug!("chime: no playback on this platform");
        }
    }
}
