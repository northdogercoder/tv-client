//! Feature-independent tests for the native-player protocol and supervision.
//!
//! These tests use a scripted fake child rather than GStreamer, so they run in
//! the headless gate (`--no-default-features`) on any host.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tv_client::player::{
    ChildCommand, ChildExit, ChildFactory, ChildMessage, CommandOutcome, LoadRequest,
    MAX_LINE_BYTES, PlayerCommand, PlayerError, PlayerEvent, PlayerMachine, PlayerNotification,
    PlayerRequest, PlayerSession, PlayerState, SupervisedChild, SupervisorConfig,
    decode_event_line, decode_request_line, encode_event, encode_request, read_bounded_line,
};

#[derive(Clone)]
enum Behavior {
    Happy {
        decoder: Option<String>,
        sink: Option<String>,
    },
    Silent,
    Fail {
        code: String,
        message: String,
    },
    ExitOnLoad,
    EndAfterReady,
}

struct FakeFactory {
    behaviors: VecDeque<Behavior>,
    spawns: Arc<AtomicUsize>,
    exits: Arc<AtomicUsize>,
}

impl FakeFactory {
    fn new(behaviors: impl IntoIterator<Item = Behavior>) -> Self {
        Self {
            behaviors: behaviors.into_iter().collect(),
            spawns: Arc::new(AtomicUsize::new(0)),
            exits: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn spawns(&self) -> usize {
        self.spawns.load(Ordering::SeqCst)
    }

    fn exits(&self) -> usize {
        self.exits.load(Ordering::SeqCst)
    }
}

impl ChildFactory for FakeFactory {
    fn spawn(
        &mut self,
    ) -> impl std::future::Future<Output = Result<SupervisedChild, PlayerError>> + Send {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let behavior = self.behaviors.pop_front().unwrap_or(Behavior::Silent);
        let exits = Arc::clone(&self.exits);
        async move { Ok(fake_child(behavior, exits)) }
    }
}

fn fake_child(behavior: Behavior, exits: Arc<AtomicUsize>) -> SupervisedChild {
    let (control_tx, mut control_rx) = mpsc::channel::<ChildCommand>(8);
    let (message_tx, message_rx) = mpsc::channel::<ChildMessage>(32);
    tokio::spawn(async move {
        loop {
            match control_rx.recv().await {
                Some(ChildCommand::Line(line)) => {
                    let Ok(request) = decode_request_line(&line) else {
                        send_event(
                            &message_tx,
                            PlayerEvent::Error {
                                code: "bad_request".to_string(),
                                message: None,
                            },
                        )
                        .await;
                        continue;
                    };
                    match request {
                        PlayerRequest::Load(load) => match &behavior {
                            Behavior::Happy { decoder, sink } => {
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Ready {
                                        decoder: decoder.clone(),
                                        sink: sink.clone(),
                                    },
                                )
                                .await;
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Tracks {
                                        video: decoder.clone(),
                                        audio: Some("autoaudiosink".to_string()),
                                        subtitle: None,
                                    },
                                )
                                .await;
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Position {
                                        position_ms: load.start_position_ms.unwrap_or(0),
                                        duration_ms: Some(60_000),
                                    },
                                )
                                .await;
                            }
                            Behavior::Fail { code, message } => {
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Error {
                                        code: code.clone(),
                                        message: Some(message.clone()),
                                    },
                                )
                                .await;
                            }
                            Behavior::ExitOnLoad => {
                                send_exit(&message_tx, &exits).await;
                                return;
                            }
                            Behavior::EndAfterReady => {
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Ready {
                                        decoder: None,
                                        sink: None,
                                    },
                                )
                                .await;
                                send_event(&message_tx, PlayerEvent::Ended).await;
                            }
                            Behavior::Silent => {}
                        },
                        PlayerRequest::Seek { position_ms } => {
                            if matches!(&behavior, Behavior::Happy { .. }) {
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Position {
                                        position_ms,
                                        duration_ms: Some(60_000),
                                    },
                                )
                                .await;
                            }
                        }
                        PlayerRequest::Stop => {
                            send_exit(&message_tx, &exits).await;
                            return;
                        }
                        PlayerRequest::Pause
                        | PlayerRequest::Resume
                        | PlayerRequest::SetVolume { .. } => {}
                    }
                }
                Some(ChildCommand::Kill) | None => {
                    send_exit(&message_tx, &exits).await;
                    return;
                }
            }
        }
    });
    SupervisedChild::from_channels(control_tx, message_rx)
}

async fn send_event(messages: &mpsc::Sender<ChildMessage>, event: PlayerEvent) {
    let line = encode_event(&event).expect("event encodes");
    let _ = messages.send(ChildMessage::Line(line)).await;
}

async fn send_exit(messages: &mpsc::Sender<ChildMessage>, exits: &AtomicUsize) {
    exits.fetch_add(1, Ordering::SeqCst);
    let _ = messages
        .send(ChildMessage::Exited(ChildExit {
            success: true,
            code: Some(0),
        }))
        .await;
}

fn config() -> SupervisorConfig {
    SupervisorConfig {
        ready_timeout: Duration::from_millis(200),
        event_timeout: Duration::from_secs(3),
        reap_timeout: Duration::from_millis(300),
        spawn_timeout: Duration::from_millis(500),
        send_timeout: Duration::from_millis(500),
    }
}

fn load(url: &str) -> PlayerCommand {
    PlayerCommand::Load(LoadRequest::new(url))
}

async fn recv_until<F>(session: &mut PlayerSession, mut predicate: F) -> PlayerNotification
where
    F: FnMut(&PlayerNotification) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let notification = tokio::time::timeout(remaining, session.next())
            .await
            .expect("notification timed out")
            .expect("notification channel closed");
        if predicate(&notification) {
            return notification;
        }
    }
}

async fn expect_state(session: &mut PlayerSession, state: PlayerState) -> PlayerNotification {
    recv_until(session, |notification| {
        matches!(notification, PlayerNotification::State { state: seen, .. } if *seen == state)
    })
    .await
}

#[test]
fn debug_output_never_contains_url_or_header_secrets() {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer SECRET-TOKEN".to_string(),
    );
    let request = PlayerRequest::Load(LoadRequest {
        url: "https://jellyfin.local/Videos/1/stream?api_key=SECRET-TOKEN&static=true".to_string(),
        headers: Some(headers),
        start_position_ms: Some(1_000),
    });
    request.validate().expect("valid request");

    let encoded = encode_request(&request).expect("encodable");
    assert!(!encoded.contains('\n'), "protocol line must be single-line");
    let decoded = decode_request_line(&encoded).expect("decodable");
    assert_eq!(decoded, request);

    let debug = format!("{request:?}");
    assert!(!debug.contains("SECRET-TOKEN"), "debug leaked: {debug}");
    assert!(debug.contains("jellyfin.local"));
    assert!(debug.contains("redacted"));
}

#[test]
fn protocol_rejects_malformed_unknown_and_out_of_range_values() {
    assert!(decode_request_line("{\"type\":\"nope\"}").is_err());
    assert!(decode_request_line("not json").is_err());
    assert!(decode_request_line("").is_err());
    assert!(decode_request_line("{\"type\":\"set_volume\",\"volume\":101}").is_err());
    assert!(decode_request_line("{\"type\":\"load\",\"url\":\"javascript:alert(1)\"}").is_err());
    assert!(decode_request_line("{\"type\":\"load\",\"url\":\"https://user:pw@host/x\"}").is_err());
    assert!(
        decode_request_line(
            "{\"type\":\"load\",\"url\":\"file:///tmp/a.mp4\",\"headers\":{\"X Bad\":\"v\"}}"
        )
        .is_err()
    );
    assert!(decode_event_line("{\"type\":\"nope\"}").is_err());
    assert!(decode_event_line("{\"type\":\"error\",\"code\":\"bad code\"}").is_err());

    let oversized = format!(
        "{{\"type\":\"load\",\"url\":\"file:///{}\"}}",
        "a".repeat(MAX_LINE_BYTES)
    );
    assert_eq!(
        decode_request_line(&oversized),
        Err(PlayerError::LineTooLong)
    );
}

#[tokio::test]
async fn bounded_line_reader_rejects_oversized_input_without_buffering_it() {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut good = BufReader::new(&b"{\"type\":\"pause\"}\n"[..]);
    let line = read_bounded_line(&mut good)
        .await
        .expect("bounded read")
        .expect("line present");
    assert_eq!(line, "{\"type\":\"pause\"}");
    good.consume(1);

    let mut input = vec![b'a'; MAX_LINE_BYTES + 1024];
    input.push(b'\n');
    let mut oversized = BufReader::new(&input[..]);
    assert_eq!(
        read_bounded_line(&mut oversized).await,
        Err(PlayerError::LineTooLong)
    );
}

#[test]
fn redaction_removes_query_strings_and_control_bytes() {
    let redacted = tv_client::player::redact_text(
        "failed https://jellyfin.local/x?api_key=SECRET and\r\nmore",
    );
    assert!(!redacted.contains("SECRET"), "redacted: {redacted}");
    assert!(!redacted.contains('\n'), "redacted: {redacted}");
    assert!(redacted.contains("jellyfin.local"));

    let event = decode_event_line(
        "{\"type\":\"error\",\"code\":\"gst_error\",\"message\":\"https://host/x?token=SECRET\"}",
    )
    .expect("valid event");
    let debug = format!("{event:?}");
    assert!(!debug.contains("SECRET"), "debug: {debug}");
}

#[test]
fn late_events_are_rejected_after_a_new_load_generation() {
    let mut machine = PlayerMachine::new();
    let first = machine
        .apply_command(load("file:///tmp/first.mp4"))
        .expect("load accepted");
    assert_eq!(first.generation, 1);
    assert_eq!(machine.state(), PlayerState::Loading);

    machine
        .apply_event(
            first.generation,
            PlayerEvent::Ready {
                decoder: Some("v4l2h264dec".to_string()),
                sink: Some("waylandsink".to_string()),
            },
        )
        .expect("ready accepted");
    assert_eq!(machine.state(), PlayerState::Playing);

    let second = machine
        .apply_command(load("file:///tmp/second.mp4"))
        .expect("replacement load accepted");
    assert_eq!(second.generation, 2);
    assert_eq!(machine.state(), PlayerState::Loading);

    let stale = machine.apply_event(
        first.generation,
        PlayerEvent::Position {
            position_ms: 1_000,
            duration_ms: Some(60_000),
        },
    );
    assert_eq!(
        stale,
        Err(PlayerError::StaleGeneration {
            expected: 2,
            received: 1,
        })
    );
    assert_eq!(machine.position_ms(), Some(0));
}

#[test]
fn seek_and_volume_bounds_are_enforced_before_io() {
    let mut machine = PlayerMachine::new();
    let outcome: CommandOutcome = machine
        .apply_command(load("file:///tmp/a.mp4"))
        .expect("load");
    machine
        .apply_event(
            outcome.generation,
            PlayerEvent::Position {
                position_ms: 0,
                duration_ms: Some(60_000),
            },
        )
        .expect("position");
    machine
        .apply_event(
            outcome.generation,
            PlayerEvent::Ready {
                decoder: None,
                sink: None,
            },
        )
        .expect("ready");

    assert!(matches!(
        machine.apply_command(PlayerCommand::Seek {
            position_ms: 60_001,
        }),
        Err(PlayerError::InvalidRequest { .. })
    ));
    assert!(matches!(
        machine.apply_command(PlayerCommand::SetVolume { volume: 101 }),
        Err(PlayerError::InvalidRequest { .. })
    ));
    let accepted = machine
        .apply_command(PlayerCommand::SetVolume { volume: 100 })
        .expect("volume accepted");
    assert_eq!(
        accepted.request,
        Some(PlayerRequest::SetVolume { volume: 100 })
    );
    assert_eq!(machine.volume(), 100);

    // Idempotent pause and stop succeed without new I/O.
    let pause = machine.apply_command(PlayerCommand::Pause).expect("pause");
    assert!(pause.request.is_some());
    let paused_again = machine
        .apply_command(PlayerCommand::Pause)
        .expect("pause is idempotent");
    assert!(paused_again.request.is_none());
    assert!(paused_again.notifications.is_empty());
    machine.apply_command(PlayerCommand::Stop).expect("stop");
    let stop_again = machine
        .apply_command(PlayerCommand::Stop)
        .expect("stop is idempotent");
    assert!(stop_again.request.is_none());
    assert!(stop_again.notifications.is_empty());
}

#[tokio::test]
async fn normal_transitions_positions_and_clean_stop() {
    let mut factory = FakeFactory::new([Behavior::Happy {
        decoder: Some("v4l2h264dec".to_string()),
        sink: Some("waylandsink".to_string()),
    }]);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), config(), 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/fixture.mp4"))
        .await
        .expect("send");

    expect_state(&mut session, PlayerState::Loading).await;
    let ready = recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Ready { .. })
    })
    .await;
    match ready {
        PlayerNotification::Ready { decoder, sink, .. } => {
            assert_eq!(decoder.as_deref(), Some("v4l2h264dec"));
            assert_eq!(sink.as_deref(), Some("waylandsink"));
        }
        other => panic!("unexpected notification: {other:?}"),
    }
    expect_state(&mut session, PlayerState::Playing).await;
    recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Position { .. })
    })
    .await;

    session
        .send(PlayerCommand::SetVolume { volume: 25 })
        .await
        .expect("volume");
    session.send(PlayerCommand::Pause).await.expect("pause");
    expect_state(&mut session, PlayerState::Paused).await;
    session.send(PlayerCommand::Resume).await.expect("resume");
    expect_state(&mut session, PlayerState::Playing).await;

    session
        .send(PlayerCommand::Seek {
            position_ms: 30_000,
        })
        .await
        .expect("seek");
    let position = recv_until(&mut session, |notification| {
        matches!(
            notification,
            PlayerNotification::Position {
                position_ms: 30_000,
                ..
            }
        )
    })
    .await;
    assert!(matches!(position, PlayerNotification::Position { .. }));

    let machine_after_stop = {
        session.send(PlayerCommand::Stop).await.expect("stop");
        expect_state(&mut session, PlayerState::Idle).await;
        session.shutdown().await.expect("clean shutdown")
    };
    assert_eq!(machine_after_stop.state(), PlayerState::Idle);
}

/// Helper so tests keep direct access to the factory's counters after moving it
/// into a session. The factory shares its counters behind `Arc`.
fn factory_proxy(factory: &mut FakeFactory) -> FakeFactory {
    FakeFactory {
        behaviors: std::mem::take(&mut factory.behaviors),
        spawns: Arc::clone(&factory.spawns),
        exits: Arc::clone(&factory.exits),
    }
}

#[tokio::test]
async fn hung_child_is_killed_and_reported_without_a_real_decoder() {
    let mut factory = FakeFactory::new([Behavior::Silent]);
    let mut fast = config();
    fast.ready_timeout = Duration::from_millis(80);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), fast, 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/hung.mp4"))
        .await
        .expect("send");

    let failed = recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Failed { .. })
    })
    .await;
    match failed {
        PlayerNotification::Failed { code, .. } => assert_eq!(code, "child_hung"),
        other => panic!("unexpected notification: {other:?}"),
    }
    expect_state(&mut session, PlayerState::Error).await;
    session.shutdown().await.expect("clean shutdown");
    assert_eq!(factory.spawns(), 1);
    // The fake child records its exit; a real child is killed and reaped the
    // same way by `teardown_child`.
    assert_eq!(factory.exits(), 1);
}

#[tokio::test]
async fn child_error_is_reported_with_a_redacted_message() {
    let mut factory = FakeFactory::new([Behavior::Fail {
        code: "gst_error".to_string(),
        message: "decode failed at https://jellyfin.local/x?api_key=SECRET".to_string(),
    }]);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), config(), 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/broken.mp4"))
        .await
        .expect("send");

    let failed = recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Failed { .. })
    })
    .await;
    match failed {
        PlayerNotification::Failed { code, message, .. } => {
            assert_eq!(code, "gst_error");
            let message = message.expect("message present");
            assert!(!message.contains("SECRET"), "message leaked: {message}");
            assert!(!message.contains("api_key"), "message leaked: {message}");
        }
        other => panic!("unexpected notification: {other:?}"),
    }
    expect_state(&mut session, PlayerState::Error).await;
    let machine = session.shutdown().await.expect("clean shutdown");
    assert_eq!(machine.state(), PlayerState::Error);
    let failure = machine.failure().expect("failure retained");
    assert_eq!(failure.code, "gst_error");
    assert!(!format!("{failure:?}").contains("SECRET"));
}

#[tokio::test]
async fn unexpected_child_exit_is_reported_as_a_failure() {
    let mut factory = FakeFactory::new([Behavior::ExitOnLoad]);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), config(), 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/exit.mp4"))
        .await
        .expect("send");

    let failed = recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Failed { .. })
    })
    .await;
    match failed {
        PlayerNotification::Failed { code, .. } => assert_eq!(code, "child_exited"),
        other => panic!("unexpected notification: {other:?}"),
    }
    expect_state(&mut session, PlayerState::Error).await;
    session.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn stop_is_idempotent_and_restart_uses_a_new_child_and_generation() {
    let mut factory = FakeFactory::new([
        Behavior::Happy {
            decoder: None,
            sink: None,
        },
        Behavior::Happy {
            decoder: None,
            sink: None,
        },
    ]);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), config(), 8, 64).expect("session starts");

    session
        .send(load("file:///tmp/one.mp4"))
        .await
        .expect("load one");
    expect_state(&mut session, PlayerState::Loading).await;
    expect_state(&mut session, PlayerState::Playing).await;
    let first_generation = {
        let notification = recv_until(&mut session, |notification| {
            matches!(notification, PlayerNotification::Position { .. })
        })
        .await;
        match notification {
            PlayerNotification::Position { generation, .. } => generation,
            other => panic!("unexpected notification: {other:?}"),
        }
    };

    session.send(PlayerCommand::Stop).await.expect("stop");
    expect_state(&mut session, PlayerState::Idle).await;
    // A second stop is a no-op: no notification arrives.
    session.send(PlayerCommand::Stop).await.expect("stop again");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), session.next())
            .await
            .is_err(),
        "second stop must not emit a state change"
    );

    session
        .send(load("file:///tmp/two.mp4"))
        .await
        .expect("load two");
    let loading = expect_state(&mut session, PlayerState::Loading).await;
    match loading {
        PlayerNotification::State { generation, .. } => {
            assert_eq!(generation, first_generation + 1);
        }
        other => panic!("unexpected notification: {other:?}"),
    }
    expect_state(&mut session, PlayerState::Playing).await;
    assert_eq!(factory.spawns(), 2);
    assert_eq!(factory.exits(), 1);
    session.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn ending_tears_down_the_child_and_reports_ended() {
    let mut factory = FakeFactory::new([Behavior::EndAfterReady]);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), config(), 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/end.mp4"))
        .await
        .expect("send");

    expect_state(&mut session, PlayerState::Loading).await;
    expect_state(&mut session, PlayerState::Playing).await;
    recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Ended { .. })
    })
    .await;
    expect_state(&mut session, PlayerState::Ended).await;

    let machine = session.shutdown().await.expect("clean shutdown");
    assert_eq!(machine.state(), PlayerState::Ended);
    assert_eq!(factory.exits(), 1);

    // Events for an ended generation are rejected rather than reviving playback.
    let mut ended = machine;
    let generation = ended.generation();
    assert!(matches!(
        ended.apply_event(
            generation,
            PlayerEvent::Position {
                position_ms: 1,
                duration_ms: None,
            },
        ),
        Err(PlayerError::InvalidState {
            state: PlayerState::Ended
        })
    ));
}

#[test]
fn invalid_session_capacity_is_rejected() {
    let error = match PlayerSession::start(FakeFactory::new(Vec::<Behavior>::new()), config(), 0, 8)
    {
        Ok(_) => panic!("zero capacity accepted"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        PlayerError::InvalidConfig {
            field: "queue capacity"
        }
    );
}
