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
   1080p60/720p60. 720p is first because measured 1080p30 playback dropped
   roughly 72% of frames on the Pi, versus about 19% at 720p30; 1080p30 remains
   an accepted last resort when a video has no 720p30 H.264/AAC format.
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
does not fail startup, but it leaves local playback effectively disabled. A
Play attempt then reports the typed `extractor_unavailable` failure. Unknown
keys still fail strict TOML parsing, and the cloud policy advertises the
`youtube` capability only when the effective switch is on.

## Failure classes and teardown

| Condition | Typed result |
|---|---|
| Invalid/missing identifier | `invalid_action` before any process |
| Kill switch off | `youtube_disabled` (ack rejected) |
| Extractor missing/unusable | `extractor_unavailable` (`missing_binary`, …) |
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

Short public-video probe (video id `6v2L2UGZJAM`), no soak. The operator
reported visible stutter on the TV; the measurements below confirm it. **The Pi
playback gate is FAIL / not smooth.**

| Configuration (short run) | Format | Renderer / hwdec | Time advanced | Frame drops | CPU | Result |
|---|---|---|---|---|---|---|
| 1080p30 | `137` avc1.640028 + mp4a | `gpu` (explicit `wayland`), `v4l2m2m` | 5.61 s / 6.03 s | **131 / 181 (~72%)** | 7.34 s | stutter |
| 720p30 | `136` avc1.4D401F + mp4a | `gpu` (explicit `wayland`), `v4l2m2m` | 6.04 s / 6.01 s | **35 / 180 (~19%)** | 4.28 s | stutter |
| 720p30 | `136` + mp4a | `gpu`, software decode | 6.04 s / 6.05 s | 27 / 181 (~15%) | 6.02 s | stutter |
| 720p30 | `136` + mp4a | `wlshm`, `v4l2m2m` | 7.61 s / 8.10 s | 47 / ~240 (~20%) | 19.51 s | stutter |
| 720p30 | `136` + mp4a | `wlshm`, software decode | 5.21 s / 5.01 s | 43 / 150 (~29%) | 15.36 s | stutter |
| 480p30 (the operator's Big Buck Bunny, `aqz-KE-bpKQ`) | progressive H.264/AAC | `gpu`, `v4l2m2m` | 5.03 s / 5.00 s | **1 / 150** | 2.71 s | smooth |

- `hwdec-current` was `v4l2m2m` for the hardware rows and `no` for the
  software rows; `current-vo` was `gpu` or `wlshm`. Display refresh reported
  60 Hz (`display-fps=60.0`, output `HDMI-A-1`), `paused-for-cache=false`, and
  demuxer cache stayed ~18–20 s, so the drops are renderer/present-side, not
  network buffering.
- With `--video-sync=display-resample` on 720p30/`gpu`, mpv reported
  `estimated-display-fps ≈ 24.9`, below the 29.97 fps source.
- One 1080p30 run with the default `gpu` context (no explicit
  `--gpu-context=wayland`) exited with **SIGSEGV** during OpenGL shader setup
  (`returncode -11`). The client now passes `--gpu-context=wayland` explicitly.
- Audio attached correctly in every completed run (`aid=1`,
  `audio-codec-name=aac`), so the stutter is video-presentation, not a missing
  audio track.
- RAM was 107–175 MiB, FD count 43–64, and package temperature 47–58 °C during
  these short runs.

The operator's Big Buck Bunny case is fixed by the 30 fps cap: it has no 30 fps
720p/1080p H.264 format, so the selector now chooses its progressive 480p30
H.264/AAC format, which measured 1 dropped frame in 150 and clearly smoother
playback. The least-bad measured high-resolution option remains 720p30 with
`vo=gpu`/`v4l2m2m`; the client prefers 720p30 and keeps 1080p30 only as a last
resort. **This is not a blanket smooth-playback claim and the Pi 720p/1080p
gate is not passed.** A renderer fix (Wayland dmabuf/zero-copy, compositor
renderer, or a faster present path) is still required for true 720p30/1080p30. `libplacebo`/Vulkan remains unreliable on this target, 1080p60
is not offered, and a video with only 60 fps H.264/VP9/AV1 formats is refused
as `unsupported_format` rather than silently preferring a desynchronized or
dropping path.

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
- **Pi renderer gate is FAIL for 720p30/1080p30:** those paths drop roughly
  15–29% and 72% of frames respectively in the current labwc/Wayland session.
  The 30 fps-capped progressive 480p30 fallback measured 1 dropped frame in
  150 for Big Buck Bunny. Keep runs short and do not generalize from that one
  smooth case; a zero-copy/faster renderer is still needed for 720p/1080p.
