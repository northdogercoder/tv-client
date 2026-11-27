#!/usr/bin/env bash
# Install a signed/attested tv-client ARM64 release on the Raspberry Pi kiosk.
#
# The script is deliberately narrow. It never installs OS packages, never edits
# the firewall, never changes SSH, and never overwrites an existing
# /etc/tv-client/client.toml. It installs one immutable release under
# /opt/tv-client/releases/<version> and atomically flips /opt/tv-client/current.
# The writable state copy is seeded once from the existing operator file, or
# from the shipped example when neither file exists.
#
# Usage:
#   sudo scripts/install-release.sh --artifact dist/tv-client-linux-arm64-0.1.0.tar.gz
#   sudo scripts/install-release.sh --check --artifact ...
#   sudo scripts/install-release.sh --dry-run --artifact ...
#   sudo scripts/install-release.sh --artifact ... --no-restart
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/deploy-common.sh
source "$script_dir/deploy-common.sh"

usage() {
  cat <<'EOF'
Usage: install-release.sh [options]

Options:
  --artifact PATH         Release tarball (default: dist/tv-client-linux-arm64-<version>.tar.gz)
  --version VERSION       Release version; derived from the artifact name when omitted
  --check                 Read-only preflight; validates host, artifact and paths
  --dry-run               Print every mutation without performing it
  --prefix DIR            Staging prefix for --check/--dry-run only
  --staging-install DIR   Complete an install under DIR as the invoking user
                          (integration testing; never touches systemd)
  --allow-foreign-arch    Permit --check/--dry-run/staging install on a non-aarch64 host
  --force                 Replace an existing release directory with different contents
  --no-restart            Install files but do not touch systemd/getty
  --keep-display-manager  Do not disable lightdm/gdm3/gdm/sddm or activate tty1
  --allow-unsupported-os  Continue on a non-Debian-13 host (operator approval only)
  -h, --help              Show this help

The real install path is fixed at /opt/tv-client. Persistent configuration is
/etc/tv-client/client.toml (operator fallback) and the writable
/var/lib/tv-client/client.toml (seeded once from the existing operator file,
or from deploy/client.toml.example when neither exists; owned by tvkiosk,
0600); persistent state is /var/lib/tv-client. Neither existing configuration
file is replaced by this script.
EOF
}

artifact=""
version=""
force=0
restart=1
keep_dm=0
allow_os=0
allow_arch=0
prefix=""
staging_install=""
STAGING_INSTALL=0
work=""
TV_STATE_CONFIG="$TV_STATE_DIR/client.toml"
TV_KIOSK_GROUP="$TV_KIOSK_USER"
TV_ROOT_OWNER="root"
TV_ROOT_GROUP="root"

# Redirect every mutable path into a self-contained tree. Used by --prefix
# (read-only modes) and --staging-install (integration install).
apply_staging_paths() {
  local root=$1
  TV_PREFIX=$root
  TV_RELEASES_DIR="$TV_PREFIX/releases"
  TV_CURRENT_LINK="$TV_PREFIX/current"
  TV_CONFIG_DIR="$TV_PREFIX/etc/tv-client"
  TV_CONFIG_FILE="$TV_CONFIG_DIR/client.toml"
  TV_STATE_DIR="$TV_PREFIX/var/lib/tv-client"
  TV_STATE_CONFIG="$TV_STATE_DIR/client.toml"
  TV_KIOSK_HOME="$TV_PREFIX/home/$TV_KIOSK_USER"
  TV_KIOSK_PROFILE="$TV_KIOSK_HOME/.profile"
  TV_SYSTEMD_DIR="$TV_PREFIX/etc/systemd/system"
  TV_GETTY_OVERRIDE_DIR="$TV_SYSTEMD_DIR/getty@tty1.service.d"
  TV_GETTY_OVERRIDE="$TV_GETTY_OVERRIDE_DIR/override.conf"
}

cleanup() {
  if [[ -n ${work:-} && -d ${work:-} ]]; then
    rm -rf -- "$work"
  fi
}
trap cleanup EXIT

while (($#)); do
  case $1 in
    --artifact)
      (($# >= 2)) || die "--artifact requires a path"
      artifact=$2
      shift
      ;;
    --version)
      (($# >= 2)) || die "--version requires a value"
      version=$2
      shift
      ;;
    --prefix)
      (($# >= 2)) || die "--prefix requires a path"
      prefix=$2
      shift
      ;;
    --staging-install)
      (($# >= 2)) || die "--staging-install requires a path"
      staging_install=$2
      shift
      ;;
    --check)
      CHECK_ONLY=1
      DRY_RUN=1
      ;;
    --dry-run)
      DRY_RUN=1
      ;;
    --force)
      force=1
      ;;
    --no-restart)
      restart=0
      ;;
    --keep-display-manager)
      keep_dm=1
      ;;
    --allow-unsupported-os)
      allow_os=1
      ;;
    --allow-foreign-arch)
      allow_arch=1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1 (see --help)"
      ;;
  esac
  shift
done

if [[ -n $prefix && -n $staging_install ]]; then
  die "--prefix and --staging-install are mutually exclusive"
fi
if [[ -n $staging_install ]]; then
  if ((CHECK_ONLY || DRY_RUN)); then
    die "--staging-install performs a real install and cannot be combined with --check or --dry-run"
  fi
  # Validate the operator-supplied string first so a `..` component cannot be
  # resolved into a real system path before it is rejected.
  validate_absolute_path "staging install root" "$staging_install"
  staging_install=$(realpath -m -- "$staging_install")
  validate_absolute_path "staging install root" "$staging_install"
  STAGING_INSTALL=1
  # The staging tree is owned by the invoking user, so the integration test
  # needs no root and no host tvkiosk account. systemd is never touched.
  TV_KIOSK_USER=$(id -un)
  TV_KIOSK_GROUP=$(id -gn)
  TV_ROOT_OWNER=$TV_KIOSK_USER
  TV_ROOT_GROUP=$TV_KIOSK_GROUP
  TV_STAGING_PREFIX=$staging_install
  apply_staging_paths "$staging_install"
  restart=0
  log "staging install: mutating only $TV_PREFIX as $TV_KIOSK_USER; systemd is not touched"
fi
if [[ -n $prefix ]]; then
  use_staging_prefix "$prefix"
  # A staging prefix is a self-contained tree: preflight never reads or writes
  # the real /etc, /var, /home or systemd paths.
  apply_staging_paths "$prefix"
fi

artifact_name() {
  printf '%s-%s.tar.gz\n' "tv-client-linux-arm64" "$1"
}

derive_version_from_artifact() {
  local base=${1##*/}
  local prefix_name="tv-client-linux-arm64-"
  [[ $base == "$prefix_name"*.tar.gz ]] \
    || die "artifact name must be ${prefix_name}<version>.tar.gz: $base"
  local derived=${base#"$prefix_name"}
  derived=${derived%.tar.gz}
  validate_version "$derived"
  printf '%s\n' "$derived"
}

if [[ -n $artifact && -z $version ]]; then
  version=$(derive_version_from_artifact "$artifact")
fi
if [[ -n $version ]]; then
  validate_version "$version"
fi
if [[ -z $artifact && -n $version ]]; then
  artifact="$script_dir/../dist/$(artifact_name "$version")"
fi
if [[ -n $artifact && -n $version ]]; then
  [[ ${artifact##*/} == "$(artifact_name "$version")" ]] \
    || die "artifact name does not match version $version: ${artifact##*/}"
fi
if [[ -n $artifact ]]; then
  artifact=$(realpath -m -- "$artifact")
  validate_absolute_path "artifact" "$artifact"
  [[ -f $artifact ]] || die "artifact not found: $artifact"
fi
if [[ -z $artifact ]] && ((CHECK_ONLY == 0)); then
  die "an artifact is required for install (see --help)"
fi

check_kiosk_user() {
  local groups group shell keyfile user_exists=0
  if [[ -n ${TV_STAGING_PREFIX:-} ]]; then
    log "staging prefix: skipping host kiosk-user checks"
    return 0
  fi
  if id -u "$TV_KIOSK_USER" >/dev/null 2>&1; then
    user_exists=1
    groups=$(id -nG "$TV_KIOSK_USER")
    for group in sudo wheel admin; do
      if [[ " $groups " == *" $group "* ]]; then
        die "kiosk user $TV_KIOSK_USER must not be in the privileged group '$group'"
      fi
    done
    if [[ ${EUID:-$(id -u)} -eq 0 ]]; then
      if grep -REqs "^[[:space:]]*${TV_KIOSK_USER}[[:space:]]" /etc/sudoers /etc/sudoers.d 2>/dev/null; then
        die "kiosk user has a sudoers rule"
      fi
    fi
    shell=$(getent passwd "$TV_KIOSK_USER" | cut -d: -f7)
    if [[ $shell != "/bin/bash" ]]; then
      log "kiosk user shell is '$shell'; it will be changed to /bin/bash for getty autologin"
    fi
  else
    log "kiosk user $TV_KIOSK_USER does not exist yet; it will be created"
  fi
  if [[ -d $TV_KIOSK_HOME/.ssh ]]; then
    for keyfile in authorized_keys authorized_keys2; do
      if [[ -e $TV_KIOSK_HOME/.ssh/$keyfile ]]; then
        die "kiosk home has $TV_KIOSK_HOME/.ssh/$keyfile; remove it first"
      fi
    done
    if [[ -n $(ls -A "$TV_KIOSK_HOME/.ssh" 2>/dev/null || true) ]]; then
      die "kiosk home has SSH material in $TV_KIOSK_HOME/.ssh; remove it first"
    fi
  fi
  if ((user_exists == 0)) && [[ -d $TV_KIOSK_HOME ]]; then
    local owner
    owner=$(stat -c %U -- "$TV_KIOSK_HOME")
    log "kiosk home already exists and is owned by $owner; verify it belongs to $TV_KIOSK_USER"
  fi
}

check_config() {
  local mode
  if [[ ! -e $TV_CONFIG_FILE ]]; then
    log "no $TV_CONFIG_FILE yet; the example will be installed"
    return 0
  fi
  [[ -f $TV_CONFIG_FILE && ! -L $TV_CONFIG_FILE ]] \
    || die "$TV_CONFIG_FILE must be a regular file, not a symlink"
  require_root_owned "$TV_CONFIG_FILE"
  mode=$(stat -c %a -- "$TV_CONFIG_FILE")
  if (( (8#$mode & 8#022) != 0 )); then
    die "$TV_CONFIG_FILE must not be group- or world-writable (mode $mode)"
  fi
}

check_state_dir() {
  local owner mode
  if [[ ! -e $TV_STATE_DIR ]]; then
    log "no $TV_STATE_DIR yet; it will be created for $TV_KIOSK_USER"
    return 0
  fi
  [[ -d $TV_STATE_DIR && ! -L $TV_STATE_DIR ]] || die "$TV_STATE_DIR must be a real directory"
  mode=$(stat -c %a -- "$TV_STATE_DIR")
  if (( (8#$mode & 8#022) != 0 )); then
    die "$TV_STATE_DIR must not be group- or world-writable (mode $mode)"
  fi
  if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
    log "skipping ownership check for $TV_STATE_DIR (not root)"
    return 0
  fi
  owner=$(stat -c %U -- "$TV_STATE_DIR")
  if [[ $owner != "$TV_KIOSK_USER" ]]; then
    die "$TV_STATE_DIR is owned by $owner; preserve it and chown it to $TV_KIOSK_USER explicitly"
  fi
}

check_state_config() {
  local mode owner
  if [[ ! -e $TV_STATE_CONFIG ]]; then
    if [[ -e $TV_CONFIG_FILE ]]; then
      log "no $TV_STATE_CONFIG yet; it will be seeded from the operator config $TV_CONFIG_FILE for $TV_KIOSK_USER (0600)"
    else
      log "no $TV_STATE_CONFIG yet; the example will be seeded for $TV_KIOSK_USER (0600)"
    fi
    return 0
  fi
  [[ -f $TV_STATE_CONFIG && ! -L $TV_STATE_CONFIG ]] \
    || die "$TV_STATE_CONFIG must be a regular file, not a symlink"
  mode=$(stat -c %a -- "$TV_STATE_CONFIG")
  if (( (8#$mode & 8#077) != 0 )); then
    die "$TV_STATE_CONFIG must be mode 0600 (found $mode)"
  fi
  if [[ ${EUID:-$(id -u)} -ne 0 || -n ${TV_STAGING_PREFIX:-} ]]; then
    log "skipping ownership check for $TV_STATE_CONFIG (staging or non-root)"
    return 0
  fi
  owner=$(stat -c %U -- "$TV_STATE_CONFIG")
  if [[ $owner != "$TV_KIOSK_USER" ]]; then
    die "$TV_STATE_CONFIG is owned by $owner; it must be owned by $TV_KIOSK_USER"
  fi
}

# Detect a graphical display manager that would keep the DRM seat on tty7.
#
# Staging trees and hosts without systemd only inspect unit files under
# $TV_SYSTEMD_DIR, so a workstation preflight stays read-only and deterministic.
detect_display_manager() {
  local unit
  for unit in lightdm gdm3 gdm sddm; do
    if [[ -e "$TV_SYSTEMD_DIR/$unit.service" ]]; then
      printf '%s\n' "$unit"
      return 0
    fi
  done
  if [[ -n ${TV_STAGING_PREFIX:-} ]] || ! command -v systemctl >/dev/null 2>&1; then
    return 1
  fi
  for unit in lightdm gdm3 gdm sddm; do
    if systemctl is-enabled --quiet "$unit" 2>/dev/null \
      || systemctl is-active --quiet "$unit" 2>/dev/null; then
      printf '%s\n' "$unit"
      return 0
    fi
  done
  return 1
}

report_display_manager() {
  local unit
  if ((!restart)); then
    log "not handling a display manager (--no-restart)"
    return 0
  fi
  if unit=$(detect_display_manager); then
    if ((keep_dm)); then
      log "display manager $unit left enabled (--keep-display-manager)"
    else
      log "display manager $unit will be disabled and stopped; tty1 will be activated"
    fi
  else
    log "no conflicting display manager detected"
  fi
}

disable_display_manager() {
  local unit
  if ((!restart)); then
    log "skipping display-manager changes (--no-restart)"
    return 0
  fi
  if ((keep_dm)); then
    log "keeping the display manager (--keep-display-manager)"
    return 0
  fi
  if ! unit=$(detect_display_manager); then
    log "no conflicting display manager found"
    return 0
  fi
  log "disabling display manager $unit so tty1 can own the DRM seat"
  run systemctl disable --now "$unit" \
    || die "could not disable $unit; pass --keep-display-manager to skip"
  if command -v chvt >/dev/null 2>&1; then
    run chvt 1
  else
    log "chvt is unavailable; switch to tty1 with Ctrl+Alt+F1"
  fi
}

check_artifact() {
  local sums expected actual top_level
  [[ -f $artifact ]] || die "artifact not found: $artifact"
  validate_tar_entries "$artifact"

  top_level=$(tar -tzf "$artifact" | cut -d/ -f1 | sort -u | tr '\n' ' ')
  for required in tv-client tv-native-player NOTICES SBOM.cdx.json; do
    [[ " $top_level " == *" $required "* ]] \
      || die "artifact is missing $required at the tar root"
  done
  for entry in $top_level; do
    case $entry in
      tv-client | tv-native-player | NOTICES | SBOM.cdx.json) ;;
      *) die "unexpected tar-root entry: $entry" ;;
    esac
  done

  sums="$(dirname -- "$artifact")/SHA256SUMS"
  if [[ -f $sums ]]; then
    expected=$(awk -v name="${artifact##*/}" '
      { candidate = $2; sub(/^\*/, "", candidate) }
      candidate == name { print $1; exit }
    ' "$sums")
    [[ -n $expected ]] || die "SHA256SUMS does not list ${artifact##*/}"
  elif [[ -n ${TV_ARTIFACT_SHA256:-} ]]; then
    expected=$TV_ARTIFACT_SHA256
  else
    die "refusing to install without SHA256SUMS next to the artifact or TV_ARTIFACT_SHA256"
  fi
  actual=$(sha256sum "$artifact" | awk '{ print $1 }')
  [[ $actual == "$expected" ]] || die "artifact checksum mismatch for ${artifact##*/}"
  artifact_sha256=$actual
  log "artifact checksum verified: ${artifact##*/}"
}

# --- Preflight (read-only) -------------------------------------------------

if ((allow_arch)); then
  if ((!DRY_RUN && !STAGING_INSTALL)); then
    die "--allow-foreign-arch is only allowed with --check, --dry-run or --staging-install"
  fi
  log "warning: skipping the aarch64 host check (--allow-foreign-arch)"
else
  require_aarch64
fi
require_command install
require_command tar
require_command sha256sum
require_command realpath
require_command awk
require_command stat
require_command find
require_command getent
require_debian_trixie "$allow_os"

if ((STAGING_INSTALL)); then
  log "staging install: skipping host runtime and service checks"
elif ((DRY_RUN)) && { ((allow_arch)) || [[ -n ${TV_STAGING_PREFIX:-} ]]; }; then
  # Read-only preflight may run on a staging workstation without the Pi's
  # runtime helpers. Report them without blocking --check/--dry-run.
  for runtime_command in systemctl useradd usermod passwd cage; do
    if ! command -v "$runtime_command" >/dev/null 2>&1; then
      log "warning: $runtime_command is missing (required for a real install)"
    fi
  done
  if [[ ! -x /sbin/agetty ]]; then
    log "warning: /sbin/agetty is missing (required for getty autologin)"
  fi
else
  require_command systemctl
  require_command useradd
  require_command usermod
  require_command passwd
  require_command cage
  [[ -x /sbin/agetty ]] || die "/sbin/agetty is missing; getty autologin cannot work"
fi
if ! command -v cec-ctl >/dev/null 2>&1; then
  log "warning: cec-ctl is absent; HDMI-CEC will report unavailable and the USB keyboard remains usable"
fi

check_kiosk_user
check_config
check_state_dir
check_state_config
report_display_manager
if [[ -n $artifact ]]; then
  check_artifact
fi
if [[ -d $TV_RELEASES_DIR/$version && -n $version ]]; then
  if [[ -f $TV_RELEASES_DIR/$version/.artifact.sha256 ]]; then
    installed_sha=$(<"$TV_RELEASES_DIR/$version/.artifact.sha256")
    if [[ $installed_sha != "${artifact_sha256:-}" ]]; then
      if ((force)); then
        log "release $version will be replaced (--force)"
      else
        die "release $version is installed with different contents; use --force to replace it"
      fi
    else
      log "release $version is already installed with the matching artifact"
    fi
  fi
fi

if ((CHECK_ONLY)); then
  log "check passed; no changes made"
  if [[ -n $version ]]; then
    log "would install release $version to $TV_RELEASES_DIR/$version"
  fi
  exit 0
fi

# --- Mutations -------------------------------------------------------------

if ((!DRY_RUN && !STAGING_INSTALL)); then
  require_root
fi

ensure_kiosk_user() {
  if ((STAGING_INSTALL)); then
    log "staging install: using $TV_KIOSK_USER instead of creating the tvkiosk account"
    return 0
  fi
  if id -u "$TV_KIOSK_USER" >/dev/null 2>&1; then
    log "kiosk user $TV_KIOSK_USER already exists"
  else
    run useradd \
      --create-home \
      --home-dir "$TV_KIOSK_HOME" \
      --shell /bin/bash \
      --user-group \
      --password '!' \
      "$TV_KIOSK_USER"
  fi
  run usermod --shell /bin/bash "$TV_KIOSK_USER"
  # getty --autologin does not need an account password. Locking it keeps
  # password logins and SSH password authentication unusable.
  run passwd -l "$TV_KIOSK_USER"
  local group
  for group in video render audio; do
    if getent group "$group" >/dev/null 2>&1; then
      run usermod --append --groups "$group" "$TV_KIOSK_USER"
    fi
  done
}

install_release() {
  local destination="$TV_RELEASES_DIR/$version"
  if [[ -d $destination ]]; then
    if [[ -f $destination/.artifact.sha256 ]] \
      && [[ $(<"$destination/.artifact.sha256") == "$artifact_sha256" ]]; then
      [[ -x $destination/tv-client && -x $destination/tv-native-player ]] \
        || die "installed release $version is missing an executable binary"
      log "reusing installed release $version"
      return 0
    fi
    if ((force)); then
      run rm -rf -- "$destination"
    else
      die "refusing to replace $destination without --force"
    fi
  fi
  if ((DRY_RUN)); then
    log "would extract $artifact into $destination"
    return 0
  fi
  install -d -m 0755 -o "$TV_ROOT_OWNER" -g "$TV_ROOT_GROUP" "$TV_RELEASES_DIR"
  work=$(mktemp -d "$TV_RELEASES_DIR/.staging.XXXXXX")
  tar -xzf "$artifact" -C "$work" --no-same-owner --no-same-permissions
  [[ -f $work/tv-client && ! -L $work/tv-client ]] \
    || die "artifact tv-client is not a regular file"
  [[ -f $work/tv-native-player && ! -L $work/tv-native-player ]] \
    || die "artifact tv-native-player is not a regular file"
  chown -R "$TV_ROOT_OWNER:$TV_ROOT_GROUP" "$work"
  find "$work" -type d -exec chmod 0755 {} +
  find "$work" -type f ! -name tv-client ! -name tv-native-player -exec chmod 0644 {} +
  chmod 0755 "$work/tv-client" "$work/tv-native-player"
  printf '%s\n' "$artifact_sha256" >"$work/.artifact.sha256"
  chmod 0644 "$work/.artifact.sha256"
  mv -- "$work" "$destination"
  work=""
  log "installed release $version at $destination"
}

install_config() {
  local example="$script_dir/../deploy/client.toml.example"
  if [[ -e $TV_CONFIG_FILE ]]; then
    log "keeping existing $TV_CONFIG_FILE"
    return 0
  fi
  [[ -f $example ]] || die "cannot install a missing config: $example not found"
  if ((DRY_RUN)); then
    log "would install $TV_CONFIG_FILE from $example ($TV_ROOT_OWNER:$TV_ROOT_GROUP, 0644)"
    return 0
  fi
  install -d -m 0755 -o "$TV_ROOT_OWNER" -g "$TV_ROOT_GROUP" "$TV_CONFIG_DIR"
  install -m 0644 -o "$TV_ROOT_OWNER" -g "$TV_ROOT_GROUP" "$example" "$TV_CONFIG_FILE"
  log "installed initial $TV_CONFIG_FILE; review it before enabling features"
}

install_state_config() {
  local example="$script_dir/../deploy/client.toml.example"
  local source
  if [[ -e $TV_STATE_CONFIG ]]; then
    log "keeping existing $TV_STATE_CONFIG"
    return 0
  fi
  if [[ -e $TV_CONFIG_FILE ]]; then
    source=$TV_CONFIG_FILE
  else
    [[ -f $example ]] || die "cannot seed a missing config: $example not found"
    source=$example
  fi
  if ((DRY_RUN)); then
    log "would seed $TV_STATE_CONFIG from $source ($TV_KIOSK_USER:$TV_KIOSK_GROUP, 0600)"
    return 0
  fi
  install -d -m 0700 -o "$TV_KIOSK_USER" -g "$TV_KIOSK_GROUP" "$TV_STATE_DIR"
  install -m 0600 -o "$TV_KIOSK_USER" -g "$TV_KIOSK_GROUP" "$source" "$TV_STATE_CONFIG"
  if [[ $source == "$TV_CONFIG_FILE" ]]; then
    log "seeded $TV_STATE_CONFIG from the operator config $TV_CONFIG_FILE for $TV_KIOSK_USER (0600)"
  else
    log "seeded $TV_STATE_CONFIG from $source for $TV_KIOSK_USER (0600); edit it from the TV Settings screen"
  fi
}

install_kiosk_profile() {
  if ((STAGING_INSTALL)); then
    log "staging install: skipping the kiosk login profile"
    return 0
  fi
  local source="$script_dir/../deploy/kiosk-profile.sh.example"
  [[ -f $source ]] || die "missing kiosk profile template: $source"
  local shadow
  for shadow in "$TV_KIOSK_HOME/.bash_profile" "$TV_KIOSK_HOME/.bash_login"; do
    if [[ -e $shadow ]]; then
      die "remove $shadow so the kiosk .profile is read at login"
    fi
  done
  if [[ ! -d $TV_KIOSK_HOME ]]; then
    run install -d -m 0755 -o "$TV_KIOSK_USER" -g "$TV_KIOSK_GROUP" "$TV_KIOSK_HOME"
  elif [[ $(stat -c %U -- "$TV_KIOSK_HOME") != "$TV_KIOSK_USER" ]]; then
    # Only the directory itself: existing root-owned dotfiles stay root-owned.
    run chown "$TV_KIOSK_USER:$TV_KIOSK_GROUP" "$TV_KIOSK_HOME"
  fi
  install_root_file_if_changed "$TV_KIOSK_PROFILE" 0644 <"$source"
}

install_getty_override() {
  if ((STAGING_INSTALL)); then
    log "staging install: skipping the getty override"
    return 0
  fi
  local source="$script_dir/../deploy/getty-override.conf.example"
  [[ -f $source ]] || die "missing getty override template: $source"
  install_root_file_if_changed "$TV_GETTY_OVERRIDE" 0644 <"$source"
}

set_current_release() {
  local temporary="$TV_PREFIX/.current.$$"
  run ln -sfn "$TV_RELEASES_DIR/$version" "$temporary"
  run mv -T -- "$temporary" "$TV_CURRENT_LINK"
  if ((DRY_RUN)); then
    log "would set current -> $TV_RELEASES_DIR/$version"
  else
    log "current -> $TV_RELEASES_DIR/$version"
  fi
}

restart_kiosk() {
  run systemctl daemon-reload
  # tty2 stays a normal login console: local recovery must not be another kiosk.
  run systemctl enable --now getty@tty2.service
  run systemctl enable getty@tty1.service
  run systemctl restart getty@tty1.service
  if ((DRY_RUN)); then
    log "would restart getty@tty1; local recovery console is tty2 (Ctrl+Alt+F2)"
  else
    log "getty@tty1 restarted; local recovery console is tty2 (Ctrl+Alt+F2)"
  fi
}

install_state_dir() {
  if [[ -d $TV_STATE_DIR ]]; then
    log "keeping existing state directory $TV_STATE_DIR"
    return 0
  fi
  run install -d -m 0700 -o "$TV_KIOSK_USER" -g "$TV_KIOSK_GROUP" "$TV_STATE_DIR"
  if ((DRY_RUN)); then
    log "would create state directory $TV_STATE_DIR for $TV_KIOSK_USER (0700)"
  else
    log "state directory $TV_STATE_DIR created for $TV_KIOSK_USER (0700)"
  fi
}

run install -d -m 0755 -o "$TV_ROOT_OWNER" -g "$TV_ROOT_GROUP" "$TV_PREFIX" "$TV_RELEASES_DIR" "$TV_CONFIG_DIR"

ensure_kiosk_user
install_state_dir
install_release
# Seed the writable copy before install_config can create /etc, so an upgrade
# copies the existing operator configuration instead of the example.
install_state_config
install_config
install_kiosk_profile
install_getty_override
set_current_release

if ((restart)); then
  disable_display_manager
  restart_kiosk
else
  log "skipping systemd/getty restart (--no-restart)"
fi

if ((DRY_RUN)); then
  log "dry-run complete; nothing was modified"
else
  log "installation complete"
fi
log "release: $TV_RELEASES_DIR/$version"
log "config:  $TV_CONFIG_FILE (operator fallback, never overwritten)"
log "state config: $TV_STATE_CONFIG (owned by $TV_KIOSK_USER, 0600, never overwritten)"
log "state:   $TV_STATE_DIR (persistent, owned by $TV_KIOSK_USER)"
