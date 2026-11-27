#!/usr/bin/env bash
# Package the ARM64 release artifact used by .github/workflows/release.yml.
#
# The tarball has the two binaries at its root, together with a CycloneDX SBOM
# and a third-party notice inventory. SHA256SUMS is written next to the
# tarball. The tar metadata is normalised (owner/group 0, sorted names, epoch
# mtime, gzip -n) so the same inputs produce the same bytes.
#
# Usage:
#   scripts/package-release.sh --version 0.1.0
#   scripts/package-release.sh --print-name 0.1.0
#   scripts/package-release.sh --inventory metadata.json out-dir
#
# Environment:
#   SOURCE_DATE_EPOCH  Normalised tar mtime (CI sets it from the commit time).
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
artifact_prefix="tv-client-linux-arm64"
source_epoch="${SOURCE_DATE_EPOCH:-0}"
out_dir="$repo_root/dist"
bin_dir="$repo_root/target/release"
version=""
metadata=""
inventory_out=""
mode="package"
work=""

usage() {
  cat <<'EOF'
Usage: package-release.sh [options]

Options:
  --version VERSION        Release version (default: package version in Cargo.toml)
  --bin-dir DIR            Directory containing tv-client and tv-native-player
                           (default: target/release)
  --out-dir DIR            Output directory for the tarball and SHA256SUMS
                           (default: dist)
  --metadata FILE          Use this cargo-metadata JSON instead of running cargo
  --source-date-epoch SEC  Normalised tar mtime (default: $SOURCE_DATE_EPOCH or 0)
  --print-name VERSION     Print the artifact file name and exit
  --inventory META OUTDIR  Generate SBOM.cdx.json and NOTICES from cargo metadata
  -h, --help               Show this help

The tarball contains exactly these root entries:
  tv-client, tv-native-player, NOTICES, SBOM.cdx.json
EOF
}

while (($#)); do
  case $1 in
    --version)
      (($# >= 2)) || { echo "error: --version requires a value" >&2; exit 2; }
      version=$2
      shift
      ;;
    --bin-dir)
      (($# >= 2)) || { echo "error: --bin-dir requires a path" >&2; exit 2; }
      bin_dir=$2
      shift
      ;;
    --out-dir)
      (($# >= 2)) || { echo "error: --out-dir requires a path" >&2; exit 2; }
      out_dir=$2
      shift
      ;;
    --metadata)
      (($# >= 2)) || { echo "error: --metadata requires a path" >&2; exit 2; }
      metadata=$2
      shift
      ;;
    --source-date-epoch)
      (($# >= 2)) || { echo "error: --source-date-epoch requires a value" >&2; exit 2; }
      source_epoch=$2
      shift
      ;;
    --print-name)
      (($# >= 2)) || { echo "error: --print-name requires a version" >&2; exit 2; }
      mode="print-name"
      version=$2
      shift
      ;;
    --inventory)
      (($# >= 3)) || { echo "error: --inventory requires METADATA and OUTDIR" >&2; exit 2; }
      mode="inventory"
      metadata=$2
      inventory_out=$3
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1 (see --help)" >&2
      exit 2
      ;;
  esac
  shift
done

validate_version() {
  local value=$1
  if [[ ! $value =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
    echo "error: invalid version: $value" >&2
    exit 2
  fi
}

artifact_name() {
  printf '%s-%s.tar.gz\n' "$artifact_prefix" "$1"
}

if [[ $mode == "print-name" ]]; then
  validate_version "$version"
  artifact_name "$version"
  exit 0
fi

if [[ ! $source_epoch =~ ^[0-9]+$ ]]; then
  echo "error: SOURCE_DATE_EPOCH/--source-date-epoch must be an integer" >&2
  exit 2
fi

require_jq() {
  command -v jq >/dev/null 2>&1 \
    || { echo "error: jq is required to generate the SBOM and notices" >&2; exit 1; }
}

generate_inventory() {
  local input=$1 output=$2 timestamp
  [[ -f $input ]] || { echo "error: cargo metadata not found: $input" >&2; exit 1; }
  require_jq
  mkdir -p -- "$output"
  timestamp=$(date -u -d "@$source_epoch" +%Y-%m-%dT%H:%M:%SZ)

  jq --arg timestamp "$timestamp" '
    . as $m
    | ($m.packages
       | map({key: .id, value: ("pkg:cargo/" + .name + "@" + .version)})
       | from_entries) as $refs
    | ($m.packages | map(select(.name == "tv-client")) | first) as $root
    | ($root
       // {id: "tv-client", name: "tv-client", version: "unknown", license: "GPL-3.0-only"}) as $root
    | {
        bomFormat: "CycloneDX",
        specVersion: "1.5",
        version: 1,
        metadata: {
          timestamp: $timestamp,
          tools: {
            components: [
              {type: "application", name: "cargo-metadata"},
              {type: "application", name: "jq"}
            ]
          },
          component: {
            type: "application",
            "bom-ref": ($refs[$root.id] // "pkg:cargo/tv-client"),
            name: $root.name,
            version: $root.version,
            purl: ($refs[$root.id] // "pkg:cargo/tv-client"),
            licenses: (if $root.license then [{expression: $root.license}] else [] end)
          }
        },
        components: [
          $m.packages[]
          | select(.name != "tv-client")
          | {
              type: "library",
              "bom-ref": $refs[.id],
              name: .name,
              version: .version,
              purl: $refs[.id],
              licenses: (if .license then [{expression: .license}] else [] end)
            }
        ],
        dependencies: [
          $m.resolve.nodes[]?
          | {
              ref: $refs[.id],
              dependsOn: [.deps[]? | .pkg as $pkg | $refs[$pkg] | select(. != null)]
            }
          | select(.ref != null)
        ]
      }
  ' "$input" >"$output/SBOM.cdx.json"

  jq empty "$output/SBOM.cdx.json"
  chmod 0644 "$output/SBOM.cdx.json"
  {
    printf 'tv-client third-party notices\n\n'
    printf 'tv-client itself is licensed under GPL-3.0-only.\n'
    printf 'The full license text is available at https://www.gnu.org/licenses/gpl-3.0.txt.\n\n'
    printf 'Rust crates in the locked dependency graph:\n\n'
    jq -r '.packages | sort_by(.name, .version) | .[] | "\(.name) \(.version)  \(.license // "license not declared")"' "$input"
  } >"$output/NOTICES"
  chmod 0644 "$output/NOTICES"
}

if [[ $mode == "inventory" ]]; then
  generate_inventory "$metadata" "$inventory_out"
  exit 0
fi

if [[ -z $version ]]; then
  version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml" | head -n1)
fi
validate_version "$version"

for binary in tv-client tv-native-player; do
  [[ -f $bin_dir/$binary ]] || { echo "error: missing release binary: $bin_dir/$binary" >&2; exit 1; }
  [[ -x $bin_dir/$binary ]] || { echo "error: release binary is not executable: $bin_dir/$binary" >&2; exit 1; }
done

command -v gzip >/dev/null 2>&1 || { echo "error: gzip is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "error: tar is required" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

cp "$bin_dir/tv-client" "$bin_dir/tv-native-player" "$work/"
chmod 0755 "$work/tv-client" "$work/tv-native-player"
if [[ -z $metadata ]]; then
  command -v cargo >/dev/null 2>&1 || { echo "error: cargo is required to generate the SBOM" >&2; exit 1; }
  cargo metadata --locked --format-version 1 \
    --features native-ui,gstreamer-player,player-overlay >"$work/cargo-metadata.json"
  metadata="$work/cargo-metadata.json"
fi
generate_inventory "$metadata" "$work"

name=$(artifact_name "$version")
mkdir -p -- "$out_dir"
cp "$work/NOTICES" "$work/SBOM.cdx.json" "$out_dir/"
chmod 0644 "$out_dir/NOTICES" "$out_dir/SBOM.cdx.json"
tar --sort=name \
  --mtime="@$source_epoch" \
  --owner=0 --group=0 --numeric-owner \
  -cf - -C "$work" tv-client tv-native-player NOTICES SBOM.cdx.json \
  | gzip -n >"$out_dir/$name"

(
  cd -- "$out_dir"
  sha256sum -- "$name" NOTICES SBOM.cdx.json >SHA256SUMS
)
printf '%s%s\n' "wrote " "$out_dir/$name"
