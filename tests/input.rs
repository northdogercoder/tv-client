//! Feature-independent tests for physical input, provenance and the kernel
//! CEC bridge.
//!
//! These tests compile with `--no-default-features`: they never start Slint,
//! never open `/dev/cec0`, and the supervised-helper tests use a temporary
//! fixture program with the production argument list.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use tv_client::input::cec::MAX_CEC_LINE;
use tv_client::input::{
    Backoff, CecBridge, CecConfig, CecDisconnect, CecParse, CecParser, CecStatus, CecUnavailable,
    InputEvent, InputQueue, InputReject, LocalOrigin, LongPress, PhysicalApproval, RepeatGate,
    Route, Router, ShutdownOutcome, key_from_cec_ui_command, key_from_text,
};
use tv_client::model::{Action, AppState, Direction, Event, InputOrigin, Mode, StateError};
use tv_client::state::Reducer;
use tv_client::ui::{Key as UiKey, Screen, SourceFilter, UiInput, UiModel, UiOutcome, UiReject};
use uuid::Uuid;

/// Exact cec-ctl 1.30.1 follow-mode framing: the `show_msg` prefix is on the
/// same line as the bare message name, and the `ui-cmd` argument follows on
/// its own indented line.
const ACTIVE_SOURCE: &str = "Received from TV to Playback Device 1 (0 to 4): ACTIVE_SOURCE (0x82):";
const ACTIVE_SOURCE_BROADCAST: &str = "Received from TV to all (0 to 15): ACTIVE_SOURCE (0x82)";
const ACTIVE_SOURCE_ADDR: &str = "\tphys-addr: 1.0.0.0";
const USER_CONTROL_PRESSED: &str =
    "Received from TV to Playback Device 1 (0 to 4): USER_CONTROL_PRESSED (0x44):";
const USER_CONTROL_RELEASED: &str =
    "Received from TV to Playback Device 1 (0 to 4): USER_CONTROL_RELEASED (0x45)";
const UI_CMD_UP: &str = "\tui-cmd: up (0x01)";

fn event(origin: InputOrigin, key: tv_client::input::Key) -> InputEvent {
    InputEvent {
        origin,
        key,
        repeat: false,
    }
}

fn recv_until<T>(receiver: &Receiver<T>, predicate: impl Fn(&T) -> bool, timeout: Duration) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let value = receiver
            .recv_timeout(remaining)
            .expect("expected a matching message before the deadline");
        if predicate(&value) {
            return value;
        }
    }
}

#[test]
fn keyboard_text_maps_named_keys_and_keeps_search_characters() {
    assert_eq!(
        key_from_text("\u{000a}"),
        Some(tv_client::input::Key::Select)
    );
    assert_eq!(key_from_text("\r"), Some(tv_client::input::Key::Select));
    assert_eq!(key_from_text("\u{001b}"), Some(tv_client::input::Key::Back));
    assert_eq!(
        key_from_text("\u{0008}"),
        Some(tv_client::input::Key::Backspace)
    );
    assert_eq!(key_from_text("\u{F700}"), Some(tv_client::input::Key::Up));
    assert_eq!(key_from_text("\u{F701}"), Some(tv_client::input::Key::Down));
    assert_eq!(key_from_text("\u{F702}"), Some(tv_client::input::Key::Left));
    assert_eq!(
        key_from_text("\u{F703}"),
        Some(tv_client::input::Key::Right)
    );
    assert_eq!(key_from_text("\u{F729}"), Some(tv_client::input::Key::Home));
    assert_eq!(key_from_text("/"), Some(tv_client::input::Key::Char('/')));
    assert_eq!(key_from_text("ab"), None);
    assert_eq!(key_from_text("\u{F704}"), None);
    assert_eq!(key_from_text("\u{0000}"), None);
}

#[test]
fn cec_ui_commands_map_without_creating_keyboard_characters() {
    use tv_client::input::Key;

    // Real cec-ctl 1.30.1 output: lowercase, dash-separated names with the
    // parenthesized ui-cmd opcode.
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: up (0x01)"),
        Some(Key::Up)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: down (0x02)"),
        Some(Key::Down)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: left (0x03)"),
        Some(Key::Left)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: right (0x04)"),
        Some(Key::Right)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: select (0x00)"),
        Some(Key::Select)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: enter (0x2b)"),
        Some(Key::Select)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: back (0x0d)"),
        Some(Key::Back)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: device-root-menu (0x09)"),
        Some(Key::Home)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: media-top-menu (0x10)"),
        Some(Key::Home)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: contents-menu (0x0b)"),
        Some(Key::Home)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: play (0x44)"),
        Some(Key::PlayPause)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: pause (0x46)"),
        Some(Key::PlayPause)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: play-function (0x60)"),
        Some(Key::PlayPause)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: pause-play-function (0x61)"),
        Some(Key::PlayPause)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: volume-up (0x41)"),
        Some(Key::VolumeUp)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: volume-down (0x42)"),
        Some(Key::VolumeDown)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: mute (0x43)"),
        Some(Key::Mute)
    );
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: mute-function (0x65)"),
        Some(Key::Mute)
    );

    // Legacy logs and fixtures that use the enum spelling are still accepted.
    assert_eq!(key_from_cec_ui_command("UI_CMD_UP"), Some(Key::Up));
    assert_eq!(key_from_cec_ui_command("\tui_cmd:"), None);
    assert_eq!(key_from_cec_ui_command("UI_CMD_UP (0x01)"), Some(Key::Up));
    assert_eq!(key_from_cec_ui_command("UI_CMD_SELECT"), Some(Key::Select));
    assert_eq!(key_from_cec_ui_command("UI_CMD_ENTER"), Some(Key::Select));
    assert_eq!(key_from_cec_ui_command("UI_CMD_BACK"), Some(Key::Back));
    assert_eq!(key_from_cec_ui_command("UI_CMD_EXIT"), Some(Key::Back));
    assert_eq!(
        key_from_cec_ui_command("UI_CMD_DEVICE_ROOT_MENU"),
        Some(Key::Home)
    );
    assert_eq!(
        key_from_cec_ui_command("UI_CMD_MEDIA_TOP_MENU"),
        Some(Key::Home)
    );
    assert_eq!(key_from_cec_ui_command("UI_CMD_PLAY"), Some(Key::PlayPause));
    assert_eq!(
        key_from_cec_ui_command("UI_CMD_PAUSE_PLAY_FUNCTION"),
        Some(Key::PlayPause)
    );
    assert_eq!(
        key_from_cec_ui_command("UI_CMD_VOLUME_UP"),
        Some(Key::VolumeUp)
    );
    assert_eq!(
        key_from_cec_ui_command("UI_CMD_VOLUME_DOWN"),
        Some(Key::VolumeDown)
    );
    assert_eq!(key_from_cec_ui_command("UI_CMD_MUTE"), Some(Key::Mute));

    // Unknown names fall back to the ui-cmd opcode; unknown opcodes and
    // non-ui message headers never become keys.
    assert_eq!(
        key_from_cec_ui_command("\tui-cmd: unrecognized (0x46)"),
        Some(Key::PlayPause)
    );
    assert_eq!(key_from_cec_ui_command("\tui-cmd: record (0x47)"), None);
    assert_eq!(key_from_cec_ui_command("\tui-cmd: stop (0x45)"), None);
    assert_eq!(key_from_cec_ui_command("UI_CMD_NUMBER_1"), None);
    assert_eq!(key_from_cec_ui_command("UI_CMD_STOP"), None);
    assert_eq!(key_from_cec_ui_command("UI_CMD_"), None);
    assert_eq!(key_from_cec_ui_command("not-a-command"), None);
    assert_eq!(
        key_from_cec_ui_command(
            "Received from TV to Playback Device 1 (0 to 4): USER_CONTROL_PRESSED (0x44):"
        ),
        None,
        "a message header is not a ui-cmd"
    );
}

#[test]
fn router_preserves_local_provenance_and_repeat_flag() {
    let keyboard = Router::route(InputEvent {
        origin: InputOrigin::LocalKeyboard,
        key: tv_client::input::Key::Down,
        repeat: false,
    });
    assert_eq!(
        keyboard,
        Route::Ui {
            key: tv_client::input::Key::Down,
            origin: LocalOrigin::Keyboard,
            repeat: false,
        }
    );

    let cec = Router::route(InputEvent {
        origin: InputOrigin::LocalCec,
        key: tv_client::input::Key::Back,
        repeat: true,
    });
    assert_eq!(
        cec,
        Route::Ui {
            key: tv_client::input::Key::Back,
            origin: LocalOrigin::Cec,
            repeat: true,
        }
    );

    let volume = Router::route(InputEvent {
        origin: InputOrigin::LocalCec,
        key: tv_client::input::Key::VolumeUp,
        repeat: false,
    });
    assert!(matches!(
        volume,
        Route::Volume {
            key: tv_client::input::Key::VolumeUp,
            origin: LocalOrigin::Cec,
            repeat: false,
        }
    ));
}

#[test]
fn router_refuses_remote_events_before_they_become_keys() {
    for key in [
        tv_client::input::Key::Up,
        tv_client::input::Key::Select,
        tv_client::input::Key::Back,
        tv_client::input::Key::VolumeUp,
    ] {
        assert_eq!(
            Router::route(event(InputOrigin::Remote, key)),
            Route::Rejected(InputReject::RemoteNotLocal)
        );
    }
    assert_eq!(
        LocalOrigin::try_from(InputOrigin::Remote),
        Err(InputReject::RemoteNotLocal)
    );
}

#[test]
fn physical_approval_requires_a_local_origin() {
    let request = Uuid::new_v4();
    let approval = PhysicalApproval::new(InputOrigin::LocalKeyboard, request)
        .expect("keyboard approval is physical");
    assert_eq!(approval.origin(), LocalOrigin::Keyboard);
    assert_eq!(approval.request_id(), request);
    assert_eq!(
        approval.into_event(),
        Event::physical_approval(InputOrigin::LocalKeyboard, request)
    );

    let cec =
        PhysicalApproval::new(InputOrigin::LocalCec, request).expect("CEC approval is physical");
    assert_eq!(cec.origin(), LocalOrigin::Cec);
    assert_eq!(
        PhysicalApproval::new(InputOrigin::Remote, request),
        Err(InputReject::RemoteNotLocal)
    );
    assert_eq!(
        PhysicalApproval::new(InputOrigin::LocalCec, Uuid::nil()),
        Err(InputReject::InvalidRequest)
    );

    // The reducer independently refuses a forged remote approval.
    assert_eq!(
        Reducer::new()
            .apply(Event::physical_approval(InputOrigin::Remote, request))
            .expect_err("remote approval must be refused"),
        StateError::RemoteApprovalDenied
    );
}

#[test]
fn repeat_gate_debounces_per_slot_and_resets() {
    let mut gate = RepeatGate::new(Duration::from_millis(120));
    assert!(gate.accept(0, Duration::ZERO));
    assert!(!gate.accept(0, Duration::from_millis(50)));
    assert!(!gate.accept(0, Duration::from_millis(119)));
    assert!(gate.accept(0, Duration::from_millis(120)));
    assert!(gate.accept(1, Duration::from_millis(120)));
    gate.reset();
    assert!(gate.accept(0, Duration::from_millis(121)));
}

#[test]
fn long_press_fires_once_and_rearms_after_release() {
    let mut long_press = LongPress::new(Duration::from_millis(600));
    let key = tv_client::input::Key::Home;
    assert!(!long_press.press(key, Duration::ZERO));
    assert!(!long_press.press(key, Duration::from_millis(500)));
    assert!(long_press.press(key, Duration::from_millis(600)));
    assert!(!long_press.press(key, Duration::from_millis(900)));
    long_press.release(key);
    assert_eq!(long_press.held(), None);
    assert!(!long_press.press(key, Duration::from_millis(1_000)));
}

#[test]
fn input_queue_is_bounded_and_fifo() {
    let queue = InputQueue::new(2).expect("valid capacity");
    let first = event(InputOrigin::LocalKeyboard, tv_client::input::Key::Up);
    let second = event(InputOrigin::LocalCec, tv_client::input::Key::Down);
    let third = event(InputOrigin::LocalCec, tv_client::input::Key::Back);

    queue.try_push(first).expect("first fits");
    queue.try_push(second).expect("second fits");
    assert_eq!(
        queue.try_push(third),
        Err(tv_client::input::InputError::QueueFull)
    );
    assert_eq!(queue.dropped(), 1);
    assert_eq!(queue.len(), 2);
    assert_eq!(queue.pop(), Some(first));
    assert_eq!(queue.pop(), Some(second));
    assert_eq!(queue.pop(), None);
    assert!(queue.is_empty());

    assert_eq!(
        InputQueue::new(0).expect_err("zero is invalid"),
        tv_client::input::InputError::InvalidCapacity
    );
    assert!(InputQueue::new(tv_client::input::MAX_INPUT_CAPACITY + 1).is_err());
}

#[test]
fn cec_parser_ignores_releases_unknown_commands_and_unframed_ui_commands() {
    let mut parser = CecParser::new();
    assert_eq!(
        parser.feed_line(ACTIVE_SOURCE, Duration::ZERO),
        CecParse::Ignored
    );
    assert_eq!(
        parser.feed_line(ACTIVE_SOURCE_ADDR, Duration::ZERO),
        CecParse::Ignored
    );
    assert_eq!(
        parser.feed_line(USER_CONTROL_PRESSED, Duration::ZERO),
        CecParse::Ignored
    );
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::ZERO),
        CecParse::Event(InputEvent {
            origin: InputOrigin::LocalCec,
            key: tv_client::input::Key::Up,
            repeat: false,
        })
    );

    assert_eq!(
        parser.feed_line(USER_CONTROL_RELEASED, Duration::from_millis(200)),
        CecParse::Ignored
    );
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::from_millis(250)),
        CecParse::Ignored,
        "a ui-cmd without a press frame is not a key"
    );

    parser.feed_line(USER_CONTROL_PRESSED, Duration::from_millis(300));
    assert_eq!(
        parser.feed_line("\tui-cmd: record (0x47)", Duration::from_millis(300)),
        CecParse::Ignored,
        "unsupported commands never become a guessed key"
    );
    assert_eq!(
        parser.feed_line(USER_CONTROL_RELEASED, Duration::from_millis(350)),
        CecParse::Ignored
    );
}

#[test]
fn cec_parser_drops_a_stale_press_frame_when_another_message_starts() {
    let mut parser = CecParser::new();
    assert_eq!(
        parser.feed_line(USER_CONTROL_PRESSED, Duration::ZERO),
        CecParse::Ignored
    );
    assert_eq!(
        parser.feed_line(ACTIVE_SOURCE_BROADCAST, Duration::ZERO),
        CecParse::Ignored
    );
    assert_eq!(
        parser.feed_line(ACTIVE_SOURCE_ADDR, Duration::ZERO),
        CecParse::Ignored
    );
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::ZERO),
        CecParse::Ignored,
        "an ACTIVE_SOURCE header cancels the pending press frame"
    );
}

#[test]
fn cec_parser_maps_real_pause_and_home_lines() {
    let mut parser = CecParser::new();
    parser.feed_line(USER_CONTROL_PRESSED, Duration::ZERO);
    assert_eq!(
        parser.feed_line("\tui-cmd: pause (0x46)", Duration::ZERO),
        CecParse::Event(InputEvent {
            origin: InputOrigin::LocalCec,
            key: tv_client::input::Key::PlayPause,
            repeat: false,
        })
    );

    parser.feed_line(USER_CONTROL_PRESSED, Duration::from_millis(200));
    assert_eq!(
        parser.feed_line(
            "\tui-cmd: device-root-menu (0x09)",
            Duration::from_millis(200)
        ),
        CecParse::Event(InputEvent {
            origin: InputOrigin::LocalCec,
            key: tv_client::input::Key::Home,
            repeat: false,
        })
    );
}

#[test]
fn cec_parser_marks_held_repeats_and_debounces_bursts() {
    let mut parser = CecParser::new();

    parser.feed_line(USER_CONTROL_PRESSED, Duration::ZERO);
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::ZERO),
        CecParse::Event(InputEvent {
            origin: InputOrigin::LocalCec,
            key: tv_client::input::Key::Up,
            repeat: false,
        })
    );

    // A burst inside the 120 ms interval is debounced.
    parser.feed_line(USER_CONTROL_PRESSED, Duration::from_millis(40));
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::from_millis(40)),
        CecParse::Ignored
    );

    // Outside the interval the same held key is marked as a repeat.
    parser.feed_line(USER_CONTROL_PRESSED, Duration::from_millis(200));
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::from_millis(200)),
        CecParse::Event(InputEvent {
            origin: InputOrigin::LocalCec,
            key: tv_client::input::Key::Up,
            repeat: true,
        })
    );

    // A release ends the hold; a later press is not a repeat.
    parser.feed_line(USER_CONTROL_RELEASED, Duration::from_millis(250));
    parser.feed_line(USER_CONTROL_PRESSED, Duration::from_millis(500));
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::from_millis(500)),
        CecParse::Event(InputEvent {
            origin: InputOrigin::LocalCec,
            key: tv_client::input::Key::Up,
            repeat: false,
        })
    );
}

#[test]
fn cec_parser_enforces_the_line_bound() {
    let mut parser = CecParser::new();
    parser.feed_line(USER_CONTROL_PRESSED, Duration::ZERO);

    let oversized = format!("ui-cmd: up {}", "x".repeat(MAX_CEC_LINE));
    assert_eq!(
        parser.feed_line(oversized.as_str(), Duration::ZERO),
        CecParse::Truncated
    );
    assert_eq!(
        parser.feed_line(UI_CMD_UP, Duration::ZERO),
        CecParse::Ignored,
        "a truncated line resets the press frame"
    );

    // Exactly the maximum length is still accepted.
    let mut exact = String::from(UI_CMD_UP);
    exact.push_str(&" ".repeat(MAX_CEC_LINE - exact.len()));
    parser.feed_line(USER_CONTROL_PRESSED, Duration::ZERO);
    assert!(matches!(
        parser.feed_line(&exact, Duration::ZERO),
        CecParse::Event(_)
    ));
}

#[test]
fn cec_helper_arguments_are_fixed_and_shell_free() {
    let arguments: Vec<String> = CecConfig::kernel()
        .arguments()
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        arguments,
        vec![
            "--device",
            "/dev/cec0",
            "--playback",
            "--no-rc-passthrough",
            "--wait-for-msgs",
            "--skip-info",
        ]
    );
}

#[test]
fn cec_backoff_doubles_then_caps_and_resets() {
    let mut backoff = Backoff::new(Duration::from_millis(10), Duration::from_millis(40));
    assert_eq!(backoff.next_delay(), Duration::from_millis(10));
    assert_eq!(backoff.next_delay(), Duration::from_millis(20));
    assert_eq!(backoff.next_delay(), Duration::from_millis(40));
    assert_eq!(backoff.next_delay(), Duration::from_millis(40));
    backoff.reset();
    assert_eq!(backoff.next_delay(), Duration::from_millis(10));
}

#[test]
fn cec_back_while_the_player_owns_the_surface_stops_and_returns_home() {
    let mut model = playing_model();
    let outcome = model.handle_event(
        event(InputOrigin::LocalCec, tv_client::input::Key::Back),
        Duration::ZERO,
    );
    assert_eq!(
        outcome,
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Back
        }),
        "the parent controller maps Back to Stop"
    );
    assert_eq!(model.screen(), Screen::Home);
}

#[test]
fn cec_play_pause_only_acts_on_the_player_screen() {
    let mut player = playing_model();
    assert_eq!(
        player.handle_event(
            event(InputOrigin::LocalCec, tv_client::input::Key::PlayPause),
            Duration::ZERO,
        ),
        UiOutcome::Action(Action::SetPaused { paused: true })
    );

    let mut library = UiModel::new();
    library.open_library(SourceFilter::All);
    library.begin_search();
    assert_eq!(
        library.handle_event(
            event(InputOrigin::LocalCec, tv_client::input::Key::PlayPause),
            Duration::ZERO,
        ),
        UiOutcome::Ignored
    );
    assert!(library.searching());
    assert_eq!(
        library.library_query(),
        "",
        "PlayPause must not type a space"
    );

    let mut home = UiModel::new();
    assert_eq!(
        home.handle_event(
            event(InputOrigin::LocalCec, tv_client::input::Key::PlayPause),
            Duration::ZERO,
        ),
        UiOutcome::Ignored
    );
    assert_eq!(home.screen(), Screen::Home);
}

#[test]
fn keyboard_escape_is_equivalent_to_cec_back() {
    let mut cec_model = playing_model();
    let mut keyboard_model = playing_model();

    let cec = cec_model.handle_event(
        event(InputOrigin::LocalCec, tv_client::input::Key::Back),
        Duration::ZERO,
    );
    let escape = keyboard_model.handle_event(
        event(InputOrigin::LocalKeyboard, tv_client::input::Key::Back),
        Duration::ZERO,
    );
    assert_eq!(cec, escape);
    assert_eq!(cec_model.screen(), keyboard_model.screen());
    assert_eq!(keyboard_model.screen(), Screen::Home);
}

#[test]
fn cec_home_while_the_player_owns_the_surface_returns_home() {
    let mut model = playing_model();
    let outcome = model.handle_event(
        event(InputOrigin::LocalCec, tv_client::input::Key::Home),
        Duration::ZERO,
    );
    assert_eq!(
        outcome,
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Home
        })
    );
    assert_eq!(model.screen(), Screen::Home);
}

fn playing_model() -> UiModel {
    let mut model = UiModel::new();
    model.apply_state(&AppState {
        mode: Mode::Playing,
        ..AppState::default()
    });
    assert_eq!(model.screen(), Screen::Player);
    model
}

#[test]
fn local_cec_select_can_approve_but_remote_select_cannot() {
    let request = Uuid::new_v4();
    let mut model = UiModel::new();
    model
        .present_pairing_request(request)
        .expect("pairing prompt is available");
    model.focus_pair_choice(1);
    assert_eq!(
        model.handle_event(
            event(InputOrigin::LocalCec, tv_client::input::Key::Select),
            Duration::ZERO,
        ),
        UiOutcome::ApprovePairing {
            request_id: request
        }
    );
    assert!(model.pair_prompt().is_none());

    let remote_request = Uuid::new_v4();
    model
        .present_pairing_request(remote_request)
        .expect("second pairing prompt is available");
    model.focus_pair_choice(1);
    assert_eq!(
        model.handle_event(
            event(InputOrigin::Remote, tv_client::input::Key::Select),
            Duration::ZERO,
        ),
        UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
    );
    assert!(
        model.pair_prompt().is_some(),
        "the remote press must not dismiss or approve the prompt"
    );

    // The existing remote wire path is refused in the same way.
    assert_eq!(
        model.handle_input(
            UiInput::RemoteAction(Action::Navigate {
                direction: Direction::Down
            }),
            Duration::ZERO,
        ),
        UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
    );

    // Constructing a local-key input with a remote origin is refused before
    // the key can reach the modal handler.
    assert_eq!(
        model.handle_input(
            UiInput::LocalKey {
                key: UiKey::Enter,
                origin: InputOrigin::Remote,
            },
            Duration::ZERO,
        ),
        UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
    );
    assert!(model.pair_prompt().is_some());
}

#[test]
fn local_keyboard_enter_still_approves_the_prompt() {
    let request = Uuid::new_v4();
    let mut model = UiModel::new();
    model
        .present_pairing_request(request)
        .expect("pairing prompt is available");
    model.focus_pair_choice(1);
    assert_eq!(
        model.handle_event(
            event(InputOrigin::LocalKeyboard, tv_client::input::Key::Select),
            Duration::ZERO,
        ),
        UiOutcome::ApprovePairing {
            request_id: request
        }
    );
}

#[test]
fn remote_events_without_a_prompt_are_ignored() {
    let mut model = UiModel::new();
    assert_eq!(
        model.handle_event(
            event(InputOrigin::Remote, tv_client::input::Key::Back),
            Duration::ZERO,
        ),
        UiOutcome::Ignored
    );
    assert_eq!(model.screen(), Screen::Home);
}

#[test]
fn ui_key_names_match_the_input_mapping() {
    assert_eq!(UiKey::Up.into_input(), tv_client::input::Key::Up);
    assert_eq!(UiKey::Enter.into_input(), tv_client::input::Key::Select);
    assert_eq!(UiKey::Escape.into_input(), tv_client::input::Key::Back);
    assert_eq!(
        UiKey::from_input(tv_client::input::Key::Select),
        Some(UiKey::Enter)
    );
    assert_eq!(UiKey::from_input(tv_client::input::Key::Mute), None);
    assert_eq!(
        UiKey::from_input(tv_client::input::Key::PlayPause),
        None,
        "PlayPause is routed by UiModel::handle_event, never as Char(' ')"
    );
}

#[test]
fn missing_device_is_a_typed_unavailable_state_and_shutdown_is_bounded() {
    let directory = tempfile::tempdir().expect("temp directory");
    let (status_tx, status_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let bridge = CecBridge::start(
        CecConfig::kernel()
            .with_device(directory.path().join("no-cec0"))
            .with_program(directory.path().join("no-cec-ctl"))
            .with_probe_interval(Duration::from_millis(20)),
        move |event| {
            let _ = event_tx.send(event);
        },
        move |status| {
            let _ = status_tx.send(status.clone());
        },
    )
    .expect("supervisor thread");

    let status = recv_until(
        &status_rx,
        |status| matches!(status, CecStatus::Unavailable { .. }),
        Duration::from_secs(2),
    );
    assert_eq!(
        status,
        CecStatus::Unavailable {
            reason: CecUnavailable::DeviceMissing
        }
    );
    assert!(event_rx.try_recv().is_err());
    assert_eq!(
        bridge.shutdown(Duration::from_secs(1)),
        ShutdownOutcome::Stopped
    );
}

#[test]
fn missing_helper_is_a_typed_unavailable_state() {
    let directory = tempfile::tempdir().expect("temp directory");
    let device = directory.path().join("cec0");
    std::fs::write(&device, b"").expect("fake device");
    let (status_tx, status_rx) = mpsc::channel();
    let bridge = CecBridge::start(
        CecConfig::kernel()
            .with_device(device)
            .with_program(directory.path().join("missing-cec-ctl"))
            .with_probe_interval(Duration::from_millis(20)),
        |_| {},
        move |status| {
            let _ = status_tx.send(status.clone());
        },
    )
    .expect("supervisor thread");

    let status = recv_until(
        &status_rx,
        |status| matches!(status, CecStatus::Unavailable { .. }),
        Duration::from_secs(2),
    );
    assert_eq!(
        status,
        CecStatus::Unavailable {
            reason: CecUnavailable::HelperMissing
        }
    );
    assert_eq!(
        bridge.shutdown(Duration::from_secs(1)),
        ShutdownOutcome::Stopped
    );
}

#[cfg(unix)]
#[test]
fn cec_helper_exit_reports_disconnect_and_events_keep_local_origin() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temp directory");
    let device = directory.path().join("cec0");
    std::fs::write(&device, b"").expect("fake device");
    let script = directory.path().join("fake-cec-ctl");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         printf 'Received from TV to Playback Device 1 (0 to 4): ACTIVE_SOURCE (0x82):\\n'\n\
         printf '\\tphys-addr: 1.0.0.0\\n'\n\
         printf 'Received from TV to Playback Device 1 (0 to 4): USER_CONTROL_PRESSED (0x44):\\n'\n\
         printf '\\tui-cmd: up (0x01)\\n'\n\
         printf 'Received from TV to Playback Device 1 (0 to 4): USER_CONTROL_RELEASED (0x45)\\n'\n\
         exit 3\n",
    )
    .expect("fixture helper");
    let mut permissions = std::fs::metadata(&script)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).expect("fixture permissions");

    let (status_tx, status_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let bridge = CecBridge::start(
        CecConfig::kernel()
            .with_device(device)
            .with_program(script)
            .with_probe_interval(Duration::from_millis(10))
            .with_backoff(Backoff::new(
                Duration::from_millis(10),
                Duration::from_millis(40),
            )),
        move |event| {
            let _ = event_tx.send(event);
        },
        move |status| {
            let _ = status_tx.send(status.clone());
        },
    )
    .expect("supervisor thread");

    let event = event_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fixture press reaches the input sink");
    assert_eq!(event.origin, InputOrigin::LocalCec);
    assert_eq!(event.key, tv_client::input::Key::Up);

    let status = recv_until(
        &status_rx,
        |status| matches!(status, CecStatus::Disconnected { .. }),
        Duration::from_secs(2),
    );
    match status {
        CecStatus::Disconnected {
            reason: CecDisconnect::Exited { code },
            retry_in,
        } => {
            assert_eq!(code, Some(3));
            assert!(retry_in >= Duration::from_millis(10));
        }
        other => panic!("unexpected disconnect status: {other:?}"),
    }

    assert_eq!(
        bridge.shutdown(Duration::from_secs(1)),
        ShutdownOutcome::Stopped
    );
}

#[cfg(unix)]
#[test]
fn shutdown_kills_a_running_helper_within_the_deadline() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temp directory");
    let device = directory.path().join("cec0");
    std::fs::write(&device, b"").expect("fake device");
    let script = directory.path().join("sleepy-cec-ctl");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 60\n").expect("fixture helper");
    let mut permissions = std::fs::metadata(&script)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).expect("fixture permissions");

    let (status_tx, status_rx) = mpsc::channel();
    let bridge = CecBridge::start(
        CecConfig::kernel()
            .with_device(device)
            .with_program(script)
            .with_probe_interval(Duration::from_millis(10)),
        |_| {},
        move |status| {
            let _ = status_tx.send(status.clone());
        },
    )
    .expect("supervisor thread");

    recv_until(
        &status_rx,
        |status| matches!(status, CecStatus::Monitoring { .. }),
        Duration::from_secs(2),
    );

    let started = Instant::now();
    assert_eq!(
        bridge.shutdown(Duration::from_secs(2)),
        ShutdownOutcome::Stopped
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown must not wait for the helper's own lifetime"
    );
}
