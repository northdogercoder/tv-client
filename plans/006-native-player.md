# Plan 006: Implement exclusive native GStreamer playback

> Executor: C006. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `cdab5fbf2d984ec83f9abee210e41d1c61872180` (post-C005 merge, 2026-11-25). Advisor verified the baseline and the absent target files below. Then run `git diff --stat cdab5fbf2d984ec83f9abee210e41d1c61872180..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/player/ src/bin/tv-native-player.rs tests/player_state.rs docs/player.md docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C002, C003
- **Category**: direction
- **Planned at**: cdab5fbf2d984ec83f9abee210e41d1c61872180, 2026-11-25
- **State**: TODO (index authoritative)

## Baseline evidence (advisor, 2026-11-25)

- Baseline `cdab5fb`; C002 merged `66d5657`, C003 merged `0bf089c`, C005 merged `c157130`; `bash scripts/check.sh` exit 0 (53 tests, 1 ignored).
- Existing interfaces: `ClientConfig`/`Storage`; `JellyfinClient` (auth/library/artwork); reducer `AppState`/`Action`/`Effect`.
- GStreamer: host dev headers `1.28.2`; Pi dev headers `1.26.2`; Pi Rust `1.100.0`; `v4l2h264dec`, `waylandsink`, `playbin3`, `pulsesink` present.
- C002 evidence: 720p30/1080p30 decode + HDMI audio + window recovery at ~real time, with ~2.8%/2.7% QoS drops under labwc `WLR_RENDERER=pixman`; 30-min soak NOT RUN.
- Absent: `src/player/`, `src/bin/tv-native-player.rs`, `tests/player_state.rs`, `docs/player.md`.
- MSRV: `gstreamer`/`gstreamer-video`/`gstreamer-audio` 0.25.4 require Rust 1.92; bump `rust-version` to 1.92 and update `docs/build.md` (scope amendment).

## Why this matters
One active backend owns video/audio. Use Rust GStreamer bindings, separate supervised child to isolate decoder errors and create fullscreen Wayland surface. Avoid CPU frame copies unless measured.

## Current state
C002, C003 and C005 are merged; `bash scripts/check.sh` exits 0 and the config/storage/reducer/Jellyfin interfaces above are available. C006 target files are absent; add `pub mod player;` and gated GStreamer dependencies without breaking the headless gate. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` has been executed on the bound baseline; player and hardware commands have not. Real hardware checks need an operator report with command output, not simulated success.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/player/`
- `src/bin/tv-native-player.rs`
- `tests/player_state.rs`
- `docs/player.md`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c006-native-player` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Define player interface and IPC

Typed start/pause/seek/volume/stop and events Ready/Position/Ended/Error/Tracks; JSON lines via inherited stdin/stdout pipe, bounded line length; stderr redacted; no token in argv. Child gets URL/header via pipe; parent kill/reap on deadline and generation-check events. Use playbin3 or tested explicit pipeline with proven decoder/sink; prefer hardware, no speculative property names.

**Verify:** `cargo test --locked --no-default-features --test player_state` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Implement playback and teardown

Use `gstreamer` 0.25.4 (GStreamer 1.26 API) with optional dependencies behind the `gstreamer-player` feature and `rust-version = "1.92"`; keep the headless gate free of GStreamer. Child initializes GStreamer, handles bus, buffering, seek, audio/subtitles, pause, volume and EOS; track stable playback clock; use validated source only, explicitly set source headers if supported for HLS segments, reject redirects across origin. If headers unavailable use Jellyfin-provided authenticated stream URL in memory only, never log or argv. Ensure UI reappears and CEC back works via parent even if child focused (C008). Keyboard Esc in child via its own Wayland window/event or tested global input routing.

**Verify:** `cargo build --release --locked --features gstreamer-player` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 3: Validate fixture playback

Use generated or legally sourced 720p/1080p H264 AAC fixture and audio-only, seek and stop 20 times; report actual decoder and sink, memory and FD count.

**Verify:** `test -s docs/player.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Fake child tests state transitions, hung child, late events, errors, teardown; physical Pi must demonstrate video, sound, focus and clean stop.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- Hardware path unavailable; no reliable input/back while child is visible; GStreamer API or source auth differs.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
