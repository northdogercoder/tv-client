//! Reducer, queue, cancellation and configuration contract tests.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tv_client::config::{self, ClientConfig, ConfigError};
use tv_client::model::{Action, Direction, Effect, Event, InputOrigin, Mode, Source, StateError};
use tv_client::state::{Reducer, event_channel, run_state_loop, shutdown_channel};
use uuid::Uuid;

type ReducerFactory = Box<dyn Fn() -> Reducer>;

fn play_jellyfin(reducer: &mut Reducer) -> (u64, Uuid) {
    let effects = reducer
        .apply(Event::action(
            InputOrigin::LocalKeyboard,
            Action::PlayJellyfin {
                item_id: "item-1".to_owned(),
            },
        ))
        .expect("jellyfin playback should start");
    assert_eq!(reducer.state().mode, Mode::Loading);
    assert_eq!(reducer.state().source, Some(Source::Jellyfin));
    assert!(matches!(effects.as_slice(), [Effect::StartPlayback { .. }]));
    let playback = reducer
        .state()
        .playback
        .as_ref()
        .expect("playback should be active");
    (playback.generation, playback.id)
}

fn begin_playing(reducer: &mut Reducer) -> (u64, Uuid) {
    let (generation, playback_id) = play_jellyfin(reducer);
    reducer
        .apply(Event::PlaybackStarted {
            generation,
            playback_id,
        })
        .expect("player should admit playback");
    assert_eq!(reducer.state().mode, Mode::Playing);
    (generation, playback_id)
}

#[test]
fn playback_lifecycle_produces_typed_effects() {
    let mut reducer = Reducer::new();
    let (generation, playback_id) = begin_playing(&mut reducer);
    assert_eq!(reducer.generation(), generation);
    assert_eq!(reducer.state().position_ms, Some(0));

    let effects = reducer
        .apply(Event::PlaybackProgress {
            generation,
            playback_id,
            position_ms: 1_000,
            duration_ms: Some(5_000),
        })
        .expect("progress should be accepted");
    assert_eq!(reducer.state().position_ms, Some(1_000));
    assert_eq!(reducer.state().duration_ms, Some(5_000));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Progress {
            position_ms: 1_000,
            duration_ms: Some(5_000),
            ..
        }]
    ));

    let effects = reducer
        .apply(Event::action(
            InputOrigin::LocalKeyboard,
            Action::SetPaused { paused: true },
        ))
        .expect("pause should be accepted");
    assert_eq!(reducer.state().mode, Mode::Paused);
    assert!(reducer.state().paused);
    assert!(matches!(
        effects.as_slice(),
        [Effect::SetPaused { paused: true, .. }]
    ));

    let effects = reducer
        .apply(Event::action(
            InputOrigin::Remote,
            Action::Seek { position_ms: 2_000 },
        ))
        .expect("seek should be accepted while paused");
    assert_eq!(reducer.state().position_ms, Some(2_000));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Seek {
            position_ms: 2_000,
            ..
        }]
    ));

    reducer
        .apply(Event::action(
            InputOrigin::Remote,
            Action::SetPaused { paused: false },
        ))
        .expect("resume should be accepted");
    assert_eq!(reducer.state().mode, Mode::Playing);
    assert!(!reducer.state().paused);

    let effects = reducer
        .apply(Event::action(
            InputOrigin::Remote,
            Action::SetVolume { volume: 42 },
        ))
        .expect("remote volume should be accepted");
    assert_eq!(reducer.state().volume, 42);
    assert!(matches!(
        effects.as_slice(),
        [Effect::SetVolume { volume: 42 }]
    ));

    let effects = reducer
        .apply(Event::action(InputOrigin::LocalCec, Action::Stop))
        .expect("stop should be accepted");
    assert_eq!(reducer.state().mode, Mode::Home);
    assert!(reducer.state().playback.is_none());
    assert_eq!(reducer.state().source, None);
    assert!(matches!(effects.as_slice(), [Effect::StopPlayback { .. }]));
}

#[test]
fn progress_without_a_duration_is_bounded_by_the_stored_duration() {
    let mut reducer = Reducer::new();
    let (generation, playback_id) = begin_playing(&mut reducer);
    reducer
        .apply(Event::PlaybackProgress {
            generation,
            playback_id,
            position_ms: 1_000,
            duration_ms: Some(5_000),
        })
        .expect("initial progress should store the duration");
    assert_eq!(reducer.state().duration_ms, Some(5_000));

    let error = reducer
        .apply(Event::PlaybackProgress {
            generation,
            playback_id,
            position_ms: 6_000,
            duration_ms: None,
        })
        .expect_err("position beyond the stored duration must be rejected");
    assert!(matches!(
        error,
        StateError::InvalidValue {
            field: "position_ms",
            ..
        }
    ));
    assert_eq!(reducer.state().position_ms, Some(1_000));
    assert_eq!(reducer.state().duration_ms, Some(5_000));

    let effects = reducer
        .apply(Event::PlaybackProgress {
            generation,
            playback_id,
            position_ms: 4_000,
            duration_ms: None,
        })
        .expect("position within the stored duration should be accepted");
    assert_eq!(reducer.state().position_ms, Some(4_000));
    assert_eq!(reducer.state().duration_ms, Some(5_000));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Progress {
            position_ms: 4_000,
            duration_ms: Some(5_000),
            ..
        }]
    ));
}

#[test]
fn replacement_playback_uses_a_new_generation_and_stops_the_old_one() {
    let mut reducer = Reducer::new();
    let (first_generation, first_id) = begin_playing(&mut reducer);
    let effects = reducer
        .apply(Event::action(
            InputOrigin::LocalKeyboard,
            Action::PlayJellyfin {
                item_id: "item-2".to_owned(),
            },
        ))
        .expect("replacement should start");
    let playback = reducer
        .state()
        .playback
        .as_ref()
        .expect("replacement playback should be active");
    assert_eq!(playback.generation, first_generation + 1);
    assert_eq!(reducer.state().mode, Mode::Loading);

    let [stop, start] = effects.as_slice() else {
        panic!("expected stop and start effects, got {effects:?}");
    };
    assert!(matches!(
        stop,
        Effect::StopPlayback {
            generation,
            playback_id,
        } if *generation == first_generation && *playback_id == first_id
    ));
    assert!(matches!(
        start,
        Effect::StartPlayback { generation, .. } if *generation == first_generation + 1
    ));

    let stale = reducer
        .apply(Event::PlaybackProgress {
            generation: first_generation,
            playback_id: first_id,
            position_ms: 1,
            duration_ms: None,
        })
        .expect_err("older generation must be stale");
    assert!(matches!(
        stale,
        StateError::StaleGeneration {
            expected,
            received,
        } if expected == first_generation + 1 && received == first_generation
    ));

    let current_id = reducer.state().playback.as_ref().expect("playback").id;
    let stale_id = reducer
        .apply(Event::PlaybackProgress {
            generation: first_generation + 1,
            playback_id: first_id,
            position_ms: 1,
            duration_ms: None,
        })
        .expect_err("older playback id must be stale");
    assert!(matches!(
        stale_id,
        StateError::StalePlayback {
            expected,
            received,
        } if expected == current_id && received == first_id
    ));
}

#[test]
fn invalid_actions_are_rejected_before_state_changes() {
    let cases = [
        ("volume above 100", Action::SetVolume { volume: 101 }),
        (
            "youtube id too short",
            Action::PlayYoutube {
                video_id: "short".to_owned(),
            },
        ),
        (
            "youtube id too long",
            Action::PlayYoutube {
                video_id: "M7lc1UVf-VEX".to_owned(),
            },
        ),
        (
            "youtube id invalid character",
            Action::PlayYoutube {
                video_id: "M7lc1UVf-V.".to_owned(),
            },
        ),
        (
            "empty jellyfin id",
            Action::PlayJellyfin {
                item_id: String::new(),
            },
        ),
        (
            "oversized jellyfin id",
            Action::PlayJellyfin {
                item_id: "a".repeat(129),
            },
        ),
        (
            "jellyfin id as URL",
            Action::PlayJellyfin {
                item_id: "https://example.test/item".to_owned(),
            },
        ),
    ];

    for (name, action) in cases {
        let mut reducer = Reducer::new();
        let error = reducer
            .apply(Event::action(InputOrigin::LocalKeyboard, action))
            .expect_err(name);
        assert!(
            matches!(&error, StateError::InvalidAction { .. }),
            "{name}: {error:?}"
        );
        assert_eq!(reducer.state().mode, Mode::Home, "{name}");
        assert!(reducer.state().playback.is_none(), "{name}");
    }
}

#[test]
fn illegal_transitions_are_rejected() {
    let cases: [(&str, ReducerFactory, Event, Mode); 6] = [
        (
            "pause while loading",
            Box::new(|| {
                let mut reducer = Reducer::new();
                play_jellyfin(&mut reducer);
                reducer
            }),
            Event::action(
                InputOrigin::LocalKeyboard,
                Action::SetPaused { paused: true },
            ),
            Mode::Loading,
        ),
        (
            "seek while loading",
            Box::new(|| {
                let mut reducer = Reducer::new();
                play_jellyfin(&mut reducer);
                reducer
            }),
            Event::action(InputOrigin::LocalKeyboard, Action::Seek { position_ms: 1 }),
            Mode::Loading,
        ),
        (
            "navigate while in standby",
            Box::new(|| {
                let mut reducer = Reducer::new();
                reducer
                    .apply(Event::action(
                        InputOrigin::LocalKeyboard,
                        Action::Standby { enabled: true },
                    ))
                    .expect("standby");
                reducer
            }),
            Event::action(
                InputOrigin::LocalKeyboard,
                Action::Navigate {
                    direction: Direction::Up,
                },
            ),
            Mode::Standby,
        ),
        (
            "play while in standby",
            Box::new(|| {
                let mut reducer = Reducer::new();
                reducer
                    .apply(Event::action(
                        InputOrigin::LocalKeyboard,
                        Action::Standby { enabled: true },
                    ))
                    .expect("standby");
                reducer
            }),
            Event::action(
                InputOrigin::LocalKeyboard,
                Action::PlayJellyfin {
                    item_id: "item-1".to_owned(),
                },
            ),
            Mode::Standby,
        ),
        (
            "stop while in standby",
            Box::new(|| {
                let mut reducer = Reducer::new();
                reducer
                    .apply(Event::action(
                        InputOrigin::LocalKeyboard,
                        Action::Standby { enabled: true },
                    ))
                    .expect("standby");
                reducer
            }),
            Event::action(InputOrigin::LocalKeyboard, Action::Stop),
            Mode::Standby,
        ),
        (
            "leave standby without enabling it",
            Box::new(Reducer::new),
            Event::action(
                InputOrigin::LocalKeyboard,
                Action::Standby { enabled: false },
            ),
            Mode::Home,
        ),
    ];

    for (name, make_reducer, event, expected_from) in cases {
        let mut reducer = make_reducer();
        let error = reducer.apply(event).expect_err(name);
        assert!(
            matches!(
                &error,
                StateError::IllegalTransition { from, .. } if *from == expected_from
            ),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn remote_physical_approval_is_always_rejected() {
    let request_id = Uuid::new_v4();
    let mut reducer = Reducer::new();

    let error = reducer
        .apply(Event::physical_approval(InputOrigin::Remote, request_id))
        .expect_err("remote approval must fail");
    assert_eq!(error, StateError::RemoteApprovalDenied);
    assert_eq!(reducer.state().mode, Mode::Home);

    let effects = reducer
        .apply(Event::physical_approval(
            InputOrigin::LocalKeyboard,
            request_id,
        ))
        .expect("local physical approval should be accepted");
    assert_eq!(effects, vec![Effect::ApprovePairing { request_id }]);

    let effects = reducer
        .apply(Event::physical_approval(InputOrigin::LocalCec, request_id))
        .expect("CEC physical approval should be accepted");
    assert_eq!(effects, vec![Effect::ApprovePairing { request_id }]);

    let error = reducer
        .apply(Event::physical_approval(
            InputOrigin::LocalKeyboard,
            Uuid::nil(),
        ))
        .expect_err("nil request must fail");
    assert!(matches!(
        error,
        StateError::InvalidValue {
            field: "request_id",
            ..
        }
    ));
}

#[test]
fn restart_does_not_resume_playback_or_accept_old_events() {
    let mut first_process = Reducer::new();
    let (generation, playback_id) = begin_playing(&mut first_process);
    assert!(first_process.state().playback.is_some());

    let mut restarted = Reducer::new();
    assert_eq!(restarted.state().mode, Mode::Home);
    assert!(restarted.state().playback.is_none());
    let error = restarted
        .apply(Event::PlaybackProgress {
            generation,
            playback_id,
            position_ms: 1,
            duration_ms: None,
        })
        .expect_err("old playback must not resume");
    assert_eq!(error, StateError::NoActivePlayback);

    let (new_generation, _) = begin_playing(&mut restarted);
    assert_eq!(new_generation, 1);
    assert!(restarted.state().playback.is_some());
}

#[test]
fn playback_failure_enters_error_mode_with_a_bounded_code() {
    let mut reducer = Reducer::new();
    let (generation, playback_id) = begin_playing(&mut reducer);
    let effects = reducer
        .apply(Event::PlaybackFailed {
            generation,
            playback_id,
            error_code: "decoder_unavailable".to_owned(),
        })
        .expect("failure should be accepted");
    assert_eq!(reducer.state().mode, Mode::Error);
    assert_eq!(
        reducer.state().error.as_deref(),
        Some("decoder_unavailable")
    );
    assert!(reducer.state().playback.is_none());
    assert!(matches!(effects.as_slice(), [Effect::StopPlayback { .. }]));

    let mut active = Reducer::new();
    let (generation, playback_id) = begin_playing(&mut active);
    let bad = active
        .apply(Event::PlaybackFailed {
            generation,
            playback_id,
            error_code: "bad\ncode".to_owned(),
        })
        .expect_err("control bytes in an error code must fail");
    assert!(matches!(
        bad,
        StateError::InvalidValue {
            field: "error_code",
            ..
        }
    ));
    assert_eq!(active.state().mode, Mode::Playing);
    assert!(active.state().playback.is_some());
}

#[tokio::test]
async fn event_queue_is_bounded_and_non_blocking() {
    assert!(matches!(
        event_channel(0),
        Err(StateError::InvalidQueueCapacity)
    ));

    let (sender, mut receiver) = event_channel(1).expect("queue");
    sender
        .try_send(Event::action(
            InputOrigin::LocalKeyboard,
            Action::SetVolume { volume: 10 },
        ))
        .expect("first event");
    assert!(matches!(
        sender.try_send(Event::action(
            InputOrigin::LocalKeyboard,
            Action::SetVolume { volume: 11 },
        )),
        Err(StateError::QueueFull)
    ));
    assert!(receiver.recv().await.is_some());
    drop(receiver);
    assert!(matches!(
        sender
            .send(Event::action(InputOrigin::Remote, Action::Stop))
            .await,
        Err(StateError::QueueClosed)
    ));
}

#[tokio::test]
async fn shutdown_is_cancellation_safe_and_preserves_applied_state() {
    let (sender, receiver) = event_channel(4).expect("queue");
    let (effect_sender, mut effect_receiver) = mpsc::channel(8);
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let task = tokio::spawn(run_state_loop(
        Reducer::new(),
        receiver,
        effect_sender,
        shutdown_signal,
    ));

    sender
        .send(Event::action(
            InputOrigin::LocalKeyboard,
            Action::PlayJellyfin {
                item_id: "item-1".to_owned(),
            },
        ))
        .await
        .expect("send event");
    let effect = timeout(Duration::from_secs(5), effect_receiver.recv())
        .await
        .expect("effect timer")
        .expect("effect channel");
    assert!(matches!(effect, Effect::StartPlayback { .. }));

    shutdown_handle.cancel();
    let reducer = timeout(Duration::from_secs(5), task)
        .await
        .expect("shutdown timer")
        .expect("join")
        .expect("state loop");
    assert_eq!(reducer.state().mode, Mode::Loading);
    assert_eq!(reducer.generation(), 1);
    assert!(reducer.state().playback.is_some());
}

#[test]
fn shipped_example_config_matches_the_documented_contract() {
    let config = ClientConfig::from_toml_str(include_str!("../deploy/client.toml.example"))
        .expect("example config should parse");
    assert_eq!(
        config.jellyfin_url.as_deref(),
        Some("https://jellyfin.YOUR-TAILNET.ts.net")
    );
    assert_eq!(
        config.server_url.as_deref(),
        Some("https://ovh-anders.minotaur-gacrux.ts.net")
    );
    assert!(!config.cloud_enabled);
    assert!(!config.youtube_enabled);
    assert!(!config.airplay_enabled);
    assert!(!config.webrtc_enabled);
    assert_eq!(config.max_width, 1280);
    assert_eq!(config.max_height, 720);
    assert_eq!(config.max_fps, 30);
    assert_eq!(config.bitrate_bps, 6_000_000);
    assert_eq!(config.max_bitrate_bps, 10_000_000);
    assert_eq!(config.artwork_cache_bytes, 134_217_728);
    assert!(!config.share_titles);
    assert_eq!(
        ClientConfig::default().artwork_cache_bytes,
        config::MAX_ARTWORK_CACHE_BYTES
    );
}

#[test]
fn missing_config_keys_use_documented_defaults() {
    let config = ClientConfig::from_toml_str("").expect("empty configuration should be valid");
    assert_eq!(config, ClientConfig::default());
    assert_eq!(config.jellyfin_url, None);
    assert_eq!(config.server_url, None);
    assert_eq!(config.max_width, 1280);
    assert_eq!(config.max_height, 720);
    assert_eq!(config.max_fps, 30);
    assert_eq!(config.bitrate_bps, 6_000_000);
    assert_eq!(config.max_bitrate_bps, 10_000_000);
    assert_eq!(config.artwork_cache_bytes, 134_217_728);
}

#[test]
fn unknown_config_keys_and_bad_values_are_rejected() {
    let error =
        ClientConfig::from_toml_str("unknown_key = 1").expect_err("unknown key must be rejected");
    assert!(matches!(error, ConfigError::InvalidToml { .. }));

    let value_cases = [
        ("max_width = 100", "max_width"),
        ("max_height = 100", "max_height"),
        ("max_fps = 0", "max_fps"),
        ("bitrate_bps = 1000", "bitrate_bps"),
        ("max_bitrate_bps = 1000", "max_bitrate_bps"),
        ("artwork_cache_bytes = 134217729", "artwork_cache_bytes"),
        ("jellyfin_url = \"http://example.com\"", "jellyfin_url"),
        ("server_url = \"http://127.0.0.1:8787\"", "server_url"),
        ("server_url = \"ftp://example.com\"", "server_url"),
        (
            "server_url = \"https://user:pass@example.com\"",
            "server_url",
        ),
    ];
    for (document, field) in value_cases {
        let error = ClientConfig::from_toml_str(document).expect_err(document);
        assert!(
            matches!(&error, ConfigError::InvalidValue { field: actual, .. } if *actual == field),
            "{document}: {error:?}"
        );
    }

    let missing_cases = [
        "cloud_enabled = true",
        "youtube_enabled = true",
        "webrtc_enabled = true",
    ];
    for document in missing_cases {
        let error = ClientConfig::from_toml_str(document).expect_err(document);
        assert!(
            matches!(
                &error,
                ConfigError::Missing { field: actual, .. } if *actual == "server_url"
            ),
            "{document}: {error:?}"
        );
    }

    let error = ClientConfig::from_toml_str("bitrate_bps = 9000000\nmax_bitrate_bps = 8000000")
        .expect_err("max bitrate below nominal bitrate must fail");
    assert!(matches!(
        error,
        ConfigError::InvalidValue {
            field: "max_bitrate_bps",
            ..
        }
    ));
}

#[test]
fn toml_type_errors_do_not_echo_the_supplied_value() {
    let error =
        ClientConfig::from_toml_str("max_width = \"https://user:supersecrettoken@example.test\"")
            .expect_err("a string value for an integer field must be rejected");
    assert!(matches!(&error, ConfigError::InvalidToml { .. }));
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(
            !rendered.contains("supersecrettoken"),
            "configuration errors must not echo values: {rendered}"
        );
        assert!(
            !rendered.contains("https://"),
            "configuration errors must not echo URLs: {rendered}"
        );
    }
}

#[test]
fn local_jellyfin_http_and_https_cloud_origins_are_accepted() {
    for document in [
        "jellyfin_url = \"http://jellyfin:8096\"",
        "jellyfin_url = \"http://127.0.0.1:8096\"",
        "jellyfin_url = \"http://192.168.1.20:8096\"",
        "jellyfin_url = \"http://[::1]:8096\"",
        "jellyfin_url = \"https://jellyfin.example.test\"",
    ] {
        ClientConfig::from_toml_str(document).expect(document);
    }

    let config = ClientConfig::from_toml_str(
        "server_url = \"https://tv-control.example.ts.net\"\ncloud_enabled = true",
    )
    .expect("https cloud origin should be accepted");
    assert!(config.cloud_enabled);
}
