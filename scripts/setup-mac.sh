#!/bin/zsh
# Installs or removes monitor-switch on the Mac it runs on. install.sh runs
# it on both Macs; it is not meant to be run by hand.
#
#   setup-mac.sh install <dir with the binaries, config and launchd plists>
#   setup-mac.sh uninstall [--purge]
set -eu -o pipefail
setopt extended_glob
umask 077

readonly STATE=$HOME/.monitor-switch
readonly DOMAIN="gui/$(id -u)"
readonly KEYS=$HOME/.ssh/authorized_keys
# Marks the authorized_keys entry of the other Mac's agent.
readonly GATE="monitor-switch ssh-gate"
# launchd label -> program and log file
typeset -rA PROGRAM=(local.monitor-switch monitor-switch local.monitor-switch-hotkey monitor-switch-hotkey)
typeset -rA LOG=(local.monitor-switch agent.log local.monitor-switch-hotkey hotkey.log)

stop_agents() {
  local label tries
  for label in ${(k)PROGRAM}; do
    launchctl bootout "$DOMAIN/$label" 2>/dev/null || true
    # bootout returns before the job is gone; bootstrap would fail on it.
    for tries in {1..50}; do launchctl print "$DOMAIN/$label" >/dev/null 2>&1 || break; sleep 0.1; done
  done
}

# Removes the authorized_keys entry that versions before 0.0.5 added for the
# other Mac's agent (it logged in over ssh then).
untrust() {
  [[ -f $KEYS ]] || return 0
  local kept
  kept=$(mktemp "$HOME/.ssh/authorized_keys.XXXXXX")
  grep -vF -- "$GATE" "$KEYS" > "$kept" || true
  chmod 600 "$kept" && mv -f -- "$kept" "$KEYS"
}

uninstall() {
  stop_agents
  local label
  for label in ${(k)PROGRAM}; do
    rm -f "$HOME/Library/LaunchAgents/$label.plist" "$HOME/bin/${PROGRAM[$label]}"
  done
  rm -f "$STATE/agent.sock"
  untrust
  if [[ ${1:-} == --purge ]]; then
    rm -f "$STATE"/config "$STATE"/config.bak "$STATE"/hotkey "$STATE"/hotkey-monitor "$STATE"/peer_input "$STATE"/peer_key "$STATE"/peer_key.pub "$STATE"/peer_secret "$STATE"/*.log(N) "$STATE"/*.log.1(N)
    rmdir "$STATE" 2>/dev/null || true
    tccutil reset Bluetooth local.monitor-switch >/dev/null 2>&1 || true
    echo "  removed, with config, logs and the Bluetooth permission"
  else
    echo "  removed (config, secret and logs kept in $STATE; 'uninstall --purge' deletes them)"
  fi
}

install_from() {
  local src=$1 label plist tries
  mkdir -p "$HOME/bin" "$HOME/Library/LaunchAgents" "$STATE"
  chmod 700 "$STATE"
  chmod 600 "$STATE"/*(.N) 2>/dev/null || true
  stop_agents

  for label in ${(k)PROGRAM}; do
    # An unchanged binary is left alone: macOS ties the Bluetooth permission
    # to the exact file, so replacing it means allowing Bluetooth again.
    local binary=${PROGRAM[$label]}
    cmp -s "$src/$binary" "$HOME/bin/$binary" || install -m 755 "$src/$binary" "$HOME/bin/$binary"
  done

  if [[ -f $STATE/config ]] && ! cmp -s "$src/config" "$STATE/config"; then
    cp "$STATE/config" "$STATE/config.bak"
    echo "  config changed (the previous one is saved as config.bak):"
    diff "$STATE/config.bak" "$src/config" | sed 's/^/    /' || true
  fi
  install -m 600 "$src/config" "$STATE/config"
  [[ -s $STATE/hotkey ]] || echo ctrl+enter > "$STATE/hotkey"
  # The hotkey that moves only the monitor; "off" in the file turns it off.
  [[ -s $STATE/hotkey-monitor ]] || echo ctrl+shift+enter > "$STATE/hotkey-monitor"
  install -m 600 "$src/peer_secret" "$STATE/peer_secret"
  # Since 0.0.5 the agents talk over HTTP; the old ssh entry and key go.
  untrust
  rm -f "$STATE/peer_key" "$STATE/peer_key.pub"

  for label in ${(k)PROGRAM}; do
    plist=$HOME/Library/LaunchAgents/$label.plist
    cp "$src/$label.plist" "$plist"
    # plutil sets the values as data, whatever characters $HOME holds.
    # On an array element -replace inserts; remove the placeholder first.
    plutil -remove ProgramArguments.0 "$plist"
    plutil -insert ProgramArguments.0 -string "$HOME/bin/${PROGRAM[$label]}" "$plist"
    plutil -replace StandardOutPath -string "$STATE/${LOG[$label]}" "$plist"
    plutil -replace StandardErrorPath -string "$STATE/${LOG[$label]}" "$plist"
    for tries in {1..5}; do
      launchctl bootstrap "$DOMAIN" "$plist" 2>/dev/null && break
      (( tries < 5 )) || { echo "  launchctl bootstrap $label failed" >&2; exit 1; }
      sleep 1
    done
  done
  echo "  agent and hotkeys ($(<"$STATE/hotkey"); monitor only: $(<"$STATE/hotkey-monitor")) started"
}

case ${1:-} in
  install) install_from "${2:?usage: setup-mac.sh install <dir>}" ;;
  uninstall) [[ ${2:-} == (|--purge) ]] || { echo "usage: setup-mac.sh uninstall [--purge]" >&2; exit 2; }; uninstall "${2:-}" ;;
  *) echo "usage: setup-mac.sh install <dir> | uninstall [--purge]" >&2; exit 2 ;;
esac
