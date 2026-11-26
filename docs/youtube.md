# YouTube search and local playback

YouTube has two deliberately separate halves:

1. **Discovery** is a thin server API. The Pi asks the control server for
   search results using its C009 device bearer. No YouTube Data API key,
   stream URL, ticket or player endpoint is exposed to the Pi.
2. **Playback** is local. The Pi resolves the video with an
   operator-managed `yt-dlp` and plays the selected H.264 streams with
   `mpv`/libmpv over a private Unix socket.

Server-side download or media relay is rejected: it would require cookies or
account credentials on the datacenter IP and would create unnecessary egress
and terms-of-service exposure. The client never has a "media URL" endpoint to
call.

This path is unofficial. yt-dlp can break when YouTube changes; the kill
switch, typed failures and explicit update policy are part of the contract.

## Search contract

```text
GET https://<control-server>/v1/youtube/search?q=<query>&page_token=<opaque>
Authorization: Bearer <device token>
```

- Authentication is the enrolled device bearer (or a paired remote session on
  the server). The token is sent only in the `Authorization` header. It is
  never placed in the URL, the query string, the page token, a stored
  preference or a log field.
- `q` is trimmed and must be 1–100 characters. Control characters are
  rejected before any request.
- `page_token` is opaque, optional and at most 512 bytes. It is echoed exactly
  as percent-encoded by the URL builder.
- The response is `{items:[{video_id,title,channel_title,thumbnail_url}],
  next_page_token}` with at most **10** items. Unknown fields, a longer list,
  a non-11-character `video_id`, an over-long title/channel or a non-official
  thumbnail URL is `invalid_response`.
- `thumbnail_url` must be `https` on `i.ytimg.com` or a `*.ytimg.com` host. The
  launcher is text-only and never fetches thumbnails; validating the host
  prevents a server bug from turning into an arbitrary image request.
- Typed non-fatal failures: `400` bad request, `401/403` unauthorized, `404`
  feature disabled, `429` rate limited, `503` temporarily unavailable, any
  other status as a bounded server code. Failures never stop local Jellyfin
  playback.
- Requests happen only on an explicit submit (local Enter or the Search
  button), never per keystroke. A new submit or Escape while a request is in
  flight aborts the previous task. A late response carries the request id and
  is ignored if it is not the currently pending page.
- The request timeout is 10 seconds and the response body is bounded to
  64 KiB. The server owns the 5-minute upstream cache; the client has no cache
  and does not persist results.

## Local playback flow

1. The wire action is `play_youtube {video_id}`. The identifier must be
   exactly 11 `[A-Za-z0-9_-]` characters. Empty, short, long, URL-like or
   playlist-like values are rejected as `invalid_action` before playback.
2. The client constructs the only URL it will ever hand to the extractor:
   `https://www.youtube.com/watch?v=<validated id>`. A remote cannot supply a
   URL, host, query or media address.
3. If local playback is disabled, the action is rejected as
   `youtube_disabled`; if the build lacks the path it is `unsupported_source`.
4. `yt-dlp` runs with an explicit argument vector and **no shell**:

   ```text
   --ignore-config --no-playlist --no-cache-dir --no-warnings
   --no-progress --simulate --dump-single-json --socket-timeout 15
   --format <fixed selector> https://www.youtube.com/watch?v=<id>
   ```

   There are no cookie, account, browser, playlist or cache flags. The child
   gets a bounded total timeout, bounded stdout (2 MiB), bounded stderr
   (8 KiB) and `kill_on_drop`. stderr is classified and discarded; it is never
   logged, because it can contain a media URL or a challenge page.
5. Format policy: H.264 (`avc1`) with AAC (`mp4a`), **720p30 first**, then a
   muxed progressive H.264 fallback, then **1080p30** only when no 720p option
   exists. The selector contains `fps<=30`, never prefers VP9/AV1 and refuses
   1080p60/720p60. 720p is first because measured 1080p30 playback in labwc
   dropped roughly 72% of frames, versus about 19% at 720p30; under Cage/GLES
   720p30 passes. 1080p30 remains an accepted last resort when a video has no
   720p30 H.264/AAC format, but is not a supported Pi 3B hardware mode.
6. The resolved stream URLs live only in memory for one session. `Debug`
   prints `redacted`, no URL is written to argv, the log, the database or a
   file, and the temporary socket directory is removed on teardown.
7. `mpv` is started with no media in argv and `--ytdl=no`:

   ```text
   --no-config --no-terminal --really-quiet --idle=yes --force-window=yes
   --fullscreen=yes --no-border --keep-open=no --no-resume-playback
   --save-position-on-quit=no --ytdl=no --osc=no --osd-level=0 --vo=gpu
   --gpu-context=wayland --hwdec=<allowlisted> --input-ipc-server=<socket>
   ```

   The resolved video URL is sent with `loadfile`; the separate AAC stream is
   attached with `audio-add` once mpv reports `file-loaded`, over a per-session
   0700 Unix socket. No token or URL is in the process list. mpv's own ffmpeg
   performs local demux/merge; nothing is downloaded or muxed on the server and
   no media file is persisted.
8. `time-pos`, `duration`, `pause` and `media-title` are observed over the
   same IPC socket. Playback advances the UI position and is the evidence used
   for remote acknowledgement.

## Kill switch and configuration

| Key | Default | Meaning |
|---|---|---|
| `youtube_playback_enabled` | `false` | Local kill switch |
| `youtube_ytdlp_path` | absent | Absolute path to the managed extractor |
| `youtube_mpv_path` | absent (`mpv` on `PATH`) | Optional absolute mpv path |
| `youtube_playback_timeout_secs` | `60` | Resolve-and-start bound, `10..=300` |
| `youtube_hwdec` | `"auto-safe"` | `auto-safe`, `v4l2m2m` or `none` |

`youtube_ytdlp_path` must be absolute and must not contain `..`; a relative
or structurally invalid path is rejected by strict configuration validation
and fails startup. An absolute path that does not exist or is not executable
does not fail startup, but it leaves local playback disabled: a Play attempt
reports `youtube_disabled`. `extractor_unavailable` is reserved for a configured
extractor that passed readiness but cannot be spawned or used at play time. Unknown
keys still fail strict TOML parsing, and the cloud policy advertises the
`youtube` capability only when the effective switch is on.

## Failure classes and teardown

| Condition | Typed result |
|---|---|
| Invalid/missing identifier | `invalid_action` before any process |
| Kill switch off | `youtube_disabled` (ack rejected) |
| Extractor missing/unusable | `youtube_disabled` when readiness fails; `extractor_unavailable` (`missing_binary`, …) when configured but spawn/use fails |
| YouTube blocks or challenges | `extractor_failed` (`blocked`) |
| Stale extractor | `extractor_failed` (`stale_extractor`) |
| Rate limited | `extractor_failed` (`rate_limited`) |
| Private/removed video | `extractor_failed` (`video_unavailable`) |
| Playlist expansion | `extractor_failed` (`playlist_rejected`) |
| Resolve/start over budget | `playback_timeout` |
| No H.264/AAC selection | `unsupported_format` |
| mpv missing or IPC failure | `player_unavailable` / `player_closed` / `player_ipc` |

A failed start is not retried automatically. The UI offers Retry only for
failures where a new attempt can help; Retry is another bounded, explicit
attempt. The player never starts a second extractor or mpv while one is
active: a new request cancels the in-flight resolution, stops/reaps the old
child with a bounded wait, then starts the new generation.

`play_youtube` is acknowledged `applied` only after the local snapshot proves
the YouTube source, the same item identifier, `playing`/`paused` mode, no
error and a non-zero position. Loading, resolver success or "process started"
are never enough. A timeout or a reported error produces a rejected/failed ack
with a stable reason.

CEC/keyboard Back, `stop`, a new source, network loss (for a remote-initiated
playback), timeout or any error stops and reaps the child, removes temporary
resources and restores the launcher. The media router serializes YouTube
libmpv and the C006 Jellyfin GStreamer controller; it waits for the previous
owner to report idle before starting the other source.

## UI behavior

- The Home `YouTube` row opens the library with the YouTube filter and a
  search prompt. Typing updates the local field; nothing leaves the process
  until Enter.
- Loading, empty ("Search YouTube with Enter."), error and Retry states are
  distinct; results show title, channel and a `YouTube` source label.
- `More results` appears only when the server returned a continuation token;
  it requests exactly one more page and is disabled while a request is
  pending.
- The player screen shows title, source, method summary
  (`yt-dlp · H.264 720p30 · mpv`), position/duration, pause state and a
  bounded error text. All updates are posted to the Slint event loop; no I/O
  happens on that loop.
- Search results and Jellyfin items remain separate lists. A cloud or YouTube
  failure does not remove the local Jellyfin library.

## Pi evidence and extractor policy

Lab target: Raspberry Pi 3B, Debian 13, kernel 6.18, labwc/Wayland at 1080p,
`mpv 0.40.0`, `pkg-config --modversion mpv` 2.5.0, `ffmpeg 7.1.5`. The probe
used the current PyPI `yt-dlp 2026.11.23` in a throwaway `/tmp` venv. The
distribution `2025.04.30` binary is **not** the playback dependency.

Short public-video probes (video id `6v2L2UGZJAM` unless noted), no soak.
The operator reported visible stutter on the TV; measurement confirmed the
labwc session is the source. The accepted C010 hardware gate is **720p30 under
Cage/GLES**, not labwc:

- **labwc/Wayland remains FAIL.** 720p30 drops ~19% and 1080p30 drops ~72% of
  frames in this compositor session, with both `vo=gpu` and `wlshm`.
- **Nested Cage/GLES 720p30 PASS.** Running a nested Cage compositor with
  `WLR_BACKENDS=wayland WLR_RENDERER=gles2` dropped **2 frames out of 240** at
  30 fps (≈0.8%) for local 720p30 playback.
- **Cage 1080p30 FAIL / unsupported.** The same Cage/GLES path dropped **151
  frames** at 1080p. 1080p30 is explicitly not supported on the Raspberry Pi
  3B.
- **Big Buck Bunny 480p30 is smooth.** `aqz-KE-bpKQ` has no 30 fps 720p/1080p
  H.264 format; the 30 fps cap selects its progressive 480p30 H.264/AAC stream,
  which dropped **1 frame out of 150** and plays smoothly.

| Session / renderer | Format | Frames dropped | Result |
|---|---|---|---|
| labwc, `gpu --gpu-context=wayland`, `v4l2m2m` | 720p30 `136` + mp4a | 35 / 180 (~19%) | FAIL |
| labwc, `gpu --gpu-context=wayland`, `v4l2m2m` | 1080p30 `137` + mp4a | 131 / 181 (~72%) | FAIL |
| labwc, `gpu`, software decode | 720p30 | 27 / 181 (~15%) | FAIL |
| labwc, `wlshm`, `v4l2m2m` | 720p30 | 47 / ~240 (~20%) | FAIL |
| labwc, `wlshm`, software decode | 720p30 | 43 / 150 (~29%) | FAIL |
| nested Cage, `WLR_BACKENDS=wayland WLR_RENDERER=gles2` | 720p30 local | **2 / 240 (~0.8%)** | **PASS** |
| Cage/GLES | 1080p30 | **151** | FAIL / unsupported |
| labwc Big Buck Bunny `aqz-KE-bpKQ` | progressive 480p30 | **1 / 150** | smooth |

labwc detail: `hwdec-current` was `v4l2m2m` for hardware rows and `no` for
software rows; `current-vo` was `gpu` or `wlshm`; `display-fps=60.0` on
`HDMI-A-1`, `paused-for-cache=false`, demuxer cache ~18–20 s, so the drops are
presentation-side, not network. With `--video-sync=display-resample` on
720p30/`gpu`, mpv reported `estimated-display-fps ≈ 24.9`. One 1080p30 run with
the default `gpu` context (no explicit `--gpu-context=wayland`) exited with
**SIGSEGV** during OpenGL shader setup (`returncode -11`). Audio attached in
completed runs (`aid=1`, `audio-codec-name=aac`). Labwc RAM was 107–188 MiB,
FD count 43–64, package temperature 47–59 °C.

The accepted renderer decision:

- C010 hardware gate: **PASS for local 720p30 under Cage/GLES**.
- 1080p30: **FAIL / explicitly unsupported on the Pi 3B**.
- labwc/pixman: **FAIL**, not a supported playback session.
- C013 kiosk will use **Cage**; the C004 UI must be revalidated under Cage
  (fullscreen, focus, input and teardown), not assumed from labwc.
- The client keeps the 720p30-first, 480p-progressive, 1080p-last selector with
  the 30 fps cap; 1080p60 and 720p60 are never offered. `libplacebo`/Vulkan
  remains unreliable on this target, and a video with only 60 fps
  H.264/VP9/AV1 formats is refused as `unsupported_format` rather than
  silently preferring a dropping or desynchronized path.

The `/tmp` venv is a probe only. Production uses the absolute managed path in
`youtube_ytdlp_path`, kept updated with:

```sh
"$HOME/.local/share/tv-client/yt-dlp/bin/pip" install --upgrade yt-dlp
```

After an update, record the new version and rerun the short probe. A playback
classification of `stale_extractor` is the signal to update; the kill switch is
the rollback. Do not add cookies, accounts, DRM/age-restriction bypasses or ad
blocking to the device.

## Known limits

- Age-restricted, member-only and DRM content is unsupported. The client does
  not store cookies or account credentials.
- YouTube can change its private endpoints at any time; yt-dlp maintenance is
  required, and playback can be disabled without a rebuild via the switch.
- Discovery needs a server feature-enabled search endpoint and an enrolled
  device. Without them, the filter reports "YouTube search is not configured"
  and local Jellyfin remains unaffected.
- The accepted tradeoff is that ads, branding and the official embed are not
  preserved. This is not a hosted-player implementation.
- **Renderer support is session-specific:** labwc is FAIL for 720p30/1080p30
  (~19%/~72% drops); Cage/GLES 720p30 PASSes (2/240 drops), Cage 1080p30 is
  unsupported (151 drops). C013 uses Cage, and C004 UI behavior must be
  revalidated under Cage. The 480p30 Big Buck Bunny result is smooth but is
  not a blanket claim for every 480p source.
