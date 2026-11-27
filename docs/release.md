# Release and deployment acceptance (C013)

This is the operator matrix for the packaged ARM64 kiosk. It is a template and
a status record, not evidence that a gate ran. The C013 executor did not build
the release workflow, install on the Pi, change services, reboot the lab target
or produce an attestation. Every row below is therefore **NOT RUN**, and the
artifact is **NOT BUILT**.

The historical C010 result (local YouTube 720p30 under Cage/GLES: 2 dropped
frames in 240) is an input to this matrix. It does not validate this artifact,
these scripts or a freshly installed kiosk.

## Artifact identity

Fill this table only from the real workflow run and the downloaded files. Do not
copy values from a plan or a previous release.

| Field | Value |
|---|---|
| Version | `<0.1.1>` |
| Git commit | `<git rev-parse HEAD>` |
| Workflow run | `<URL / run id>` |
| Artifact | `tv-client-linux-arm64-<version>.tar.gz` |
| SHA256 | `<from SHA256SUMS>` |
| Attestation | `<attestation URL or "not produced">` |
| Build host | `ubuntu-24.04-arm` (GitHub-hosted) |
| Install date | `<YYYY-MM-DD>` |
| Operator | `<name>` |

Status: **NOT BUILT**. The workflow exists at `.github/workflows/release.yml`;
it was not dispatched, pushed or run by C013. No checksum, ELF inspection or
attestation result is recorded here.

## Lab target

- Raspberry Pi 3 Model B Rev 1.2, Debian 13 (trixie), aarch64.
- Cage 0.3.1 on the DRM backend with `WLR_RENDERER=gles2`.
- Admin access over SSH or the tty2 recovery console; the kiosk account is
  `tvkiosk` with no sudo, no SSH keys and a locked password.
- Persistent paths: `/etc/tv-client/client.toml`, `/var/lib/tv-client`.
- Releases: `/opt/tv-client/releases/<version>`, selected by the atomic
  `/opt/tv-client/current` symlink.

## Operator matrix

| ID | Gate | Pass criterion | Status |
|---|---|---|---|
| R0 | Artifact build and verification | Workflow green on `ubuntu-24.04-arm`; checksum and attestation verify; both binaries are AArch64 with no unresolved libraries | **NOT RUN** (artifact NOT BUILT) |
| R1 | Install preflight, dry-run and idempotence | `--check` and `--dry-run` change nothing; repeated install keeps one release and one config | **NOT RUN** |
| R2 | Core local Jellyfin | Library loads, playback starts with audio, remote/keyboard input works, no stale process | **NOT RUN** |
| R3 | Pairing and cloud control | Device pairs, TV approval requires local keyboard/CEC, remote pause/seek acknowledged | **NOT RUN** |
| R4 | YouTube search and playback | Search returns results; local 720p30 under Cage/GLES plays without sustained stutter; 1080p30 is reported unsupported | **NOT RUN** |
| R5 | Reboot x5 | Five consecutive boots reach the kiosk with exactly one `cage` and one `tv-client`, no duplicate or stale process | **NOT RUN** |
| R6 | Safe power-loss | After the documented safe procedure the database opens, config is intact, kiosk recovers | **NOT RUN** |
| R7 | Offline Jellyfin / relay | Local UI shows a typed error without crashing; reconnect recovers without a restart | **NOT RUN** |
| R8 | Network relay and reconnect | Remote commands survive a short network loss or fail with a typed ack, then reconnect | **NOT RUN** |
| R9 | Two-hour soak | 2 h of repeated playback has stable RSS/FD counts, no leaked mpv sockets and no orphan children | **NOT RUN** |
| R10 | Rollback | Rollback selects the previous release atomically, preserves config/state bytes and restarts the kiosk | **NOT RUN** |
| R11 | Crash backoff and tty2 recovery | Repeated kiosk exits back off to 60 s; tty2 allows local recovery without rebooting | **NOT RUN** |

## R0: artifact build and verification

Preconditions: public repository, GitHub-hosted `ubuntu-24.04-arm`, no
production credentials.

Build on a tag or by manual dispatch:

```sh
git tag -s v0.1.1 -m 'tv-client 0.1.1'   # operator only; do not push from C013
git push origin v0.1.1                    # operator only
# or: gh workflow run release.yml --ref <branch-or-tag>
```

Download and verify:

```sh
version=0.1.1
gh run download <run-id> -n "tv-client-linux-arm64-$version" -D dist
cd dist
sha256sum -c SHA256SUMS
tar -tzf "tv-client-linux-arm64-$version.tar.gz"
# Expected root entries, nothing else:
#   NOTICES
#   SBOM.cdx.json
#   tv-client
#   tv-native-player
# dist/ also contains NOTICES and SBOM.cdx.json next to the tarball; SHA256SUMS
# covers all three files.
gh attestation verify "tv-client-linux-arm64-$version.tar.gz" --repo <owner>/<repo>
```

The workflow itself runs `file` and `ldd` on both binaries and fails on
`not found`; retain that log:

```sh
file target/release/tv-client target/release/tv-native-player
ldd target/release/tv-client
ldd target/release/tv-native-player
```

Evidence: `<paste the workflow log lines for file/ldd>`, `<paste sha256sum -c>`,
`<paste gh attestation verify>`. Status: **NOT RUN** — the workflow was not
executed and no artifact exists.

## R1: install preflight, dry-run and idempotence

```sh
sudo scripts/install-release.sh --check \
  --artifact dist/tv-client-linux-arm64-$version.tar.gz
sudo scripts/install-release.sh --dry-run \
  --artifact dist/tv-client-linux-arm64-$version.tar.gz
sudo scripts/install-release.sh \
  --artifact dist/tv-client-linux-arm64-$version.tar.gz
# Rerun the same command; it must reuse the release and keep the config.
sudo scripts/install-release.sh \
  --artifact dist/tv-client-linux-arm64-$version.tar.gz
readlink -f /opt/tv-client/current
find /opt/tv-client/releases -mindepth 1 -maxdepth 1 -type d
stat -c '%U:%G %a %n' /etc/tv-client/client.toml /var/lib/tv-client/client.toml /var/lib/tv-client
id -nG tvkiosk
```

A workstation-only staging preflight uses a self-contained tree; `--prefix`
is accepted only with `--check`/`--dry-run` and redirects the release,
configuration, state, home and systemd paths under that tree:

```sh
scripts/install-release.sh --check --prefix /var/tmp/tv-client-stage \
  --allow-foreign-arch --allow-unsupported-os \
  --artifact dist/tv-client-linux-arm64-$version.tar.gz
scripts/install-release.sh --dry-run --prefix /var/tmp/tv-client-stage \
  --allow-foreign-arch --allow-unsupported-os \
  --artifact dist/tv-client-linux-arm64-$version.tar.gz
test ! -e /var/tmp/tv-client-stage   # read-only preflight creates nothing
```

Pass requires: only the intended version directory; `current` points at it;
`/var/lib/tv-client` exists owned by `tvkiosk` with mode 0700;
`/var/lib/tv-client/client.toml` is seeded once, owned by `tvkiosk`, mode 0600;
`/etc/tv-client/client.toml` stays the operator fallback and is not replaced;
config and state are unchanged by the second run; `tvkiosk` groups contain no
`sudo`, `wheel` or `admin`; `/home/tvkiosk/.ssh` is absent or empty.

The scripts come from the repository checkout at the same tag; the release
tarball contains the two binaries, `NOTICES` and `SBOM.cdx.json` only.
`--allow-foreign-arch` is rejected for a real install; `--allow-unsupported-os`
is an explicit operator override that is also accepted for a real install.
`--prefix` is rejected for a real install.

### Display manager and tty1

The kiosk needs the DRM seat on tty1. A graphical display manager left enabled
on tty7 keeps a VT in front of the autologin session, so the 0.1.0 install
stayed black until the operator ran `systemctl disable --now lightdm` and
`chvt 1` by hand. The installer now detects `lightdm`, `gdm3`, `gdm` and
`sddm` (unit file or `systemctl is-enabled`/`is-active`), disables and stops
the detected unit, and activates tty1 with `chvt 1` when available. `--check`
and `--dry-run` only report the plan; they never disable a service.

Re-run the install (or the display-manager step) and verify:

```sh
systemctl status lightdm gdm3 gdm sddm --no-pager   # inactive and disabled
systemctl is-enabled getty@tty1.service
systemctl status getty@tty1.service --no-pager
pgrep -a cage
```

Pass requires the kiosk to appear on tty1 without a manual `chvt`. Recovery
console remains tty2.

Keep a display manager deliberately (for example on a lab host that still
needs the desktop) only with the explicit opt-out:

```sh
sudo scripts/install-release.sh --artifact ... --keep-display-manager
```

`--no-restart` also skips all display-manager and getty changes. Restore the
previous graphical autologin if the host is repurposed:

```sh
sudo systemctl enable --now lightdm   # or gdm3/gdm/sddm, whichever was disabled
sudo systemctl set-default graphical.target
```

Record which unit was disabled.
Evidence: `<paste preflight output>`, `<paste both install summaries>`,
`<paste id/stat output>`, `<paste systemctl status for the display manager>`,
`<paste the tty1 kiosk check>`. Status: **NOT RUN**.

## R2: core local Jellyfin

Do this with the network path the operator intends to keep (LAN or Tailscale).

```sh
systemctl status getty@tty1.service --no-pager
pgrep -a cage
pgrep -a tv-client
sudo -u tvkiosk /opt/tv-client/current/tv-client \
  --config /var/lib/tv-client/client.toml --check-config
journalctl -b -u getty@tty1.service -n 80 --no-pager
```

On the TV: open the Jellyfin library, start an H.264 title, confirm audio,
pause/resume, seek, then Back to the launcher. Confirm the player child exits
and no `tv-native-player` or `mpv` process remains.

Evidence: `<photo or notes: library loaded / playback / audio>`,
`<paste pgrep before and after teardown>`. Status: **NOT RUN**.

## Native player controls and HDMI audio (next-release candidate)

0.1.1 shipped a native player that preferred `pulsesink`. The kiosk account
`tvkiosk` has no PulseAudio or PipeWire server, so video was smooth while HDMI
audio stayed silent; the fullscreen `waylandsink` surface also had no controls
and swallowed keyboard/remote input. This change is not installed on the lab
target and **no Pi hardware was used**:

- **NOT RUN** — HDMI audio through `alsasink device=hw:0,0` as `tvkiosk`.
- **NOT RUN** — ALSA fallback order (`hw:0,0`, then `default`, then
  `autoaudiosink`, then a silent `fakesink`) with the HDMI card absent.
- **NOT RUN** — `textoverlay` hint visible over video at 720p30.
- **NOT RUN** — evdev Esc/Back stop, Space pause, Left/Right seek and Up/Down
  volume; no child process remains and the launcher returns without a failure.
- **NOT RUN** — the `input_unavailable` degraded state when `tvkiosk` is not in
  the `input` group.
- **NOT RUN** — `usermod --append --groups input tvkiosk` on an existing host
  and the reinstall that applies the new profile.

A generic desktop keeps `autoaudiosink`; only `deploy/kiosk-profile.sh.example`
sets `TV_AUDIO_SINK=alsa` and `TV_AUDIO_DEVICE=hw:0,0`. Set
`TV_PLAYER_OVERLAY=off` to disable the hint. Verification commands to run on
the Pi after reinstall (status **NOT RUN**):

```sh
id -nG tvkiosk
sudo -u tvkiosk aplay -D hw:0,0 /usr/share/sounds/alsa/Front_Center.wav
journalctl -b -t tv-native-player -n 40 --no-pager
```

Expected journal lines include `audio plan alsasink device=hw:0,0 > ...;
selected alsasink device=hw:0,0` and `evdev controls available`. If the log
says `permission denied`, the `input` group membership was not applied; if it
says `audio sink fallback to fakesink`, HDMI was absent or busy. Any result
from the list above remains **NOT RUN** until it is recorded here with
evidence.

HDMI-CEC remote keys still travel through the parent's CEC bridge; mouse
control is not implemented on the waylandsink surface. Both are limitations,
not verified features. The `textoverlay` bin inserts a `videoconvert` copy per
frame, so 720p30 frame headroom with the hint enabled is **NOT RUN** on
hardware; `TV_PLAYER_OVERLAY=off` falls back to the plain `waylandsink` path.

## R3: pairing and cloud control

Only if `cloud_enabled = true` and the control server is enrolled.

1. Start pairing from the operator control UI.
2. On the TV, approve with the physical keyboard or the TV remote (CEC). A
   remote cloud action must not approve itself.
3. From the control UI, send pause, seek and stop; confirm each acknowledgement.

```sh
journalctl -b -t tv-kiosk -n 40 --no-pager
journalctl -b -u getty@tty1.service -n 80 --no-pager
```

Evidence: `<pairing request id and local approval source>`,
`<paste remote acknowledgement timestamps>`. Status: **NOT RUN**.

## R4: YouTube search and 720p30 under Cage/GLES

Preconditions: `youtube_enabled = true`, `youtube_playback_enabled = true`, an
absolute executable `youtube_ytdlp_path`, and `mpv` present. The accepted
hardware gate is **720p30 under Cage/GLES**; **1080p30 is explicitly
unsupported** on the Pi 3B.

```sh
# Confirm the live session and renderer of the playing client.
pid=$(pgrep -n mpv)
tr '\0' '\n' < "/proc/$pid/environ" | grep -E 'WLR_RENDERER|WAYLAND_DISPLAY|XDG_RUNTIME_DIR'
journalctl -b -t tv-kiosk -n 60 --no-pager
```

Play a public 720p30 H.264/AAC video (for example the C010 probe id
`6v2L2UGZJAM`), then record the same evidence C010 used: selected format,
`hwdec-current`, dropped frames and total frames. A result is acceptable only
when the renderer is Cage/GLES and frame drops are comparable to the C010
720p30 run (2/240). Do not present a 1080p run as a pass; record it as
unsupported and keep the 720p-first policy.

Evidence: `<format id, resolution, fps>`, `<hwdec-current and renderer>`,
`<dropped/total frames>`, `<playback duration>`. Status: **NOT RUN**.

## R5: reboot x5

After each boot:

```sh
journalctl --list-boots --no-pager | tail -6
systemctl --failed --no-pager
systemctl status getty@tty1.service --no-pager
pgrep -a cage
pgrep -a tv-client
journalctl -b -t tv-kiosk -n 30 --no-pager
```

Repeat five times. Pass requires exactly one kiosk `cage` and one `tv-client`
per boot, no `getty@tty1` failure state and no stale child left by the previous
boot. A boot that reaches a login prompt instead of the kiosk fails.

Evidence: `<table: boot number / cage count / tv-client count / failures>`.
Status: **NOT RUN**.

## R6: safe power-loss procedure

This power loss procedure is destructive to the lab unit's filesystem only if
the procedure is wrong. Back up first and do it only with operator approval.

```sh
sudo tar -C / -czf /root/tv-client-state-backup.tgz \
  etc/tv-client var/lib/tv-client
sync
# Start a title, note the position, then remove mains power (or hold the power
# button) -- do not use `echo b > /proc/sysrq-trigger` without approval.
```

After boot:

```sh
sudo -u tvkiosk /opt/tv-client/current/tv-client \
  --config /var/lib/tv-client/client.toml --check-config
journalctl -b -p warning --no-pager
ls -la /var/lib/tv-client
```

Pass requires the SQLite state to open (no unrecoverable corruption), the
config to be byte-identical to the backup, and the kiosk to restart. Record any
`.corrupt-*` file; deleting the database is not an accepted recovery.

Evidence: `<backup checksum>`, `<position before/after>`, `<journal errors>`,
`<state directory listing>`. Status: **NOT RUN**.

## R7: offline Jellyfin and network relay

Simulate an unreachable Jellyfin origin without editing the Pi firewall:

```sh
ping -c 3 <jellyfin-host>
# Preferred: stop the Jellyfin service on the server, or disconnect the Pi's
# network cable / leave the Tailnet. Do not add firewall rules on the Pi.
```

Expected: the launcher remains usable and shows a typed error; it does not
crash or spin. Restore the network and retry without restarting the client.

Evidence: `<paste error text>`, `<recovery time>`. Status: **NOT RUN**.

## R8: network relay and reconnect

With a paired device, issue pause/seek/stop from the remote UI while playback is
active. Then drop the Pi's network for 10–30 s and restore it.

Expected: the in-flight acknowledgement does not claim success on a lost
command; after reconnect the device reconnects with a new pending queue, and a
new command applies. Core local Jellyfin playback is not stopped by a cloud
failure.

Evidence: `<ack results before/during/after>`, `<reconnect timestamps>`.
Status: **NOT RUN**.

## R9: two-hour soak

Run repeated Jellyfin and (if enabled) 720p30 YouTube playback for two hours.
Sample every 15 minutes:

```sh
pid=$(pgrep -n tv-client)
ps -o pid,rss,etime,cmd -p "$pid"
ls "/proc/$pid/fd" | wc -l
find /tmp -maxdepth 2 -name 'tv-client*' -o -name 'mpv*' | head
systemctl --failed --no-pager
```

Pass requires bounded RSS/FD growth, no leaked mpv/LibMPV sockets, no orphan
`mpv` or `tv-native-player` after each stop, and no service failure. C002's
30-minute soak waiver does not make this row run.

Evidence: `<samples table>`, `<leak observations>`. Status: **NOT RUN**.

## R10: rollback

Install a newer version first (or use the previously installed one), then:

```sh
sudo scripts/rollback.sh --list
sudo sha256sum /etc/tv-client/client.toml \
  /var/lib/tv-client/client.toml /var/lib/tv-client/client.db \
  > /tmp/tv-client-before-rollback.sha256
sudo scripts/rollback.sh --check --to <previous-version>
sudo scripts/rollback.sh --to <previous-version>
sudo sha256sum -c /tmp/tv-client-before-rollback.sha256
readlink -f /opt/tv-client/current
systemctl status getty@tty1.service --no-pager
```

Pass requires: the symlink flips atomically to the previous release; the
configuration and state checksums match the pre-rollback values; both release
directories still exist; the kiosk starts and shows the previous last-view/
progress state. By default the target is the release installed immediately
before the current one (install order, not version sort), so a `1.0.0-rc.1`
installed before `1.0.0` is selected when rolling back `1.0.0`; use `--to` for
an explicit version. Rollback never deletes a release and never migrates or
clears the database. If the newer binary migrated the database to a schema the older
binary does not support, the older binary refuses to start with a typed error:
reinstall the newer release instead of deleting `/var/lib/tv-client`.

Evidence: `<list output>`, `<before/after sha256>`, `<symlink>`,
`<journal after restart>`. Status: **NOT RUN**.

## R11: crash backoff and tty2 recovery

```sh
# In the kiosk session, simulate repeated exits without changing application
# code: stop the current release's process as tvkiosk, or point the profile at a
# deliberately invalid config in a lab-only copy.
sudo -u tvkiosk pkill -TERM -f '/opt/tv-client/current/tv-client'
journalctl -b -t tv-kiosk -n 40 --no-pager
```

Expected: the profile logs each exit and waits 5, 10, 20, 40, then 60 s; it does
not spin. Press Ctrl+Alt+F2, log in as the admin account (not `tvkiosk`), run
`--check-config`, fix the config, then `sudo systemctl restart getty@tty1`.
tty2 must have no autologin and must remain usable while tty1 is crashing.

Evidence: `<backoff timestamps>`, `<tty2 login and repair commands>`.
Status: **NOT RUN**.

## Evidence rules

- Paste command exit status and the relevant log lines; do not paste bearer
  tokens, enrollment tokens, URLs with credentials or full media URLs.
- A screenshot of the TV is acceptable for visual checks; it is not a substitute
  for the process and journal evidence above.
- Any row that was not executed stays **NOT RUN**. Do not turn an expected
  result into a measured result.
- The release artifact remains **NOT BUILT** until the workflow has produced it
  and the checksum/attestation values above are filled from that run.

## Sign-off

| Gate | Operator | Date | Result (NOT RUN / PASS / FAIL) | Evidence link |
|---|---|---|---|---|
| R0 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R1 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R2 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R3 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R4 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R5 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R6 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R7 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R8 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R9 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R10 | `<name>` | `<date>` | NOT RUN | `<link>` |
| R11 | `<name>` | `<date>` | NOT RUN | `<link>` |

A row may be marked PASS only with its evidence filled in. The accepted C010
YouTube gate is 720p30 under Cage/GLES; 1080p30 remains unsupported on the
Raspberry Pi 3B and is not a release blocker as long as it is not claimed.
