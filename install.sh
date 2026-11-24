#!/bin/zsh
# Installs monitor-switch on this MacBook and on the Mac mini.
#
#   ./install.sh                     build, test and install on both Macs
#   ./install.sh uninstall [--purge] remove from both Macs (--purge: with config, keys and logs)
#
# The two Macs' addresses and the devices to hand over are read from
# install.conf (see install.conf.example). install.conf is the single source
# of the settings: every install rewrites ~/.monitor-switch/config on both
# Macs from it (the previous one is kept as config.bak).
set -eu -o pipefail
setopt extended_glob
umask 077
cd "${0:A:h}"

# install.conf is read as KEY=value lines, never run as shell code.
[[ -f install.conf ]] || { echo "install.conf is missing: copy install.conf.example and fill it in" >&2; exit 1; }
typeset -A conf
while IFS= read -r line || [[ -n $line ]]; do
  [[ $line == (#s)[[:space:]]#(\#*|)(#e) ]] && continue
  if [[ $line != (#b)(#s)(MINI|MINI_FROM|MACBOOK|MACBOOK_FROM|DEVICES|HDMI_INPUT)=(*)(#e) ]]; then
    echo "install.conf: cannot read: $line" >&2; exit 1
  fi
  key=${match[1]} value=${match[2]}
  [[ $value == (#b)(#s)\"(*)\"(#e) || $value == (#b)(#s)\'(*)\'(#e) ]] && value=${match[1]}
  conf[$key]=$value
done < install.conf

# Every value ends up in an ssh command line, a config file or
# authorized_keys: only plain characters are accepted.
check() { # name pattern description
  [[ ${conf[$1]:-} == (#s)${~2}(#e) ]] || { echo "install.conf: $1 must be $3" >&2; exit 1; }
}
check MINI '[A-Za-z0-9._-]##@[A-Za-z0-9.:_-]##' "user@host"
MINI=${conf[MINI]}

ssh_mini() { ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes -- "$MINI" "$@"; }

if [[ ${1:-} == uninstall ]]; then
  [[ ${2:-} == (|--purge) ]] || { echo "usage: ./install.sh uninstall [--purge]" >&2; exit 2; }
  echo "MacBook:"; zsh scripts/setup-mac.sh uninstall ${2:-}
  echo "Mac mini:"
  ssh_mini "zsh -s uninstall ${2:-}" < scripts/setup-mac.sh || echo "  not reachable; run the uninstall again later"
  exit 0
fi

conf[MACBOOK]=${conf[MACBOOK]:-"$USER@$(scutil --get LocalHostName).local"}
conf[MACBOOK_FROM]=${conf[MACBOOK_FROM]:-"*"}
conf[HDMI_INPUT]=${conf[HDMI_INPUT]:-18}
check MACBOOK '[A-Za-z0-9._-]##@[A-Za-z0-9.:_-]##' "user@host"
check MINI_FROM '[A-Za-z0-9.:%*,-]##' "a comma-separated list of addresses"
check MACBOOK_FROM '[A-Za-z0-9.:%*,-]##' "a comma-separated list of addresses"
check DEVICES '[0-9A-Fa-f:, -]##' "Bluetooth addresses separated by commas"
check HDMI_INPUT '[0-9]##' "a number"

echo "Checking the Mac mini..."
remote_arch=$(ssh_mini uname -m) || { echo "cannot reach $MINI over ssh (is its host key in known_hosts?)" >&2; exit 1; }
[[ $remote_arch == $(uname -m) ]] || { echo "the Mac mini is $remote_arch, this Mac is $(uname -m)" >&2; exit 1; }

cargo test --release --locked --quiet
cargo build --release --locked

stage=$(mktemp -d)
remote=
activated=
cleanup() {
  rm -rf -- "$stage"
  [[ -z $remote ]] || ssh_mini "rm -rf -- '$remote'" 2>/dev/null || true
}
failed() {
  [[ -z $activated ]] || echo "install stopped after starting on: $activated; fix the error above and run ./install.sh again" >&2
}
trap cleanup EXIT
trap failed ZERR
remote=$(ssh_mini mktemp -d)

cp target/release/monitor-switch target/release/monitor-switch-hotkey launchd/*.plist scripts/setup-mac.sh "$stage/"
printf 'role = macbook\npeer_ssh = %s\ndevices = %s\n' "$MINI" "${conf[DEVICES]}" > "$stage/config.macbook"
printf 'role = mini\npeer_ssh = %s\ndevices = %s\nhdmi_input = %s\n' \
  "${conf[MACBOOK]}" "${conf[DEVICES]}" "${conf[HDMI_INPUT]}" > "$stage/config.mini"

# Copy everything to the Mac mini before starting anything on either Mac, so
# a network failure here leaves both Macs as they were.
cp "$stage/config.mini" "$stage/config"
scp -q -o BatchMode=yes -o StrictHostKeyChecking=yes "$stage"/* "$MINI:$remote/"

echo "MacBook:"
cp "$stage/config.macbook" "$stage/config"
zsh "$stage/setup-mac.sh" install "$stage"
activated="the MacBook"

echo "Mac mini:"
ssh_mini "zsh '$remote/setup-mac.sh' install '$remote'"
activated="both Macs"

# Each agent logs in to the other Mac with its own key, which may run only
# the peer commands (ssh-gate), and only from the addresses in install.conf.
echo "Trust:"
mini_key=$(ssh_mini 'cat ~/.monitor-switch/peer_key.pub')
macbook_key=$(<"$HOME/.monitor-switch/peer_key.pub")
printf '  MacBook:'; zsh "$stage/setup-mac.sh" trust "${conf[MINI_FROM]}" "$mini_key"
printf '  Mac mini:'; ssh_mini "zsh '$remote/setup-mac.sh' trust ${(q)conf[MACBOOK_FROM]} ${(q)macbook_key}"

ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes \
  -o IdentityAgent=none -i "$HOME/.monitor-switch/peer_key" -- "$MINI" '~/bin/monitor-switch ping' >/dev/null \
  && echo "  this MacBook's agent reaches the Mac mini" \
  || echo "  this MacBook's agent cannot reach $MINI: does MACBOOK_FROM list this Mac's address as the Mac mini sees it?"
ssh_mini "ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes \
  -o IdentityAgent=none -i ~/.monitor-switch/peer_key -- '${conf[MACBOOK]}' '~/bin/monitor-switch ping'" >/dev/null \
  && echo "  the Mac mini's agent reaches this MacBook" \
  || echo "  the Mac mini cannot reach ${conf[MACBOOK]}: is Remote Login on, its host key in the Mac mini's known_hosts, and the Mac mini's address in MINI_FROM?"

hotkey=$(<"$HOME/.monitor-switch/hotkey")
cat <<NOTE

Installed. macOS asks once per Mac whether "monitor-switch" may use Bluetooth,
after every install that changed the binary. Run this on each Mac while you
can see its screen, and click Allow:

    ~/bin/monitor-switch status

Then press $hotkey on either Mac (set in ~/.monitor-switch/hotkey).
NOTE
