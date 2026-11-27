# Plan 014: On-device settings, writable config and Jellyfin Quick Connect

> Executor: C014. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. Reviewer owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `e2e6680` (2026-11-27). Then run `git diff --stat e2e6680..HEAD -- Cargo.toml Cargo.lock src/config.rs src/settings.rs src/main.rs src/ui.rs src/jellyfin/ ui/settings.slint ui/app.slint scripts/install-release.sh deploy/kiosk-profile.sh.example deploy/client.toml.example tests/settings.rs docs/settings.md docs/config.md docs/release.md`.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C004, C005, C009, C010
- **Planned at**: e2e6680, 2026-11-27
- **State**: TODO (index authoritative)

## Why this matters
The installed 0.1.0 kiosk has a read-only Settings screen. `/etc/tv-client/client.toml` is root-owned, so `tvkiosk` cannot edit connections/features. Jellyfin sign-in requires typing credentials or a TV-side Quick Connect. The TV also clips the bottom UI strip (overscan).

## Baseline evidence (advisor, 2026-11-27)
- 0.1.0 installed on the Pi; `tvkiosk` has no sudo/SSH; `/etc/tv-client/client.toml` is root:root 0644; `/var/lib/tv-client` is tvkiosk 0700.
- C005 Jellyfin client has Quick Connect Initiate/Poll/Authenticate and stores tokens via `Storage` 0600. Server Quick Connect is enabled.
- C004 Settings rows are informational only; C009 pairing and C010 YouTube config exist.
- Operator requested editable connections (Jellyfin/server URLs), feature flags (cloud/YouTube, yt-dlp path/timeout/hwdec), display safe area, pairing and Jellyfin sign-in. Writable state config approved.

## Scope
**In scope**:
- `Cargo.toml`, `Cargo.lock`
- `src/config.rs`, `src/settings.rs`, `src/main.rs`, `src/ui.rs`
- `src/jellyfin/`
- `ui/settings.slint`, `ui/app.slint`
- `scripts/install-release.sh`, `deploy/kiosk-profile.sh.example`, `deploy/client.toml.example`
- `tests/settings.rs`
- `docs/settings.md`, `docs/config.md`, `docs/release.md`

**Out of scope**: sibling repository, protocol changes, root helper/daemon, arbitrary shell, secrets in logs, deployment.

## Steps

### Step 1: Writable config and precedence
Add `ui_safe_area_percent` (0..=10, default 0; opt-in overscan inset) and a state config at `<state-dir>/client.toml`. Precedence: explicit `--config`, else state config if present, else `/etc/tv-client/client.toml`, else defaults. Validate before write; write atomically 0600 with fsync+rename in the 0700 state dir; never log values. The installer seeds the state config from the root example only when absent, owned tvkiosk 0600; the kiosk profile passes the state config. Preserve the root config as operator fallback.

**Verify:** `cargo test --locked --no-default-features --test settings` → exit 0.

### Step 2: Editable Settings UI + overscan
Replace read-only rows with a keyboard-driven editor for Jellyfin URL, server URL, cloud/YouTube toggles, managed yt-dlp path, timeout, hwdec, UI safe area, Pair a remote and Jellyfin sign-in. Use Slint TextInput/checkbox rows; arrows/Enter/Esc; validation errors shown without secrets; Save writes the state config and shows “Restart to apply”; Restart exits cleanly so the kiosk supervisor restarts. Apply `ui_safe_area_percent` as padding in `ui/app.slint` so the bottom strip is not clipped. Keep Slint loop free of I/O; perform writes on a worker.

**Verify:** `cargo test --locked --no-default-features --test settings`; `cargo check --features native-ui` in Docker/Pi.

### Step 3: Jellyfin Quick Connect from the TV
Use C005 Quick Connect: initiate, show six/eight-character code, poll with cancellation, authenticate, store access token 0600, show signed-in user; logout revokes/clears. No password typing required; manual fallback optional and never persisted/logged. Pairing remains local-only.

**Verify:** mock Quick Connect lifecycle, expiry/cancel, no token in debug/logs; Pi short login.

## Test plan
Config precedence/0600/atomic/invalid; safe area clamp; settings edit/save/restart; Quick Connect initiate/poll/expire/cancel/logout; no secrets; UI model navigation; existing suites.

## Done criteria
- `bash scripts/check.sh` exit 0; native-ui/gstreamer clippy; settings tests.
- 0.1.1 artifact rebuilt/installed; TV Settings edits persist across restart; bottom strip visible; Quick Connect works or is honestly blocked.
- Reviewer APPROVE; no secrets.

## STOP conditions
- Root-only write required; token leaks; Quick Connect disabled/unavailable; safe-area fix cannot be verified on TV; scope expansion to a privileged daemon.
