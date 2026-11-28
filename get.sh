#!/bin/zsh
# Downloads the latest monitor-switch release and sets up this Mac; no Rust
# or source checkout needed. Run it on both Macs, which then pair by a code:
#
#   curl -fsSL https://github.com/danijar2000/monitor-switch/releases/latest/download/get.sh | zsh
#
# `monitor-switch setup` copies the programs into ~/bin; the download is
# deleted afterwards. Arguments are passed to setup, e.g. "| zsh -s -- uninstall".
set -eu -o pipefail
umask 077

readonly BASE=${MONITOR_SWITCH_URL:-https://github.com/danijar2000/monitor-switch/releases/latest/download}
readonly ARCHIVE=monitor-switch-macos.tar.gz

[[ $(uname -s) == Darwin ]] || { echo "monitor-switch runs on macOS only" >&2; exit 1; }

mkdir -p "$HOME/.monitor-switch"
work=$(mktemp -d "$HOME/.monitor-switch/download.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

echo "==> Downloading the latest release"
curl -fsSL --proto '=https' -o "$work/$ARCHIVE" "$BASE/$ARCHIVE"
curl -fsSL --proto '=https' -o "$work/$ARCHIVE.sha256" "$BASE/$ARCHIVE.sha256"
expected=$(awk '{print $1}' "$work/$ARCHIVE.sha256")
actual=$(shasum -a 256 "$work/$ARCHIVE" | awk '{print $1}')
[[ -n $expected && $expected == "$actual" ]] || { echo "checksum mismatch: the download is damaged, try again" >&2; exit 1; }
tar -xzf "$work/$ARCHIVE" -C "$work"
echo "  $(cat "$work/monitor-switch/VERSION")"

# The terminal answers setup's questions, also when this script came
# through a pipe.
"$work/monitor-switch/bin/monitor-switch" setup "$@" < /dev/tty
