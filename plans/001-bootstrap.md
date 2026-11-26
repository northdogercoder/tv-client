# Plan 001: Establish a reproducible Rust client baseline

> Executor: C001. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `a445a0fba4cbc66432f1550226f901de76432f01` (2026-11-25). Advisor verified the baseline with `git rev-parse HEAD`; all C001 target paths are explicitly absent below. Then run `git diff --stat a445a0fba4cbc66432f1550226f901de76432f01..HEAD -- Cargo.toml Cargo.lock rust-toolchain.toml .gitignore .github/workflows/ci.yml src/main.rs src/lib.rs tests/bootstrap.rs scripts/check.sh docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction
- **Planned at**: a445a0fba4cbc66432f1550226f901de76432f01 (greenfield seed), 2026-11-25
- **State**: TODO (index authoritative)

## Baseline evidence (advisor preflight, 2026-11-25)

Bound to the sole seed commit. Worktree clean; 20 tracked files, all documentation/spec/example files.

| Check | Command | Result |
|---|---|---|
| Baseline SHA | `git rev-parse HEAD` | `a445a0fba4cbc66432f1550226f901de76432f01` |
| Worktree | `git status --short --branch` | `## main...origin/main` (clean) |
| Inventory | `git ls-files` | 20 files: `AGENTS.md`, `docs/` (2), `deploy/*.example` (3), `plans/` (14) |
| Rust stable (recommended pin) | `rustup run stable rustc --version` | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Cargo stable | `rustup run stable cargo --version` | `cargo 1.100.0 (3acc79c0c 2026-11-05)` |
| Clippy stable | `rustup run stable cargo clippy --version` | `clippy 0.1.100 (0ef1bc3860 2026-11-09)` |
| rustfmt stable | `rustup run stable cargo fmt --version` | `rustfmt 1.11.0-stable (0ef1bc3860 2026-11-09)` |
| Edition 2024 smoke | `rustup run stable cargo new --edition 2024` scratch crate outside the repo, then `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check` | pass; edition 2024 accepted by 1.100.0 |
| Host arch | `uname -m` | `x86_64` (not the ARM64 target) |
| Native libs | `pkg-config --modversion gstreamer-1.0 sqlite3 openssl` | `1.28.2`, `3.46.1`, `3.5.5` |
| Cross toolchain | `rustup target list --installed`; `command -v aarch64-linux-gnu-gcc` | x86_64 only; `aarch64-linux-gnu-gcc` present but no Rust aarch64 std |

Explicit absence at the bound SHA, covering every C001 scope target: `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.gitignore`, `.github/workflows/ci.yml`, `src/main.rs`, `src/lib.rs`, `tests/bootstrap.rs`, `scripts/check.sh`, `docs/build.md`.

Not run: any `cargo build|test|clippy|fmt` gate inside this repository, because no package exists yet. The scratch edition-2024 crate above is the only executed Rust check. `rust-toolchain.toml` must pin the tested version the executor uses: pin `1.100.0` as verified above. `$HOME/.cargo/bin` is not on the default non-login shell PATH, so run `source "$HOME/.cargo/env"` before gates. The also-installed 1.93.1 caps Slint at 1.18.1 (MSRV 1.92); current Slint 1.19.1 requires 1.95, so 1.100.0 avoids a later C004 blocker.

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
