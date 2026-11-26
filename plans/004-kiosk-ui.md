# Plan 004: Build a native remote-first Slint launcher

> Executor: C004. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `806cb297a9e89362adaa0b1592eb0d48cf4a0093` (post-C006 merge, 2026-11-25). Advisor verified the baseline and the absent target files below. Then run `git diff --stat 806cb297a9e89362adaa0b1592eb0d48cf4a0093..HEAD -- Cargo.toml Cargo.lock build.rs src/lib.rs src/main.rs src/ui.rs ui/ tests/ui_navigation.rs docs/design.md docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C002, C003
- **Category**: direction
- **Planned at**: 806cb297a9e89362adaa0b1592eb0d48cf4a0093, 2026-11-25
- **State**: TODO (index authoritative)
- **License decision**: GPL-3.0-only (advisor, 2026-11-25; private project, no commercial distribution planned)

## Baseline evidence (advisor, 2026-11-25)

- Baseline `806cb29`; C002, C003, C005 and C006 are merged; `bash scripts/check.sh` exit 0 (17 player tests plus prior suites) and the gated GStreamer feature build passes.
- Slint `1.19.1` requires Rust 1.95; pinned toolchain is 1.100.0. Bump `rust-version` to 1.95 and update `docs/build.md` (scope amendment). License is GPL-3.0-only.
- Host build dependency gap: `cargo check --features native-ui` fails at `yeslogic-fontconfig-sys` because the host lacks `fontconfig.pc`/`xkbcommon.pc`; host `sudo` needs a password. Compile the Slint feature in Docker (`rust:1.100-slim` + `libfontconfig1-dev libxkbcommon-dev`) or on the Pi. The headless `--no-default-features` gate must remain dependency-light.
- Pi environment: labwc 0.20.2 Wayland session; `fontconfig 2.15.0`, `xkbcommon 1.13.1`, `wayland-client 1.24.0`, EGL/GL present. C002 proved 720p/1080p H.264 with ~2.8%/2.7% drops; C006 provides the exclusive player supervisor.
- Absent: `build.rs`, `src/ui.rs`, `ui/`, `tests/ui_navigation.rs`, `docs/design.md`.

## Why this matters
Native 10-foot launcher with no persistent browser. All I/O stays off Slint event loop.

## Current state
C002, C003, C005 and C006 are merged; `bash scripts/check.sh` exits 0 and the reducer/storage/Jellyfin/player interfaces are available. C004 target files are absent; add the optional Slint dependency, `build.rs` and UI modules without breaking the headless gate. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` is verified on the bound baseline; the `native-ui` feature check has not run on the host because `fontconfig` dev headers are missing. Run it in Docker or on the Pi and record which. Real hardware checks need an operator report with command output, not simulated success.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `Cargo.toml`
- `Cargo.lock`
- `build.rs`
- `src/lib.rs`
- `src/main.rs`
- `src/ui.rs`
- `ui/`
- `tests/ui_navigation.rs`
- `docs/design.md`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c004-kiosk-ui` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Build UI skeleton

Use Slint `1.19.1` (MSRV 1.95) as an optional `native-ui` dependency plus `slint-build`; winit Wayland software or FemtoVG based on C002; use the GPL-3.0-only option and record that choice plus its distribution obligations in `docs/design.md`. The host lacks `fontconfig` dev headers, so run the feature check in Docker (`rust:1.100-slim` + `libfontconfig1-dev libxkbcommon-dev`) or on the Pi; keep the headless gate free of Slint. Create ui/app.slint, home.slint, library.slint, player.slint, settings.slint, pair.slint, cast.slint; 16:9 layout scaled at 720/1080, large type, contrast, visible focus, no unbounded image models. Home rows: resume, Jellyfin, YouTube, Cast, settings. Add local physical confirmation modal only from local input.

**Verify:** `cargo check --locked --features native-ui` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Connect view model and navigation

src/ui.rs maps reducer state to UI and events, use weak handles/event-loop invocation; keyboard arrows/enter/escape, repeat throttling, stable focus when lists refresh, search keyboard and source-labelled results, loading/empty/error/retry. Keep remote event origin separate from local input.

**Verify:** `cargo test --locked --no-default-features --test ui_navigation` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 3: Run on Pi

Launch under the proven labwc 0.20.2 Wayland session (Cage is installed but not the active session); verify 720p/1080, text, navigation and focus recovery; document timings and screenshots in docs/design.md (no secrets).

**Verify:** `cargo build --release --locked --features native-ui` → exit 0; all named checks pass; inspect actual assertions/output.

## Test plan
Navigation model tests; actual physical arrows and back; no freeze during mock slow fetch; local physical approval cannot be driven remotely.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- Slint licensing not selected; actual target renderer fails; accessibility/focus lost after child window.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
