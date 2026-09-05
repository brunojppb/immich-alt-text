#!/bin/sh
set -eu

REPOSITORY="brunojppb/immich-alt-text"
BINARY_NAME="immich-alt-text"
LATEST_RELEASE_API="https://api.github.com/repos/${REPOSITORY}/releases/latest"

temporary_dir=""
install_tmp=""

die() {
  printf 'immich-alt-text installer: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "Required command '$1' was not found."
}

cleanup() {
  if [ -n "$install_tmp" ]; then
    rm -f "$install_tmp"
  fi
  if [ -n "$temporary_dir" ]; then
    rm -rf "$temporary_dir"
  fi
}

find_asset_url() {
  asset_name=$1
  printf '%s\n' "$release_json" |
    sed -n 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
    awk -v suffix="/$asset_name" '
      substr($0, length($0) - length(suffix) + 1) == suffix {
        matches++
        url = $0
      }
      END {
        if (matches == 1) {
          print url
        } else {
          exit 1
        }
      }
    '
}

trap cleanup EXIT
trap 'exit 1' HUP INT TERM

require_command curl
require_command tar

if command -v sha256sum >/dev/null 2>&1; then
  checksum_command=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  checksum_command=shasum
else
  die "Install sha256sum or shasum to verify the downloaded archive."
fi

os=$(uname -s)
architecture=$(uname -m)

case "$os:$architecture" in
  Linux:x86_64|Linux:amd64)
    target="x86_64-unknown-linux-musl"
    ;;
  Linux:aarch64|Linux:arm64)
    target="aarch64-unknown-linux-musl"
    ;;
  Darwin:aarch64|Darwin:arm64)
    target="aarch64-apple-darwin"
    ;;
  *)
    die "Unsupported platform '$os $architecture'. Use a supported release archive or build from source."
    ;;
esac

if [ -n "${INSTALL_DIR:-}" ]; then
  install_dir=$INSTALL_DIR
elif [ -n "${HOME:-}" ]; then
  install_dir="$HOME/.local/bin"
else
  die "HOME is not set; set INSTALL_DIR to a writable directory."
fi

if [ -e "$install_dir" ] && [ ! -d "$install_dir" ]; then
  die "Install path '$install_dir' exists but is not a directory."
fi
mkdir -p "$install_dir" || die "Could not create install directory '$install_dir'."
[ -w "$install_dir" ] || die "Install directory '$install_dir' is not writable; choose another with INSTALL_DIR."

archive_name="${BINARY_NAME}-${target}.tar.gz"
checksum_name="${archive_name}.sha256"

printf 'Finding the latest %s release...\n' "$BINARY_NAME"
release_json=$(curl -fsSL \
  -H 'Accept: application/vnd.github+json' \
  -H 'X-GitHub-Api-Version: 2022-11-28' \
  "$LATEST_RELEASE_API") || die "Could not resolve the latest GitHub release."

archive_url=$(find_asset_url "$archive_name") || die "Release asset '$archive_name' was not found exactly once."
checksum_url=$(find_asset_url "$checksum_name") || die "Release asset '$checksum_name' was not found exactly once."

case "$archive_url" in
  "https://github.com/${REPOSITORY}/releases/download/"*/"$archive_name") ;;
  *) die "Release returned an unexpected archive URL." ;;
esac
case "$checksum_url" in
  "https://github.com/${REPOSITORY}/releases/download/"*/"$checksum_name") ;;
  *) die "Release returned an unexpected checksum URL." ;;
esac

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/${BINARY_NAME}.XXXXXX") || die "Could not create a temporary directory."
archive_path="$temporary_dir/$archive_name"
checksum_path="$temporary_dir/$checksum_name"

printf 'Downloading %s...\n' "$archive_name"
curl -fsSL "$archive_url" -o "$archive_path" || die "Could not download '$archive_name'."
curl -fsSL "$checksum_url" -o "$checksum_path" || die "Could not download '$checksum_name'."

expected_hash=$(awk 'NR == 1 { print tolower($1); exit }' "$checksum_path")
if [ "${#expected_hash}" -ne 64 ]; then
  die "Downloaded checksum is not a SHA-256 hash."
fi
case "$expected_hash" in
  *[!0-9a-f]*) die "Downloaded checksum is not a SHA-256 hash." ;;
esac

if [ "$checksum_command" = sha256sum ]; then
  actual_hash=$(sha256sum "$archive_path" | awk '{ print tolower($1); exit }')
else
  actual_hash=$(shasum -a 256 "$archive_path" | awk '{ print tolower($1); exit }')
fi
[ "$actual_hash" = "$expected_hash" ] || die "Archive checksum verification failed."

archive_entries=$(tar -tzf "$archive_path") || die "Downloaded archive could not be read."
[ "$archive_entries" = "$BINARY_NAME" ] || die "Archive contents were unexpected; refusing to install."

extract_dir="$temporary_dir/extracted"
mkdir "$extract_dir"
tar -xzf "$archive_path" -C "$extract_dir" || die "Downloaded archive could not be extracted."
extracted_binary="$extract_dir/$BINARY_NAME"
[ -f "$extracted_binary" ] && [ -x "$extracted_binary" ] || die "Archive did not contain an executable '$BINARY_NAME'."

destination="$install_dir/$BINARY_NAME"
[ -d "$destination" ] && die "Install destination '$destination' is a directory."

install_tmp=$(mktemp "$install_dir/.${BINARY_NAME}.XXXXXX") || die "Could not create a file in '$install_dir'."
cp "$extracted_binary" "$install_tmp" || die "Could not copy '$BINARY_NAME' into '$install_dir'."
chmod 755 "$install_tmp" || die "Could not make the installed binary executable."
mv -f "$install_tmp" "$destination" || die "Could not install '$BINARY_NAME' into '$install_dir'."
install_tmp=""

[ -f "$destination" ] && [ -x "$destination" ] || die "Installed '$BINARY_NAME' is not executable."

printf 'Installed %s to %s/%s\n' "$BINARY_NAME" "$install_dir" "$BINARY_NAME"
case ":${PATH:-}:" in
  *":$install_dir:"*) ;;
  *)
    printf '%s is not currently on PATH. Add it with:\n' "$install_dir"
    printf '  export PATH="%s:$PATH"\n' "$install_dir"
    ;;
esac
printf 'Next command: %s\n' "$BINARY_NAME"
