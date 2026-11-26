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
| `image` | `GET /Items/{itemId}/Images/{type}?tag=&maxWidth=&maxHeight=` | Cached artwork |

Pagination defaults to 50 items per page (`PAGE_SIZE`). `Page::new(start, limit)`
rejects zero or more than 200 items and is emitted as `StartIndex` and `Limit`.
`Page::next()` advances by the page size.

`BaseItem` includes IDs, names, types, collection type, parent/series/season/
album IDs, artists, indexes, duration ticks, overview, image tags and user
metadata. It deliberately has no `Path`, stream URL or transcoding URL fields.
`PlaybackInfoResponse` exposes source identifiers, capabilities and stream
descriptors, not URLs.

## Artwork cache

Images are cached through `Storage::store_artwork` / `Storage::artwork_path`.
The cache key contains the server origin, user ID, item ID, image kind and the
server's image tag, so a new tag replaces stale artwork. Long keys are reduced
to a stable bounded hash key.

- The cache cap is the `Storage` cap (maximum 128 MiB); least-recently-used
  entries are evicted by `Storage`.
- Each download is capped at 8 MiB. Declared `Content-Length` and streamed
  chunks are checked, so an oversized response is rejected before storage.
- `Content-Type` is not trusted. PNG, JPEG, GIF and WebP headers are parsed.
  Dimensions above 8192 pixels per side or 33,554,432 pixels are rejected as
  `ImageTooLarge` before a decoder or the artwork store sees the bytes.
- Cache writes are atomic and private through `Storage`; no artwork path is
  exposed outside the process state directory.

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
