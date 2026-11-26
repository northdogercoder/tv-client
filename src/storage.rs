//! Bounded local persistence: preferences, last view, artwork LRU and
//! credentials.
//!
//! All sensitive values live in separate 0600 files below a 0700 state
//! directory. SQLite stores only non-secret metadata. Opening a corrupt
//! database returns [`StorageError::CorruptDatabase`] without moving it;
//! [`Storage::recover_corrupt`] is the only operation that renames files aside.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{Source, is_valid_item_id};

/// Schema version created by this build.
pub const SCHEMA_VERSION: i64 = 1;
/// Absolute artwork-cache ceiling.
pub const HARD_ARTWORK_CACHE_BYTES: u64 = 128 * 1024 * 1024;
/// Number of newest progress rows retained.
pub const PROGRESS_JOURNAL_LIMIT: usize = 256;
/// Largest accepted preference value.
pub const MAX_PREFERENCE_VALUE_BYTES: usize = 4096;
/// Largest accepted credential payload.
pub const MAX_CREDENTIAL_BYTES: usize = 4096;
const MAX_CACHE_KEY_BYTES: usize = 128;
const MAX_TITLE_BYTES: usize = 512;
const DATABASE_FILE: &str = "client.db";

const SCHEMA_V1: &str = r"
CREATE TABLE preferences (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE last_view (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    item_id TEXT NOT NULL,
    title TEXT NOT NULL,
    source TEXT,
    position_ms INTEGER NOT NULL CHECK (position_ms >= 0),
    duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
    updated_at INTEGER NOT NULL
);
CREATE TABLE artwork_index (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    cache_key TEXT NOT NULL UNIQUE,
    file_name TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    last_used_seq INTEGER NOT NULL
);
CREATE INDEX artwork_index_lru ON artwork_index (last_used_seq, id);
CREATE TABLE progress_journal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id TEXT NOT NULL,
    position_ms INTEGER NOT NULL CHECK (position_ms >= 0),
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
    recorded_at INTEGER NOT NULL
);
CREATE INDEX progress_journal_recent ON progress_journal (id DESC);
";

/// Durable local storage rooted at one state directory.
pub struct Storage {
    connection: Connection,
    state_directory: PathBuf,
    database: PathBuf,
    artwork_directory: PathBuf,
    credentials_directory: PathBuf,
    artwork_cap: u64,
}

impl Storage {
    /// Open or create storage, applying migrations.
    ///
    /// The artwork cache is clamped to [`HARD_ARTWORK_CACHE_BYTES`].
    pub fn open(state_directory: &Path, artwork_cache_bytes: u64) -> Result<Self, StorageError> {
        if state_directory.as_os_str().is_empty() || state_directory.file_name().is_none() {
            return Err(StorageError::StateDir);
        }
        ensure_private_dir(state_directory)?;
        let artwork_directory = state_directory.join("artwork");
        let credentials_directory = state_directory.join("credentials");
        ensure_private_dir(&artwork_directory)?;
        ensure_private_dir(&credentials_directory)?;

        let database = state_directory.join(DATABASE_FILE);
        ensure_regular_file(&database)?;
        let preexisting_data = fs::metadata(&database).is_ok();

        create_private_file(&database)?;
        let mut connection =
            Connection::open(&database).map_err(|_| sqlite_error(preexisting_data))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| sqlite_error(preexisting_data))?;

        if preexisting_data {
            let quick_check: rusqlite::Result<String> =
                connection.query_row("PRAGMA quick_check", [], |row| row.get(0));
            if !quick_check.as_deref().is_ok_and(|result| result == "ok") {
                return Err(StorageError::CorruptDatabase);
            }
        }

        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|_| sqlite_error(preexisting_data))?;
        if version < 0 {
            return Err(StorageError::CorruptDatabase);
        }
        if version > SCHEMA_VERSION {
            return Err(StorageError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;",
            )
            .map_err(|_| sqlite_error(preexisting_data))?;

        if version < SCHEMA_VERSION {
            migrate(&mut connection, preexisting_data)?;
        }

        set_mode(&database, 0o600)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = with_suffix(&database, suffix);
            if sidecar.exists() {
                set_mode(&sidecar, 0o600)?;
            }
        }

        Ok(Self {
            connection,
            state_directory: state_directory.to_path_buf(),
            database,
            artwork_directory,
            credentials_directory,
            artwork_cap: artwork_cache_bytes.min(HARD_ARTWORK_CACHE_BYTES),
        })
    }

    /// Root state directory.
    #[must_use]
    pub fn state_directory(&self) -> &Path {
        &self.state_directory
    }

    /// SQLite database path.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Effective artwork cache cap after the hard limit is applied.
    #[must_use]
    pub fn artwork_cap_bytes(&self) -> u64 {
        self.artwork_cap
    }

    /// Current SQLite schema version.
    pub fn schema_version(&self) -> Result<i64, StorageError> {
        Ok(self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Store a non-secret preference. Secret-looking keys are rejected.
    pub fn set_preference(&mut self, key: &str, value: &str) -> Result<(), StorageError> {
        validate_preference(key, value)?;
        self.connection.execute(
            "INSERT INTO preferences (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value,
             updated_at = excluded.updated_at",
            params![key, value, unix_seconds()],
        )?;
        Ok(())
    }

    /// Read a preference, or `None` when it has not been set.
    pub fn get_preference(&self, key: &str) -> Result<Option<String>, StorageError> {
        validate_preference_key(key)?;
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM preferences WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Remove a preference.
    pub fn delete_preference(&mut self, key: &str) -> Result<(), StorageError> {
        validate_preference_key(key)?;
        self.connection
            .execute("DELETE FROM preferences WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Save the latest viewed item.
    ///
    /// This stores metadata only: it never records a local media path or
    /// implies that playback can continue without the upstream source.
    pub fn save_last_view(&mut self, view: &LastView) -> Result<(), StorageError> {
        validate_last_view(view)?;
        let position_ms =
            i64::try_from(view.position_ms).map_err(|_| StorageError::InvalidLastView)?;
        let duration_ms = match view.duration_ms {
            Some(duration) => {
                Some(i64::try_from(duration).map_err(|_| StorageError::InvalidLastView)?)
            }
            None => None,
        };
        self.connection.execute(
            "INSERT OR REPLACE INTO last_view
             (singleton, item_id, title, source, position_ms, duration_ms, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                view.item_id,
                view.title,
                view.source.map(Source::code),
                position_ms,
                duration_ms,
                unix_seconds(),
            ],
        )?;
        Ok(())
    }

    /// Load the last viewed item, which is metadata only.
    pub fn last_view(&self) -> Result<Option<LastView>, StorageError> {
        let row = self
            .connection
            .query_row(
                "SELECT item_id, title, source, position_ms, duration_ms
                 FROM last_view WHERE singleton = 1",
                [],
                |row| {
                    Ok(LastViewRow {
                        item_id: row.get(0)?,
                        title: row.get(1)?,
                        source: row.get(2)?,
                        position_ms: row.get(3)?,
                        duration_ms: row.get(4)?,
                    })
                },
            )
            .optional()?;
        let Some(row) = row else {
            return Ok(None);
        };
        let position_ms = u64::try_from(row.position_ms).map_err(|_| StorageError::Database)?;
        let duration_ms = match row.duration_ms {
            Some(duration) => Some(u64::try_from(duration).map_err(|_| StorageError::Database)?),
            None => None,
        };
        let source = match row.source.as_deref() {
            Some(code) => Some(code.parse::<Source>().map_err(|_| StorageError::Database)?),
            None => None,
        };
        Ok(Some(LastView {
            item_id: row.item_id,
            title: row.title,
            source,
            position_ms,
            duration_ms,
        }))
    }

    /// Store artwork bytes and evict least-recently-used entries to fit the cap.
    pub fn store_artwork(&mut self, cache_key: &str, bytes: &[u8]) -> Result<(), StorageError> {
        validate_cache_key(cache_key)?;
        let byte_len = u64::try_from(bytes.len()).map_err(|_| StorageError::ArtworkTooLarge)?;
        if byte_len > self.artwork_cap {
            return Err(StorageError::ArtworkTooLarge);
        }
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT file_name FROM artwork_index WHERE cache_key = ?1",
                params![cache_key],
                |row| row.get(0),
            )
            .optional()?;
        let file_name = match existing {
            Some(file_name) => {
                safe_artwork_path(&self.artwork_directory, &file_name)?;
                file_name
            }
            None => format!("{}.bin", Uuid::new_v4()),
        };
        let path = safe_artwork_path(&self.artwork_directory, &file_name)?;
        write_private_atomic(&path, bytes)?;

        let artwork_directory = self.artwork_directory.clone();
        let cap = i64::try_from(self.artwork_cap).unwrap_or(i64::MAX);
        let byte_len = i64::try_from(byte_len).map_err(|_| StorageError::ArtworkTooLarge)?;
        let transaction = self.connection.transaction()?;
        let maximum: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(last_used_seq), 0) FROM artwork_index",
            [],
            |row| row.get(0),
        )?;
        let sequence = unix_millis().max(maximum.saturating_add(1));
        transaction.execute(
            "INSERT INTO artwork_index (cache_key, file_name, bytes, last_used_seq)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(cache_key) DO UPDATE SET
             file_name = excluded.file_name,
             bytes = excluded.bytes,
             last_used_seq = excluded.last_used_seq",
            params![cache_key, file_name, byte_len, sequence],
        )?;

        let mut total: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM artwork_index",
            [],
            |row| row.get(0),
        )?;
        let mut victims = Vec::new();
        while total > cap {
            let victim: Option<(i64, String, i64)> = transaction
                .query_row(
                    "SELECT id, file_name, bytes FROM artwork_index
                     ORDER BY last_used_seq ASC, id ASC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((id, victim_name, victim_bytes)) = victim else {
                break;
            };
            transaction.execute("DELETE FROM artwork_index WHERE id = ?1", params![id])?;
            total = total.saturating_sub(victim_bytes);
            victims.push(safe_artwork_path(&artwork_directory, &victim_name)?);
        }
        transaction.commit()?;
        for victim in victims {
            remove_file_if_present(&victim)?;
        }
        Ok(())
    }

    /// Resolve a cache key to a file, updating its LRU position.
    pub fn artwork_path(&mut self, cache_key: &str) -> Result<Option<PathBuf>, StorageError> {
        validate_cache_key(cache_key)?;
        let row: Option<(i64, String)> = self
            .connection
            .query_row(
                "SELECT id, file_name FROM artwork_index WHERE cache_key = ?1",
                params![cache_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, file_name)) = row else {
            return Ok(None);
        };
        let path = safe_artwork_path(&self.artwork_directory, &file_name)?;
        if !path.is_file() {
            self.connection
                .execute("DELETE FROM artwork_index WHERE id = ?1", params![id])?;
            return Ok(None);
        }
        let maximum: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(last_used_seq), 0) FROM artwork_index",
            [],
            |row| row.get(0),
        )?;
        let sequence = unix_millis().max(maximum.saturating_add(1));
        self.connection.execute(
            "UPDATE artwork_index SET last_used_seq = ?1 WHERE id = ?2",
            params![sequence, id],
        )?;
        Ok(Some(path))
    }

    /// Total indexed artwork bytes.
    pub fn artwork_total_bytes(&self) -> Result<u64, StorageError> {
        let total: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM artwork_index",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(total).map_err(|_| StorageError::Database)
    }

    /// Append a progress record and trim the journal to its bound.
    pub fn record_progress(
        &mut self,
        item_id: &str,
        position_ms: u64,
        duration_ms: u64,
    ) -> Result<(), StorageError> {
        if !is_valid_item_id(item_id) || position_ms > duration_ms {
            return Err(StorageError::InvalidProgress);
        }
        let position_ms = i64::try_from(position_ms).map_err(|_| StorageError::InvalidProgress)?;
        let duration_ms = i64::try_from(duration_ms).map_err(|_| StorageError::InvalidProgress)?;
        let limit = i64::try_from(PROGRESS_JOURNAL_LIMIT).unwrap_or(i64::MAX);
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO progress_journal (item_id, position_ms, duration_ms, recorded_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![item_id, position_ms, duration_ms, unix_seconds()],
        )?;
        transaction.execute(
            "DELETE FROM progress_journal
             WHERE id <= (SELECT COALESCE(MAX(id), 0) - ?1 FROM progress_journal)",
            params![limit],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Newest progress records first, clamped to the journal bound.
    pub fn recent_progress(&self, limit: usize) -> Result<Vec<ProgressRecord>, StorageError> {
        let limit = limit.clamp(1, PROGRESS_JOURNAL_LIMIT);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self.connection.prepare(
            "SELECT item_id, position_ms, duration_ms, recorded_at
             FROM progress_journal ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit], |row| {
            Ok(ProgressRow {
                item_id: row.get(0)?,
                position_ms: row.get(1)?,
                duration_ms: row.get(2)?,
                recorded_at: row.get(3)?,
            })
        })?;
        let mut records = Vec::new();
        for row in rows {
            let row = row?;
            records.push(ProgressRecord {
                item_id: row.item_id,
                position_ms: u64::try_from(row.position_ms).map_err(|_| StorageError::Database)?,
                duration_ms: u64::try_from(row.duration_ms).map_err(|_| StorageError::Database)?,
                recorded_at: row.recorded_at,
            });
        }
        Ok(records)
    }

    /// Number of retained progress records.
    pub fn progress_count(&self) -> Result<usize, StorageError> {
        let count: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM progress_journal", [], |row| {
                    row.get(0)
                })?;
        usize::try_from(count).map_err(|_| StorageError::Database)
    }

    /// Path a named credential would use, without creating it.
    pub fn credential_path(&self, name: &str) -> Result<PathBuf, StorageError> {
        validate_credential_name(name)?;
        Ok(self.credentials_directory.join(name))
    }

    /// Atomically write a credential with 0600 permissions.
    pub fn write_credential(&self, name: &str, secret: &[u8]) -> Result<PathBuf, StorageError> {
        validate_credential_name(name)?;
        if secret.is_empty() || secret.len() > MAX_CREDENTIAL_BYTES {
            return Err(StorageError::InvalidCredential);
        }
        let path = self.credentials_directory.join(name);
        write_private_atomic(&path, secret)?;
        Ok(path)
    }

    /// Read a credential after checking its permissions.
    pub fn read_credential(&self, name: &str) -> Result<Vec<u8>, StorageError> {
        validate_credential_name(name)?;
        let path = self.credentials_directory.join(name);
        let link_metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(StorageError::CredentialNotFound);
            }
            Err(error) => return Err(StorageError::Io(error)),
        };
        if link_metadata.file_type().is_symlink() {
            return Err(StorageError::InsecurePermissions);
        }
        let metadata = fs::metadata(&path)?;
        let maximum = u64::try_from(MAX_CREDENTIAL_BYTES).unwrap_or(u64::MAX);
        if metadata.len() > maximum {
            return Err(StorageError::InvalidCredential);
        }
        check_private_file(&path)?;
        let secret = fs::read(path)?;
        if secret.is_empty() || secret.len() > MAX_CREDENTIAL_BYTES {
            return Err(StorageError::InvalidCredential);
        }
        Ok(secret)
    }

    /// Move an unreadable database aside after explicit recovery.
    ///
    /// A healthy database, including one with a newer schema, is never moved.
    pub fn recover_corrupt(state_directory: &Path) -> Result<PathBuf, StorageError> {
        if state_directory.as_os_str().is_empty() || state_directory.file_name().is_none() {
            return Err(StorageError::StateDir);
        }
        let database = state_directory.join(DATABASE_FILE);
        if !database.exists() {
            return Err(StorageError::NoCorruptDatabase);
        }

        let healthy = match Connection::open(&database) {
            Ok(connection) => {
                let quick_check: rusqlite::Result<String> =
                    connection.query_row("PRAGMA quick_check", [], |row| row.get(0));
                if quick_check.as_deref().is_ok_and(|result| result == "ok") {
                    let version: rusqlite::Result<i64> =
                        connection.query_row("PRAGMA user_version", [], |row| row.get(0));
                    match version {
                        Ok(version) => {
                            if version > SCHEMA_VERSION {
                                return Err(StorageError::UnsupportedSchema {
                                    found: version,
                                    supported: SCHEMA_VERSION,
                                });
                            }
                            version >= 0
                        }
                        Err(_) => false,
                    }
                } else {
                    false
                }
            }
            Err(_) => false,
        };
        if healthy {
            return Err(StorageError::DatabaseNotCorrupt);
        }

        let stamp = unix_millis();
        let mut target = state_directory.join(format!("{}.corrupt-{stamp}", DATABASE_FILE));
        let mut attempt = 0u32;
        while target.exists() {
            attempt = attempt.saturating_add(1);
            target = state_directory.join(format!("{}.corrupt-{stamp}-{attempt}", DATABASE_FILE));
        }
        fs::rename(&database, &target)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = with_suffix(&database, suffix);
            if sidecar.exists() {
                fs::rename(&sidecar, with_suffix(&target, suffix))?;
            }
        }
        Ok(target)
    }
}

/// Last-viewed metadata. No media bytes or local path are stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastView {
    pub item_id: String,
    pub title: String,
    pub source: Option<Source>,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
}

impl LastView {
    /// Offline resume is never claimed; this is metadata only.
    #[must_use]
    pub const fn is_playable_offline(&self) -> bool {
        false
    }
}

/// One bounded progress journal entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressRecord {
    pub item_id: String,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub recorded_at: i64,
}

struct LastViewRow {
    item_id: String,
    title: String,
    source: Option<String>,
    position_ms: i64,
    duration_ms: Option<i64>,
}

struct ProgressRow {
    item_id: String,
    position_ms: i64,
    duration_ms: i64,
    recorded_at: i64,
}

/// Storage and configuration failures that never include secret values.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("state directory is not usable")]
    StateDir,
    #[error("database operation failed")]
    Database,
    #[error("database is corrupt")]
    CorruptDatabase,
    #[error("database schema version {found} is not supported (maximum {supported})")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("no corrupt database to recover")]
    NoCorruptDatabase,
    #[error("database is healthy; refusing to move it")]
    DatabaseNotCorrupt,
    #[error("invalid artwork cache key or payload")]
    InvalidArtwork,
    #[error("artwork is larger than the configured cache")]
    ArtworkTooLarge,
    #[error("invalid credential name or secret")]
    InvalidCredential,
    #[error("credential file not found")]
    CredentialNotFound,
    #[error("credential file permissions are too broad")]
    InsecurePermissions,
    #[error("invalid preference")]
    InvalidPreference,
    #[error("secrets may not be stored in preferences")]
    SecretNotAllowed,
    #[error("invalid progress record")]
    InvalidProgress,
    #[error("invalid last view record")]
    InvalidLastView,
    #[error("I/O operation failed")]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for StorageError {
    fn from(_error: rusqlite::Error) -> Self {
        Self::Database
    }
}

/// Default state directory: `$XDG_STATE_HOME/tv-client` or
/// `$HOME/.local/state/tv-client`.
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    if let Some(state_home) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(state_home).join("tv-client"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/tv-client"))
}

fn migrate(connection: &mut Connection, preexisting_data: bool) -> Result<(), StorageError> {
    let transaction = connection
        .transaction()
        .map_err(|_| sqlite_error(preexisting_data))?;
    transaction
        .execute_batch(SCHEMA_V1)
        .map_err(|_| sqlite_error(preexisting_data))?;
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|_| sqlite_error(preexisting_data))?;
    transaction
        .commit()
        .map_err(|_| sqlite_error(preexisting_data))?;
    Ok(())
}

fn sqlite_error(preexisting_data: bool) -> StorageError {
    if preexisting_data {
        StorageError::CorruptDatabase
    } else {
        StorageError::Database
    }
}

fn ensure_regular_file(path: &Path) -> Result<(), StorageError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StorageError::Io(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StorageError::StateDir);
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(StorageError::StateDir);
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;

                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(path)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(path)?;
            }
        }
        Err(error) => return Err(StorageError::Io(error)),
    }
    set_mode(path, 0o700)
}

fn create_private_file(path: &Path) -> Result<(), StorageError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    drop(file);
    set_mode(path, 0o600)
}

fn write_private_atomic(path: &Path, data: &[u8]) -> Result<(), StorageError> {
    let parent = path.parent().ok_or(StorageError::StateDir)?;
    let temporary = parent.join(format!(".tmp-{}", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let write_result = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(StorageError::Io(error));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(StorageError::Io(error));
    }
    set_mode(path, 0o600)?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_file_if_present(path: &Path) -> Result<(), StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StorageError::Io(error)),
    }
}

fn validate_preference(key: &str, value: &str) -> Result<(), StorageError> {
    validate_preference_key(key)?;
    if value.len() > MAX_PREFERENCE_VALUE_BYTES {
        return Err(StorageError::InvalidPreference);
    }
    if is_secret_key(key) {
        return Err(StorageError::SecretNotAllowed);
    }
    Ok(())
}

fn validate_preference_key(key: &str) -> Result<(), StorageError> {
    if key.is_empty()
        || key.len() > 64
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(StorageError::InvalidPreference);
    }
    if is_secret_key(key) {
        return Err(StorageError::SecretNotAllowed);
    }
    Ok(())
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "password",
        "secret",
        "cookie",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|needle| key.contains(*needle))
}

fn validate_last_view(view: &LastView) -> Result<(), StorageError> {
    if !is_valid_item_id(&view.item_id)
        || view.title.len() > MAX_TITLE_BYTES
        || view.title.chars().any(char::is_control)
    {
        return Err(StorageError::InvalidLastView);
    }
    if view
        .duration_ms
        .is_some_and(|duration| view.position_ms > duration)
    {
        return Err(StorageError::InvalidLastView);
    }
    Ok(())
}

fn validate_cache_key(cache_key: &str) -> Result<(), StorageError> {
    if cache_key.is_empty()
        || cache_key.len() > MAX_CACHE_KEY_BYTES
        || cache_key.chars().any(char::is_control)
    {
        return Err(StorageError::InvalidArtwork);
    }
    Ok(())
}

fn safe_artwork_path(directory: &Path, file_name: &str) -> Result<PathBuf, StorageError> {
    if file_name.is_empty()
        || file_name.len() > 255
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains("..")
    {
        return Err(StorageError::Database);
    }
    Ok(directory.join(file_name))
}

fn validate_credential_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(StorageError::InvalidCredential);
    }
    Ok(())
}

#[cfg(unix)]
fn check_private_file(path: &Path) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(StorageError::InsecurePermissions);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_file(_path: &Path) -> Result<(), StorageError> {
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), StorageError> {
    Ok(())
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
