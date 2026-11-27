//! Supervised native GStreamer playback child.
//!
//! The process speaks the JSON-lines protocol in [`tv_client::player`] on
//! stdin/stdout. It is started by the parent UI, receives the media URL and
//! optional authentication headers over the pipe (never argv or environment),
//! and exits on `stop`, stdin EOF or the default SIGTERM action.
//!
//! `waylandsink` owns its own Wayland surface, so the compositor delivers
//! keyboard events to that surface and the UI process cannot see them. The
//! child therefore reads Linux evdev key events directly and draws a bounded
//! `textoverlay` hint on the video. `Back`/`Esc` stops playback and exits with a
//! `stopped` event, so the parent returns to the launcher instead of treating
//! the exit as a crash.
//!
//! Audio is selected deterministically: the kiosk profile asks for the direct
//! ALSA path (`TV_AUDIO_SINK=alsa`, `TV_AUDIO_DEVICE=hw:0,0`), a generic
//! desktop keeps `autoaudiosink`, and a failed candidate advances to the next
//! one. When no audio device can be opened at all, a silent `fakesink` keeps
//! video playing and the child reports a typed `degraded` event.

use std::collections::{BTreeMap, VecDeque};
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::{self, Read};
use std::io::{BufRead, Write};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::glib::prelude::*;
use gstreamer::prelude::*;
use tv_client::player::{
    self, LoadRequest, PlayerEvent, PlayerRequest, audio, controls, redact_text,
};

const POSITION_INTERVAL: Duration = Duration::from_millis(500);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(target_os = "linux")]
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(target_os = "linux")]
const INPUT_RESCAN_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const MAX_INPUT_DEVICES: usize = 32;
#[cfg(target_os = "linux")]
const MAX_EVENTS_PER_DEVICE: usize = 16;
/// Set to `off`/`0` to skip the on-screen hint (keeps the plain video sink).
const OVERLAY_ENV: &str = "TV_PLAYER_OVERLAY";
const OVERLAY_PROBE_TIMEOUT_SECONDS: u32 = 2;
const MAX_LOG_BYTES: usize = 300;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            // Static, secret-free diagnostics only.
            log_line(&format!("tv-native-player: {message}"));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), &'static str> {
    gst::init().map_err(|_| "gstreamer initialization failed")?;

    let (inbound_tx, inbound_rx) = std_mpsc::channel::<Inbound>();
    let stdin_tx = inbound_tx.clone();
    std::thread::Builder::new()
        .name("player-stdin".to_string())
        .spawn(move || read_stdin(stdin_tx))
        .map_err(|_| "stdin reader failed")?;
    spawn_input_reader(inbound_tx);

    let player = Player::new()?;
    let player_handle = Rc::clone(&player);
    glib::timeout_add_local(COMMAND_POLL_INTERVAL, move || {
        player_handle.poll(&inbound_rx);
        glib::ControlFlow::Continue
    });

    player.main_loop.run();
    let _ = player.pipeline.set_state(gst::State::Null);
    Ok(())
}

enum Inbound {
    Request(PlayerRequest),
    Key(controls::KeyAction),
    InputStatus {
        available: bool,
        detail: Option<&'static str>,
    },
    Invalid,
    Eof,
}

fn read_stdin(sender: std_mpsc::Sender<Inbound>) {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    loop {
        match read_line_bounded(&mut reader, player::MAX_LINE_BYTES) {
            Ok(Some(bytes)) => {
                let request = match String::from_utf8(bytes) {
                    Ok(line) => player::decode_request_line(&line).ok(),
                    Err(_) => None,
                };
                let inbound = match request {
                    Some(request) => Inbound::Request(request),
                    None => Inbound::Invalid,
                };
                if sender.send(inbound).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(Inbound::Eof);
                return;
            }
            Err(_) => {
                let _ = sender.send(Inbound::Invalid);
                return;
            }
        }
    }
}

/// Bounded newline reader for a blocking child pipe.
fn read_line_bounded<R: BufRead>(reader: &mut R, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Ok(Some(bytes))
            };
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if bytes.len().saturating_add(position) > limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line too long",
                ));
            }
            bytes.extend_from_slice(&available[..position]);
            reader.consume(position + 1);
            return Ok(Some(bytes));
        }
        bytes.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if bytes.len() > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line too long",
            ));
        }
    }
}

/// Ordered audio-sink candidates and the currently selected sink label.
struct AudioChain {
    remaining: VecDeque<audio::AudioSinkSpec>,
    label: String,
    silent: bool,
}

impl AudioChain {
    fn new(plan: &audio::AudioSinkPlan) -> Self {
        let mut remaining: VecDeque<_> = plan.candidates().iter().cloned().collect();
        remaining.push_back(audio::silent_fallback());
        Self {
            remaining,
            label: String::new(),
            silent: false,
        }
    }

    /// Build the next usable sink, skipping factories and ALSA cards the host
    /// does not have. A `fakesink` result means video continues without sound.
    fn next(&mut self) -> Option<gst::Element> {
        while let Some(spec) = self.remaining.pop_front() {
            if !audio_sink_present(&spec) {
                continue;
            }
            if let Ok(element) = make_audio_sink(&spec) {
                self.silent = spec.factory() == audio::SILENT_FACTORY;
                self.label = spec.label();
                return Some(element);
            }
        }
        None
    }
}

fn audio_sink_present(spec: &audio::AudioSinkSpec) -> bool {
    if gst::ElementFactory::find(spec.factory()).is_none() {
        return false;
    }
    match audio::alsa_card(spec.device().unwrap_or_default()) {
        Some(card) => std::path::Path::new(&format!("/dev/snd/controlC{card}")).exists(),
        None => true,
    }
}

fn make_audio_sink(spec: &audio::AudioSinkSpec) -> Result<gst::Element, ()> {
    let element = gst::ElementFactory::make(spec.factory())
        .build()
        .map_err(|_| ())?;
    if let Some(device) = spec.device() {
        if element.find_property("device").is_none() {
            return Err(());
        }
        element.set_property("device", device);
    }
    Ok(element)
}

#[derive(Default)]
struct PlaybackState {
    ready: bool,
    ended: bool,
    paused: bool,
    input_available: bool,
    last_position_ms: u64,
    last_position_notification: Option<Instant>,
    video_decoder: Option<String>,
    audio_decoder: Option<String>,
    headers: BTreeMap<String, String>,
    pending_seek_ms: Option<u64>,
    pending_notices: Vec<PlayerEvent>,
    volume: u8,
}

struct Player {
    pipeline: gst::Element,
    main_loop: glib::MainLoop,
    state: Arc<Mutex<PlaybackState>>,
    overlay: Option<gst::Element>,
    // The bus watch owns the `AudioChain` shared state; dropping the guard
    // removes the watch, so keep it for the process lifetime.
    _bus_watch: gst::bus::BusWatchGuard,
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Player {
    fn new() -> Result<Rc<Self>, &'static str> {
        let pipeline = gst::ElementFactory::make("playbin3")
            .build()
            .map_err(|_| "playbin3 is unavailable")?;

        let mut pending_notices = Vec::new();
        let (video_sink, overlay, video_label) = build_video_sink(&mut pending_notices)?;

        let plan = audio::AudioSinkPlan::from_env();
        if let Some(warning) = plan.warning() {
            log_line(&format!(
                "tv-native-player: {warning}; falling back to autoaudiosink"
            ));
        }
        let mut chain = AudioChain::new(&plan);
        let audio_sink = chain.next().ok_or("no audio sink is available")?;
        if chain.silent {
            pending_notices.push(PlayerEvent::Degraded {
                code: "audio_unavailable".to_string(),
                message: Some("no audio device usable; video continues".to_string()),
            });
        }
        log_line(&format!(
            "tv-native-player: audio plan {}; selected {}",
            plan.describe(),
            chain.label
        ));
        pipeline.set_property("video-sink", &video_sink);
        pipeline.set_property("audio-sink", &audio_sink);

        let bus = pipeline.bus().ok_or("playbin3 has no bus")?;
        let main_loop = glib::MainLoop::new(None, false);
        let state = Arc::new(Mutex::new(PlaybackState {
            volume: player::DEFAULT_VOLUME,
            input_available: true,
            pending_notices,
            ..PlaybackState::default()
        }));
        let audio_state = Arc::new(Mutex::new(chain));

        let element_state = Arc::clone(&state);
        let _ = pipeline.connect("element-setup", false, move |values| {
            if let Some(element) = values
                .get(1)
                .and_then(|value| value.get::<gst::Element>().ok())
            {
                let klass = element
                    .factory()
                    .and_then(|factory| factory.metadata("klass").map(ToOwned::to_owned))
                    .unwrap_or_default();
                let name = element
                    .factory()
                    .map(|factory| format!("{}", factory.name()))
                    .unwrap_or_else(|| format!("{}", element.name()));
                let mut state = lock(&element_state);
                if klass.contains("Decoder/Video") {
                    state.video_decoder = Some(name);
                } else if klass.contains("Decoder/Audio") {
                    state.audio_decoder = Some(name);
                }
            }
            None
        });

        let source_state = Arc::clone(&state);
        let _ = pipeline.connect("source-setup", false, move |values| {
            if let Some(source) = values
                .get(1)
                .and_then(|value| value.get::<gst::Element>().ok())
            {
                let headers = lock(&source_state).headers.clone();
                if !headers.is_empty() && source.find_property("extra-headers").is_some() {
                    let mut structure = gst::Structure::new_empty("extra-headers");
                    for (name, value) in headers {
                        structure.set(name.as_str(), value);
                    }
                    source.set_property_from_value("extra-headers", &structure.to_value());
                }
            }
            None
        });

        let bus_state = Arc::clone(&state);
        let bus_audio = Arc::clone(&audio_state);
        let bus_pipeline = pipeline.clone();
        let bus_video_sink = video_sink;
        let bus_audio_sink = audio_sink;
        let bus_overlay = overlay.clone();
        let bus_watch = bus
            .add_watch_local(move |_, message| {
                use gst::MessageView;
                match message.view() {
                    MessageView::Eos(..) => {
                        let mut state = lock(&bus_state);
                        if !state.ended {
                            state.ended = true;
                            emit(&PlayerEvent::Ended);
                        }
                    }
                    MessageView::Error(error) => {
                        if try_audio_fallback(error.src(), &bus_pipeline, &bus_state, &bus_audio) {
                            return glib::ControlFlow::Continue;
                        }
                        let detail = error.error().to_string();
                        let message = redact_text(&detail);
                        let mut state = lock(&bus_state);
                        if !state.ended {
                            state.ended = true;
                            emit(&PlayerEvent::Error {
                                code: "gst_error".to_string(),
                                message: Some(message),
                            });
                        }
                    }
                    MessageView::StateChanged(change) => {
                        let is_pipeline = change
                            .src()
                            .is_some_and(|source| source.name() == bus_pipeline.name());
                        if is_pipeline {
                            match change.current() {
                                gst::State::Paused | gst::State::Playing => {
                                    let first_ready = {
                                        let mut state = lock(&bus_state);
                                        let first = !state.ready;
                                        state.ready = true;
                                        first
                                    };
                                    if first_ready {
                                        let (decoder, audio, notices, input_available) = {
                                            let mut state = lock(&bus_state);
                                            (
                                                state.video_decoder.clone(),
                                                state.audio_decoder.clone(),
                                                std::mem::take(&mut state.pending_notices),
                                                state.input_available,
                                            )
                                        };
                                        let audio_label = lock(&bus_audio).label.clone();
                                        emit(&PlayerEvent::Ready {
                                            decoder: decoder.clone(),
                                            sink: Some(sink_summary(video_label, &audio_label)),
                                        });
                                        emit(&PlayerEvent::Tracks {
                                            video: describe_video(&bus_video_sink).or(decoder),
                                            audio: describe_audio(&bus_audio_sink).or(audio),
                                            subtitle: None,
                                        });
                                        for notice in notices {
                                            emit(&notice);
                                        }
                                        if let Some(overlay) = bus_overlay.as_ref() {
                                            overlay.set_property(
                                                "text",
                                                controls::hint_text(input_available, "Playing"),
                                            );
                                        }
                                    }
                                    if change.current() == gst::State::Playing {
                                        let pending_seek = lock(&bus_state).pending_seek_ms.take();
                                        if let Some(position_ms) = pending_seek {
                                            match gst::ClockTime::try_from(Duration::from_millis(
                                                position_ms,
                                            )) {
                                                Ok(position) => {
                                                    let _ = bus_pipeline.seek_simple(
                                                        gst::SeekFlags::FLUSH
                                                            | gst::SeekFlags::ACCURATE,
                                                        position,
                                                    );
                                                }
                                                Err(_) => {
                                                    emit(&PlayerEvent::Error {
                                                        code: "invalid_request".to_string(),
                                                        message: Some(
                                                            "seek position out of range"
                                                                .to_string(),
                                                        ),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                                gst::State::Null | gst::State::Ready => {}
                                _ => {}
                            }
                        }
                    }
                    MessageView::Buffering(buffering) => {
                        let percent = buffering.percent();
                        let target = if percent < 100 {
                            gst::State::Paused
                        } else {
                            gst::State::Playing
                        };
                        let _ = bus_pipeline.set_state(target);
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .map_err(|_| "bus watch registration failed")?;

        Ok(Rc::new(Self {
            pipeline,
            main_loop,
            state,
            overlay,
            _bus_watch: bus_watch,
        }))
    }

    fn poll(&self, inbound: &std_mpsc::Receiver<Inbound>) {
        // Bounded per tick: a stuck or noisy input source cannot starve the
        // position timer or the bus watch.
        for _ in 0..64 {
            match inbound.try_recv() {
                Ok(Inbound::Request(request)) => self.handle(request),
                Ok(Inbound::Key(action)) => self.handle_key(action),
                Ok(Inbound::InputStatus { available, detail }) => {
                    self.handle_input_status(available, detail);
                }
                Ok(Inbound::Invalid) => emit(&PlayerEvent::Error {
                    code: "bad_request".to_string(),
                    message: None,
                }),
                Ok(Inbound::Eof) | Err(std_mpsc::TryRecvError::Disconnected) => {
                    self.stop();
                    return;
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
            }
        }
        self.publish_position();
    }

    fn handle(&self, request: PlayerRequest) {
        match request {
            PlayerRequest::Load(load) => self.load(load),
            PlayerRequest::Pause => {
                if self.pipeline.set_state(gst::State::Paused).is_ok() {
                    lock(&self.state).paused = true;
                    self.set_hint("Paused - SPACE resumes");
                } else {
                    emit(&PlayerEvent::Error {
                        code: "state_failed".to_string(),
                        message: None,
                    });
                }
            }
            PlayerRequest::Resume => {
                if self.pipeline.set_state(gst::State::Playing).is_ok() {
                    lock(&self.state).paused = false;
                    self.set_hint("Playing");
                } else {
                    emit(&PlayerEvent::Error {
                        code: "state_failed".to_string(),
                        message: None,
                    });
                }
            }
            PlayerRequest::Seek { position_ms } => self.seek(position_ms),
            PlayerRequest::SetVolume { volume } => {
                lock(&self.state).volume = volume;
                self.pipeline
                    .set_property("volume", f64::from(volume) / 100.0);
                self.set_hint(&format!("Volume {volume}%"));
            }
            PlayerRequest::Stop => self.stop(),
        }
    }

    fn handle_key(&self, action: controls::KeyAction) {
        match action {
            controls::KeyAction::Stop => {
                // Tell the parent this was intentional before exiting, so the
                // supervisor returns to the launcher instead of reporting
                // `child_exited`.
                emit(&PlayerEvent::Stopped);
                log_line("tv-native-player: stopped from local controls");
                self.stop();
            }
            controls::KeyAction::TogglePause => self.toggle_pause(),
            controls::KeyAction::SeekBackward => self.seek_by(false),
            controls::KeyAction::SeekForward => self.seek_by(true),
            controls::KeyAction::VolumeUp => self.adjust_volume(true),
            controls::KeyAction::VolumeDown => self.adjust_volume(false),
        }
    }

    fn toggle_pause(&self) {
        match self.pipeline.current_state() {
            gst::State::Playing if self.pipeline.set_state(gst::State::Paused).is_ok() => {
                lock(&self.state).paused = true;
                self.set_hint("Paused - SPACE resumes");
            }
            gst::State::Paused if self.pipeline.set_state(gst::State::Playing).is_ok() => {
                lock(&self.state).paused = false;
                self.set_hint("Playing");
            }
            _ => {}
        }
    }

    fn seek_by(&self, forward: bool) {
        let (ready, ended, position) = {
            let state = lock(&self.state);
            (state.ready, state.ended, state.last_position_ms)
        };
        if !ready || ended {
            return;
        }
        let duration = self
            .pipeline
            .query_duration::<gst::ClockTime>()
            .map(|duration| duration.mseconds());
        let target = controls::seek_target(position, duration, forward);
        let Ok(seek_position) = gst::ClockTime::try_from(Duration::from_millis(target)) else {
            return;
        };
        if self
            .pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                seek_position,
            )
            .is_ok()
        {
            let mut state = lock(&self.state);
            state.last_position_ms = target;
            state.last_position_notification = None;
            drop(state);
            self.set_hint(&format!("Seek {} s", target / 1_000));
        }
    }

    fn adjust_volume(&self, up: bool) {
        let volume = {
            let mut state = lock(&self.state);
            let volume = controls::volume_step(state.volume, up);
            state.volume = volume;
            volume
        };
        self.pipeline
            .set_property("volume", f64::from(volume) / 100.0);
        self.set_hint(&format!("Volume {volume}%"));
    }

    fn handle_input_status(&self, available: bool, detail: Option<&'static str>) {
        let changed = {
            let mut state = lock(&self.state);
            let changed = state.input_available != available;
            state.input_available = available;
            changed
        };
        if !changed {
            return;
        }
        if available {
            log_line("tv-native-player: evdev controls available");
            let paused = lock(&self.state).paused;
            self.set_hint(if paused {
                "Paused - SPACE resumes"
            } else {
                "Controls ready"
            });
            return;
        }
        let detail = detail.unwrap_or("no input device");
        log_line(&format!(
            "tv-native-player: evdev controls unavailable: {detail}"
        ));
        self.set_hint(&format!("Controls unavailable: {detail}"));
        let notice = PlayerEvent::Degraded {
            code: "input_unavailable".to_string(),
            message: Some(detail.to_string()),
        };
        let ready = lock(&self.state).ready;
        if ready {
            emit(&notice);
        } else {
            lock(&self.state).pending_notices.push(notice);
        }
    }

    fn set_hint(&self, status: &str) {
        let Some(overlay) = self.overlay.as_ref() else {
            return;
        };
        let available = lock(&self.state).input_available;
        overlay.set_property("text", controls::hint_text(available, status));
    }

    fn load(&self, load: LoadRequest) {
        let start_position_ms = load.start_position_ms.unwrap_or(0);
        let volume = {
            let mut state = lock(&self.state);
            state.ready = false;
            state.ended = false;
            state.paused = false;
            state.last_position_ms = start_position_ms;
            state.last_position_notification = None;
            state.video_decoder = None;
            state.audio_decoder = None;
            state.headers = load.headers.unwrap_or_default();
            state.pending_seek_ms = (start_position_ms > 0).then_some(start_position_ms);
            state.volume
        };
        self.pipeline
            .set_property("volume", f64::from(volume) / 100.0);
        self.set_hint("Loading");
        let _ = self.pipeline.set_state(gst::State::Null);
        self.pipeline.set_property("uri", &load.url);
        if self.pipeline.set_state(gst::State::Playing).is_err() {
            emit(&PlayerEvent::Error {
                code: "state_failed".to_string(),
                message: None,
            });
        }
    }

    fn seek(&self, position_ms: u64) {
        let Ok(position) = gst::ClockTime::try_from(Duration::from_millis(position_ms)) else {
            emit(&PlayerEvent::Error {
                code: "invalid_request".to_string(),
                message: Some("seek position out of range".to_string()),
            });
            return;
        };
        let result = self
            .pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, position);
        if result.is_ok() {
            let mut state = lock(&self.state);
            state.last_position_ms = position_ms;
            state.last_position_notification = None;
            drop(state);
            self.set_hint(&format!("Seek {} s", position_ms / 1_000));
        } else {
            self.publish_position();
        }
    }

    fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.main_loop.quit();
    }

    fn publish_position(&self) {
        let should_emit = {
            let state = lock(&self.state);
            state.ready
                && !state.ended
                && matches!(
                    self.pipeline.current_state(),
                    gst::State::Playing | gst::State::Paused
                )
                && state
                    .last_position_notification
                    .is_none_or(|last| last.elapsed() >= POSITION_INTERVAL)
        };
        if !should_emit {
            return;
        }
        let position_ms = self
            .pipeline
            .query_position::<gst::ClockTime>()
            .map_or(0, |position| position.mseconds());
        let duration_ms = self
            .pipeline
            .query_duration::<gst::ClockTime>()
            .map(|duration| duration.mseconds());
        let mut state = lock(&self.state);
        // Keep a stable clock: a query that briefly reports a position before
        // the last emitted one must not move the UI backwards.
        let position_ms = position_ms.max(state.last_position_ms);
        let position_ms = duration_ms.map_or(position_ms, |duration| position_ms.min(duration));
        state.last_position_ms = position_ms;
        state.last_position_notification = Some(Instant::now());
        emit(&PlayerEvent::Position {
            position_ms,
            duration_ms,
        });
    }
}

/// Try the next audio candidate after an audio-sink error.
///
/// Returns `true` when the pipeline was restarted with a new sink. The retry
/// is bounded by the candidate list, so a broken audio device cannot loop.
fn try_audio_fallback(
    source: Option<&gst::Object>,
    pipeline: &gst::Element,
    state: &Mutex<PlaybackState>,
    audio: &Mutex<AudioChain>,
) -> bool {
    let from_audio = source
        .and_then(|source| source.downcast_ref::<gst::Element>())
        .and_then(|element| element.factory())
        .is_some_and(|factory| is_audio_factory(factory.name().as_str()));
    if !from_audio {
        return false;
    }
    let position = lock(state).last_position_ms;
    let replacement = {
        let mut chain = lock(audio);
        let element = chain.next();
        element.map(|element| (element, chain.label.clone(), chain.silent))
    };
    let Some((element, label, silent)) = replacement else {
        return false;
    };
    let _ = pipeline.set_state(gst::State::Null);
    pipeline.set_property("audio-sink", &element);
    {
        let mut state = lock(state);
        state.ready = false;
        if position > 0 {
            state.pending_seek_ms = Some(position);
        }
    }
    if pipeline.set_state(gst::State::Playing).is_err() {
        return false;
    }
    log_line(&format!("tv-native-player: audio sink fallback to {label}"));
    if silent {
        emit(&PlayerEvent::Degraded {
            code: "audio_unavailable".to_string(),
            message: Some("no audio device usable; video continues".to_string()),
        });
    }
    true
}

fn is_audio_factory(name: &str) -> bool {
    matches!(
        name,
        "pulsesink"
            | "alsasink"
            | "autoaudiosink"
            | "pipewiresink"
            | "osssink"
            | "oss4sink"
            | "fakesink"
    )
}

fn build_video_sink(
    notices: &mut Vec<PlayerEvent>,
) -> Result<(gst::Element, Option<gst::Element>, &'static str), &'static str> {
    let requested = overlay_requested();
    if requested {
        if overlay_supported()
            && let Ok((bin, overlay)) = build_overlay_bin()
        {
            return Ok((bin, Some(overlay), "waylandsink+textoverlay"));
        }
        notices.push(PlayerEvent::Degraded {
            code: "overlay_unavailable".to_string(),
            message: Some("textoverlay unavailable; controls stay active".to_string()),
        });
    }
    let sink = gst::ElementFactory::make("waylandsink")
        .build()
        .map_err(|_| "waylandsink is unavailable")?;
    Ok((sink, None, "waylandsink"))
}

fn overlay_requested() -> bool {
    match std::env::var(OVERLAY_ENV) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        ),
        Err(_) => true,
    }
}

/// Check that `textoverlay` actually prerolls with the local font stack.
///
/// The factory alone is not enough: a missing Pango/font backend fails later
/// inside the real pipeline. A one-buffer probe keeps playback predictable.
fn overlay_supported() -> bool {
    if gst::ElementFactory::find("textoverlay").is_none() {
        return false;
    }
    let pipeline = gst::Pipeline::new();
    let Ok(source) = gst::ElementFactory::make("videotestsrc").build() else {
        return false;
    };
    source.set_property("num-buffers", 1i32);
    let Ok(convert) = gst::ElementFactory::make("videoconvert").build() else {
        return false;
    };
    let Ok(overlay) = gst::ElementFactory::make("textoverlay").build() else {
        return false;
    };
    let Ok(sink) = gst::ElementFactory::make("fakesink").build() else {
        return false;
    };
    let elements = [&source, &convert, &overlay, &sink];
    if pipeline.add_many(elements).is_err() || gst::Element::link_many(elements).is_err() {
        return false;
    }
    let _ = pipeline.set_state(gst::State::Paused);
    let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(
        OVERLAY_PROBE_TIMEOUT_SECONDS.into(),
    ));
    let _ = pipeline.set_state(gst::State::Null);
    result.is_ok()
}

fn build_overlay_bin() -> Result<(gst::Element, gst::Element), ()> {
    let bin = gst::Bin::new();
    let queue = gst::ElementFactory::make("queue").build().map_err(|_| ())?;
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|_| ())?;
    let overlay = gst::ElementFactory::make("textoverlay")
        .build()
        .map_err(|_| ())?;
    let sink = gst::ElementFactory::make("waylandsink")
        .build()
        .map_err(|_| ())?;
    overlay.set_property("text", controls::hint_text(true, "Loading"));
    let elements = [&queue, &convert, &overlay, &sink];
    bin.add_many(elements).map_err(|_| ())?;
    gst::Element::link_many(elements).map_err(|_| ())?;
    let pad = queue.static_pad("sink").ok_or(())?;
    let ghost = gst::GhostPad::with_target(&pad).map_err(|_| ())?;
    bin.add_pad(&ghost).map_err(|_| ())?;
    Ok((bin.upcast::<gst::Element>(), overlay))
}

fn describe_video(sink: &gst::Element) -> Option<String> {
    let caps = sink.static_pad("sink")?.current_caps()?;
    let info = gstreamer_video::VideoInfo::from_caps(&caps).ok()?;
    Some(format!("{}x{}", info.width(), info.height()))
}

fn describe_audio(sink: &gst::Element) -> Option<String> {
    let caps = sink.static_pad("sink")?.current_caps()?;
    let info = gstreamer_audio::AudioInfo::from_caps(&caps).ok()?;
    Some(format!("{} Hz, {} ch", info.rate(), info.channels()))
}

/// Bounded sink label for the `ready` event.
fn sink_summary(video: &str, audio_label: &str) -> String {
    format!("{video}; audio={audio_label}")
        .chars()
        .take(120)
        .collect()
}

/// Static diagnostics: stderr for foreground runs and the kernel log for the
/// kiosk, whose parent discards stderr. Never pass URLs or headers here.
fn log_line(message: &str) {
    let message: String = message
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_LOG_BYTES)
        .collect();
    eprintln!("{message}");
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::net::UnixDatagram;
        if let Ok(socket) = UnixDatagram::unbound() {
            let _ = socket.send_to(format!("<14>{message}").as_bytes(), "/dev/log");
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_input_reader(sender: std_mpsc::Sender<Inbound>) {
    let _ = std::thread::Builder::new()
        .name("player-input".to_string())
        .spawn(move || read_input_devices(sender));
}

#[cfg(not(target_os = "linux"))]
fn spawn_input_reader(sender: std_mpsc::Sender<Inbound>) {
    let _ = sender.send(Inbound::InputStatus {
        available: false,
        detail: Some("unsupported platform"),
    });
}

/// Read Linux evdev key presses without grabbing a device.
///
/// The compositor keeps its own read of every keyboard; this is an additive,
/// non-exclusive reader for the fullscreen player surface. Devices are
/// rescanned so a remote plugged in after boot starts working.
#[cfg(target_os = "linux")]
fn read_input_devices(sender: std_mpsc::Sender<Inbound>) {
    // O_NONBLOCK is 0o4000 on every Linux architecture. Pulling in `libc` only
    // for this one flag would add a dependency for no behavioural gain.
    const O_NONBLOCK: i32 = 0o4000;
    let mut devices: Vec<(PathBuf, File)> = Vec::new();
    let mut announced_unavailable = false;
    let mut last_scan: Option<Instant> = None;
    let mut buffer = [0u8; controls::EVDEV_EVENT_BYTES];
    loop {
        if last_scan.is_none_or(|last| last.elapsed() >= INPUT_RESCAN_INTERVAL) {
            last_scan = Some(Instant::now());
            let scan = scan_input_devices(&mut devices, O_NONBLOCK);
            if devices.is_empty() && !announced_unavailable {
                announced_unavailable = true;
                let status = Inbound::InputStatus {
                    available: false,
                    detail: Some(scan.failure_detail()),
                };
                if sender.send(status).is_err() {
                    return;
                }
            } else if !devices.is_empty() && announced_unavailable {
                announced_unavailable = false;
                let status = Inbound::InputStatus {
                    available: true,
                    detail: None,
                };
                if sender.send(status).is_err() {
                    return;
                }
            }
        }
        for index in (0..devices.len()).rev() {
            let mut remove = false;
            for _ in 0..MAX_EVENTS_PER_DEVICE {
                match devices[index].1.read(&mut buffer) {
                    Ok(0) => {
                        remove = true;
                        break;
                    }
                    Ok(count) if count == buffer.len() => {
                        if let Some(action) = controls::parse_evdev(&buffer)
                            && sender.send(Inbound::Key(action)).is_err()
                        {
                            return;
                        }
                    }
                    Ok(_) => break,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        remove = true;
                        break;
                    }
                }
            }
            if remove {
                devices.swap_remove(index);
            }
        }
        std::thread::sleep(INPUT_POLL_INTERVAL);
    }
}

#[cfg(target_os = "linux")]
struct InputScan {
    present: bool,
    denied: bool,
}

#[cfg(target_os = "linux")]
impl InputScan {
    fn failure_detail(&self) -> &'static str {
        if self.denied {
            "permission denied"
        } else if self.present {
            "no readable device"
        } else {
            "no input device"
        }
    }
}

#[cfg(target_os = "linux")]
fn scan_input_devices(devices: &mut Vec<(PathBuf, File)>, flags: i32) -> InputScan {
    use std::os::unix::fs::OpenOptionsExt;

    let mut scan = InputScan {
        present: false,
        denied: false,
    };
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return scan;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_event_node)
        })
        .collect();
    paths.sort();
    for path in paths {
        if devices.len() >= MAX_INPUT_DEVICES {
            break;
        }
        scan.present = true;
        if devices.iter().any(|(open, _)| *open == path) {
            continue;
        }
        match OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(&path)
        {
            Ok(file) => devices.push((path, file)),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => scan.denied = true,
            Err(_) => {}
        }
    }
    scan
}

#[cfg(target_os = "linux")]
fn is_event_node(name: &str) -> bool {
    name.strip_prefix("event").is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn emit(event: &PlayerEvent) {
    let Ok(line) = player::encode_event(event) else {
        return;
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if writeln!(handle, "{line}")
        .and_then(|()| handle.flush())
        .is_err()
    {
        std::process::exit(0);
    }
}
