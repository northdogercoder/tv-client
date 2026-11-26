//! Deterministic tests for the pure launcher view model.
//!
//! These tests never start Slint, read the network or touch the player, and
//! they compile without the `native-ui` feature.

use std::time::Duration;

use tv_client::model::{Action, AppState, Direction, InputOrigin, Mode, Source};
use tv_client::ui::{
    HOME_ROW_COUNT, HomeRow, Key, LoadState, MediaItem, REPEAT_INTERVAL, Screen, SourceFilter,
    UiInput, UiModel, UiOutcome, UiReject, key_from_text, source_label,
};
use uuid::Uuid;

fn item(source: Source, id: &str, title: &str) -> MediaItem {
    MediaItem::new(source, id, title)
}

fn press(model: &mut UiModel, key: Key, millis: u64) -> UiOutcome {
    model.handle_key(key, Duration::from_millis(millis))
}

fn press_origin(model: &mut UiModel, key: Key, origin: InputOrigin, millis: u64) -> UiOutcome {
    model.handle_input(
        UiInput::LocalKey { key, origin },
        Duration::from_millis(millis),
    )
}

#[test]
fn home_rows_are_in_the_expected_order() {
    let labels: Vec<&str> = HomeRow::ALL.iter().map(|row| row.label()).collect();
    assert_eq!(
        labels.as_slice(),
        ["Resume", "Jellyfin", "YouTube", "Cast", "Settings"]
    );
    assert_eq!(HomeRow::ALL.len(), HOME_ROW_COUNT);
    for (index, row) in HomeRow::ALL.iter().enumerate() {
        assert_eq!(row.index(), index);
    }
}

#[test]
fn arrows_map_to_navigation_actions_and_move_home_focus() {
    let mut model = UiModel::new();
    assert_eq!(model.home_focus(), 0);

    assert_eq!(
        press(&mut model, Key::Down, 0),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Down
        })
    );
    assert_eq!(model.home_focus(), 1);

    assert_eq!(
        press(&mut model, Key::Up, 200),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Up
        })
    );
    assert_eq!(model.home_focus(), 0);

    // Clamped at the top boundary, but still a valid navigation command.
    assert_eq!(
        press(&mut model, Key::Up, 400),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Up
        })
    );
    assert_eq!(model.home_focus(), 0);

    assert_eq!(
        press(&mut model, Key::Right, 600),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Right
        })
    );
    assert_eq!(
        press(&mut model, Key::Left, 800),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Left
        })
    );
}

#[test]
fn enter_opens_a_home_source_and_escape_returns_home() {
    let mut model = UiModel::new();
    model.set_home_focus(HomeRow::Jellyfin.index());

    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Select
        })
    );
    assert_eq!(model.screen(), Screen::Library);
    assert_eq!(model.library_filter(), SourceFilter::Jellyfin);
    assert!(matches!(model.library_load(), LoadState::Loading));
    assert!(model.status_text().contains("Jellyfin"));
    assert!(model.status_text().contains("Loading"));

    assert_eq!(
        press(&mut model, Key::Escape, 200),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Back
        })
    );
    assert_eq!(model.screen(), Screen::Home);
}

#[test]
fn focus_follows_the_item_identifier_across_a_refresh() {
    let mut model = UiModel::new();
    model.open_library(SourceFilter::Jellyfin);
    model.set_library_items(vec![
        item(Source::Jellyfin, "item-a", "Alpha"),
        item(Source::Jellyfin, "item-b", "Bravo"),
        item(Source::Jellyfin, "item-c", "Charlie"),
    ]);
    assert!(model.select_library(1));
    assert_eq!(
        model.focused_item().map(|item| item.id.as_str()),
        Some("item-b")
    );
    assert_eq!(model.library_focus(), 1);

    // Bravo moves to the end; focus must follow the identifier, not the slot.
    model.set_library_items(vec![
        item(Source::Jellyfin, "item-x", "Xray"),
        item(Source::Jellyfin, "item-y", "Yankee"),
        item(Source::Jellyfin, "item-b", "Bravo"),
    ]);
    assert_eq!(model.library_focus(), 2);
    assert_eq!(
        model.focused_item().map(|item| item.id.as_str()),
        Some("item-b")
    );

    // When the selected item disappears, focus is clamped into the new list.
    model.set_library_items(vec![
        item(Source::Jellyfin, "item-x", "Xray"),
        item(Source::Jellyfin, "item-y", "Yankee"),
    ]);
    assert_eq!(model.library_focus(), 1);
    assert_eq!(
        model.focused_item().map(|item| item.id.as_str()),
        Some("item-y")
    );
}

#[test]
fn held_navigation_keys_are_throttled_per_key() {
    let mut model = UiModel::new();

    assert_eq!(
        press(&mut model, Key::Down, 0),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Down
        })
    );
    assert_eq!(model.home_focus(), 1);

    assert_eq!(press(&mut model, Key::Down, 50), UiOutcome::Throttled);
    assert_eq!(model.home_focus(), 1);

    assert_eq!(press(&mut model, Key::Down, 119), UiOutcome::Throttled);
    assert_eq!(model.home_focus(), 1);

    assert_eq!(
        press(&mut model, Key::Down, REPEAT_INTERVAL.as_millis() as u64),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Down
        })
    );
    assert_eq!(model.home_focus(), 2);

    // The throttle is per key: a different arrow is accepted immediately.
    assert_eq!(
        press(&mut model, Key::Up, 130),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Up
        })
    );
    assert_eq!(model.home_focus(), 1);

    // Non-repeatable keys are never rate limited.
    assert_eq!(press(&mut model, Key::Char('x'), 130), UiOutcome::Ignored);
    assert_eq!(press(&mut model, Key::Char('x'), 130), UiOutcome::Ignored);
}

#[test]
fn search_filters_items_and_results_carry_source_labels() {
    assert_eq!(source_label(Source::Jellyfin), "Jellyfin");
    assert_eq!(source_label(Source::YouTube), "YouTube");

    let mut model = UiModel::new();
    model.open_library(SourceFilter::All);
    model.set_library_items(vec![
        item(Source::Jellyfin, "abc", "Alpha Movie").with_subtitle("Drama"),
        item(Source::YouTube, "abcdefghijk", "Beta Clip").with_subtitle("Live"),
    ]);
    assert_eq!(model.visible_len(), 2);

    model.begin_search();
    assert!(model.searching());
    assert_eq!(
        press(&mut model, Key::Char('b'), 0),
        UiOutcome::Navigated {
            screen: Screen::Library,
            focus: 0,
        }
    );
    press(&mut model, Key::Char('e'), 200);

    assert_eq!(model.library_query(), "be");
    assert_eq!(model.visible_len(), 1);
    let focused = model.focused_item().expect("one matching result");
    assert_eq!(focused.title, "Beta Clip");
    assert_eq!(focused.source_label(), "YouTube");
    assert!(model.status_text().contains("All sources"));
    assert!(model.status_text().contains("\"be\""));

    assert_eq!(press(&mut model, Key::Escape, 400), UiOutcome::Dismissed);
    assert!(!model.searching());
    assert_eq!(model.library_query(), "");
    assert_eq!(model.visible_len(), 2);
    assert_eq!(model.screen(), Screen::Library);
}

#[test]
fn loading_empty_error_and_retry_are_distinct_states() {
    let mut model = UiModel::new();
    model.open_library(SourceFilter::Jellyfin);
    assert_eq!(model.library_load(), &LoadState::Loading);
    assert!(model.status_text().contains("Loading Jellyfin"));

    model.set_library_items(Vec::new());
    assert_eq!(model.library_load(), &LoadState::Empty);
    assert!(model.status_text().contains("No items in Jellyfin"));

    model.set_library_error("upstream timeout");
    assert_eq!(
        model.library_load(),
        &LoadState::Error {
            message: "upstream timeout".to_owned()
        }
    );
    assert_eq!(model.library_error_text(), "upstream timeout");
    assert!(model.status_text().contains("Jellyfin could not load"));
    assert!(model.status_text().contains("upstream timeout"));

    assert_eq!(press(&mut model, Key::Enter, 0), UiOutcome::Retry);
    assert_eq!(model.library_load(), &LoadState::Loading);
    assert!(model.status_text().contains("Retrying Jellyfin"));
}

#[test]
fn library_items_are_bounded_and_source_filters_cycle() {
    let mut model = UiModel::new();
    model.open_library(SourceFilter::All);
    let items = (0..100)
        .map(|index| {
            item(
                Source::Jellyfin,
                &format!("item-{index}"),
                &format!("Title {index}"),
            )
        })
        .collect();
    model.set_library_items(items);
    assert_eq!(model.stored_item_count(), 64);
    assert_eq!(model.visible_len(), 64);

    assert_eq!(model.library_filter(), SourceFilter::All);
    press(&mut model, Key::Right, 0);
    assert_eq!(model.library_filter(), SourceFilter::Jellyfin);
    press(&mut model, Key::Right, 200);
    assert_eq!(model.library_filter(), SourceFilter::YouTube);
    assert_eq!(model.visible_len(), 0);
    assert_eq!(model.library_load(), &LoadState::Empty);
    press(&mut model, Key::Left, 400);
    assert_eq!(model.library_filter(), SourceFilter::Jellyfin);
    assert_eq!(model.visible_len(), 64);
}

#[test]
fn remote_input_can_never_approve_a_local_confirmation() {
    let request_id = Uuid::new_v4();
    let mut model = UiModel::new();
    model
        .present_pairing_request(request_id)
        .expect("fresh prompt");
    assert!(model.pair_visible());

    let remote_actions = [
        Action::Navigate {
            direction: Direction::Select,
        },
        Action::Navigate {
            direction: Direction::Back,
        },
        Action::PlayJellyfin {
            item_id: "item-a".to_owned(),
        },
    ];
    for action in remote_actions {
        assert_eq!(
            model.handle_input(UiInput::RemoteAction(action), Duration::ZERO),
            UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
        );
        assert!(model.pair_visible());
        assert_eq!(
            model.pair_prompt().map(|prompt| prompt.choice),
            Some(tv_client::ui::PairChoice::Deny)
        );
    }

    // A remote-origin key event is refused as well.
    assert_eq!(
        press_origin(&mut model, Key::Right, InputOrigin::Remote, 0),
        UiOutcome::Rejected(UiReject::RemoteApprovalDenied)
    );
    assert!(model.pair_visible());

    // The local-only opener rejects a remote origin without showing anything.
    let mut other = UiModel::new();
    assert_eq!(
        other.open_local_confirmation(InputOrigin::Remote, request_id),
        Err(UiReject::RemoteApprovalDenied)
    );
    assert!(!other.pair_visible());
}

#[test]
fn local_keyboard_and_cec_can_approve_a_pending_request() {
    let keyboard_request = Uuid::new_v4();
    let mut keyboard = UiModel::new();
    keyboard
        .present_pairing_request(keyboard_request)
        .expect("fresh prompt");
    assert_eq!(press(&mut keyboard, Key::Right, 0), UiOutcome::Ignored);
    assert_eq!(
        keyboard.pair_prompt().map(|prompt| prompt.choice),
        Some(tv_client::ui::PairChoice::Approve)
    );
    assert_eq!(
        press(&mut keyboard, Key::Enter, 0),
        UiOutcome::ApprovePairing {
            request_id: keyboard_request
        }
    );
    assert!(!keyboard.pair_visible());

    let cec_request = Uuid::new_v4();
    let mut cec = UiModel::new();
    cec.present_pairing_request(cec_request)
        .expect("fresh prompt");
    assert_eq!(
        press_origin(&mut cec, Key::Right, InputOrigin::LocalCec, 0),
        UiOutcome::Ignored
    );
    assert_eq!(
        press_origin(&mut cec, Key::Enter, InputOrigin::LocalCec, 0),
        UiOutcome::ApprovePairing {
            request_id: cec_request
        }
    );
    assert!(!cec.pair_visible());
}

#[test]
fn a_pending_confirmation_cannot_be_replaced() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut model = UiModel::new();
    model.present_pairing_request(first).expect("first prompt");
    assert_eq!(
        model.present_pairing_request(second),
        Err(UiReject::ConfirmationBusy)
    );
    assert_eq!(
        model.pair_prompt().map(|prompt| prompt.request_id),
        Some(first)
    );
    assert_eq!(
        model.present_pairing_request(Uuid::nil()),
        Err(UiReject::InvalidRequest)
    );
}

#[test]
fn library_activation_validates_identifiers_and_targets_the_right_source() {
    let mut model = UiModel::new();
    model.open_library(SourceFilter::YouTube);
    model.set_library_items(vec![
        item(Source::YouTube, "short", "Bad Identifier"),
        item(Source::YouTube, "abcdefghijk", "Good Clip"),
    ]);

    // The first item is focused; a bad identifier is refused without leaving
    // the library.
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Rejected(UiReject::InvalidItem)
    );
    assert_eq!(model.screen(), Screen::Library);
    assert!(model.status_text().contains("Invalid YouTube"));

    assert!(model.select_library(1));
    assert_eq!(
        press(&mut model, Key::Enter, 200),
        UiOutcome::Action(Action::PlayYoutube {
            video_id: "abcdefghijk".to_owned()
        })
    );
    assert_eq!(model.screen(), Screen::Player);
    assert!(model.player_loading());
    assert_eq!(model.player_source_label(), "YouTube");
}

#[test]
fn escape_from_player_requests_back_and_home() {
    let mut model = UiModel::new();
    model.open_library(SourceFilter::Jellyfin);
    model.set_library_items(vec![item(Source::Jellyfin, "item-a", "Alpha")]);
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Action(Action::PlayJellyfin {
            item_id: "item-a".to_owned()
        })
    );
    assert_eq!(model.screen(), Screen::Player);
    assert_eq!(
        press(&mut model, Key::Escape, 200),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Back
        })
    );
    assert_eq!(model.screen(), Screen::Home);
}

#[test]
fn resume_row_plays_the_supplied_item_or_reports_nothing() {
    let mut model = UiModel::new();
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Navigated {
            screen: Screen::Home,
            focus: 0,
        }
    );
    assert_eq!(model.status_text(), "Nothing to resume");

    model.set_resume(Some(item(Source::Jellyfin, "item-r", "Resume Me")));
    assert_eq!(
        press(&mut model, Key::Enter, 200),
        UiOutcome::Action(Action::PlayJellyfin {
            item_id: "item-r".to_owned()
        })
    );
    assert_eq!(model.screen(), Screen::Player);
    assert_eq!(model.player_title(), "Resume Me");
}

#[test]
fn reducer_state_projects_onto_the_player_and_screens() {
    let mut model = UiModel::new();
    let playing = AppState {
        mode: Mode::Playing,
        source: Some(Source::Jellyfin),
        item_id: Some("item-a".to_owned()),
        title: Some("Alpha".to_owned()),
        position_ms: Some(65_000),
        duration_ms: Some(120_000),
        ..AppState::default()
    };
    model.apply_state(&playing);
    assert_eq!(model.screen(), Screen::Player);
    assert_eq!(model.player_title(), "Alpha");
    assert_eq!(model.player_source_label(), "Jellyfin");
    assert_eq!(model.player_position_text(), "1:05 / 2:00");
    assert!((model.player_progress() - (65.0 / 120.0)).abs() < 0.0001);
    assert!(model.status_text().contains("Playing"));

    model.apply_state(&AppState::default());
    assert_eq!(model.screen(), Screen::Home);
    assert_eq!(model.status_text(), "Choose a source");

    let failed = AppState {
        mode: Mode::Error,
        error: Some("player_exit_1".to_owned()),
        ..AppState::default()
    };
    model.apply_state(&failed);
    assert_eq!(model.screen(), Screen::Player);
    assert!(model.status_text().contains("player_exit_1"));
}

#[test]
fn slint_special_keys_never_become_search_text() {
    assert_eq!(key_from_text("\u{000a}"), Some(Key::Enter));
    assert_eq!(key_from_text("\u{0008}"), Some(Key::Backspace));
    assert_eq!(key_from_text("\u{F700}"), Some(Key::Up));
    assert_eq!(key_from_text("\u{F729}"), Some(Key::Home));

    for special in [
        "\u{F704}", "\u{F727}", "\u{F72B}", "\u{F72C}", "\u{F72D}", "\u{F7FF}",
    ] {
        assert_eq!(
            key_from_text(special),
            None,
            "{special:?} must not become text input"
        );
    }

    assert_eq!(key_from_text("a"), Some(Key::Char('a')));
    assert_eq!(key_from_text(" "), Some(Key::Char(' ')));
    assert_eq!(key_from_text("\u{0009}"), None);
}

#[test]
fn home_key_ends_search_instead_of_typing() {
    let mut model = UiModel::new();
    model.open_library(SourceFilter::Jellyfin);
    model.set_library_items(vec![item(Source::Jellyfin, "item-a", "Alpha")]);
    model.begin_search();

    assert_eq!(
        press(&mut model, Key::Home, 0),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Home
        })
    );
    assert_eq!(model.screen(), Screen::Home);
    assert_eq!(model.library_query(), "");
    assert!(!model.searching());
}

#[test]
fn library_load_requests_follow_loading_transitions() {
    let mut model = UiModel::new();
    assert!(!model.is_library_loading());
    assert_eq!(model.pending_library_load(false), None);

    model.set_home_focus(HomeRow::Jellyfin.index());
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Action(Action::Navigate {
            direction: Direction::Select
        })
    );
    assert!(model.is_library_loading());
    assert_eq!(
        model.pending_library_load(false),
        Some(SourceFilter::Jellyfin)
    );
    assert_eq!(model.pending_library_load(true), None);

    model.set_library_error("timeout");
    assert!(!model.is_library_loading());
    assert_eq!(model.pending_library_load(false), None);
    assert_eq!(press(&mut model, Key::Enter, 200), UiOutcome::Retry);
    assert!(model.is_library_loading());
    assert_eq!(
        model.pending_library_load(false),
        Some(SourceFilter::Jellyfin)
    );
}
