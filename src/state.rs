//! State reducer, bounded event queues and cancellation-safe shutdown.
//!
//! The reducer is synchronous and does no I/O. A bounded Tokio task consumes
//! events, ignores rejected commands, and emits effects to another bounded
//! queue. Shutdown is observed through a watch channel and never blocks.

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::watch;
use uuid::Uuid;

use crate::model::{
    Action, AppState, Direction, Effect, Event, InputOrigin, Mode, Playback, Source, StateError,
};

/// Default capacity for the event queue, matching the protocol command bound.
pub const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 32;
/// Default capacity for the effect queue.
pub const DEFAULT_EFFECT_QUEUE_CAPACITY: usize = 32;
/// Maximum accepted byte length for a player error code.
const MAX_ERROR_CODE_BYTES: usize = 128;

/// Synchronous reducer for typed application state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reducer {
    state: AppState,
    generation: u64,
}

impl Reducer {
    /// Create a reducer in the home state with no playback generation used.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Last playback generation allocated, or zero before the first playback.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Apply one event and return the effects it requires.
    ///
    /// Rejected events leave state unchanged. Callers can treat any error as a
    /// command rejection and acknowledge it with a safe reason.
    pub fn apply(&mut self, event: Event) -> Result<Vec<Effect>, StateError> {
        match event {
            Event::Action { origin, action } => self.apply_action(origin, action),
            Event::PhysicalApproval { origin, request_id } => {
                self.apply_physical_approval(origin, request_id)
            }
            Event::PlaybackStarted {
                generation,
                playback_id,
            } => {
                self.active_playback(generation, playback_id)?;
                self.transition(Mode::Playing)?;
                self.state.paused = false;
                Ok(Vec::new())
            }
            Event::PlaybackProgress {
                generation,
                playback_id,
                position_ms,
                duration_ms,
            } => {
                let playback = self.active_playback(generation, playback_id)?;
                if !matches!(self.state.mode, Mode::Playing | Mode::Paused) {
                    return Err(StateError::IllegalTransition {
                        from: self.state.mode,
                        to: Mode::Playing,
                    });
                }
                let limit = duration_ms.or(self.state.duration_ms);
                if limit.is_some_and(|duration| position_ms > duration) {
                    return Err(StateError::InvalidValue {
                        field: "position_ms",
                        reason: "must not exceed duration_ms",
                    });
                }
                if let Some(duration) = duration_ms {
                    self.state.duration_ms = Some(duration);
                }
                self.state.position_ms = Some(position_ms);
                Ok(vec![Effect::Progress {
                    item_id: playback.item_id,
                    position_ms,
                    duration_ms: self.state.duration_ms,
                }])
            }
            Event::PlaybackPaused {
                generation,
                playback_id,
                paused,
            } => {
                self.active_playback(generation, playback_id)?;
                let target = if paused { Mode::Paused } else { Mode::Playing };
                if !matches!(self.state.mode, Mode::Playing | Mode::Paused) {
                    return Err(StateError::IllegalTransition {
                        from: self.state.mode,
                        to: target,
                    });
                }
                self.transition(target)?;
                self.state.paused = paused;
                Ok(Vec::new())
            }
            Event::PlaybackEnded {
                generation,
                playback_id,
            } => {
                self.active_playback(generation, playback_id)?;
                let effects = self.stop_active();
                self.transition(Mode::Home)?;
                Ok(effects)
            }
            Event::PlaybackFailed {
                generation,
                playback_id,
                error_code,
            } => {
                let playback = self.active_playback(generation, playback_id)?;
                if !is_valid_error_code(&error_code) {
                    return Err(StateError::InvalidValue {
                        field: "error_code",
                        reason: "must be 1..=128 characters without control bytes",
                    });
                }
                self.transition(Mode::Error)?;
                self.state.playback = None;
                self.state.item_id = None;
                self.state.title = None;
                self.state.position_ms = None;
                self.state.duration_ms = None;
                self.state.paused = false;
                self.state.error = Some(error_code);
                Ok(vec![Effect::StopPlayback {
                    generation: playback.generation,
                    playback_id: playback.id,
                }])
            }
        }
    }

    fn apply_action(
        &mut self,
        _origin: InputOrigin,
        action: Action,
    ) -> Result<Vec<Effect>, StateError> {
        action.validate()?;
        match action {
            Action::Navigate { direction } => self.navigate(direction),
            Action::SetPaused { paused } => self.set_paused(paused),
            Action::Seek { position_ms } => self.seek(position_ms),
            Action::SetVolume { volume } => {
                self.state.volume = volume;
                Ok(vec![Effect::SetVolume { volume }])
            }
            Action::Stop => self.stop(),
            Action::PlayYoutube { video_id } => self.start_playback(Source::YouTube, video_id),
            Action::PlayJellyfin { item_id } => self.start_playback(Source::Jellyfin, item_id),
            Action::Standby { enabled } => self.standby(enabled),
        }
    }

    fn apply_physical_approval(
        &mut self,
        origin: InputOrigin,
        request_id: Uuid,
    ) -> Result<Vec<Effect>, StateError> {
        if origin.is_remote() {
            return Err(StateError::RemoteApprovalDenied);
        }
        if request_id.is_nil() {
            return Err(StateError::InvalidValue {
                field: "request_id",
                reason: "must not be nil",
            });
        }
        if self.state.mode == Mode::Standby {
            return Err(StateError::IllegalTransition {
                from: Mode::Standby,
                to: Mode::Standby,
            });
        }
        Ok(vec![Effect::ApprovePairing { request_id }])
    }

    fn navigate(&mut self, direction: Direction) -> Result<Vec<Effect>, StateError> {
        if self.state.mode == Mode::Standby {
            return Err(StateError::IllegalTransition {
                from: Mode::Standby,
                to: Mode::Standby,
            });
        }

        match direction {
            Direction::Home => {
                let mut effects = self.stop_active();
                self.transition(Mode::Home)?;
                effects.push(Effect::Navigate { direction });
                Ok(effects)
            }
            Direction::Back => match self.state.mode {
                Mode::Home => Ok(vec![Effect::Navigate { direction }]),
                Mode::Browsing => {
                    self.transition(Mode::Home)?;
                    Ok(vec![Effect::Navigate { direction }])
                }
                _ => {
                    let mut effects = self.stop_active();
                    self.transition(Mode::Home)?;
                    effects.push(Effect::Navigate { direction });
                    Ok(effects)
                }
            },
            Direction::Select => {
                if self.state.mode == Mode::Home {
                    self.transition(Mode::Browsing)?;
                }
                Ok(vec![Effect::Navigate { direction }])
            }
            Direction::Up | Direction::Down | Direction::Left | Direction::Right => {
                Ok(vec![Effect::Navigate { direction }])
            }
        }
    }

    fn set_paused(&mut self, paused: bool) -> Result<Vec<Effect>, StateError> {
        let playback = self
            .state
            .playback
            .as_ref()
            .ok_or(StateError::NoActivePlayback)?
            .clone();
        let target = if paused { Mode::Paused } else { Mode::Playing };
        match (self.state.mode, paused) {
            (Mode::Playing, true) | (Mode::Paused, false) => {}
            (Mode::Playing, false) | (Mode::Paused, true) => return Ok(Vec::new()),
            _ => {
                return Err(StateError::IllegalTransition {
                    from: self.state.mode,
                    to: target,
                });
            }
        }
        self.transition(target)?;
        self.state.paused = paused;
        Ok(vec![Effect::SetPaused {
            generation: playback.generation,
            playback_id: playback.id,
            paused,
        }])
    }

    fn seek(&mut self, position_ms: u64) -> Result<Vec<Effect>, StateError> {
        let playback = self
            .state
            .playback
            .as_ref()
            .ok_or(StateError::NoActivePlayback)?
            .clone();
        if !matches!(self.state.mode, Mode::Playing | Mode::Paused) {
            return Err(StateError::IllegalTransition {
                from: self.state.mode,
                to: Mode::Playing,
            });
        }
        if self
            .state
            .duration_ms
            .is_some_and(|duration| position_ms > duration)
        {
            return Err(StateError::InvalidValue {
                field: "position_ms",
                reason: "must not exceed duration_ms",
            });
        }
        self.state.position_ms = Some(position_ms);
        Ok(vec![Effect::Seek {
            generation: playback.generation,
            playback_id: playback.id,
            position_ms,
        }])
    }

    fn stop(&mut self) -> Result<Vec<Effect>, StateError> {
        if self.state.mode == Mode::Standby {
            return Err(StateError::IllegalTransition {
                from: Mode::Standby,
                to: Mode::Home,
            });
        }
        let effects = self.stop_active();
        self.transition(Mode::Home)?;
        Ok(effects)
    }

    fn standby(&mut self, enabled: bool) -> Result<Vec<Effect>, StateError> {
        if enabled {
            if self.state.mode == Mode::Standby {
                return Ok(Vec::new());
            }
            let mut effects = self.stop_active();
            self.transition(Mode::Standby)?;
            effects.push(Effect::Standby { enabled });
            Ok(effects)
        } else {
            if self.state.mode != Mode::Standby {
                return Err(StateError::IllegalTransition {
                    from: self.state.mode,
                    to: Mode::Standby,
                });
            }
            self.transition(Mode::Home)?;
            Ok(vec![Effect::Standby { enabled }])
        }
    }

    fn start_playback(
        &mut self,
        source: Source,
        item_id: String,
    ) -> Result<Vec<Effect>, StateError> {
        if self.state.mode == Mode::Standby {
            return Err(StateError::IllegalTransition {
                from: Mode::Standby,
                to: Mode::Loading,
            });
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(StateError::GenerationExhausted)?;
        let mut effects = self.stop_active();
        self.transition(Mode::Loading)?;

        let playback_id = Uuid::new_v4();
        self.generation = generation;
        self.state.playback = Some(Playback {
            generation,
            id: playback_id,
            source,
            item_id: item_id.clone(),
        });
        self.state.source = Some(source);
        self.state.item_id = Some(item_id.clone());
        self.state.title = None;
        self.state.position_ms = Some(0);
        self.state.duration_ms = None;
        self.state.paused = false;
        self.state.error = None;
        effects.push(Effect::StartPlayback {
            generation,
            playback_id,
            source,
            item_id,
        });
        Ok(effects)
    }

    fn active_playback(&self, generation: u64, playback_id: Uuid) -> Result<Playback, StateError> {
        match self.state.playback.as_ref() {
            None => Err(StateError::NoActivePlayback),
            Some(playback) if playback.generation != generation => {
                Err(StateError::StaleGeneration {
                    expected: playback.generation,
                    received: generation,
                })
            }
            Some(playback) if playback.id != playback_id => Err(StateError::StalePlayback {
                expected: playback.id,
                received: playback_id,
            }),
            Some(playback) => Ok(playback.clone()),
        }
    }

    fn transition(&mut self, next: Mode) -> Result<(), StateError> {
        let current = self.state.mode;
        if current == next {
            return Ok(());
        }
        if current.can_transition_to(next) {
            self.state.mode = next;
            Ok(())
        } else {
            Err(StateError::IllegalTransition {
                from: current,
                to: next,
            })
        }
    }

    fn stop_active(&mut self) -> Vec<Effect> {
        let stopped = self
            .state
            .playback
            .take()
            .map(|playback| Effect::StopPlayback {
                generation: playback.generation,
                playback_id: playback.id,
            });
        self.state.source = None;
        self.state.item_id = None;
        self.state.title = None;
        self.state.position_ms = None;
        self.state.duration_ms = None;
        self.state.paused = false;
        self.state.error = None;
        stopped.into_iter().collect()
    }
}

fn is_valid_error_code(error_code: &str) -> bool {
    !error_code.is_empty()
        && error_code.len() <= MAX_ERROR_CODE_BYTES
        && !error_code.chars().any(char::is_control)
}

/// Bounded sender for reducer events.
#[derive(Clone, Debug)]
pub struct EventSender {
    sender: mpsc::Sender<Event>,
}

impl EventSender {
    /// Try to enqueue without blocking.
    ///
    /// Returns [`StateError::QueueFull`] when the bound is reached so a remote
    /// or input task can reject the command instead of growing memory.
    pub fn try_send(&self, event: Event) -> Result<(), StateError> {
        self.sender.try_send(event).map_err(queue_error)
    }

    /// Enqueue, waiting for capacity. This future is cancel-safe.
    pub async fn send(&self, event: Event) -> Result<(), StateError> {
        self.sender
            .send(event)
            .await
            .map_err(|_| StateError::QueueClosed)
    }

    /// Queue capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }

    /// Whether the receiver has been dropped.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

/// Bounded receiver for reducer events.
#[derive(Debug)]
pub struct EventReceiver {
    receiver: mpsc::Receiver<Event>,
}

impl EventReceiver {
    /// Receive the next event, or `None` after all senders are dropped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }

    /// Current queue capacity, useful for non-blocking diagnostics.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.receiver.capacity()
    }
}

fn queue_error(error: TrySendError<Event>) -> StateError {
    match error {
        TrySendError::Full(_) => StateError::QueueFull,
        TrySendError::Closed(_) => StateError::QueueClosed,
    }
}

/// Create a bounded event channel.
///
/// A capacity of zero is rejected rather than panicking inside Tokio.
pub fn event_channel(capacity: usize) -> Result<(EventSender, EventReceiver), StateError> {
    if capacity == 0 {
        return Err(StateError::InvalidQueueCapacity);
    }
    let (sender, receiver) = mpsc::channel(capacity);
    Ok((EventSender { sender }, EventReceiver { receiver }))
}

/// Handle used to request shutdown.
#[derive(Clone, Debug)]
pub struct ShutdownHandle {
    sender: watch::Sender<bool>,
}

impl ShutdownHandle {
    /// Request shutdown. Idempotent and non-blocking.
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }

    /// Whether shutdown has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }
}

/// Cancellation signal observed by the state loop.
#[derive(Debug)]
pub struct ShutdownSignal {
    receiver: watch::Receiver<bool>,
}

impl ShutdownSignal {
    /// Wait until shutdown is requested or the handle is dropped.
    ///
    /// This future is cancel-safe: it can be used repeatedly in `select!`
    /// without losing a notification.
    pub async fn cancelled(&mut self) {
        if *self.receiver.borrow() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow() {
                return;
            }
        }
    }

    /// Whether shutdown has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }
}

/// Create a shutdown handle and signal.
#[must_use]
pub fn shutdown_channel() -> (ShutdownHandle, ShutdownSignal) {
    let (sender, receiver) = watch::channel(false);
    (ShutdownHandle { sender }, ShutdownSignal { receiver })
}

/// Consume events until shutdown, queue closure or an effect-queue failure.
///
/// Invalid events are rejected and skipped; they never terminate the loop. The
/// reducer state is returned so it can be persisted or inspected after a clean
/// cancellation.
pub async fn run_state_loop(
    mut reducer: Reducer,
    mut events: EventReceiver,
    effects: mpsc::Sender<Effect>,
    mut shutdown: ShutdownSignal,
) -> Result<Reducer, StateError> {
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            event = events.recv() => {
                let Some(event) = event else {
                    break;
                };
                if let Ok(produced) = reducer.apply(event) {
                    for effect in produced {
                        effects.try_send(effect).map_err(|error| match error {
                            TrySendError::Full(_) => StateError::QueueFull,
                            TrySendError::Closed(_) => StateError::QueueClosed,
                        })?;
                    }
                }
            }
        }
    }
    Ok(reducer)
}
