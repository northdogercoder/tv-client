# Plan 016: Book and comic viewing

> Executor: C016. Work in an isolated worktree, follow steps, run each gate, and stop on mismatch. Reviewer owns the index. No push, merge, deployment or credentials.

## Status
- **Priority**: P3
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C004, C005
- **Planned at**: 1b28126, 2026-11-27
- **State**: TODO (index authoritative)

## Why this matters
Jellyfin can expose Books and Comics. The library currently filters `MediaTypes=Video`, so these libraries appear empty. A reader must handle untrusted book files without a browser/HTML execution surface or unbounded memory.

## Baseline
- Jellyfin `BaseItem` has id/name/overview/image tags; download/image APIs exist.
- Slint UI has fixed-column grids and a preview flow; no document renderer.
- No EPUB/PDF/comic parser or download storage contract.

## Scope
`Cargo.toml`, `Cargo.lock`, `src/jellyfin/`, `src/storage.rs`, `src/ui.rs`, `ui/`, `src/reader/` (new), `tests/`, `docs/`. No sibling repository.

## Steps
1. Browse Books/Comics with typed item metadata and cover artwork; bounded pages.
2. Download the selected book into a size-bounded state cache with an allowlist of formats (EPUB, PDF, CBZ; reject HTML/scripts and DRM). No arbitrary URL.
3. Render: EPUB/CBZ as images/text page-by-page; PDF via a vetted parser or rasterizer. Strict page/byte/dimension caps, no JavaScript/network, explicit errors for unsupported files.
4. UI: cover grid, open book, page navigation (keyboard/mouse), zoom/fit where safe, progress/position persistence; no reading while video/audio is active without explicit arbitration.
5. Tests: format allowlist, path traversal, decompression/size bombs, corrupt files, page bounds, cache eviction, no scripts/URLs; Pi evidence with one real EPUB/CBZ/PDF.

## STOP conditions
No safe parser within memory budget, DRM-only books, parser license incompatible, or need for a browser/HTML engine.
