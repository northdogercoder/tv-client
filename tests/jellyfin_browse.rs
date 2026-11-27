//! Deterministic tests for the Jellyfin browse screens.
//!
//! These tests never start Slint, read the network or touch the player. They
//! drive the pure view model with the same typed updates the playback
//! controller publishes.

use std::time::Duration;

use tv_client::jellyfin::{ArtworkClass, BrowseItem, LibraryEntry};
use tv_client::playback::BrowseUpdate;
use tv_client::ui::{
    HomeRow, JELLYFIN_LIBRARY_COLUMNS, JELLYFIN_POSTER_COLUMNS, Key, LoadState,
    MAX_JELLYFIN_LIBRARIES, MAX_LIBRARY_ITEMS, SESSION_EXPIRED_MESSAGE, Screen, UiBrowseCommand,
    UiModel, UiOutcome,
};

fn library(id: &str, name: &str) -> LibraryEntry {
    LibraryEntry {
        id: id.to_owned(),
        name: name.to_owned(),
        collection_type: Some("movies".to_owned()),
    }
}

fn item(id: &str, title: &str, tag: Option<&str>) -> BrowseItem {
    BrowseItem {
        id: id.to_owned(),
        title: title.to_owned(),
        subtitle: "Movie · 2021".to_owned(),
        overview: Some("An overview".to_owned()),
        primary_tag: tag.map(str::to_owned),
        item_type: Some("Movie".to_owned()),
    }
}

fn items(count: usize, first: usize, tag: bool) -> Vec<BrowseItem> {
    (first..first + count)
        .map(|index| {
            item(
                &format!("movie-{index}"),
                &format!("Movie {index}"),
                tag.then_some("primary-tag"),
            )
        })
        .collect()
}

fn press(model: &mut UiModel, key: Key, millis: u64) -> UiOutcome {
    model.handle_key(key, Duration::from_millis(millis))
}

fn open_library_grid() -> UiModel {
    let mut model = UiModel::new();
    model.set_home_focus(HomeRow::Jellyfin.index());
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Browse(UiBrowseCommand::LoadLibraries)
    );
    model.apply_browse_update(&BrowseUpdate::LibrariesReady {
        libraries: vec![
            library("library-1", "Movies"),
            library("library-2", "Shows"),
        ],
    });
    model
}

fn open_items(model: &mut UiModel) -> UiOutcome {
    press(model, Key::Enter, 0)
}

#[test]
fn browse_screens_have_stable_indices_after_cast() {
    assert_eq!(Screen::Cast.index(), 4);
    assert_eq!(Screen::JellyfinLibraries.index(), 5);
    assert_eq!(Screen::JellyfinItems.index(), 6);
    assert_eq!(Screen::Preview.index(), 7);
}

#[test]
fn home_jellyfin_opens_the_library_grid_and_enter_requests_a_parent_page() {
    let mut model = open_library_grid();
    assert_eq!(model.screen(), Screen::JellyfinLibraries);
    assert_eq!(model.jellyfin_libraries_load(), &LoadState::Ready);
    assert_eq!(model.jellyfin_libraries().len(), 2);
    assert_eq!(
        model.jellyfin_libraries()[0].name,
        "Movies",
        "server names are preserved for display"
    );

    assert_eq!(
        open_items(&mut model),
        UiOutcome::Browse(UiBrowseCommand::LoadItems {
            library_id: "library-1".to_owned(),
            collection_type: Some("movies".to_owned()),
            start_index: 0,
        })
    );
    assert_eq!(model.screen(), Screen::JellyfinItems);
    assert_eq!(model.jellyfin_items_load(), &LoadState::Loading);
    assert_eq!(model.jellyfin_library_name(), "Movies");
}

#[test]
fn grid_focus_uses_the_column_count_and_wraps_at_every_edge() {
    // Fresh request: no previous selection exists, so the first tile wins.
    let mut model = UiModel::new();
    model.set_home_focus(HomeRow::Jellyfin.index());
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Browse(UiBrowseCommand::LoadLibraries)
    );
    model.apply_browse_update(&BrowseUpdate::LibrariesReady {
        libraries: (0..5)
            .map(|index| library(&format!("library-{index}"), &format!("Library {index}")))
            .collect(),
    });
    assert_eq!(model.jellyfin_library_focus(), 0);

    press(&mut model, Key::Right, 0);
    assert_eq!(model.jellyfin_library_focus(), 1);
    for (index, millis) in [(2, 200), (3, 400), (4, 600)] {
        press(&mut model, Key::Right, millis);
        assert_eq!(model.jellyfin_library_focus(), index);
    }
    // Right on the last slot wraps to the first tile.
    press(&mut model, Key::Right, 800);
    assert_eq!(model.jellyfin_library_focus(), 0);
    // Up from the first row wraps to the last row, same column.
    press(&mut model, Key::Up, 1000);
    assert_eq!(model.jellyfin_library_focus(), 4);
    // Down from the last row wraps to the first row.
    press(&mut model, Key::Down, 1200);
    assert_eq!(model.jellyfin_library_focus(), 0);

    // The poster grid has five columns and the same wrapping rules.
    assert!(matches!(
        open_items(&mut model),
        UiOutcome::Browse(UiBrowseCommand::LoadItems { .. })
    ));
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-0".to_owned(),
        items: items(6, 0, false),
        total: 6,
        next_start: None,
    });
    assert_eq!(model.jellyfin_item_focus(), 0);
    press(&mut model, Key::Down, 1400);
    assert_eq!(model.jellyfin_item_focus(), JELLYFIN_POSTER_COLUMNS);
    press(&mut model, Key::Down, 1600);
    assert_eq!(model.jellyfin_item_focus(), 0);
    press(&mut model, Key::Right, 1800);
    assert_eq!(model.jellyfin_item_focus(), 1);
    assert_eq!(JELLYFIN_LIBRARY_COLUMNS, 4);
}

#[test]
fn library_focus_follows_the_identifier_across_a_refresh() {
    let mut model = open_library_grid();
    assert!(model.select_jellyfin_library(1));
    let selected = model.jellyfin_libraries()[1].id.clone();
    model.apply_browse_update(&BrowseUpdate::LibrariesReady {
        libraries: vec![
            library("library-0", "Music"),
            library(&selected, "Shows"),
            library("library-1", "Movies"),
        ],
    });
    assert_eq!(model.jellyfin_library_focus(), 1);
    assert_eq!(
        model.jellyfin_selected_library_id(),
        Some(selected.as_str())
    );
    assert_eq!(model.jellyfin_libraries()[1].name, "Shows");

    // If the selected library disappears, the old index is clamped.
    model.apply_browse_update(&BrowseUpdate::LibrariesReady {
        libraries: vec![library("library-1", "Movies")],
    });
    assert_eq!(model.jellyfin_library_focus(), 0);
    assert_eq!(model.jellyfin_libraries()[0].id, "library-1");
}

#[test]
fn items_ready_requests_bounded_posters_and_more_is_an_explicit_page() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: items(24, 0, true),
        total: 100,
        next_start: Some(24),
    });

    let requests = model.pending_browse_requests().to_vec();
    assert_eq!(
        requests.len(),
        24,
        "one poster request per item on a 24-item page"
    );
    assert!(requests.iter().all(|request| matches!(
        request,
        UiBrowseCommand::LoadArtwork {
            class: ArtworkClass::Poster,
            ..
        }
    )));
    assert!(
        !requests
            .iter()
            .any(|request| matches!(request, UiBrowseCommand::LoadItems { .. })),
        "a ready page must not fetch the next page by itself"
    );
    assert!(model.jellyfin_next_page_available());
    assert_eq!(model.jellyfin_items().len(), 24);
    assert_eq!(model.jellyfin_total(), 100);
    assert_eq!(model.jellyfin_next_start(), Some(24));
    assert_eq!(model.jellyfin_selected_item_id(), Some("movie-0"));

    model.take_browse_requests();
    assert!(model.select_jellyfin_more());
    assert_eq!(model.jellyfin_item_focus(), 24);
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Browse(UiBrowseCommand::LoadItems {
            library_id: "library-1".to_owned(),
            collection_type: Some("movies".to_owned()),
            start_index: 24,
        })
    );

    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: items(24, 24, true),
        total: 100,
        next_start: Some(48),
    });
    assert_eq!(model.jellyfin_items().len(), 48);
    assert_eq!(
        model.pending_browse_requests().len(),
        24,
        "the second page requests only its own posters"
    );
}

#[test]
fn retained_items_are_bounded_to_sixty_four() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    for page in 0..3_u32 {
        let start = page * 24;
        model.apply_browse_update(&BrowseUpdate::ItemsReady {
            library_id: "library-1".to_owned(),
            items: items(24, start as usize, false),
            total: 100,
            next_start: Some(start + 24),
        });
    }
    assert_eq!(model.jellyfin_items().len(), MAX_LIBRARY_ITEMS);
    assert!(
        !model.jellyfin_next_page_available(),
        "no More tile once the retention bound is reached"
    );
}

#[test]
fn libraries_are_bounded_to_the_public_limit() {
    let mut model = open_library_grid();
    model.apply_browse_update(&BrowseUpdate::LibrariesReady {
        libraries: (0..40)
            .map(|index| library(&format!("library-{index}"), &format!("Library {index}")))
            .collect(),
    });
    assert_eq!(model.jellyfin_libraries().len(), MAX_JELLYFIN_LIBRARIES);
}

#[test]
fn escape_moves_back_exactly_one_browse_level() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    assert_eq!(
        press(&mut model, Key::Escape, 0),
        UiOutcome::Navigated {
            screen: Screen::JellyfinLibraries,
            focus: 0,
        }
    );
    assert_eq!(model.screen(), Screen::JellyfinLibraries);
    assert_eq!(
        press(&mut model, Key::Escape, 200),
        UiOutcome::Navigated {
            screen: Screen::Home,
            focus: 1,
        }
    );
    assert_eq!(model.screen(), Screen::Home);
}

#[test]
fn items_failure_has_a_retry_target_for_the_same_page() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    model.apply_browse_update(&BrowseUpdate::ItemsFailed {
        library_id: "library-1".to_owned(),
        message: "upstream timeout".to_owned(),
    });
    assert_eq!(
        model.jellyfin_items_load(),
        &LoadState::Error {
            message: "upstream timeout".to_owned(),
        }
    );
    assert_eq!(
        model.retry_jellyfin_items(),
        UiOutcome::Browse(UiBrowseCommand::LoadItems {
            library_id: "library-1".to_owned(),
            collection_type: Some("movies".to_owned()),
            start_index: 0,
        })
    );
    assert_eq!(model.jellyfin_items_load(), &LoadState::Loading);

    // A failed continuation page retries from that page's start index.
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: items(24, 0, false),
        total: 100,
        next_start: Some(24),
    });
    assert!(model.select_jellyfin_more());
    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Browse(UiBrowseCommand::LoadItems {
            library_id: "library-1".to_owned(),
            collection_type: Some("movies".to_owned()),
            start_index: 24,
        })
    );
    model.apply_browse_update(&BrowseUpdate::ItemsFailed {
        library_id: "library-1".to_owned(),
        message: "page failed".to_owned(),
    });
    assert_eq!(
        model.retry_jellyfin_items(),
        UiOutcome::Browse(UiBrowseCommand::LoadItems {
            library_id: "library-1".to_owned(),
            collection_type: Some("movies".to_owned()),
            start_index: 24,
        })
    );
}

#[test]
fn library_collection_type_is_forwarded_to_item_queries_and_retries() {
    let mut model = UiModel::new();
    model.set_home_focus(HomeRow::Jellyfin.index());
    let _ = press(&mut model, Key::Enter, 0);
    model.apply_browse_update(&BrowseUpdate::LibrariesReady {
        libraries: vec![LibraryEntry {
            id: "library-music".to_owned(),
            name: "Music".to_owned(),
            collection_type: Some("music".to_owned()),
        }],
    });

    assert_eq!(
        open_items(&mut model),
        UiOutcome::Browse(UiBrowseCommand::LoadItems {
            library_id: "library-music".to_owned(),
            collection_type: Some("music".to_owned()),
            start_index: 0,
        })
    );

    model.apply_browse_update(&BrowseUpdate::ItemsFailed {
        library_id: "library-music".to_owned(),
        message: "upstream timeout".to_owned(),
    });
    assert_eq!(
        model.retry_jellyfin_items(),
        UiOutcome::Browse(UiBrowseCommand::LoadItems {
            library_id: "library-music".to_owned(),
            collection_type: Some("music".to_owned()),
            start_index: 0,
        }),
        "a retry keeps the library's query shape"
    );
}

#[test]
fn preview_back_and_play_are_explicit_actions() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: vec![item("movie-0", "Movie 0", Some("tag-0"))],
        total: 1,
        next_start: None,
    });
    model.take_browse_requests();

    assert_eq!(
        press(&mut model, Key::Enter, 0),
        UiOutcome::Browse(UiBrowseCommand::LoadArtwork {
            item_id: "movie-0".to_owned(),
            tag: Some("tag-0".to_owned()),
            class: ArtworkClass::Preview,
        })
    );
    assert_eq!(model.screen(), Screen::Preview);
    assert_eq!(
        model.jellyfin_preview().map(|item| item.id.as_str()),
        Some("movie-0")
    );

    assert_eq!(
        press(&mut model, Key::Escape, 200),
        UiOutcome::Navigated {
            screen: Screen::JellyfinItems,
            focus: 0,
        }
    );
    assert_eq!(model.screen(), Screen::JellyfinItems);

    // Enter opens the preview again without a second preview fetch.
    assert!(matches!(
        press(&mut model, Key::Enter, 400),
        UiOutcome::Navigated {
            screen: Screen::Preview,
            focus: 0,
        }
    ));
    // The second action is Back.
    press(&mut model, Key::Right, 600);
    assert_eq!(model.jellyfin_preview_focus(), 1);
    assert_eq!(
        press(&mut model, Key::Enter, 800),
        UiOutcome::Navigated {
            screen: Screen::JellyfinItems,
            focus: 0,
        }
    );

    // Play is only emitted after an explicit Enter on the Play action.
    assert!(matches!(
        press(&mut model, Key::Enter, 1000),
        UiOutcome::Navigated {
            screen: Screen::Preview,
            ..
        }
    ));
    assert_eq!(
        model.jellyfin_preview_focus(),
        0,
        "reopened preview focuses Play"
    );
    assert_eq!(
        press(&mut model, Key::Enter, 1200),
        UiOutcome::PlayJellyfin {
            item_id: "movie-0".to_owned(),
        }
    );
    assert_eq!(model.screen(), Screen::Player);
    assert!(model.player_loading());
    assert_eq!(model.player_source_label(), "Jellyfin");
}

fn typed_item(id: &str, title: &str, item_type: &str) -> BrowseItem {
    BrowseItem {
        id: id.to_owned(),
        title: title.to_owned(),
        subtitle: item_type.to_owned(),
        overview: None,
        primary_tag: None,
        item_type: Some(item_type.to_owned()),
    }
}

fn preview_for(item_type: &str) -> UiModel {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: vec![typed_item("item-1", "Item", item_type)],
        total: 1,
        next_start: None,
    });
    let _ = press(&mut model, Key::Enter, 0);
    assert_eq!(model.screen(), Screen::Preview);
    model
}

#[test]
fn audio_and_book_previews_show_planned_state_and_never_play() {
    for (item_type, note) in [
        ("Audio", "Audio player planned (C015)"),
        ("MusicAlbum", "Audio player planned (C015)"),
        ("AudioBook", "Audio player planned (C015)"),
        ("Book", "Book reader planned (C016)"),
    ] {
        let mut model = preview_for(item_type);
        assert!(
            !model.jellyfin_preview_playable(),
            "{item_type} must not offer Play"
        );
        assert_eq!(
            model.jellyfin_preview_focus(),
            1,
            "{item_type} preview focuses the visible Back action"
        );
        assert_eq!(model.jellyfin_preview_planned_note(), Some(note));
        assert!(
            model.status_text().contains(note),
            "typed planned state is visible in the status"
        );

        // Asking the UI to focus Play is clamped to Back.
        model.set_jellyfin_preview_focus(0);
        assert_eq!(model.jellyfin_preview_focus(), 1);
        press(&mut model, Key::Right, 10);
        assert_eq!(model.jellyfin_preview_focus(), 1);

        let outcome = press(&mut model, Key::Enter, 20);
        assert_eq!(
            outcome,
            UiOutcome::Navigated {
                screen: Screen::JellyfinItems,
                focus: 0,
            },
            "{item_type} Enter must act on Back, not emit a fake Play"
        );
        assert_eq!(model.screen(), Screen::JellyfinItems);
    }
}

#[test]
fn music_videos_use_the_regular_video_play_path() {
    let mut model = preview_for("MusicVideo");
    assert!(model.jellyfin_preview_playable());
    assert_eq!(model.jellyfin_preview_focus(), 0);
    assert_eq!(model.jellyfin_preview_planned_note(), None);
    assert_eq!(
        press(&mut model, Key::Enter, 10),
        UiOutcome::PlayJellyfin {
            item_id: "item-1".to_owned(),
        }
    );
    assert_eq!(model.screen(), Screen::Player);
}

#[test]
fn missing_or_failed_artwork_never_fails_the_page() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: vec![
            item("movie-0", "No Art", None),
            item("movie-1", "Broken Art", Some("bad-tag")),
        ],
        total: 2,
        next_start: None,
    });
    assert_eq!(model.jellyfin_items_load(), &LoadState::Ready);
    assert_eq!(
        model.pending_browse_requests().len(),
        1,
        "only the tagged item is fetched; a tagless item is a placeholder"
    );
    assert_eq!(
        model.pending_browse_requests()[0],
        UiBrowseCommand::LoadArtwork {
            item_id: "movie-1".to_owned(),
            tag: Some("bad-tag".to_owned()),
            class: ArtworkClass::Poster,
        }
    );
    assert_eq!(model.jellyfin_poster_path("movie-0"), None);

    model.apply_browse_update(&BrowseUpdate::ArtworkFailed {
        item_id: "movie-1".to_owned(),
        class: ArtworkClass::Poster,
        message: "decode failed".to_owned(),
    });
    model.apply_browse_update(&BrowseUpdate::ArtworkMissing {
        item_id: "movie-2".to_owned(),
        class: ArtworkClass::Poster,
    });
    assert_eq!(model.jellyfin_items_load(), &LoadState::Ready);
    assert_eq!(model.jellyfin_poster_path("movie-1"), None);
}

#[test]
fn session_expiry_clears_browse_metadata_and_artwork_epoch() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: vec![item("movie-0", "Movie 0", Some("tag-0"))],
        total: 1,
        next_start: None,
    });
    let _ = press(&mut model, Key::Enter, 0);
    assert_eq!(model.screen(), Screen::Preview);
    let epoch = model.artwork_epoch();

    model.apply_browse_update(&BrowseUpdate::SessionExpired);
    assert_eq!(model.screen(), Screen::JellyfinLibraries);
    assert_eq!(
        model.jellyfin_libraries_load(),
        &LoadState::Error {
            message: SESSION_EXPIRED_MESSAGE.to_owned(),
        }
    );
    assert!(model.jellyfin_libraries().is_empty());
    assert!(model.jellyfin_items().is_empty());
    assert!(model.jellyfin_preview().is_none());
    assert!(model.artwork_epoch() > epoch);
    assert!(model.status_text().contains("expired"));
}

#[test]
fn browse_models_never_contain_urls_tokens_or_stream_paths() {
    let mut model = open_library_grid();
    let _ = open_items(&mut model);
    let page = items(2, 0, true);
    model.apply_browse_update(&BrowseUpdate::ItemsReady {
        library_id: "library-1".to_owned(),
        items: page.clone(),
        total: 2,
        next_start: None,
    });
    let rendered = format!(
        "{:?} {:?} {:?}",
        model.jellyfin_libraries(),
        model.jellyfin_items(),
        model.pending_browse_requests()
    );
    for forbidden in ["http://", "https://", "Token=", "Bearer", "api_key"] {
        assert!(
            !rendered.contains(forbidden),
            "browse presentation model leaked {forbidden}: {rendered}"
        );
    }
    assert_eq!(page[0].id, "movie-0");
}
