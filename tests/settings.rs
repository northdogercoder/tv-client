//! Settings persistence, editable UI model and Jellyfin Quick Connect tests.
//!
//! Everything here is offline. The Quick Connect lifecycle runs against a
//! local mock server; the live Pi login is an operator gate, not a test.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use tv_client::config::{
    self, ClientConfig, ConfigOrigin, MAX_UI_SAFE_AREA_PERCENT, MIN_UI_SAFE_AREA_PERCENT,
};
use tv_client::jellyfin::{
    ACCESS_TOKEN_CREDENTIAL, JellyfinClient, JellyfinError, QuickConnectPoll, cancellation,
};
use tv_client::settings::{
    JellyfinSignOutError, SettingRow, SettingsEvent, SettingsForm, save_state_config,
};
use tv_client::storage::{HARD_ARTWORK_CACHE_BYTES, Storage};
use tv_client::ui::{Key, Screen, UiModel, UiOutcome};

const ACCESS_TOKEN: &str = "token-quick-connect-secret";
const QUICK_CONNECT_SECRET: &str = "quick-connect-server-secret";
const DEVICE_NAME: &str = "settings-test-tv";

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("temporary directory")
}

fn open_storage(directory: &Path) -> Storage {
    Storage::open(directory, HARD_ARTWORK_CACHE_BYTES).expect("storage opens")
}

fn client_config(url: &str) -> ClientConfig {
    ClientConfig {
        jellyfin_url: Some(url.to_owned()),
        ..ClientConfig::default()
    }
}

fn write_config(path: &Path, fps: u32) {
    fs::write(
        path,
        format!("max_fps = {fps}\nui_safe_area_percent = 3\n",),
    )
    .expect("write test configuration");
}

fn press(model: &mut UiModel, key: Key, tick: u64) -> UiOutcome {
    model.handle_key(key, Duration::from_millis(tick))
}

// --- Configuration precedence and persistence -----------------------------

#[test]
fn config_precedence_is_explicit_then_state_then_system() {
    let temporary = temp_dir();
    let state = temporary.path().join("state");
    fs::create_dir_all(&state).expect("state directory");
    let system = temporary.path().join("system.toml");
    let explicit = temporary.path().join("explicit.toml");
    write_config(&system, 30);
    write_config(&explicit, 24);

    let selected = config::select_config_with(None, Some(&state), &system);
    assert_eq!(selected.origin, ConfigOrigin::System);
    assert_eq!(selected.load().expect("system config").max_fps, 30);

    let state_file = state.join("client.toml");
    write_config(&state_file, 25);
    let selected = config::select_config_with(None, Some(&state), &system);
    assert_eq!(selected.origin, ConfigOrigin::State);
    assert_eq!(selected.path.as_deref(), Some(state_file.as_path()));
    assert_eq!(selected.load().expect("state config").max_fps, 25);

    let selected = config::select_config_with(Some(&explicit), Some(&state), &system);
    assert_eq!(selected.origin, ConfigOrigin::Explicit);
    assert_eq!(selected.load().expect("explicit config").max_fps, 24);

    let missing = temporary.path().join("missing.toml");
    let selected = config::select_config_with(Some(&missing), Some(&state), &system);
    assert_eq!(selected.origin, ConfigOrigin::Explicit);
    assert!(
        selected.load().is_err(),
        "an explicit path must not fall back to another file"
    );
}

#[test]
fn state_config_is_private_atomic_and_validated_before_write() {
    let temporary = temp_dir();
    let state = temporary.path().join("state");
    fs::create_dir_all(&state).expect("state directory");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).expect("initial mode");
    let config = ClientConfig {
        ui_safe_area_percent: 5,
        youtube_playback_timeout_secs: 45,
        ..ClientConfig::default()
    };

    let path = save_state_config(&state, &config).expect("save succeeds");
    assert_eq!(path, state.join("client.toml"));
    assert_eq!(mode(&state), 0o700, "state directory must be 0700");
    assert_eq!(mode(&path), 0o600, "configuration must be 0600");
    assert_eq!(ClientConfig::load(&path).expect("load saved"), config);

    let leftovers: Vec<PathBuf> = fs::read_dir(&state)
        .expect("read state directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".tmp."))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files remain: {leftovers:?}"
    );

    let before = fs::read(&path).expect("read before");
    let invalid = ClientConfig {
        ui_safe_area_percent: MAX_UI_SAFE_AREA_PERCENT + 1,
        ..ClientConfig::default()
    };
    let error = save_state_config(&state, &invalid).expect_err("invalid config is refused");
    assert!(
        error.to_string().contains("ui_safe_area_percent"),
        "error must name the field: {error}"
    );
    assert_eq!(
        fs::read(&path).expect("read after"),
        before,
        "a rejected save must not touch the existing file"
    );

    let too_high = ClientConfig::from_toml_str("ui_safe_area_percent = 11\n")
        .expect_err("11 percent is out of range");
    assert!(too_high.to_string().contains("ui_safe_area_percent"));
    ClientConfig::from_toml_str("ui_safe_area_percent = 10\n").expect("10 percent is accepted");
    ClientConfig::from_toml_str("ui_safe_area_percent = 0\n").expect("0 percent is accepted");
}

#[test]
fn invalid_config_errors_do_not_echo_secrets() {
    let config = ClientConfig {
        jellyfin_url: Some("https://alice:hunter2@example.test".to_owned()),
        ..ClientConfig::default()
    };
    let error = config.validate().expect_err("credentials are refused");
    let text = error.to_string();
    assert!(text.contains("jellyfin_url"));
    assert!(!text.contains("alice"), "error leaked the user: {text}");
    assert!(
        !text.contains("hunter2"),
        "error leaked the password: {text}"
    );
}

// --- Safe area and the editable form --------------------------------------

#[test]
fn safe_area_defaults_and_clamps_in_the_editor() {
    assert_eq!(
        ClientConfig::default().ui_safe_area_percent,
        MIN_UI_SAFE_AREA_PERCENT + 3
    );

    let mut form = SettingsForm::new(&ClientConfig::default());
    form.set_focus(SettingRow::SafeArea.index());
    assert_eq!(form.handle_key(Key::Enter), SettingsEvent::Editing);
    assert_eq!(form.edit_value(), "3");
    assert_eq!(form.handle_key(Key::Backspace), SettingsEvent::Edited);
    assert_eq!(form.handle_key(Key::Char('1')), SettingsEvent::Edited);
    assert_eq!(form.handle_key(Key::Char('1')), SettingsEvent::Edited);
    assert_eq!(form.handle_key(Key::Enter), SettingsEvent::Rejected);
    assert!(form.editing(), "an invalid value keeps the edit open");
    assert!(
        form.error()
            .expect("error")
            .contains("ui_safe_area_percent")
    );

    assert_eq!(form.handle_key(Key::Escape), SettingsEvent::Navigated);
    assert_eq!(form.handle_key(Key::Enter), SettingsEvent::Editing);
    assert_eq!(form.handle_key(Key::Backspace), SettingsEvent::Edited);
    assert_eq!(form.handle_key(Key::Char('1')), SettingsEvent::Edited);
    assert_eq!(form.handle_key(Key::Char('0')), SettingsEvent::Edited);
    assert_eq!(form.handle_key(Key::Enter), SettingsEvent::Edited);
    assert_eq!(
        form.build_config()
            .expect("accepted configuration")
            .ui_safe_area_percent,
        10
    );
}

#[test]
fn settings_edit_save_and_restart_flow_through_the_ui_model() {
    let mut model = UiModel::new();
    model.reset_settings(&ClientConfig::default());
    model.set_home_focus(4);
    assert_eq!(
        press(&mut model, Key::Enter, 10),
        UiOutcome::Navigated {
            screen: Screen::Settings,
            focus: 0,
        }
    );
    assert_eq!(model.screen(), Screen::Settings);

    // Row 0: edit the Jellyfin URL, character by character.
    model.set_settings_focus(SettingRow::JellyfinUrl.index());
    assert_eq!(
        press(&mut model, Key::Enter, 20),
        UiOutcome::Navigated {
            screen: Screen::Settings,
            focus: 0,
        }
    );
    for (index, character) in "https://jellyfin.example.test".chars().enumerate() {
        assert_eq!(
            press(&mut model, Key::Char(character), 30 + index as u64),
            UiOutcome::Navigated {
                screen: Screen::Settings,
                focus: 0,
            }
        );
    }
    assert_eq!(
        press(&mut model, Key::Enter, 100),
        UiOutcome::Navigated {
            screen: Screen::Settings,
            focus: 0,
        }
    );
    assert!(!model.settings_editing());
    assert!(
        model.settings_error().is_none(),
        "valid URL was rejected: {:?}",
        model.settings_error()
    );

    // Save emits a fully validated configuration.
    model.set_settings_focus(SettingRow::Save.index());
    let outcome = press(&mut model, Key::Enter, 110);
    let UiOutcome::SaveSettings(config) = outcome else {
        panic!("expected SaveSettings, got {outcome:?}");
    };
    assert_eq!(
        config.jellyfin_url.as_deref(),
        Some("https://jellyfin.example.test")
    );

    // Restart is a distinct clean-exit request.
    model.set_settings_focus(SettingRow::Restart.index());
    assert_eq!(press(&mut model, Key::Enter, 120), UiOutcome::Restart);
}

#[test]
fn save_is_refused_for_an_inconsistency_and_reports_the_field() {
    let mut model = UiModel::new();
    model.reset_settings(&ClientConfig::default());
    model.set_home_focus(4);
    let _ = press(&mut model, Key::Enter, 10);

    // Cloud on without a server URL is inconsistent.
    model.set_settings_focus(SettingRow::CloudEnabled.index());
    let _ = press(&mut model, Key::Enter, 20);
    model.set_settings_focus(SettingRow::Save.index());
    assert_eq!(
        press(&mut model, Key::Enter, 30),
        UiOutcome::Navigated {
            screen: Screen::Settings,
            focus: SettingRow::Save.index(),
        }
    );
    let error = model.settings_error().expect("validation error");
    assert!(error.contains("server_url"), "unexpected error: {error}");

    // Escape leaves the settings screen without saving.
    assert_eq!(
        press(&mut model, Key::Escape, 40),
        UiOutcome::Action(tv_client::model::Action::Navigate {
            direction: tv_client::model::Direction::Back,
        })
    );
    assert_eq!(model.screen(), Screen::Home);
}

// --- Quick Connect ---------------------------------------------------------

#[tokio::test]
async fn quick_connect_lifecycle_stores_the_token_and_logs_out() {
    let polls = Arc::new(AtomicUsize::new(0));
    let handler_polls = Arc::clone(&polls);
    let server = MockServer::start(move |request| match request.path() {
        "/QuickConnect/Initiate" => MockResponse::json(
            200,
            json!({ "Secret": QUICK_CONNECT_SECRET, "Code": "483920" }),
        ),
        "/QuickConnect/Connect" => {
            let count = handler_polls.fetch_add(1, Ordering::SeqCst);
            MockResponse::json(200, json!({ "Authenticated": count >= 1 }))
        }
        "/Users/AuthenticateWithQuickConnect" => MockResponse::json(
            200,
            json!({
                "User": { "Id": "user-1", "Name": "tester" },
                "AccessToken": ACCESS_TOKEN,
                "ServerId": "server-1"
            }),
        ),
        "/Sessions/Logout" => MockResponse::empty(204),
        _ => MockResponse::empty(404),
    });
    let temporary = temp_dir();
    let mut storage = open_storage(temporary.path());
    let mut client = JellyfinClient::new(&client_config(&server.url), DEVICE_NAME, &mut storage)
        .expect("client builds");
    client
        .set_quick_connect_ttl(Duration::from_secs(60))
        .expect("ttl");
    let (cancel, token) = cancellation();

    let initiation = client
        .quick_connect_initiate(&token)
        .await
        .expect("initiate");
    assert_eq!(initiation.code(), "483920");
    let debug = format!("{initiation:?}");
    assert!(
        !debug.contains(QUICK_CONNECT_SECRET),
        "debug leaked the Quick Connect secret: {debug}"
    );
    assert!(debug.contains("redacted"));

    assert_eq!(
        client
            .quick_connect_poll(&initiation, &token)
            .await
            .expect("poll"),
        QuickConnectPoll::Pending
    );
    assert_eq!(
        client
            .quick_connect_poll(&initiation, &token)
            .await
            .expect("poll"),
        QuickConnectPoll::Approved
    );
    let session = client
        .quick_connect_authenticate(&mut storage, &initiation, &token)
        .await
        .expect("authenticate");
    assert_eq!(session.user_name(), Some("tester"));

    let credential = temporary.path().join("credentials/jellyfin_access_token");
    assert_eq!(mode(&credential), 0o600, "the access token must be 0600");
    assert_eq!(
        fs::read(&credential).expect("token file"),
        ACCESS_TOKEN.as_bytes()
    );
    let client_debug = format!("{client:?}");
    assert!(
        !client_debug.contains(ACCESS_TOKEN),
        "client debug leaked the access token: {client_debug}"
    );

    client.logout(&mut storage, &token).await.expect("logout");
    assert!(!credential.exists(), "logout must clear the stored token");
    assert!(!client.is_authenticated());
    let logout = server
        .requests()
        .into_iter()
        .filter(|request| request.path() == "/Sessions/Logout")
        .count();
    assert_eq!(logout, 1, "logout must revoke the server session");
    cancel.cancel();
}

#[tokio::test]
async fn quick_connect_expires_and_cancels_without_authenticating() {
    let server = MockServer::start(|request| match request.path() {
        "/QuickConnect/Initiate" => MockResponse::json(
            200,
            json!({ "Secret": QUICK_CONNECT_SECRET, "Code": "112233" }),
        ),
        "/QuickConnect/Connect" => MockResponse::json(200, json!({ "Authenticated": false }))
            .delayed(Duration::from_secs(2)),
        _ => MockResponse::empty(404),
    });
    let temporary = temp_dir();
    let mut storage = open_storage(temporary.path());
    let mut client = JellyfinClient::new(&client_config(&server.url), DEVICE_NAME, &mut storage)
        .expect("client builds");
    client
        .set_quick_connect_ttl(Duration::from_millis(30))
        .expect("ttl");
    let (_cancel, token) = cancellation();
    let initiation = client
        .quick_connect_initiate(&token)
        .await
        .expect("initiate");
    thread::sleep(Duration::from_millis(60));
    assert!(initiation.is_expired());
    assert!(matches!(
        client.quick_connect_poll(&initiation, &token).await,
        Err(JellyfinError::QuickConnectExpired)
    ));
    assert!(matches!(
        client
            .quick_connect_authenticate(&mut storage, &initiation, &token)
            .await,
        Err(JellyfinError::QuickConnectExpired)
    ));

    let (cancel, token) = cancellation();
    let initiation = client
        .quick_connect_initiate(&token)
        .await
        .expect("initiate");
    cancel.cancel();
    assert!(matches!(
        client.quick_connect_poll(&initiation, &token).await,
        Err(JellyfinError::Cancelled)
    ));
    assert!(
        !temporary
            .path()
            .join("credentials/jellyfin_access_token")
            .exists(),
        "no token may be stored for an expired or cancelled request"
    );
}

#[tokio::test]
async fn quick_connect_cancelled_during_authentication_stores_no_token() {
    let server = MockServer::start(|request| match request.path() {
        "/QuickConnect/Initiate" => MockResponse::json(
            200,
            json!({ "Secret": QUICK_CONNECT_SECRET, "Code": "778899" }),
        ),
        "/QuickConnect/Connect" => MockResponse::json(200, json!({ "Authenticated": true })),
        "/Users/AuthenticateWithQuickConnect" => MockResponse::json(
            200,
            json!({
                "User": { "Id": "user-1", "Name": "tester" },
                "AccessToken": ACCESS_TOKEN,
                "ServerId": "server-1"
            }),
        )
        .delayed(Duration::from_secs(2)),
        _ => MockResponse::empty(404),
    });
    let temporary = temp_dir();
    let mut storage = open_storage(temporary.path());
    let mut client = JellyfinClient::new(&client_config(&server.url), DEVICE_NAME, &mut storage)
        .expect("client builds");
    let (cancel, token) = cancellation();
    let initiation = client
        .quick_connect_initiate(&token)
        .await
        .expect("initiate");
    assert_eq!(
        client
            .quick_connect_poll(&initiation, &token)
            .await
            .expect("poll"),
        QuickConnectPoll::Approved
    );

    let authenticate = client.quick_connect_authenticate(&mut storage, &initiation, &token);
    tokio::pin!(authenticate);
    tokio::select! {
        _ = &mut authenticate => panic!("authentication completed before cancellation"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => cancel.cancel(),
    }
    assert!(matches!(authenticate.await, Err(JellyfinError::Cancelled)));
    assert!(
        !temporary
            .path()
            .join("credentials/jellyfin_access_token")
            .exists(),
        "a cancelled token exchange must not store a token"
    );
}

#[tokio::test]
async fn sign_out_clears_local_token_before_a_revocation_timeout() {
    let server = MockServer::start(|request| match request.path() {
        "/Sessions/Logout" => MockResponse::empty(204).delayed(Duration::from_secs(2)),
        _ => MockResponse::empty(404),
    });
    let temporary = temp_dir();
    let storage = open_storage(temporary.path());
    let credential = storage
        .write_credential(ACCESS_TOKEN_CREDENTIAL, ACCESS_TOKEN.as_bytes())
        .expect("seed access token");
    drop(storage);

    let outcome = tv_client::settings::sign_out_jellyfin(
        &client_config(&server.url),
        temporary.path(),
        Duration::from_millis(50),
    )
    .await;
    assert!(matches!(outcome, Err(JellyfinSignOutError::Timeout)));
    assert!(
        !credential.exists(),
        "a timed-out revocation must not leave the local token"
    );
}

#[tokio::test]
async fn sign_out_clears_local_token_when_revocation_is_rejected() {
    let server = MockServer::start(|request| match request.path() {
        "/Sessions/Logout" => MockResponse::empty(500),
        _ => MockResponse::empty(404),
    });
    let temporary = temp_dir();
    let storage = open_storage(temporary.path());
    let credential = storage
        .write_credential(ACCESS_TOKEN_CREDENTIAL, ACCESS_TOKEN.as_bytes())
        .expect("seed access token");
    drop(storage);

    let outcome = tv_client::settings::sign_out_jellyfin(
        &client_config(&server.url),
        temporary.path(),
        Duration::from_secs(2),
    )
    .await;
    assert!(matches!(outcome, Err(JellyfinSignOutError::Revoke(_))));
    assert!(
        !credential.exists(),
        "a rejected revocation must not leave the local token"
    );
}

#[tokio::test]
async fn sign_out_clears_local_token_without_a_configured_server() {
    let temporary = temp_dir();
    let storage = open_storage(temporary.path());
    let credential = storage
        .write_credential(ACCESS_TOKEN_CREDENTIAL, ACCESS_TOKEN.as_bytes())
        .expect("seed access token");
    drop(storage);

    let outcome = tv_client::settings::sign_out_jellyfin(
        &ClientConfig::default(),
        temporary.path(),
        Duration::from_secs(1),
    )
    .await;
    assert!(matches!(outcome, Err(JellyfinSignOutError::Revoke(_))));
    assert!(
        !credential.exists(),
        "an unusable server configuration must not leave the local token"
    );
}

// --- Mock HTTP server ------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)]
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

    #[allow(dead_code)]
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
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
