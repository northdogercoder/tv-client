#!/usr/bin/env bash
# Shared helpers for the C013 deployment scripts.
#
# This file is sourced, never executed. The deployment paths are fixed on
# purpose: an operator must not accidentally install over, or roll back into, a
# different tree because an environment variable was set in the sudo context.
# `--prefix` exists only for --check/--dry-run against a staging tree.

TV_PREFIX="/opt/tv-client"
TV_RELEASES_DIR="$TV_PREFIX/releases"
TV_CURRENT_LINK="$TV_PREFIX/current"
TV_CONFIG_DIR="/etc/tv-client"
TV_CONFIG_FILE="$TV_CONFIG_DIR/client.toml"
TV_STATE_DIR="/var/lib/tv-client"
TV_SYSTEMD_DIR="/etc/systemd/system"
TV_GETTY_OVERRIDE_DIR="$TV_SYSTEMD_DIR/getty@tty1.service.d"
TV_GETTY_OVERRIDE="$TV_GETTY_OVERRIDE_DIR/override.conf"
TV_KIOSK_USER="tvkiosk"
TV_KIOSK_HOME="/home/tvkiosk"
TV_KIOSK_PROFILE="$TV_KIOSK_HOME/.profile"
TV_REQUIRED_DEBIAN_VERSION="13"

# Set by use_staging_prefix(); empty for a real install/rollback.
TV_STAGING_PREFIX=""

DRY_RUN="${DRY_RUN:-0}"
CHECK_ONLY="${CHECK_ONLY:-0}"

script_name="${0##*/}"

log() {
  printf '%s: %s\n' "$script_name" "$*" >&2
}

die() {
  log "error: $*"
  exit 1
}

run() {
  if ((DRY_RUN)); then
    printf '  +' >&2
    printf ' %q' "$@" >&2
    printf '\n' >&2
    return 0
  fi
  "$@"
}

require_root() {
  if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
    die "must run as root (use --check or --dry-run for a read-only preflight)"
  fi
}

require_command() {
  local name=$1
  command -v "$name" >/dev/null 2>&1 || die "required command not found: $name"
}

validate_absolute_path() {
  local label=$1 path=$2
  [[ -n $path ]] || die "$label must not be empty"
  [[ $path == /* ]] || die "$label must be an absolute path: $path"
  [[ $path != *..* ]] || die "$label must not contain '..': $path"
  [[ $path != / ]] || die "$label must not be the filesystem root"
}

validate_version() {
  local version=$1
  [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] \
    || die "invalid release version (expected semver, e.g. 0.1.0): $version"
}

# Point the releases/current paths at a staging prefix. Only read-only modes
# may use this; a real install or rollback always uses /opt/tv-client.
use_staging_prefix() {
  local prefix=$1
  validate_absolute_path "prefix" "$prefix"
  if ((!DRY_RUN && !CHECK_ONLY)); then
    die "--prefix is only allowed with --check or --dry-run"
  fi
  TV_PREFIX=$prefix
  TV_RELEASES_DIR="$TV_PREFIX/releases"
  TV_CURRENT_LINK="$TV_PREFIX/current"
  TV_STAGING_PREFIX=$prefix
}

require_aarch64() {
  local machine
  machine=$(uname -m)
  case $machine in
    aarch64 | arm64) ;;
    *) die "release artifacts target aarch64; this host is $machine" ;;
  esac
}

require_debian_trixie() {
  local allow=${1:-0} id="" version_id=""
  [[ -r /etc/os-release ]] || die "cannot read /etc/os-release"
  # shellcheck disable=SC1091
  . /etc/os-release
  id=${ID:-}
  version_id=${VERSION_ID:-}
  if [[ $id != "debian" || $version_id != "$TV_REQUIRED_DEBIAN_VERSION" ]]; then
    if ((allow)); then
      log "warning: expected Debian $TV_REQUIRED_DEBIAN_VERSION, found ${id:-unknown} ${version_id:-unknown}"
    else
      die "expected Debian $TV_REQUIRED_DEBIAN_VERSION, found ${id:-unknown} ${version_id:-unknown}; pass --allow-unsupported-os only with operator approval"
    fi
  fi
}

# Install stdin as a root-owned, non-writable file. Idempotent: rewrite only
# when the content differs, so repeated installs do not churn mtimes.
install_root_file_if_changed() {
  local path=$1 mode=$2 tmp
  tmp=$(mktemp "${TMPDIR:-/tmp}/tv-client-file.XXXXXX") || die "could not create a temporary file"
  if ! cat >"$tmp"; then
    rm -f -- "$tmp"
    die "could not read file content for $path"
  fi
  if [[ -f $path ]] && cmp -s -- "$tmp" "$path"; then
    log "$path is up to date"
    rm -f -- "$tmp"
    return 0
  fi
  if ((DRY_RUN)); then
    log "would install $path (root:root, mode $mode)"
    sed 's/^/    | /' "$tmp" >&2
    rm -f -- "$tmp"
    return 0
  fi
  install -D -m "$mode" -o root -g root "$tmp" "$path"
  rm -f -- "$tmp"
}

# Reject absolute entries and parent traversal before extraction. The expected
# top-level names are checked separately by the installer.
validate_tar_entries() {
  local archive=$1 entry mode
  while IFS= read -r entry; do
    case $entry in
      /* | *..*) die "unsafe tar entry: $entry" ;;
    esac
  done < <(tar -tzf "$archive")
  # The release contains regular files only. Refuse links and device nodes so a
  # tampered archive cannot redirect a later write outside the staging tree.
  while IFS= read -r mode; do
    case ${mode:0:1} in
      -) ;;
      *) die "unsupported tar entry type in $archive: $mode" ;;
    esac
  done < <(tar -tvzf "$archive")
}

require_root_owned() {
  local path=$1
  if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
    log "skipping ownership check for $path (not root)"
    return 0
  fi
  [[ $(stat -c %u -- "$path") == "0" ]] || die "$path is not root-owned"
}

require_not_world_writable() {
  local path=$1
  if find "$path" -xdev -perm -0002 -print -quit 2>/dev/null | grep -q .; then
    die "$path contains a world-writable entry"
  fi
}
