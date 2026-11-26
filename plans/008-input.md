# Plan 008: Unify keyboard, HDMI-CEC and physical input provenance

> Executor: C008. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `61f267587c525466928b49f44c0401ffa9757e9c` (post-C007 merge, 2026-11-26). Advisor verified the baseline and the absent target files below. Then run `git diff --stat 61f267587c525466928b49f44c0401ffa9757e9c..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/input/ src/main.rs src/ui.rs tests/input.rs docs/input.md docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: C004, C006
- **Category**: direction
- **Planned at**: 61f267587c525466928b49f44c0401ffa9757e9c, 2026-11-26
- **State**: TODO (index authoritative)

## Baseline evidence (advisor, 2026-11-26)

- Baseline `61f2675`; C004, C006 and C007 are merged; host gate exit 0 (22 playback + 1 ignored, 18 UI, prior suites) and the combined `native-ui,gstreamer-player` clippy passes.
- Existing interfaces: fullscreen Slint UI, `UiModel`/`UiCommand`, C006 player supervisor, C007 playback controller; local-vs-remote input is already a documented trust boundary.
- Pi: `/dev/cec0` via `vc4_hdmi`, `cec-ctl` 1.30.1 present; libCEC/`cec-client` absent; USB wireless keyboard/mouse; labwc 0.20.2; TV on HDMI 1080p.
- CEC physical remote behavior/settings are NOT yet verified. The kernel CEC path is the default; do not install libCEC unless the kernel path is proven unusable.
- Absent: `src/input/`, `tests/input.rs`, `docs/input.md`.

## Why this matters
TV remote should navigate and recover playback without a keyboard; CEC varies by TV. Local physical approval must not be reachable through remote command channel.

## Current state
C004, C006 and C007 are merged; the Slint UI, player supervisor and playback controller interfaces above are available. C008 target files are absent; add the input module and CEC bridge without breaking the headless/native-ui gates. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| CEC probe | `ssh raspberry 'cec-ctl --list-devices; ls -l /dev/cec0'` | `vc4_hdmi` / `/dev/cec0` |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` is verified on the bound baseline; CEC and physical-remote checks have not. The kernel CEC path is present; real remote evidence needs the operator at the TV and must not be simulated.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `Cargo.toml`
- `Cargo.lock`
- `src/lib.rs`
- `src/input/`
- `src/main.rs`
- `src/ui.rs`
- `tests/input.rs`
- `docs/input.md`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c008-input` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Implement input abstraction

InputEvent with origin LocalKeyboard/LocalCec/Remote; key map arrows/select/back/home/playpause, volume, repeat debounce and long press optional. Keep physical-only approvals in separate reducer path; do not simulate keyboard from remote.

**Verify:** `cargo test --locked --no-default-features --test input` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Integrate CEC and focus

`cec-client`/libCEC is absent; use the kernel CEC path (`/dev/cec0` via `vc4_hdmi`) with `cec-ctl` or a fixed-argument supervised helper, parse bounded events, and do not install libCEC unless the kernel path is proven unusable. No arbitrary CLI input. Document TV HDMI-CEC settings and the USB/Bluetooth keyboard fallback. Route global back while the player child owns focus via local CEC plus the parent's explicit stop path; never auto power cycle the TV. Physical-remote evidence is short and operator-driven; record NOT VERIFIED if no remote press is available.

**Verify:** `cargo test --locked --no-default-features --test input` → exit 0; all named checks pass; inspect actual assertions/output.

## Test plan
Repeat, disconnect, unknown key, local/remote approval provenance, no CEC gracefully; short real TV back/home/volume test with active native player, operator-driven, no power cycle.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- No usable back path during playback; permissions require root; CEC unavailable and no physical keyboard fallback.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
