//! Pure state, time formatting and geometry for the video control bar.
//!
//! The bar itself is a separate Slint window owned by `tv-native-player` (see
//! `ui/player_overlay.slint`), so no video frame is ever converted. This module
//! is feature-independent and has no Slint, GStreamer or Wayland dependency: it
//! only maps positions to labels, pointer coordinates to seek targets, keys to
//! bounded actions and inactivity to visibility. That keeps every policy rule
//! testable in the headless gate.
//!
//! Times are monotonic durations supplied by the caller. Nothing here reads the
//! clock, spawns a task or allocates more than the formatted label.

use std::collections::BTreeSet;
use std::time::Duration;

use super::{MAX_POSITION_MS, controls};

/// Time without pointer or key activity after which the bar hides itself.
pub const AUTO_HIDE: Duration = Duration::from_secs(3);

/// Key step used by Left/Right in the overlay.
pub const SEEK_STEP_MS: u64 = controls::SEEK_STEP_MS;

/// Minimum logical height of an interactive control, in pixels.
///
/// Slint uses the same value when laying out the seek track and the buttons.
pub const MIN_HIT_HEIGHT: f32 = 48.0;

/// Label used when the duration is not known yet.
pub const UNKNOWN_TIME: &str = "--:--";

/// Largest number of simultaneously held keys tracked for repeat rejection.
///
/// A keyboard cannot legitimately hold more keys than this for the overlay.
const MAX_HELD_KEYS: usize = 8;

/// One bounded action produced by the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAction {
    /// Stop playback and exit the child.
    Stop,
    /// Toggle between playing and paused.
    TogglePause,
    /// Seek backwards by [`SEEK_STEP_MS`].
    SeekBackward,
    /// Seek forwards by [`SEEK_STEP_MS`].
    SeekForward,
    /// Seek to the start of the media.
    SeekToStart,
    /// Seek to the end of the media, if the duration is known.
    SeekToEnd,
}

/// Format a millisecond position as `M:SS` or `H:MM:SS`.
///
/// Inputs above 24 hours are not clamped: every caller already bounds
/// positions with [`MAX_POSITION_MS`], and formatting must stay a pure
/// function of its argument.
#[must_use]
pub fn format_time(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Remaining time label, or [`UNKNOWN_TIME`] when the duration is unknown.
#[must_use]
pub fn remaining_time(position_ms: u64, duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(duration) => format!("-{}", format_time(duration.saturating_sub(position_ms))),
        None => UNKNOWN_TIME.to_string(),
    }
}

/// Played fraction in `0.0..=1.0`, or zero when the duration is unknown.
#[must_use]
pub fn progress(position_ms: u64, duration_ms: Option<u64>) -> f32 {
    match duration_ms {
        Some(duration) if duration > 0 => {
            (position_ms.min(duration) as f64 / duration as f64) as f32
        }
        _ => 0.0,
    }
}

/// Map a pointer x coordinate inside a seek track to `0.0..=1.0`.
///
/// A zero, negative or non-finite width yields zero rather than a NaN, and a
/// pointer outside the track is clamped instead of wrapping.
#[must_use]
pub fn fraction_from_x(x: f32, width: f32) -> f32 {
    if !x.is_finite() || !width.is_finite() || width <= 0.0 {
        return 0.0;
    }
    (x / width).clamp(0.0, 1.0)
}

/// Absolute seek position for a `0.0..=1.0` fraction of a known duration.
#[must_use]
pub fn position_from_fraction(fraction: f32, duration_ms: u64) -> u64 {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let position = (f64::from(fraction) * duration_ms as f64).round() as u64;
    position.min(duration_ms).min(MAX_POSITION_MS)
}

/// Absolute seek target for a pointer x coordinate, if the duration is known.
#[must_use]
pub fn position_from_x(x: f32, width: f32, duration_ms: Option<u64>) -> Option<u64> {
    let duration = duration_ms?;
    Some(position_from_fraction(fraction_from_x(x, width), duration))
}

/// Seek target for a key action, or `None` when the action does not seek.
///
/// Forward/backward steps reuse [`controls::seek_target`] so the keyboard and
/// the evdev fallback cannot drift apart. `End` is a no-op without a duration.
#[must_use]
pub fn seek_target(
    action: OverlayAction,
    position_ms: u64,
    duration_ms: Option<u64>,
) -> Option<u64> {
    match action {
        OverlayAction::SeekBackward => Some(controls::seek_target(position_ms, duration_ms, false)),
        OverlayAction::SeekForward => Some(controls::seek_target(position_ms, duration_ms, true)),
        OverlayAction::SeekToStart => Some(0),
        OverlayAction::SeekToEnd => duration_ms.map(|duration| duration.min(MAX_POSITION_MS)),
        OverlayAction::Stop | OverlayAction::TogglePause => None,
    }
}

/// Map one Slint key-event text to an overlay action.
///
/// Slint reports special keys as the private characters in its `Key`
/// namespace; ordinary characters are themselves. Only single-character
/// events are accepted, so composed input can never be interpreted as a
/// command.
#[must_use]
pub fn map_key(text: &str) -> Option<OverlayAction> {
    let mut characters = text.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    map_key_char(character)
}

/// Map one key character; the character values match Slint's `Key` namespace.
#[must_use]
pub fn map_key_char(character: char) -> Option<OverlayAction> {
    Some(match character {
        // Key.Escape
        '\u{001b}' => OverlayAction::Stop,
        // Key.Space
        '\u{0020}' => OverlayAction::TogglePause,
        // Key.LeftArrow / Key.RightArrow
        '\u{f702}' => OverlayAction::SeekBackward,
        '\u{f703}' => OverlayAction::SeekForward,
        // Key.Home / Key.End
        '\u{f729}' => OverlayAction::SeekToStart,
        '\u{f72b}' => OverlayAction::SeekToEnd,
        _ => return None,
    })
}

/// Whether the overlay window is enabled for this build.
///
/// The compile-time `player-overlay` feature is the real switch. At runtime
/// `TV_PLAYER_OVERLAY_WINDOW` can turn the window off (`off`, `0`, `false`,
/// `no`) so the evdev-only path remains available without rebuilding. The
/// separate `TV_PLAYER_OVERLAY` variable keeps its old meaning: the opt-in
/// `textoverlay` hint, which stays off by default.
#[must_use]
pub fn window_enabled_from_env(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        )
    })
}

/// Pure overlay state: playback snapshot, visibility and held-key filtering.
#[derive(Debug, Clone)]
pub struct OverlayState {
    position_ms: u64,
    duration_ms: Option<u64>,
    paused: bool,
    visible: bool,
    active: bool,
    last_activity: Duration,
    held: BTreeSet<char>,
}

impl OverlayState {
    /// Start visible for a short grace period, like a freshly opened player.
    #[must_use]
    pub fn new(now: Duration) -> Self {
        Self {
            position_ms: 0,
            duration_ms: None,
            paused: false,
            visible: true,
            active: true,
            last_activity: now,
            held: BTreeSet::new(),
        }
    }

    /// Current position in milliseconds.
    #[must_use]
    pub fn position_ms(&self) -> u64 {
        self.position_ms
    }

    /// Current duration, if known.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// Whether playback is paused.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Whether the control bar should be painted.
    #[must_use]
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Whether the overlay is still associated with active playback.
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }

    /// Update the playback snapshot. Positions past the duration are clamped.
    pub fn update_position(&mut self, position_ms: u64, duration_ms: Option<u64>) {
        self.duration_ms = duration_ms;
        self.position_ms = duration_ms.map_or(position_ms, |duration| position_ms.min(duration));
    }

    /// Record pointer or key activity, which shows the bar.
    pub fn note_activity(&mut self, now: Duration) {
        if !self.active {
            return;
        }
        self.visible = true;
        self.last_activity = now;
    }

    /// Apply a pause transition. Both pausing and resuming reveal the bar.
    pub fn set_paused(&mut self, paused: bool, now: Duration) {
        if self.paused != paused {
            self.visible = true;
            self.last_activity = now;
        }
        self.paused = paused;
    }

    /// Hide the bar after [`AUTO_HIDE`] of inactivity while playing.
    ///
    /// Returns `true` when the visibility changed, so the caller only touches
    /// the UI property when needed. Paused playback keeps the bar on screen.
    pub fn tick(&mut self, now: Duration) -> bool {
        if !self.active || self.paused || !self.visible {
            return false;
        }
        if now.saturating_sub(self.last_activity) >= AUTO_HIDE {
            self.visible = false;
            true
        } else {
            false
        }
    }

    /// Stop showing controls; called on stop, end of stream and errors.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.visible = false;
    }

    /// Elapsed-time label for the current position.
    #[must_use]
    pub fn elapsed_label(&self) -> String {
        format_time(self.position_ms)
    }

    /// Remaining-time label for the current position.
    #[must_use]
    pub fn remaining_label(&self) -> String {
        remaining_time(self.position_ms, self.duration_ms)
    }

    /// Played fraction for the progress track.
    #[must_use]
    pub fn progress(&self) -> f32 {
        progress(self.position_ms, self.duration_ms)
    }

    /// Seek target for a pointer fraction, if the duration is known.
    #[must_use]
    pub fn fraction_target(&self, fraction: f32) -> Option<u64> {
        self.duration_ms
            .map(|duration| position_from_fraction(fraction, duration))
    }

    /// Seek target for a key action.
    #[must_use]
    pub fn action_target(&self, action: OverlayAction) -> Option<u64> {
        seek_target(action, self.position_ms, self.duration_ms)
    }

    /// Apply one key press and return the action it requests.
    ///
    /// A repeated press of a key that is already held returns `None`; the
    /// matching [`Self::key_release`] clears the key. This mirrors the evdev
    /// reader, which also ignores kernel repeats.
    pub fn key_press(&mut self, text: &str, now: Duration) -> Option<OverlayAction> {
        let mut characters = text.chars();
        let character = characters.next()?;
        if characters.next().is_some() {
            return None;
        }
        self.note_activity(now);
        if !self.held.insert(character) {
            return None;
        }
        if self.held.len() > MAX_HELD_KEYS {
            self.held.clear();
            self.held.insert(character);
        }
        map_key_char(character)
    }

    /// Forget a released key so a later press is handled again.
    pub fn key_release(&mut self, text: &str) {
        let mut characters = text.chars();
        if let Some(character) = characters.next()
            && characters.next().is_none()
        {
            self.held.remove(&character);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    #[test]
    fn time_formatting_switches_at_one_hour() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(999), "0:00");
        assert_eq!(format_time(61_000), "1:01");
        assert_eq!(format_time(3_599_000), "59:59");
        assert_eq!(format_time(3_600_000), "1:00:00");
        assert_eq!(format_time(7_384_000), "2:03:04");
    }

    #[test]
    fn remaining_time_is_bounded_and_never_negative() {
        assert_eq!(remaining_time(10_000, Some(70_000)), "-1:00");
        assert_eq!(remaining_time(70_000, Some(70_000)), "-0:00");
        assert_eq!(remaining_time(90_000, Some(70_000)), "-0:00");
        assert_eq!(remaining_time(0, None), UNKNOWN_TIME);
    }

    #[test]
    fn visibility_auto_hides_only_while_playing() {
        let mut state = OverlayState::new(at(0));
        assert!(state.visible());
        assert!(!state.tick(at(2_999)));
        assert!(state.tick(at(3_000)));
        assert!(!state.visible());
        assert!(!state.tick(at(9_000)));

        state.note_activity(at(9_500));
        assert!(state.visible());
        state.set_paused(true, at(9_600));
        assert!(!state.tick(at(60_000)));
        assert!(state.visible());

        state.set_paused(false, at(60_100));
        assert!(!state.tick(at(62_000)));
        assert!(state.tick(at(63_100)));
    }

    #[test]
    fn deactivation_hides_and_ignores_activity() {
        let mut state = OverlayState::new(at(0));
        state.deactivate();
        assert!(!state.visible());
        state.note_activity(at(10));
        assert!(!state.visible());
        assert!(!state.tick(at(100_000)));
        assert!(!state.active());
    }

    #[test]
    fn key_mapping_covers_controls_and_ignores_unknown_keys() {
        assert_eq!(map_key("\u{001b}"), Some(OverlayAction::Stop));
        assert_eq!(map_key(" "), Some(OverlayAction::TogglePause));
        assert_eq!(map_key("\u{f702}"), Some(OverlayAction::SeekBackward));
        assert_eq!(map_key("\u{f703}"), Some(OverlayAction::SeekForward));
        assert_eq!(map_key("\u{f729}"), Some(OverlayAction::SeekToStart));
        assert_eq!(map_key("\u{f72b}"), Some(OverlayAction::SeekToEnd));
        assert_eq!(map_key("a"), None);
        assert_eq!(map_key("ab"), None);
        assert_eq!(map_key(""), None);
    }

    #[test]
    fn held_keys_do_not_repeat() {
        let mut state = OverlayState::new(at(0));
        assert_eq!(
            state.key_press("\u{f703}", at(10)),
            Some(OverlayAction::SeekForward)
        );
        assert_eq!(state.key_press("\u{f703}", at(20)), None);
        state.key_release("\u{f703}");
        assert_eq!(
            state.key_press("\u{f703}", at(30)),
            Some(OverlayAction::SeekForward)
        );
    }

    #[test]
    fn seek_targets_are_clamped_to_the_media() {
        assert_eq!(position_from_fraction(0.0, 120_000), 0);
        assert_eq!(position_from_fraction(0.5, 120_000), 60_000);
        assert_eq!(position_from_fraction(1.0, 120_000), 120_000);
        assert_eq!(position_from_fraction(-1.0, 120_000), 0);
        assert_eq!(position_from_fraction(f32::NAN, 120_000), 0);
        assert_eq!(position_from_fraction(2.0, 120_000), 120_000);
        assert_eq!(position_from_fraction(0.5, 0), 0);
        assert_eq!(position_from_fraction(1.0, u64::MAX), MAX_POSITION_MS);
    }

    #[test]
    fn pointer_mapping_clamps_and_survives_bad_widths() {
        assert_eq!(fraction_from_x(0.0, 100.0), 0.0);
        assert_eq!(fraction_from_x(50.0, 100.0), 0.5);
        assert_eq!(fraction_from_x(150.0, 100.0), 1.0);
        assert_eq!(fraction_from_x(-5.0, 100.0), 0.0);
        assert_eq!(fraction_from_x(10.0, 0.0), 0.0);
        assert_eq!(fraction_from_x(10.0, -4.0), 0.0);
        assert_eq!(fraction_from_x(f32::NAN, 100.0), 0.0);
        assert_eq!(position_from_x(25.0, 100.0, Some(80_000)), Some(20_000));
        assert_eq!(position_from_x(25.0, 100.0, None), None);
    }

    #[test]
    fn key_seek_targets_match_the_evdev_steps() {
        assert_eq!(
            seek_target(OverlayAction::SeekForward, 10_000, Some(60_000)),
            Some(20_000)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekBackward, 5_000, Some(60_000)),
            Some(0)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekBackward, 5_000, None),
            Some(0)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekToStart, 42_000, None),
            Some(0)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekToEnd, 0, Some(90_000)),
            Some(90_000)
        );
        assert_eq!(seek_target(OverlayAction::SeekToEnd, 0, None), None);
        assert_eq!(seek_target(OverlayAction::Stop, 0, Some(1)), None);
        assert_eq!(seek_target(OverlayAction::TogglePause, 0, Some(1)), None);
    }

    #[test]
    fn window_environment_defaults_on_and_accepts_off() {
        assert!(window_enabled_from_env(None));
        assert!(window_enabled_from_env(Some("on")));
        assert!(window_enabled_from_env(Some(" yes ")));
        assert!(!window_enabled_from_env(Some("off")));
        assert!(!window_enabled_from_env(Some("0")));
        assert!(!window_enabled_from_env(Some("FALSE")));
        assert!(!window_enabled_from_env(Some("no")));
    }
}
