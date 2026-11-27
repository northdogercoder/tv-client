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
    MAX_LINE_BYTES, MAX_POSITION_MS, PlayerCommand, PlayerError, PlayerEvent, PlayerMachine,
    PlayerNotification, PlayerRequest, PlayerSession, PlayerState, SupervisedChild,
    SupervisorConfig, audio, controls, decode_event_line, decode_request_line, encode_event,
    encode_request, read_bounded_line,
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
    /// Play, then stop from the native surface and exit like the real child.
    StoppedByUser,
    MalformedOnLoad,
    RequireVolume,
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
        let mut loaded = false;
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
                            Behavior::StoppedByUser => {
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Ready {
                                        decoder: None,
                                        sink: None,
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
                                send_event(&message_tx, PlayerEvent::Stopped).await;
                                send_exit(&message_tx, &exits).await;
                                return;
                            }
                            Behavior::Silent => {}
                            Behavior::MalformedOnLoad => {
                                let _ = message_tx
                                    .send(ChildMessage::Line("{\"type\":\"position\"".to_string()))
                                    .await;
                            }
                            Behavior::RequireVolume => {
                                loaded = true;
                            }
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
                        PlayerRequest::SetVolume { volume } => {
                            if matches!(&behavior, Behavior::RequireVolume) && loaded {
                                loaded = false;
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Tracks {
                                        video: None,
                                        audio: Some(format!("volume:{volume}")),
                                        subtitle: None,
                                    },
                                )
                                .await;
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Ready {
                                        decoder: None,
                                        sink: None,
                                    },
                                )
                                .await;
                                send_event(
                                    &message_tx,
                                    PlayerEvent::Position {
                                        position_ms: 0,
                                        duration_ms: Some(60_000),
                                    },
                                )
                                .await;
                            }
                        }
                        PlayerRequest::Pause | PlayerRequest::Resume => {}
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
    assert!(
        decode_request_line(&format!(
            "{{\"type\":\"seek\",\"position_ms\":{}}}",
            MAX_POSITION_MS + 1
        ))
        .is_err()
    );
    assert!(
        decode_request_line(&format!(
            "{{\"type\":\"load\",\"url\":\"file:///tmp/a.mp4\",\"start_position_ms\":{}}}",
            u64::MAX
        ))
        .is_err()
    );

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

#[test]
fn huge_positions_are_rejected_before_any_child_can_overflow() {
    let mut machine = PlayerMachine::new();
    let outcome = machine
        .apply_command(load("file:///tmp/huge.mp4"))
        .expect("load");
    machine
        .apply_event(
            outcome.generation,
            PlayerEvent::Ready {
                decoder: None,
                sink: None,
            },
        )
        .expect("ready");
    assert_eq!(machine.state(), PlayerState::Playing);

    assert!(matches!(
        machine.apply_command(PlayerCommand::Seek {
            position_ms: MAX_POSITION_MS + 1,
        }),
        Err(PlayerError::InvalidRequest { .. })
    ));
    assert!(matches!(
        machine.apply_command(PlayerCommand::Seek {
            position_ms: u64::MAX,
        }),
        Err(PlayerError::InvalidRequest { .. })
    ));

    let mut huge = LoadRequest::new("file:///tmp/huge-start.mp4");
    huge.start_position_ms = Some(MAX_POSITION_MS + 1);
    assert!(machine.apply_command(PlayerCommand::Load(huge)).is_err());
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
async fn ready_deadline_is_cleared_once_the_child_is_ready() {
    let mut factory = FakeFactory::new([Behavior::Happy {
        decoder: Some("v4l2h264dec".to_string()),
        sink: Some("waylandsink".to_string()),
    }]);
    let mut fast = config();
    fast.ready_timeout = Duration::from_millis(100);
    fast.event_timeout = Duration::from_secs(1);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), fast, 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/ready.mp4"))
        .await
        .expect("send");

    expect_state(&mut session, PlayerState::Loading).await;
    expect_state(&mut session, PlayerState::Playing).await;
    recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Position { .. })
    })
    .await;

    // The child is deliberately silent for longer than ready_timeout. A healthy
    // session must not turn that into child_hung after it already became ready.
    let quiet = tokio::time::timeout(Duration::from_millis(300), session.next()).await;
    assert!(
        quiet.is_err(),
        "healthy session produced a late failure: {quiet:?}"
    );

    // Pause is only accepted from Playing, which also proves no hidden Error.
    session.send(PlayerCommand::Pause).await.expect("pause");
    expect_state(&mut session, PlayerState::Paused).await;
    let machine = session.shutdown().await.expect("clean shutdown");
    assert_eq!(machine.state(), PlayerState::Paused);
}

#[tokio::test]
async fn protocol_failure_is_not_overwritten_by_a_later_hung_deadline() {
    let mut factory = FakeFactory::new([Behavior::MalformedOnLoad]);
    let mut fast = config();
    fast.ready_timeout = Duration::from_millis(100);
    fast.event_timeout = Duration::from_secs(1);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), fast, 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/bad-protocol.mp4"))
        .await
        .expect("send");

    let failed = recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Failed { .. })
    })
    .await;
    match failed {
        PlayerNotification::Failed { code, .. } => assert_eq!(code, "protocol"),
        other => panic!("unexpected notification: {other:?}"),
    }
    expect_state(&mut session, PlayerState::Error).await;

    let quiet = tokio::time::timeout(Duration::from_millis(300), session.next()).await;
    assert!(
        quiet.is_err(),
        "failure was overwritten by a deadline: {quiet:?}"
    );

    let machine = session.shutdown().await.expect("clean shutdown");
    assert_eq!(
        machine.failure().expect("failure retained").code,
        "protocol"
    );
}

#[tokio::test]
async fn new_child_receives_the_current_volume_after_load() {
    let mut factory = FakeFactory::new([Behavior::RequireVolume]);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), config(), 8, 64).expect("session starts");

    // Set the volume while no child exists, then load. The supervisor must
    // apply the retained volume to the fresh child.
    session
        .send(PlayerCommand::SetVolume { volume: 25 })
        .await
        .expect("volume");
    session
        .send(load("file:///tmp/volume.mp4"))
        .await
        .expect("load");

    expect_state(&mut session, PlayerState::Loading).await;
    let tracks = recv_until(&mut session, |notification| {
        matches!(notification, PlayerNotification::Tracks { .. })
    })
    .await;
    match tracks {
        PlayerNotification::Tracks { audio, .. } => {
            assert_eq!(audio.as_deref(), Some("volume:25"));
        }
        other => panic!("unexpected notification: {other:?}"),
    }
    expect_state(&mut session, PlayerState::Playing).await;
    session.shutdown().await.expect("clean shutdown");
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
fn audio_plan_prefers_alsa_for_the_kiosk_and_keeps_a_desktop_default() {
    // A generic desktop has no configuration and keeps GStreamer's detection.
    let plan = audio::AudioSinkPlan::from_values(None, None);
    assert_eq!(plan.candidates().len(), 1);
    assert_eq!(plan.candidates()[0].factory(), "autoaudiosink");
    assert!(plan.warning().is_none());

    // The kiosk asks for the deterministic HDMI path, then the host ALSA
    // default, then GStreamer's auto-detection.
    let plan = audio::AudioSinkPlan::from_values(Some("alsa"), None);
    let factories: Vec<_> = plan
        .candidates()
        .iter()
        .map(audio::AudioSinkSpec::factory)
        .collect();
    assert_eq!(factories, ["alsasink", "alsasink", "autoaudiosink"]);
    assert_eq!(plan.candidates()[0].device(), Some("hw:0,0"));
    assert_eq!(plan.candidates()[1].device(), Some("default"));
    assert!(plan.describe().contains("hw:0,0"));
    assert!(plan.warning().is_none());

    let plan = audio::AudioSinkPlan::from_values(Some("ALSA"), Some(" plughw:1,3 "));
    assert_eq!(plan.candidates()[0].device(), Some("plughw:1,3"));

    // A device with a newline or other unsafe bytes must never reach GStreamer
    // or a log line; the safe HDMI default is used instead.
    let plan = audio::AudioSinkPlan::from_values(Some("alsa"), Some("hw:0,0\ninjected"));
    assert_eq!(plan.candidates()[0].device(), Some("hw:0,0"));

    let plan = audio::AudioSinkPlan::from_values(Some("bogus"), None);
    assert_eq!(plan.warning(), Some("unknown_audio_sink"));
    assert_eq!(plan.candidates()[0].factory(), "autoaudiosink");

    let plan = audio::AudioSinkPlan::from_values(Some("pulse"), None);
    let factories: Vec<_> = plan
        .candidates()
        .iter()
        .map(audio::AudioSinkSpec::factory)
        .collect();
    assert_eq!(factories, ["pulsesink", "autoaudiosink"]);

    assert_eq!(audio::silent_fallback().factory(), "fakesink");
}

#[test]
fn alsa_device_card_parsing_is_exact() {
    assert_eq!(audio::alsa_card("hw:0,0"), Some(0));
    assert_eq!(audio::alsa_card("plughw:2,7"), Some(2));
    assert_eq!(audio::alsa_card("default"), None);
    assert_eq!(audio::alsa_card("hw:CARD=vc4hdmi,DEV=0"), None);
    assert_eq!(audio::alsa_card(""), None);
}

#[test]
fn evdev_key_mapping_is_bounded_and_ignores_releases() {
    use controls::KeyAction;

    assert_eq!(
        controls::map_key(controls::KEY_ESC, 1),
        Some(KeyAction::Stop)
    );
    assert_eq!(
        controls::map_key(controls::KEY_BACK, 1),
        Some(KeyAction::Stop)
    );
    assert_eq!(
        controls::map_key(controls::KEY_SPACE, 1),
        Some(KeyAction::TogglePause)
    );
    assert_eq!(
        controls::map_key(controls::KEY_PLAYPAUSE, 1),
        Some(KeyAction::TogglePause)
    );
    assert_eq!(
        controls::map_key(controls::KEY_LEFT, 1),
        Some(KeyAction::SeekBackward)
    );
    assert_eq!(
        controls::map_key(controls::KEY_RIGHT, 1),
        Some(KeyAction::SeekForward)
    );
    assert_eq!(
        controls::map_key(controls::KEY_UP, 1),
        Some(KeyAction::VolumeUp)
    );
    assert_eq!(
        controls::map_key(controls::KEY_DOWN, 1),
        Some(KeyAction::VolumeDown)
    );
    // Releases, kernel repeats and unknown codes are ignored, never executed.
    assert_eq!(controls::map_key(controls::KEY_ESC, 0), None);
    assert_eq!(controls::map_key(controls::KEY_ESC, 2), None);
    assert_eq!(controls::map_key(29_999, 1), None);
}

#[test]
fn evdev_records_are_parsed_only_as_full_key_presses() {
    use controls::KeyAction;

    let mut record = [0u8; controls::EVDEV_EVENT_BYTES];
    let tail = record.len() - 8;
    record[tail..tail + 2].copy_from_slice(&controls::EVDEV_KEY.to_ne_bytes());
    record[tail + 2..tail + 4].copy_from_slice(&controls::KEY_SPACE.to_ne_bytes());
    record[tail + 4..].copy_from_slice(&1i32.to_ne_bytes());
    assert_eq!(controls::parse_evdev(&record), Some(KeyAction::TogglePause));

    record[tail + 4..].copy_from_slice(&0i32.to_ne_bytes());
    assert_eq!(controls::parse_evdev(&record), None, "release ignored");

    record[tail..tail + 2].copy_from_slice(&0u16.to_ne_bytes());
    record[tail + 4..].copy_from_slice(&1i32.to_ne_bytes());
    assert_eq!(
        controls::parse_evdev(&record),
        None,
        "non-key event ignored"
    );

    assert_eq!(controls::parse_evdev(&record[..record.len() - 1]), None);
}

#[test]
fn seek_volume_and_hint_stay_inside_the_protocol_bounds() {
    assert_eq!(controls::seek_target(5_000, Some(60_000), true), 15_000);
    assert_eq!(controls::seek_target(5_000, Some(60_000), false), 0);
    assert_eq!(controls::seek_target(59_000, Some(60_000), true), 60_000);
    assert_eq!(
        controls::seek_target(MAX_POSITION_MS - 1_000, None, true),
        MAX_POSITION_MS
    );

    assert_eq!(controls::volume_step(0, false), 0);
    assert_eq!(controls::volume_step(100, true), 100);
    assert_eq!(controls::volume_step(50, true), 55);
    assert_eq!(controls::volume_step(50, false), 45);

    let hint = controls::hint_text(true, "");
    assert!(hint.contains("ESC"));
    assert!(hint.contains("SPACE"));
    let unavailable = controls::hint_text(false, "permission denied");
    assert!(unavailable.contains("unavailable"));
    assert!(unavailable.contains("permission denied"));

    // Status text cannot inject extra lines or grow without bound.
    let injected = controls::hint_text(true, "one\nsecond\nthird");
    assert!(injected.lines().count() <= 2);
    let huge = controls::hint_text(true, &"z".repeat(4_096));
    assert!(huge.len() <= controls::MAX_HINT_BYTES);
}

#[test]
fn degraded_and_stopped_events_are_typed_and_never_fail() {
    let mut machine = PlayerMachine::new();
    machine
        .apply_command(load("file:///tmp/local-stop.mp4"))
        .expect("load accepted");
    let generation = machine.generation();
    machine
        .apply_event(
            generation,
            PlayerEvent::Ready {
                decoder: None,
                sink: None,
            },
        )
        .expect("ready accepted");
    assert_eq!(machine.state(), PlayerState::Playing);

    let notes = machine
        .apply_event(
            generation,
            PlayerEvent::Degraded {
                code: "input_unavailable".to_string(),
                message: Some("permission denied".to_string()),
            },
        )
        .expect("degraded accepted");
    assert!(matches!(
        notes.as_slice(),
        [PlayerNotification::Degraded { code, .. }] if code == "input_unavailable"
    ));
    assert_eq!(machine.state(), PlayerState::Playing);
    assert!(machine.failure().is_none());

    let notes = machine
        .apply_event(generation, PlayerEvent::Stopped)
        .expect("stopped accepted");
    assert!(matches!(
        notes.as_slice(),
        [PlayerNotification::State {
            state: PlayerState::Idle,
            ..
        }]
    ));
    assert_eq!(machine.state(), PlayerState::Idle);
    assert!(machine.failure().is_none());
    assert!(
        machine
            .apply_event(generation, PlayerEvent::Stopped)
            .expect("second stopped accepted")
            .is_empty(),
        "a repeated stop must not emit another notification"
    );
}

#[test]
fn stopped_and_degraded_event_lines_round_trip_with_bounded_text() {
    assert_eq!(
        decode_event_line("{\"type\":\"stopped\"}").expect("stopped decodes"),
        PlayerEvent::Stopped
    );
    let degraded = decode_event_line(
        "{\"type\":\"degraded\",\"code\":\"audio_unavailable\",\"message\":\"no device\"}",
    )
    .expect("degraded decodes");
    match degraded {
        PlayerEvent::Degraded { code, message } => {
            assert_eq!(code, "audio_unavailable");
            assert_eq!(message.as_deref(), Some("no device"));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    assert!(decode_event_line("{\"type\":\"degraded\",\"code\":\"bad code\"}").is_err());

    let redacted = decode_event_line(
        "{\"type\":\"degraded\",\"code\":\"input_unavailable\",\"message\":\"open http://x/y?token=SECRET failed\"}",
    )
    .expect("degraded decodes");
    let text = format!("{redacted:?}");
    assert!(!text.contains("SECRET"), "degraded detail leaked: {text}");
    assert!(!text.contains("token="), "degraded detail leaked: {text}");
}

#[tokio::test]
async fn local_stop_returns_to_the_launcher_without_a_failure() {
    let mut factory = FakeFactory::new([Behavior::StoppedByUser]);
    let mut session =
        PlayerSession::start(factory_proxy(&mut factory), config(), 8, 64).expect("session starts");
    session
        .send(load("file:///tmp/local-stop.mp4"))
        .await
        .expect("load");

    expect_state(&mut session, PlayerState::Loading).await;
    expect_state(&mut session, PlayerState::Playing).await;
    // The child sends `stopped` and exits. The parent must treat that as a
    // clean return to the launcher, not as `child_exited`.
    expect_state(&mut session, PlayerState::Idle).await;
    let quiet = tokio::time::timeout(Duration::from_millis(300), session.next()).await;
    assert!(
        quiet.is_err(),
        "local stop produced an extra notification: {quiet:?}"
    );

    let machine = session.shutdown().await.expect("clean shutdown");
    assert_eq!(machine.state(), PlayerState::Idle);
    assert!(machine.failure().is_none());
    assert_eq!(factory.exits(), 1);
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

/// Pure control-bar policy: labels, seek mapping, visibility and keys. These
/// run in the headless gate and never touch GStreamer, Slint or Wayland.
mod overlay_policy {
    use std::time::Duration;

    use tv_client::player::MAX_POSITION_MS;
    use tv_client::player::controls;
    use tv_client::player::overlay::{
        AUTO_HIDE, OverlayAction, OverlayState, UNKNOWN_TIME, format_time, fraction_from_x,
        map_key, position_from_fraction, position_from_x, progress, remaining_time, seek_target,
        window_enabled_from_env,
    };

    fn at(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    #[test]
    fn labels_are_compact_and_bounded() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(59_999), "0:59");
        assert_eq!(format_time(3_600_000), "1:00:00");
        assert_eq!(format_time(MAX_POSITION_MS), "24:00:00");
        assert_eq!(remaining_time(30_000, Some(90_000)), "-1:00");
        assert_eq!(remaining_time(90_000, None), UNKNOWN_TIME);
        assert!((progress(30_000, Some(90_000)) - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(progress(30_000, None), 0.0);
    }

    #[test]
    fn pointer_and_keyboard_seeks_share_one_mapping() {
        assert_eq!(position_from_x(0.0, 1280.0, Some(600_000)), Some(0));
        assert_eq!(position_from_x(640.0, 1280.0, Some(600_000)), Some(300_000));
        assert_eq!(
            position_from_x(2000.0, 1280.0, Some(600_000)),
            Some(600_000)
        );
        assert_eq!(fraction_from_x(-20.0, 1280.0), 0.0);
        assert_eq!(position_from_fraction(0.25, 600_000), 150_000);
        assert_eq!(position_from_fraction(2.0, 600_000), 600_000);
        assert_eq!(
            seek_target(OverlayAction::SeekForward, 590_000, Some(600_000)),
            Some(600_000)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekBackward, 5_000, Some(600_000)),
            Some(0)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekForward, 0, None),
            Some(controls::SEEK_STEP_MS)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekToStart, 5_000, None),
            Some(0)
        );
        assert_eq!(
            seek_target(OverlayAction::SeekToEnd, 5_000, Some(600_000)),
            Some(600_000)
        );
        assert_eq!(seek_target(OverlayAction::SeekToEnd, 5_000, None), None);
    }

    #[test]
    fn bar_shows_on_activity_and_hides_after_three_seconds() {
        let mut state = OverlayState::new(at(0));
        assert!(state.visible());
        assert!(state.tick(at(AUTO_HIDE.as_millis() as u64)));
        assert!(!state.visible());
        state.note_activity(at(10_000));
        assert!(state.visible());
        assert_eq!(
            state.key_press("\u{f703}", at(10_100)),
            Some(OverlayAction::SeekForward)
        );
        assert!(state.tick(at(13_100)));
        assert!(!state.tick(at(13_200)));
    }

    #[test]
    fn paused_playback_keeps_the_bar_visible() {
        let mut state = OverlayState::new(at(0));
        state.set_paused(true, at(0));
        assert!(!state.tick(at(60_000)));
        assert!(state.visible());
        state.set_paused(false, at(60_100));
        assert!(!state.tick(at(62_500)));
        assert!(state.tick(at(63_100)));
    }

    #[test]
    fn keys_map_to_bounded_actions_once_per_press() {
        assert_eq!(map_key("\u{001b}"), Some(OverlayAction::Stop));
        assert_eq!(map_key("\u{0020}"), Some(OverlayAction::TogglePause));
        assert_eq!(map_key("\u{f702}"), Some(OverlayAction::SeekBackward));
        assert_eq!(map_key("\u{f703}"), Some(OverlayAction::SeekForward));
        assert_eq!(map_key("\u{f729}"), Some(OverlayAction::SeekToStart));
        assert_eq!(map_key("\u{f72b}"), Some(OverlayAction::SeekToEnd));
        assert_eq!(map_key("x"), None);
        assert_eq!(map_key("esc"), None);

        let mut state = OverlayState::new(at(0));
        assert_eq!(
            state.key_press("\u{0020}", at(10)),
            Some(OverlayAction::TogglePause)
        );
        // Kernel/Wayland repeat: held keys never fire twice.
        assert_eq!(state.key_press("\u{0020}", at(20)), None);
        state.key_release("\u{0020}");
        assert_eq!(
            state.key_press("\u{0020}", at(30)),
            Some(OverlayAction::TogglePause)
        );
    }

    #[test]
    fn deactivation_hides_the_bar_and_ignores_late_activity() {
        let mut state = OverlayState::new(at(0));
        state.deactivate();
        assert!(!state.visible());
        assert!(!state.active());
        state.note_activity(at(100));
        assert!(!state.visible());
        assert!(!state.tick(at(10_000)));
    }

    #[test]
    fn the_window_switch_is_on_by_default_and_can_be_turned_off() {
        assert!(window_enabled_from_env(None));
        assert!(window_enabled_from_env(Some("on")));
        assert!(!window_enabled_from_env(Some("off")));
        assert!(!window_enabled_from_env(Some(" 0 ")));
        assert!(!window_enabled_from_env(Some("false")));
    }
}
