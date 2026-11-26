# Plan 002: Prove Pi graphics, H.264, audio and networking before UI work

> Executor: C002. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is UNBOUND because this is a new repo. Advisor records actual baseline SHA with `git rev-parse HEAD` and live file excerpts (or explicitly absent) before dispatch. Then run `git diff --stat <BOUND_SHA>..HEAD -- scripts/probe-pi.sh docs/hardware-report.md docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C001
- **Category**: direction
- **Planned at**: UNBOUND (greenfield), 2026-11-25
- **State**: TODO (index authoritative)

## Why this matters
Pi 3B+ has 1 GB and nominal H.264 1080p30 decode. Hardware acceleration, window switching and browser performance are not guaranteed by specifications.

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
- `scripts/probe-pi.sh`
- `docs/hardware-report.md`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c002-hardware-spike` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Capture real target capabilities

Record OS release, uname -m, apt versions, KMS, DRM, free -m, vcgencmd get_throttled, v4l2-ctl --list-devices, gst-inspect-1.0 for v4l2h264dec/playbin3/waylandsink and audio sinks. Probe over wired ethernet and Tailscale direct vs relay; script read-only, no tokens.

**Verify:** `bash scripts/probe-pi.sh` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Test playback inside actual console session

From physical unprivileged login run Cage with GStreamer test video/audio, then licensed local H264 AAC MP4 fixture at 720p and 1080p30; use explicit hardware decoder and waylandsink when present; check frame drops and HDMI audio. Use a disposable demo/temporary directory outside the repository for a Slint placeholder; test sequential Slint placeholder→video window→launcher focus; use a second window only while playback active. Log decoder actually selected, CPU, RSS and temp.

**Verify:** `test -s docs/hardware-report.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

### Step 3: Record pass/fail and fallback

Report 30 min each video mode, stable audio, no crash, no throttling, no sustained frame loss (>1% proposed). If 1080 fails test 720; if Cage fails evaluate minimal Weston shell or single surface using GStreamer appsink only if benchmark passes; do not switch architecture without ADR.

**Verify:** `grep -E "^Core gate: (PASS|BLOCKED)" docs/hardware-report.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Hardware report includes actual command outputs/versions and 720p/1080p results, not inferred from mock tests. Core gate PASS requires 720p30 usable hardware path and window recovery; otherwise BLOCKED.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- No real Pi/TV available; hardware decoder unavailable or unacceptable; compositor cannot restore focus; do not mark DONE on simulated results.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
