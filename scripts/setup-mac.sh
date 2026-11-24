#!/bin/zsh
# Installs or removes monitor-switch on the Mac it runs on. install.sh runs
# it on both Macs; it is not meant to be run by hand.
#
#   setup-mac.sh install <dir with the binaries, config and launchd plists>
#   setup-mac.sh trust <from patterns> <public key>   let the other Mac's agent in
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

# Removes the other Mac's agent key from authorized_keys.
untrust() {
  [[ -f $KEYS ]] || return 0
  local kept
  kept=$(mktemp "$HOME/.ssh/authorized_keys.XXXXXX")
  grep -vF -- "$GATE" "$KEYS" > "$kept" || true
  chmod 600 "$kept" && mv -f -- "$kept" "$KEYS"
}

# Lets the other Mac's agent key run the peer commands, and nothing else,
# from the given addresses only.
trust() {
  local from=$1 key=$2
  # Exactly one ed25519 public key, and plain address patterns: both go into
  # authorized_keys, where a quote or a line break would change its meaning.
  if [[ $key != (#s)ssh-ed25519\ AAAAC3NzaC1lZDI1NTE5[A-Za-z0-9+/]##(=|)(=|)(\ [A-Za-z0-9@._-]#|)(#e) ]]; then
    echo "  not one ed25519 public key; authorized_keys left unchanged" >&2; exit 1
  fi
  if [[ $from != (#s)[A-Za-z0-9.:%*,-]##(#e) ]]; then
    echo "  bad address list '$from'; authorized_keys left unchanged" >&2; exit 1
  fi
  mkdir -p "$HOME/.ssh" && chmod 700 "$HOME/.ssh"
  touch "$KEYS"
  local body=${${(s: :)key}[2]} kept
  kept=$(mktemp "$HOME/.ssh/authorized_keys.XXXXXX")
  grep -vF -e "$GATE" -e "$body" "$KEYS" > "$kept" || true
  print -r -- "command=\"$HOME/bin/$GATE\",restrict,from=\"$from\" $key" >> "$kept"
  chmod 600 "$kept" && mv -f -- "$kept" "$KEYS"
  echo "  the other Mac's agent may run the peer commands from $from"
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
    rm -f "$STATE"/config "$STATE"/config.bak "$STATE"/hotkey "$STATE"/peer_key "$STATE"/peer_key.pub "$STATE"/*.log(N) "$STATE"/*.log.1(N)
    rmdir "$STATE" 2>/dev/null || true
    tccutil reset Bluetooth local.monitor-switch >/dev/null 2>&1 || true
    echo "  removed, with config, logs and the Bluetooth permission"
  else
    echo "  removed (config, agent key and logs kept in $STATE; 'uninstall --purge' deletes them)"
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
  # The agent's own ssh key: the other Mac lets it run the peer commands only.
  [[ -f $STATE/peer_key ]] || ssh-keygen -q -t ed25519 -N '' -C "monitor-switch@$(scutil --get LocalHostName)" -f "$STATE/peer_key"

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
  echo "  agent and hotkey ($(<"$STATE/hotkey")) started"
}

case ${1:-} in
  install) install_from "${2:?usage: setup-mac.sh install <dir>}" ;;
  trust) trust "${2:?usage: setup-mac.sh trust <from> <key>}" "${3:?usage: setup-mac.sh trust <from> <key>}" ;;
  uninstall) [[ ${2:-} == (|--purge) ]] || { echo "usage: setup-mac.sh uninstall [--purge]" >&2; exit 2; }; uninstall "${2:-}" ;;
  *) echo "usage: setup-mac.sh install <dir> | trust <from> <key> | uninstall [--purge]" >&2; exit 2 ;;
esac
