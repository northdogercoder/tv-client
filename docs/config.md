# Client configuration and local state

`tv-client` reads one strict TOML file. Unknown keys are rejected so a typo
cannot silently leave cloud access or a feature enabled. Every value is
validated before any network, UI or player work starts.

## Configuration file

Default path:

- `$XDG_CONFIG_HOME/tv-client/client.toml` when `XDG_CONFIG_HOME` is set.
- `$HOME/.config/tv-client/client.toml` otherwise.

`--config PATH` overrides the path. A named file must exist; if the default
path is absent, the built-in defaults below are used. The file is limited to
64 KiB.

Keys and defaults (identical to `deploy/client.toml.example`):

```toml
jellyfin_url = "https://jellyfin.example.ts.net"
server_url = "https://tv-control.example.ts.net"
cloud_enabled = false
youtube_enabled = false
airplay_enabled = false
webrtc_enabled = false
max_width = 1280
max_height = 720
max_fps = 30
bitrate_bps = 6000000
max_bitrate_bps = 10000000
artwork_cache_bytes = 134217728
share_titles = false
```

| Key | Type | Default | Validation |
|---|---|---|---|
| `jellyfin_url` | string, optional | absent | Absolute URL; `https` anywhere, `http` only for a local Jellyfin host |
| `server_url` | string, optional | absent | Absolute `https` URL; required by `cloud_enabled`, `youtube_enabled` and `webrtc_enabled` |
| `cloud_enabled` | boolean | `false` | Must remain `false` until the control server is validated |
| `youtube_enabled` | boolean | `false` | Requires `server_url` |
| `airplay_enabled` | boolean | `false` | Local feature switch |
| `webrtc_enabled` | boolean | `false` | Requires `server_url` |
| `max_width` | integer | `1280` | `320..=7680` |
| `max_height` | integer | `720` | `240..=4320` |
| `max_fps` | integer | `30` | `1..=120` |
| `bitrate_bps` | integer | `6000000` | `64000..=100000000` |
| `max_bitrate_bps` | integer | `10000000` | `64000..=100000000` and at least `bitrate_bps` |
| `artwork_cache_bytes` | integer | `134217728` | `0..=134217728` (128 MiB) |
| `share_titles` | boolean | `false` | Metadata-sharing consent; never enables credentials or media upload |

Every key is optional and missing keys take the defaults above. A present but
invalid value, an unknown key or a duplicate key fails startup.

### URL rules

- `server_url` must use `https`. HTTP is never accepted for the control server.
- `jellyfin_url` may use `http` only for an explicitly local host:
  `localhost`, a `.local` name, a single-label LAN name such as `jellyfin`,
  loopback, RFC 1918 private IPv4, link-local IPv4, `100.64.0.0/10`, IPv6
  loopback, IPv6 unique-local (`fc00::/7`) or IPv6 link-local (`fe80::/10`).
- URLs must be absolute, at most 2048 bytes, and contain no whitespace,
  control bytes, credentials (`user:pass@`), query string, fragment or
  backslashes. Ports must be `1..=65535`.
- Error messages name the field only and never render the supplied URL, so a
  token pasted into a URL is not copied into logs.

## Command line

| Flag | Effect |
|---|---|
| `--config PATH` | Read the given TOML file |
| `--state-dir DIR` | Use the given state directory |
| `--check-config` | Validate configuration, print `configuration OK`, exit without opening state |
| `--version` | Print package name and version |
| `--help` | Print usage |

Without `--check-config`, the binary performs bounded headless initialization:
it opens the state database, applies migrations and exits. It does not start a
UI, network connection, player or browser. Exit code 2 reports configuration
problems; other initialization failures use exit code 1.

Default state directory:

- `$XDG_STATE_HOME/tv-client` when `XDG_STATE_HOME` is set.
- `$HOME/.local/state/tv-client` otherwise.

## State directory layout

```text
<state-dir>/                    0700
├── client.db                   0600 SQLite database
├── client.db-wal               WAL when present, inside the 0700 directory
├── client.db-shm               shared memory when present
├── client.db-journal           rollback journal when present
├── artwork/                    0700
│   └── <uuid>.bin              0600 bounded artwork bytes
└── credentials/                0700
    └── <name>                  0600 atomically replaced credentials
```

The state directory, `artwork/` and `credentials/` are set to 0700. The
database, artwork files and credential files are set to 0600. Symbolic links
are rejected for the state directory, database and credential files.

### Database contents

SQLite migrations create four bounded stores:

- `preferences` — non-secret key/value settings. Values are limited to 4096
  bytes; keys containing `token`, `password`, `secret`, `cookie`,
  `authorization` or `credential` are rejected with `SecretNotAllowed`.
- `last_view` — one item id, title, source, position and duration. This is
  metadata only. `LastView::is_playable_offline()` is always `false`; no media
  path or offline-playable claim is stored.
- `artwork_index` — cache key, file name, byte count and LRU sequence. The
  effective cap is `min(artwork_cache_bytes, 128 MiB)`. New writes evict the
  least recently used entries and delete their files.
- `progress_journal` — at most the newest 256 progress records. Every insert
  trims older rows in the same transaction.

### Credentials

Device tokens and other secrets are not stored in SQLite and are never logged.
They are written to `credentials/<name>` with a temporary 0600 file in the same
directory, `fsync`, atomic rename and a directory sync. Reads reject files with
group/other permission bits set, symlinks, empty payloads and files larger than
4096 bytes.

## Corruption and recovery

Opening a database that fails `PRAGMA quick_check`, or whose schema cannot be
read, returns `StorageError::CorruptDatabase`. Opening a database with a newer
schema returns `StorageError::UnsupportedSchema`. Neither open path renames or
deletes the file.

Recovery is explicit:

```rust
let moved = tv_client::storage::Storage::recover_corrupt(state_directory)?;
```

`recover_corrupt` first verifies that the database is unreadable. A healthy
database returns `DatabaseNotCorrupt`; a newer schema returns
`UnsupportedSchema`; a missing database returns `NoCorruptDatabase`. Only an
actually unreadable database is renamed to
`client.db.corrupt-<unix-millis>[-n]`, together with `-wal`, `-shm` and
`-journal` sidecars when present. A fresh database is created by the next `Storage::open` call.

## Bounds and failure behavior

- Event and effect queues default to 32 items. `try_send` reports `QueueFull`
  instead of blocking or growing memory.
- The state loop rejects invalid and stale events without terminating. A full
  effect queue stops the loop with a typed error.
- Shutdown uses a watch channel and `select!`; cancellation never waits for a
  queue drain and applied state is returned to the caller.
- The configuration file is capped at 64 KiB; preference values at 4 KiB;
  credentials at 4 KiB; artwork at the configured cap (hard maximum 128 MiB).
