# Jellyfin library integration

The `tv_client::jellyfin` module is a direct, local-first client for one
configured Jellyfin server. It authenticates the user, browses the library with
typed metadata, and caches artwork locally. Browsing state contains opaque item
IDs only; stream and transcoding URLs are never modeled, stored or forwarded.

## Configuration

`ClientConfig::jellyfin_url` is the required base origin, optionally with a path
prefix (for example `https://media.example/jellyfin`). Absence is reported as
`JellyfinError::NotConfigured`. `ClientConfig::validate` still enforces the
shared URL policy: HTTPS except for an explicitly local Jellyfin host, no
credentials, query or fragment.

The client:

- uses the configured origin and path for every request,
- enables normal TLS certificate and hostname validation through reqwest's
  rustls backend (no invalid-certificate switches exist in this code),
- applies connect, per-request and total timeouts (`Timeouts`, defaults 5 s,
  10 s and 30 s),
- disables proxy use with `no_proxy()`,
- refuses redirects (`redirect::Policy::none()`) and rejects any response whose
  final origin differs from the configured origin.

## Authorization header

Authenticated requests carry exactly one header:

```text
Authorization: MediaBrowser Client="tv-client", Device="<name>", DeviceId="<stable>", Version="<package version>", Token="<access token>"
```

Before login the same header is sent without the `Token` parameter. The
device ID is a random UUID persisted in the `jellyfin_device_id` preference, so
it survives restarts. Quick Connect does not use `X-Emby-Authorization` or
`X-Emby-Token`; the verified test server (Jellyfin 12.4.0) returns HTTP 400 and
HTTP 401 for those legacy headers respectively.

## Authentication

### Username and password

`JellyfinClient::authenticate_by_name` posts the credentials once to
`POST /Users/AuthenticateByName`. The password exists only in the request body;
it is never written to preferences, the credential store, logs or error text.
On success the returned access token is written to the
`jellyfin_access_token` credential file (0600 through `Storage`), and only the
user ID, display name and optional server ID are kept in memory.

### Quick Connect

1. `POST /QuickConnect/Initiate` returns `Secret` and `Code`.
   `QuickConnectInitiation::code()` is shown to the user; the secret stays in
   memory and is redacted from `Debug`.
2. `GET /QuickConnect/Connect?secret=...` returns `Authenticated: false` until
   the user approves the code on another client. The client returns
   `QuickConnectPoll::Pending` or `Approved`.
3. `POST /Users/AuthenticateWithQuickConnect` with `{"Secret": "..."}`
   exchanges an approved request for an access token.

A request expires after `QUICK_CONNECT_TTL` (10 minutes by default, adjustable
with `set_quick_connect_ttl`). Polling or exchanging an expired request returns
`JellyfinError::QuickConnectExpired`. Cancellation, rejection and disabled
Quick Connect are reported as `Cancelled`, `QuickConnectNotApproved` (HTTP
401/400 on exchange) and `QuickConnectUnavailable`.

### Session restore and logout

`restore_session` resolves a persisted token through `GET /Users/Me`; an
unauthorized token is deleted locally and reported as `false`.
`logout` posts `POST /Sessions/Logout`, removes the credential file, drops the
in-memory session and removes cached artwork files. Local cleanup happens even
if the server revocation call fails; the revocation error is returned after
cleanup.

## Library endpoints

All library methods require an active session and accept a `CancelToken`.
Reads are strictly typed with serde and use PascalCase Jellyfin fields.

| Method | Endpoint | Notes |
|---|---|---|
| `views` | `GET /Users/{userId}/Views` | Top-level Movies, TV Shows, Music, Books, etc. |
| `libraries` | `GET /Users/{userId}/Views` | Bounded `Vec<LibraryEntry>`: id, name, collection type (max 32) |
| `browse` | `GET /Users/{userId}/Items` | One 24-item page below a library; `collection_type` selects `IncludeItemTypes`/`MediaTypes` (video default `Movie,Episode,Video` + `Video`), always `Fields=UserData,ImageTags,Overview` |
| `items` | `GET /Users/{userId}/Items` | Typed `ItemQuery`: parent, types, media types, recursion, search, sort, filters, fields |
| `resume` | `GET /Users/{userId}/Items` | `Filters=IsResumable`, `MediaTypes=Video`, sorted by `DatePlayed` descending |
| `latest` | `GET /Users/{userId}/Items/Latest` | Optional parent, bounded limit |
| `seasons` | `GET /Shows/{seriesId}/Seasons?userId=...` | |
| `episodes` | `GET /Shows/{seriesId}/Episodes?userId=...` | Optional `SeasonId`, episode ordering |
| `music` | `GET /Users/{userId}/Items` | `IncludeItemTypes=Audio`, `MediaTypes=Audio` |
| `search` | `GET /Users/{userId}/Items?SearchTerm=...` | Recursive item search |
| `user_data` | `GET /Users/{userId}/Items/{itemId}/UserData` | Played state, resume ticks, favourite |
| `update_user_data` | `POST /Users/{userId}/Items/{itemId}/UserData` | Typed played/position/favourite update |
| `playback_info` | `POST /Items/{itemId}/PlaybackInfo?UserId=...` | Capabilities only; URLs in the upstream JSON are ignored |
| `image` | `GET /Items/{itemId}/Images/{type}?tag=&maxWidth=&maxHeight=&format=jpg` | Typed artwork class, cached `CachedArtwork` |

Pagination defaults to 50 items per page (`PAGE_SIZE`). `Page::new(start, limit)`
rejects zero or more than 200 items and is emitted as `StartIndex` and `Limit`.
`Page::next()` advances by the page size. Library grids use `ItemQuery::browse`,
which requests 24 items per page (`BROWSE_PAGE_SIZE`) so poster fetches stay
bounded.

`BaseItem` includes IDs, names, types, collection type, parent/series/season/
album IDs, artists, indexes, duration ticks, overview, image tags and user
metadata. It deliberately has no `Path`, stream URL or transcoding URL fields.
`PlaybackInfoResponse` exposes source identifiers, capabilities and stream
descriptors, not file paths.

## PlaybackInfo negotiation

`JellyfinClient::playback_info_with` posts a typed
`PlaybackInfoRequest` to `POST /Items/{itemId}/PlaybackInfo?UserId=...`. The
body carries the user, maximum bitrate, resume ticks, optional stream/source
selection and a device profile that describes only what the C006 player has
proven on the target hardware:

| Capability | Profile value |
|---|---|
| Direct-play container/codecs | `mp4`, H.264 8-bit SDR, AAC-LC stereo |
| Direct-play bounds | 1920x1080, 30 fps, H.264 level 4.1 |
| Transcode output | HLS, H.264 video, AAC stereo, MPEG-TS segments |
| Streaming bitrate | starts at 6,000,000 bit/s, hard cap 10,000,000 bit/s |
| Subtitles | server burn-in (`Method: Encode`) only |
| Live streams | `AutoOpenLiveStream: false` |

HEVC, AV1, VP9, HDR, 10-bit, interlaced, over-1080p, over-30 fps and
non-AAC/multichannel audio therefore negotiate a transcode. The request has
`EnableDirectPlay`, `EnableDirectStream`, `EnableTranscoding` and stream-copy
flags enabled so Jellyfin can choose remux when the proven codecs fit, and
`AlwaysBurnInSubtitleWhenTranscoding` when a subtitle stream is selected or
burn-in is requested. A selected subtitle always forces a transcode because
the native child does not render subtitle overlays.

`select_media_source` (also in `jellyfin::playback`) scores direct play above
remux above transcode, preserves server source order among equal scores, and
returns a typed `SelectionError` when the server reports
`NotAllowed`/`NoCompatibleStream`/`RateLimitExceeded` or no source can be used.
A direct-play URL is built locally as
`/Videos/{itemId}/stream?static=true&mediaSourceId=...&playSessionId=...` below
the configured base path. Remux/transcode URLs must come from the server.

`StreamUrl` keeps every server URL opaque (`Debug` prints
`StreamUrl(<redacted>)`) and only resolves it when the scheme is HTTP(S), the
origin equals the configured Jellyfin origin, and the path stays below the
configured base path. `playback_info_with` enforces that allowlist for every
URL in the response before the controller can load it; cross-origin or
out-of-prefix values return `JellyfinError::UnexpectedRedirect`. Neither the
client nor the controller rewrites a stream through a proxy or another host.

## Controller, progress and ticks

`playback::PlaybackController` owns one `JellyfinClient`, one C006
`PlayerSession` and, when constructed with `with_storage`, the `Storage` handle
used for artwork caching. Starting an item:

1. reads `GET /Items/{itemId}` and the embedded (or explicit) user data;
2. resumes from `PlaybackPositionTicks` when the position is at least 30 s and
   not within 30 s of the end, and is not marked played;
3. negotiates PlaybackInfo and selects one compatible source;
4. stops any previous playback, loads the child with an `Authorization`
   header and a bounded start position, and reports
   `POST /Sessions/Playing` once;
5. reports `POST /Sessions/Playing/Progress` every 10 s, on pause/resume and
   after a seek; and
6. reports `POST /Sessions/Playing/Stopped` exactly once on back, stop,
   natural end or failure, then releases the transcode.

Jellyfin uses 100 ns ticks. Conversion is explicit and bounded:
`ticks_from_millis` rejects positions beyond 24 hours and cannot overflow;
`millis_from_ticks` floors to milliseconds and saturates at the 24 hour player
bound. Server resume ticks are never passed to the child as a 64-bit tick
value.

Progress reporting is best effort but not lossy for transient failures. A
failed snapshot that is retryable (timeout, network error, 429, HTTP 5xx)
enters a bounded (16 entry) `ProgressJournal`. Entries carry the user, item,
play session, sequence, timestamp and attempt count. A newer snapshot for the
same play session replaces the older one; retries always send the freshest
active position, never the stale snapshot; entries older than 120 s or with
five failed attempts are dropped; and an acknowledged snapshot removes older
entries for that play session. The journal is cleared on user change and on a
401, so a token failure cannot leak one user's progress into another session.
`/Sessions/Playing/Stopped` is sent once and is not journaled.

Teardown is typed and idempotent: the controller sends the final Stopped
report, sends `PlayerCommand::Stop` to the supervisor, clears journal entries
for that play session and, for transcodes, makes a best-effort
`DELETE /Videos/ActiveEncodings?deviceId=...&playSessionId=...`. Jellyfin 12.4
does not list that endpoint in its OpenAPI document, so a 404/405 is treated as
"not supported"; the documented Stopped report remains the primary release
path. Shutdown, command-channel closure and player errors all take the same
path.

## Real Pi playback matrix

> Environment: Raspberry Pi 3B, labwc 0.20.2, 1080p HDMI, GStreamer 1.26.2,
> Rust 1.100.0, limited non-admin Jellyfin user. Runs were intentionally short;
> no soak was performed. Credentials stayed in
> `~/.config/tv-client/jellyfin-test.env`.

| Check | Result |
|---|---|
| Live PlaybackInfo against Jellyfin 12.4.0 | PASS: first library video negotiated `Transcode` from source `a99f60bc456e7d5834792a3a046a2c1a`; start, progress and stopped reports all returned success |
| Native child Transcode short run | **PASS**: 19.5 s on the Pi; position updates observed, pause/resume/stop exercised, controller reported `Transcode`. The panel-visible codec was not independently logged. |
| Native child H.264 direct/remux | NOT VERIFIED: the only real item negotiated Transcode |
| Native child HEVC transcode | NOT VERIFIED |
| Subtitle burn-in | NOT VERIFIED on the panel; selection is forced to transcode by the mock tests |
| Resume | PASS offline mock matrix: played, below 30 s, within 30 s of the end, and a normal 50 s resume all select the expected start position; the real server item was not resumed in this change |
| Network interruption | Not exercised against the real server; retry journal/freshness/backoff is covered offline |

The GStreamer-feature live test (`live_pi_playback_short_run`) initially gave up
5 s after the child reported Tracks; that was a test-window bug. With the full
45 s deadline it reached Position in 19.5 s with a panel-visible run. The
negotiation/session-reporting test and the offline controller suite also pass.
Direct-play/remux, HEVC and subtitle burn-in remain open.

## Artwork cache

Artwork is modeled by `ImageRequest` plus a typed `ArtworkClass`; the UI never
sees a server URL or supplies one.

| Class | Requested size | Encoded cap | Pixel cap | Side cap |
|---|---|---|---|---|
| `ArtworkClass::Poster` | 320x480 | 2 MiB | 1,000,000 | 320x480 |
| `ArtworkClass::Preview` | 1280x720 | 8 MiB | 2,073,600 | 1280x720 |

`ImageRequest::poster(item_id, tag)` and `ImageRequest::preview(item_id, tag)`
build the two standard requests; `ImageRequest::class()` reports the class.
Requests carry `tag`, `maxWidth`, `maxHeight`, `format=jpg` and `quality=90`.
JPEG is used because every supported Slint backend decodes it; WebP/GIF are not
required.

`JellyfinClient::image` returns `CachedArtwork { path, width, height, bytes }`.
The path is produced only by `Storage`; width/height/bytes are validated
against the class before the value is returned. `CachedArtwork` dereferences to
`Path` and implements `AsRef<Path>`.

The cache key contains the server origin, user ID, item ID, image kind, the
server's image tag **and** `class:widthxheight:format`. A poster and a preview
of the same item/tag therefore never collide (`jf2` key version). A new server
tag invalidates the old entry. Cached payloads are re-validated (bounded by the
class byte cap) on every hit; a corrupt or out-of-class file is replaced.

Caps are enforced before storage:

- the encoded payload is capped per class (2 MiB poster, 8 MiB preview); both
  declared `Content-Length` and streamed chunks are checked;
- the format header is parsed without a decoder;
- decoded sides and pixels must fit the class caps, and the global 8192-side /
  33,554,432-pixel limits still apply;
- `Content-Type` is never trusted.

A missing image (`HTTP 404`) returns `JellyfinError::ArtworkMissing`, and an
item with no primary tag is reported as missing without a request. Both are
normal placeholder outcomes; they are not browse/page failures. The overall
artwork directory remains bounded by the `Storage` LRU cap (maximum 128 MiB),
and cache writes are atomic and private.

`PlaybackController` owns the `Storage` handle used by `image`; a controller
built without one still plays items and only rejects artwork requests. Browse
and artwork results are published as `PlaybackUpdate::Browse(BrowseUpdate)` so
they never move the player screen.

| `BrowseUpdate` | Meaning |
|---|---|
| `LibrariesReady { libraries }` | `Vec<LibraryEntry>` (id, name, collection type), max 32 |
| `LibrariesFailed { message }` | Redacted failure text |
| `ItemsReady { library_id, items, total, next_start }` | Bounded `Vec<BrowseItem>` (max 24); `next_start` is `Some` only when more items remain |
| `ItemsFailed { library_id, message }` | Redacted failure text |
| `ArtworkReady { item_id, class, artwork }` | Cached path and validated metadata |
| `ArtworkMissing { item_id, class }` | 404 or no tag |
| `ArtworkFailed { item_id, class, message }` | Size/decode/transport failure |
| `SessionExpired` | HTTP 401 during a browse or artwork request |

`BrowseItem` carries exactly `id`, `title`, `subtitle`, `overview`,
`primary_tag` and `item_type`. Titles and overview are truncated to fixed
character counts; no path, URL or token field exists, and `Debug` prints only
these values.

## Browse UI flow

Home's Jellyfin row now opens `JellyfinLibraries`, not the text YouTube library
screen. The flow is libraries (4 columns) -> items (5-column poster grid) ->
preview (large image plus Play/Back, or a typed planned badge for audio and
books) -> the existing `PlayJellyfin` action for video items. Escape and Back
move one level; Home returns to the launcher. Focus is stored as an opaque
identifier so a refresh keeps the same library or item selected.

Each library keeps the Jellyfin `CollectionType` in `LibraryEntry`. The UI
forwards it in `UiBrowseCommand::LoadItems` / `PlaybackCommand::LoadJellyfinItems`
so the parent-scoped query matches the library instead of always asking for
video:

| Collection type | `IncludeItemTypes` | `MediaTypes` | Preview action |
|---|---|---|---|
| `movies`, `tvshows`, missing/unknown | `Movie,Episode,Video` | `Video` | Play (`PlayJellyfin`) |
| `musicvideos` | `MusicVideo` | `Video` | Play (`PlayJellyfin`) |
| `music` | `Audio,MusicAlbum` | none (no video filter) | Player planned (C015) |
| `audiobooks` | `AudioBook` | `Audio` | Player planned (C015) |
| `books` | `Book` | none (no video filter) | Reader planned (C016) |

Matching is exact and case-insensitive; any unrecognised collection type keeps
the video default, so a hostile view name cannot widen the query. Every page is
still 24 items and requests only `UserData,ImageTags,Overview`.

The preview decides from the item's `Type`: `Movie`, `Episode`, `Video` and
`MusicVideo` use the existing video player, while `Audio`, `MusicAlbum`,
`AudioBook` and `Book` are browse-only in this build. Their preview focuses
`Back`, shows a typed "Audio player planned (C015)" or "Book reader planned
(C016)" note, and never emits `PlayJellyfin`; the dedicated players are C015
(`plans/015-music-audiobooks.md`) and C016 (`plans/016-books.md`). A missing or
unknown type is shown as `Unavailable` rather than being sent to the player.

The preview keeps its action row inside the screen host at both 720p and 1080p:
title, subtitle and overview scroll in the upper area while Play/Back stay
pinned above the fixed global footer.

The UI sends typed `UiBrowseCommand`s (`LoadLibraries`, `LoadItems`,
`LoadArtwork`) through the router to the controller's existing
`LoadJellyfinLibraries`, `LoadJellyfinItems` and `LoadArtwork` commands. On an
`ItemsReady` page the view queues poster requests for at most 24 items and never
fetches the next page automatically. The `Load more` tile is focusable and is
the only way to request `next_start`.

Retention is bounded: at most 32 library views and 64 items per library. A page
is 24 items, so the third page is truncated at the retention limit and no
further More tile is offered. Artwork results are local `CachedArtwork` paths;
the Slint bridge decodes at most 64 images and shows an empty placeholder for
missing, failed or tagless images. A missing/failed image never changes a
browse page from `Ready` to an error state. `SessionExpired` and completed
sign-out clear libraries, items, preview, image paths and the bridge cache.

Known limits: the verified `/Users/{id}/Views` route is used with no
`/UserViews` fallback; backdrops are not modelled, so the Primary image is used
for posters and preview; and hardware image rendering (femtovg on the target
TV) has **NOT RUN**. Music, audiobooks and books are **browse and preview only**
in this build: C015 will add the dedicated audio player and queue, and C016 the
bounded EPUB/PDF/CBZ reader. No audio or book stream URL, download or reader is
implemented here.

## Cancellation and failures

Every request send and every response body read runs inside `tokio::select!`
against a `CancelToken` from `cancellation()`. Cancellation during a request
returns `JellyfinError::Cancelled` promptly. Dropping the `CancelHandle` also
cancels pending work. There are no detached request tasks and no unbounded
metadata cache.

| Condition | Error |
|---|---|
| Missing `jellyfin_url` | `NotConfigured` |
| URL/origin/device parameter invalid | `InvalidConfig`, `InvalidUrl`, `InvalidRequest` |
| Item ID looks like a URL or fails shared validation | `InvalidItemId` |
| Missing session/token | `NotAuthenticated` |
| Login rejected (HTTP 400/401) | `AuthenticationRejected` |
| Quick Connect exchange rejected | `QuickConnectNotApproved` |
| Quick Connect disabled/missing | `QuickConnectUnavailable` |
| Authenticated request HTTP 401 | `Unauthorized` |
| HTTP 403 | `Forbidden` |
| HTTP 429 | `RateLimited { retry_after_secs }` |
| Timeout | `Timeout` |
| Other transport failure | `Network` (no source text, so URLs/secrets cannot leak) |
| 3xx or cross-origin final URL | `UnexpectedRedirect` |
| Other HTTP status | `Http { status }` |
| Malformed JSON/shape | `InvalidResponse` |
| Body above 8 MiB | `ResponseTooLarge` |
| Image payload too large | `ArtworkTooLarge` |
| Image dimensions above limits | `ImageTooLarge` |
| Unknown image header | `InvalidImage` |
| Image absent (HTTP 404) or no primary tag | `ArtworkMissing` |
| Storage failure | `Storage` or `Artwork` |

Error `Display` and `Debug` output never includes passwords, access tokens,
Quick Connect secrets, authorization headers or request URLs. Transport errors
intentionally drop reqwest's source text because it can echo a URL containing a
Quick Connect secret.

## Tests and the live server

`tests/jellyfin_library.rs` is offline by default and uses a small local mock
HTTP server. It covers the authorization header and stable device ID, Quick
Connect initiation/poll/exchange/expiry, password non-persistence, pagination,
endpoint wiring, 401/403/429/500 mapping, redirect rejection, timeouts,
cancellation, cache hits, logout cleanup, image dimension rejection and
credential-free error output.

The single live test is `#[ignore]` and runs only when `JELLYFIN_URL`,
`JELLYFIN_USER` and `JELLYFIN_PASS` are set:

```sh
JELLYFIN_URL=http://jellyfin:8096 \
JELLYFIN_USER=test \
JELLYFIN_PASS=... \
cargo test --no-default-features --test jellyfin_library -- --ignored
```

The reference test server is Jellyfin 12.4.0 at `http://jellyfin:8096` with
Quick Connect enabled and a limited non-admin user. Credentials live outside
the repository in `~/.config/tv-client/jellyfin-test.env` (0600) and must never
be committed, printed or included in test fixtures.

### Playback tests

`tests/jellyfin_playback.rs` is offline by default. Its local mock server and
scripted child cover the device-profile body, PlaybackInfo source selection,
tick conversion and bounds, the start/progress/stop ordering, 401 and
cancellation failures, cross-origin stream URLs and HTTP redirect rejection,
progress retry freshness/user isolation/backoff, transcode teardown, stale load
generations, resume positions (played, below minimum, near end and normal),
audio/subtitle renegotiation and retry/next-episode commands. The two live
tests are `#[ignore]`:

```sh
# Negotiation and session reporting against the real 12.4 server.
JELLYFIN_URL=http://jellyfin:8096 JELLYFIN_USER=test JELLYFIN_PASS=... \
cargo test --locked --no-default-features --test jellyfin_playback -- --ignored \
  live_jellyfin_playback_negotiation

# Native child run on the Pi (needs the GStreamer feature and the active labwc
# Wayland display); TV_LIVE_ITEM optionally selects one library item.
XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
  cargo test --features gstreamer-player --test jellyfin_playback -- --ignored \
  live_pi_playback_short_run
```

Both read credentials only from the environment; no token or password appears
in output, fixtures or panic messages.
