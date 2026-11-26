# Hardware report: Raspberry Pi 3B graphics, H.264, HDMI audio and Wayland

- **Date of capture:** 2026-11-26 UTC (01:52–02:02Z; evening of 2026-11-25 local).
- **Target:** Raspberry Pi 3 Model B Rev 1.2, alias `raspberry` (user `anders`, NOPASSWD sudo).
- **Worktree:** `/home/anders/dev/tv-client-c002`, branch `advisor/c002-hardware-spike`.
- **Method:** read-only capability probe from the development host over SSH, then
  short GStreamer playbacks inside the already-running labwc session over SSH
  (`XDG_RUNTIME_DIR=/run/user/1000`, `WAYLAND_DISPLAY=wayland-0`). Playback ran in
  the existing session, not from a physical console login. The probe and playback
  themselves installed nothing, started or restarted no compositor, and changed no
  VT.
- **Target preparation:** the target received operator-approved installs after
  the baseline: `libgstreamer1.0-dev` (20:40), rustup 1.100.0 (20:43),
  `libgstreamer-plugins-base1.0-dev` (20:45) and `cage` 0.3.1 (20:47:58);
  `gstreamer1.0-tools` was installed by the advisor. Cage was installed with
  operator approval and was **NOT started** during this work.
- **Raw evidence:** probe capture `362` lines; per-run logs on the Pi in
  `/tmp/c002-*.out` and `/tmp/c002-*.err` (disposable); window screenshot
  `/tmp/c002-window.png` (also copied to the development host).

## 1. Capability probe

Command (from the repository root of this worktree):

```sh
bash scripts/probe-pi.sh
```

`PI_HOST` defaults to `raspberry`; the script uses `ssh -o BatchMode=yes`, runs a
quoted heredoc of read-only commands, installs nothing, changes no configuration
and uses no tokens. `GST_REGISTRY_UPDATE=no` is exported in the remote shell, so
the `gst-inspect-1.0` calls do not refresh
`~/.cache/gstreamer-1.0/registry.aarch64.bin`. It exited `0`.

| Check | Observed value |
|---|---|
| Model | `Raspberry Pi 3 Model B Rev 1.2` (not 3B+) |
| OS / kernel | Debian GNU/Linux 13 (trixie), `6.18.39+rpt-rpi-v8` |
| Architecture | `aarch64`, dpkg `arm64` |
| Memory (`free -m`) | 905 MB total, 651 MB available, 904 MB swap |
| SD/CPU temp / throttle | `temp=41.9'C`, `throttled=0x0` |
| HDMI connector | `card0-HDMI-A-1`: `connected`, `enabled`; modes include 1920x1080, 1280x720 |
| V4L2 decoder | `bcm2835-codec-decode` → `/dev/video10` (also 11, 12, 18, 31) |
| Decode input formats | H264 (also MPG4, MJPG, H263) |
| Decode output formats | YU12, YV12, NV12, NV21, NC12, RGBP, AB24, BGR4 |
| GStreamer | `gst-inspect-1.0 version 1.26.2`, GStreamer 1.26.2 |
| Elements present | `v4l2h264dec`, `waylandsink`, `kmssink`, `playbin3`, `alsasink`, `pulsesink`, `queue` |
| GStreamer dev headers | `pkg-config --modversion gstreamer-1.0` = 1.26.2; `gstreamer-video-1.0` = 1.26.2; `/usr/include/gstreamer-1.0/gst/gst.h` present |
| ALSA devices | card 0 `bcm2835 Headphones`; card 1 `vc4-hdmi` (`MAI PCM i2s-hifi-0`) |
| PipeWire sinks | 35 `Built-in Audio Digital Stereo (HDMI)`, 56 `Built-in Audio Stereo`; PipeWire 1.4.2, `pipewire-pulse` running |
| CEC | `cec-ctl --list-devices` → `vc4_hdmi`: `/dev/cec0` |
| Compositor / session | labwc 0.20.2 on tty1; no cage/weston process; `WLR_RENDERER=pixman` in labwc's environment |
| Installed shells | cage 0.3.1 installed (not running), labwc 0.20.2 |
| Chromium | `Chromium 156.0.8078.108` |
| Network | eth0 `10.0.0.2/24`, link `100` Mbit/s full duplex, state up |
| Rust | `rustc 1.100.0` / `cargo 1.100.0` at `~/.cargo/bin` (not on non-login `PATH`) |
| C compiler | gcc 14.2.0 |
| CMA at probe time | `CmaTotal: 262144 kB`, `CmaFree: 12824 kB` (idle; later idle samples ranged ~48,000–102,000 kB free, including a 101,860 kB reading) |

Quoted excerpts:

```text
/sys/firmware/devicetree/base/model: Raspberry Pi 3 Model B Rev 1.2
Linux raspberry 6.18.39+rpt-rpi-v8 #1 SMP PREEMPT Debian 1:6.18.39-1+rpt1 (2026-07-29) aarch64 GNU/Linux
temp=41.9'C
throttled=0x0
v4l2-ctl -d /dev/video10 --list-formats-out → [1]: 'H264' (H.264, compressed)
v4l2-ctl -d /dev/video10 --list-formats     → [2]: 'NV12' (Y/UV 4:2:0)
gst-inspect-1.0 --version → gst-inspect-1.0 version 1.26.2 / GStreamer 1.26.2
/sys/class/drm/card0-HDMI-A-1/status: connected
/sys/class/net/eth0/speed: 100
```

## 2. Synthetic fixtures

Generated on the Pi with its installed ffmpeg `7.1.5-0+deb13u1+rpt2`; no media was
downloaded. Directory: `/tmp/tv-client-c002-fixtures` (disposable). Both fixtures
are 20 s, 30 fps, H.264 High level 4.1 yuv420p, AAC-LC stereo 128 kbit/s, video
`testsrc2` plus a 440 Hz sine at reduced volume (`volume=0.25`).

```sh
mkdir -p /tmp/tv-client-c002-fixtures

ffmpeg -hide_banner -y \
  -f lavfi -i testsrc2=rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 20 -c:v libx264 -profile:v high -level:v 4.1 -x264-params cabac=1:bframes=3 \
  -pix_fmt yuv420p -preset veryfast -crf 23 -g 60 \
  -c:a aac -b:a 128k -ac 2 -af volume=0.25 -movflags +faststart \
  -vf scale=1280:720 /tmp/tv-client-c002-fixtures/fixture-720p30.mp4

ffmpeg -hide_banner -y \
  -f lavfi -i testsrc2=rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 20 -c:v libx264 -profile:v high -level:v 4.1 -x264-params cabac=1:bframes=3 \
  -pix_fmt yuv420p -preset veryfast -crf 23 -g 60 \
  -c:a aac -b:a 128k -ac 2 -af volume=0.25 -movflags +faststart \
  -vf scale=1920:1080 /tmp/tv-client-c002-fixtures/fixture-1080p30.mp4
```

| Fixture | Size (bytes) | Video bitrate | Streams (ffprobe) |
|---|---|---|---|
| `fixture-720p30.mp4` | 3,576,726 (3.41 MiB) | 1,293,170 bit/s | h264 High, level 4.1, 1280x720, yuv420p, 30/1; aac LC, 48 kHz, 2 ch, 128,011 bit/s |
| `fixture-1080p30.mp4` | 5,722,126 (5.46 MiB) | 2,151,330 bit/s | h264 High, level 4.1, 1920x1080, yuv420p, 30/1; aac LC, 48 kHz, 2 ch, 128,011 bit/s |

Both report `duration=20.000000` (600 video frames). A first attempt with
`-preset ultrafast` produced Constrained Baseline because that preset disables
CABAC and B-frames; it was discarded and both fixtures were regenerated with the
commands above, then verified with `ffprobe -show_entries stream=profile,...`.

## 3. Playback runs

`/usr/bin/time` is not installed on the target, so CPU and peak RSS were measured
with a Python 3.13 wrapper using `resource.getrusage(RUSAGE_CHILDREN)` around the
`gst-launch-1.0` child and `time.monotonic()` for wall time. Every run was wrapped
in `timeout 25`; the 20 s fixtures ran to EOS in about 20.2–21.8 s.

### 3.1 HDMI sink selection

```sh
XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 wpctl set-default 35
```

After the change, `wpctl status` shows the HDMI sink selected:

```text
 ├─ Sinks:
 │  *   35. Built-in Audio Digital Stereo (HDMI) [vol: 0.40]
 │      56. Built-in Audio Stereo               [vol: 0.00]
```

The choice persisted for all runs; `pactl` is not installed, so stream-to-sink
routing was verified through `wpctl status` and gst's `pulsesink` device property
instead.

### 3.2 720p30 hardware playthrough (waylandsink)

Effective pipeline (video and audio branches run in one `gst-launch-1.0`
process; `timeout 25` and the Python measurement wrapper were external):

```sh
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0
timeout 25 gst-launch-1.0 -v \
  filesrc location=/tmp/tv-client-c002-fixtures/fixture-720p30.mp4 \
  ! qtdemux name=d \
  d.video_0 ! queue ! h264parse ! v4l2h264dec ! videoconvert ! waylandsink \
  d.audio_0 ! queue ! aacparse ! avdec_aac ! audioconvert ! audioresample ! pulsesink
```

Exit status `0`, pipeline end `Execution ended after 0:00:20.208476594`.

Negotiated caps (proof of the elements actually used):

```text
v4l2h264dec0.GstPad:sink: video/x-h264, stream-format=byte-stream, alignment=au,
  level=4.1, profile=high, width=1280, height=720, framerate=30/1, chroma-format=4:2:0
v4l2h264dec0.GstPad:src:  video/x-raw, format=I420, width=1280, height=720,
  interlace-mode=progressive, framerate=30/1, colorimetry=bt709
waylandsink0.GstPad:sink: video/x-raw, width=1280, height=720, framerate=30/1,
  format=RGBx, colorimetry=1:1:5:1
pulsesink0: current-device = alsa_output.platform-3f902000.hdmi.hdmi-stereo
pulsesink0.GstPad:sink: audio/x-raw, rate=48000, format=F32LE, channels=2
```

| Metric | Observed |
|---|---|
| Exit status / timing | 0; EOS at 20.21 s of content, 21.75 s total including startup |
| CPU (user+sys / wall) | 20.89 s user + 0.94 s sys = **100.3 %** (single core; Pi 3B has four) |
| Peak RSS | 65,536 kB |
| Temp before / during / after | 48.3 °C / 49.4 °C / 48.3 °C |
| `get_throttled` | `0x0` before and after |
| CmaFree before / during / after | 50,560 / 49,368 / 57,284 kB (CmaTotal 262,144 kB) |
| Stream proof | `wpctl status` during playback: stream `gst-launch-1.0`, ports `output_FL/FR > MAI PCM i2s-hifi-0` active |
| QoS warnings | **14 ×** `waylandsink0: A lot of buffers are being dropped` |

The buffer-drop warnings mean this is not a clean 30 fps presentation; see §3.4.

### 3.3 1080p30 hardware playthrough (waylandsink)

Same pipeline and sink selection with the 1080p fixture.

| Metric | Observed |
|---|---|
| Exit status / timing | 0; 21.66 s total including startup (EOS before the 25 s timeout) |
| CPU (user+sys / wall) | 17.89 s user + 0.95 s sys = **87.0 %** |
| Peak RSS | 78,880 kB |
| Temp before / during / after | 46.2 °C / 49.4 °C / 49.9 °C |
| `get_throttled` | `0x0` before and after |
| CmaFree before / during / after | 57,372 / 48,988 / 61,688 kB |
| Decoder caps | h264 high, level 4.1, 1920x1080, 30/1 → `v4l2h264dec` → video/x-raw I420 1920x1080 |
| Sink caps | `waylandsink0.GstPad:sink`: 1920x1080, 30/1, `RGBx` |
| Audio | `pulsesink0: current-device = alsa_output.platform-3f902000.hdmi.hdmi-stereo` |
| QoS warnings | **16 ×** `A lot of buffers are being dropped` |

1080p did not error and no fallback path was needed: it also played to EOS at
real time in 20.06 s, but with its own presentation-side drops (§3.4), so it is
not a drop-free result either.

### 3.4 Presentation rate and bottleneck isolation

The runs in §3.2–3.3 reached EOS in about 20.2–21.7 s wall time for 20 s of
content, so the sink kept up with the clock. Two different measurements are
reported below and must not be conflated:

- **Paced playback** (`fpsdisplaysink video-sink=waylandsink text-overlay=false
  sync=true`, audio branch as in §3.2): the rate at which frames actually reach
  the display.
- **Throughput micro-benchmarks** (`sync=false`, and/or fpsdisplaysink overlay
  accounting): how fast the graph can move frames when the clock is not
  applied. These are ceilings, not playback rates.

#### Paced runs (`sync=true`)

Logs: `/tmp/c002-paced-720.out`, `/tmp/c002-paced-1080.out`,
`/tmp/c002-paced-720-recheck.out`.

| Run | Result |
|---|---|
| 720p30 paced | exit `0`, `Execution ended after 0:00:20.059526714`, **17** QoS warnings |
| 1080p30 paced | exit `0`, `Execution ended after 0:00:20.055754092`, **16** QoS warnings |
| 720p30 paced, repeat | exit `0`, `Execution ended after 0:00:20.062321828`, **15** QoS warnings |

Each fixture has 600 video frames (§2), so both resolutions played 20 s of
30 fps content in 20.06 s and stopped at EOS. The QoS warning
(`gst_base_sink_is_too_late`) is posted when the sink's late/drop ratio over a
reporting window is too high; each warning therefore stands for at least one
dropped buffer, and the counts give a drop *lower bound* of 17/600 ≈ **2.8 %**
at 720p and 16/600 ≈ **2.7 %** at 1080p (the repeat run's 15 warnings show the
usual run-to-run variation). Presentation is thus real time with roughly 3 %
late/dropped frames, not 3.6–7.8 fps. Whether that clears the plan's proposed
1 % threshold is discussed in §5.

#### Throughput micro-benchmarks (`sync=false`)

These runs used the 720p fixture unless noted; durations 20 s (or 10 s for the
synthetic-source checks). They measure headroom and drop behaviour under
artificial flooding, so their fps values are not presentation rates.

| Configuration | Result |
|---|---|
| `v4l2h264dec ! fpsdisplaysink video-sink=fakesink sync=false` | 550 frames, **120.2 fps** average, 0 dropped |
| 1080p fixture, same decode-only path | 593 frames, **34.6 fps** average, 0 dropped |
| `v4l2h264dec ! videoconvert ! fpsdisplaysink(fakesink) sync=false` | 546 frames, **120.0 fps** |
| `v4l2h264dec ! videoconvert ! fpsdisplaysink(waylandsink) sync=false` | 189 frames, **7.8 fps** |
| 720p with audio + `videoconvert ! waylandsink`, frame accounting | 50 rendered / 91 dropped, 3.6 fps |
| Same plus explicit `video/x-raw,format=RGBx` cap and `queue` | 1.8 fps, 38 rendered / 107 dropped |
| `v4l2h264dec ! fpsdisplaysink(glimagesink)` + HDMI audio | 369 rendered / 109 dropped, 18.3 fps average, exit 0 |
| `videotestsrc` 1280x720 → `videoconvert ! RGBx ! waylandsink` (sync=false) | 59.6 fps reported, 152 rendered / 104 dropped |
| `videotestsrc` 640x480 → `waylandsink`, 150 buffers, sync=true | EOS at 5.003 s (30 fps), exit 0 |

So the hardware decoder itself is fast enough (120 fps at 720p, 34.6 fps at
1080p, zero drops), and 720p decode + conversion reaches 120 fps to a fakesink.
The **3.6–7.8 fps figures in the micro-benchmark table are `sync=false` /
frame-accounting artifacts**: with the clock bypassed, `waylandsink` is flooded
with decoded buffers and reports its late-frame handling, not the rate seen
during playback. Under the clock (§3.2–3.3 and the paced runs above) the same
`waylandsink` path completes 20 s of 30 fps content in 20.06 s and drops a
small fraction instead. The loss that remains is presentation-side, not decode:
about 3 % of frames are late/dropped at both resolutions, and the GL-sink
alternative did not do better (369 rendered / 109 dropped, 18.3 fps average in
its frame-accounting run). labwc reports `WLR_RENDERER=pixman`, i.e. software
compositing at a 1920x1080 output; the exact split between wl_shm buffer handling
in `waylandsink` and pixman compositing was not profiled. A 1280x720 window fed
from a synthetic source can reach 30–60 fps, so the residual limitation is
specific to this decoded-video presentation path, not to labwc windows in
general.

### 3.5 Window / focus recovery

After the playback runs, a fresh 5-second window was opened from the same SSH
environment:

```sh
XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
  timeout 12 gst-launch-1.0 videotestsrc num-buffers=150 \
  ! video/x-raw,width=640,height=480 ! waylandsink
```

Observed: exit `0`, `Got EOS from element "pipeline0"`, `Execution ended after
0:00:05.003318625`. A `grim` screenshot taken during the run (1920x1080,
1,805,818 bytes) shows the labwc desktop with the new colour-bar test window
mapped in front of the wallpaper, so the compositor still creates and shows new
Wayland windows after playback. No focus-stealing or window-restore interaction
with a second application was exercised.

## 4. Reduced scope and not-run items

The operator explicitly waived the 30-minute soak, so every playback was a
20-second fixture run: 720p paced twice (plus the §3.2 playthrough), 1080p once.
The remaining reductions:

- 30-min soak: NOT RUN — waived by the operator; no sustained thermal,
  throttling, audio-continuity or frame-loss-over-time data exists.
- Slint placeholder: NOT RUN — no placeholder UI was built or launched
  (time and memory); the window check used `videotestsrc` only. No
  Slint→video→launcher focus sequence was exercised.
- No compositor change: cage was not started (it was installed with operator
  approval only), labwc was not restarted, no VT was switched, no `WLR_RENDERER`
  override was applied, and the 1920x1080 output mode was left untouched.
- No playback from the physical console; no operator visual confirmation was
  used as evidence beyond the screenshot.
- `pactl` is absent, so per-stream sink routing was confirmed via `wpctl` and
  gst properties rather than PulseAudio stream inspection.
- No 1080p fallback to 720p was needed (1080p exited 0), but no 1080p-quality
  claim is made: it dropped frames as well.
- Only 20 s of HDMI audio was heard per run; no multi-hour audio soak, no
  underrun counters, no volume/AV-sync measurement.

## 5. Gate assessment

Observed successfully: hardware H.264 decode through `v4l2h264dec` (caps and
120 fps at 720p to fakesink), HDMI audio via `pulsesink` routed to
`alsa_output.platform-3f902000.hdmi.hdmi-stereo` (active stream during playback),
creation of a new Wayland window in labwc after playback (exit 0, 5.003 s,
screenshot), and paced playback of both 20 s fixtures to EOS at real time:
720p in 20.0595 s with 17 QoS warnings, 1080p in 20.0558 s with 16 warnings
(a 720p repeat took 20.0623 s with 15 warnings).

The presentation is real time, not 3.6–7.8 fps; those earlier figures were
`sync=false`/frame-accounting micro-benchmark artifacts (§3.4). What remains
short of the plan's proposed 1 % drop budget is the presentation-side frame
loss: 17 warnings over 600 frames is ≈ 2.8 % (720p) and 16/600 is ≈ 2.7 %
(1080p) as a lower bound, under labwc 0.20.2 with `WLR_RENDERER=pixman`. The
drops are not decode-side (decode-only ran at 120 fps for 720p and 34.6 fps for
1080p); 1080p drops as well; and no long soak or physical-console visual check
beyond the screenshot exists. Per the operator's waiver of the soak and request
for basic coverage only, these are recorded as caveats rather than blockers.
If the 1 % drop budget matters for the product, retry under a GLES-capable
compositor or an approved Cage session, and if composition cost persists,
evaluate direct KMS presentation before UI work.

Core gate: PASS (basic) — 720p30/1080p30 hardware decode to waylandsink completed to EOS at ~real time with HDMI audio; 17/16 QoS drop warnings (~2.8% at 720p, ~2.7% at 1080p) exceed the proposed 1% threshold; 30-min soak and Slint placeholder NOT RUN per operator waiver.
