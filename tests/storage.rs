//! Persistence, migration, cache-bound, permission and recovery tests.

use std::fs;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;

use tempfile::TempDir;
use tv_client::model::Source;
use tv_client::storage::{
    HARD_ARTWORK_CACHE_BYTES, LastView, MAX_PREFERENCE_VALUE_BYTES, PROGRESS_JOURNAL_LIMIT,
    SCHEMA_VERSION, Storage, StorageError,
};

fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("temporary directory")
}

fn open(dir: &TempDir, artwork_cap: u64) -> Storage {
    Storage::open(dir.path(), artwork_cap).expect("storage should open")
}

#[test]
fn schema_migration_is_idempotent_and_records_the_version() {
    let dir = temp_dir();
    {
        let storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
        assert_eq!(storage.schema_version().expect("version"), SCHEMA_VERSION);
    }
    {
        let storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
        assert_eq!(storage.schema_version().expect("version"), SCHEMA_VERSION);
    }
}

#[test]
fn newer_schema_is_rejected_without_moving_the_database() {
    let dir = temp_dir();
    let database = dir.path().join("client.db");
    {
        let connection = rusqlite::Connection::open(&database).expect("future database");
        connection
            .pragma_update(None, "user_version", 999_i64)
            .expect("set version");
    }

    let error = match Storage::open(dir.path(), HARD_ARTWORK_CACHE_BYTES) {
        Ok(_) => panic!("newer schema should be rejected"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        StorageError::UnsupportedSchema {
            found: 999,
            supported,
        } if supported == SCHEMA_VERSION
    ));
    assert!(database.exists(), "unsupported schema must not be moved");
}

#[test]
fn corrupt_database_is_moved_only_by_explicit_recovery() {
    let dir = temp_dir();
    let database = dir.path().join("client.db");
    fs::write(&database, vec![0xAB_u8; 4096]).expect("write corrupt database");

    let result = Storage::open(dir.path(), HARD_ARTWORK_CACHE_BYTES);
    assert!(matches!(result, Err(StorageError::CorruptDatabase)));
    assert!(
        database.exists(),
        "opening must not move a corrupt database"
    );

    let recovered = Storage::recover_corrupt(dir.path()).expect("recovery");
    assert!(recovered.exists());
    assert!(!database.exists());

    let storage = Storage::open(dir.path(), HARD_ARTWORK_CACHE_BYTES).expect("fresh database");
    assert_eq!(storage.schema_version().expect("version"), SCHEMA_VERSION);
}

#[test]
fn recovery_refuses_a_healthy_database_and_reports_missing_files() {
    let healthy_dir = temp_dir();
    let healthy = open(&healthy_dir, HARD_ARTWORK_CACHE_BYTES);
    drop(healthy);

    assert!(matches!(
        Storage::recover_corrupt(healthy_dir.path()),
        Err(StorageError::DatabaseNotCorrupt)
    ));
    assert!(healthy_dir.path().join("client.db").exists());

    let empty_dir = temp_dir();
    assert!(matches!(
        Storage::recover_corrupt(empty_dir.path()),
        Err(StorageError::NoCorruptDatabase)
    ));
}

#[cfg(unix)]
#[test]
fn state_and_cache_directories_are_private() {
    let dir = temp_dir();
    let storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);

    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(dir.path()), 0o700);
    assert_eq!(mode(&dir.path().join("credentials")), 0o700);
    assert_eq!(mode(&dir.path().join("artwork")), 0o700);
    assert_eq!(mode(&dir.path().join("client.db")), 0o600);
    assert_eq!(storage.state_directory(), dir.path());
}

#[cfg(unix)]
#[test]
fn credentials_are_atomic_private_and_permission_checked() {
    let dir = temp_dir();
    let storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
    let path = storage.credential_path("device").expect("credential path");

    storage
        .write_credential("device", b"first-token")
        .expect("write credential");
    let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        storage
            .read_credential("device")
            .expect("read credential")
            .as_slice(),
        b"first-token".as_slice()
    );

    storage
        .write_credential("device", b"rotated-token")
        .expect("overwrite credential");
    assert_eq!(
        storage
            .read_credential("device")
            .expect("read rotated")
            .as_slice(),
        b"rotated-token".as_slice()
    );

    assert!(matches!(
        storage.write_credential("../escape", b"x"),
        Err(StorageError::InvalidCredential)
    ));
    assert!(matches!(
        storage.write_credential("device", b""),
        Err(StorageError::InvalidCredential)
    ));
    assert!(matches!(
        storage.read_credential("missing"),
        Err(StorageError::CredentialNotFound)
    ));

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("broaden mode");
    assert!(matches!(
        storage.read_credential("device"),
        Err(StorageError::InsecurePermissions)
    ));
}

#[test]
fn artwork_cache_evicts_least_recently_used_and_respects_the_cap() {
    let dir = temp_dir();
    let mut storage = open(&dir, 8);
    assert_eq!(storage.artwork_cap_bytes(), 8);

    storage.store_artwork("a", b"aaaa").expect("store a");
    storage.store_artwork("b", b"bbbb").expect("store b");
    let a_path = storage
        .artwork_path("a")
        .expect("lookup a")
        .expect("a should exist");
    assert!(a_path.is_file());

    storage.store_artwork("c", b"cccc").expect("store c");
    assert_eq!(storage.artwork_total_bytes().expect("total bytes"), 8);
    assert!(
        storage.artwork_path("b").expect("lookup b").is_none(),
        "least recently used entry should be evicted"
    );
    assert!(storage.artwork_path("a").expect("lookup a").is_some());
    assert!(storage.artwork_path("c").expect("lookup c").is_some());
    assert_eq!(
        fs::read_dir(dir.path().join("artwork"))
            .expect("artwork directory")
            .count(),
        2
    );
}

#[test]
fn artwork_filenames_are_generated_uuids_independent_of_the_key() {
    let dir = temp_dir();
    let mut storage = open(&dir, 1_024);
    let first_key = "k".repeat(128);
    let second_key = "m".repeat(128);
    for key in [&first_key, &second_key] {
        storage.store_artwork(key, b"data").expect("store long key");
    }

    let first_path = storage
        .artwork_path(&first_key)
        .expect("lookup first")
        .expect("first artwork");
    let second_path = storage
        .artwork_path(&second_key)
        .expect("lookup second")
        .expect("second artwork");
    assert!(first_path.is_file());
    assert!(second_path.is_file());

    let first_name = first_path
        .file_name()
        .expect("first file name")
        .to_string_lossy()
        .into_owned();
    let second_name = second_path
        .file_name()
        .expect("second file name")
        .to_string_lossy()
        .into_owned();
    assert_ne!(
        first_name, second_name,
        "different long keys need different files"
    );
    for name in [&first_name, &second_name] {
        assert!(
            name.len() <= 40,
            "generated artwork file name must stay compact: {name}"
        );
        assert!(
            !name.contains(first_key.as_str()) && !name.contains(second_key.as_str()),
            "generated artwork file name must not embed the cache key: {name}"
        );
        let Some(stem) = name.strip_suffix(".bin") else {
            panic!("artwork file name should keep the .bin suffix: {name}");
        };
        assert!(
            uuid::Uuid::parse_str(stem).is_ok(),
            "artwork file name should be a generated UUID, not the cache key: {name}"
        );
    }
}

#[test]
fn failed_artwork_index_write_leaves_no_temp_file() {
    let dir = temp_dir();
    let mut storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
    {
        let connection =
            rusqlite::Connection::open(dir.path().join("client.db")).expect("second connection");
        connection
            .execute_batch(
                "CREATE TRIGGER fail_artwork_insert BEFORE INSERT ON artwork_index
                 BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
            )
            .expect("install failure trigger");
    }

    let error = storage
        .store_artwork("cache-key", b"payload")
        .expect_err("index failure should fail the store");
    assert!(matches!(error, StorageError::Database));
    assert_eq!(storage.artwork_total_bytes().expect("total"), 0);
    assert_eq!(
        fs::read_dir(dir.path().join("artwork"))
            .expect("artwork directory")
            .count(),
        0,
        "a failed store must not leave a temporary or final artwork file"
    );
}

#[test]
fn open_reconciles_artwork_orphans_and_missing_index_rows() {
    let dir = temp_dir();
    let mut storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
    storage
        .store_artwork("kept", b"kept-bytes")
        .expect("store kept");
    storage
        .store_artwork("lost", b"lost-bytes")
        .expect("store lost");
    let lost_path = storage
        .artwork_path("lost")
        .expect("lookup lost")
        .expect("lost artwork");
    drop(storage);
    fs::remove_file(&lost_path).expect("remove indexed file");

    let artwork = dir.path().join("artwork");
    fs::write(artwork.join("orphan.bin"), b"orphan").expect("plant orphan file");
    fs::write(artwork.join(".tmp-stale"), b"stale").expect("plant stale temporary file");

    let mut storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
    assert_eq!(
        storage.artwork_total_bytes().expect("total"),
        "kept-bytes".len() as u64,
        "the missing file's index row must be dropped"
    );
    assert!(
        !artwork.join("orphan.bin").exists(),
        "unindexed files must be removed on open"
    );
    assert!(
        !artwork.join(".tmp-stale").exists(),
        "stale temporary files must be removed on open"
    );
    assert!(storage.artwork_path("kept").expect("lookup kept").is_some());
    assert!(storage.artwork_path("lost").expect("lookup lost").is_none());
    assert_eq!(
        fs::read_dir(&artwork).expect("artwork directory").count(),
        1,
        "only the reconciled, indexed file should remain"
    );
}

#[test]
fn open_evicts_indexed_artwork_over_the_configured_cap() {
    let dir = temp_dir();
    let mut storage = open(&dir, 1_024);
    storage.store_artwork("a", b"aaaa").expect("store a");
    storage.store_artwork("b", b"bbbb").expect("store b");
    drop(storage);

    let mut storage = open(&dir, 4);
    assert_eq!(storage.artwork_total_bytes().expect("total"), 4);
    assert!(
        storage.artwork_path("a").expect("lookup a").is_none(),
        "the least recently used entry must be evicted on open"
    );
    assert!(storage.artwork_path("b").expect("lookup b").is_some());
    assert_eq!(
        fs::read_dir(dir.path().join("artwork"))
            .expect("artwork directory")
            .count(),
        1,
        "on-disk artwork must respect the cap after reconciliation"
    );
}

#[test]
fn artwork_cap_is_clamped_and_oversized_payloads_are_rejected() {
    let dir = temp_dir();
    let storage = open(&dir, u64::MAX);
    assert_eq!(storage.artwork_cap_bytes(), 128 * 1024 * 1024);
    drop(storage);

    let mut small = open(&dir, 4);
    assert!(matches!(
        small.store_artwork("too-big", b"12345"),
        Err(StorageError::ArtworkTooLarge)
    ));
    assert_eq!(small.artwork_total_bytes().expect("total"), 0);
    assert!(matches!(
        small.store_artwork("", b"1"),
        Err(StorageError::InvalidArtwork)
    ));
}

#[test]
fn progress_journal_is_bounded_and_newest_first() {
    let dir = temp_dir();
    let mut storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);

    for index in 0..300_u32 {
        storage
            .record_progress(&format!("item-{index}"), 100, 1_000)
            .expect("record progress");
    }
    assert_eq!(
        storage.progress_count().expect("count"),
        PROGRESS_JOURNAL_LIMIT
    );

    let newest = storage.recent_progress(1).expect("newest");
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].item_id.as_str(), "item-299");
    assert_eq!(newest[0].position_ms, 100);
    assert_eq!(newest[0].duration_ms, 1_000);

    let all = storage.recent_progress(10_000).expect("all");
    assert_eq!(all.len(), PROGRESS_JOURNAL_LIMIT);
    assert_eq!(all.first().expect("first").item_id.as_str(), "item-299");
    assert_eq!(all.last().expect("last").item_id.as_str(), "item-44");

    assert!(matches!(
        storage.record_progress("item-invalid", 2_000, 1_000),
        Err(StorageError::InvalidProgress)
    ));
}

#[test]
fn preferences_hold_non_secret_values_only() {
    let dir = temp_dir();
    let mut storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);

    storage
        .set_preference("theme", "dark")
        .expect("set preference");
    assert_eq!(
        storage.get_preference("theme").expect("get preference"),
        Some("dark".to_owned())
    );
    storage
        .set_preference("theme", "light")
        .expect("replace preference");
    assert_eq!(
        storage.get_preference("theme").expect("get preference"),
        Some("light".to_owned())
    );
    storage
        .delete_preference("theme")
        .expect("delete preference");
    assert_eq!(storage.get_preference("theme").expect("get"), None);

    for key in ["device_token", "password", "api_secret", "authorization"] {
        assert!(
            matches!(
                storage.set_preference(key, "value"),
                Err(StorageError::SecretNotAllowed)
            ),
            "{key} should be rejected"
        );
    }
    assert!(matches!(
        storage.set_preference("bad key", "value"),
        Err(StorageError::InvalidPreference)
    ));
    let oversized = "x".repeat(MAX_PREFERENCE_VALUE_BYTES + 1);
    assert!(matches!(
        storage.set_preference("note", &oversized),
        Err(StorageError::InvalidPreference)
    ));
}

#[test]
fn last_view_round_trips_metadata_without_an_offline_playback_claim() {
    let dir = temp_dir();
    let view = LastView {
        item_id: "episode-1".to_owned(),
        title: "Pilot".to_owned(),
        source: Some(Source::Jellyfin),
        position_ms: 1_000,
        duration_ms: Some(2_000),
    };
    {
        let mut storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
        storage.save_last_view(&view).expect("save last view");
    }

    let mut storage = open(&dir, HARD_ARTWORK_CACHE_BYTES);
    let loaded = storage
        .last_view()
        .expect("load last view")
        .expect("view should exist");
    assert_eq!(loaded, view);
    assert!(!loaded.is_playable_offline());
    assert!(
        !storage
            .last_view()
            .expect("second load")
            .expect("view")
            .is_playable_offline()
    );

    assert!(matches!(
        storage.save_last_view(&LastView {
            position_ms: 2_001,
            ..view
        }),
        Err(StorageError::InvalidLastView)
    ));
    assert!(matches!(
        storage.save_last_view(&LastView {
            item_id: "https://example.test/item".to_owned(),
            title: "bad".to_owned(),
            source: None,
            position_ms: 0,
            duration_ms: None,
        }),
        Err(StorageError::InvalidLastView)
    ));
}
