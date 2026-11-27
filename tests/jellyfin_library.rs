//! Offline mock-server tests for the Jellyfin client.
//!
//! The live test at the bottom is `#[ignore]` and requires explicit
//! `JELLYFIN_URL`, `JELLYFIN_USER` and `JELLYFIN_PASS` environment variables.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use tv_client::config::ClientConfig;
use tv_client::jellyfin::{
    ArtworkClass, BrowseItem, CancelToken, ImageRequest, ImageType, ItemQuery, JellyfinClient,
    JellyfinError, LibraryEntry, Page, PreviewKind, QuickConnectPoll, Timeouts, cancellation,
};
use tv_client::storage::{HARD_ARTWORK_CACHE_BYTES, Storage, StorageError};

const PASSWORD: &str = "s3cret-password-value";
const ACCESS_TOKEN: &str = "token-abc-123";
const DEVICE_NAME: &str = "test-tv";

#[derive(Clone)]
struct MockRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl MockRequest {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
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
}

struct MockResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl MockResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: serde_json::to_vec(&body).expect("JSON body"),
            delay: Duration::ZERO,
        }
    }

    fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body,
            delay: Duration::ZERO,
        }
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
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
                            if !response.delay.is_zero() {
                                thread::sleep(response.delay);
                            }
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

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
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
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    }
}

fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("temporary state directory")
}

fn open_storage(directory: &TempDir) -> Storage {
    Storage::open(directory.path(), HARD_ARTWORK_CACHE_BYTES).expect("storage opens")
}

fn client_config(url: &str) -> ClientConfig {
    ClientConfig {
        jellyfin_url: Some(url.to_owned()),
        ..ClientConfig::default()
    }
}

fn make_client(url: &str, storage: &mut Storage) -> JellyfinClient {
    JellyfinClient::new(&client_config(url), DEVICE_NAME, storage).expect("client builds")
}

fn auth_body(user_id: &str, name: &str, token: &str) -> Value {
    json!({
        "User": { "Id": user_id, "Name": name },
        "AccessToken": token,
        "ServerId": "server-1"
    })
}

fn is_authentication(request: &MockRequest) -> bool {
    request.path() == "/Users/AuthenticateByName" && request.method == "POST"
}

fn auth_response() -> MockResponse {
    MockResponse::json(200, auth_body("user-1", "tester", ACCESS_TOKEN))
}

fn device_id_from_header(value: &str) -> String {
    value
        .split("DeviceId=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("device id in authorization header")
        .to_owned()
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

fn recursive_files(directory: &std::path::Path, output: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory)
        .expect("read state directory")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            recursive_files(&path, output);
        } else {
            output.push(path);
        }
    }
}

async fn login(client: &mut JellyfinClient, storage: &mut Storage, cancel: &CancelToken) {
    let session = client
        .authenticate_by_name(storage, "tester", PASSWORD, cancel)
        .await
        .expect("authentication succeeds");
    assert_eq!(session.user_id(), "user-1");
    assert_eq!(session.user_name(), Some("tester"));
}

#[test]
fn missing_jellyfin_url_is_rejected() {
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let error = JellyfinClient::new(&ClientConfig::default(), DEVICE_NAME, &mut storage)
        .expect_err("missing URL must fail");
    assert!(matches!(error, JellyfinError::NotConfigured));
}

#[tokio::test]
async fn authorization_header_is_mediabrowser_with_stable_device_id() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Users/Me" => MockResponse::json(200, json!({"Id": "user-1", "Name": "tester"})),
        "/Users/user-1/Views" => {
            MockResponse::json(200, json!({"Items": [], "TotalRecordCount": 0}))
        }
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;
    client.views(&cancel).await.expect("views");

    let requests = server.requests();
    let authentication = requests
        .iter()
        .find(|request| is_authentication(request))
        .expect("authentication request");
    let initial = authentication
        .header("authorization")
        .expect("authorization header");
    assert!(
        initial.starts_with("MediaBrowser Client=\"tv-client\", Device=\"test-tv\", DeviceId=\"")
    );
    assert!(initial.contains(&format!(", Version=\"{}\"", env!("CARGO_PKG_VERSION"))));
    assert!(
        !initial.contains("Token="),
        "unauthenticated header: {initial}"
    );
    let device_id = device_id_from_header(initial);

    let views = requests
        .iter()
        .find(|request| request.path() == "/Users/user-1/Views")
        .expect("views request");
    let authenticated = views.header("authorization").expect("auth header");
    assert!(authenticated.contains(&format!("DeviceId=\"{device_id}\"")));
    assert!(authenticated.contains(&format!("Token=\"{ACCESS_TOKEN}\"")));

    let mut second = make_client(&server.url, &mut storage);
    assert!(
        second
            .restore_session(&mut storage, &cancel)
            .await
            .expect("restore")
    );
    second.views(&cancel).await.expect("views again");
    let later = server
        .requests()
        .into_iter()
        .rev()
        .find(|request| request.path() == "/Users/user-1/Views")
        .expect("later views request");
    let reused = later.header("authorization").expect("auth header");
    assert_eq!(device_id_from_header(reused), device_id);
    assert!(reused.contains(&format!("Token=\"{ACCESS_TOKEN}\"")));
}

#[tokio::test]
async fn base_path_prefix_joins_without_duplicate_slash() {
    let server = MockServer::start(|request| match request.path() {
        "/jellyfin/Users/AuthenticateByName" => auth_response(),
        "/jellyfin/Users/user-1/Views" => {
            MockResponse::json(200, json!({"Items": [], "TotalRecordCount": 0}))
        }
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let url = format!("{}/jellyfin", server.url);
    let mut client = make_client(&url, &mut storage);

    login(&mut client, &mut storage, &cancel).await;
    client.views(&cancel).await.expect("views");

    let paths: Vec<String> = server
        .requests()
        .into_iter()
        .map(|request| request.path().to_owned())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/jellyfin/Users/AuthenticateByName".to_owned(),
            "/jellyfin/Users/user-1/Views".to_owned(),
        ]
    );
    assert!(paths.iter().all(|path| !path.contains("//")), "{paths:?}");
}

#[tokio::test]
async fn quick_connect_lifecycle_polls_then_authenticates() {
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_counter = Arc::clone(&polls);
    let server = MockServer::start(move |request| match request.path() {
        "/QuickConnect/Initiate" => {
            MockResponse::json(200, json!({"Secret": "quick-secret", "Code": "123456"}))
        }
        "/QuickConnect/Connect" => {
            let count = poll_counter.fetch_add(1, Ordering::SeqCst);
            MockResponse::json(
                200,
                json!({"Authenticated": count >= 1, "Secret": "quick-secret"}),
            )
        }
        "/Users/AuthenticateWithQuickConnect" => auth_response(),
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);

    let initiation = client
        .quick_connect_initiate(&cancel)
        .await
        .expect("initiate");
    assert_eq!(initiation.code(), "123456");
    assert!(!initiation.is_expired());
    assert_eq!(
        client
            .quick_connect_poll(&initiation, &cancel)
            .await
            .unwrap(),
        QuickConnectPoll::Pending
    );
    assert_eq!(
        client
            .quick_connect_poll(&initiation, &cancel)
            .await
            .unwrap(),
        QuickConnectPoll::Approved
    );
    let session = client
        .quick_connect_authenticate(&mut storage, &initiation, &cancel)
        .await
        .expect("quick connect authentication");
    assert_eq!(session.user_id(), "user-1");
    assert_eq!(
        storage
            .read_credential("jellyfin_access_token")
            .unwrap()
            .as_slice(),
        ACCESS_TOKEN.as_bytes()
    );

    let requests = server.requests();
    let connect = requests
        .iter()
        .find(|request| request.path() == "/QuickConnect/Connect")
        .expect("connect request");
    assert_eq!(
        connect.query().get("secret").map(String::as_str),
        Some("quick-secret")
    );
    let exchange = requests
        .iter()
        .find(|request| request.path() == "/Users/AuthenticateWithQuickConnect")
        .expect("exchange request");
    let body = String::from_utf8(exchange.body.clone()).unwrap();
    assert!(body.contains("\"Secret\":\"quick-secret\""));
    assert!(!body.contains(PASSWORD));
}

#[tokio::test]
async fn quick_connect_expiry_is_reported() {
    let server = MockServer::start(|request| match request.path() {
        "/QuickConnect/Initiate" => {
            MockResponse::json(200, json!({"Secret": "quick-secret", "Code": "654321"}))
        }
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    client
        .set_quick_connect_ttl(Duration::from_millis(10))
        .expect("ttl");
    let initiation = client
        .quick_connect_initiate(&cancel)
        .await
        .expect("initiate");
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(initiation.is_expired());
    let error = client
        .quick_connect_poll(&initiation, &cancel)
        .await
        .expect_err("expired initiation");
    assert!(matches!(error, JellyfinError::QuickConnectExpired));
}

#[tokio::test]
async fn manual_authentication_never_persists_the_password() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Users/user-1/Views" => {
            MockResponse::json(200, json!({"Items": [], "TotalRecordCount": 0}))
        }
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;
    client.views(&cancel).await.expect("views");

    let mut files = Vec::new();
    recursive_files(directory.path(), &mut files);
    assert!(!files.is_empty(), "credential and database files exist");
    for file in files {
        let bytes = fs::read(&file).expect("read state file");
        assert!(
            !contains(&bytes, PASSWORD.as_bytes()),
            "password persisted in {:?}",
            file.file_name()
        );
    }
    let credential = storage
        .read_credential("jellyfin_access_token")
        .expect("token stored");
    assert_eq!(credential.as_slice(), ACCESS_TOKEN.as_bytes());
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

#[tokio::test]
async fn pagination_sets_start_index_and_limit() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Users/user-1/Items" => MockResponse::json(
            200,
            json!({
                "Items": [{"Id": "item-1", "Name": "One", "Type": "Movie"}],
                "TotalRecordCount": 51,
                "StartIndex": 100
            }),
        ),
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let page = Page::new(100, 50).expect("valid page");
    let result = client
        .items(&ItemQuery::new().page(page), &cancel)
        .await
        .expect("items");
    assert_eq!(result.total_record_count, 51);
    assert_eq!(result.items.len(), 1);
    let query = server
        .requests()
        .into_iter()
        .find(|request| request.path() == "/Users/user-1/Items")
        .expect("items request")
        .query();
    assert_eq!(query.get("StartIndex").map(String::as_str), Some("100"));
    assert_eq!(query.get("Limit").map(String::as_str), Some("50"));
    assert_eq!(query.get("Recursive").map(String::as_str), Some("true"));

    assert!(matches!(
        Page::new(0, 0),
        Err(JellyfinError::InvalidRequest)
    ));
    assert_eq!(page.next().start_index(), 150);
    assert_eq!(Page::default().limit(), 50);
}

#[tokio::test]
async fn browse_query_is_parent_scoped_fielded_and_bounded() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Users/user-1/Views" => MockResponse::json(
            200,
            json!({
                "Items": [
                    {"Id": "library-1", "Name": "Movies", "CollectionType": "movies"},
                    {"Id": "library-2", "Name": "TV Shows", "CollectionType": "tvshows"}
                ],
                "TotalRecordCount": 2
            }),
        ),
        "/Users/user-1/Items" => MockResponse::json(
            200,
            json!({
                "Items": [{
                    "Id": "movie-1",
                    "Name": "Dune",
                    "Type": "Movie",
                    "ProductionYear": 2021,
                    "RunTimeTicks": 67_200_000_000u64,
                    "Overview": "A desert epic",
                    "ImageTags": {"Primary": "primary-tag"},
                    "UserData": {"Played": false, "PlaybackPositionTicks": 0}
                }],
                "TotalRecordCount": 49,
                "StartIndex": 24
            }),
        ),
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let libraries = client.libraries(&cancel).await.expect("libraries");
    assert_eq!(libraries.len(), 2);
    assert_eq!(
        libraries[0],
        LibraryEntry {
            id: "library-1".to_owned(),
            name: "Movies".to_owned(),
            collection_type: Some("movies".to_owned()),
        }
    );

    let page = client
        .browse("library-1", Some("movies"), 24, &cancel)
        .await
        .expect("browse page");
    assert_eq!(page.total_record_count, 49);
    let item = BrowseItem::from_base_item(&page.items[0]);
    assert_eq!(item.id, "movie-1");
    assert_eq!(item.title, "Dune");
    assert_eq!(item.item_type.as_deref(), Some("Movie"));
    assert_eq!(item.primary_tag.as_deref(), Some("primary-tag"));
    assert_eq!(item.overview.as_deref(), Some("A desert epic"));
    assert_eq!(item.subtitle, "Movie · 2021 · 1h 52m");

    let requests = server.requests();
    let query = requests
        .iter()
        .find(|request| request.path() == "/Users/user-1/Items")
        .expect("browse request")
        .query();
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
    assert_eq!(query.get("Recursive").map(String::as_str), Some("true"));
    assert!(
        requests
            .iter()
            .all(|request| !request.target.contains("http")),
        "requests stay on the configured path"
    );
}

#[test]
fn browse_queries_map_each_library_collection_type() {
    let cases: &[(Option<&str>, &[&str], &[&str])] = &[
        (Some("movies"), &["Movie", "Episode", "Video"], &["Video"]),
        (Some("tvshows"), &["Movie", "Episode", "Video"], &["Video"]),
        (None, &["Movie", "Episode", "Video"], &["Video"]),
        (Some("musicvideos"), &["MusicVideo"], &["Video"]),
        (Some("MUSIC"), &["Audio", "MusicAlbum"], &[]),
        (Some("audiobooks"), &["AudioBook"], &["Audio"]),
        (Some("books"), &["Book"], &[]),
        // Unknown collection types keep the video default rather than
        // widening the query.
        (
            Some("playlists"),
            &["Movie", "Episode", "Video"],
            &["Video"],
        ),
    ];

    for (collection_type, item_types, media_types) in cases {
        let query = ItemQuery::browse("library-1", *collection_type, 24).expect("query");
        assert_eq!(
            query
                .include_item_types
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            item_types.to_vec(),
            "IncludeItemTypes for {collection_type:?}"
        );
        assert_eq!(
            query
                .media_types
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            media_types.to_vec(),
            "MediaTypes for {collection_type:?}"
        );
        assert_eq!(query.parent_id.as_deref(), Some("library-1"));
        assert_eq!(query.page.start_index(), 24);
        assert_eq!(query.page.limit(), 24);
        assert_eq!(
            query.fields,
            ["UserData", "ImageTags", "Overview"]
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn preview_kind_separates_video_from_planned_audio_and_books() {
    let kind = |item_type: Option<&str>| {
        BrowseItem {
            item_type: item_type.map(str::to_owned),
            ..BrowseItem::default()
        }
        .preview_kind()
    };
    for video in ["Movie", "Episode", "Video", "MusicVideo"] {
        assert_eq!(kind(Some(video)), PreviewKind::Video, "{video}");
        assert!(kind(Some(video)).is_playable(), "{video}");
    }
    for audio in ["Audio", "MusicAlbum", "AudioBook"] {
        assert_eq!(kind(Some(audio)), PreviewKind::AudioPlanned, "{audio}");
        assert!(!kind(Some(audio)).is_playable(), "{audio}");
    }
    assert_eq!(kind(Some("Book")), PreviewKind::BookPlanned);
    assert_eq!(kind(None), PreviewKind::Unsupported);
    assert!(!PreviewKind::BookPlanned.is_playable());
}

#[tokio::test]
async fn music_library_browse_requests_audio_and_albums_without_a_video_filter() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Users/user-1/Items" => MockResponse::json(
            200,
            json!({
                "Items": [
                    {"Id": "album-1", "Name": "Discovery", "Type": "MusicAlbum"},
                    {"Id": "track-1", "Name": "One More Time", "Type": "Audio"}
                ],
                "TotalRecordCount": 2,
                "StartIndex": 0
            }),
        ),
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let page = client
        .browse("library-1", Some("music"), 0, &cancel)
        .await
        .expect("music page");
    assert_eq!(
        page.items
            .iter()
            .map(BrowseItem::from_base_item)
            .map(|item| item.preview_kind())
            .collect::<Vec<_>>(),
        [PreviewKind::AudioPlanned, PreviewKind::AudioPlanned].to_vec()
    );

    let requests = server.requests();
    let query = requests
        .iter()
        .find(|request| request.path() == "/Users/user-1/Items")
        .expect("music request")
        .query();
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
        Some("Audio,MusicAlbum")
    );
    assert!(
        !query.contains_key("MediaTypes"),
        "music libraries must not carry the video media filter"
    );
    assert_eq!(query.get("Limit").map(String::as_str), Some("24"));
}

#[tokio::test]
async fn missing_artwork_is_a_typed_outcome_not_a_page_failure() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Items/item-1/Images/Primary" => MockResponse::empty(404),
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let request = ImageRequest::poster("item-1", Some("tag-1")).expect("poster");
    let error = client
        .image(&mut storage, &request, &cancel)
        .await
        .expect_err("missing artwork");
    assert!(matches!(error, JellyfinError::ArtworkMissing));
    assert_eq!(storage.artwork_total_bytes().unwrap(), 0);
}

#[tokio::test]
async fn poster_and_preview_do_not_share_a_cache_entry() {
    let image_requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&image_requests);
    let server = MockServer::start(move |request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Items/item-1/Images/Primary" => {
            counter.fetch_add(1, Ordering::SeqCst);
            match request.query().get("maxWidth").map(String::as_str) {
                Some("320") => MockResponse::bytes(200, "image/png", png(320, 480)),
                Some("1280") => MockResponse::bytes(200, "image/png", png(1280, 720)),
                _ => MockResponse::empty(400),
            }
        }
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let poster_request = ImageRequest::poster("item-1", Some("tag-1")).expect("poster");
    assert_eq!(poster_request.class(), ArtworkClass::Poster);
    assert_eq!(
        (poster_request.max_width(), poster_request.max_height()),
        (320, 480)
    );
    let poster = client
        .image(&mut storage, &poster_request, &cancel)
        .await
        .expect("poster");
    let preview_request = ImageRequest::preview("item-1", Some("tag-1")).expect("preview");
    assert_eq!(preview_request.class(), ArtworkClass::Preview);
    let preview = client
        .image(&mut storage, &preview_request, &cancel)
        .await
        .expect("preview");

    assert_ne!(
        poster.path(),
        preview.path(),
        "poster and preview must not share a cache entry"
    );
    assert_eq!((poster.width(), poster.height()), (320, 480));
    assert_eq!((preview.width(), preview.height()), (1280, 720));
    assert_eq!(image_requests.load(Ordering::SeqCst), 2);

    let cached = client
        .image(&mut storage, &poster_request, &cancel)
        .await
        .expect("cached poster");
    assert_eq!(cached.path(), poster.path());
    assert_eq!(image_requests.load(Ordering::SeqCst), 2, "cache hit");

    let images: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.path() == "/Items/item-1/Images/Primary")
        .collect();
    assert_eq!(images.len(), 2);
    for request in &images {
        let query = request.query();
        assert_eq!(query.get("tag").map(String::as_str), Some("tag-1"));
        assert_eq!(query.get("format").map(String::as_str), Some("jpg"));
    }
    assert_eq!(
        images[0].query().get("maxWidth").map(String::as_str),
        Some("320")
    );
    assert_eq!(
        images[1].query().get("maxWidth").map(String::as_str),
        Some("1280")
    );
}

#[tokio::test]
async fn per_class_artwork_caps_reject_oversized_payloads() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => auth_response(),
        "/Items/wide/Images/Primary" => MockResponse::bytes(200, "image/png", png(321, 480)),
        "/Items/tall/Images/Primary" => MockResponse::bytes(200, "image/png", png(1280, 721)),
        "/Items/big/Images/Primary" => {
            MockResponse::bytes(200, "image/png", vec![0_u8; 2 * 1024 * 1024 + 1])
        }
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let wide = ImageRequest::poster("wide", Some("tag")).expect("poster");
    assert!(matches!(
        client.image(&mut storage, &wide, &cancel).await,
        Err(JellyfinError::ImageTooLarge {
            width: 321,
            height: 480
        })
    ));
    let tall = ImageRequest::preview("tall", Some("tag")).expect("preview");
    assert!(matches!(
        client.image(&mut storage, &tall, &cancel).await,
        Err(JellyfinError::ImageTooLarge {
            width: 1280,
            height: 721
        })
    ));
    let big = ImageRequest::poster("big", Some("tag")).expect("poster");
    assert!(matches!(
        client.image(&mut storage, &big, &cancel).await,
        Err(JellyfinError::ArtworkTooLarge)
    ));
    assert_eq!(storage.artwork_total_bytes().unwrap(), 0);
}

#[tokio::test]
async fn artwork_uses_the_configured_base_path_and_format() {
    let server = MockServer::start(|request| match request.path() {
        "/jellyfin/Users/AuthenticateByName" => auth_response(),
        "/jellyfin/Items/item-1/Images/Primary" => {
            MockResponse::bytes(200, "image/png", png(320, 480))
        }
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let url = format!("{}/jellyfin", server.url);
    let mut client = make_client(&url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let request = ImageRequest::poster("item-1", Some("tag-1")).expect("poster");
    let artwork = client
        .image(&mut storage, &request, &cancel)
        .await
        .expect("artwork");
    assert_eq!((artwork.width(), artwork.height()), (320, 480));

    let image = server
        .requests()
        .into_iter()
        .find(|request| request.path() == "/jellyfin/Items/item-1/Images/Primary")
        .expect("base-path image request");
    assert!(!image.target.contains("http"), "{}", image.target);
    let query = image.query();
    assert_eq!(query.get("maxWidth").map(String::as_str), Some("320"));
    assert_eq!(query.get("maxHeight").map(String::as_str), Some("480"));
    assert_eq!(query.get("format").map(String::as_str), Some("jpg"));
}

#[test]
fn browse_models_never_expose_urls_paths_or_tokens() {
    let base: tv_client::jellyfin::BaseItem = serde_json::from_value(json!({
        "Id": "item-1",
        "Name": "Dune",
        "Type": "Movie",
        "ProductionYear": 2021,
        "Overview": "A desert epic",
        "ImageTags": {"Primary": "primary-tag"},
        "Path": "/srv/media/secret/dune.mkv",
        "MediaSources": [{"TranscodingUrl": "http://secret.invalid/stream?Token=leak"}]
    }))
    .expect("base item");
    let browse = BrowseItem::from_base_item(&base);
    let library = LibraryEntry::from_base_item(&base);
    let debug = format!("{browse:?} {library:?}");
    assert!(!debug.contains("http"));
    assert!(!debug.contains("/srv/media"));
    assert!(!debug.contains("Token"));
    assert!(!debug.contains("secret"));
    assert_eq!(browse.primary_tag.as_deref(), Some("primary-tag"));
}

#[tokio::test]
async fn library_endpoints_and_typed_user_data_are_wired() {
    let server = MockServer::start(|request| match (request.method.as_str(), request.path()) {
        (_, "/Users/AuthenticateByName") => auth_response(),
        (_, "/Users/user-1/Views") => MockResponse::json(
            200,
            json!({"Items": [{"Id": "views-1", "Name": "Movies", "CollectionType": "movies"}], "TotalRecordCount": 1}),
        ),
        (_, "/Users/user-1/Items") => {
            MockResponse::json(200, json!({"Items": [], "TotalRecordCount": 0}))
        }
        (_, "/Users/user-1/Items/Latest") => MockResponse::json(
            200,
            json!([{"Id": "latest-1", "Name": "New", "Type": "Episode"}]),
        ),
        (_, "/Shows/series-1/Seasons") => MockResponse::json(
            200,
            json!({"Items": [{"Id": "season-1", "Name": "Season 1", "IndexNumber": 1}], "TotalRecordCount": 1}),
        ),
        (_, "/Shows/series-1/Episodes") => MockResponse::json(
            200,
            json!({"Items": [{"Id": "episode-1", "Name": "Pilot", "SeriesId": "series-1"}], "TotalRecordCount": 1}),
        ),
        (_, "/Users/user-1/Items/item-1/UserData") => {
            if request.method == "POST" {
                MockResponse::json(
                    200,
                    json!({"Played": true, "PlaybackPositionTicks": 0, "PlayCount": 1}),
                )
            } else {
                MockResponse::json(
                    200,
                    json!({"Played": false, "PlaybackPositionTicks": 500, "PlayCount": 0}),
                )
            }
        }
        (_, "/Items/item-1/PlaybackInfo") => MockResponse::json(
            200,
            json!({
                "PlaySessionId": "play-1",
                "MediaSources": [{
                    "Id": "source-1",
                    "Container": "mkv",
                    "SupportsDirectPlay": true,
                    "TranscodingUrl": "http://example.invalid/never-modeled",
                    "MediaStreams": [{"Type": "Video", "Codec": "h264", "Width": 1920, "Height": 1080}]
                }]
            }),
        ),
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let views = client.views(&cancel).await.expect("views");
    assert_eq!(views.items[0].collection_type.as_deref(), Some("movies"));

    client.resume(Page::first(), &cancel).await.expect("resume");
    client
        .latest(Some("parent-1"), 10, &cancel)
        .await
        .expect("latest");
    client.seasons("series-1", &cancel).await.expect("seasons");
    client
        .episodes("series-1", Some("season-1"), Page::first(), &cancel)
        .await
        .expect("episodes");
    client
        .music(None, Page::first(), &cancel)
        .await
        .expect("music");
    client
        .search("dune", Page::new(50, 50).unwrap(), &cancel)
        .await
        .expect("search");
    let user_data = client
        .user_data("item-1", &cancel)
        .await
        .expect("user data");
    assert_eq!(user_data.playback_position_ticks, 500);
    let updated = client
        .update_user_data(
            "item-1",
            &tv_client::jellyfin::UserDataUpdate {
                played: Some(true),
                ..Default::default()
            },
            &cancel,
        )
        .await
        .expect("update user data");
    assert!(updated.played);
    let playback = client
        .playback_info("item-1", &cancel)
        .await
        .expect("playback info");
    assert_eq!(playback.play_session_id.as_deref(), Some("play-1"));
    assert_eq!(playback.media_sources[0].id.as_deref(), Some("source-1"));
    assert!(playback.media_sources[0].supports_direct_play);
    assert!(
        !format!("{playback:?}").contains("example.invalid"),
        "stream URLs must not be modeled: {playback:?}"
    );

    let requests = server.requests();
    let resume = requests
        .iter()
        .find(|request| request.path() == "/Users/user-1/Items")
        .expect("resume request")
        .query();
    assert_eq!(
        resume.get("Filters").map(String::as_str),
        Some("IsResumable")
    );
    assert_eq!(resume.get("MediaTypes").map(String::as_str), Some("Video"));
    assert_eq!(resume.get("StartIndex").map(String::as_str), Some("0"));
    assert_eq!(resume.get("Limit").map(String::as_str), Some("50"));
    let latest = requests
        .iter()
        .find(|request| request.path() == "/Users/user-1/Items/Latest")
        .expect("latest request")
        .query();
    assert_eq!(latest.get("ParentId").map(String::as_str), Some("parent-1"));
    assert_eq!(latest.get("Limit").map(String::as_str), Some("10"));
    let seasons = requests
        .iter()
        .find(|request| request.path() == "/Shows/series-1/Seasons")
        .expect("seasons request")
        .query();
    assert_eq!(seasons.get("userId").map(String::as_str), Some("user-1"));
    let episodes = requests
        .iter()
        .find(|request| request.path() == "/Shows/series-1/Episodes")
        .expect("episodes request")
        .query();
    assert_eq!(
        episodes.get("SeasonId").map(String::as_str),
        Some("season-1")
    );
    assert_eq!(episodes.get("StartIndex").map(String::as_str), Some("0"));
    let music = requests
        .iter()
        .find(|request| {
            request.path() == "/Users/user-1/Items"
                && request.query().contains_key("IncludeItemTypes")
        })
        .expect("music request")
        .query();
    assert_eq!(
        music.get("IncludeItemTypes").map(String::as_str),
        Some("Audio")
    );
    let search = requests
        .iter()
        .filter(|request| request.path() == "/Users/user-1/Items")
        .find(|request| request.query().contains_key("SearchTerm"))
        .expect("search request")
        .query();
    assert_eq!(search.get("SearchTerm").map(String::as_str), Some("dune"));
    assert_eq!(search.get("StartIndex").map(String::as_str), Some("50"));
    let update = requests
        .iter()
        .find(|request| request.method == "POST" && request.path().ends_with("/UserData"))
        .expect("user-data update");
    assert!(String::from_utf8_lossy(&update.body).contains("\"Played\":true"));
    let playback_request = requests
        .iter()
        .find(|request| request.path() == "/Items/item-1/PlaybackInfo")
        .expect("playback request");
    assert_eq!(
        playback_request.query().get("UserId").map(String::as_str),
        Some("user-1")
    );
}

#[tokio::test]
async fn status_codes_map_to_typed_errors() {
    for (status, retry_after) in [(401, None), (403, None), (429, Some("7")), (500, None)] {
        let server = MockServer::start(move |request| {
            if is_authentication(request) {
                auth_response()
            } else if request.path() == "/Users/user-1/Views" {
                let mut response = MockResponse::empty(status);
                if let Some(retry_after) = retry_after {
                    response
                        .headers
                        .push(("Retry-After".to_owned(), retry_after.to_owned()));
                }
                response
            } else {
                MockResponse::empty(404)
            }
        });
        let directory = temp_dir();
        let mut storage = open_storage(&directory);
        let (_cancel, cancel) = cancellation();
        let mut client = make_client(&server.url, &mut storage);
        login(&mut client, &mut storage, &cancel).await;
        let error = client.views(&cancel).await.expect_err("status must fail");
        match status {
            401 => assert!(matches!(error, JellyfinError::Unauthorized)),
            403 => assert!(matches!(error, JellyfinError::Forbidden)),
            429 => assert!(matches!(
                error,
                JellyfinError::RateLimited {
                    retry_after_secs: Some(7)
                }
            )),
            _ => assert!(matches!(error, JellyfinError::Http { status: 500 })),
        }
    }
}

#[tokio::test]
async fn redirects_are_rejected_without_being_followed() {
    let server = MockServer::start(|request| {
        if is_authentication(request) {
            auth_response()
        } else if request.path() == "/Users/user-1/Views" {
            MockResponse {
                status: 302,
                headers: vec![(
                    "Location".to_owned(),
                    "http://example.invalid/steal".to_owned(),
                )],
                body: Vec::new(),
                delay: Duration::ZERO,
            }
        } else {
            MockResponse::empty(404)
        }
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;
    let error = client.views(&cancel).await.expect_err("redirect rejected");
    assert!(matches!(error, JellyfinError::UnexpectedRedirect));
    assert_eq!(server.request_count(), 2, "no redirect target is contacted");
}

#[tokio::test]
async fn timeouts_are_typed() {
    let server = MockServer::start(|request| {
        if is_authentication(request) {
            auth_response()
        } else if request.path() == "/Users/user-1/Views" {
            MockResponse::json(200, json!({"Items": [], "TotalRecordCount": 0}))
                .delayed(Duration::from_millis(500))
        } else {
            MockResponse::empty(404)
        }
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let timeouts = Timeouts::new(
        Duration::from_millis(200),
        Duration::from_millis(80),
        Duration::from_millis(150),
    )
    .expect("timeouts");
    let mut client = JellyfinClient::with_timeouts(
        &client_config(&server.url),
        DEVICE_NAME,
        &mut storage,
        timeouts,
    )
    .expect("client");
    login(&mut client, &mut storage, &cancel).await;
    let error = client.views(&cancel).await.expect_err("timeout");
    assert!(matches!(error, JellyfinError::Timeout));
}

#[tokio::test]
async fn cancellation_interrupts_an_in_flight_request() {
    let server = MockServer::start(|request| {
        if is_authentication(request) {
            auth_response()
        } else if request.path() == "/Users/user-1/Views" {
            MockResponse::json(200, json!({"Items": [], "TotalRecordCount": 0}))
                .delayed(Duration::from_secs(2))
        } else {
            MockResponse::empty(404)
        }
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (handle, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let cancel_handle = handle.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel_handle.cancel();
    });
    let started = Instant::now();
    let error = client.views(&cancel).await.expect_err("cancelled");
    assert!(matches!(error, JellyfinError::Cancelled));
    assert!(started.elapsed() < Duration::from_millis(500));
    canceller.await.expect("canceller");
}

#[tokio::test]
async fn image_cache_hits_are_reused_and_logout_clears_credentials_and_artwork() {
    let image_requests = Arc::new(AtomicUsize::new(0));
    let image_counter = Arc::clone(&image_requests);
    let image_counter_for_server = Arc::clone(&image_requests);
    let server =
        MockServer::start(
            move |request| match (request.method.as_str(), request.path()) {
                (_, "/Users/AuthenticateByName") => auth_response(),
                (_, "/Users/Me") => {
                    MockResponse::json(200, json!({"Id": "user-1", "Name": "tester"}))
                }
                (_, "/Items/item-1/Images/Primary") => {
                    image_counter_for_server.fetch_add(1, Ordering::SeqCst);
                    MockResponse::bytes(200, "image/png", png(320, 180))
                }
                ("POST", "/Sessions/Logout") => MockResponse::empty(204),
                _ => MockResponse::empty(404),
            },
        );
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;

    let request = ImageRequest::with_tag("item-1", ImageType::Primary, Some("tag-1")).unwrap();
    let first = client
        .image(&mut storage, &request, &cancel)
        .await
        .expect("image");
    assert!(first.is_file());
    assert_eq!(fs::read(&first).unwrap(), png(320, 180));
    let second = client
        .image(&mut storage, &request, &cancel)
        .await
        .expect("cached image");
    assert_eq!(first, second);
    assert_eq!(
        image_counter.load(Ordering::SeqCst),
        1,
        "cache hit avoids a request"
    );

    client.logout(&mut storage, &cancel).await.expect("logout");
    let logout = server
        .requests()
        .into_iter()
        .find(|request| request.path() == "/Sessions/Logout")
        .expect("logout request");
    assert!(
        logout
            .header("authorization")
            .unwrap()
            .contains(&format!("Token=\"{ACCESS_TOKEN}\""))
    );
    assert!(matches!(
        storage.read_credential("jellyfin_access_token"),
        Err(StorageError::CredentialNotFound)
    ));
    let artwork = directory.path().join("artwork");
    let remaining = fs::read_dir(artwork)
        .expect("artwork directory")
        .filter_map(Result::ok)
        .count();
    assert_eq!(remaining, 0, "logout clears cached artwork files");

    let mut fresh = make_client(&server.url, &mut storage);
    assert!(
        !fresh
            .restore_session(&mut storage, &cancel)
            .await
            .expect("restore"),
        "logged-out token is gone"
    );
}

#[tokio::test]
async fn image_dimension_limit_rejects_before_storage() {
    let server = MockServer::start(|request| {
        if is_authentication(request) {
            auth_response()
        } else if request.path() == "/Items/item-1/Images/Primary" {
            MockResponse::bytes(200, "image/png", png(100_000, 100_000))
        } else {
            MockResponse::empty(404)
        }
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;
    let request = ImageRequest::new("item-1", ImageType::Primary).unwrap();
    let error = client
        .image(&mut storage, &request, &cancel)
        .await
        .expect_err("oversized dimensions");
    assert!(matches!(
        error,
        JellyfinError::ImageTooLarge {
            width: 100_000,
            height: 100_000
        }
    ));
    assert_eq!(storage.artwork_total_bytes().unwrap(), 0);
}

#[tokio::test]
async fn invalid_item_ids_are_rejected_before_any_request() {
    let server = MockServer::start(|request| {
        if is_authentication(request) {
            auth_response()
        } else {
            MockResponse::empty(404)
        }
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);
    login(&mut client, &mut storage, &cancel).await;
    let before = server.request_count();
    let error = client
        .user_data("http://example.invalid/stream", &cancel)
        .await
        .expect_err("URL-looking item id");
    assert!(matches!(error, JellyfinError::InvalidItemId));
    assert_eq!(server.request_count(), before);
}

#[tokio::test]
async fn errors_and_redacted_debug_output_never_contain_credentials() {
    let server = MockServer::start(|request| match request.path() {
        "/Users/AuthenticateByName" => MockResponse::empty(401),
        "/QuickConnect/Initiate" => MockResponse::json(
            200,
            json!({"Secret": "quick-secret-value", "Code": "123456"}),
        ),
        "/Users/AuthenticateWithQuickConnect" => MockResponse::empty(401),
        _ => MockResponse::empty(404),
    });
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let (_cancel, cancel) = cancellation();
    let mut client = make_client(&server.url, &mut storage);

    let error = client
        .authenticate_by_name(&mut storage, "tester", "password-never-logged", &cancel)
        .await
        .expect_err("authentication rejected");
    assert!(matches!(error, JellyfinError::AuthenticationRejected));
    let display = format!("{error}");
    let debug = format!("{error:?}");
    assert!(!display.contains("password-never-logged"));
    assert!(!debug.contains("password-never-logged"));
    assert!(!display.to_ascii_lowercase().contains("token"));
    assert!(!debug.to_ascii_lowercase().contains("token"));
    assert!(!display.contains(ACCESS_TOKEN));
    assert!(!debug.contains(ACCESS_TOKEN));

    let initiation = client
        .quick_connect_initiate(&cancel)
        .await
        .expect("initiate");
    let debug = format!("{initiation:?}");
    assert!(!debug.contains("quick-secret-value"));
    let error = client
        .quick_connect_authenticate(&mut storage, &initiation, &cancel)
        .await
        .expect_err("quick connect not approved");
    assert!(matches!(error, JellyfinError::QuickConnectNotApproved));
    assert!(!format!("{error}").contains("quick-secret-value"));
    assert!(!format!("{error:?}").contains("quick-secret-value"));
}

#[tokio::test]
#[ignore = "requires a live Jellyfin server and JELLYFIN_URL/JELLYFIN_USER/JELLYFIN_PASS"]
async fn live_jellyfin_authenticates_and_lists_views() {
    let (Ok(url), Ok(user), Ok(password)) = (
        std::env::var("JELLYFIN_URL"),
        std::env::var("JELLYFIN_USER"),
        std::env::var("JELLYFIN_PASS"),
    ) else {
        eprintln!("live test skipped: JELLYFIN_URL, JELLYFIN_USER and JELLYFIN_PASS are required");
        return;
    };
    let directory = temp_dir();
    let mut storage = open_storage(&directory);
    let config = client_config(&url);
    let mut client =
        JellyfinClient::new(&config, "tv-client-live-test", &mut storage).expect("live client");
    let (handle, cancel) = cancellation();
    let session = client
        .authenticate_by_name(&mut storage, &user, &password, &cancel)
        .await
        .expect("live authentication");
    assert!(!session.user_id().is_empty());
    let views = client.views(&cancel).await.expect("live views");
    assert!(
        !views.items.is_empty(),
        "server should expose at least one view"
    );
    client
        .logout(&mut storage, &cancel)
        .await
        .expect("live logout");
    handle.cancel();
}
