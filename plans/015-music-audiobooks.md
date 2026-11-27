# Plan 015: Music and audiobook playback

> Executor: C015. Work in an isolated worktree, follow steps, run each gate, and stop on mismatch. Reviewer owns the index. No push, merge, deployment or credentials.

## Status
- **Priority**: P2
- **Effort**: L
- **Risk**: MED
- **Depends on**: C005, C006, C007
- **Planned at**: 1b28126, 2026-11-27
- **State**: TODO (index authoritative)

## Why this matters
The Jellyfin library grid exposes Music and Audiobooks. The current browse query and native player are video-oriented, so these views are empty or unplayable. A dedicated audio player avoids opening a video surface for music and gives a usable queue/metadata/control experience.

## Baseline
- Jellyfin `views` and parent-scoped `browse` exist; browse currently requests `Movie,Episode,Video` and `MediaTypes=Video`.
- `PlaybackController` owns one GStreamer player session for video; the YouTube path uses mpv locally.
- No audio queue, album/track artwork model, or audio-only OSD/control bar.
- Music videos are video items and can use the regular video player; do not build a separate music-video path.

## Scope
`Cargo.toml`, `Cargo.lock`, `src/jellyfin/`, `src/playback.rs`, `src/main.rs`, `src/ui.rs`, `src/player/`, `ui/`, `tests/`, `docs/`. No sibling repository.

## Steps
1. Typed audio browse: map library collection types (`music`, `audiobooks`) to `Audio`, `AudioBook`, `MusicAlbum`, `MusicArtist` and album/track metadata; bounded pages and artwork; no video-only filter for those libraries.
2. Audio player: exclusive audio pipeline/queue, play/pause/next/previous/seek/volume, track/album identity, duration; reuse the single-owner supervisor; no video surface. Resume/progress semantics documented.
3. UI: album/track selection and now-playing screen/control bar with keyboard and mouse; artwork classes; clear error for unsupported codecs.
4. Tests: parent-scoped audio query, queue transitions, seek bounds, end-of-track behavior, cancellation, no simultaneous video/audio, artwork caps, session expiry.
5. Pi evidence: a real album and audiobook chapter with audio, track changes, pause/resume and clean teardown; CPU/memory stable.

## STOP conditions
No usable audio decoder/sink, no exclusive ownership, unsupported DRM/format, hardware absence, or scope expansion.
