//! Kernel HDMI-CEC bridge.
//!
//! The Raspberry Pi exposes the TV remote through the kernel CEC adapter
//! (`/dev/cec0`, driver `vc4_hdmi`). This module supervises a fixed-argument
//! [`cec_ctl`](CecConfig::kernel) helper, parses a bounded stream of CEC
//! messages into [`InputEvent`]s tagged [`InputOrigin::LocalCec`], and
//! reconnects with backoff when the helper exits or the adapter disappears.
//!
//! No shell is involved and no CLI or remote value reaches the helper
//! arguments. Monitor mode is deliberately not used: it requires root, while
//! follow mode (`--playback --no-rc-passthrough --wait-for-msgs`) works for a
//! user in the `video` group and leaves the kernel RC passthrough disabled so
//! the remote has exactly one input path.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use super::{InputEvent, REPEAT_INTERVAL, RepeatGate, key_from_cec_ui_command};
use crate::model::InputOrigin;

/// Largest accepted CEC monitor line in bytes.
///
/// Longer lines are discarded without allocating; a truncated message is
/// never parsed because its `ui_cmd` field may be incomplete.
pub const MAX_CEC_LINE: usize = 512;
/// How long a same-key press is treated as a held repeat rather than a new
/// press when the TV does not send a release between repeats.
pub const HOLD_REPEAT_WINDOW: Duration = Duration::from_secs(2);
/// Delay between probes when the adapter or helper is absent.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// First reconnect delay.
pub const BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Maximum reconnect delay.
pub const BACKOFF_MAXIMUM: Duration = Duration::from_secs(5);
/// How long the supervisor waits for an exited helper to be reaped.
const REAP_TIMEOUT: Duration = Duration::from_millis(500);
/// Poll slice for bounded waits and shutdown checks.
const WAIT_SLICE: Duration = Duration::from_millis(25);

/// Fixed-argument helper and device settings.
///
/// [`Self::kernel`] is the production configuration. The builder methods exist
/// for tests that substitute a fixture helper; they are not reachable from the
/// command line, the network or the pairing protocol.
#[derive(Debug, Clone)]
pub struct CecConfig {
    /// CEC adapter device.
    pub device: PathBuf,
    /// Helper program. A bare name is resolved through `PATH`.
    pub program: PathBuf,
    /// Delay between probes while the device or helper is absent.
    pub probe_interval: Duration,
    /// Reconnect backoff after a helper exits or a read fails.
    pub backoff: Backoff,
}

impl CecConfig {
    /// Production configuration for the Pi kernel CEC adapter.
    #[must_use]
    pub fn kernel() -> Self {
        Self {
            device: PathBuf::from("/dev/cec0"),
            program: PathBuf::from("cec-ctl"),
            probe_interval: DEFAULT_PROBE_INTERVAL,
            backoff: Backoff::default(),
        }
    }

    /// Override the adapter device (test fixture).
    #[must_use]
    pub fn with_device(mut self, device: PathBuf) -> Self {
        self.device = device;
        self
    }

    /// Override the helper program (test fixture).
    #[must_use]
    pub fn with_program(mut self, program: PathBuf) -> Self {
        self.program = program;
        self
    }

    /// Override the absent-device probe interval.
    #[must_use]
    pub fn with_probe_interval(mut self, probe_interval: Duration) -> Self {
        self.probe_interval = probe_interval;
        self
    }

    /// Override the reconnect backoff.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Fixed helper arguments, in the order passed to `cec-ctl`.
    ///
    /// The helper is configured as a playback device, RC passthrough is
    /// disabled so received CEC messages remain visible on stdout, and
    /// `--wait-for-msgs` keeps the process in follow mode without root-only
    /// monitor mode. Nothing from the network, pairing protocol or CLI is
    /// appended to this list.
    #[must_use]
    pub fn arguments(&self) -> Vec<OsString> {
        vec![
            OsString::from("--device"),
            self.device.clone().into_os_string(),
            OsString::from("--playback"),
            OsString::from("--no-rc-passthrough"),
            OsString::from("--wait-for-msgs"),
            OsString::from("--skip-info"),
        ]
    }
}

/// Exponential reconnect backoff with a hard cap.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    maximum: Duration,
    next: Duration,
}

impl Backoff {
    /// Create a backoff; a zero initial delay is raised to one millisecond.
    #[must_use]
    pub fn new(initial: Duration, maximum: Duration) -> Self {
        let initial = if initial.is_zero() {
            Duration::from_millis(1)
        } else {
            initial
        };
        let maximum = maximum.max(initial);
        Self {
            initial,
            maximum,
            next: initial,
        }
    }

    /// Return the current delay and advance the next one.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.maximum);
        delay
    }

    /// Reset to the initial delay after a connection stayed up.
    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    /// Delay that will be returned next.
    #[must_use]
    pub const fn current(&self) -> Duration {
        self.next
    }

    /// Maximum delay.
    #[must_use]
    pub const fn maximum(&self) -> Duration {
        self.maximum
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BACKOFF_INITIAL, BACKOFF_MAXIMUM)
    }
}

/// Result of feeding one monitor line to [`CecParser`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CecParse {
    /// The line carried no supported key.
    Ignored,
    /// The line exceeded [`MAX_CEC_LINE`] and was discarded.
    Truncated,
    /// One physical key event.
    Event(InputEvent),
}

/// Bounded parser for `cec-ctl` follow-mode output.
///
/// Only `CEC_MSG_USER_CONTROL_PRESSED` messages with a supported `ui_cmd`
/// produce events. Releases clear the held-key state. Unknown commands and
/// other CEC traffic are ignored.
#[derive(Debug)]
pub struct CecParser {
    pressed: bool,
    held: Option<(super::Key, Duration)>,
    gate: RepeatGate,
}

impl CecParser {
    /// Create an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pressed: false,
            held: None,
            gate: RepeatGate::new(REPEAT_INTERVAL),
        }
    }

    /// Forget any held key and repeat history, for example on reconnect.
    pub fn reset(&mut self) {
        self.pressed = false;
        self.held = None;
        self.gate.reset();
    }

    /// Parse one line at a monotonic timestamp.
    pub fn feed_line(&mut self, line: &str, now: Duration) -> CecParse {
        if line.len() > MAX_CEC_LINE {
            self.reset();
            return CecParse::Truncated;
        }
        let line = line.trim();
        if line.is_empty() {
            return CecParse::Ignored;
        }
        if line.contains("CEC_MSG_USER_CONTROL_RELEASED") {
            self.pressed = false;
            self.held = None;
            return CecParse::Ignored;
        }
        if line.contains("CEC_MSG_USER_CONTROL_PRESSED") {
            self.pressed = true;
        } else if line.contains("CEC_MSG_") {
            // A different message starts; a stale press context cannot apply.
            self.pressed = false;
        }
        let Some(token) = line
            .split_whitespace()
            .find(|token| token.contains("UI_CMD_"))
        else {
            return CecParse::Ignored;
        };
        if !self.pressed {
            return CecParse::Ignored;
        }
        // A user-control message carries at most one ui_cmd; leave the press
        // frame as soon as it is consumed so later unrelated lines cannot be
        // mistaken for another key.
        self.pressed = false;
        let Some(key) = key_from_cec_ui_command(token) else {
            return CecParse::Ignored;
        };
        let repeat = self.held.is_some_and(|(held, last)| {
            held == key && now.saturating_sub(last) < HOLD_REPEAT_WINDOW
        });
        self.held = Some((key, now));
        if let Some(slot) = key.repeat_slot()
            && !self.gate.accept(slot, now)
        {
            return CecParse::Ignored;
        }
        CecParse::Event(InputEvent {
            origin: InputOrigin::LocalCec,
            key,
            repeat,
        })
    }
}

impl Default for CecParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed state of one CEC bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CecStatus {
    /// The supervisor thread is starting.
    Starting,
    /// The helper is running and the adapter is present.
    Monitoring {
        /// Adapter path reported by the configuration.
        device: PathBuf,
    },
    /// CEC cannot be used right now; the bridge will retry.
    Unavailable {
        /// Why the bridge is unavailable.
        reason: CecUnavailable,
    },
    /// The helper exited or the adapter read failed; a retry is scheduled.
    Disconnected {
        /// Observed disconnect.
        reason: CecDisconnect,
        /// Delay before the next attempt.
        retry_in: Duration,
    },
    /// The bridge stopped permanently.
    Stopped,
}

/// Reasons the CEC path cannot start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CecUnavailable {
    /// `/dev/cec0` (or the configured device) does not exist.
    DeviceMissing,
    /// The device path exists but cannot be inspected.
    DeviceUnreadable {
        /// Operating-system error kind.
        kind: io::ErrorKind,
    },
    /// The fixed helper (`cec-ctl`) is not installed or not on `PATH`.
    HelperMissing,
    /// The helper or device denied access to the current user.
    PermissionDenied,
    /// The helper could not be spawned for another reason.
    SpawnFailed {
        /// Operating-system error kind.
        kind: io::ErrorKind,
    },
}

/// Observed disconnect from the CEC helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CecDisconnect {
    /// The helper exited, optionally with a status code.
    Exited {
        /// Process exit code, or `None` when killed by a signal.
        code: Option<i32>,
    },
    /// Reading the helper's output failed.
    ReadFailed {
        /// Operating-system error kind.
        kind: io::ErrorKind,
    },
}

/// Result of a bounded bridge shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// The supervisor thread joined inside the deadline.
    Stopped,
    /// The supervisor did not join inside the deadline.
    TimedOut,
}

/// Failure starting the bridge thread itself.
#[derive(Debug, Error)]
pub enum CecError {
    /// The OS refused a new thread.
    #[error("cannot start the CEC supervisor: {0}")]
    ThreadSpawn(#[source] io::Error),
}

/// Supervised kernel CEC bridge.
///
/// Callbacks run on the bridge thread and must not block. Dropping the bridge
/// requests stop and kills the helper; prefer [`Self::shutdown`] for a bounded
/// join during application teardown.
pub struct CecBridge {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    join: Option<JoinHandle<()>>,
}

impl CecBridge {
    /// Start the supervisor. Device/helper absence is reported through
    /// `on_status`, not as an error; only a failed thread spawn returns `Err`.
    pub fn start<E, S>(config: CecConfig, on_event: E, on_status: S) -> Result<Self, CecError>
    where
        E: Fn(InputEvent) + Send + 'static,
        S: Fn(&CecStatus) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_child = Arc::clone(&child);
        let join = thread::Builder::new()
            .name("tv-cec".to_string())
            .spawn(move || supervise(&config, &on_event, &on_status, &thread_stop, &thread_child))
            .map_err(CecError::ThreadSpawn)?;
        Ok(Self {
            stop,
            child,
            join: Some(join),
        })
    }

    /// Request stop and join within `timeout`.
    pub fn shutdown(mut self, timeout: Duration) -> ShutdownOutcome {
        self.request_stop();
        let Some(join) = self.join.take() else {
            return ShutdownOutcome::Stopped;
        };
        let deadline = Instant::now() + timeout;
        while !join.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if join.is_finished() {
            let _ = join.join();
            ShutdownOutcome::Stopped
        } else {
            ShutdownOutcome::TimedOut
        }
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        let mut slot = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(child) = slot.as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for CecBridge {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(join) = self.join.take()
            && join.is_finished()
        {
            let _ = join.join();
        }
    }
}

fn supervise<E, S>(
    config: &CecConfig,
    on_event: &E,
    on_status: &S,
    stop: &AtomicBool,
    slot: &Mutex<Option<Child>>,
) where
    E: Fn(InputEvent),
    S: Fn(&CecStatus),
{
    on_status(&CecStatus::Starting);
    let mut backoff = config.backoff.clone();
    let mut reported_unavailable: Option<CecUnavailable> = None;

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        match config.device.try_exists() {
            Ok(true) => {}
            Ok(false) => {
                report_unavailable(
                    on_status,
                    &mut reported_unavailable,
                    CecUnavailable::DeviceMissing,
                );
                if wait_or_stop(stop, config.probe_interval) {
                    break;
                }
                continue;
            }
            Err(error) => {
                report_unavailable(
                    on_status,
                    &mut reported_unavailable,
                    CecUnavailable::DeviceUnreadable { kind: error.kind() },
                );
                if wait_or_stop(stop, config.probe_interval) {
                    break;
                }
                continue;
            }
        }

        let mut command = Command::new(&config.program);
        command
            .args(config.arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let reason = match error.kind() {
                    io::ErrorKind::NotFound => CecUnavailable::HelperMissing,
                    io::ErrorKind::PermissionDenied => CecUnavailable::PermissionDenied,
                    kind => CecUnavailable::SpawnFailed { kind },
                };
                report_unavailable(on_status, &mut reported_unavailable, reason);
                let delay = if error.kind() == io::ErrorKind::NotFound {
                    config.probe_interval
                } else {
                    backoff.next_delay()
                };
                if wait_or_stop(stop, delay) {
                    break;
                }
                continue;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            report_unavailable(
                on_status,
                &mut reported_unavailable,
                CecUnavailable::SpawnFailed {
                    kind: io::ErrorKind::Other,
                },
            );
            if wait_or_stop(stop, config.probe_interval) {
                break;
            }
            continue;
        };

        {
            let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(previous) = slot.as_mut() {
                let _ = previous.kill();
            }
            *slot = Some(child);
        }
        reported_unavailable = None;
        on_status(&CecStatus::Monitoring {
            device: config.device.clone(),
        });

        let started = Instant::now();
        let mut parser = CecParser::new();
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::with_capacity(MAX_CEC_LINE);
        let mut read_failure = None;

        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match read_line_bounded(&mut reader, &mut line, MAX_CEC_LINE) {
                Ok(ReadLine::Eof) => break,
                Ok(ReadLine::Line { truncated }) => {
                    if truncated {
                        parser.reset();
                        continue;
                    }
                    let text = String::from_utf8_lossy(&line);
                    if let CecParse::Event(event) =
                        parser.feed_line(text.as_ref(), started.elapsed())
                    {
                        on_event(event);
                    }
                }
                Err(error) => {
                    read_failure = Some(error.kind());
                    break;
                }
            }
        }

        if stop.load(Ordering::Acquire) {
            kill_and_reap(slot);
            break;
        }

        let code = if read_failure.is_some() {
            kill_and_reap(slot);
            None
        } else {
            reap_exited(slot, stop)
        };
        parser.reset();
        if started.elapsed() >= Duration::from_secs(5) {
            backoff.reset();
        }
        let delay = backoff.next_delay();
        let reason = match read_failure {
            Some(kind) => CecDisconnect::ReadFailed { kind },
            None => CecDisconnect::Exited { code },
        };
        on_status(&CecStatus::Disconnected {
            reason,
            retry_in: delay,
        });
        if wait_or_stop(stop, delay) {
            break;
        }
    }

    kill_and_reap(slot);
    on_status(&CecStatus::Stopped);
}

fn report_unavailable<S>(
    on_status: &S,
    reported: &mut Option<CecUnavailable>,
    reason: CecUnavailable,
) where
    S: Fn(&CecStatus),
{
    if reported.as_ref() != Some(&reason) {
        on_status(&CecStatus::Unavailable {
            reason: reason.clone(),
        });
        *reported = Some(reason);
    }
}

fn wait_or_stop(stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if stop.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep((deadline - now).min(WAIT_SLICE));
    }
}

fn kill_and_reap(slot: &Mutex<Option<Child>>) {
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(mut child) = slot.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn reap_exited(slot: &Mutex<Option<Child>>, stop: &AtomicBool) -> Option<i32> {
    let deadline = Instant::now() + REAP_TIMEOUT;
    loop {
        {
            let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
            let child = slot.as_mut()?;
            match child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            kill_and_reap(slot);
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// One bounded read result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadLine {
    /// A line (possibly truncated) was read; `truncated` means bytes were
    /// discarded before the newline.
    Line { truncated: bool },
    /// End of the helper's output.
    Eof,
}

/// Read one line without ever growing `buffer` beyond `max`.
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    max: usize,
) -> io::Result<ReadLine> {
    buffer.clear();
    let mut truncated = false;
    loop {
        let (take, newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return if buffer.is_empty() {
                    Ok(ReadLine::Eof)
                } else {
                    Ok(ReadLine::Line { truncated })
                };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.unwrap_or(available.len());
            let remaining = max.saturating_sub(buffer.len());
            if take <= remaining {
                buffer.extend_from_slice(&available[..take]);
            } else {
                buffer.extend_from_slice(&available[..remaining]);
                truncated = true;
            }
            (take, newline.is_some())
        };
        reader.consume(take + usize::from(newline));
        if newline {
            return Ok(ReadLine::Line { truncated });
        }
    }
}
