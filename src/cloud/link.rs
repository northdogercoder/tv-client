//! Authenticated device WebSocket, heartbeat, state publishing and commands.
//!
//! The bearer is sent in the `Authorization` header, never in the URL. Frame
//! text is parsed into typed values and is never formatted into a diagnostic.
//! The command queue is bounded, deduplication survives reconnects, and no
//! command is replayed after a connection loss.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Bytes, Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::Url;
use uuid::Uuid;

use crate::cloud::action::{
    ApplyOutcome, ApplyRequest, COMMAND_APPLY_TIMEOUT, COMMAND_QUEUE_CAPACITY, CommandApplier,
    CommandPolicy, CommandQueue, DEDUPE_CAPACITY, DEDUPE_WINDOW, DedupeCache, Rejection,
    SharedApplier,
};
use crate::cloud::error::CloudError;
use crate::cloud::http::device_websocket_url;
use crate::cloud::protocol::{
    AckPayload, AckStatus, HelloAck, HelloPayload, InboundMessage, MAX_FRAME_BYTES,
    PairRequestPayload, PairStatusPayload, ProtocolError, StatePayload, ack_frame, hello_frame,
    parse_inbound, state_frame,
};
use crate::cloud::token::DeviceToken;
use crate::model::Mode;
use crate::state::ShutdownSignal;

/// Heartbeat ping interval, matching protocol v1.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// Inbound silence after which the connection is considered dead.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);
/// Interval for repeated state updates while media is playing.
pub const PLAYBACK_STATE_INTERVAL: Duration = Duration::from_secs(5);
/// Default reconnect jitter lower bound.
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
/// Default reconnect jitter upper bound.
pub const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// Bound for the event notification channel.
pub const LINK_EVENT_CAPACITY: usize = 32;
/// Bound for outbound socket frames.
pub const OUTBOUND_CAPACITY: usize = 32;
/// Bound for establishing a WebSocket and its TLS handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

type DeviceSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type DeviceSink = SplitSink<DeviceSocket, Message>;

/// Why the socket closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkClose {
    /// The peer closed the socket.
    Closed,
    /// The network or TLS layer failed.
    Network,
    /// No inbound frame arrived within the heartbeat timeout.
    Timeout,
    /// A protocol violation caused a close.
    Protocol,
}

/// Notifications for the owner of the pairing UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// The WebSocket handshake succeeded.
    Connected,
    /// The server acknowledged the device hello.
    HelloAck(HelloAck),
    /// A pairing request needs local confirmation.
    PairRequest(PairRequestPayload),
    /// A pairing status changed.
    PairStatus(PairStatusPayload),
    /// The connection ended; reconnection follows unless revoked.
    Disconnected(LinkClose),
    /// The server revoked this device; no reconnection is attempted.
    Revoked,
}

/// How the link stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEnd {
    /// Local shutdown won the race.
    Shutdown,
    /// The server revoked the device.
    Revoked,
}

/// Reconnect delay policy with uniform jitter in the configured range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    minimum: Duration,
    maximum: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            minimum: RECONNECT_MIN,
            maximum: RECONNECT_MAX,
        }
    }
}

impl ReconnectPolicy {
    /// Build a bounded jitter policy.
    pub fn new(minimum: Duration, maximum: Duration) -> Result<Self, CloudError> {
        if minimum.is_zero() || minimum > maximum {
            return Err(CloudError::Endpoint);
        }
        Ok(Self { minimum, maximum })
    }

    /// Lower bound.
    #[must_use]
    pub const fn minimum(self) -> Duration {
        self.minimum
    }

    /// Upper bound.
    #[must_use]
    pub const fn maximum(self) -> Duration {
        self.maximum
    }

    /// Draw the next delay. Production uses OS randomness from `uuid`.
    #[must_use]
    pub fn next_delay(self) -> Duration {
        self.delay_with_entropy(entropy())
    }

    /// Deterministic jitter used by tests and the production draw.
    #[must_use]
    pub fn delay_with_entropy(self, entropy: u64) -> Duration {
        let minimum = u64::try_from(self.minimum.as_millis()).unwrap_or(u64::MAX);
        let maximum = u64::try_from(self.maximum.as_millis()).unwrap_or(u64::MAX);
        let span = maximum.saturating_sub(minimum);
        if span == 0 {
            return self.minimum;
        }
        Duration::from_millis(minimum + entropy % (span.saturating_add(1)))
    }
}

fn entropy() -> u64 {
    let bytes = Uuid::new_v4().into_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

/// Link settings. The token is deliberately held by [`DeviceLink`], not here,
/// so this value can be logged or cloned without a secret.
#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// Authenticated WebSocket URL (`wss://` in production).
    pub ws_url: String,
    /// Device id sent in hello.
    pub device_id: Uuid,
    /// Client version sent in hello.
    pub client_version: String,
    /// Advertised capabilities.
    pub capabilities: Vec<String>,
    /// Ping interval.
    pub heartbeat_interval: Duration,
    /// Inbound silence timeout.
    pub heartbeat_timeout: Duration,
    /// Repeated state interval during playback.
    pub playback_state_interval: Duration,
    /// Reconnect jitter policy.
    pub reconnect: ReconnectPolicy,
    /// Pending command bound.
    pub queue_capacity: usize,
    /// Whether titles may be included in published state.
    pub share_titles: bool,
}

impl LinkConfig {
    /// Build defaults for a configured HTTPS control-server origin.
    pub fn new(
        server_url: &str,
        device_id: Uuid,
        client_version: impl Into<String>,
        capabilities: Vec<String>,
    ) -> Result<Self, CloudError> {
        Ok(Self {
            ws_url: device_websocket_url(server_url)?,
            device_id,
            client_version: client_version.into(),
            capabilities,
            heartbeat_interval: HEARTBEAT_INTERVAL,
            heartbeat_timeout: HEARTBEAT_TIMEOUT,
            playback_state_interval: PLAYBACK_STATE_INTERVAL,
            reconnect: ReconnectPolicy::default(),
            queue_capacity: COMMAND_QUEUE_CAPACITY,
            share_titles: false,
        })
    }

    /// Override heartbeat timings (tests and server preference).
    #[must_use]
    pub fn with_heartbeat(mut self, interval: Duration, timeout: Duration) -> Self {
        self.heartbeat_interval = interval;
        self.heartbeat_timeout = timeout;
        self
    }

    /// Override playback state cadence.
    #[must_use]
    pub fn with_playback_interval(mut self, interval: Duration) -> Self {
        self.playback_state_interval = interval;
        self
    }

    /// Override the reconnect policy.
    #[must_use]
    pub fn with_reconnect(mut self, reconnect: ReconnectPolicy) -> Self {
        self.reconnect = reconnect;
        self
    }

    /// Override the pending command bound.
    #[must_use]
    pub fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// Include or omit titles in state updates.
    #[must_use]
    pub fn with_share_titles(mut self, share: bool) -> Self {
        self.share_titles = share;
        self
    }

    fn validate(&self) -> Result<(), CloudError> {
        let parsed = Url::parse(&self.ws_url).map_err(|_| CloudError::Endpoint)?;
        if !matches!(parsed.scheme(), "ws" | "wss")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(CloudError::Endpoint);
        }
        if self.heartbeat_interval.is_zero()
            || self.heartbeat_timeout <= self.heartbeat_interval
            || self.playback_state_interval.is_zero()
            || self.queue_capacity == 0
            || self.queue_capacity > COMMAND_QUEUE_CAPACITY
        {
            return Err(CloudError::Endpoint);
        }
        if self.client_version.is_empty()
            || self.client_version.len() > 32
            || self.client_version.chars().any(char::is_control)
        {
            return Err(CloudError::Endpoint);
        }
        let hello = HelloPayload {
            device_id: self.device_id,
            client_version: self.client_version.clone(),
            capabilities: self.capabilities.clone(),
        };
        hello_frame(&hello)?;
        Ok(())
    }
}

/// One device socket with reconnect, heartbeat and command handling.
pub struct DeviceLink {
    config: LinkConfig,
    token: DeviceToken,
    state: watch::Receiver<StatePayload>,
    events: mpsc::Sender<LinkEvent>,
    applier: SharedApplier,
    policy: CommandPolicy,
    dedupe: Arc<Mutex<DedupeCache>>,
}

impl DeviceLink {
    /// Build a link. The state receiver is updated by the local application.
    pub fn new(
        config: LinkConfig,
        token: DeviceToken,
        state: watch::Receiver<StatePayload>,
        events: mpsc::Sender<LinkEvent>,
        applier: Arc<dyn CommandApplier>,
        policy: CommandPolicy,
    ) -> Result<Self, CloudError> {
        config.validate()?;
        Ok(Self {
            config,
            token,
            state,
            events,
            applier,
            policy,
            dedupe: Arc::new(Mutex::new(DedupeCache::new(DEDUPE_CAPACITY, DEDUPE_WINDOW))),
        })
    }

    /// Run until shutdown or device revocation.
    pub async fn run(mut self, mut shutdown: ShutdownSignal) -> LinkEnd {
        loop {
            if shutdown.is_cancelled() {
                return LinkEnd::Shutdown;
            }
            match self.connect_once(&mut shutdown).await {
                ConnectOutcome::Shutdown => return LinkEnd::Shutdown,
                ConnectOutcome::Revoked => {
                    self.emit(LinkEvent::Revoked);
                    return LinkEnd::Revoked;
                }
                ConnectOutcome::Ended(reason) => {
                    self.emit(LinkEvent::Disconnected(reason));
                    let delay = self.config.reconnect.next_delay();
                    tracing::debug!(
                        reason = ?reason,
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        "device socket disconnected; reconnecting"
                    );
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => return LinkEnd::Shutdown,
                        () = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    async fn connect_once(&mut self, shutdown: &mut ShutdownSignal) -> ConnectOutcome {
        let mut request = match self.config.ws_url.as_str().into_client_request() {
            Ok(request) => request,
            Err(_) => return ConnectOutcome::Ended(LinkClose::Protocol),
        };
        let authorization = format!("Bearer {}", self.token.expose());
        let value = match HeaderValue::from_bytes(authorization.as_bytes()) {
            Ok(value) => value,
            Err(_) => return ConnectOutcome::Ended(LinkClose::Protocol),
        };
        request.headers_mut().insert(AUTHORIZATION, value);

        let frame_limit = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        let connected = tokio::time::timeout(
            CONNECT_TIMEOUT,
            connect_async_with_config(request, Some(frame_limit), true),
        )
        .await;
        let socket = match connected {
            Err(_) => return ConnectOutcome::Ended(LinkClose::Timeout),
            Ok(Err(error)) => return classify_handshake(error),
            Ok(Ok((socket, _response))) => socket,
        };
        self.emit(LinkEvent::Connected);

        let (sink, mut stream) = socket.split();
        let (outbound, outbound_rx) = mpsc::channel::<Outbound>(OUTBOUND_CAPACITY);
        let writer = tokio::spawn(writer_task(sink, outbound_rx));

        let hello = HelloPayload {
            device_id: self.config.device_id,
            client_version: self.config.client_version.clone(),
            capabilities: self.config.capabilities.clone(),
        };
        let hello = match hello_frame(&hello) {
            Ok(frame) => frame,
            Err(_) => {
                cleanup(writer, outbound).await;
                return ConnectOutcome::Ended(LinkClose::Protocol);
            }
        };
        if outbound.try_send(Outbound::Text(hello)).is_err() {
            cleanup(writer, outbound).await;
            return ConnectOutcome::Ended(LinkClose::Network);
        }

        let state_rx = self.state.clone();
        let mut change_rx = state_rx.clone();
        let mut state = state_rx.borrow().clone();
        let mut sequence = 0_u64;
        if !send_state(&outbound, &state, &mut sequence, self.config.share_titles) {
            cleanup(writer, outbound).await;
            return ConnectOutcome::Ended(LinkClose::Network);
        }

        let (queue, command_rx) = match CommandQueue::new(self.config.queue_capacity) {
            Ok(pair) => pair,
            Err(_) => {
                cleanup(writer, outbound).await;
                return ConnectOutcome::Ended(LinkClose::Protocol);
            }
        };
        let worker = tokio::spawn(command_worker(
            command_rx,
            Arc::clone(&self.applier),
            Arc::clone(&self.dedupe),
            outbound.clone(),
        ));

        let events = self.events.clone();
        let policy = self.policy;
        let dedupe = Arc::clone(&self.dedupe);
        let mut heartbeat = tokio::time::interval(self.config.heartbeat_interval);
        let mut playback = tokio::time::interval(self.config.playback_state_interval);
        // Tokio intervals tick immediately once; the first heartbeat/periodic
        // state must wait for one full period.
        heartbeat.tick().await;
        playback.tick().await;
        let mut last_inbound = Instant::now();
        let outcome = loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    let _ = outbound.try_send(Outbound::Close(1000, "client shutdown"));
                    break ConnectOutcome::Shutdown;
                }
                _ = heartbeat.tick() => {
                    if last_inbound.elapsed() >= self.config.heartbeat_timeout {
                        break ConnectOutcome::Ended(LinkClose::Timeout);
                    }
                    if outbound.try_send(Outbound::Ping).is_err() {
                        break ConnectOutcome::Ended(LinkClose::Network);
                    }
                }
                _ = playback.tick() => {
                    if matches!(state.mode, Mode::Playing | Mode::Paused)
                        && !send_state(&outbound, &state, &mut sequence, self.config.share_titles)
                    {
                        break ConnectOutcome::Ended(LinkClose::Network);
                    }
                }
                changed = change_rx.changed() => {
                    if changed.is_err() {
                        break ConnectOutcome::Ended(LinkClose::Closed);
                    }
                    state = state_rx.borrow().clone();
                    if !send_state(&outbound, &state, &mut sequence, self.config.share_titles) {
                        break ConnectOutcome::Ended(LinkClose::Network);
                    }
                }
                message = stream.next() => {
                    let Some(message) = message else {
                        break ConnectOutcome::Ended(LinkClose::Closed);
                    };
                    match message {
                        Ok(Message::Text(text)) => {
                            last_inbound = Instant::now();
                            if text.len() > MAX_FRAME_BYTES {
                                let _ = outbound.try_send(Outbound::Close(1009, "frame too large"));
                                break ConnectOutcome::Ended(LinkClose::Protocol);
                            }
                            match parse_inbound(text.as_str().as_bytes()) {
                                Ok(InboundMessage::HelloAck(hello)) => {
                                    let _ = events.try_send(LinkEvent::HelloAck(hello));
                                }
                                // The server echoes the device's own state after
                                // hello_ack; the device remains authoritative.
                                Ok(InboundMessage::State(_state)) => {}
                                Ok(InboundMessage::Command(command)) => {
                                    handle_command(
                                        command,
                                        &state,
                                        &policy,
                                        &dedupe,
                                        &queue,
                                        &outbound,
                                    );
                                }
                                Ok(InboundMessage::PairRequest(request)) => {
                                    let _ = events.try_send(LinkEvent::PairRequest(request));
                                }
                                Ok(InboundMessage::PairStatus(status)) => {
                                    let _ = events.try_send(LinkEvent::PairStatus(status));
                                }
                                Err(ProtocolError::UnsupportedVersion) => {
                                    let _ = outbound.try_send(Outbound::Close(1002, "unsupported version"));
                                    break ConnectOutcome::Ended(LinkClose::Protocol);
                                }
                                Err(ProtocolError::FrameTooLarge) => {
                                    let _ = outbound.try_send(Outbound::Close(1009, "frame too large"));
                                    break ConnectOutcome::Ended(LinkClose::Protocol);
                                }
                                Err(_) => {
                                    let _ = outbound.try_send(Outbound::Close(1008, "protocol policy"));
                                    break ConnectOutcome::Ended(LinkClose::Protocol);
                                }
                            }
                        }
                        Ok(Message::Binary(_)) => {
                            let _ = outbound.try_send(Outbound::Close(1008, "text frames only"));
                            break ConnectOutcome::Ended(LinkClose::Protocol);
                        }
                        Ok(Message::Ping(payload)) => {
                            last_inbound = Instant::now();
                            if outbound.try_send(Outbound::Pong(payload)).is_err() {
                                break ConnectOutcome::Ended(LinkClose::Network);
                            }
                        }
                        Ok(Message::Pong(_)) => {
                            last_inbound = Instant::now();
                        }
                        Ok(Message::Close(_)) => {
                            break ConnectOutcome::Ended(LinkClose::Closed);
                        }
                        Ok(Message::Frame(_)) => {}
                        Err(error) => {
                            if matches!(error, WsError::Capacity(_)) {
                                let _ = outbound.try_send(Outbound::Close(1009, "frame too large"));
                            }
                            break ConnectOutcome::Ended(classify_read(error));
                        }
                    }
                }
            }
        };

        worker.abort();
        let _ = worker.await;
        drop(queue);
        cleanup(writer, outbound).await;
        outcome
    }

    fn emit(&self, event: LinkEvent) {
        let _ = self.events.try_send(event);
    }
}

fn send_state(
    outbound: &mpsc::Sender<Outbound>,
    state: &StatePayload,
    sequence: &mut u64,
    share_titles: bool,
) -> bool {
    *sequence = sequence.saturating_add(1);
    let mut payload = state.clone();
    payload.seq = *sequence;
    if !share_titles {
        payload.title = None;
    }
    match state_frame(&payload) {
        Ok(frame) => outbound.try_send(Outbound::Text(frame)).is_ok(),
        Err(_) => false,
    }
}

fn handle_command(
    command: crate::cloud::protocol::CommandPayload,
    state: &StatePayload,
    policy: &CommandPolicy,
    dedupe: &Arc<Mutex<DedupeCache>>,
    queue: &CommandQueue,
    outbound: &mpsc::Sender<Outbound>,
) {
    let now = Instant::now();
    let cached = dedupe
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .lookup(command.command_id, now);
    if let Some(ack) = cached {
        // While the worker is still applying, a duplicate has no final status
        // yet. Do not answer early with a rejection the server would record as
        // final; the eventual applied/failed ack will cover both deliveries.
        if ack.status == AckStatus::Rejected && ack.reason.as_deref() == Some("in_progress") {
            return;
        }
        send_ack(outbound, &ack);
        return;
    }

    if let Err(rejection) = policy.evaluate(&command.action, state.mode) {
        record_and_ack(
            dedupe,
            outbound,
            command.command_id,
            AckStatus::Rejected,
            Some(rejection.code),
        );
        return;
    }

    let request = ApplyRequest {
        command_id: command.command_id,
        action: command.action,
        mode: state.mode,
        received_at: now,
        deadline: now + COMMAND_APPLY_TIMEOUT,
    };
    match queue.try_push(request) {
        Ok(()) => {
            // A duplicate arriving while the worker is still applying must
            // not be applied a second time. Recording the in-progress marker
            // does not send an acknowledgement: only the worker may do that.
            record_only(
                dedupe,
                command.command_id,
                AckStatus::Rejected,
                Some("in_progress"),
            );
        }
        Err(crate::cloud::action::QueueReject::Full) => {
            record_and_ack(
                dedupe,
                outbound,
                command.command_id,
                AckStatus::Rejected,
                Some("queue_full"),
            );
        }
        Err(crate::cloud::action::QueueReject::Closed) => {
            record_and_ack(
                dedupe,
                outbound,
                command.command_id,
                AckStatus::Failed,
                Some("queue_closed"),
            );
        }
    }
}

fn record_only(
    dedupe: &Arc<Mutex<DedupeCache>>,
    command_id: Uuid,
    status: AckStatus,
    reason: Option<&str>,
) {
    let ack = AckPayload::new(command_id, status, reason);
    dedupe
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(command_id, Instant::now(), ack);
}

fn record_and_ack(
    dedupe: &Arc<Mutex<DedupeCache>>,
    outbound: &mpsc::Sender<Outbound>,
    command_id: Uuid,
    status: AckStatus,
    reason: Option<&str>,
) {
    let ack = AckPayload::new(command_id, status, reason);
    dedupe
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(command_id, Instant::now(), ack.clone());
    send_ack(outbound, &ack);
}

fn send_ack(outbound: &mpsc::Sender<Outbound>, ack: &AckPayload) {
    if let Ok(frame) = ack_frame(ack) {
        let _ = outbound.try_send(Outbound::Text(frame));
    }
}

async fn command_worker(
    mut commands: mpsc::Receiver<ApplyRequest>,
    applier: SharedApplier,
    dedupe: Arc<Mutex<DedupeCache>>,
    outbound: mpsc::Sender<Outbound>,
) {
    while let Some(request) = commands.recv().await {
        let command_id = request.command_id;
        if request.is_expired(Instant::now()) {
            let ack = AckPayload::new(command_id, AckStatus::Rejected, Some("expired"));
            dedupe
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(command_id, Instant::now(), ack.clone());
            if let Ok(frame) = ack_frame(&ack) {
                let _ = outbound.try_send(Outbound::Text(frame));
            }
            continue;
        }
        let outcome =
            match tokio::time::timeout(COMMAND_APPLY_TIMEOUT, applier.apply(request)).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    ApplyOutcome::Failed(Rejection::new("timeout", "command application timed out"))
                }
            };
        let (status, reason) = match outcome {
            ApplyOutcome::Applied => (AckStatus::Applied, None),
            ApplyOutcome::Rejected(rejection) => (AckStatus::Rejected, Some(rejection.code)),
            ApplyOutcome::Failed(rejection) => (AckStatus::Failed, Some(rejection.code)),
        };
        let ack = AckPayload::new(command_id, status, reason);
        dedupe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(command_id, Instant::now(), ack.clone());
        if let Ok(frame) = ack_frame(&ack) {
            if outbound.try_send(Outbound::Text(frame)).is_err() {
                return;
            }
        } else {
            return;
        }
    }
}

#[derive(Debug)]
enum Outbound {
    Text(Vec<u8>),
    Ping,
    Pong(Bytes),
    Close(u16, &'static str),
}

async fn writer_task(mut sink: DeviceSink, mut receiver: mpsc::Receiver<Outbound>) {
    while let Some(outbound) = receiver.recv().await {
        match outbound {
            Outbound::Text(bytes) => {
                let Ok(text) = String::from_utf8(bytes) else {
                    return;
                };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    return;
                }
            }
            Outbound::Ping => {
                if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
            }
            Outbound::Pong(payload) => {
                if sink.send(Message::Pong(payload)).await.is_err() {
                    return;
                }
            }
            Outbound::Close(code, reason) => {
                let code = match code {
                    1002 => CloseCode::Protocol,
                    1008 => CloseCode::Policy,
                    1009 => CloseCode::Size,
                    _ => CloseCode::Normal,
                };
                let frame = CloseFrame {
                    code,
                    reason: reason.into(),
                };
                let _ = sink.send(Message::Close(Some(frame))).await;
                return;
            }
        }
    }
    let _ = sink.close().await;
}

async fn cleanup(mut writer: tokio::task::JoinHandle<()>, outbound: mpsc::Sender<Outbound>) {
    drop(outbound);
    if tokio::time::timeout(Duration::from_millis(500), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }
}

enum ConnectOutcome {
    Shutdown,
    Revoked,
    Ended(LinkClose),
}

fn classify_handshake(error: WsError) -> ConnectOutcome {
    if let WsError::Http(response) = error
        && matches!(response.status().as_u16(), 401 | 403)
    {
        return ConnectOutcome::Revoked;
    }
    ConnectOutcome::Ended(LinkClose::Network)
}

fn classify_read(error: WsError) -> LinkClose {
    match error {
        WsError::ConnectionClosed | WsError::AlreadyClosed => LinkClose::Closed,
        WsError::Capacity(_) => LinkClose::Protocol,
        _ => LinkClose::Network,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_queued_command_is_rejected_without_applying() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::model::Action;

        struct TestApplier(Arc<AtomicBool>);

        impl CommandApplier for TestApplier {
            fn apply(
                &self,
                _request: ApplyRequest,
            ) -> futures_util::future::BoxFuture<'_, ApplyOutcome> {
                let applied = Arc::clone(&self.0);
                Box::pin(async move {
                    applied.store(true, Ordering::SeqCst);
                    ApplyOutcome::Applied
                })
            }
        }

        let (commands, receiver) = mpsc::channel(1);
        let (outbound, mut received) = mpsc::channel(1);
        let applied = Arc::new(AtomicBool::new(false));
        let command_id = Uuid::new_v4();
        let now = Instant::now();
        commands
            .send(ApplyRequest {
                command_id,
                action: Action::Stop,
                mode: Mode::Home,
                received_at: now - Duration::from_secs(11),
                deadline: now - Duration::from_secs(1),
            })
            .await
            .expect("queue");
        drop(commands);
        let worker = tokio::spawn(command_worker(
            receiver,
            Arc::new(TestApplier(Arc::clone(&applied))),
            Arc::new(Mutex::new(DedupeCache::default())),
            outbound,
        ));
        match received.recv().await.expect("ack frame") {
            Outbound::Text(bytes) => {
                let frame: serde_json::Value = serde_json::from_slice(&bytes).expect("ack json");
                assert_eq!(frame["type"], "ack");
                assert_eq!(frame["payload"]["command_id"], command_id.to_string());
                assert_eq!(frame["payload"]["status"], "rejected");
                assert_eq!(frame["payload"]["reason"], "expired");
            }
            other => panic!("unexpected outbound frame: {other:?}"),
        }
        assert!(!applied.load(Ordering::SeqCst));
        worker.await.expect("worker");
    }

    #[test]
    fn jitter_stays_inside_one_to_sixty_seconds() {
        let policy = ReconnectPolicy::default();
        for entropy in [0, 1, 7, 99, 1_000, u64::MAX] {
            let delay = policy.delay_with_entropy(entropy);
            assert!(delay >= RECONNECT_MIN);
            assert!(delay <= RECONNECT_MAX);
        }
        assert!(ReconnectPolicy::new(Duration::ZERO, RECONNECT_MAX).is_err());
    }
}
