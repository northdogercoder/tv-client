#!/usr/bin/env bash
# Read-only capability probe for the TV client target Pi.
#
# Runs a fixed set of read-only commands over SSH and prints their raw output.
# It installs nothing, writes nothing on the target, and uses no credentials
# beyond the caller's SSH agent/key. BatchMode=yes guarantees no password prompt.
#
# Usage:
#   bash scripts/probe-pi.sh            # targets host alias "raspberry"
#   PI_HOST=pi.lan bash scripts/probe-pi.sh
set -euo pipefail

PI_HOST=${PI_HOST:-raspberry}

if ! ssh -o BatchMode=yes -o ConnectTimeout=10 "$PI_HOST" true; then
  printf 'probe-pi.sh: cannot reach %s over SSH (BatchMode)\n' "$PI_HOST" >&2
  exit 1
fi

printf 'probe-pi.sh: target %s, host UTC %s\n' "$PI_HOST" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

ssh -o BatchMode=yes "$PI_HOST" bash -s <<'REMOTE'
export LC_ALL=C

section() { printf '\n========== %s ==========\n' "$1"; }

# Print a command line, then its stdout+stderr; a non-zero status is reported
# but never aborts the probe.
show() {
  local label=$1
  shift
  printf '\n--- %s\n$ %s\n' "$label" "$*"
  "$@" 2>&1 || printf '[exit status %s]\n' "$?"
}

file_show() {
  local path=$1
  if [ -r "$path" ]; then
    printf '%s: ' "$path"
    tr -d '\0' <"$path" || true
    printf '\n'
  else
    printf '%s: [unreadable or absent]\n' "$path"
  fi
}

section "identity"
show hostname hostname
show date date -u
file_show /sys/firmware/devicetree/base/model
show os-release cat /etc/os-release
show uname uname -a
show machine uname -m
show arch dpkg --print-architecture
show user id

section "memory and CMA"
show free free -m
printf '\n--- CMA totals (/proc/meminfo)\n'
grep -E '^(Cma|MemAvailable|MemFree)' /proc/meminfo || true

section "thermal and throttling"
show temperature vcgencmd measure_temp
show throttled vcgencmd get_throttled

section "DRM / HDMI"
printf '\n--- /sys/class/drm entries\n'
ls /sys/class/drm || true
for attr in status enabled modes; do
  file_show "/sys/class/drm/card0-HDMI-A-1/$attr"
done

section "V4L2 hardware decode (/dev/video10)"
show v4l2-devices v4l2-ctl --list-devices
show formats-out v4l2-ctl -d /dev/video10 --list-formats-out
show formats-capture v4l2-ctl -d /dev/video10 --list-formats

section "GStreamer"
show gst-version gst-inspect-1.0 --version
printf '\n--- element presence\n'
for element in v4l2h264dec waylandsink kmssink playbin3 alsasink pulsesink queue; do
  if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
    printf '%-14s present\n' "$element"
  else
    printf '%-14s MISSING\n' "$element"
  fi
done

section "audio devices"
show aplay-list aplay -l
show wpctl-default wpctl get-default
printf '\n--- wpctl status (sinks)\n'
wpctl status 2>&1 | sed -n '1,40p' || true

section "HDMI CEC"
show cec-devices cec-ctl --list-devices

section "compositor and session"
printf '\n--- compositor processes\n'
ps -eo pid,user,comm,args | grep -E 'labwc|cage|weston|sway' | grep -v grep || true
printf '\n--- session runtime files\n'
ls -la /run/user/1000/wayland-* 2>&1 || true
show loginctl-sessions loginctl list-sessions --no-legend
show installed-shells dpkg-query -W -f='${Package} ${Version}\n' labwc cage weston 2>&1

section "browser"
show chromium-version chromium --version

section "network"
show addresses ip -br addr
printf '\n--- eth0 speed/duplex\n'
for attr in speed duplex operstate; do
  file_show "/sys/class/net/eth0/$attr"
done
show link ip -s link show eth0

section "build toolchain"
if [ -x "$HOME/.cargo/bin/rustc" ]; then
  show rustc "$HOME/.cargo/bin/rustc" --version
  show cargo "$HOME/.cargo/bin/cargo" --version
else
  printf '\n--- rust toolchain\n[absent: %s/.cargo/bin/rustc]\n' "$HOME"
fi
show pkg-config-gstreamer pkg-config --modversion gstreamer-1.0
show pkg-config-gstreamer-video pkg-config --modversion gstreamer-video-1.0
show gst-dev-headers sh -c 'ls /usr/include/gstreamer-1.0/gst/gst.h 2>/dev/null || echo absent'
show compiler gcc --version

printf '\n========== probe complete ==========\n'
REMOTE
