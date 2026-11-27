#!/usr/bin/env bash
# Roll /opt/tv-client/current back to an already-installed release.
#
# Rollback only flips the current symlink and restarts the kiosk login. It never
# deletes a release, never edits /etc/tv-client/client.toml and never touches
# /var/lib/tv-client. Persistent state is deliberately shared by all releases:
# if a newer binary migrated the database to a schema the older binary does not
# understand, the older binary refuses to start with a typed error. Reinstall
# the newer release in that case; do not delete the database.
#
# Usage:
#   sudo scripts/rollback.sh --list
#   sudo scripts/rollback.sh --check [--to VERSION]
#   sudo scripts/rollback.sh [--to VERSION] [--no-restart]
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=scripts/deploy-common.sh
source "$script_dir/deploy-common.sh"

usage() {
  cat <<'EOF'
Usage: rollback.sh [options]

Options:
  --list                  List installed releases; mark the current one
  --to VERSION            Roll back to this installed version
                          (default: the release installed before current)
  --check                 Read-only validation; print the planned target
  --dry-run               Print the symlink swap without performing it
  --prefix DIR            Staging prefix for --list/--check/--dry-run only
  --no-restart            Swap the symlink but do not restart getty@tty1
  -h, --help              Show this help

Persistent configuration (/etc/tv-client) and state (/var/lib/tv-client) are
never modified. No installed release is ever removed.
EOF
}

target_requested=""
list_only=0
restart=1
prefix=""
temporary_link=""

cleanup() {
  if [[ -n ${temporary_link:-} ]]; then
    rm -f -- "$temporary_link"
  fi
}
trap cleanup EXIT

while (($#)); do
  case $1 in
    --list)
      list_only=1
      ;;
    --to)
      (($# >= 2)) || die "--to requires a version"
      target_requested=$2
      shift
      ;;
    --check)
      CHECK_ONLY=1
      DRY_RUN=1
      ;;
    --dry-run)
      DRY_RUN=1
      ;;
    --prefix)
      (($# >= 2)) || die "--prefix requires a path"
      prefix=$2
      shift
      ;;
    --no-restart)
      restart=0
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

if [[ -n $prefix ]]; then
  if ((list_only)); then
    # Listing is read-only; treat it like a dry-run for prefix validation.
    DRY_RUN=1
  fi
  use_staging_prefix "$prefix"
fi
if [[ -n $target_requested ]]; then
  validate_version "$target_requested"
fi

list_releases() {
  if [[ ! -d $TV_RELEASES_DIR ]]; then
    return 0
  fi
  find "$TV_RELEASES_DIR" -mindepth 1 -maxdepth 1 -type d ! -name '.*' -printf '%f\n' \
    | sort -V
}

current_target=""
if [[ -L $TV_CURRENT_LINK ]]; then
  current_target=$(readlink -f -- "$TV_CURRENT_LINK" 2>/dev/null || true)
fi
if [[ -n $current_target ]]; then
  case $current_target in
    "$TV_RELEASES_DIR"/*) ;;
    *) die "$TV_CURRENT_LINK points outside $TV_RELEASES_DIR: $current_target" ;;
  esac
fi
current_version=""
if [[ -n $current_target ]]; then
  current_version=${current_target##*/}
fi

mapfile -t releases < <(list_releases)
if ((${#releases[@]} == 0)); then
  if ((list_only)); then
    log "no releases installed under $TV_RELEASES_DIR"
    exit 0
  fi
  die "no releases installed under $TV_RELEASES_DIR"
fi

if ((list_only)); then
  for release in "${releases[@]}"; do
    marker=""
    if [[ $release == "$current_version" ]]; then
      marker=" (current)"
    fi
    printf '%s%s\n' "$release" "$marker"
  done
  exit 0
fi

select_target() {
  if [[ -n $target_requested ]]; then
    printf '%s\n' "$target_requested"
    return 0
  fi
  if [[ -z $current_version ]]; then
    printf '%s\n' "${releases[-1]}"
    return 0
  fi
  local index=-1
  local i
  for i in "${!releases[@]}"; do
    if [[ ${releases[i]} == "$current_version" ]]; then
      index=$i
      break
    fi
  done
  if ((index < 0)); then
    die "current release '$current_version' is not present in $TV_RELEASES_DIR"
  fi
  if ((index == 0)); then
    die "no older release to roll back to; use --to VERSION"
  fi
  printf '%s\n' "${releases[index - 1]}"
}

target_version=$(select_target)
validate_version "$target_version"
if [[ $target_version == "$current_version" ]]; then
  die "release $target_version is already current"
fi
target_dir="$TV_RELEASES_DIR/$target_version"

check_target_release() {
  [[ -d $target_dir ]] || die "release not installed: $target_version"
  local binary
  for binary in tv-client tv-native-player; do
    [[ -f $target_dir/$binary && -x $target_dir/$binary ]] \
      || die "release $target_version is incomplete: $binary missing or not executable"
    require_root_owned "$target_dir/$binary"
  done
  require_root_owned "$target_dir"
  if find "$target_dir" -xdev -perm -0002 -print -quit 2>/dev/null | grep -q .; then
    die "release $target_version contains a world-writable entry"
  fi
}

check_target_release
# The deployment target is aarch64. A staging prefix is a read-only test tree
# and may live on the operator's x86_64 workstation.
if [[ $TV_PREFIX == "/opt/tv-client" ]]; then
  require_aarch64
fi

log "current: ${current_version:-none}"
log "rollback target: $target_version"
log "preserving $TV_CONFIG_DIR and $TV_STATE_DIR unchanged"

if ((CHECK_ONLY)); then
  log "check passed; no changes made"
  exit 0
fi

if ((!DRY_RUN)); then
  require_root
fi
require_command systemctl

temporary_link="$TV_PREFIX/.current.$$"
run ln -sfn "$target_dir" "$temporary_link"
run mv -T -- "$temporary_link" "$TV_CURRENT_LINK"
temporary_link=""
if ((DRY_RUN)); then
  log "would switch current -> $target_dir"
else
  log "current -> $target_dir"
fi

if ((restart)); then
  run systemctl restart getty@tty1.service
  if ((DRY_RUN)); then
    log "would restart getty@tty1; local recovery console is tty2 (Ctrl+Alt+F2)"
  else
    log "getty@tty1 restarted; local recovery console is tty2 (Ctrl+Alt+F2)"
  fi
else
  log "skipping getty restart (--no-restart)"
fi

if ((DRY_RUN)); then
  log "dry-run complete; nothing was modified"
else
  log "rollback complete; configuration and state were not modified"
fi
