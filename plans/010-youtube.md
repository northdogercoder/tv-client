# Plan 010: Implement YouTube discovery and on-demand player bridge

> Executor: C010. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is UNBOUND because this is a new repo. Advisor records actual baseline SHA with `git rev-parse HEAD` and live file excerpts (or explicitly absent) before dispatch. Then run `git diff --stat <BOUND_SHA>..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/main.rs src/youtube/ src/ui.rs ui/ tests/youtube.rs docs/youtube.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C006, C008, C009
- **Category**: direction
- **Planned at**: UNBOUND (greenfield), 2026-11-25
- **State**: TODO (index authoritative)

## Why this matters
Official IFrame playback only; Chromium is not a permanent desktop. Target 720p30 where achievable; player API cannot guarantee resolution/codec.

## Current state
No repository source was provided. Expected state: prerequisite plans merged, with their tested interfaces available. Before dispatch, advisor confirms actual tree and adds excerpts if needed. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (created in 001) |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

Commands are proposed greenfield gates, not previously executed on this repo. The bootstrap establishes them; advisor updates this table if actual conventions differ. Real hardware checks need an operator report with command output, not simulated success.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/main.rs`
- `src/youtube/`
- `src/ui.rs`
- `ui/`
- `tests/youtube.rs`
- `docs/youtube.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c010-youtube` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Implement discovery and player lifecycle

Use cloud search with debounce only explicit submit, source attribution, paging, quota errors, thumbnails; accept validated YouTube video ID/link parser (youtube.com/watch, youtu.be; no arbitrary URL fetch). Request ticket, launch Chromium --ozone-platform=wayland --kiosk --user-data-dir=<ephemeral dir> fixed first-party /player#ticket; sandbox ON, no remote debug, browser runs non-root. Track PID group, cleanup profile after exit, no process args in logs.

**Verify:** `cargo test --locked --no-default-features --test youtube` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Bridge actions and events

On ready/position/errors update reducer only matching playback_id; use player_control and player_close, await ack; stop/reap on back, network disconnect or timeout. Physical CEC Back + keyboard escape proof required. Never overlay/block official controls or ads; use visible controls and branding, no extraction or ad blocking.

**Verify:** `cargo test --locked --no-default-features --test youtube` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 3: Measure real Chromium

Use official embeddable video, inspect stats via supported diagnostics (not bypass), 30 min at manually selected 720p if available; check CPU/memory/thermal, 20 open/close cycles. Report restricted videos, autoplay/referrer errors, disable feature if unacceptable.

**Verify:** `test -s docs/youtube.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Mock ticket/session failure, invalid ID, late player event, cancellation, no persistent profile; real test with S006 and YouTube. No production claim if disabled due hardware.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- Browser OOM/overheats or cannot restore UI; player violates official policies; no legitimate embedding path. Optional lower-res supported only if user accepts.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
