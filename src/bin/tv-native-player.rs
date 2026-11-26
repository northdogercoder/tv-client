//! Supervised native GStreamer playback child.
//!
//! The process speaks the JSON-lines protocol in [`tv_client::player`] on
//! stdin/stdout. It is started by the parent UI, receives the media URL and
//! optional authentication headers over the pipe (never argv or environment),
//! and exits on `stop`, stdin EOF or the default SIGTERM action.
//!
//! `Back`/`Esc` is routed by the parent (C008); this child has no window
//! manager policy of its own and never reads the keyboard.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::glib::prelude::*;
use gstreamer::prelude::*;
use tv_client::player::{self, LoadRequest, PlayerEvent, PlayerRequest, redact_text};

const POSITION_INTERVAL: Duration = Duration::from_millis(500);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            // Static, secret-free diagnostics only.
            eprintln!("tv-native-player: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), &'static str> {
    gst::init().map_err(|_| "gstreamer initialization failed")?;

    let (inbound_tx, inbound_rx) = std_mpsc::channel::<Inbound>();
    std::thread::Builder::new()
        .name("player-stdin".to_string())
        .spawn(move || read_stdin(inbound_tx))
        .map_err(|_| "stdin reader failed")?;

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

#[derive(Default)]
struct PlaybackState {
    ready: bool,
    ended: bool,
    last_position_ms: u64,
    last_position_notification: Option<Instant>,
    video_decoder: Option<String>,
    audio_decoder: Option<String>,
    headers: BTreeMap<String, String>,
    pending_seek_ms: Option<u64>,
    volume: u8,
}

struct Player {
    pipeline: gst::Element,
    main_loop: glib::MainLoop,
    state: Arc<Mutex<PlaybackState>>,
    // Dropping the guard removes the bus watch, so keep it for the process
    // lifetime.
    _bus_watch: gst::bus::BusWatchGuard,
}

fn lock(state: &Mutex<PlaybackState>) -> MutexGuard<'_, PlaybackState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Player {
    fn new() -> Result<Rc<Self>, &'static str> {
        let pipeline = gst::ElementFactory::make("playbin3")
            .build()
            .map_err(|_| "playbin3 is unavailable")?;

        let video_sink = gst::ElementFactory::make("waylandsink")
            .build()
            .map_err(|_| "waylandsink is unavailable")?;
        let audio_sink = gst::ElementFactory::make("pulsesink")
            .build()
            .or_else(|_| gst::ElementFactory::make("autoaudiosink").build())
            .map_err(|_| "no audio sink is available")?;
        pipeline.set_property("video-sink", &video_sink);
        pipeline.set_property("audio-sink", &audio_sink);

        let bus = pipeline.bus().ok_or("playbin3 has no bus")?;
        let main_loop = glib::MainLoop::new(None, false);
        let state = Arc::new(Mutex::new(PlaybackState {
            volume: player::DEFAULT_VOLUME,
            ..PlaybackState::default()
        }));

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
        let bus_pipeline = pipeline.clone();
        let bus_video_sink = video_sink;
        let bus_audio_sink = audio_sink;
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
                                        if state.ready {
                                            false
                                        } else {
                                            state.ready = true;
                                            true
                                        }
                                    };
                                    if first_ready {
                                        let (decoder, audio) = {
                                            let state = lock(&bus_state);
                                            (
                                                state.video_decoder.clone(),
                                                state.audio_decoder.clone(),
                                            )
                                        };
                                        emit(&PlayerEvent::Ready {
                                            decoder: decoder.clone(),
                                            sink: Some("waylandsink".to_string()),
                                        });
                                        emit(&PlayerEvent::Tracks {
                                            video: describe_video(&bus_video_sink).or(decoder),
                                            audio: describe_audio(&bus_audio_sink).or(audio),
                                            subtitle: None,
                                        });
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
            _bus_watch: bus_watch,
        }))
    }

    fn poll(&self, inbound: &std_mpsc::Receiver<Inbound>) {
        for _ in 0..32 {
            match inbound.try_recv() {
                Ok(Inbound::Request(request)) => self.handle(request),
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
                if self.pipeline.set_state(gst::State::Paused).is_err() {
                    emit(&PlayerEvent::Error {
                        code: "state_failed".to_string(),
                        message: None,
                    });
                }
            }
            PlayerRequest::Resume => {
                if self.pipeline.set_state(gst::State::Playing).is_err() {
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
            }
            PlayerRequest::Stop => self.stop(),
        }
    }

    fn load(&self, load: LoadRequest) {
        let start_position_ms = load.start_position_ms.unwrap_or(0);
        let volume = {
            let mut state = lock(&self.state);
            state.ready = false;
            state.ended = false;
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
