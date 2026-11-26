//! Contract tests for enrollment, pairing, device socket and remote actions.
//!
//! These tests use a local mock WebSocket server and a recording HTTP
//! transport. They do not contact the staging server and make no claim that a
//! live enrollment has happened.

// The tungstenite handshake callback requires an unboxed error response.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::ErrorResponse;
use tokio_tungstenite::tungstenite::http::{Request, Response, StatusCode};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_async, accept_hdr_async};
use uuid::Uuid;

use tv_client::cloud::{
    self, AckPayload, AckStatus, ApplyOutcome, ApplyRequest, COMMAND_APPLY_TIMEOUT,
    COMMAND_QUEUE_CAPACITY, CloudError, CloudHttp, CommandApplier, CommandPolicy, DEDUPE_CAPACITY,
    DedupeCache, DeviceLink, DeviceToken, EnrollmentToken, HttpTransport, InboundMessage,
    LinkConfig, LinkEnd, LinkEvent, PROTOCOL_VERSION, PairError, PairRequestPayload,
    PairingSession, PairingState, ProtocolError, ReconnectPolicy, StatePayload, parse_inbound,
};
use tv_client::model::{Action, AppState, InputOrigin, Mode, Source};
use tv_client::playback::PlaybackUpdate;
use tv_client::state::shutdown_channel;
use tv_client::ui::UiModel;

const CLIENT_VERSION: &str = "0.1.0-test";
const TOKEN_TEXT: &str = "test-device-token-1234567890";

fn device_token() -> DeviceToken {
    DeviceToken::new(TOKEN_TEXT).expect("valid test token")
}

fn home_state(device_id: Uuid) -> StatePayload {
    StatePayload::from_app_state(&AppState::default(), device_id, false)
        .expect("default state is valid")
}

fn link_config(ws_url: String, device_id: Uuid) -> LinkConfig {
    let mut config = LinkConfig::new(
        "https://staging.example.test",
        device_id,
        CLIENT_VERSION,
        cloud::capabilities(true, false),
    )
    .expect("link config");
    config.ws_url = ws_url;
    config.queue_capacity = COMMAND_QUEUE_CAPACITY;
    config.reconnect = ReconnectPolicy::new(Duration::from_millis(10), Duration::from_millis(25))
        .expect("reconnect policy");
    config
}

fn envelope(kind: &str, payload: Value) -> String {
    serde_json::to_string(&json!({
        "v": PROTOCOL_VERSION,
        "type": kind,
        "id": Uuid::new_v4(),
        "payload": payload,
    }))
    .expect("envelope")
}

async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    (listener, format!("ws://{address}/v1/ws/device"))
}

async fn accept(stream: TcpStream) -> WebSocketStream<TcpStream> {
    accept_async(stream).await.expect("websocket handshake")
}

async fn recv_json(ws: &mut WebSocketStream<TcpStream>) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .expect("frame timeout")
        .expect("stream ended")
        .expect("frame error");
    match message {
        Message::Text(text) => serde_json::from_str(text.as_str()).expect("json frame"),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

async fn send_json(ws: &mut WebSocketStream<TcpStream>, kind: &str, payload: Value) {
    ws.send(Message::Text(envelope(kind, payload).into()))
        .await
        .expect("send frame");
}

async fn expect_hello_and_state(ws: &mut WebSocketStream<TcpStream>, device_id: Uuid) -> Value {
    let hello = recv_json(ws).await;
    assert_eq!(hello["v"], PROTOCOL_VERSION);
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["payload"]["device_id"], device_id.to_string());
    assert_eq!(hello["payload"]["client_version"], CLIENT_VERSION);
    assert!(
        hello["payload"]["capabilities"]
            .as_array()
            .expect("capabilities")
            .iter()
            .any(|capability| capability == "jellyfin")
    );

    let state = recv_json(ws).await;
    assert_eq!(state["type"], "state");
    assert!(state["id"].as_str().is_some());
    assert_eq!(state["payload"]["device_id"], device_id.to_string());
    assert_eq!(state["payload"]["seq"], 1);
    assert_eq!(state["payload"]["mode"], "home");
    state
}

struct CountingApplier {
    calls: Arc<AtomicUsize>,
}

impl CommandApplier for CountingApplier {
    fn apply(&self, _request: ApplyRequest) -> BoxFuture<'_, ApplyOutcome> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            ApplyOutcome::Applied
        })
    }
}

struct GatedApplier {
    gate: Arc<Notify>,
    started: mpsc::UnboundedSender<()>,
}

impl CommandApplier for GatedApplier {
    fn apply(&self, _request: ApplyRequest) -> BoxFuture<'_, ApplyOutcome> {
        let gate = Arc::clone(&self.gate);
        let started = self.started.clone();
        Box::pin(async move {
            let _ = started.send(());
            gate.notified().await;
            ApplyOutcome::Applied
        })
    }
}

struct BlockingApplier {
    started: mpsc::UnboundedSender<()>,
    gate: Arc<Notify>,
}

impl CommandApplier for BlockingApplier {
    fn apply(&self, _request: ApplyRequest) -> BoxFuture<'_, ApplyOutcome> {
        let started = self.started.clone();
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            let _ = started.send(());
            gate.notified().await;
            ApplyOutcome::Applied
        })
    }
}

#[derive(Clone)]
struct RecordingTransport {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    responses: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    url: String,
    bearer: Option<String>,
    body: Option<Vec<u8>>,
}

impl RecordingTransport {
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn respond(&self, path: &str, body: Value) {
        let bytes = serde_json::to_vec(&body).expect("response json");
        self.responses
            .lock()
            .expect("responses")
            .insert(path.to_owned(), bytes);
    }

    fn last(&self) -> RecordedRequest {
        self.requests
            .lock()
            .expect("requests")
            .last()
            .cloned()
            .expect("recorded request")
    }
}

impl HttpTransport for RecordingTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        bearer: Option<&'a str>,
        body: Option<&'a [u8]>,
    ) -> BoxFuture<'a, Result<Vec<u8>, CloudError>> {
        let requests = Arc::clone(&self.requests);
        let responses = Arc::clone(&self.responses);
        let method = method.to_owned();
        let url = url.to_owned();
        let bearer = bearer.map(str::to_owned);
        let body = body.map(|bytes| bytes.to_vec());
        Box::pin(async move {
            let path = url::Url::parse(&url)
                .map_err(|_| CloudError::Transport)?
                .path()
                .to_owned();
            requests.lock().expect("requests").push(RecordedRequest {
                method,
                path: path.clone(),
                url,
                bearer,
                body,
            });
            responses
                .lock()
                .expect("responses")
                .get(&path)
                .cloned()
                .ok_or(CloudError::NotFound)
        })
    }
}

fn spawn_link(
    config: LinkConfig,
    state_rx: watch::Receiver<StatePayload>,
    events: mpsc::Sender<LinkEvent>,
    applier: Arc<dyn CommandApplier>,
    policy: CommandPolicy,
    shutdown: tv_client::state::ShutdownSignal,
) -> tokio::task::JoinHandle<LinkEnd> {
    let link =
        DeviceLink::new(config, device_token(), state_rx, events, applier, policy).expect("link");
    tokio::spawn(link.run(shutdown))
}

#[test]
fn protocol_fixture_parity_and_invalid_actions() {
    let fixture: Value =
        serde_json::from_str(include_str!("../docs/protocol-fixtures.json")).expect("fixture json");
    assert_eq!(fixture["version"], PROTOCOL_VERSION);

    let command = serde_json::to_vec(&fixture["command"]).expect("command fixture");
    match parse_inbound(&command).expect("fixture command parses") {
        InboundMessage::Command(payload) => {
            assert_eq!(
                payload.action,
                Action::PlayYoutube {
                    video_id: "M7lc1UVf-VE".to_owned()
                }
            );
        }
        other => panic!("unexpected fixture message: {other:?}"),
    }

    let ack: AckPayload =
        serde_json::from_value(fixture["ack"]["payload"].clone()).expect("ack fixture");
    assert_eq!(ack.status, AckStatus::Applied);
    assert_eq!(ack.reason, None);

    let state: StatePayload =
        serde_json::from_value(fixture["state"].clone()).expect("state fixture");
    assert_eq!(state.seq, 1);
    assert_eq!(state.mode, Mode::Home);
    state.validate().expect("fixture state is valid");

    for action in fixture["invalid_actions"]
        .as_array()
        .expect("invalid action array")
    {
        let frame = json!({
            "v": PROTOCOL_VERSION,
            "type": "command",
            "id": Uuid::new_v4(),
            "payload": {
                "command_id": Uuid::new_v4(),
                "action": action,
            },
        });
        assert!(
            parse_inbound(serde_json::to_vec(&frame).expect("frame").as_slice()).is_err(),
            "invalid action parsed: {action}"
        );
    }
}

#[tokio::test]
async fn hello_state_bearer_header_and_applied_ack() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let observed_header = Arc::new(Mutex::new(None::<String>));

    let header_slot = Arc::clone(&observed_header);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let ws = accept_hdr_async(
            stream,
            move |request: &Request<()>,
                  response: Response<()>|
                  -> Result<Response<()>, ErrorResponse> {
                *header_slot.lock().expect("header") = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                Ok(response)
            },
        )
        .await
        .expect("handshake");
        let mut ws = ws;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        let command_id = Uuid::new_v4();
        send_json(
            &mut ws,
            "command",
            json!({
                "command_id": command_id,
                "action": {"kind": "set_volume", "volume": 10},
            }),
        )
        .await;
        let ack = recv_json(&mut ws).await;
        assert_eq!(ack["type"], "ack");
        assert_eq!(ack["payload"]["command_id"], command_id.to_string());
        assert_eq!(ack["payload"]["status"], "applied");
        assert_eq!(ack["payload"]["reason"], Value::Null);
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let applier = Arc::new(CountingApplier {
        calls: Arc::clone(&calls),
    });
    let link = spawn_link(
        link_config(ws_url.clone(), device_id),
        state_rx,
        events,
        applier,
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    assert_eq!(
        observed_header.lock().expect("header").as_deref(),
        Some("Bearer test-device-token-1234567890")
    );
    assert!(!ws_url.contains(TOKEN_TEXT));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn ack_is_sent_only_after_the_application_finishes() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let gate = Arc::new(Notify::new());
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let applier = Arc::new(GatedApplier {
        gate: Arc::clone(&gate),
        started: started_tx,
    });

    let server_gate = Arc::clone(&gate);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        let command_id = Uuid::new_v4();
        send_json(
            &mut ws,
            "command",
            json!({
                "command_id": command_id,
                "action": {"kind": "set_volume", "volume": 20},
            }),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .expect("application started");
        let early = tokio::time::timeout(Duration::from_millis(250), recv_json(&mut ws)).await;
        assert!(early.is_err(), "ack arrived before application completed");
        server_gate.notify_waiters();
        let ack = recv_json(&mut ws).await;
        assert_eq!(ack["payload"]["command_id"], command_id.to_string());
        assert_eq!(ack["payload"]["status"], "applied");
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        applier,
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn duplicate_commands_are_acked_without_reapplying() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let applier_calls = Arc::clone(&calls);

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        let command_id = Uuid::new_v4();
        let command = json!({
            "command_id": command_id,
            "action": {"kind": "set_volume", "volume": 30},
        });
        send_json(&mut ws, "command", command.clone()).await;
        let first = recv_json(&mut ws).await;
        assert_eq!(first["payload"]["status"], "applied");
        send_json(&mut ws, "command", command).await;
        let second = recv_json(&mut ws).await;
        assert_eq!(second["payload"]["status"], "applied");
        assert_eq!(second["payload"]["command_id"], command_id.to_string());
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let applier = Arc::new(CountingApplier {
        calls: applier_calls,
    });
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        applier,
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn invalid_mode_is_rejected_without_application() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let applier_calls = Arc::clone(&calls);

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        send_json(
            &mut ws,
            "command",
            json!({
                "command_id": Uuid::new_v4(),
                "action": {"kind": "set_paused", "paused": true},
            }),
        )
        .await;
        let ack = recv_json(&mut ws).await;
        assert_eq!(ack["payload"]["status"], "rejected");
        assert_eq!(ack["payload"]["reason"], "invalid_mode");
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let applier = Arc::new(CountingApplier {
        calls: applier_calls,
    });
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        applier,
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn play_youtube_is_rejected_until_the_c010_playback_path_exists() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let applier_calls = Arc::clone(&calls);

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        send_json(
            &mut ws,
            "command",
            json!({
                "command_id": Uuid::new_v4(),
                "action": {"kind": "play_youtube", "video_id": "M7lc1UVf-VE"},
            }),
        )
        .await;
        let ack = recv_json(&mut ws).await;
        assert_eq!(ack["payload"]["status"], "rejected");
        assert_eq!(ack["payload"]["reason"], "unsupported_source");
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let applier = Arc::new(CountingApplier {
        calls: applier_calls,
    });
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        applier,
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn malformed_and_unknown_messages_close_with_protocol_status() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        ws.send(Message::Text(envelope("shell", json!({})).into()))
            .await
            .expect("send");
        let close = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("close timeout")
            .expect("stream")
            .expect("frame");
        match close {
            Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Policy),
            other => panic!("expected policy close, got {other:?}"),
        }
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn unsupported_version_closes_with_1002() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        ws.send(Message::Text(
            serde_json::to_string(&json!({
                "v": 2,
                "type": "hello_ack",
                "id": Uuid::new_v4(),
                "payload": {"connection_id": Uuid::new_v4()},
            }))
            .expect("json")
            .into(),
        ))
        .await
        .expect("send");
        let close = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("close timeout")
            .expect("stream")
            .expect("frame");
        match close {
            Message::Close(Some(frame)) => assert_eq!(frame.code, CloseCode::Protocol),
            other => panic!("expected protocol close, got {other:?}"),
        }
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn heartbeat_pings_keep_an_idle_connection_alive() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_millis(750) {
            let message = tokio::time::timeout(Duration::from_millis(250), ws.next()).await;
            match message {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    ws.send(Message::Pong(payload)).await.expect("pong");
                }
                Ok(Some(Ok(Message::Pong(_)))) => {}
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
            }
        }
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, mut events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let mut config = link_config(ws_url, device_id);
    config.heartbeat_interval = Duration::from_millis(50);
    config.heartbeat_timeout = Duration::from_millis(500);
    let link = spawn_link(
        config,
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut disconnected = false;
    while let Ok(event) = events_rx.try_recv() {
        if matches!(event, LinkEvent::Disconnected(_) | LinkEvent::Revoked) {
            disconnected = true;
        }
    }
    assert!(!disconnected, "heartbeat timeout closed an alive socket");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
    server.abort();
}

#[tokio::test]
async fn reconnect_after_disconnect_uses_a_fresh_connection() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("first accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        drop(ws);

        let (stream, _) = listener.accept().await.expect("second accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, mut events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    let mut connected = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while connected < 2 && tokio::time::Instant::now() < deadline {
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(500), events_rx.recv()).await
            && matches!(event, LinkEvent::Connected)
        {
            connected += 1;
        }
    }
    assert_eq!(connected, 2, "client did not reconnect");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

fn reject_unauthorized(
    _request: &Request<()>,
    _response: Response<()>,
) -> Result<Response<()>, ErrorResponse> {
    let rejection: ErrorResponse = Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(Some("unauthorized".to_owned()))
        .expect("response");
    Err(rejection)
}

#[tokio::test]
async fn revoked_handshake_stops_reconnection() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let _ = accept_hdr_async(stream, reject_unauthorized).await;
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, mut events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    let end = tokio::time::timeout(Duration::from_secs(3), link)
        .await
        .expect("link ended")
        .expect("join");
    assert_eq!(end, LinkEnd::Revoked);
    let mut revoked = false;
    while let Ok(event) = events_rx.try_recv() {
        if matches!(event, LinkEvent::Revoked) {
            revoked = true;
        }
    }
    assert!(revoked, "revocation event missing");
    shutdown.cancel();
    server.abort();
}

#[tokio::test]
async fn pairing_request_is_pushed_and_remote_cannot_decide() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let request_id = Uuid::new_v4();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        send_json(
            &mut ws,
            "pair_request",
            json!({
                "request_id": request_id,
                "comparison_code": "1234",
                "expires_at": "2026-11-26T12:02:00Z",
            }),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, mut events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    let payload = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events_rx.recv().await {
                Some(LinkEvent::Connected) | Some(LinkEvent::HelloAck(_)) => {}
                Some(LinkEvent::PairRequest(payload)) => break payload,
                Some(other) => panic!("unexpected event: {other:?}"),
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("pair request event");
    assert_eq!(payload.request_id, request_id);
    assert_eq!(payload.comparison_code, "1234");

    let now = cloud::parse_rfc3339("2026-11-26T12:01:00Z").expect("now");
    let mut state = PairingState::new();
    state
        .begin(cloud::PairInit {
            pair_id: Uuid::new_v4(),
            code: "00012345".to_owned(),
            expires_at: "2026-11-26T12:02:00Z".to_owned(),
        })
        .expect("code");
    state
        .on_request(
            PairRequestPayload {
                request_id,
                comparison_code: "1234".to_owned(),
                expires_at: "2026-11-26T12:02:00Z".to_owned(),
            },
            now,
        )
        .expect("pending");
    assert_eq!(
        state.decide(InputOrigin::Remote, request_id, true, now),
        Err(PairError::RemoteDenied)
    );
    assert!(state.pending().is_some());

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn pair_decision_posts_only_after_local_physical_choice() {
    let transport = Arc::new(RecordingTransport::new());
    transport.respond(
        "/v1/pair/init",
        json!({
            "pair_id": Uuid::new_v4(),
            "code": "00012345",
            "expires_at": "2026-11-26T12:02:00Z",
        }),
    );
    transport.respond("/v1/pair/decision", json!(null));
    let http = CloudHttp::with_transport("https://staging.example.test", transport.clone())
        .expect("http")
        .with_token(device_token());
    let mut session = PairingSession::new(http);
    let code = session.begin().await.expect("pair init");
    assert_eq!(code.code, "00012345");

    let request_id = Uuid::new_v4();
    let now = cloud::parse_rfc3339("2026-11-26T12:01:00Z").expect("now");
    session
        .on_request(
            PairRequestPayload {
                request_id,
                comparison_code: "4321".to_owned(),
                expires_at: "2026-11-26T12:02:00Z".to_owned(),
            },
            now,
        )
        .expect("pending");
    let decision = session
        .decide(InputOrigin::LocalKeyboard, request_id, true, now)
        .await
        .expect("decision");
    assert_eq!(decision.request_id, request_id);
    assert!(decision.accept);

    let recorded = transport.last();
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/pair/decision");
    assert!(!recorded.url.contains(TOKEN_TEXT));
    assert_eq!(
        recorded.bearer.as_deref(),
        Some("Bearer test-device-token-1234567890")
    );
    let body: Value =
        serde_json::from_slice(recorded.body.as_deref().expect("body")).expect("decision json");
    assert_eq!(body["request_id"], request_id.to_string());
    assert_eq!(body["accept"].as_bool(), Some(true));
}

#[test]
fn expired_pairing_cannot_be_decided() {
    let now = cloud::parse_rfc3339("2026-11-26T12:01:00Z").expect("now");
    let mut state = PairingState::new();
    state
        .begin(cloud::PairInit {
            pair_id: Uuid::new_v4(),
            code: "00012345".to_owned(),
            expires_at: "2026-11-26T12:02:00Z".to_owned(),
        })
        .expect("code");
    let request_id = Uuid::new_v4();
    state
        .on_request(
            PairRequestPayload {
                request_id,
                comparison_code: "1234".to_owned(),
                expires_at: "2026-11-26T12:02:00Z".to_owned(),
            },
            now,
        )
        .expect("pending");
    let after = now + Duration::from_secs(120);
    assert_eq!(
        state.decide(InputOrigin::LocalCec, request_id, true, after),
        Err(PairError::Expired)
    );
}

#[tokio::test]
async fn command_queue_is_bounded_and_rejects_overflow() {
    assert_eq!(COMMAND_QUEUE_CAPACITY, 32);
    assert_eq!(COMMAND_APPLY_TIMEOUT, Duration::from_secs(10));
    assert_eq!(DEDUPE_CAPACITY, 256);

    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let gate = Arc::new(Notify::new());
    let applier = Arc::new(BlockingApplier {
        started: started_tx,
        gate: Arc::clone(&gate),
    });
    let third_id = Uuid::new_v4();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        send_json(
            &mut ws,
            "command",
            json!({
                "command_id": first,
                "action": {"kind": "set_volume", "volume": 40},
            }),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .expect("first application started");
        for command_id in [second, third_id] {
            send_json(
                &mut ws,
                "command",
                json!({
                    "command_id": command_id,
                    "action": {"kind": "set_volume", "volume": 40},
                }),
            )
            .await;
        }
        let ack = recv_json(&mut ws).await;
        assert_eq!(ack["payload"]["command_id"], third_id.to_string());
        assert_eq!(ack["payload"]["status"], "rejected");
        assert_eq!(ack["payload"]["reason"], "queue_full");
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let mut config = link_config(ws_url, device_id);
    config.queue_capacity = 1;
    let link = spawn_link(
        config,
        state_rx,
        events,
        applier,
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn offline_and_unreachable_cloud_is_not_fatal() {
    struct FailingTransport;
    impl HttpTransport for FailingTransport {
        fn request<'a>(
            &'a self,
            _method: &'a str,
            _url: &'a str,
            _bearer: Option<&'a str>,
            _body: Option<&'a [u8]>,
        ) -> BoxFuture<'a, Result<Vec<u8>, CloudError>> {
            Box::pin(async { Err(CloudError::Transport) })
        }
    }

    let http =
        CloudHttp::with_transport("https://staging.example.test", Arc::new(FailingTransport))
            .expect("http")
            .with_token(device_token());
    assert!(matches!(
        http.device_config().await,
        Err(CloudError::Transport)
    ));

    let device_id = Uuid::new_v4();
    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, mut events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let mut config = link_config("ws://127.0.0.1:9/v1/ws/device".to_owned(), device_id);
    config.reconnect = ReconnectPolicy::new(Duration::from_millis(10), Duration::from_millis(20))
        .expect("reconnect");
    let link = spawn_link(
        config,
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut saw_disconnect = false;
    while let Ok(event) = events_rx.try_recv() {
        if matches!(event, LinkEvent::Disconnected(_)) {
            saw_disconnect = true;
        }
    }
    assert!(saw_disconnect, "unreachable cloud should retry, not exit");
    shutdown.cancel();
    let end = tokio::time::timeout(Duration::from_secs(2), link)
        .await
        .expect("link stops")
        .expect("join");
    assert_eq!(end, LinkEnd::Shutdown);
    assert_eq!(tv_client::version(), "0.1.0");
}

#[tokio::test]
async fn enrollment_posts_the_token_and_stores_a_private_credential() {
    let secret = "enrollment-token-value-1234567890";
    let device_id = Uuid::new_v4();
    let device_secret = "device-token-value-1234567890";
    let transport = Arc::new(RecordingTransport::new());
    transport.respond(
        "/v1/enroll",
        json!({
            "device_id": device_id,
            "device_token": device_secret,
            "protocol": PROTOCOL_VERSION,
        }),
    );
    let http =
        CloudHttp::with_transport("https://staging.example.test", transport.clone()).expect("http");
    let enrollment_token = EnrollmentToken::new(secret).expect("token");
    let enrollment = http
        .enroll(&enrollment_token, "living-room")
        .await
        .expect("enrollment");
    assert_eq!(enrollment.device_id, device_id);
    assert_eq!(enrollment.token.expose(), device_secret);
    assert_eq!(enrollment.protocol, PROTOCOL_VERSION);

    let recorded = transport.last();
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/enroll");
    assert!(!recorded.url.contains(secret));
    let body: Value =
        serde_json::from_slice(recorded.body.as_deref().expect("body")).expect("enrollment json");
    assert_eq!(body["token"], secret);
    assert_eq!(body["label"], "living-room");

    let directory = tempfile::tempdir().expect("tempdir");
    let mut storage =
        tv_client::storage::Storage::open(directory.path(), 1_048_576).expect("storage");
    cloud::store_device(&mut storage, &enrollment).expect("store device");
    let stored = cloud::load_device(&storage).expect("load").expect("device");
    assert_eq!(stored.device_id, device_id);
    assert_eq!(stored.token.expose(), device_secret);
    let expected_id = device_id.to_string();
    assert_eq!(
        storage
            .get_preference(cloud::DEVICE_ID_PREFERENCE)
            .expect("preference")
            .as_deref(),
        Some(expected_id.as_str())
    );
    assert_eq!(
        storage
            .read_credential(cloud::DEVICE_TOKEN_CREDENTIAL)
            .expect("credential")
            .as_slice(),
        device_secret.as_bytes()
    );
}

#[tokio::test]
async fn device_config_parses_features_heartbeat_and_bearer() {
    let transport = Arc::new(RecordingTransport::new());
    transport.respond(
        "/v1/device/config",
        json!({
            "protocol": PROTOCOL_VERSION,
            "features": {"youtube": false, "airplay": false, "webrtc": false},
            "heartbeat_ms": 15_000,
        }),
    );
    let http = CloudHttp::with_transport("https://staging.example.test", transport.clone())
        .expect("http")
        .with_token(device_token());
    let config = http.device_config().await.expect("config");
    assert_eq!(config.protocol, PROTOCOL_VERSION);
    assert!(!config.features.youtube);
    assert_eq!(config.heartbeat(), Duration::from_millis(15_000));
    let recorded = transport.last();
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.path, "/v1/device/config");
    assert_eq!(
        recorded.bearer.as_deref(),
        Some("Bearer test-device-token-1234567890")
    );
}

#[test]
fn tokens_never_appear_in_argv_or_debug_output() {
    let secret = "enroll-secret-value-1234567890";
    let token = EnrollmentToken::new(secret).expect("token");
    let rendered = format!("{token:?}");
    assert!(!rendered.contains(secret));
    assert!(rendered.contains("redacted"));

    let stored = cloud::StoredDevice {
        device_id: Uuid::new_v4(),
        token: device_token(),
    };
    assert!(!format!("{stored:?}").contains(TOKEN_TEXT));

    let http = CloudHttp::with_transport(
        "https://staging.example.test",
        Arc::new(RecordingTransport::new()),
    )
    .expect("http")
    .with_token(device_token());
    assert!(!format!("{http:?}").contains(TOKEN_TEXT));
    let config = link_config("ws://127.0.0.1:9/v1/ws/device".to_owned(), Uuid::new_v4());
    assert!(!format!("{config:?}").contains(TOKEN_TEXT));

    let binary = env!("CARGO_BIN_EXE_tv-client");
    let output = Command::new(binary)
        .args(["enroll", "--token", secret])
        .output()
        .expect("run binary");
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
}

#[test]
fn enrollment_reads_the_token_from_stdin_without_printing_it() {
    let secret = "enroll-secret-value-1234567890";
    let directory = tempfile::tempdir().expect("tempdir");
    let config_path = directory.path().join("client.toml");
    std::fs::write(
        &config_path,
        "server_url = \"https://127.0.0.1:9\"\ncloud_enabled = true\n",
    )
    .expect("config");
    let state_dir = directory.path().join("state");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tv-client"))
        .args([
            "enroll",
            "--config",
            config_path.to_str().expect("path"),
            "--state-dir",
            state_dir.to_str().expect("path"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin
            .write_all(format!("{secret}\n").as_bytes())
            .expect("write token");
    }
    let output = child.wait_with_output().expect("wait");
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(secret));
    assert!(!stderr.contains(secret));
}

#[test]
fn ui_snapshot_projects_playback_without_media_urls() {
    let mut model = UiModel::new();
    model.set_cloud_status("Cloud offline");
    assert_eq!(model.cloud_status(), "Cloud offline");
    model.apply_playback_update(&PlaybackUpdate::Loading {
        item_id: "opaque-item".to_owned(),
        title: Some("Episode one".to_owned()),
    });
    let state = model.snapshot();
    assert_eq!(state.mode, Mode::Loading);
    assert_eq!(state.source, Some(Source::Jellyfin));
    assert_eq!(state.item_id.as_deref(), Some("opaque-item"));
    let payload = StatePayload::from_app_state(&state, Uuid::new_v4(), true)
        .expect("snapshot is a valid state payload");
    assert_eq!(payload.title.as_deref(), Some("Episode one"));
    assert!(
        !serde_json::to_string(&payload)
            .expect("json")
            .contains("://")
    );

    model.apply_playback_update(&PlaybackUpdate::Stopped {
        item_id: "opaque-item".to_owned(),
    });
    assert_eq!(model.snapshot().mode, Mode::Home);
    assert_eq!(model.snapshot().item_id, None);
}

#[test]
fn dedupe_cache_is_bounded_to_256_entries() {
    let now = std::time::Instant::now();
    let mut cache = DedupeCache::default();
    for _ in 0..300 {
        let id = Uuid::new_v4();
        cache.record(id, now, AckPayload::new(id, AckStatus::Applied, None));
    }
    assert_eq!(cache.len(), DEDUPE_CAPACITY);
}

#[test]
fn unknown_fields_are_rejected() {
    let frame = json!({
        "v": 1,
        "type": "command",
        "id": Uuid::new_v4(),
        "payload": {
            "command_id": Uuid::new_v4(),
            "action": {"kind": "stop"},
            "extra": true,
        },
    });
    assert_eq!(
        parse_inbound(serde_json::to_vec(&frame).expect("frame").as_slice()),
        Err(ProtocolError::Malformed)
    );
}

#[test]
fn malformed_fixture_actions_are_typed_rejections() {
    let action = json!({"kind": "play_youtube", "video_id": "too-short"});
    assert_eq!(
        cloud::parse_action(&action),
        Err(ProtocolError::InvalidAction)
    );
    let unknown = json!({"kind": "pair_accept"});
    assert_eq!(
        cloud::parse_action(&unknown),
        Err(ProtocolError::InvalidAction)
    );
}

#[tokio::test]
async fn ws_url_is_not_used_for_bearer_tokens() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let observed = Arc::new(Mutex::new(None::<String>));
    let observed_slot = Arc::clone(&observed);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept_hdr_async(
            stream,
            move |request: &Request<()>,
                  response: Response<()>|
                  -> Result<Response<()>, ErrorResponse> {
                *observed_slot.lock().expect("header") = request
                    .uri()
                    .query()
                    .map(str::to_owned)
                    .or_else(|| Some(request.uri().path().to_owned()));
                Ok(response)
            },
        )
        .await
        .expect("handshake");
        expect_hello_and_state(&mut ws, device_id).await;
    });
    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let _state_tx = state_tx;
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let link = spawn_link(
        link_config(ws_url, device_id),
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    let request_target = observed.lock().expect("target").clone().expect("target");
    assert_eq!(request_target, "/v1/ws/device");
    assert!(!request_target.contains(TOKEN_TEXT));
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}

#[tokio::test]
async fn state_is_republished_during_playback() {
    let device_id = Uuid::new_v4();
    let (listener, ws_url) = listener().await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept(stream).await;
        expect_hello_and_state(&mut ws, device_id).await;
        send_json(
            &mut ws,
            "hello_ack",
            json!({"connection_id": Uuid::new_v4()}),
        )
        .await;
        let _ = ready_tx.send(());
        let mut times = 0;
        while times < 2 {
            let frame = recv_json(&mut ws).await;
            if frame["type"] == "state" {
                assert_eq!(frame["payload"]["seq"], times + 2);
                times += 1;
            }
        }
    });

    let (state_tx, state_rx) = watch::channel(home_state(device_id));
    let (events, _events_rx) = mpsc::channel(32);
    let (shutdown, signal) = shutdown_channel();
    let mut config = link_config(ws_url, device_id);
    config.playback_state_interval = Duration::from_millis(100);
    let link = spawn_link(
        config,
        state_rx,
        events,
        Arc::new(CountingApplier {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CommandPolicy::for_build(true),
        signal,
    );

    // Let the link publish the initial home state before switching to
    // playing; the server asserts home for the first state.
    ready_rx.await.expect("initial state published");
    let mut playing = home_state(device_id);
    playing.mode = Mode::Playing;
    let _ = state_tx.send(playing);

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), link).await;
}
