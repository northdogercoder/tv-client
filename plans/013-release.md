# Plan 013: Package, deploy and recover the Pi kiosk

> Executor: C013. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `f344ff8150684fdc7c7f417581c23c9927fee264` (post-C010 merge, 2026-11-26). Advisor verified the baseline and the absent target files below. Then run `git diff --stat f344ff8150684fdc7c7f417581c23c9927fee264..HEAD -- scripts/ deploy/ .github/workflows/ Cargo.toml Cargo.lock docs/release.md docs/build.md tests/release.rs`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C007, C008, C009, C010
- **Category**: direction
- **Planned at**: f344ff8150684fdc7c7f417581c23c9927fee264, 2026-11-26
- **State**: TODO (index authoritative)

## Baseline evidence (advisor, 2026-11-26)

- Baseline `f344ff8`; C001–C010 are done; host gate exit 0 (227 tests, 2 live ignored) and combined `native-ui,gstreamer-player` clippy passes.
- Accepted hardware gates: C010 YouTube **720p30 under Cage/GLES** (2/240 drops), 1080p unsupported on Pi 3B, labwc FAIL, BBB 480p30 smooth. C002 short-run only; 30-min soak waived. C004 UI still needs Cage revalidation.
- Pi 3B Rev 1.2, Debian 13 trixie, labwc/Cage, 1080p; server S006 Phase A deployed; no enrollment token. C011/C012 remain optional TODO.
- Repository is public; GitHub-hosted `ubuntu-24.04-arm` is available. `deploy/client.toml.example` is stale (missing YouTube playback keys); no install/rollback scripts, release workflow, docs/release or tests/release exist.
- Baseline `bash scripts/check.sh` verifies code only; no ARM64 artifact or hardware acceptance exists yet.

## Why this matters
Provide reproducible ARM64 release, boot recovery, atomic updates, operator-controlled rollback. Optional C011/C012 only included if independently accepted.

## Current state
C001–C010 are merged; the code, tests and accepted hardware gates above exist. C013 targets are absent; update the stale deploy example and add packaging/scripts/docs without changing application behavior. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| ARM64 runner | `runs-on: ubuntu-24.04-arm` | public repo; pin action SHAs |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` is verified on the bound baseline; ARM64 packaging and hardware acceptance are not. Real hardware/rollback checks need the operator and must not be simulated. Do not deploy or change services from the executor worktree.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `scripts/`
- `deploy/`
- `.github/workflows/`
- `Cargo.toml`
- `Cargo.lock`
- `docs/release.md`
- `tests/release.rs`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c013-release` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Produce release artifact

Pin Rust, distro ABI and features; build on ARM64 Debian/Trixie runner or compatible Pi OS sysroot, never assume macOS cargo build is Linux. Package tv-client and tv-native-player at tar root in versioned tv-client-linux-arm64.tar.gz, SHA256SUMS plus signature or attestation, SBOM and license notices. `file` verifies AArch64, ldd target no missing deps. CI no production keys on PR.

**Verify:** `bash scripts/check.sh` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Implement deployment scripts

Copy vetted templates from deploy/ and update `deploy/client.toml.example` with the current keys (YouTube playback path/kill switch/timeout/hwdec, cloud/CEC). Install version under /opt/tv-client/releases/<version>, root-owned, atomic current symlink, unprivileged tvkiosk user, separate admin SSH, state/config persistent. Console PAM session starts **Cage with WLR_RENDERER=gles2** (the accepted YouTube renderer), systemd/getty restarts on exit; watchdog crash backoff; local recovery on tty2. Validate script syntax and idempotence, never blindly change firewall/OS. Do not install/deploy from the executor worktree.

**Verify:** `bash -n scripts/install-release.sh scripts/rollback.sh` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 3: Run full acceptance

Run docs/release.md matrix including core local, OVH pairing/YouTube if enabled, reboot, power loss simulation with safe procedure, offline server, network relay, 2h soak and rollback; capture sanitized evidence. Operator-owned; the executor produces the matrix/templates and marks unrun items NOT RUN. YouTube acceptance is 720p30 under Cage/GLES; 1080p is explicitly unsupported.

**Verify:** `test -s docs/release.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Checksum/attestation verified, no root app, boot 5 times, no stale process, rollback preserves state, core gate PASS; feature gates separately PASS or explicitly excluded.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- No hardware acceptance; no verified rollback or signature; kiosk account has sudo or SSH keys; data migration not backward compatible.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
