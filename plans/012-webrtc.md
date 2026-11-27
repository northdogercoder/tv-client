# Plan 012: Add optional browser screen sharing receiver

> Executor: C012. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is UNBOUND because this is a new repo. Advisor records actual baseline SHA with `git rev-parse HEAD` and live file excerpts (or explicitly absent) before dispatch. Then run `git diff --stat <BOUND_SHA>..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/main.rs src/cast/webrtc.rs src/ui.rs ui/ tests/webrtc.rs docs/webrtc.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P3
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C006, C008, C009
- **Category**: direction
- **Planned at**: 6f06424 (2026-11-27)
- **State**: TODO (index authoritative)

## Why this matters
Browser capture is distinct from URL handoff. Keep disabled until paired consent, H264 negotiation and network reliability verified.

## Current state
Baseline `6f06424`: C006/C008/C009 are merged. `Screen::Cast` and `ui/cast.slint` already exist; the `webrtc` feature is empty and `src/cast/` is absent. Pi has GStreamer 1.26.2 with `webrtcbin`, `rtph264depay`, `avdec_h264`; Chromium is installed. `docs/protocol-v1.md` has the optional WebRTC cast extension, but no server S009 deployment/join test exists yet, and `src/cloud/` does not currently route `cast_request`/`cast_signal`. Implement the strict local receiver state machine, consent, SDP/ICE validation and GStreamer pipeline; mock signaling and mark the live S009/browser test blocked until the server is deployed. Never log SDP/ICE/tokens or claim joint integration.

New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

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
- `src/cast/mod.rs`
- `src/cast/webrtc.rs`
- `src/ui.rs`
- `ui/`
- `tests/webrtc.rs`
- `docs/webrtc.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c012-webrtc` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Implement signaling and consent

Coordinate S009; use v1 optional cast request/decision/signals; physical approve, strict cast id and generation, timeouts, reject unknown session; no arbitrary remote-control input. GStreamer webrtcbin, ICE/DTLS-SRTP, recvonly, prefer H264 720p30; no transcoding on Pi.

**Verify:** `cargo test --locked --no-default-features --test webrtc` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Validate sessions

Stop pipeline and clear session on disconnect/expiry; real desktop Chromium screen capture on LAN/tailnet, audio where supported, check 15 min and permission refusal. If ICE fails record blockers; TURN only separate approved deployment and network audit.

**Verify:** `test -s docs/webrtc.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Mock offers/ICE invalid, late messages, revoke, shutdown; real browser consent and decoded video. No Pixel native mirroring promise.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- No compatible H264/ICE path; no physical approval; required public TURN not approved.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
