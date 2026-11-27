# On-device settings and Jellyfin Quick Connect

The kiosk no longer needs a root-owned `/etc/tv-client/client.toml` edit to
change connections or features. The Settings screen edits the writable state
copy directly and offers TV-side Jellyfin sign-in through Quick Connect.

## Where values are stored

Precedence is fixed:

1. `--config PATH` (explicit; must exist).
2. `<state-dir>/client.toml` on the writable state volume.
3. `/etc/tv-client/client.toml` as a root-owned operator fallback.
4. Built-in defaults.

On the installed kiosk the state directory is `/var/lib/tv-client`, so the
effective writable file is `/var/lib/tv-client/client.toml`, owned by
`tvkiosk` with mode 0600 inside a 0700 directory. When absent, the installer
copies an existing `/etc/tv-client/client.toml` verbatim so an upgrade keeps the
operator's settings; it falls back to `deploy/client.toml.example` only when
neither file exists. The kiosk profile passes the state file explicitly; `/etc`
is kept for operator recovery.

Saving validates the complete configuration first, then writes a 0600
temporary file in the same directory, `fsync`s it, renames it over the target
and `fsync`s the directory. A failed validation or write leaves the previous
file byte-identical. No configuration value or token is logged, and validation
messages name only the field and a static reason.

## Controls

The app-level keyboard focus scope owns every key; there is no second focus
owner and no on-screen keyboard.

| Key | Action |
|---|---|
| Up / Down | Move within the current column |
| Left / Right | Move between columns; never change a value |
| Enter | Start editing a text row, toggle a switch, cycle a choice, or run the action |
| Character / Backspace | Edit the focused text row at the caret |
| Left / Right while editing | Move the caret one character, without leaving the row |
| Home while editing | Move the caret to the start of the value |
| End while editing | Move the caret to the end of the value |
| Enter while editing | Apply the edited value |
| Esc while editing | Cancel the edit and restore the previous value |
| Esc | Cancel an active Quick Connect poll, otherwise return home |

The Settings footer repeats the editing keys: arrows move, Enter edits or
activates, Home/End move the caret while editing, and Esc cancels or goes
back.

Toggles and choices change only with Enter or a pointer click on the row; the
arrow keys always move focus when no field is being edited.

Text rows render in a clipped, full-width value box with an amber border and a
`|` caret marker while editing. The marker is display-only; the stored value
never contains it. Long values are elided at the end of the box.

Pointer behaviour is explicit and local-only:

- clicking any part of a row focuses it and activates it;
- clicking a text value box focuses the row, starts editing when needed, and
  places the caret at the nearest character to the click (Slint exposes no
  glyph metrics, so this is an approximation rather than an exact text
  offset);
- clicking a different row while editing commits the current value first; an
  invalid value keeps its field open instead of silently discarding the edit;
- toggles are a large custom pill and the whole row is the hit target, so no
  small stock `CheckBox` can steal focus from the app-level keyboard scope.

## Rows

- `Jellyfin URL` and `Control server URL`: empty means absent. URLs are
  validated with the same rules as the configuration file; HTTP is accepted
  only for an explicitly local Jellyfin host.
- `Cloud control`, `YouTube search`, `YouTube playback`: boolean features.
  Enabling cloud or YouTube without a server URL is rejected at Save.
- `yt-dlp path`: absolute path to the operator-managed extractor. Playback
  still requires the kill switch and an executable file.
- `Playback timeout (s)`: whole seconds, `10..=300`.
- `Hardware decode`: `auto-safe`, `v4l2m2m` or `none`.
- `UI safe area (%)`: whole percent, `0..=10`, default `0` (disabled). A
  nonzero value removes that percentage of the short edge from every side of
  the launcher frame, so the header, screen host and footer stay aligned. The
  screen host takes the space between the fixed header and footer; each
  screen scales its own base to that host height, so rows compact rather than
  push the footer off-screen. Enable it only if the panel crops its edges.
  The running value comes from the effective configuration at startup, so a
  change is applied after Restart.
- `Pair a remote`: starts the existing local-only cloud pairing code.
- `Jellyfin sign-in` / `Jellyfin sign out`: see below.
- `Save settings`: validates and writes the state copy, then shows
  `Saved. Restart to apply.`
- `Restart app`: exits the process cleanly (status 0). The kiosk profile's
  watchdog loop starts it again with the new configuration. Unsaved edits are
  discarded.
- `Back to home`: leave Settings.

The Slint loop performs no file or network I/O. Save, sign-in, sign-out and
restart requests are posted to a `tv-settings` worker thread; configuration
writes run on `spawn_blocking` and network calls are bounded and cancellable.
Cancel, sign-out, restart and shutdown signal the active Quick Connect request
immediately through a shared cancel handle, so an in-flight poll or token
exchange stops without waiting for the queued command.

## Jellyfin Quick Connect

Quick Connect is the only sign-in path on the TV; no password is typed,
stored or logged. Manual password authentication remains available in the
Jellyfin client API for tests, but the kiosk does not use it.

Lifecycle:

1. `Jellyfin sign-in` calls `QuickConnect/Initiate` with the existing
   `MediaBrowser` client identity and a 15 s deadline.
2. The six-to-eight-character code is shown on the TV. The secret stays in
   memory only; it is never written to a file or log.
3. The worker polls `QuickConnect/Connect` every two seconds, each poll
   bounded at 15 s, until approval, expiry or cancellation. `Esc` cancels
   immediately: the shared handle stops an in-flight poll or token exchange.
4. On approval it calls `AuthenticateWithQuickConnect` and stores the access
   token through `Storage::write_credential` (0600, atomic). If cancellation
   lands while that exchange is finishing, the freshly written token is
   removed before the flow reports cancellation. Otherwise the worker shows
   `Quick Connect succeeded; press Restart to load Jellyfin`; it does not
   claim the library is ready. The playback thread built its `JellyfinClient`
   at startup, found no token and entered `run_unavailable`, so only the
   Restart row can rebuild a signed-in client. The settings worker stays alive
   so a Save queued during sign-in is still written.
5. On expiry or cancellation the code is cleared and a typed message is
   shown. Transient timeouts keep the code visible until the request's
   600 s approval window ends.
6. `Jellyfin sign out` removes the stored local token before calling
   `Sessions/Logout`. The stored token and the in-memory session are
   distinct: the long-lived playback client on the `tv-playback` thread keeps
   its own authenticated session and cannot be told to forget it. After local
   token cleanup succeeds, every outcome shows a notice and leaves the
   Settings worker alive; press Restart to drop the in-memory client and let
   the supervisor rebuild signed out. A timed-out or failed revocation can
   leave the cached artwork and the server session until that restart; the
   local token is already gone:
   - success shows `Signed out of Jellyfin; press Restart to apply`;
   - a revocation timeout shows `Jellyfin sign-out timed out; local token
     cleared; press Restart to finish signing out`;
   - a failed revocation shows the typed error followed by
     `; local token cleared; press Restart to finish signing out`.

   If local cleanup itself fails (`JellyfinSignOutError::Storage`), nothing
   was removed, so the signed-in row is kept,
   `Jellyfin storage is unavailable` is shown, and no restart happens: a
   restart would only restore the same session.

The worker restores a previously stored token at startup with
`Users/Me` (5 s deadline) so the row can show the signed-in user without a new
sign-in. A rejected token is removed.

Errors are mapped to static text (`Quick Connect is disabled on the server`,
`Quick Connect expired`, `Jellyfin is not reachable`, and so on). Typed
`JellyfinError` values never carry URLs, headers, credentials or response
bodies, and the worker never logs a code, secret or token.

## Verification

`tests/settings.rs` covers:

- configuration precedence and explicit-path handling;
- 0600/0700 persistence, atomic replacement and invalid-config refusal;
- safe-area default (`0`) and clamping to `0..=10`;
- caret movement, pointer caret placement and display-only caret marker;
- whole-row pointer activation of settings rows and local toggles;
- settings edit, save and restart through the UI model;
- Quick Connect initiate/poll/approve, expiry and cancellation against a local
  mock server, including cancellation during the token exchange;
- sign-out clearing the local token before revocation fails or times out;
- debug/log output containing no Quick Connect secret or access token.

The restart decisions themselves are not covered by `tests/settings.rs`: they
live in the `run_settings` worker in `src/main.rs`, which needs the Slint UI
handle and the supervisor's restart path. The library tests cover the storage
and revocation halves only; the restart after Quick Connect success and after
sign-out cleanup must be exercised on the kiosk.

The UI navigation suites remain in `tests/ui_navigation.rs`. A native-ui build
check (`cargo check --features native-ui`) must run in Docker or on the Pi
because the workstation lacks the Slint/GStreamer development packages.
