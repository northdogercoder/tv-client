# Plan 010: Implement YouTube discovery and native local playback

> Executor: C010. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan and the current `HANDOFF.md` before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is UNBOUND until dispatch. Before dispatch the advisor binds the actual `git rev-parse HEAD` and the current `HANDOFF.md` hash, then re-reads the handoff. Then run `git diff --stat <BOUND_SHA>..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/main.rs src/youtube/ src/player/ src/ui.rs ui/ tests/youtube.rs docs/youtube.md`. If paths changed, reconcile against live code first. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C006, C008, C009
- **Category**: direction
- **Planned at**: UNBOUND (amended 2026-11-25 for native local playback)
- **State**: TODO (index authoritative)
- **Cross-repo contract**: `HANDOFF.md` (untracked live handoff; baseline hash `7c9df0cc102c578700189602dc188dc603eb3703095c6704e1c69105969b9d8d`). Server S006 Phase A staging `ovh-1.minotaur-gacrux.ts.net`, release `0.1.0-ee4814880aca` (branch `advisor/s006-youtube`, commit `ee4814880aca8278ba6fd1f840d8db84e4157c3c`). Re-read the handoff before dispatch and integration.

## Why this matters
The operator split YouTube work: the server owns YouTube Data API search/browse only, while the Pi resolves and plays locally with yt-dlp + libmpv + ffmpeg. No media URL, API key or player ticket crosses the wire. Playback bypasses the official embed/ads/branding path; yt-dlp is unofficial and may break. This is the accepted tradeoff.

## Baseline evidence (advisor, 2026-11-25)
- Server contract: `GET https://<server>/v1/youtube/search?q=<query>&page_token=<opaque>` with exactly one of device bearer or paired remote session; device bearer is the C009/C010 credential. Feature-off `404`, key/upstream `503`, bad auth `401`, bad query/token `400`, rate/quota `429`. Response `{items:[{video_id,title,channel_title,thumbnail_url}],next_page_token}`; at most 10 items; no stream URL/ticket/player endpoints.
- Limits: query 1–100 chars; page token <=512 chars; 5-minute server cache; explicit submit only; `thumbnail_url` is official i.ytimg.com and is not a playback source.
- Pi at amendment: `ffmpeg 7.1.5`; `mpv`, `yt-dlp`, `libmpv-dev` **ABSENT**. Install prerequisite: `sudo apt-get install -y mpv yt-dlp libmpv-dev`; verify `mpv --version`, `yt-dlp --version`, `pkg-config --modversion mpv`.
- Pi playback context: labwc 0.20.2 with `WLR_RENDERER=pixman`; C002 found ~2.8%/2.7% presentation drops at 720p/1080p. Prefer H.264 <=720p and report actual codec/hardware decode.
- Repo baseline: C002, C003 and C005 merged; C006 provides the exclusive player supervisor; C008 input and C009 device/auth are prerequisites.

## Current state
C002/C003/C005 are merged. C006 is in progress and must expose a backend-agnostic exclusive-owner seam; C008 and C009 are TODO. New files below are target paths; existing code must not be overwritten. Rust package is `tv-client`; typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. Never log tokens, media URLs, cookies, or full child output.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |
| Pi tools | `ssh raspberry 'mpv --version; yt-dlp --version; pkg-config --modversion mpv'` | versions present |

`bash scripts/check.sh` is verified on the bound baseline only after C006/C008/C009 are merged; playback commands are not run until the Pi tools are installed.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/main.rs`
- `src/youtube/`
- `src/player/`
- `src/ui.rs`
- `ui/`
- `tests/youtube.rs`
- `docs/youtube.md`

**Out of scope**: sibling repository, protocol contract edits, plans index edits by executor, Chromium/IFrame/ticket/player-bridge code, media relay/proxy, arbitrary URL playback, cookies/account credentials, DRM/age-restriction bypass, ad blocking, persistent media, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment.

## Git workflow
Branch `advisor/c010-youtube` from the reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD`, untracked files, assertions, and reruns gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Search and browse via the server

Use the C009 device bearer; call `GET /v1/youtube/search` with query and opaque page token. Explicit submit only (no per-keystroke request), cancel in-flight requests, render title/channel/official thumbnail, support `next_page_token`, and surface `400/401/404/429/503` as typed non-fatal states. Never request or store an API key or media URL. Keep cloud failure non-fatal for local Jellyfin.

**Verify:** `cargo test --locked --no-default-features --test youtube` → exit 0; inspect assertions for auth header, paging, limits, cancellation and error mapping.

### Step 2: Resolve and play locally

Accept only the existing `play_youtube` action with a validated 11-character `video_id`; reject arbitrary URLs and any server-supplied media URL. Resolve locally with yt-dlp using an explicit argument vector and no shell: fixed `https://www.youtube.com/watch?v=<id>` constructed from the validated ID, `--no-playlist`, bounded timeout, no persistent cache/cookies, fixed H.264 <=720p selection (prefer `avc1` + `mp4a`; do not prefer VP9/AV1). Feed libmpv directly or through its private IPC socket; use ffmpeg only for local mux/remux. Never place tokens in argv or logs; never persist media or resolved URLs.

**Verify:** `cargo test --locked --no-default-features --test youtube` → exit 0; mock yt-dlp/mpv lifecycle, invalid ID, missing binary, non-zero exit, timeout, cancellation, late events, duplicate actions.

### Step 3: Exclusive ownership, input and cleanup

Integrate with the C006 exclusive player supervisor; never run a Jellyfin GStreamer pipeline and a YouTube libmpv pipeline concurrently. Stop/reap the child on Back/CEC, stop, network loss, timeout or error; remove temporary files; restore the launcher and focus. Ack `play_youtube` as applied only after playback actually starts; never ack before application. Reuse C008 input/back handling; no overlay that blocks native controls; no ad blocking, extraction beyond local playback, or server relay.

**Verify:** `cargo build --release --locked --features gstreamer-player` → exit 0 (if C006 features are required); plus the mock tests above.

### Step 4: Real Pi playback

Install/verify `mpv`, `yt-dlp`, `libmpv-dev`; use a public video and the 720p H.264 path. Run short load/seek/pause/stop cycles (not a long soak unless the operator asks), report actual codec/decoder, CPU, memory, temperature, FD count, and clean teardown. Do not run at the same time as C006/Jellyfin playback.

**Verify:** `test -s docs/youtube.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Mock server search responses and mock yt-dlp/mpv children: auth header, paging, 401/429/503, invalid/missing ID, missing binary, non-zero exit, timeout, cancellation, duplicate/late events, no persistent URL/token, exclusive ownership, clean teardown. Real Pi public-video test at 720p H.264 with CPU/memory/thermal and UI/focus recovery.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- `mpv`, `yt-dlp`, `libmpv-dev` unavailable; Pi cannot decode a usable 720p H.264 path; exclusive ownership cannot be guaranteed; server contract/handoff hash mismatches; no legitimate local playback path; yt-dlp is blocked and the operator declines the accepted risk. Do not mark DONE on simulated results.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step.

## Maintenance notes
Pin/update policy and a kill switch for yt-dlp are required before release. Age-restricted/member-only/DRM content is unsupported; do not add cookies or account credentials to the device. Report the actual selected codec and whether hardware decode is used. Ads/branding/ToS are not preserved; this is the accepted consequence. Re-read `HANDOFF.md` before integration; the server S006 Phase A branch may not be merged.
