# Plan 017: Video control bar overlay

> Executor: C017. Work in an isolated worktree, follow steps, run each gate, and stop on mismatch. Reviewer owns the index. No push, merge, deployment or credentials.

## Status
- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: C004, C006, C007
- **Planned at**: 1b28126, 2026-11-27
- **State**: TODO (index authoritative)

## Why this matters
Native video owns a fullscreen waylandsink surface. The launcher controls are behind it, so the operator cannot see play/pause, seek, elapsed/remaining time or stop without knowing keyboard shortcuts. The 0.1.2 software `textoverlay` made video choppy, so the control bar must not insert a per-frame video conversion.

## Baseline
- `tv-native-player` owns waylandsink, evdev keys, JSON-lines control and a player supervisor.
- The launcher has a Player screen and state, but it is occluded by the video surface.
- `textoverlay` is opt-in and not acceptable for always-on controls on the Pi 3B.

## Scope
`src/player/`, `src/bin/tv-native-player.rs`, `src/playback.rs`, `src/main.rs`, `src/ui.rs`, `ui/` (new overlay component), `Cargo.toml`, `Cargo.lock`, `tests/`, `docs/`. No sibling repository.

## Steps
1. Separate overlay surface: a lightweight Slint/other native overlay window above waylandsink, or an equivalent compositor-friendly control surface. No per-frame video filter; no opaque fullscreen cover. Verify transparency/z-order on Cage.
2. Controls: play/pause, seek bar with played/remaining time, stop/back, volume (if available); mouse move shows the bar, inactivity hides it; click/touch and keyboard (arrows/Enter/Esc/Space/Home/End) operate it. Bound hit areas and focus; no remote shell.
3. State: subscribe to existing player position/duration/paused/volume events; do not poll the child unboundedly; keep overlay and video state consistent on stop/end/error.
4. Arbitration: overlay exists only during native video; audio/music and books have their own controls; no duplicate input ownership.
5. Tests: pure overlay state/navigation/time formatting; show/hide and keyboard/mouse mapping; stop/end/error cleanup; no URL/token. Pi: 720p30 smooth with overlay visible, seek accuracy, mouse show/hide, Esc stop, no orphan child.

## STOP conditions
No compositor-supported overlay/transparency path without per-frame conversion, unacceptable video stutter, input focus cannot be made deterministic, or scope expansion.
