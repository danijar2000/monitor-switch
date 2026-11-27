#!/bin/zsh
# Downloads the latest monitor-switch release and runs its installer; no Rust
# or source checkout needed. Run it on either Mac:
#
#   curl -fsSL https://github.com/danijar2000/monitor-switch/releases/latest/download/get.sh | zsh
#
# The release (ready universal binaries and the scripts) is unpacked into
# ~/.monitor-switch/installer, where install.conf is kept between updates.
# Arguments are passed to install.sh, e.g. "| zsh -s -- check".
set -eu -o pipefail
umask 077

readonly BASE=${MONITOR_SWITCH_URL:-https://github.com/danijar2000/monitor-switch/releases/latest/download}
readonly ARCHIVE=monitor-switch-macos.tar.gz
readonly DEST=$HOME/.monitor-switch/installer

[[ $(uname -s) == Darwin ]] || { echo "monitor-switch runs on macOS only" >&2; exit 1; }

# On the same disk as $DEST, so moving the new release in is a rename.
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
# Replace the previous release but keep install.conf. The old one is
# moved aside first and deleted only once the new one is in place.
[[ -f $DEST/install.conf ]] && cp "$DEST/install.conf" "$work/monitor-switch/install.conf"
[[ -e $DEST ]] && mv "$DEST" "$work/previous"
if ! mv "$work/monitor-switch" "$DEST"; then
  [[ -e $work/previous ]] && mv "$work/previous" "$DEST"
  echo "cannot put the new release into $DEST" >&2
  exit 1
fi
echo "  $(cat "$DEST/VERSION") unpacked into $DEST"

exec zsh "$DEST/install.sh" "$@"
