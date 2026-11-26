# Plan 002: Prove Pi graphics, H.264, audio and networking before UI work

> Executor: C002. Work in an isolated worktree, follow steps in order, run each gate, and stop on mismatch. Read the full plan before acting. The reviewer (not executor) owns the index. No push, merge, deployment, or production credentials without separate operator approval.
>
> Drift check: planned-at is bound to `14c3186fae4065209eb65d7a76e9c98399f778c3` (post-C001 merge, 2026-11-25). Advisor verified the baseline and captured live Pi capability evidence below. Then run `git diff --stat 14c3186fae4065209eb65d7a76e9c98399f778c3..HEAD -- scripts/probe-pi.sh docs/hardware-report.md docs/build.md`. If paths changed, reconcile against live code first. Check dependency rows and cross-repo gates below. Do not replace placeholders with invented SHA.

## Status
- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: C001
- **Category**: direction
- **Planned at**: 14c3186fae4065209eb65d7a76e9c98399f778c3, 2026-11-25
- **State**: TODO (index authoritative)

## Baseline evidence (advisor preflight, 2026-11-25)

Target reached over Tailscale as `raspberry` (user `anders`). Capability capture only; playback is not proven.

| Check | Command | Result |
|---|---|---|
| Model | `tr -d '\0' < /sys/firmware/devicetree/base/model` | Raspberry Pi 3 Model B Rev 1.2 (**not 3B+**) |
| OS/kernel | `cat /etc/os-release`; `uname -a` | Debian 13 trixie; `6.18.39+rpt-rpi-v8`; aarch64 |
| Memory | `free -m` | 905 MB total, ~568 MB available |
| Thermal/throttle | `vcgencmd measure_temp`; `vcgencmd get_throttled` | 41.9 °C; `0x0` |
| HDMI/DRM | `cat /sys/class/drm/card0-HDMI-A-1/{status,enabled,modes}` | connected, enabled, 1920×1080 modes |
| H.264 decode | `v4l2-ctl -d /dev/video10 --list-formats-out`; `--list-formats` | H264 input, NV12 output, `bcm2835-codec-decode` |
| GStreamer | `gst-inspect-1.0 --version`; element checks | 1.26.2; `v4l2h264dec`, `waylandsink`, `kmssink`, `playbin3`, `alsasink` present |
| CEC | `cec-ctl --list-devices` | `/dev/cec0` via `vc4_hdmi`; libCEC `cec-client` absent |
| Audio | `aplay -l`; `wpctl status` | bcm2835 Headphones + vc4-hdmi; **default sink is analog**, HDMI sink id 35 |
| Compositor | `ps`; packages; `/run/user/1000/wayland-0` | labwc 0.20.2; Cage/Weston absent |
| Browser | `chromium --version` | 156.0.8078.108 |
| Input | `/dev/input/by-id` | USB wireless keyboard/mouse receiver |
| CMA | `grep Cma /proc/meminfo` | 262144 kB total, ~1.2 MB free at idle |
| Network | `ip -br addr`; eth0 speed | 10.0.0.2/24, 100 Mbps; Tailscale 100.65.4.42 |
| Build deps | `command -v cargo rustc`; `pkg-config gstreamer-1.0` | cargo/rustc absent; GStreamer dev headers absent; gcc/pkg-config present |
| Permissions | `id`; `sudo -n true` | video/render/audio/input groups; NOPASSWD sudo |

Playback, HDMI audio, 1080p decode and focus recovery are **not** proven; capability presence is not a C002 PASS. `gstreamer1.0-tools` was installed by the advisor on 2026-11-25; no other target changes.

## Why this matters
The verified target is a Pi 3 Model B Rev 1.2 with 1 GB and nominal H.264 1080p30 decode. Hardware acceleration, window switching and browser performance are not guaranteed by specifications.

## Current state
C001 is merged at `14c3186`; `bash scripts/check.sh` exits 0 on `main`. New files listed below are target paths; existing code must not be overwritten. Rust package is `tv-client`; use typed errors, structured redacted tracing, bounded queues, explicit cancellation, committed Cargo.lock and non-root services. For cross-boundary behavior follow `docs/protocol-v1.md` and fixture examples. Core local Jellyfin must not depend on cloud; no arbitrary remote shell, URL proxy, or credentials in logs.

## Commands you will need
| Purpose | Command | Expected |
|---|---|---|
| Baseline | `git status --short` | clean before execution |
| Standard check | `bash scripts/check.sh` | exit 0 (verified on bound baseline) |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Diff | `git diff --stat <BOUND_SHA>..HEAD` | only approved paths |

`bash scripts/check.sh` has been executed on the bound baseline; playback and probe commands have not. Real hardware checks need an operator report with command output, not simulated success.

## Scope
**In scope** (only these files/directories; trailing slash includes descendants):
- `scripts/probe-pi.sh`
- `docs/hardware-report.md`
- `docs/build.md`

**Out of scope**: sibling repository, protocol contract edits (coordinate separate reviewed change), plans index edits by executor, unrelated refactoring, production secrets, destructive infrastructure changes. If a required file is missing from scope, STOP and request a scope amendment. `Cargo.lock` changes only where in scope and justified.

## Git workflow
Branch `advisor/c002-hardware-spike` from reviewed baseline; commit logical steps using conventional commits; no automatic merge or push. Reviewer checks `git diff <BOUND_SHA>..HEAD` and untracked files, tests assertions, and re-runs gates. Review verdict APPROVE/REVISE/BLOCK, max two revisions.

## Steps

### Step 1: Capture real target capabilities

Record OS release, uname -m, apt versions, KMS, DRM, free -m, vcgencmd get_throttled, v4l2-ctl --list-devices, gst-inspect-1.0 for v4l2h264dec/playbin3/waylandsink and audio sinks. Probe over wired ethernet and Tailscale direct vs relay; script read-only, no tokens.

**Verify:** `bash scripts/probe-pi.sh` → exit 0; all named checks pass; inspect actual assertions/output.

### Step 2: Test playback inside actual console session

From physical unprivileged login use the installed labwc 0.20.2 Wayland session (Cage is not installed; install Cage only with explicit operator approval) with GStreamer test video/audio, then licensed local H264 AAC MP4 fixture at 720p and 1080p30; use explicit hardware decoder (`v4l2h264dec`) and `waylandsink` when present; check frame drops and HDMI audio. Select the HDMI sink explicitly (`wpctl set-default 35` or an explicit GStreamer audio sink) because the default PipeWire sink is analog. Record `CmaFree` before and during decode; the idle pool showed ~1.2 MB free of 256 MB. Use a disposable demo/temporary directory outside the repository for a Slint placeholder; test sequential Slint placeholder→video window→launcher focus; use a second window only while playback active. Log decoder actually selected, CPU, RSS and temp.

**Verify:** `test -s docs/hardware-report.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

### Step 3: Record pass/fail and fallback

Report 30 min each video mode, stable audio, no crash, no throttling, no sustained frame loss (>1% proposed). If 1080 fails test 720; if labwc cannot host or restore the video window, evaluate installing Cage (with approval) or a minimal Weston shell, or a single surface using GStreamer appsink only if the benchmark passes; do not switch architecture without ADR.

**Verify:** `grep -E "^Core gate: (PASS|BLOCKED)" docs/hardware-report.md` → exit 0; documented real measurements must meet the stated gate, file existence alone is not acceptance.

## Test plan
Hardware report includes actual command outputs/versions on the Pi 3B Rev 1.2 and 720p/1080p results, not inferred from mock tests. Core gate PASS requires 720p30 usable hardware path and window recovery under labwc (or an approved compositor change); otherwise BLOCKED.

## Done criteria
- [ ] Every step command run with actual exit status and meaningful assertions; hardware evidence where specified.
- [ ] `bash scripts/check.sh` exits 0; any enabled platform feature also built/tested on matching target.
- [ ] Scope clean (`git diff --name-only <BOUND_SHA>..HEAD` plus `git status --short`); no secrets, no undocumented changes.
- [ ] Reviewer independently reruns checks and records evidence, commit ID and verdict in index after merge (or records approved awaiting merge as IN PROGRESS).

## STOP conditions
- No real Pi/TV available; hardware decoder unavailable or unacceptable; `CmaFree` exhaustion prevents decoder buffer allocation; HDMI audio cannot be selected; compositor cannot restore focus; do not mark DONE on simulated results.
- Prerequisite missing, baseline drift, verification fails twice, or scope expansion required. Report observed evidence and a bounded proposed next step; never fabricate successful results.

## Maintenance notes
Keep APIs and feature flags consistent with the wire contract. Document changed dependencies, failure behavior and release implications. Reviewer checks meaningful negative tests and production vs mock separation; preserve this file after completion.
