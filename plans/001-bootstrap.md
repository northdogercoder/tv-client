# Plan 001: Establish a reproducible Rust client baseline

> Executor: C001. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is UNBOUND because this is a new repo. Advisor records actual baseline SHA with `git rev-parse HEAD` and live file excerpts (or explicitly absent) before dispatch. Then run `git diff --stat <BOUND_SHA>..HEAD -- Cargo.toml Cargo.lock rust-toolchain.toml .gitignore .github/workflows/ci.yml src/main.rs src/lib.rs tests/bootstrap.rs scripts/check.sh docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction
- **Planned at**: UNBOUND (greenfield), 2026-11-25
- **State**: TODO (index authoritative)

## Why this matters
Start with a single Rust package `tv-client` (binary and library), edition 2024 if toolchain supports it. Keep platform dependencies behind features so headless CI works.

## Current state
No repository source was provided. Expected state: only seed plans, AGENTS.md, docs and deploy examples. Before dispatch, advisor confirms actual tree and adds excerpts if needed. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

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
- `rust-toolchain.toml`
- `.gitignore`
- `.github/workflows/ci.yml`
- `src/main.rs`
- `src/lib.rs`
- `tests/bootstrap.rs`
- `scripts/check.sh`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c001-bootstrap` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Create the package and feature flags

Use `cargo init --name tv-client --bin .` only if no Cargo.toml exists; add lib.rs, features `native-ui`, `gstreamer-player`, `cec`, `airplay`, `webrtc` (empty initially, no feature claims). CLI `--version` and `--help` via clap; commit Cargo.lock and pin real tested Rust stable version in rust-toolchain.toml.

**Verify:** `cargo test --locked --no-default-features --test bootstrap` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Create repeatable checks

Implement scripts/check.sh with fmt --check, clippy --locked --all-targets --no-default-features -- -D warnings, test --locked --no-default-features; CI Linux, minimal token permissions, pinned action SHAs, no secrets. Document architecture targets and dependency update procedure.

**Verify:** `bash scripts/check.sh` → exit 0; all named checks pass; inspect actual assertions/output.

## Test plan
bootstrap: version/help exit 0; headless library compiles; CI runs same check script.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- Existing source/package names conflict; stable version unavailable; cannot run cargo.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
