//! Pure input abstraction shared by the local keyboard, HDMI-CEC and remote
//! callers.
//!
//! The module is always compiled and uses no platform or UI types. It owns
//! the provenance boundary: [`LocalOrigin`] can only be built from
//! [`InputOrigin::LocalKeyboard`] or [`InputOrigin::LocalCec`], so a remote
//! event cannot be turned into a local key or a physical approval downstream.
//! Remote wire actions travel on their own [`crate::ui::UiInput::RemoteAction`]
//! path and never pass through this router.
//!
//! Events are bounded twice: the CEC reader caps a line at
//! [`cec::MAX_CEC_LINE`], and [`InputQueue`] drops input rather than blocking
//! when the consumer falls behind. Holding a key is debounced by
//! [`RepeatGate`]; long press is available through [`LongPress`] for callers
//! that need it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use thiserror::Error;
use uuid::Uuid;

use crate::model::{Event, InputOrigin};

pub mod cec;

pub use cec::{
    Backoff, CecBridge, CecConfig, CecDisconnect, CecError, CecParse, CecParser, CecStatus,
    CecUnavailable, ShutdownOutcome,
};

/// Minimum interval between accepted repeats of a held key.
pub const REPEAT_INTERVAL: Duration = Duration::from_millis(120);
/// Default threshold for [`LongPress`].
pub const LONG_PRESS_INTERVAL: Duration = Duration::from_millis(600);
/// Default capacity of the bounded physical-input queue.
pub const DEFAULT_INPUT_CAPACITY: usize = 32;
/// Largest accepted physical-input queue capacity.
pub const MAX_INPUT_CAPACITY: usize = 256;

/// Number of distinct repeat slots, one per repeatable key.
const REPEAT_SLOTS: usize = 12;

/// A key understood by the input layer.
///
/// The first group is produced by both the keyboard and CEC. `Char` and
/// `Backspace` are local-keyboard-only: search text is never synthesized from
/// a remote or CEC event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Select/OK/Enter.
    Select,
    /// Back/Escape.
    Back,
    /// Home.
    Home,
    /// Play/pause toggle.
    PlayPause,
    /// Volume up.
    VolumeUp,
    /// Volume down.
    VolumeDown,
    /// Mute toggle.
    Mute,
    /// A printable character typed on the local keyboard.
    Char(char),
    /// Backspace typed on the local keyboard.
    Backspace,
}

impl Key {
    /// Repeat-throttle slot for this key, or `None` when it is not repeatable.
    #[must_use]
    pub const fn repeat_slot(self) -> Option<usize> {
        match self {
            Self::Up => Some(0),
            Self::Down => Some(1),
            Self::Left => Some(2),
            Self::Right => Some(3),
            Self::Select => Some(4),
            Self::Back => Some(5),
            Self::Home => Some(6),
            Self::PlayPause => Some(7),
            Self::VolumeUp => Some(8),
            Self::VolumeDown => Some(9),
            Self::Mute => Some(10),
            Self::Backspace => Some(11),
            Self::Char(_) => None,
        }
    }

    /// Whether this key belongs to the volume/mute group.
    #[must_use]
    pub const fn is_volume(self) -> bool {
        matches!(self, Self::VolumeUp | Self::VolumeDown | Self::Mute)
    }
}

/// One physical input event with its provenance preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    /// Where the event was produced.
    pub origin: InputOrigin,
    /// Logical key.
    pub key: Key,
    /// Whether the source reported this as a held/repeated press.
    pub repeat: bool,
}

impl InputEvent {
    /// Construct a local keyboard event.
    #[must_use]
    pub const fn keyboard(key: Key) -> Self {
        Self {
            origin: InputOrigin::LocalKeyboard,
            key,
            repeat: false,
        }
    }

    /// Construct a local CEC event.
    #[must_use]
    pub const fn cec(key: Key) -> Self {
        Self {
            origin: InputOrigin::LocalCec,
            key,
            repeat: false,
        }
    }
}

/// The two provenances that may become local input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOrigin {
    /// A key pressed on the locally attached keyboard.
    Keyboard,
    /// A key received over the local HDMI-CEC adapter.
    Cec,
}

impl LocalOrigin {
    /// The protocol/reducer spelling of this origin.
    #[must_use]
    pub const fn input_origin(self) -> InputOrigin {
        match self {
            Self::Keyboard => InputOrigin::LocalKeyboard,
            Self::Cec => InputOrigin::LocalCec,
        }
    }
}

impl From<LocalOrigin> for InputOrigin {
    fn from(origin: LocalOrigin) -> Self {
        origin.input_origin()
    }
}

/// A remote origin cannot become local input.
impl TryFrom<InputOrigin> for LocalOrigin {
    type Error = InputReject;

    fn try_from(origin: InputOrigin) -> Result<Self, Self::Error> {
        match origin {
            InputOrigin::LocalKeyboard => Ok(Self::Keyboard),
            InputOrigin::LocalCec => Ok(Self::Cec),
            InputOrigin::Remote => Err(InputReject::RemoteNotLocal),
        }
    }
}

/// A physical confirmation constructed only from a local origin.
///
/// This is defence in depth in front of the reducer, which also rejects a
/// remote [`crate::model::Event::PhysicalApproval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalApproval {
    origin: LocalOrigin,
    request_id: Uuid,
}

impl PhysicalApproval {
    /// Build an approval, refusing any remote origin.
    pub fn new(origin: InputOrigin, request_id: Uuid) -> Result<Self, InputReject> {
        let origin = LocalOrigin::try_from(origin)?;
        if request_id.is_nil() {
            return Err(InputReject::InvalidRequest);
        }
        Ok(Self { origin, request_id })
    }

    /// Proven local origin.
    #[must_use]
    pub const fn origin(self) -> LocalOrigin {
        self.origin
    }

    /// Pairing request being approved.
    #[must_use]
    pub const fn request_id(self) -> Uuid {
        self.request_id
    }

    /// Convert into the reducer event.
    #[must_use]
    pub fn into_event(self) -> Event {
        Event::physical_approval(self.origin.input_origin(), self.request_id)
    }
}

/// Why an input event could not become local input.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum InputReject {
    /// The event arrived over the remote command channel.
    #[error("remote input cannot act as a local key")]
    RemoteNotLocal,
    /// A physical approval carried a nil request identifier.
    #[error("pairing request identifier must not be nil")]
    InvalidRequest,
}

/// Route decision for one physical event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// A local UI key. The origin is proven local.
    Ui {
        /// Logical key for the view model.
        key: Key,
        /// Proven local origin.
        origin: LocalOrigin,
        /// Whether the source reported a held press.
        repeat: bool,
    },
    /// A local volume/mute key. This build has no audio sink, so the consumer
    /// may ignore it without blocking the input path.
    Volume {
        /// Volume key.
        key: Key,
        /// Proven local origin.
        origin: LocalOrigin,
        /// Whether the source reported a held press.
        repeat: bool,
    },
    /// The event was refused before reaching the UI.
    Rejected(InputReject),
}

/// Provenance-preserving router for physical events.
#[derive(Debug, Clone, Copy, Default)]
pub struct Router;

impl Router {
    /// Route one physical event.
    #[must_use]
    pub fn route(event: InputEvent) -> Route {
        let origin = match LocalOrigin::try_from(event.origin) {
            Ok(origin) => origin,
            Err(reason) => return Route::Rejected(reason),
        };
        if event.key.is_volume() {
            Route::Volume {
                key: event.key,
                origin,
                repeat: event.repeat,
            }
        } else {
            Route::Ui {
                key: event.key,
                origin,
                repeat: event.repeat,
            }
        }
    }
}

/// Per-key repeat gate shared by the keyboard and CEC paths.
#[derive(Debug, Clone)]
pub struct RepeatGate {
    interval: Duration,
    last: [Option<Duration>; REPEAT_SLOTS],
}

impl RepeatGate {
    /// Create a gate with an explicit minimum repeat interval.
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: [None; REPEAT_SLOTS],
        }
    }

    /// Configured repeat interval.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Accept the event if its slot has not repeated inside the interval.
    ///
    /// Unknown slots are always accepted.
    pub fn accept(&mut self, slot: usize, now: Duration) -> bool {
        let Some(last) = self.last.get_mut(slot) else {
            return true;
        };
        if let Some(previous) = *last
            && now.saturating_sub(previous) < self.interval
        {
            return false;
        }
        *last = Some(now);
        true
    }

    /// Forget all repeat history, for example after a CEC reconnect.
    pub fn reset(&mut self) {
        self.last = [None; REPEAT_SLOTS];
    }
}

impl Default for RepeatGate {
    fn default() -> Self {
        Self::new(REPEAT_INTERVAL)
    }
}

/// Optional long-press detector.
///
/// The caller drives it with monotonic timestamps; it fires at most once per
/// held key and re-arms after [`Self::release`] or a new key.
#[derive(Debug, Clone)]
pub struct LongPress {
    threshold: Duration,
    key: Option<Key>,
    started: Duration,
    fired: bool,
}

impl LongPress {
    /// Create a detector with an explicit threshold.
    #[must_use]
    pub const fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            key: None,
            started: Duration::ZERO,
            fired: false,
        }
    }

    /// Configured threshold.
    #[must_use]
    pub const fn threshold(&self) -> Duration {
        self.threshold
    }

    /// Feed a press. Returns `true` exactly once when the threshold elapses.
    pub fn press(&mut self, key: Key, now: Duration) -> bool {
        if self.key != Some(key) {
            self.key = Some(key);
            self.started = now;
            self.fired = false;
            return false;
        }
        if !self.fired && now.saturating_sub(self.started) >= self.threshold {
            self.fired = true;
            return true;
        }
        false
    }

    /// Release the key, re-arming the detector.
    pub fn release(&mut self, key: Key) {
        if self.key == Some(key) {
            self.key = None;
            self.fired = false;
        }
    }

    /// Currently held key, if any.
    #[must_use]
    pub const fn held(&self) -> Option<Key> {
        self.key
    }
}

impl Default for LongPress {
    fn default() -> Self {
        Self::new(LONG_PRESS_INTERVAL)
    }
}

/// Errors from the bounded input queue.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum InputError {
    /// Capacity must be within `1..=MAX_INPUT_CAPACITY`.
    #[error("input queue capacity must be within 1..={MAX_INPUT_CAPACITY}")]
    InvalidCapacity,
    /// The consumer is behind; the event was dropped.
    #[error("input queue is full")]
    QueueFull,
}

/// Bounded, non-blocking queue of physical input events.
#[derive(Debug)]
pub struct InputQueue {
    capacity: usize,
    events: Mutex<VecDeque<InputEvent>>,
    dropped: AtomicU64,
}

impl InputQueue {
    /// Create a queue with the given capacity.
    pub fn new(capacity: usize) -> Result<Self, InputError> {
        if capacity == 0 || capacity > MAX_INPUT_CAPACITY {
            return Err(InputError::InvalidCapacity);
        }
        Ok(Self {
            capacity,
            events: Mutex::new(VecDeque::with_capacity(capacity)),
            dropped: AtomicU64::new(0),
        })
    }

    /// Push without blocking, returning [`InputError::QueueFull`] when full.
    pub fn try_push(&self, event: InputEvent) -> Result<(), InputError> {
        let mut events = lock(&self.events);
        if events.len() >= self.capacity {
            drop(events);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(InputError::QueueFull);
        }
        events.push_back(event);
        Ok(())
    }

    /// Pop the oldest event.
    pub fn pop(&self) -> Option<InputEvent> {
        lock(&self.events).pop_front()
    }

    /// Number of queued events.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.events).len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        lock(&self.events).is_empty()
    }

    /// Configured capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total events dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Translate a `cec-ctl` `ui-cmd` line or legacy `UI_CMD_*` token into an
/// input key.
///
/// `cec-ctl` 1.30.1 prints the real CEC names, for example
/// `ui-cmd: up (0x01)`, `ui-cmd: pause (0x46)` and
/// `ui-cmd: device-root-menu (0x09)`. Older fixtures and logs may print
/// `ui_cmd: UI_CMD_UP (0x01)`; both spellings are accepted. A known command
/// maps by name; otherwise the parenthesized `ui-cmd` opcode is used. Unknown
/// commands return `None` and never become a guess at a local key.
#[must_use]
pub fn key_from_cec_ui_command(text: &str) -> Option<Key> {
    let (value, has_ui_command) = if let Some((_, value)) = text.split_once("ui-cmd:") {
        (value, true)
    } else if let Some((_, value)) = text.split_once("ui_cmd:") {
        (value, true)
    } else {
        (text, text.contains("UI_CMD_"))
    };
    let value = value.trim();
    let token = value.split_whitespace().next()?;
    let name = token.trim_matches(|character: char| {
        !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
    });
    let name = name.strip_prefix("UI_CMD_").unwrap_or(name);
    let normalized = name.to_ascii_lowercase().replace('_', "-");
    if let Some(key) = key_from_ui_command_name(normalized.as_str()) {
        return Some(key);
    }
    if !has_ui_command {
        return None;
    }
    key_from_ui_command_opcode(parse_ui_command_opcode(value)?)
}

fn key_from_ui_command_name(name: &str) -> Option<Key> {
    Some(match name {
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "select" | "enter" => Key::Select,
        "back" | "exit" => Key::Back,
        "device-root-menu" | "media-top-menu" | "contents-menu" => Key::Home,
        "play" | "pause" | "play-function" | "pause-play-function" => Key::PlayPause,
        "volume-up" => Key::VolumeUp,
        "volume-down" => Key::VolumeDown,
        "mute" | "mute-function" => Key::Mute,
        _ => return None,
    })
}

fn parse_ui_command_opcode(value: &str) -> Option<u8> {
    let start = value.find("(0x")? + 3;
    let rest = &value[start..];
    let end = rest.find(')')?;
    let digits = &rest[..end];
    if digits.len() != 2 {
        return None;
    }
    u8::from_str_radix(digits, 16).ok()
}

fn key_from_ui_command_opcode(opcode: u8) -> Option<Key> {
    Some(match opcode {
        0x00 => Key::Select,
        0x01 => Key::Up,
        0x02 => Key::Down,
        0x03 => Key::Left,
        0x04 => Key::Right,
        0x09 | 0x0b | 0x10 => Key::Home,
        0x0d => Key::Back,
        0x2b => Key::Select,
        0x41 => Key::VolumeUp,
        0x42 => Key::VolumeDown,
        0x43 => Key::Mute,
        0x44 | 0x46 | 0x60 | 0x61 => Key::PlayPause,
        _ => return None,
    })
}

/// Translate the text field of a Slint `KeyEvent` into an input key.
///
/// Slint uses private-use code points (`U+F700..=U+F7FF`) for named keys.
/// Named keys without a launcher action are rejected here so they can never
/// become search text; only printable characters produce [`Key::Char`].
#[must_use]
pub fn key_from_text(text: &str) -> Option<Key> {
    Some(match text {
        "\u{000a}" | "\r" => Key::Select,
        "\u{001b}" => Key::Back,
        "\u{0008}" | "\u{007f}" => Key::Backspace,
        "\u{F700}" => Key::Up,
        "\u{F701}" => Key::Down,
        "\u{F702}" => Key::Left,
        "\u{F703}" => Key::Right,
        "\u{F729}" => Key::Home,
        _ => {
            let mut characters = text.chars();
            let character = characters.next()?;
            if characters.next().is_some()
                || character.is_control()
                || ('\u{F700}'..='\u{F7FF}').contains(&character)
            {
                return None;
            }
            Key::Char(character)
        }
    })
}
