# Plan 005: Authenticate and browse the existing Jellyfin server

> Executor: C005. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is UNBOUND because this is a new repo. Advisor records actual baseline SHA with `git rev-parse HEAD` and live file excerpts (or explicitly absent) before dispatch. Then run `git diff --stat <BOUND_SHA>..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/jellyfin/ tests/jellyfin_library.rs docs/jellyfin.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: C003
- **Category**: direction
- **Planned at**: UNBOUND (greenfield), 2026-11-25
- **State**: TODO (index authoritative)

## Why this matters
Jellyfin is authoritative for identity and library. Pi contacts it directly, independent of cloud.

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
- `src/jellyfin/`
- `tests/jellyfin_library.rs`
- `docs/jellyfin.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c005-jellyfin-library` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Implement typed API and Quick Connect

Use Reqwest with timeouts, TLS validation, configured origin incl base path, no cross-origin redirects. Verify installed Jellyfin OpenAPI/version. Implement QuickConnect Initiate/Connect and Users/AuthenticateWithQuickConnect using server documented methods, cancellation and expiry; stable device id and MediaBrowser authorization header; manual username/password fallback only over HTTPS or approved encrypted tailnet HTTP, never persist password. Logout revoke session and erase local cache/secret.

**Verify:** `cargo test --locked --no-default-features --test jellyfin_library` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Implement paginated library and cache

Methods get views, items, resume, latest, seasons, episodes, music, search, images, user data; page size 50 and request cancellation, cache by server/user/item/image tag with LRU 128 MiB, safe decoded image size, no private addresses other than configured Jellyfin. Use item IDs not stream URLs as browse model. Handle 401, 403, timeout, 429.

**Verify:** `cargo test --locked --no-default-features --test jellyfin_library` → exit 0; all named checks pass; inspect actual assertions/output.

## Test plan
Mock HTTP fixtures assert auth headers, Quick Connect lifecycle, pagination, logout/cache isolation, cancellation, no redirects or credential logs. Test on separate real limited Jellyfin user.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- Installed server endpoints differ or Quick Connect disabled without approved fallback; server requires admin API key; TLS failure.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
