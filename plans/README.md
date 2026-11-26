# Implementation plans — Client

Greenfield backlog, 2026-11-25. All rows begin TODO; no application code exists in this bundle. Execute ready plans only; reviewer owns index. See ../AGENTS.md and ../docs/protocol-v1.md. Bind real baseline SHA before dispatch. Hardware-dependent checks require operator evidence.

| Plan | Title | Priority | Effort | Risk | Depends on | Status |
|---|---|---|---|---|---|---|
| [C001](001-bootstrap.md) | Establish a reproducible Rust client baseline | P1 | M | LOW | none | DONE |
| [C002](002-hardware-spike.md) | Prove Pi graphics, H.264, audio and networking before UI work | P1 | L | HIGH | C001 | DONE |
| [C003](003-core-state.md) | Implement typed application state, configuration and bounded persistence | P1 | M | MED | C001 | DONE |
| [C004](004-kiosk-ui.md) | Build a native remote-first Slint launcher | P1 | L | HIGH | C002, C003 | TODO |
| [C005](005-jellyfin-library.md) | Authenticate and browse the existing Jellyfin server | P1 | L | MED | C003 | DONE |
| [C006](006-native-player.md) | Implement exclusive native GStreamer playback | P1 | L | HIGH | C002, C003 | TODO |
| [C007](007-jellyfin-playback.md) | Negotiate Jellyfin playback and report watch progress | P1 | L | HIGH | C004, C005, C006 | TODO |
| [C008](008-input.md) | Unify keyboard, HDMI-CEC and physical input provenance | P1 | M | HIGH | C004, C006 | TODO |
| [C009](009-cloud-device.md) | Implement enrollment, pairing and remote command client | P1 | L | MED | C003, C004, C008 | TODO |
| [C010](010-youtube.md) | Implement YouTube discovery and on-demand player bridge | P1 | L | HIGH | C006, C008, C009 | TODO |
| [C011](011-airplay.md) | Add opt-in LAN AirPlay mirroring | P2 | M | HIGH | C006, C008, C004 | TODO |
| [C012](012-webrtc.md) | Add optional browser screen sharing receiver | P3 | L | HIGH | C006, C008, C009 | TODO |
| [C013](013-release.md) | Package, deploy and recover the Pi kiosk | P1 | L | HIGH | C007, C008, C009, C010 | TODO |

## Dependency notes

Independent plans can run in parallel only when their write scopes do not overlap; otherwise serialize or split and have reviewer integrate explicitly. Cargo.toml, Cargo.lock, src/lib.rs, UI and routes are conflict hotspots. All dependencies must be merged before dispatch.

Cross-repository gates: C009 integration requires S002–S004; C010 real playback requires S006; C013 packaging/staging may proceed with C010 BLOCKED only after operator explicitly authorizes a reduced-scope build and advisor amends dependency; requested-scope release requires C010 PASS and S007; S008 final acceptance requires C013 evidence (C013 packaging/staging happens before S008 signoff, no cyclic dependency); C012/S009 may implement independently after local dependencies, but final DONE on both requires joint optional negotiated tests; stage code approved-awaiting-integration before final DONE. C011 AirPlay independent of OVH.

Status: TODO / IN PROGRESS / DONE / BLOCKED (reason) / REJECTED (rationale). Mark DONE only after review and merge; pending hardware is BLOCKED, not DONE. Record commit, commands, hardware evidence and reviewer verdict in a notes column or below.

## Execution notes
- **C001 DONE 2026-11-25**: commit `14c3186fae4065209eb65d7a76e9c98399f778c3` merged to `main`; `bash scripts/check.sh` exit 0; bootstrap tests 3/3. C002 and C003 are the next dependency-ready plans.
- **C002 DONE 2026-11-25**: merged in `66d5657` from `advisor/c002-hardware-spike` @ `97f12f9`; reviewer APPROVE; `bash scripts/check.sh` exit 0; core gate `PASS (basic)` with 17/16 QoS drop warnings (~2.8%/2.7%); 30-min soak and Slint placeholder NOT RUN per operator waiver.
- **C003 DONE 2026-11-25**: merged in `0bf089c` from `advisor/c003-core-state` @ `943ebe9`; reviewer APPROVE after one revision; `bash scripts/check.sh` exit 0 (3 bootstrap + 15 reducer + 15 storage tests); TOML error redaction, artwork reconciliation and MSRV 1.89.
- **C005 DONE 2026-11-25**: merged in `c157130` from `advisor/c005-jellyfin-library` @ `fb1181b`; reviewer APPROVE after one base-path fix; `bash scripts/check.sh` exit 0 (53 tests, 1 ignored); live Jellyfin 12.4.0 auth/views/logout passes; path-prefixed URL regression covered.
- **Repository public**: switched to public 2026-11-25 after a history scan found no credentials; GitHub-hosted arm64 runners (`ubuntu-24.04-arm`) are available for C013.

## Considered and deferred
- Full Chromecast receiver: proprietary compatibility not guaranteed; no clone in core.
- Always-on browser, cloud video transcoding, arbitrary URL playback, public remote access, Kubernetes: excluded to reduce footprint and trust surface.
- Native Android MediaProjection sender and TURN: separately scoped extensions, not core promises.
