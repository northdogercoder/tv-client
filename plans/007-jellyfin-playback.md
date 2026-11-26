# Plan 007: Negotiate Jellyfin playback and report watch progress

> Executor: C007. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `7ab8998a937519d03fba2fa4a6193f1fdd1ab943` (post-C004 merge, 2026-11-26). Advisor verified the baseline and the absent target files below. Then run `git diff --stat 7ab8998a937519d03fba2fa4a6193f1fdd1ab943..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/main.rs src/jellyfin/ src/playback.rs src/ui.rs ui/ tests/jellyfin_playback.rs docs/jellyfin.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C004, C005, C006
- **Category**: direction
- **Planned at**: 7ab8998a937519d03fba2fa4a6193f1fdd1ab943, 2026-11-26
- **State**: TODO (index authoritative)

## Baseline evidence (advisor, 2026-11-26)

- Baseline `7ab8998`; C004, C005 and C006 are merged; host `bash scripts/check.sh` exit 0 (90 tests, 1 live ignored) and the gated GStreamer feature build/clippy pass.
- Existing interfaces: `JellyfinClient` (auth, library, artwork, `playback_info`), `ClientConfig`/`Storage`, `PlayerSession`/supervisor, `UiModel`/`UiCommand`; fullscreen Slint launcher verified on the Pi.
- Jellyfin test server: 12.4.0 at `http://jellyfin:8096`, limited non-admin user `test`; credentials in `~/.config/tv-client/jellyfin-test.env` (0600, outside the repo). Use `Authorization: MediaBrowser ..., Token="..."`; `X-Emby-Token` returns 401 on this server.
- Pi: labwc 0.20.2, 1080p HDMI, GStreamer 1.26.2 `v4l2h264dec`/`waylandsink`, C006 short-run playback evidence. C002's 30-min soak was waived.
- Absent: `src/playback.rs`, `tests/jellyfin_playback.rs`. `docs/jellyfin.md` exists from C005 and is extended here.

## Why this matters
Do not force unsupported codecs onto the Pi or proxy through OVH; use PlaybackInfo device profiles and server-selected streams.

## Current state
C004, C005 and C006 are merged; `bash scripts/check.sh` exits 0 and the Jellyfin/player/UI interfaces above are available. C007 target files are absent; extend the existing modules without breaking the headless or native-ui gates. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` is verified on the bound baseline; Jellyfin playback and hardware commands have not. Real hardware checks need an operator report with command output, not simulated success.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/main.rs`
- `src/jellyfin/`
- `src/playback.rs`
- `src/ui.rs`
- `ui/`
- `tests/jellyfin_playback.rs`
- `docs/jellyfin.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c007-jellyfin-playback` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Negotiate device profile

Implement src/jellyfin/playback.rs POST Items/{id}/PlaybackInfo using installed OpenAPI. Profile H264 8-bit SDR <= tested resolution/30fps, AAC stereo, HLS H264/AAC transcode, direct play/remux only proven containers/codecs; start 6 Mbps cap 10, lower for relay. HEVC/AV1/VP9/HDR transcode; request burn-in for unsupported image/ASS subtitles; verify Jellyfin transcode capability. Preserve base path, mediaSourceId/playSessionId, stream URL origin allowlist.

**Verify:** `cargo test --locked --no-default-features --test jellyfin_playback` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Wire UI and reporting

Orchestrate native player from reducer; request resume and choose compatible media source; report Sessions/Playing, Progress every 10s plus pause/seek, Stopped once best effort, tick conversion with bounds. Re-negotiate on track changes when necessary, release transcodes via documented endpoint where supported; bounded progress retry journal with freshness and user isolation, no stale overwrite. Add continue watching, next episode, subtitles/audio menu, error retry.

**Verify:** `cargo test --locked --no-default-features --test jellyfin_playback` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 3: Real playback matrix

Use limited test user and own media: H264 direct/remux, HEVC transcode, subtitles, episode resume, network interruption. Confirm dashboard play method and progress, OVH down still works. Keep runs short unless the operator asks for a soak; record actual durations. Record in docs/jellyfin.md.

**Verify:** `test -s docs/jellyfin.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Mock requests verify profile, ticks, stop/report sequence, redirects, expired token, progress retry; Pi 30 min playback and watch state matches Jellyfin.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- Jellyfin cannot transcode required files in real time; profile cannot be verified; authentication header leakage or codec fallback not tested.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
