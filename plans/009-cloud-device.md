# Plan 009: Implement enrollment, pairing and remote command client

> Executor: C009. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `fa531335cce0857d36bc5e6c6ef4d5c80893310f` (post-C008 merge, 2026-11-26). Advisor verified the baseline and the absent target files below. Then run `git diff --stat fa531335cce0857d36bc5e6c6ef4d5c80893310f..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/main.rs src/cloud/ src/ui.rs ui/ tests/cloud_device.rs docs/cloud.md docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: C003, C004, C008
- **Category**: direction
- **Planned at**: fa531335cce0857d36bc5e6c6ef4d5c80893310f, 2026-11-26
- **State**: TODO (index authoritative)

## Baseline evidence (advisor, 2026-11-26)

- Baseline `fa53133`; C003, C004 and C008 are merged; host gate exit 0 (140 tests, 2 live ignored) and combined `native-ui,gstreamer-player` clippy passes.
- Server S002/S003/S004 are DONE (`b9f2ee8`, `a6ca230`, `ef432e9`), server `main` is clean, and `docs/protocol-v1.md` / `docs/protocol-fixtures.json` hashes are identical across the two repos (`8695cf14…`, `3a7d0e53…`).
- Staging S006 Phase A is at `ovh-1.minotaur-gacrux.ts.net`; no one-use enrollment token has been minted for this run, so live enrollment/pairing integration is pending an operator token. Implement against the protocol and mocks; do not fabricate a live claim.
- Existing interfaces: `ClientConfig`/`Storage` (0600 credentials), `UiModel`/input provenance, playback/player supervisor; rustls-backed reqwest.
- Absent: `src/cloud/`, `tests/cloud_device.rs`, `docs/cloud.md`. Add `tokio-tungstenite` (rustls) for the device WebSocket and document it in `docs/build.md` (scope amendment).

## Why this matters
OVH is an optional control plane. Contract is docs/protocol-v1.md; never transmit Jellyfin secrets or media URLs.

## Current state
C003, C004 and C008 are merged; the config/storage/UI/input/playback interfaces above are available and the server contract is merged. C009 target files are absent; add the cloud module and WebSocket dependency without breaking the offline core. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| Protocol parity | `sha256sum docs/protocol-v1.md ../tv-server/docs/protocol-v1.md` | identical hashes |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` and protocol parity are verified on the bound baseline; live enrollment/pairing is not, because no operator-minted token exists yet. Mock/contract tests are the acceptance path until a token is available.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/main.rs`
- `src/cloud/`
- `src/ui.rs`
- `ui/`
- `tests/cloud_device.rs`
- `docs/cloud.md`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c009-cloud-device` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Implement device auth and socket

Server S002–S004 are merged and the protocol files match. CLI `tv-client enroll --config PATH --state-dir DIR` prompts the one-use token via stdin; store the device token 0600, use HTTPS bearer. Implement v1 device config, heartbeat, hello/state over `tokio-tungstenite`/rustls, reconnect with jitter 1–60 s, bounded queue and no replay, device revocation state; offline core stays usable. Use `Authorization: Bearer` for `/v1/ws/device`; never put the token in argv or logs. A live enrollment token is not available yet; cover the protocol with mocks and record integration as pending.

**Verify:** `cargo test --locked --no-default-features --test cloud_device` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Implement pairing and remote actions

Pair init UI code, comparison code, local-only confirm; add `pair_request` WS event payload {request_id,comparison_code,expires_at} and `pair_status` as defined in docs/protocol-v1.md. Server S003/S004 are merged; implement against the tested contract. Validate typed action, dedupe, send ack after real application; remote never invokes physical approval. For `play_youtube`, route to C010's local playback path once C010 is merged and ack applied only after playback actually starts; until then reject with a typed reason. Never ack before playback is applied.

**Verify:** `cargo test --locked --no-default-features --test cloud_device` → exit 0; all named checks pass; inspect actual assertions/output.

## Test plan
Fixture parity, mock WS reconnect/heartbeat, duplicate command, expired pairing, invalid mode, no remote approval, offline no fatal exit; `play_youtube` is acked only after C010 playback actually starts. S002–S004 are merged; live integration needs an operator-minted one-use token, otherwise record it as pending.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- Protocol hash mismatch or server contract drift; token appears in diagnostics/argv/logs. A missing live enrollment token blocks the integration claim, not the mock implementation.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
