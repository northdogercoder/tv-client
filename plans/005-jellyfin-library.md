# Plan 005: Authenticate and browse the existing Jellyfin server

> Executor: C005. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `56ac3a66d5798e8970db4f98670b846e8ccefdce` (post-C002/C003 merge, 2026-11-25). Advisor verified the baseline and the absent target files below. Then run `git diff --stat 56ac3a66d5798e8970db4f98670b846e8ccefdce..HEAD -- Cargo.toml Cargo.lock src/lib.rs src/jellyfin/ tests/jellyfin_library.rs docs/jellyfin.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: C003
- **Category**: direction
- **Planned at**: 56ac3a66d5798e8970db4f98670b846e8ccefdce, 2026-11-25
- **State**: TODO (index authoritative)
- **Test server**: Jellyfin 12.4.0 at `http://jellyfin:8096` (verified 2026-11-25); limited non-admin user `test`; credentials are in `~/.config/tv-client/jellyfin-test.env` (0600, outside the repo) and must never be committed or logged. Send the `Authorization: MediaBrowser ...` header with `Token="..."`; this server returned HTTP 400 for `X-Emby-Authorization` and HTTP 401 for `X-Emby-Token`.

## Why this matters
Jellyfin is authoritative for identity and library. Pi contacts it directly, independent of cloud.

## Baseline evidence (advisor, 2026-11-25)

- Baseline `56ac3a6`; C003 merged at `0bf089c`; `bash scripts/check.sh` exit 0 (33 tests).
- Existing interfaces: `ClientConfig`/`ConfigError`; `Storage` (`store_artwork`, `artwork_path`, `write_credential`, `read_credential`, `save_last_view`, `record_progress`); `AppState`/`Action`/`Effect`; all re-exported from `src/lib.rs`.
- Existing direct dependencies: clap, serde, toml, tokio, rusqlite (bundled), thiserror, tracing, uuid; `tempfile` dev-only. MSRV 1.89.
- Absent: `src/jellyfin/`, `tests/jellyfin_library.rs`, `docs/jellyfin.md`.
- Server: Jellyfin 12.4.0, Quick Connect enabled, views include Movies, TV Shows, Music, Books, Audiobooks, Music Videos.
- Host: x86_64, Rust 1.100.0 pinned. `reqwest 0.13.5`'s `default-tls` feature maps to rustls, so no OpenSSL headers are required.

## Current state
C003 is merged at `0bf089c`; `bash scripts/check.sh` exits 0 (33 tests) and the config/storage/reducer interfaces above are available. C005 target files are absent; add `pub mod jellyfin;` to `src/lib.rs` and new dependencies without breaking existing behavior. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` has been executed on the bound baseline; C005 network and live-server checks have not. Live checks need the test environment below and must not be simulated.

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

Use Reqwest with timeouts, TLS validation, configured origin incl base path, no cross-origin redirects. Verify installed Jellyfin OpenAPI/version (server reports 12.4.0; Quick Connect is enabled). Implement QuickConnect Initiate/Connect and Users/AuthenticateWithQuickConnect using server documented methods, cancellation and expiry; stable device id and MediaBrowser authorization header; manual username/password fallback only over HTTPS or approved encrypted tailnet HTTP, never persist password. Logout revoke session and erase local cache/secret.

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
