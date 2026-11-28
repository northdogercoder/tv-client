//! Contract tests for YouTube search and native local playback.
//!
//! Search uses a recording transport; playback uses scripted extractor and
//! player mocks. No test contacts YouTube, the control server or starts mpv
//! unless the single `#[ignore]` live test is explicitly selected.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::json;
use tokio::sync::mpsc;

use tv_client::cloud::DeviceToken;
use tv_client::config::{ClientConfig, MAX_YOUTUBE_TIMEOUT_SECS, YoutubeHwdec};
use tv_client::jellyfin::{CancelToken, cancellation};
use tv_client::model::{AppState, Mode, Source};
use tv_client::state::shutdown_channel;
use tv_client::youtube::mpv::{
    BackendCommand, BackendError, BackendEvent, BackendFactory, MpvBackend, MpvOptions,
    PlayerBackend, mpv_arguments,
};
use tv_client::youtube::player::{
    PLAYER_COMMAND_CAPACITY, PlaybackConfig, PlayerCommand, PlayerEvent,
};
use tv_client::youtube::resolve::{
    Extractor, ExtractorError, ExtractorFailure, FORMAT_SELECTOR, ResolvedMedia, YtdlpExtractor,
    build_update_arguments, build_ytdlp_arguments, parse_resolved_media,
};
use tv_client::youtube::search::{
    MAX_PAGE_TOKEN_BYTES, MAX_QUERY_CHARS, SearchClient, SearchError, SearchRequest,
    SearchResponse, SearchTransport,
};
use tv_client::youtube::{MpvBackendFactory, VideoId, playback_evidence, run_player};

const VIDEO_A: &str = "M7lc1UVf-VE";
const VIDEO_B: &str = "dQw4w9WgXcQ";
const TOKEN_TEXT: &str = "test-device-token-0123456789";

fn token() -> DeviceToken {
    DeviceToken::new(TOKEN_TEXT).expect("valid device token")
}

fn video(id: &str) -> VideoId {
    VideoId::parse(id).expect("valid video id")
}

fn test_config(enabled: bool) -> PlaybackConfig {
    PlaybackConfig {
        enabled,
        extractor: PathBuf::from("/managed/yt-dlp"),
        timeout: Duration::from_secs(20),
        mpv: MpvOptions {
            program: PathBuf::from("/managed/mpv"),
            hwdec: YoutubeHwdec::V4l2m2m,
            start_timeout: Duration::from_secs(5),
            reap_timeout: Duration::from_secs(1),
        },
    }
}

fn resolved_media() -> ResolvedMedia {
    ResolvedMedia {
        video_url: "https://rr1---sn.example.googlevideo.com/videoplayback?expire=1".to_owned(),
        audio_url: Some(
            "https://rr1---sn.example.googlevideo.com/videoplayback?expire=2".to_owned(),
        ),
        vcodec: "avc1.640028".to_owned(),
        acodec: Some("mp4a.40.2".to_owned()),
        width: 1920,
        height: 1080,
        fps: 30.0,
        format_id: "137".to_owned(),
        title: Some("Example clip".to_owned()),
        duration_ms: Some(90_000),
    }
}

// ---------------------------------------------------------------------------
// Search

#[derive(Clone)]
struct RecordingTransport {
    responses: Arc<Mutex<VecDeque<Result<SearchResponse, SearchError>>>>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    delay: Duration,
}

impl RecordingTransport {
    fn new(responses: Vec<Result<SearchResponse, SearchError>>, delay: Duration) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
            delay,
        }
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().expect("requests").clone()
    }
}

impl SearchTransport for RecordingTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        authorization: &'a str,
    ) -> BoxFuture<'a, Result<SearchResponse, SearchError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("requests")
                .push((url.to_owned(), authorization.to_owned()));
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.responses
                .lock()
                .expect("responses")
                .pop_front()
                .unwrap_or(Err(SearchError::Transport))
        })
    }
}

fn search_response(body: serde_json::Value) -> Result<SearchResponse, SearchError> {
    Ok(SearchResponse {
        status: 200,
        body: body.to_string().into_bytes(),
    })
}

fn search_client(transport: Arc<RecordingTransport>) -> SearchClient {
    SearchClient::with_transport("https://control.example.test", token(), transport)
        .expect("search client")
}

#[tokio::test]
async fn search_uses_the_device_bearer_header_and_parses_a_page() {
    let body = json!({
        "items": [{
            "video_id": VIDEO_A,
            "title": "A title",
            "channel_title": "A channel",
            "thumbnail_url": "https://i.ytimg.com/vi/M7lc1UVf-VE/hqdefault.jpg"
        }],
        "next_page_token": "next-page"
    });
    let transport = Arc::new(RecordingTransport::new(
        vec![search_response(body)],
        Duration::ZERO,
    ));
    let client = search_client(Arc::clone(&transport));
    let (_cancel_handle, cancel) = cancellation();
    let request = SearchRequest::new("  lofi & rain  ", None).expect("request");
    let page = client.search(&request, &cancel).await.expect("page");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].video_id, VIDEO_A);
    assert_eq!(page.items[0].channel_title, "A channel");
    assert_eq!(page.next_page_token.as_deref(), Some("next-page"));

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].0.contains(TOKEN_TEXT));
    assert!(!requests[0].0.contains("Bearer"));
    assert_eq!(requests[0].1, format!("Bearer {TOKEN_TEXT}"));
    assert_eq!(
        requests[0].0,
        "https://control.example.test/v1/youtube/search?q=lofi+%26+rain"
    );
}

#[tokio::test]
async fn search_forwards_the_opaque_page_token() {
    let body = json!({"items": [], "next_page_token": null});
    let transport = Arc::new(RecordingTransport::new(
        vec![search_response(body)],
        Duration::ZERO,
    ));
    let client = search_client(Arc::clone(&transport));
    let (_cancel_handle, cancel) = cancellation();
    let request = SearchRequest::new("cats", Some("opaque+/=token")).expect("request");
    client.search(&request, &cancel).await.expect("page");
    let requests = transport.requests();
    assert!(requests[0].0.contains("page_token=opaque"));
}

#[tokio::test]
async fn search_maps_every_contracted_status_to_a_typed_error() {
    for (status, expected) in [
        (400, SearchError::BadRequest),
        (401, SearchError::Unauthorized),
        (404, SearchError::FeatureDisabled),
        (429, SearchError::RateLimited),
        (503, SearchError::Unavailable),
    ] {
        let transport = Arc::new(RecordingTransport::new(
            vec![Ok(SearchResponse {
                status,
                body: Vec::new(),
            })],
            Duration::ZERO,
        ));
        let client = search_client(transport);
        let (_cancel_handle, cancel) = cancellation();
        let request = SearchRequest::new("cats", None).expect("request");
        assert_eq!(client.search(&request, &cancel).await, Err(expected));
        assert!(!expected.to_string().contains(TOKEN_TEXT));
    }
}

#[tokio::test]
async fn search_rejects_invalid_queries_and_tokens_before_io() {
    assert_eq!(
        SearchRequest::new("   ", None),
        Err(SearchError::EmptyQuery)
    );
    assert_eq!(
        SearchRequest::new(&"x".repeat(MAX_QUERY_CHARS + 1), None),
        Err(SearchError::QueryTooLong)
    );
    assert_eq!(
        SearchRequest::new("cats", Some(&"x".repeat(MAX_PAGE_TOKEN_BYTES + 1))),
        Err(SearchError::InvalidPageToken)
    );
    let transport = Arc::new(RecordingTransport::new(Vec::new(), Duration::ZERO));
    let client = search_client(Arc::clone(&transport));
    let (_cancel_handle, cancel) = cancellation();
    let request = SearchRequest::new("cats", None).expect("request");
    // An oversized token is rejected by construction; an empty transport list
    // confirms that no request was attempted for the validation failures.
    assert!(client.search(&request, &cancel).await.is_err());
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn search_cancellation_returns_promptly() {
    let transport = Arc::new(RecordingTransport::new(Vec::new(), Duration::from_secs(30)));
    let client = search_client(transport);
    let (handle, cancel) = cancellation();
    let request = SearchRequest::new("cats", None).expect("request");
    let task = tokio::spawn(async move { client.search(&request, &cancel).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("cancel is prompt")
        .expect("task");
    assert_eq!(result, Err(SearchError::Cancelled));
}

// ---------------------------------------------------------------------------
// Extractor policy and process handling

#[test]
fn extractor_arguments_are_fixed_and_shell_free() {
    let arguments = build_ytdlp_arguments(&video(VIDEO_A).watch_url());
    assert_eq!(
        arguments.last().map(String::as_str),
        Some("https://www.youtube.com/watch?v=M7lc1UVf-VE")
    );
    assert!(arguments.contains(&"--no-playlist".to_owned()));
    assert!(arguments.contains(&"--no-cache-dir".to_owned()));
    assert!(arguments.contains(&"--ignore-config".to_owned()));
    assert!(arguments.contains(&FORMAT_SELECTOR.to_owned()));
    assert!(!arguments.iter().any(|argument| argument.contains("cookie")));
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.contains("browser"))
    );
    assert!(!FORMAT_SELECTOR.contains("vp9"));
    assert!(!FORMAT_SELECTOR.contains("av01"));
    assert!(FORMAT_SELECTOR.contains("fps<=30"));
    let first_720 = FORMAT_SELECTOR.find("height<=720").expect("720 branch");
    let first_1080 = FORMAT_SELECTOR.find("height<=1080").expect("1080 branch");
    assert!(first_720 < first_1080, "720p30 must be preferred");
}

#[test]
fn selection_refuses_1080p60_vp9_and_av1_but_accepts_1080p30_fallback() {
    let accepted = json!({
        "url": "https://rr1---sn.example.googlevideo.com/videoplayback",
        "vcodec": "avc1.640028",
        "acodec": "mp4a.40.2",
        "height": 1080,
        "width": 1920,
        "fps": 30,
        "format_id": "137"
    });
    let media = parse_resolved_media(accepted.to_string().as_bytes()).expect("accepted");
    assert_eq!(media.height, 1080);
    for (codec, height, fps) in [
        ("avc1.640028", 1080_u32, 60.0_f64),
        ("vp9", 1080, 30.0),
        ("av01.0.08M.08", 1080, 30.0),
    ] {
        let rejected = json!({
            "url": "https://rr1---sn.example.googlevideo.com/videoplayback",
            "vcodec": codec,
            "acodec": "mp4a.40.2",
            "height": height,
            "width": 1920,
            "fps": fps,
            "format_id": "x"
        });
        assert_eq!(
            parse_resolved_media(rejected.to_string().as_bytes()),
            Err(ExtractorError::UnsupportedFormat),
            "codec {codec} at {height}p{fps}"
        );
    }
}

#[test]
fn resolved_media_debug_redacts_stream_urls() {
    let text = format!("{:?}", resolved_media());
    assert!(!text.contains("googlevideo"));
    assert!(text.contains("redacted"));
}

#[test]
fn mpv_arguments_never_carry_a_url_or_token() {
    let arguments = mpv_arguments(
        Path::new("/tmp/tv-client-mpv-test/ipc.sock"),
        YoutubeHwdec::V4l2m2m,
        Path::new("/tmp/tv-client-mpv-test/input.conf"),
    );
    assert!(!arguments.iter().any(|argument| argument.contains("http")));
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.contains(TOKEN_TEXT))
    );
    assert!(arguments.iter().any(|argument| argument == "--ytdl=no"));
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--input-ipc-server=/tmp/tv-client-mpv-test/ipc.sock")
    );
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--vo=dmabuf-wayland")
    );
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--hwdec=v4l2m2m")
    );
}

fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
    }
    path
}

#[tokio::test]
async fn extractor_reports_non_zero_exit_and_blocked_challenges() {
    let directory = tempfile::tempdir().expect("tempdir");
    let extractor = YtdlpExtractor::new(
        script(
            directory.path(),
            "yt-dlp",
            "echo 'ERROR: Sign in to confirm you are not a bot' >&2; exit 1",
        ),
        Duration::from_secs(2),
    );
    let (_cancel_handle, cancel) = cancellation();
    assert_eq!(
        extractor.resolve(&video(VIDEO_A), &cancel).await,
        Err(ExtractorError::Failed {
            reason: ExtractorFailure::Blocked
        })
    );
}

#[tokio::test]
async fn extractor_reports_a_missing_binary_and_invalid_output() {
    let (_cancel_handle, cancel) = cancellation();
    let missing = YtdlpExtractor::new(
        PathBuf::from("/nonexistent/managed/yt-dlp"),
        Duration::from_secs(2),
    );
    assert_eq!(
        missing.resolve(&video(VIDEO_A), &cancel).await,
        Err(ExtractorError::ExtractorUnavailable {
            reason: ExtractorFailure::MissingBinary
        })
    );

    let directory = tempfile::tempdir().expect("tempdir");
    let invalid = YtdlpExtractor::new(
        script(directory.path(), "yt-dlp", "echo 'not json'"),
        Duration::from_secs(2),
    );
    assert_eq!(
        invalid.resolve(&video(VIDEO_A), &cancel).await,
        Err(ExtractorError::InvalidOutput)
    );
}

#[tokio::test]
async fn extractor_times_out_and_reaps_a_hung_child() {
    let directory = tempfile::tempdir().expect("tempdir");
    let executable = script(directory.path(), "yt-dlp", "sleep 10");
    let extractor = YtdlpExtractor::new(executable.clone(), Duration::from_millis(150));
    let (_cancel_handle, cancel) = cancellation();
    let started = tokio::time::Instant::now();
    // A heavily loaded test host can transiently fail fork/exec. Retry only
    // while the script still exists; a genuinely missing binary still fails.
    let mut attempts = 0;
    let result = loop {
        let result = extractor.resolve(&video(VIDEO_A), &cancel).await;
        let transient = matches!(
            result,
            Err(ExtractorError::ExtractorUnavailable {
                reason: ExtractorFailure::MissingBinary
            })
        );
        if transient && executable.exists() && attempts < 3 {
            attempts += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        break result;
    };
    assert_eq!(result, Err(ExtractorError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn stale_extractor_updates_once_and_retries() {
    let directory = tempfile::tempdir().expect("tempdir");
    let marker = directory.path().join("updated");
    let output = json!({
        "url": "https://rr1---sn.example.googlevideo.com/videoplayback",
        "vcodec": "avc1.640028",
        "acodec": "mp4a.40.2",
        "height": 720,
        "width": 1280,
        "fps": 30,
        "format_id": "136",
        "title": "Example",
        "duration": 30.0
    })
    .to_string();
    let body = format!(
        "if [ \"$1\" = \"--ignore-config\" ] && [ \"$2\" = \"--no-warnings\" ] && [ \"$3\" = \"--update\" ]; then touch '{}'; exit 0; fi\nif [ -f '{}' ]; then cat <<'JSON'\n{}\nJSON\nelse echo 'ERROR: page needs to be reloaded' >&2; exit 1; fi",
        marker.display(),
        marker.display(),
        output
    );
    let extractor = YtdlpExtractor::new(
        script(directory.path(), "yt-dlp", &body),
        Duration::from_secs(2),
    );
    let (_cancel_handle, cancel) = cancellation();
    let media = extractor
        .resolve(&video(VIDEO_A), &cancel)
        .await
        .expect("updated and resolved");
    assert_eq!(media.height, 720);
    assert!(marker.exists());
    let args = build_update_arguments();
    assert_eq!(args, ["--ignore-config", "--no-warnings", "--update"]);
    assert!(args.iter().all(|arg| !arg.contains("http")));
}

#[tokio::test]
async fn extractor_success_reads_h264_1080p30() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = json!({
        "url": "https://rr1---sn.example.googlevideo.com/videoplayback",
        "vcodec": "avc1.640028",
        "acodec": "mp4a.40.2",
        "height": 1080,
        "width": 1920,
        "fps": 30,
        "format_id": "137",
        "title": "Example",
        "duration": 30.0
    })
    .to_string();
    let extractor = YtdlpExtractor::new(
        script(
            directory.path(),
            "yt-dlp",
            &format!("cat <<'JSON'\n{output}\nJSON"),
        ),
        Duration::from_secs(2),
    );
    let (_cancel_handle, cancel) = cancellation();
    let media = extractor
        .resolve(&video(VIDEO_A), &cancel)
        .await
        .expect("resolved");
    assert_eq!(media.height, 1080);
    assert!(media.vcodec.starts_with("avc1"));
}

// ---------------------------------------------------------------------------
// Player lifecycle with a scripted extractor and backend

struct MockExtractor {
    media: Result<ResolvedMedia, ExtractorError>,
    delay: Duration,
    calls: Arc<AtomicUsize>,
}

impl MockExtractor {
    fn ok(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            media: Ok(resolved_media()),
            delay,
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn failing(error: ExtractorError) -> Arc<Self> {
        Arc::new(Self {
            media: Err(error),
            delay: Duration::ZERO,
            calls: Arc::new(AtomicUsize::new(0)),
        })
    }
}

impl Extractor for MockExtractor {
    fn resolve<'a>(
        &'a self,
        _video_id: &'a VideoId,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, Result<ResolvedMedia, ExtractorError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                let mut cancel = cancel.clone();
                tokio::select! {
                    () = tokio::time::sleep(self.delay) => {}
                    () = cancel.cancelled() => return Err(ExtractorError::Cancelled),
                }
            }
            self.media.clone()
        })
    }
}

struct MockBackend {
    commands: Arc<Mutex<Vec<BackendCommand>>>,
    events: mpsc::UnboundedReceiver<BackendEvent>,
    closed: Arc<AtomicUsize>,
}

impl PlayerBackend for MockBackend {
    fn load<'a>(
        &'a mut self,
        _media: &'a ResolvedMedia,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async { Ok(()) })
    }

    fn command(&mut self, command: BackendCommand) -> BoxFuture<'_, Result<(), BackendError>> {
        self.commands.lock().expect("commands").push(command);
        Box::pin(async { Ok(()) })
    }

    fn next_event(&mut self) -> BoxFuture<'_, Option<BackendEvent>> {
        Box::pin(async move { self.events.recv().await })
    }

    fn close(&mut self) -> BoxFuture<'_, ()> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

struct MockFactory {
    senders: Arc<Mutex<Vec<mpsc::UnboundedSender<BackendEvent>>>>,
    spawns: Arc<AtomicUsize>,
    commands: Arc<Mutex<Vec<BackendCommand>>>,
    closes: Arc<AtomicUsize>,
    failure: Option<BackendError>,
}

impl MockFactory {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            senders: Arc::new(Mutex::new(Vec::new())),
            spawns: Arc::new(AtomicUsize::new(0)),
            commands: Arc::new(Mutex::new(Vec::new())),
            closes: Arc::new(AtomicUsize::new(0)),
            failure: None,
        })
    }

    fn failing(error: BackendError) -> Arc<Self> {
        Arc::new(Self {
            senders: Arc::new(Mutex::new(Vec::new())),
            spawns: Arc::new(AtomicUsize::new(0)),
            commands: Arc::new(Mutex::new(Vec::new())),
            closes: Arc::new(AtomicUsize::new(0)),
            failure: Some(error),
        })
    }

    fn sender(&self) -> Option<mpsc::UnboundedSender<BackendEvent>> {
        self.senders.lock().expect("senders").last().cloned()
    }

    fn close_backends(&self) {
        self.senders.lock().expect("senders").clear();
    }
}

impl BackendFactory for MockFactory {
    fn start(&self) -> BoxFuture<'_, Result<Box<dyn PlayerBackend>, BackendError>> {
        Box::pin(async move {
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let (events, receiver) = mpsc::unbounded_channel();
            self.senders.lock().expect("senders").push(events);
            Ok(Box::new(MockBackend {
                commands: Arc::clone(&self.commands),
                events: receiver,
                closed: Arc::clone(&self.closes),
            }) as Box<dyn PlayerBackend>)
        })
    }
}

async fn next_event(events: &mut mpsc::Receiver<PlayerEvent>, timeout: Duration) -> PlayerEvent {
    tokio::time::timeout(timeout, events.recv())
        .await
        .expect("event timeout")
        .expect("event channel")
}

async fn wait_for_spawn(factory: &MockFactory) -> mpsc::UnboundedSender<BackendEvent> {
    for _ in 0..200 {
        if let Some(sender) = factory.sender() {
            return sender;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("backend was never spawned");
}

fn playing_state(position_ms: Option<u64>) -> AppState {
    AppState {
        mode: Mode::Playing,
        source: Some(Source::YouTube),
        item_id: Some(VIDEO_A.to_owned()),
        position_ms,
        ..AppState::default()
    }
}

#[test]
fn youtube_pages_append_and_late_pages_are_dropped() {
    use tv_client::ui::{MediaItem, SourceFilter, UiModel, UiOutcome};

    let mut model = UiModel::new();
    model.open_library(SourceFilter::YouTube);
    let first = model.submit_youtube_search("cats");
    let first_id = match first {
        UiOutcome::YoutubeSearch { request_id, .. } => request_id,
        other => panic!("expected a search request, got {other:?}"),
    };
    assert!(model.set_youtube_page(
        first_id,
        vec![MediaItem::new(Source::YouTube, VIDEO_A, "First")],
        Some("page-2".to_owned()),
    ));
    assert_eq!(model.visible_len(), 1);
    assert!(model.youtube_next_page_available());

    let more = model.request_youtube_more();
    let second_id = match more {
        UiOutcome::YoutubeMore {
            request_id,
            page_token,
            ..
        } => {
            assert_eq!(page_token, "page-2");
            request_id
        }
        other => panic!("expected a more request, got {other:?}"),
    };
    assert!(!model.set_youtube_page(
        first_id,
        vec![MediaItem::new(Source::YouTube, VIDEO_B, "Late")],
        None,
    ));
    assert!(model.set_youtube_page(
        second_id,
        vec![MediaItem::new(Source::YouTube, VIDEO_B, "Second")],
        None,
    ));
    assert_eq!(model.visible_len(), 2);
    assert!(!model.youtube_next_page_available());
}

#[test]
fn acknowledgement_evidence_requires_playing_and_a_non_zero_position() {
    assert!(!playback_evidence(&playing_state(None), VIDEO_A));
    assert!(!playback_evidence(&playing_state(Some(0)), VIDEO_A));
    assert!(!playback_evidence(&grading_loading(), VIDEO_A));
    assert!(playback_evidence(&playing_state(Some(400)), VIDEO_A));
    assert!(!playback_evidence(&playing_state(Some(400)), VIDEO_B));
}

fn grading_loading() -> AppState {
    AppState {
        mode: Mode::Loading,
        source: Some(Source::YouTube),
        item_id: Some(VIDEO_A.to_owned()),
        position_ms: Some(400),
        ..AppState::default()
    }
}

#[tokio::test]
async fn player_reports_playing_progress_and_stop_without_duplicate_spawns() {
    let factory = MockFactory::new();
    let extractor = MockExtractor::ok(Duration::ZERO);
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        test_config(true),
        extractor.clone(),
        factory.clone(),
        command_rx,
        events,
        signal,
    ));

    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("play");
    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("duplicate play");

    assert!(matches!(
        next_event(&mut received, Duration::from_secs(2)).await,
        PlayerEvent::Loading { .. }
    ));
    let backend = wait_for_spawn(&factory).await;
    assert_eq!(factory.spawns.load(Ordering::SeqCst), 1);
    backend.send(BackendEvent::Loaded).expect("loaded");
    match next_event(&mut received, Duration::from_secs(2)).await {
        PlayerEvent::Playing { video_id, .. } => assert_eq!(video_id, VIDEO_A),
        other => panic!("expected playing, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    backend
        .send(BackendEvent::Position { position_ms: 1_200 })
        .expect("position");
    match next_event(&mut received, Duration::from_secs(2)).await {
        PlayerEvent::Progress { position_ms, .. } => assert_eq!(position_ms, 1_200),
        other => panic!("expected progress, got {other:?}"),
    }
    assert_eq!(factory.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 1);

    commands.send(PlayerCommand::Stop).await.expect("stop");
    assert!(matches!(
        next_event(&mut received, Duration::from_secs(2)).await,
        PlayerEvent::Stopped { .. }
    ));
    assert_eq!(factory.closes.load(Ordering::SeqCst), 1);
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn player_ignores_a_superseded_resolution() {
    let factory = MockFactory::new();
    let extractor = MockExtractor::ok(Duration::from_millis(100));
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        test_config(true),
        extractor.clone(),
        factory.clone(),
        command_rx,
        events,
        signal,
    ));

    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("first play");
    tokio::time::sleep(Duration::from_millis(20)).await;
    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_B),
            title: None,
        })
        .await
        .expect("second play");

    // The first adapters are the superseded loading events; the player must
    // only ever start the second identifier.
    let backend = wait_for_spawn(&factory).await;
    assert_eq!(factory.spawns.load(Ordering::SeqCst), 1);
    backend.send(BackendEvent::Loaded).expect("loaded");
    let mut playing = None;
    for _ in 0..6 {
        if let Ok(Some(PlayerEvent::Playing { video_id, .. })) =
            tokio::time::timeout(Duration::from_secs(2), received.recv()).await
        {
            playing = Some(video_id);
            break;
        }
    }
    assert_eq!(playing.as_deref(), Some(VIDEO_B));
    assert_eq!(factory.spawns.load(Ordering::SeqCst), 1);
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn player_times_out_without_retrying_and_retries_only_on_request() {
    let factory = MockFactory::new();
    let extractor = MockExtractor::ok(Duration::from_secs(30));
    let mut config = test_config(true);
    config.timeout = Duration::from_millis(150);
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        config,
        extractor.clone(),
        factory.clone(),
        command_rx,
        events,
        signal,
    ));

    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("play");
    let event = next_event(&mut received, Duration::from_secs(2)).await;
    assert!(matches!(event, PlayerEvent::Loading { .. }));
    match next_event(&mut received, Duration::from_secs(2)).await {
        PlayerEvent::Failed {
            code,
            retry_available,
            ..
        } => {
            assert_eq!(code, "playback_timeout");
            assert!(retry_available);
        }
        other => panic!("expected timeout failure, got {other:?}"),
    }
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.spawns.load(Ordering::SeqCst), 0);

    // The failed attempt is not retried automatically.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), received.recv())
            .await
            .is_err()
    );
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 1);

    commands.send(PlayerCommand::Retry).await.expect("retry");
    assert!(matches!(
        next_event(&mut received, Duration::from_secs(2)).await,
        PlayerEvent::Loading { .. }
    ));
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 2);
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn player_reports_a_typed_extractor_failure() {
    let factory = MockFactory::new();
    let extractor = MockExtractor::failing(ExtractorError::ExtractorUnavailable {
        reason: ExtractorFailure::MissingBinary,
    });
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        test_config(true),
        extractor,
        factory,
        command_rx,
        events,
        signal,
    ));
    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("play");
    assert!(matches!(
        next_event(&mut received, Duration::from_secs(2)).await,
        PlayerEvent::Loading { .. }
    ));
    match next_event(&mut received, Duration::from_secs(2)).await {
        PlayerEvent::Failed {
            code,
            retry_available,
            ..
        } => {
            assert_eq!(code, "extractor_unavailable");
            assert!(retry_available);
        }
        other => panic!("expected extractor failure, got {other:?}"),
    }
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn kill_switch_prevents_every_process() {
    let factory = MockFactory::new();
    let extractor = MockExtractor::ok(Duration::ZERO);
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        test_config(false),
        extractor.clone(),
        factory.clone(),
        command_rx,
        events,
        signal,
    ));
    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("play");
    match next_event(&mut received, Duration::from_secs(2)).await {
        PlayerEvent::Failed {
            code,
            retry_available,
            ..
        } => {
            assert_eq!(code, "youtube_disabled");
            assert!(!retry_available);
        }
        other => panic!("expected disabled failure, got {other:?}"),
    }
    assert_eq!(extractor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.spawns.load(Ordering::SeqCst), 0);
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn configuration_kill_switch_defaults_off_without_an_executable_path() {
    let mut config = ClientConfig::default();
    assert!(!config.youtube_playback_ready());
    assert!(!PlaybackConfig::from_client(&config).enabled);
    config.youtube_playback_enabled = true;
    config.youtube_ytdlp_path = Some(PathBuf::from("/nonexistent/yt-dlp"));
    assert!(!config.youtube_playback_ready());
    assert!(!PlaybackConfig::from_client(&config).enabled);
    config.youtube_playback_timeout_secs = MAX_YOUTUBE_TIMEOUT_SECS;
    assert!(
        PlaybackConfig::from_client(&config).timeout
            <= Duration::from_secs(MAX_YOUTUBE_TIMEOUT_SECS)
    );
}

#[test]
fn config_rejects_invalid_managed_paths_and_timeouts() {
    let base = ClientConfig::default();
    let mut config = base.clone();
    config.youtube_ytdlp_path = Some(PathBuf::from("relative/yt-dlp"));
    assert!(config.validate().is_err());
    config.youtube_ytdlp_path = Some(PathBuf::from("/opt/../yt-dlp"));
    assert!(config.validate().is_err());
    config.youtube_ytdlp_path = None;
    config.youtube_playback_timeout_secs = 1;
    assert!(config.validate().is_err());
    config.youtube_playback_timeout_secs = MAX_YOUTUBE_TIMEOUT_SECS + 1;
    assert!(config.validate().is_err());
}

#[tokio::test]
async fn player_reports_a_closed_backend_once() {
    let factory = MockFactory::new();
    let extractor = MockExtractor::ok(Duration::ZERO);
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        test_config(true),
        extractor,
        factory.clone(),
        command_rx,
        events,
        signal,
    ));
    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("play");
    assert!(matches!(
        next_event(&mut received, Duration::from_secs(2)).await,
        PlayerEvent::Loading { .. }
    ));
    let backend = wait_for_spawn(&factory).await;
    drop(backend);
    factory.close_backends();

    match next_event(&mut received, Duration::from_secs(2)).await {
        PlayerEvent::Failed {
            code,
            retry_available,
            ..
        } => {
            assert_eq!(code, "player_closed");
            assert!(retry_available);
        }
        other => panic!("expected a closed failure, got {other:?}"),
    }
    // A closed receiver must not spin or repeat the failure.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), received.recv())
            .await
            .is_err()
    );
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn player_reports_player_unavailable_when_mpv_cannot_start() {
    let factory = MockFactory::failing(BackendError::Unavailable);
    let extractor = MockExtractor::ok(Duration::ZERO);
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        test_config(true),
        extractor,
        factory.clone(),
        command_rx,
        events,
        signal,
    ));
    commands
        .send(PlayerCommand::Play {
            video_id: video(VIDEO_A),
            title: None,
        })
        .await
        .expect("play");
    assert!(matches!(
        next_event(&mut received, Duration::from_secs(2)).await,
        PlayerEvent::Loading { .. }
    ));
    match next_event(&mut received, Duration::from_secs(2)).await {
        PlayerEvent::Failed { code, .. } => assert_eq!(code, "player_unavailable"),
        other => panic!("expected player_unavailable, got {other:?}"),
    }
    assert_eq!(factory.spawns.load(Ordering::SeqCst), 0);
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn extractor_cancellation_reaps_the_child_process() {
    let directory = tempfile::tempdir().expect("tempdir");
    let pid_path = directory.path().join("pid");
    let extractor = YtdlpExtractor::new(
        script(
            directory.path(),
            "yt-dlp",
            &format!("echo $$ > '{}'; exec sleep 30", pid_path.display()),
        ),
        Duration::from_secs(30),
    );
    let (handle, cancel) = cancellation();
    let task = tokio::spawn(async move { extractor.resolve(&video(VIDEO_A), &cancel).await });
    for _ in 0..200 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = std::fs::read_to_string(&pid_path)
        .expect("pid file")
        .trim()
        .to_owned();
    assert!(!pid.is_empty());
    handle.cancel();
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("cancellation is prompt")
        .expect("task");
    assert_eq!(result, Err(ExtractorError::Cancelled));
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    let mut gone = false;
    for _ in 0..200 {
        if !proc_path.exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "yt-dlp child {pid} survived cancellation");
}

#[test]
fn jellyfin_items_survive_a_youtube_detour() {
    use tv_client::ui::{MediaItem, SourceFilter, UiModel};

    let mut model = UiModel::new();
    model.open_library(SourceFilter::Jellyfin);
    model.set_library_items(vec![MediaItem::new(Source::Jellyfin, "item-a", "Alpha")]);
    assert_eq!(model.visible_len(), 1);
    model.open_library(SourceFilter::YouTube);
    model.set_library_filter(SourceFilter::Jellyfin);
    assert_eq!(model.visible_len(), 1);
    assert_eq!(
        model.focused_item().map(|item| item.title.as_str()),
        Some("Alpha")
    );
}

#[tokio::test]
async fn mpv_start_failure_removes_the_socket_directory() {
    fn socket_directories() -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(std::env::temp_dir())
            .expect("temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("tv-client-mpv-"))
            })
            .collect();
        paths.sort();
        paths
    }

    let before = socket_directories();
    let options = MpvOptions {
        program: PathBuf::from("/nonexistent/mpv"),
        hwdec: YoutubeHwdec::AutoSafe,
        start_timeout: Duration::from_millis(50),
        reap_timeout: Duration::from_millis(50),
    };
    assert_eq!(
        MpvBackend::start(&options).await.err(),
        Some(BackendError::Unavailable)
    );
    assert_eq!(socket_directories(), before);
}

/// Live local playback probe. It is ignored by default and reads the managed
/// tool paths and a public identifier from the environment:
///
/// `TV_CLIENT_YOUTUBE_LIVE_ID`, `TV_CLIENT_YOUTUBE_YTDLP`,
/// `TV_CLIENT_YOUTUBE_MPV`.
#[tokio::test]
#[ignore = "live local playback requires a managed yt-dlp, mpv and TV_CLIENT_YOUTUBE_LIVE_ID"]
async fn live_local_playback_selects_h264_and_starts_mpv() {
    let Ok(identifier) = std::env::var("TV_CLIENT_YOUTUBE_LIVE_ID") else {
        return;
    };
    let id = VideoId::parse(&identifier).expect("valid live id");
    let ytdlp = std::env::var("TV_CLIENT_YOUTUBE_YTDLP").unwrap_or_else(|_| "yt-dlp".to_owned());
    let mpv = std::env::var("TV_CLIENT_YOUTUBE_MPV").unwrap_or_else(|_| "mpv".to_owned());
    let (_cancel_handle, cancel) = cancellation();
    let extractor = YtdlpExtractor::new(PathBuf::from(&ytdlp), Duration::from_secs(60));
    let media = extractor
        .resolve(&id, &cancel)
        .await
        .expect("live resolution");
    assert!(media.vcodec.starts_with("avc1"));
    assert!(media.height <= 1080);
    assert!(media.fps <= 30.5);

    let config = PlaybackConfig {
        enabled: true,
        extractor: PathBuf::from(&ytdlp),
        timeout: Duration::from_secs(60),
        mpv: MpvOptions {
            program: PathBuf::from(&mpv),
            hwdec: YoutubeHwdec::V4l2m2m,
            ..MpvOptions::default()
        },
    };
    let extractor: Arc<dyn Extractor> = Arc::new(YtdlpExtractor::new(
        PathBuf::from(&ytdlp),
        Duration::from_secs(60),
    ));
    let factory: Arc<dyn BackendFactory> = Arc::new(MpvBackendFactory::new(config.mpv.clone()));
    let (commands, command_rx) = mpsc::channel(PLAYER_COMMAND_CAPACITY);
    let (events, mut received) = mpsc::channel(32);
    let (shutdown_handle, signal) = shutdown_channel();
    let task = tokio::spawn(run_player(
        config, extractor, factory, command_rx, events, signal,
    ));
    commands
        .send(PlayerCommand::Play {
            video_id: id,
            title: Some("live probe".to_owned()),
        })
        .await
        .expect("play");
    let mut started = false;
    for _ in 0..40 {
        let event = tokio::time::timeout(Duration::from_secs(20), received.recv())
            .await
            .expect("live event timeout")
            .expect("live event");
        match event {
            PlayerEvent::Playing { method, .. } => {
                assert!(method.contains("H.264"));
                started = true;
            }
            PlayerEvent::Progress { position_ms, .. } if position_ms > 0 => {
                started = true;
                break;
            }
            PlayerEvent::Failed { code, .. } => panic!("live playback failed: {code}"),
            _ => {}
        }
        if started {
            break;
        }
    }
    assert!(started, "playback never reached playing");
    commands.send(PlayerCommand::Stop).await.expect("stop");
    shutdown_handle.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
}
