//! Offline mock-server tests for Jellyfin playback negotiation and reporting.
//!
//! The single live test at the bottom is `#[ignore]` and needs explicit
//! `JELLYFIN_URL`, `JELLYFIN_USER` and `JELLYFIN_PASS` environment variables.
//! Credentials are never embedded in fixtures or printed.

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tv_client::config::ClientConfig;
use tv_client::jellyfin::{
    ArtworkClass, CancelHandle, CancelToken, ItemQuery, JellyfinClient, JellyfinError, Page,
    PlayMethod, PlaybackInfoOptions, PlaybackInfoResponse, SelectionError, SelectionOptions,
    StreamUrl, TICKS_PER_MILLISECOND, cancellation, select_media_source,
};
use tv_client::model::Source;
use tv_client::playback::{
    BrowseUpdate, LibraryFilter, MAX_PROGRESS_AGE, PlayRequest, PlaybackCommand, PlaybackConfig,
    PlaybackController, PlaybackUpdate, ProgressEntry, ProgressJournal, RESUME_MIN_TICKS,
    millis_from_ticks, ticks_from_millis,
};
use tv_client::player::{
    ChildCommand, ChildExit, ChildFactory, ChildMessage, PlayerError, PlayerNotification,
    PlayerSession, PlayerState, SupervisedChild, SupervisorConfig,
};
use tv_client::storage::Storage;
use tv_client::ui::{Key, MediaItem, PlayerMenu, Screen, UiModel, UiOutcome, UiPlaybackCommand};

const DEVICE_NAME: &str = "playback-test-tv";
const ACCESS_TOKEN: &str = "playback-token";

// ---------------------------------------------------------------------------
// Minimal HTTP mock server (same style as tests/jellyfin_library.rs).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockRequest {
    method: String,
    target: String,
    #[allow(dead_code)]
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl MockRequest {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }

    fn query(&self) -> HashMap<String, String> {
        self.target
            .split_once('?')
            .map(|(_, query)| {
                query
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

struct MockResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl MockResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: serde_json::to_vec(&body).expect("JSON body"),
        }
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body,
        }
    }
}

struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    fn start<F>(handler: F) -> Self
    where
        F: Fn(&MockRequest) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        if let Some(request) = read_request(&mut stream) {
                            thread_requests.lock().unwrap().push(request.clone());
                            let response = handler(&request);
                            let _ = write_response(&mut stream, &response);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{address}"),
            requests,
            stop,
        }
    }

    fn requests(&self) -> Vec<MockRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn read_request(stream: &mut TcpStream) -> Option<MockRequest> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 2048];
    let header_end = loop {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&buffer[..read]);
        if let Some(position) = find(&raw, b"\r\n\r\n") {
            break position + 4;
        }
        if raw.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end - 4]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = raw[header_end..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    body.truncate(length);
    Some(MockRequest {
        method,
        target,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, response: &MockResponse) -> std::io::Result<()> {
    let mut output = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason(response.status),
        response.body.len()
    );
    for (name, value) in &response.headers {
        output.push_str(name);
        output.push_str(": ");
        output.push_str(value);
        output.push_str("\r\n");
    }
    output.push_str("\r\n");
    let mut bytes = output.into_bytes();
    bytes.extend_from_slice(&response.body);
    stream.write_all(&bytes)?;
    stream.flush()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

// ---------------------------------------------------------------------------
// Fixtures and a scripted native child.
// ---------------------------------------------------------------------------

fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("temporary state directory")
}

fn auth_body() -> Value {
    json!({
        "User": {"Id": "user-1", "Name": "tester"},
        "AccessToken": ACCESS_TOKEN,
        "ServerId": "server-1"
    })
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
    bytes
}

fn direct_item(user_data: bool) -> Value {
    let mut item = json!({
        "Id": "item-1",
        "Name": "Pilot",
        "Type": "Episode",
        "RunTimeTicks": 600_000_000_u64,
        "SeriesId": "series-1",
        "IndexNumber": 1,
        "ParentIndexNumber": 1
    });
    if user_data {
        item["UserData"] = json!({
            "Played": false,
            "PlayCount": 0,
            "PlaybackPositionTicks": 500_000_000_u64,
            "PlayedPercentage": 83.0
        });
    }
    item
}

fn direct_playback_info() -> Value {
    json!({
        "PlaySessionId": "play-1",
        "MediaSources": [{
            "Id": "source-1",
            "Name": "1080p H264",
            "Container": "mp4",
            "RunTimeTicks": 600_000_000_u64,
            "SupportsDirectPlay": true,
            "SupportsDirectStream": true,
            "SupportsTranscoding": true,
            "DefaultAudioStreamIndex": 1,
            "TranscodingUrl": "/Videos/item-1/stream?static=false&mediaSourceId=source-1&playSessionId=play-1&api_key=never-print",
            "TranscodingContainer": "ts",
            "TranscodingSubProtocol": "hls",
            "MediaStreams": [
                {"Type":"Video","Index":0,"Codec":"h264","Width":1920,"Height":1080,"BitDepth":8,"VideoRangeType":"SDR","Level":41,"AverageFrameRate":30.0},
                {"Type":"Audio","Index":1,"Codec":"aac","Channels":2,"SampleRate":48000},
                {"Type":"Subtitle","Index":2,"Codec":"srt","IsTextSubtitleStream":true,"DisplayTitle":"English"}
            ]
        }]
    })
}

fn transcode_playback_info() -> Value {
    json!({
        "PlaySessionId": "play-hevc",
        "MediaSources": [{
            "Id": "source-hevc",
            "Container": "mkv",
            "RunTimeTicks": 600_000_000_u64,
            "SupportsDirectPlay": true,
            "SupportsDirectStream": false,
            "SupportsTranscoding": true,
            "TranscodingUrl": "/Videos/item-1/stream?static=false&mediaSourceId=source-hevc&playSessionId=play-hevc&api_key=never-print",
            "TranscodingContainer": "ts",
            "TranscodingSubProtocol": "hls",
            "MediaStreams": [
                {"Type":"Video","Index":0,"Codec":"hevc","Width":3840,"Height":2160,"BitDepth":10,"VideoRangeType":"HDR10","AverageFrameRate":24.0},
                {"Type":"Audio","Index":1,"Codec":"eac3","Channels":6}
            ]
        }]
    })
}

fn stream_response() -> MockResponse {
    MockResponse::empty(204)
}

/// Scripted child: acknowledges loads with ready and position events and
/// records every command line.
#[derive(Clone)]
struct FakeFactory {
    commands: Arc<Mutex<Vec<Value>>>,
    spawns: Arc<AtomicUsize>,
    duration_ms: u64,
}

impl FakeFactory {
    fn new(duration_ms: u64) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
            spawns: Arc::new(AtomicUsize::new(0)),
            duration_ms,
        }
    }
}

impl ChildFactory for FakeFactory {
    fn spawn(&mut self) -> impl Future<Output = Result<SupervisedChild, PlayerError>> + Send {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let commands = Arc::clone(&self.commands);
        let duration_ms = self.duration_ms;
        async move {
            let (control, mut control_rx) = mpsc::channel(8);
            let (messages, message_rx) = mpsc::channel(32);
            tokio::spawn(async move {
                while let Some(command) = control_rx.recv().await {
                    match command {
                        ChildCommand::Line(line) => {
                            let value: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                            commands.lock().unwrap().push(value.clone());
                            match value["type"].as_str() {
                                Some("load") => {
                                    let start = value["start_position_ms"].as_u64().unwrap_or(0);
                                    send_event(
                                        &messages,
                                        json!({"type":"ready","decoder":"fake","sink":"fake"}),
                                    )
                                    .await;
                                    send_event(
                                        &messages,
                                        json!({"type":"position","position_ms":start + 1000,"duration_ms":duration_ms}),
                                    )
                                    .await;
                                }
                                Some("seek") => {
                                    let position = value["position_ms"].as_u64().unwrap_or(0);
                                    send_event(
                                        &messages,
                                        json!({"type":"position","position_ms":position,"duration_ms":duration_ms}),
                                    )
                                    .await;
                                }
                                Some("stop") => {
                                    let _ = messages
                                        .send(ChildMessage::Exited(ChildExit {
                                            success: true,
                                            code: Some(0),
                                        }))
                                        .await;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        ChildCommand::Kill => break,
                    }
                }
            });
            Ok(SupervisedChild::from_channels(control, message_rx))
        }
    }
}

async fn send_event(messages: &mpsc::Sender<ChildMessage>, value: Value) {
    let _ = messages.send(ChildMessage::Line(value.to_string())).await;
}

struct Fixture {
    _directory: TempDir,
    server: MockServer,
    controller: PlaybackController,
    cancel_handle: CancelHandle,
    cancel: CancelToken,
    fake: FakeFactory,
}

async fn fixture<F>(handler: F, config: PlaybackConfig, duration_ms: u64) -> Fixture
where
    F: Fn(&MockRequest) -> MockResponse + Send + Sync + 'static,
{
    let server = MockServer::start(handler);
    let directory = temp_dir();
    let mut storage = Storage::open(
        directory.path(),
        tv_client::storage::HARD_ARTWORK_CACHE_BYTES,
    )
    .expect("storage opens");
    let client_config = ClientConfig {
        jellyfin_url: Some(server.url.clone()),
        ..ClientConfig::default()
    };
    let mut client =
        JellyfinClient::new(&client_config, DEVICE_NAME, &mut storage).expect("client builds");
    let (_login_handle, cancel) = cancellation();
    client
        .authenticate_by_name(&mut storage, "tester", "secret", &cancel)
        .await
        .expect("login");
    let fake = FakeFactory::new(duration_ms);
    let session = PlayerSession::start(fake.clone(), SupervisorConfig::default(), 8, 64)
        .expect("player session");
    let controller =
        PlaybackController::new(client, session, config).expect("controller").with_storage(storage);
    Fixture {
        _directory: directory,
        server,
        controller,
        cancel_handle: _login_handle,
        cancel,
        fake,
    }
}

struct ClientFixture {
    _directory: TempDir,
    server: MockServer,
    client: JellyfinClient,
    _cancel_handle: CancelHandle,
    cancel: CancelToken,
}

async fn client_fixture<F>(handler: F) -> ClientFixture
where
    F: Fn(&MockRequest) -> MockResponse + Send + Sync + 'static,
{
    let server = MockServer::start(handler);
    let directory = temp_dir();
    let mut storage = Storage::open(
        directory.path(),
        tv_client::storage::HARD_ARTWORK_CACHE_BYTES,
    )
    .expect("storage opens");
    let client_config = ClientConfig {
        jellyfin_url: Some(server.url.clone()),
        ..ClientConfig::default()
    };
    let mut client =
        JellyfinClient::new(&client_config, DEVICE_NAME, &mut storage).expect("client builds");
    let (cancel_handle, cancel) = cancellation();
    client
        .authenticate_by_name(&mut storage, "tester", "secret", &cancel)
        .await
        .expect("login");
    ClientFixture {
        _directory: directory,
        server,
        client,
        _cancel_handle: cancel_handle,
        cancel,
    }
}

fn standard_handler(request: &MockRequest) -> MockResponse {
    match (request.method.as_str(), request.path()) {
        (_, "/Users/AuthenticateByName") => MockResponse::json(200, auth_body()),
        (_, "/Items/item-1") => MockResponse::json(200, direct_item(true)),
        (_, "/Users/user-1/Items/item-1/UserData") => MockResponse::json(
            200,
            json!({"Played": false, "PlayCount": 0, "PlaybackPositionTicks": 0}),
        ),
        (_, "/Items/item-1/PlaybackInfo") => MockResponse::json(200, direct_playback_info()),
        (_, "/Sessions/Playing")
        | (_, "/Sessions/Playing/Progress")
        | (_, "/Sessions/Playing/Stopped")
        | (_, "/Videos/ActiveEncodings") => stream_response(),
        _ => MockResponse::empty(404),
    }
}

// ---------------------------------------------------------------------------
// Negotiation and profile tests.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn playback_info_posts_the_proven_device_profile() {
    let fixture = client_fixture(standard_handler).await;
    let response = fixture
        .client
        .playback_info_with("item-1", &PlaybackInfoOptions::default(), &fixture.cancel)
        .await
        .expect("playback info");
    assert_eq!(response.play_session_id.as_deref(), Some("play-1"));
    let selection =
        select_media_source(&response, &SelectionOptions::default()).expect("selection");
    assert_eq!(selection.method, PlayMethod::DirectPlay);
    assert!(matches!(
        selection.stream,
        tv_client::jellyfin::MediaStreamChoice::DirectPlay
    ));

    let request = fixture
        .server
        .requests()
        .into_iter()
        .find(|request| request.path() == "/Items/item-1/PlaybackInfo")
        .expect("PlaybackInfo request");
    assert_eq!(
        request.query().get("UserId").map(String::as_str),
        Some("user-1")
    );
    let body = request.json();
    assert_eq!(body["MaxStreamingBitrate"], json!(6_000_000_u64));
    assert_eq!(body["MaxAudioChannels"], json!(2));
    assert_eq!(body["EnableDirectPlay"], json!(true));
    assert_eq!(body["EnableTranscoding"], json!(true));
    assert_eq!(body["AutoOpenLiveStream"], json!(false));
    let profile = &body["DeviceProfile"];
    assert_eq!(profile["MaxStreamingBitrate"], json!(6_000_000_u64));
    assert_eq!(
        profile["DirectPlayProfiles"],
        json!([{
            "Container": "mp4",
            "AudioCodec": "aac",
            "VideoCodec": "h264",
            "Type": "Video"
        }])
    );
    assert_eq!(profile["TranscodingProfiles"][0]["Protocol"], json!("hls"));
    assert_eq!(
        profile["TranscodingProfiles"][0]["VideoCodec"],
        json!("h264")
    );
    assert_eq!(
        profile["TranscodingProfiles"][0]["AudioCodec"],
        json!("aac")
    );
    assert_eq!(
        profile["TranscodingProfiles"][0]["MaxAudioChannels"],
        json!("2")
    );
    assert_eq!(profile["TranscodingProfiles"][0]["Container"], json!("ts"));

    let conditions = profile["CodecProfiles"]
        .as_array()
        .expect("codec profiles")
        .iter()
        .flat_map(|profile| profile["Conditions"].as_array().into_iter().flatten())
        .map(|condition| {
            (
                condition["Property"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                condition["Value"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(
        conditions.get("VideoRangeType").map(String::as_str),
        Some("SDR")
    );
    assert_eq!(
        conditions.get("VideoBitDepth").map(String::as_str),
        Some("8")
    );
    assert_eq!(conditions.get("Width").map(String::as_str), Some("1920"));
    assert_eq!(conditions.get("Height").map(String::as_str), Some("1080"));
    assert_eq!(
        conditions.get("VideoFramerate").map(String::as_str),
        Some("30")
    );
    assert_eq!(
        conditions.get("AudioChannels").map(String::as_str),
        Some("2")
    );

    let subtitle_formats = profile["SubtitleProfiles"]
        .as_array()
        .expect("subtitle profiles")
        .iter()
        .map(|profile| profile["Format"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    for format in ["ass", "ssa", "pgs", "dvdsub"] {
        assert!(subtitle_formats.iter().any(|value| value == format));
    }
    assert!(
        profile["SubtitleProfiles"]
            .as_array()
            .unwrap()
            .iter()
            .all(|profile| profile["Method"] == json!("Encode"))
    );
}

#[tokio::test]
async fn burn_in_subtitles_and_bitrate_cap_are_requested() {
    let fixture = client_fixture(standard_handler).await;
    let options = PlaybackInfoOptions {
        max_streaming_bitrate: 10_000_000,
        subtitle_stream_index: Some(2),
        burn_in_subtitles: true,
        ..PlaybackInfoOptions::default()
    };
    fixture
        .client
        .playback_info_with("item-1", &options, &fixture.cancel)
        .await
        .expect("playback info");
    let request = fixture
        .server
        .requests()
        .into_iter()
        .find(|request| request.path() == "/Items/item-1/PlaybackInfo")
        .unwrap();
    let body = request.json();
    assert_eq!(body["AlwaysBurnInSubtitleWhenTranscoding"], json!(true));
    assert_eq!(body["SubtitleStreamIndex"], json!(2));
    assert_eq!(body["MaxStreamingBitrate"], json!(10_000_000));
    assert!(
        PlaybackInfoOptions {
            max_streaming_bitrate: 10_000_001,
            ..PlaybackInfoOptions::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn selection_never_direct_plays_unproven_codecs() {
    let direct = serde_json::from_value::<PlaybackInfoResponse>(direct_playback_info())
        .expect("direct response");
    let selected = select_media_source(&direct, &SelectionOptions::default()).unwrap();
    assert_eq!(selected.method, PlayMethod::DirectPlay);

    let subtitle = SelectionOptions {
        subtitle_stream_index: Some(2),
        ..SelectionOptions::default()
    };
    let selected = select_media_source(&direct, &subtitle).unwrap();
    assert_eq!(selected.method, PlayMethod::Transcode);

    let hevc = serde_json::from_value::<PlaybackInfoResponse>(transcode_playback_info())
        .expect("transcode response");
    let selected = select_media_source(&hevc, &SelectionOptions::default()).unwrap();
    assert_eq!(selected.method, PlayMethod::Transcode);

    let mut hdr = transcode_playback_info();
    hdr["MediaSources"][0]["MediaStreams"][0]["Codec"] = json!("h264");
    hdr["MediaSources"][0]["MediaStreams"][0]["Width"] = json!(1920);
    hdr["MediaSources"][0]["MediaStreams"][0]["Height"] = json!(1080);
    hdr["MediaSources"][0]["MediaStreams"][0]["VideoRangeType"] = json!("HDR10");
    let hdr: PlaybackInfoResponse = serde_json::from_value(hdr).unwrap();
    let selected = select_media_source(&hdr, &SelectionOptions::default()).unwrap();
    assert_eq!(selected.method, PlayMethod::Transcode);

    let mut ten_bit = transcode_playback_info();
    ten_bit["MediaSources"][0]["MediaStreams"][0]["Codec"] = json!("h264");
    ten_bit["MediaSources"][0]["MediaStreams"][0]["Width"] = json!(1920);
    ten_bit["MediaSources"][0]["MediaStreams"][0]["Height"] = json!(1080);
    ten_bit["MediaSources"][0]["MediaStreams"][0]["VideoRangeType"] = json!("SDR");
    ten_bit["MediaSources"][0]["MediaStreams"][0]["BitDepth"] = json!(10);
    let ten_bit: PlaybackInfoResponse = serde_json::from_value(ten_bit).unwrap();
    let selected = select_media_source(&ten_bit, &SelectionOptions::default()).unwrap();
    assert_eq!(selected.method, PlayMethod::Transcode);

    let mut empty = direct_playback_info();
    empty["MediaSources"] = json!([]);
    let empty: PlaybackInfoResponse = serde_json::from_value(empty).unwrap();
    assert!(matches!(
        select_media_source(&empty, &SelectionOptions::default()),
        Err(SelectionError::NoCompatibleSource)
    ));

    let mut error = direct_playback_info();
    error["ErrorCode"] = json!("NoCompatibleStream");
    let error: PlaybackInfoResponse = serde_json::from_value(error).unwrap();
    assert!(matches!(
        select_media_source(&error, &SelectionOptions::default()),
        Err(SelectionError::ServerCode { .. })
    ));
}

#[test]
fn ticks_convert_with_a_twenty_four_hour_bound() {
    assert_eq!(ticks_from_millis(0).unwrap(), 0);
    assert_eq!(ticks_from_millis(1).unwrap(), 10_000);
    assert_eq!(ticks_from_millis(1_000).unwrap(), 10_000_000);
    assert_eq!(
        ticks_from_millis(86_400_000).unwrap(),
        86_400_000 * TICKS_PER_MILLISECOND
    );
    assert!(ticks_from_millis(86_400_001).is_err());
    assert!(ticks_from_millis(u64::MAX).is_err());

    assert_eq!(millis_from_ticks(0), 0);
    assert_eq!(millis_from_ticks(9_999), 0);
    assert_eq!(millis_from_ticks(10_000), 1);
    assert_eq!(millis_from_ticks(u64::MAX), 86_400_000);
    assert_eq!(RESUME_MIN_TICKS, 30 * TICKS_PER_MILLISECOND * 1_000);
}

#[test]
fn stream_urls_are_origin_and_base_path_bounded() {
    let base = url::Url::parse("https://media.example/jellyfin/").unwrap();
    let relative = StreamUrl::parse("/Videos/item/stream?api_key=secret").unwrap();
    assert_eq!(
        relative.resolve(&base).unwrap().as_str(),
        "https://media.example/jellyfin/Videos/item/stream?api_key=secret"
    );
    let same = StreamUrl::parse("https://media.example/jellyfin/Videos/x").unwrap();
    assert!(same.resolve(&base).is_ok());
    let other = StreamUrl::parse("https://evil.example/jellyfin/Videos/x").unwrap();
    assert!(matches!(
        other.resolve(&base),
        Err(JellyfinError::UnexpectedRedirect)
    ));
    let outside_prefix = StreamUrl::parse("https://media.example/other/Videos/x").unwrap();
    assert!(matches!(
        outside_prefix.resolve(&base),
        Err(JellyfinError::UnexpectedRedirect)
    ));
    assert!(!format!("{relative:?}").contains("secret"));
    assert!(StreamUrl::parse("file:///etc/passwd").is_err());
    assert!(StreamUrl::parse("//evil.example/x").is_err());
}

// ---------------------------------------------------------------------------
// Start / progress / stop sequencing and teardown.
// ---------------------------------------------------------------------------

async fn next_update(
    updates: &mut mpsc::Receiver<PlaybackUpdate>,
    deadline: Duration,
) -> Option<PlaybackUpdate> {
    tokio::time::timeout(deadline, updates.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test]
async fn reports_start_progress_and_stop_once() {
    let config = PlaybackConfig {
        progress_interval: Duration::from_millis(100),
        ..PlaybackConfig::default()
    };
    let fixture = fixture(standard_handler, config, 60_000).await;
    let Fixture {
        controller,
        cancel,
        server,
        fake,
        cancel_handle: _cancel_handle,
        ..
    } = fixture;
    let (command_tx, command_rx) = mpsc::channel(8);
    let (update_tx, mut update_rx) = mpsc::channel(64);
    let (shutdown, signal) = tv_client::state::shutdown_channel();
    let task = tokio::spawn(controller.run(command_rx, update_tx, signal, cancel.clone()));

    command_tx
        .send(PlaybackCommand::Play(
            PlayRequest::new("item-1").from_start(),
        ))
        .await
        .unwrap();

    let deadline = Duration::from_secs(5);
    let mut saw_loading = false;
    let mut saw_position = false;
    while !(saw_loading && saw_position) {
        match next_update(&mut update_rx, deadline).await {
            Some(PlaybackUpdate::Loading { .. }) => saw_loading = true,
            Some(PlaybackUpdate::Position { .. }) => saw_position = true,
            Some(_) => {}
            None => panic!("timed out waiting for playback updates"),
        }
    }

    // Let the 500 ms controller tick observe the ten-second-style interval.
    tokio::time::sleep(Duration::from_millis(700)).await;
    command_tx
        .send(PlaybackCommand::Seek {
            position_ms: 12_000,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    command_tx.send(PlaybackCommand::Stop).await.unwrap();

    let mut saw_stopped = false;
    while !saw_stopped {
        match next_update(&mut update_rx, deadline).await {
            Some(PlaybackUpdate::Stopped { .. }) => saw_stopped = true,
            Some(_) => {}
            None => panic!("timed out waiting for stop"),
        }
    }
    shutdown.cancel();
    let _ = task.await;

    let requests = server.requests();
    let starts = requests
        .iter()
        .filter(|request| request.path() == "/Sessions/Playing")
        .count();
    let stops = requests
        .iter()
        .filter(|request| request.path() == "/Sessions/Playing/Stopped")
        .count();
    let progress = requests
        .iter()
        .filter(|request| request.path() == "/Sessions/Playing/Progress")
        .count();
    assert_eq!(starts, 1, "start must be reported once");
    assert_eq!(stops, 1, "stop must be reported once");
    assert!(progress >= 2, "progress after tick and seek: {progress}");

    let first_start = requests
        .iter()
        .position(|request| request.path() == "/Sessions/Playing")
        .unwrap();
    let first_progress = requests
        .iter()
        .position(|request| request.path() == "/Sessions/Playing/Progress")
        .unwrap();
    let first_stop = requests
        .iter()
        .position(|request| request.path() == "/Sessions/Playing/Stopped")
        .unwrap();
    assert!(first_start < first_progress);
    assert!(first_progress < first_stop);

    let start_body = requests[first_start].json();
    assert_eq!(start_body["ItemId"], json!("item-1"));
    assert_eq!(start_body["MediaSourceId"], json!("source-1"));
    assert_eq!(start_body["PlaySessionId"], json!("play-1"));
    assert_eq!(start_body["PlayMethod"], json!("DirectPlay"));
    assert_eq!(start_body["IsPaused"], json!(false));

    let last_progress = requests
        .iter()
        .rev()
        .find(|request| request.path() == "/Sessions/Playing/Progress")
        .unwrap()
        .json();
    assert_eq!(last_progress["PositionTicks"], json!(120_000_000_u64));

    let commands = fake.commands.lock().unwrap().clone();
    assert!(
        commands
            .iter()
            .any(|command| command["type"] == json!("load"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command["type"] == json!("seek"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command["type"] == json!("stop"))
    );
}

#[tokio::test]
async fn transcode_teardown_releases_the_active_encoding() {
    let handler = |request: &MockRequest| match (request.method.as_str(), request.path()) {
        (_, "/Users/AuthenticateByName") => MockResponse::json(200, auth_body()),
        (_, "/Items/item-1") => MockResponse::json(200, direct_item(true)),
        (_, "/Items/item-1/PlaybackInfo") => MockResponse::json(200, transcode_playback_info()),
        (_, "/Sessions/Playing") => stream_response(),
        (_, "/Sessions/Playing/Progress") => stream_response(),
        (_, "/Sessions/Playing/Stopped") => stream_response(),
        ("DELETE", "/Videos/ActiveEncodings") => stream_response(),
        _ => MockResponse::empty(404),
    };
    let fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;
    let Fixture {
        controller,
        cancel,
        server,
        cancel_handle: _cancel_handle,
        ..
    } = fixture;
    let (command_tx, command_rx) = mpsc::channel(8);
    let (update_tx, mut update_rx) = mpsc::channel(64);
    let (shutdown, signal) = tv_client::state::shutdown_channel();
    let task = tokio::spawn(controller.run(command_rx, update_tx, signal, cancel.clone()));
    command_tx
        .send(PlaybackCommand::Play(
            PlayRequest::new("item-1").from_start(),
        ))
        .await
        .unwrap();
    let deadline = Duration::from_secs(5);
    let mut method = None;
    while method.is_none() {
        match next_update(&mut update_rx, deadline).await {
            Some(PlaybackUpdate::Method { method: value }) => method = Some(value),
            Some(_) => {}
            None => panic!("timed out waiting for negotiation"),
        }
    }
    assert_eq!(method, Some(PlayMethod::Transcode));
    tokio::time::sleep(Duration::from_millis(100)).await;
    command_tx.send(PlaybackCommand::Stop).await.unwrap();
    let mut saw_stopped = false;
    while !saw_stopped {
        match next_update(&mut update_rx, deadline).await {
            Some(PlaybackUpdate::Stopped { .. }) => saw_stopped = true,
            Some(_) => {}
            None => panic!("timed out waiting for teardown"),
        }
    }
    shutdown.cancel();
    let _ = task.await;

    let requests = server.requests();
    let release = requests
        .iter()
        .find(|request| request.method == "DELETE" && request.path() == "/Videos/ActiveEncodings")
        .expect("transcode release request");
    let query = release.query();
    assert_eq!(
        query.get("playSessionId").map(String::as_str),
        Some("play-hevc")
    );
    assert!(query.contains_key("deviceId"));
}

// ---------------------------------------------------------------------------
// Failure, cancellation and retry behaviour.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expired_token_fails_without_spawning_a_child() {
    let handler = |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Items/item-1" => MockResponse::json(200, direct_item(true)),
        "/Items/item-1/PlaybackInfo" => MockResponse::empty(401),
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;
    let now = Instant::now();
    let updates = fixture
        .controller
        .handle_command(
            PlaybackCommand::Play(PlayRequest::new("item-1")),
            now,
            &fixture.cancel,
        )
        .await;
    let failure = updates
        .iter()
        .find_map(|update| match update {
            PlaybackUpdate::Failed { code, .. } => Some(code.clone()),
            _ => None,
        })
        .expect("typed failure");
    assert_eq!(failure, "unauthorized");
    assert_eq!(fixture.fake.spawns.load(Ordering::SeqCst), 0);
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn cross_origin_stream_urls_are_rejected_before_load() {
    let handler = |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Items/item-1" => MockResponse::json(200, direct_item(true)),
        "/Items/item-1/PlaybackInfo" => {
            let mut body = transcode_playback_info();
            body["MediaSources"][0]["TranscodingUrl"] =
                json!("https://evil.invalid/Videos/item-1/stream?api_key=steal");
            MockResponse::json(200, body)
        }
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;
    let updates = fixture
        .controller
        .handle_command(
            PlaybackCommand::Play(PlayRequest::new("item-1")),
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let failure = updates
        .iter()
        .find_map(|update| match update {
            PlaybackUpdate::Failed { code, .. } => Some(code.clone()),
            _ => None,
        })
        .expect("typed failure");
    assert_eq!(failure, "redirect");
    assert_eq!(fixture.fake.spawns.load(Ordering::SeqCst), 0);
    assert!(!format!("{updates:?}").contains("steal"));
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn http_redirect_is_rejected() {
    let handler = |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Items/item-1" => MockResponse::json(200, direct_item(true)),
        "/Items/item-1/PlaybackInfo" => MockResponse::empty(302),
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;
    let updates = fixture
        .controller
        .handle_command(
            PlaybackCommand::Play(PlayRequest::new("item-1")),
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(updates.iter().any(|update| matches!(
        update,
        PlaybackUpdate::Failed { code, .. } if code == "redirect"
    )));
    assert_eq!(fixture.fake.spawns.load(Ordering::SeqCst), 0);
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn cancellation_before_negotiation_is_typed() {
    let mut fixture = fixture(standard_handler, PlaybackConfig::default(), 60_000).await;
    fixture.cancel_handle.cancel();
    let updates = fixture
        .controller
        .handle_command(
            PlaybackCommand::Play(PlayRequest::new("item-1")),
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(updates.iter().any(|update| matches!(
        update,
        PlaybackUpdate::Failed { code, .. } if code == "cancelled"
    )));
    assert_eq!(fixture.fake.spawns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_progress_is_retried_with_the_freshest_position() {
    let progress_calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&progress_calls);
    let handler = move |request: &MockRequest| match (request.method.as_str(), request.path()) {
        (_, "/Users/AuthenticateByName") => MockResponse::json(200, auth_body()),
        (_, "/Items/item-1") => MockResponse::json(200, direct_item(true)),
        (_, "/Items/item-1/PlaybackInfo") => MockResponse::json(200, direct_playback_info()),
        (_, "/Sessions/Playing") => stream_response(),
        (_, "/Sessions/Playing/Progress") => {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            if call < 2 {
                MockResponse::empty(500)
            } else {
                stream_response()
            }
        }
        (_, "/Sessions/Playing/Stopped") => stream_response(),
        _ => MockResponse::empty(404),
    };
    let config = PlaybackConfig {
        progress_interval: Duration::from_secs(60),
        progress_retry_delay: Duration::from_millis(10),
        max_progress_retries: 5,
        ..PlaybackConfig::default()
    };
    let mut fixture = fixture(handler, config, 60_000).await;
    let updates = fixture
        .controller
        .handle_command(
            PlaybackCommand::Play(PlayRequest::new("item-1").from_start()),
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, PlaybackUpdate::Method { .. }))
    );

    // First seek fails and is journaled.
    fixture.controller.seek(20_000, &fixture.cancel).await;
    assert_eq!(fixture.controller.journal().len(), 1);
    // A newer seek replaces the stale snapshot; the retry must carry 30 s,
    // never 20 s.
    fixture.controller.seek(30_000, &fixture.cancel).await;
    assert_eq!(fixture.controller.journal().len(), 1);
    let retry_at = Instant::now() + Duration::from_millis(20);
    fixture.controller.tick(retry_at, &fixture.cancel).await;
    assert!(fixture.controller.journal().is_empty());
    let last = fixture
        .server
        .requests()
        .into_iter()
        .rev()
        .find(|request| request.path() == "/Sessions/Playing/Progress")
        .expect("progress request")
        .json();
    assert_eq!(last["PositionTicks"], json!(300_000_000_u64));
    assert!(progress_calls.load(Ordering::SeqCst) >= 3);
    fixture.cancel_handle.cancel();
}

#[test]
fn progress_journal_is_bounded_fresh_and_user_isolated() {
    let now = Instant::now();
    let entry = |user: &str, sequence: u64, position: u64, next: Instant| ProgressEntry {
        user_id: user.to_string(),
        item_id: "item-1".to_string(),
        media_source_id: Some("source-1".to_string()),
        play_session_id: Some("play-1".to_string()),
        position_ticks: position,
        is_paused: false,
        play_method: PlayMethod::DirectPlay,
        audio_stream_index: None,
        subtitle_stream_index: None,
        sequence,
        recorded_at: now,
        attempts: 0,
        next_attempt: next,
    };
    let mut journal = ProgressJournal::new(2);
    journal.record(entry("user-1", 1, 10, now));
    journal.record(entry("user-1", 2, 20, now));
    assert_eq!(journal.len(), 1);
    let due = journal.due(now);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].position_ticks, 20);

    // A stale snapshot arriving after a newer one is ignored.
    journal.record(entry("user-1", 1, 5, now));
    assert_eq!(journal.entries()[0].position_ticks, 20);

    // A future attempt is not due.
    journal.record(entry("user-2", 3, 30, now + Duration::from_secs(60)));
    assert_eq!(journal.due(now).len(), 1);
    assert_eq!(journal.due(now + Duration::from_secs(61)).len(), 2);

    // Ageing drops snapshots that can no longer be safe to send.
    let mut old = ProgressJournal::new(4);
    old.record(entry("user-1", 4, 40, now));
    assert!(
        old.due(now + MAX_PROGRESS_AGE + Duration::from_secs(1))
            .is_empty()
    );

    // Capacity evicts the oldest entry.
    let mut bounded = ProgressJournal::new(1);
    bounded.record(entry("user-1", 5, 50, now));
    bounded.record(entry("user-2", 6, 60, now));
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded.entries()[0].user_id, "user-2");

    // User isolation and acknowledgement only touch their own entries.
    let mut isolated = ProgressJournal::new(4);
    isolated.record(entry("user-1", 7, 70, now));
    isolated.record(entry("user-2", 8, 80, now));
    isolated.clear_user("user-1");
    assert_eq!(isolated.len(), 1);
    assert_eq!(isolated.entries()[0].user_id, "user-2");
    isolated.acknowledge("user-2", "item-1", Some("play-1"), 8);
    assert!(isolated.is_empty());
}

// ---------------------------------------------------------------------------
// Resume, generation and track-change regressions.
// ---------------------------------------------------------------------------

async fn wait_for_command(commands: &Arc<Mutex<Vec<Value>>>, kind: &str) -> Value {
    for _ in 0..100 {
        if let Some(value) = commands
            .lock()
            .unwrap()
            .iter()
            .find(|value| value["type"].as_str() == Some(kind))
            .cloned()
        {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {kind} command");
}

async fn resume_case(played: bool, position_ticks: u64, expected_ms: u64) {
    let handler = move |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Items/item-1" => {
            let mut item = direct_item(false);
            item["RunTimeTicks"] = json!(6_000_000_000_u64);
            item["UserData"] = json!({
                "Played": played,
                "PlayCount": 0,
                "PlaybackPositionTicks": position_ticks,
                "PlayedPercentage": 0.0
            });
            MockResponse::json(200, item)
        }
        "/Items/item-1/PlaybackInfo" => {
            let mut response = direct_playback_info();
            response["MediaSources"][0]["RunTimeTicks"] = json!(6_000_000_000_u64);
            MockResponse::json(200, response)
        }
        "/Sessions/Playing" => stream_response(),
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 600_000).await;
    let updates = fixture
        .controller
        .handle_command(
            PlaybackCommand::Play(PlayRequest::new("item-1")),
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, PlaybackUpdate::Method { .. })),
        "resume case must negotiate: {updates:?}"
    );
    let load = wait_for_command(&fixture.fake.commands, "load").await;
    assert_eq!(
        load["start_position_ms"],
        json!(expected_ms),
        "played={played} position={position_ticks}"
    );
    let request = fixture
        .server
        .requests()
        .into_iter()
        .find(|request| request.path() == "/Items/item-1/PlaybackInfo")
        .expect("PlaybackInfo request");
    let body = request.json();
    if expected_ms > 0 {
        assert_eq!(
            body["StartTimeTicks"],
            json!(expected_ms * TICKS_PER_MILLISECOND)
        );
    } else {
        assert!(body.get("StartTimeTicks").is_none());
    }
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn resume_positions_follow_played_min_and_end_bounds() {
    // 10 s: below the 30 s minimum.
    resume_case(false, 100_000_000, 0).await;
    // Marked played always restarts.
    resume_case(true, 300_000_000, 0).await;
    // 30 s from the end of a 10 minute item: restart.
    resume_case(false, 5_700_000_000, 0).await;
    // 50 s into a 10 minute item: resume exactly.
    resume_case(false, 500_000_000, 50_000).await;
}

#[tokio::test]
async fn stale_idle_generation_never_stops_the_replacement() {
    let mut fixture = fixture(standard_handler, PlaybackConfig::default(), 60_000).await;
    let request = PlayRequest::new("item-1").from_start();
    fixture
        .controller
        .play(request.clone(), &fixture.cancel)
        .await
        .expect("first play");
    let now = Instant::now();
    fixture
        .controller
        .handle_notification(
            PlayerNotification::State {
                generation: 1,
                state: PlayerState::Loading,
            },
            now,
            &fixture.cancel,
        )
        .await;
    assert_eq!(fixture.controller.active_generation(), Some(1));

    fixture
        .controller
        .play(request, &fixture.cancel)
        .await
        .expect("replacement play");
    let stops_before = fixture
        .server
        .requests()
        .iter()
        .filter(|request| request.path() == "/Sessions/Playing/Stopped")
        .count();

    let updates = fixture
        .controller
        .handle_notification(
            PlayerNotification::State {
                generation: 1,
                state: PlayerState::Loading,
            },
            now,
            &fixture.cancel,
        )
        .await;
    assert!(updates.is_empty());
    assert_eq!(
        fixture.controller.active_generation(),
        None,
        "stale Loading from the replaced load must not be adopted"
    );

    let updates = fixture
        .controller
        .handle_notification(
            PlayerNotification::State {
                generation: 1,
                state: PlayerState::Idle,
            },
            now,
            &fixture.cancel,
        )
        .await;
    assert!(updates.is_empty(), "stale Idle must not emit: {updates:?}");
    assert!(fixture.controller.active_item_id().is_some());

    fixture
        .controller
        .handle_notification(
            PlayerNotification::State {
                generation: 2,
                state: PlayerState::Loading,
            },
            now,
            &fixture.cancel,
        )
        .await;
    assert_eq!(fixture.controller.active_generation(), Some(2));
    let updates = fixture
        .controller
        .handle_notification(
            PlayerNotification::State {
                generation: 1,
                state: PlayerState::Idle,
            },
            now,
            &fixture.cancel,
        )
        .await;
    assert!(updates.is_empty());
    assert_eq!(fixture.controller.active_generation(), Some(2));

    let stops_after = fixture
        .server
        .requests()
        .iter()
        .filter(|request| request.path() == "/Sessions/Playing/Stopped")
        .count();
    assert_eq!(stops_before, stops_after, "stale Idle must not report stop");
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn audio_change_preserves_the_selected_subtitle() {
    let mut fixture = fixture(transcode_handler, PlaybackConfig::default(), 60_000).await;
    let request = PlayRequest {
        subtitle_stream_index: Some(2),
        burn_in_subtitles: true,
        ..PlayRequest::new("item-1").from_start()
    };
    fixture
        .controller
        .play(request, &fixture.cancel)
        .await
        .expect("first play");
    fixture
        .controller
        .handle_command(
            PlaybackCommand::SelectAudio { index: Some(1) },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let bodies: Vec<Value> = fixture
        .server
        .requests()
        .into_iter()
        .filter(|request| request.path() == "/Items/item-1/PlaybackInfo")
        .map(|request| request.json())
        .collect();
    assert_eq!(bodies.len(), 2, "audio change must renegotiate");
    assert_eq!(bodies[1]["SubtitleStreamIndex"], json!(2));
    assert_eq!(
        bodies[1]["AlwaysBurnInSubtitleWhenTranscoding"],
        json!(true)
    );
    assert_eq!(bodies[1]["AudioStreamIndex"], json!(1));
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn subtitle_clear_disables_burn_in() {
    let mut fixture = fixture(transcode_handler, PlaybackConfig::default(), 60_000).await;
    fixture
        .controller
        .play(PlayRequest::new("item-1").from_start(), &fixture.cancel)
        .await
        .expect("first play");
    fixture
        .controller
        .handle_command(
            PlaybackCommand::SelectSubtitle { index: None },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let bodies: Vec<Value> = fixture
        .server
        .requests()
        .into_iter()
        .filter(|request| request.path() == "/Items/item-1/PlaybackInfo")
        .map(|request| request.json())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[1].get("SubtitleStreamIndex").is_none());
    assert_eq!(
        bodies[1]["AlwaysBurnInSubtitleWhenTranscoding"],
        json!(false)
    );
    fixture.cancel_handle.cancel();
}

fn transcode_handler(request: &MockRequest) -> MockResponse {
    match (request.method.as_str(), request.path()) {
        (_, "/Users/AuthenticateByName") => MockResponse::json(200, auth_body()),
        (_, "/Items/item-1") => MockResponse::json(200, direct_item(true)),
        (_, "/Items/item-1/PlaybackInfo") => MockResponse::json(200, transcode_playback_info()),
        (_, "/Sessions/Playing")
        | (_, "/Sessions/Playing/Progress")
        | (_, "/Sessions/Playing/Stopped") => stream_response(),
        _ => MockResponse::empty(404),
    }
}

#[tokio::test]
async fn retry_before_start_keeps_resume_position() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let handler = move |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Items/item-1" => {
            let mut item = direct_item(false);
            item["RunTimeTicks"] = json!(6_000_000_000_u64);
            item["UserData"] = json!({
                "Played": false,
                "PlayCount": 0,
                "PlaybackPositionTicks": 500_000_000_u64,
                "PlayedPercentage": 0.0
            });
            MockResponse::json(200, item)
        }
        "/Items/item-1/PlaybackInfo" => {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResponse::empty(500)
            } else {
                let mut response = direct_playback_info();
                response["MediaSources"][0]["RunTimeTicks"] = json!(6_000_000_000_u64);
                MockResponse::json(200, response)
            }
        }
        "/Sessions/Playing" => stream_response(),
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 600_000).await;
    let first = fixture
        .controller
        .handle_command(
            PlaybackCommand::Play(PlayRequest::new("item-1")),
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(
        first
            .iter()
            .any(|update| matches!(update, PlaybackUpdate::Failed { .. }))
    );
    let retried = fixture
        .controller
        .handle_command(PlaybackCommand::Retry, Instant::now(), &fixture.cancel)
        .await;
    assert!(
        retried
            .iter()
            .any(|update| matches!(update, PlaybackUpdate::Method { .. }))
    );
    let load = wait_for_command(&fixture.fake.commands, "load").await;
    assert_eq!(load["start_position_ms"], json!(50_000));
    let playback_bodies: Vec<Value> = fixture
        .server
        .requests()
        .into_iter()
        .filter(|request| request.path() == "/Items/item-1/PlaybackInfo")
        .map(|request| request.json())
        .collect();
    assert_eq!(playback_bodies[1]["StartTimeTicks"], json!(500_000_000_u64));
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn retry_after_failure_keeps_backoff_attempts() {
    let progress_calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&progress_calls);
    let handler = move |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Items/item-1" => MockResponse::json(200, direct_item(true)),
        "/Items/item-1/PlaybackInfo" => MockResponse::json(200, direct_playback_info()),
        "/Sessions/Playing" => stream_response(),
        "/Sessions/Playing/Progress" => {
            counter.fetch_add(1, Ordering::SeqCst);
            MockResponse::empty(500)
        }
        _ => MockResponse::empty(404),
    };
    let config = PlaybackConfig {
        progress_interval: Duration::from_secs(60),
        progress_retry_delay: Duration::from_millis(50),
        max_progress_retries: 5,
        ..PlaybackConfig::default()
    };
    let mut fixture = fixture(handler, config, 60_000).await;
    fixture
        .controller
        .play(PlayRequest::new("item-1").from_start(), &fixture.cancel)
        .await
        .expect("play");
    fixture.controller.seek(20_000, &fixture.cancel).await;
    let failed_at = Instant::now();
    assert_eq!(progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.controller.journal().len(), 1);

    // Before the backoff expires, a tick must not retry (or reset attempts).
    fixture.controller.tick(failed_at, &fixture.cancel).await;
    assert_eq!(progress_calls.load(Ordering::SeqCst), 1);
    // First retry fails and increments attempts; the snapshot stays.
    fixture
        .controller
        .tick(failed_at + Duration::from_millis(60), &fixture.cancel)
        .await;
    assert_eq!(progress_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.controller.journal().entries()[0].attempts, 1);
    // Still inside the new backoff: no extra request.
    fixture
        .controller
        .tick(failed_at + Duration::from_millis(70), &fixture.cancel)
        .await;
    assert_eq!(progress_calls.load(Ordering::SeqCst), 2);
    fixture
        .controller
        .tick(failed_at + Duration::from_millis(120), &fixture.cancel)
        .await;
    assert_eq!(progress_calls.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.controller.journal().entries()[0].attempts, 2);
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn next_episode_starts_the_following_episode() {
    let handler = |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Items/item-1" => MockResponse::json(200, direct_item(true)),
        "/Items/item-2" => MockResponse::json(
            200,
            json!({
                "Id": "item-2",
                "Name": "Second",
                "Type": "Episode",
                "RunTimeTicks": 600_000_000_u64,
                "SeriesId": "series-1",
                "IndexNumber": 2,
                "ParentIndexNumber": 1,
                "UserData": {"Played": false, "PlaybackPositionTicks": 0}
            }),
        ),
        "/Shows/series-1/Episodes" => MockResponse::json(
            200,
            json!({"Items": [
                {"Id": "item-1", "Name": "Pilot", "Type": "Episode"},
                {"Id": "item-2", "Name": "Second", "Type": "Episode"}
            ], "TotalRecordCount": 2}),
        ),
        "/Items/item-1/PlaybackInfo" | "/Items/item-2/PlaybackInfo" => {
            MockResponse::json(200, direct_playback_info())
        }
        "/Sessions/Playing" => stream_response(),
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;
    fixture
        .controller
        .play(PlayRequest::new("item-1").from_start(), &fixture.cancel)
        .await
        .expect("first episode");
    fixture
        .controller
        .handle_command(
            PlaybackCommand::NextEpisode,
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert_eq!(fixture.controller.active_item_id(), Some("item-2"));
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn library_command_reports_items_and_persistent_errors() {
    let fail = Arc::new(AtomicBool::new(false));
    let failing = Arc::clone(&fail);
    let handler = move |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Users/user-1/Items" => {
            if failing.load(Ordering::SeqCst) {
                MockResponse::empty(500)
            } else {
                MockResponse::json(
                    200,
                    json!({"Items": [
                        {"Id": "item-1", "Name": "Pilot", "Type": "Episode"},
                        {"Id": "item-2", "Name": "Second", "Type": "Episode"}
                    ], "TotalRecordCount": 2}),
                )
            }
        }
        _ => MockResponse::empty(500),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;
    let updates = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadLibrary {
                filter: LibraryFilter::Jellyfin,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let ready = updates
        .iter()
        .find_map(|update| match update {
            PlaybackUpdate::LibraryReady { items, .. } => Some(items.clone()),
            _ => None,
        })
        .expect("library ready");
    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].id, "item-1");
    assert_eq!(ready[0].title, "Pilot");

    fail.store(true, Ordering::SeqCst);
    let failed = fixture
        .controller
        .handle_command(
            PlaybackCommand::RetryLibrary,
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(
        failed
            .iter()
            .any(|update| matches!(update, PlaybackUpdate::LibraryFailed { .. }))
    );

    fixture
        .controller
        .handle_command(
            PlaybackCommand::Unsupported {
                code: "unsupported_source".to_string(),
                message: "YouTube playback is not supported by this build".to_string(),
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert_eq!(fixture.fake.spawns.load(Ordering::SeqCst), 0);
    fixture.cancel_handle.cancel();
}

// ---------------------------------------------------------------------------
// Browse and artwork commands.
// ---------------------------------------------------------------------------

fn browse_updates(updates: &[PlaybackUpdate]) -> Vec<&BrowseUpdate> {
    updates
        .iter()
        .filter_map(|update| match update {
            PlaybackUpdate::Browse(browse) => Some(browse),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn browse_commands_query_the_parent_and_report_pagination() {
    let handler = |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Users/user-1/Views" => MockResponse::json(
            200,
            json!({
                "Items": [
                    {"Id": "library-1", "Name": "Movies", "CollectionType": "movies"},
                    {"Id": "library-2", "Name": "Shows", "CollectionType": "tvshows"}
                ],
                "TotalRecordCount": 2
            }),
        ),
        "/Users/user-1/Items" => {
            let query = request.query();
            let start = query
                .get("StartIndex")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            let items = if start >= 48 {
                json!([{"Id": "movie-3", "Name": "Third", "Type": "Movie"}])
            } else {
                json!([
                    {
                        "Id": "movie-1",
                        "Name": "Dune",
                        "Type": "Movie",
                        "ProductionYear": 2021,
                        "Overview": "A desert epic",
                        "ImageTags": {"Primary": "primary-tag"}
                    },
                    {"Id": "movie-2", "Name": "Arrival", "Type": "Movie"}
                ])
            };
            MockResponse::json(
                200,
                json!({"Items": items, "TotalRecordCount": 49, "StartIndex": start}),
            )
        }
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;

    let libraries = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadJellyfinLibraries,
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let browse = browse_updates(&libraries);
    let BrowseUpdate::LibrariesReady { libraries } = browse[0] else {
        panic!("libraries ready: {browse:?}");
    };
    assert_eq!(libraries.len(), 2);
    assert_eq!(libraries[0].id, "library-1");
    assert_eq!(libraries[0].name, "Movies");
    assert_eq!(libraries[0].collection_type.as_deref(), Some("movies"));

    let page = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadJellyfinItems {
                library_id: "library-1".to_owned(),
                start_index: 24,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let browse = browse_updates(&page);
    let BrowseUpdate::ItemsReady {
        library_id,
        items,
        total,
        next_start,
    } = browse[0]
    else {
        panic!("items ready: {browse:?}");
    };
    assert_eq!(library_id, "library-1");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].title, "Dune");
    assert_eq!(items[0].primary_tag.as_deref(), Some("primary-tag"));
    assert_eq!(items[0].subtitle, "Movie · 2021");
    assert_eq!(*total, 49);
    assert_eq!(*next_start, Some(26), "24 + 2 items remain below 49");

    let last = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadJellyfinItems {
                library_id: "library-1".to_owned(),
                start_index: 48,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let browse = browse_updates(&last);
    let BrowseUpdate::ItemsReady {
        next_start, total, ..
    } = browse[0]
    else {
        panic!("items ready: {browse:?}");
    };
    assert_eq!(*next_start, None);
    assert_eq!(*total, 49);

    let requests = fixture.server.requests();
    let items_request = requests
        .iter()
        .find(|request| request.path() == "/Users/user-1/Items")
        .expect("items request");
    let query = items_request.query();
    assert_eq!(query.get("ParentId").map(String::as_str), Some("library-1"));
    let decode = |value: &str| {
        url::form_urlencoded::parse(value.as_bytes())
            .map(|(key, _)| key.into_owned())
            .collect::<String>()
    };
    assert_eq!(
        query
            .get("IncludeItemTypes")
            .map(|value| decode(value))
            .as_deref(),
        Some("Movie,Episode,Video")
    );
    assert_eq!(query.get("MediaTypes").map(String::as_str), Some("Video"));
    assert_eq!(
        query.get("Fields").map(|value| decode(value)).as_deref(),
        Some("UserData,ImageTags,Overview")
    );
    assert_eq!(query.get("StartIndex").map(String::as_str), Some("24"));
    assert_eq!(query.get("Limit").map(String::as_str), Some("24"));
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn browse_401_maps_to_a_session_expired_update() {
    let handler = |request: &MockRequest| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::json(200, auth_body()),
        "/Users/user-1/Views" | "/Users/user-1/Items" => MockResponse::empty(401),
        _ => MockResponse::empty(404),
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;

    let libraries = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadJellyfinLibraries,
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(matches!(
        browse_updates(&libraries)[0],
        BrowseUpdate::SessionExpired
    ));

    let items = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadJellyfinItems {
                library_id: "library-1".to_owned(),
                start_index: 0,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(matches!(
        browse_updates(&items)[0],
        BrowseUpdate::SessionExpired
    ));
    fixture.cancel_handle.cancel();
}

#[tokio::test]
async fn artwork_command_caches_metadata_and_reports_missing_images() {
    let image_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&image_requests);
    let handler = move |request: &MockRequest| {
        let path = request.path();
        if path == "/Users/AuthenticateByName" {
            MockResponse::json(200, auth_body())
        } else if path.ends_with("/Images/Primary") {
            counter.fetch_add(1, Ordering::SeqCst);
            if path == "/Items/item-1/Images/Primary" {
                MockResponse::bytes(200, "image/png", png(320, 480))
            } else {
                MockResponse::empty(404)
            }
        } else {
            MockResponse::empty(404)
        }
    };
    let mut fixture = fixture(handler, PlaybackConfig::default(), 60_000).await;

    let ready = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadArtwork {
                item_id: "item-1".to_owned(),
                tag: Some("tag-1".to_owned()),
                class: ArtworkClass::Poster,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    let BrowseUpdate::ArtworkReady {
        item_id,
        class,
        artwork,
    } = browse_updates(&ready)[0]
    else {
        panic!("artwork ready: {ready:?}");
    };
    assert_eq!(item_id, "item-1");
    assert_eq!(*class, ArtworkClass::Poster);
    assert_eq!((artwork.width(), artwork.height()), (320, 480));
    assert!(artwork.path().is_file());
    assert_eq!(image_requests.load(Ordering::SeqCst), 1);

    let cached = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadArtwork {
                item_id: "item-1".to_owned(),
                tag: Some("tag-1".to_owned()),
                class: ArtworkClass::Poster,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(matches!(
        browse_updates(&cached)[0],
        BrowseUpdate::ArtworkReady { .. }
    ));
    assert_eq!(
        image_requests.load(Ordering::SeqCst),
        1,
        "second request is a cache hit"
    );

    let missing_tag = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadArtwork {
                item_id: "item-1".to_owned(),
                tag: None,
                class: ArtworkClass::Preview,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(matches!(
        browse_updates(&missing_tag)[0],
        BrowseUpdate::ArtworkMissing {
            class: ArtworkClass::Preview,
            ..
        }
    ));
    assert_eq!(image_requests.load(Ordering::SeqCst), 1, "no tag, no request");

    let missing_image = fixture
        .controller
        .handle_command(
            PlaybackCommand::LoadArtwork {
                item_id: "other-item".to_owned(),
                tag: Some("tag-2".to_owned()),
                class: ArtworkClass::Poster,
            },
            Instant::now(),
            &fixture.cancel,
        )
        .await;
    assert!(matches!(
        browse_updates(&missing_image)[0],
        BrowseUpdate::ArtworkMissing { .. }
    ));
    assert_eq!(image_requests.load(Ordering::SeqCst), 2);
    fixture.cancel_handle.cancel();
}

// ---------------------------------------------------------------------------
// UI wiring.
// ---------------------------------------------------------------------------

#[test]
fn ui_wires_continue_watching_tracks_and_retry_to_commands() {
    let mut model = UiModel::new();
    model.set_continue_watching(vec![MediaItem::new(
        Source::Jellyfin,
        "item-r",
        "Continue Me",
    )]);
    model.set_home_focus(0);
    assert_eq!(
        model.handle_key(Key::Enter, Duration::ZERO),
        UiOutcome::Action(tv_client::model::Action::PlayJellyfin {
            item_id: "item-r".to_string()
        })
    );
    assert_eq!(model.screen(), Screen::Player);

    model.apply_playback_update(&PlaybackUpdate::Loading {
        item_id: "item-1".to_string(),
        title: Some("Pilot".to_string()),
    });
    model.apply_playback_update(&PlaybackUpdate::Tracks {
        audio: vec![tv_client::playback::TrackChoice {
            index: 1,
            label: "English AAC 2 ch".to_string(),
        }],
        subtitles: vec![tv_client::playback::TrackChoice {
            index: 2,
            label: "English SRT".to_string(),
        }],
        selected_audio: Some(1),
        selected_subtitle: None,
    });

    model.set_player_focus(1);
    let _ = model.handle_key(Key::Enter, Duration::from_millis(0));
    assert_eq!(model.player_menu(), Some(PlayerMenu::Subtitles));
    let _ = model.handle_key(Key::Down, Duration::from_millis(200));
    assert_eq!(
        model.handle_key(Key::Enter, Duration::from_millis(300)),
        UiOutcome::Playback(UiPlaybackCommand::SelectSubtitle { index: Some(2) })
    );
    assert_eq!(model.player_menu(), None);

    model.apply_playback_update(&PlaybackUpdate::Failed {
        item_id: Some("item-1".to_string()),
        code: "child_failed".to_string(),
        message: None,
        retry_available: true,
    });
    model.set_player_focus(4);
    assert_eq!(
        model.handle_key(Key::Enter, Duration::from_millis(500)),
        UiOutcome::Playback(UiPlaybackCommand::Retry)
    );
}

// ---------------------------------------------------------------------------
// Live server (opt-in).
// ---------------------------------------------------------------------------

fn live_credentials() -> Option<(String, String, String)> {
    let url = std::env::var("JELLYFIN_URL").ok()?;
    let user = std::env::var("JELLYFIN_USER").ok()?;
    let password = std::env::var("JELLYFIN_PASS").ok()?;
    Some((url, user, password))
}

#[tokio::test]
#[ignore]
async fn live_jellyfin_playback_negotiation() {
    let Some((url, user, password)) = live_credentials() else {
        panic!("set JELLYFIN_URL, JELLYFIN_USER and JELLYFIN_PASS");
    };
    let directory = temp_dir();
    let mut storage = Storage::open(
        directory.path(),
        tv_client::storage::HARD_ARTWORK_CACHE_BYTES,
    )
    .expect("storage");
    let config = ClientConfig {
        jellyfin_url: Some(url),
        ..ClientConfig::default()
    };
    let mut client = JellyfinClient::new(&config, DEVICE_NAME, &mut storage).expect("client");
    let (_handle, cancel) = cancellation();
    client
        .authenticate_by_name(&mut storage, &user, &password, &cancel)
        .await
        .expect("live login");
    let items = client
        .items(
            &ItemQuery::new().media_types(&["Video"]).page(Page::first()),
            &cancel,
        )
        .await
        .expect("live items");
    let item = items.items.first().expect("at least one video item");
    let response = client
        .playback_info_with(&item.id, &PlaybackInfoOptions::default(), &cancel)
        .await
        .expect("live playback info");
    let selection =
        select_media_source(&response, &SelectionOptions::default()).expect("live selection");
    assert!(!selection.source_id.is_empty());
    println!(
        "live negotiation item={} method={:?} source={}",
        item.id, selection.method, selection.source_id
    );
    let start = tv_client::jellyfin::PlaybackStartInfo {
        item_id: item.id.clone(),
        media_source_id: Some(selection.source_id.clone()),
        play_session_id: selection.play_session_id.clone(),
        position_ticks: 0,
        is_paused: false,
        is_muted: true,
        play_method: selection.method,
        audio_stream_index: selection.audio_stream_index,
        subtitle_stream_index: selection.subtitle_stream_index,
        can_seek: false,
        volume_level: None,
    };
    client
        .report_playback_start(&start, &cancel)
        .await
        .expect("live start report");
    let mut progress = start.clone();
    progress.position_ticks = 10_000_000;
    client
        .report_playback_progress(&progress, &cancel)
        .await
        .expect("live progress report");
    let stop = tv_client::jellyfin::PlaybackStopInfo {
        item_id: item.id.clone(),
        media_source_id: Some(selection.source_id),
        play_session_id: selection.play_session_id,
        position_ticks: 10_000_000,
        failed: false,
    };
    client
        .report_playback_stopped(&stop, &cancel)
        .await
        .expect("live stop report");
}

#[cfg(feature = "gstreamer-player")]
#[tokio::test]
#[ignore]
async fn live_pi_playback_short_run() {
    let Some((url, user, password)) = live_credentials() else {
        panic!("set JELLYFIN_URL, JELLYFIN_USER and JELLYFIN_PASS");
    };
    let directory = temp_dir();
    let mut storage = Storage::open(
        directory.path(),
        tv_client::storage::HARD_ARTWORK_CACHE_BYTES,
    )
    .expect("storage");
    let config = ClientConfig {
        jellyfin_url: Some(url),
        ..ClientConfig::default()
    };
    let mut client = JellyfinClient::new(&config, DEVICE_NAME, &mut storage).expect("client");
    let (_handle, cancel) = cancellation();
    client
        .authenticate_by_name(&mut storage, &user, &password, &cancel)
        .await
        .expect("live login");
    let item_id = match std::env::var("TV_LIVE_ITEM") {
        Ok(value) => value,
        Err(_) => {
            let items = client
                .items(
                    &ItemQuery::new().media_types(&["Video"]).page(Page::first()),
                    &cancel,
                )
                .await
                .expect("live items");
            items.items.first().expect("one video item").id.clone()
        }
    };
    let factory =
        tv_client::player::GStreamerChildFactory::new(env!("CARGO_BIN_EXE_tv-native-player"));
    let session = PlayerSession::start(factory, SupervisorConfig::default(), 8, 64)
        .expect("gstreamer session");
    let controller =
        PlaybackController::new(client, session, PlaybackConfig::default()).expect("controller");
    let (command_tx, command_rx) = mpsc::channel(8);
    let (update_tx, mut update_rx) = mpsc::channel(64);
    let (shutdown, signal) = tv_client::state::shutdown_channel();
    let task = tokio::spawn(controller.run(command_rx, update_tx, signal, cancel.clone()));
    command_tx
        .send(PlaybackCommand::Play(
            PlayRequest::new(item_id).from_start(),
        ))
        .await
        .unwrap();
    let mut playing = false;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_secs(10));
        match next_update(&mut update_rx, wait).await {
            Some(PlaybackUpdate::Position { .. }) => {
                playing = true;
                break;
            }
            Some(PlaybackUpdate::Failed { code, message, .. }) => {
                panic!("live playback failed: {code} {message:?}");
            }
            Some(other) => println!("live update: {other:?}"),
            None => {
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }
    if !playing {
        let _ = command_tx.send(PlaybackCommand::Stop).await;
        let outcome = tokio::time::timeout(Duration::from_secs(20), task).await;
        panic!("live playback never reported a position: task={outcome:?}");
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    command_tx.send(PlaybackCommand::Pause).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    command_tx.send(PlaybackCommand::Resume).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    command_tx.send(PlaybackCommand::Stop).await.unwrap();
    let stop_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < stop_deadline {
        match next_update(&mut update_rx, Duration::from_secs(5)).await {
            Some(PlaybackUpdate::Stopped { .. }) => break,
            Some(_) => {}
            None => break,
        }
    }
    shutdown.cancel();
    let _ = task.await;
}
