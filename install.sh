#!/bin/zsh
# Installs monitor-switch on both Macs; run it on either one.
#
#   ./install.sh                     check and set up what is missing, then install on both Macs
#   ./install.sh check               only check and set up the prerequisites
#   ./install.sh uninstall [--purge] remove from both Macs (--purge: with config, keys and logs)
#
# It works out whether this Mac is the MacBook or the Mac mini, installs here
# and, over ssh, on the other Mac with the opposite role. ssh is used only
# while installing; afterwards the two agents talk over HTTP on the home
# network (port 47811), signing every call with a shared secret that this
# script creates and puts on both Macs. Before installing it checks
# everything it needs and fixes what it can, asking first: Xcode command line
# tools and Rust (source checkout only), ssh to the other Mac, and install.conf
# (written from what it finds when missing). install.conf names both Macs, so
# the same file works from either; it is the single source of the settings:
# every install rewrites ~/.monitor-switch/config on both Macs from it (the
# previous one is kept as config.bak).
#
# Next to ready binaries in bin/ (the release archive, which get.sh
# downloads) it installs those; in a source checkout it builds them first.
set -eu -o pipefail
setopt extended_glob
umask 077
cd "${0:A:h}"
# Ready universal binaries from the release archive, or a source checkout.
if [[ -x bin/monitor-switch && -x bin/monitor-switch-hotkey ]]; then
  binaries=bin
else
  binaries=target/release
fi

if [[ -t 1 ]]; then
  bold=$'\e[1m' green=$'\e[32m' yellow=$'\e[33m' red=$'\e[31m' plain=$'\e[0m'
else
  bold= green= yellow= red= plain=
fi
step() { print -r -- "${bold}==> $*${plain}"; }
ok() { print -r -- "  ${green}✓${plain} $*"; }
warn() { print -r -- "  ${yellow}!${plain} $*"; }
die() { print -r -- "  ${red}✗${plain} $*" >&2; exit 1; }
# ask <question> <default>: prints the answer; reads the terminal even when
# stdin is redirected.
ask() {
  local answer
  print -rn -- "  $1${2:+ [$2]}: " > /dev/tty
  read -r answer < /dev/tty || answer=
  print -r -- "${answer:-$2}"
}
confirm() { [[ $(ask "$1 (y/n)" y) == [Yy]* ]]; }

readonly SELF_HOST=$(scutil --get LocalHostName).local
typeset -rA ROLE_NAME=(macbook "the MacBook" mini "the Mac mini")
typeset -rA ROLE_KEY=(macbook MACBOOK mini MINI)
typeset -rA ROLE_TITLE=(macbook MacBook mini "Mac mini")
other_role() { [[ $1 == macbook ]] && print mini || print macbook; }

# ---------------------------------------------------------------- install.conf

typeset -A conf
# install.conf is read as KEY=value lines, never run as shell code.
load_conf() {
  conf=()
  local line key value
  while IFS= read -r line || [[ -n $line ]]; do
    [[ $line == (#s)[[:space:]]#(\#*|)(#e) ]] && continue
    if [[ $line != (#b)(#s)(MINI|MINI_FROM|MACBOOK|MACBOOK_FROM|DEVICES|HDMI_INPUT)=(*)(#e) ]]; then
      die "install.conf: cannot read: $line"
    fi
    key=${match[1]} value=${match[2]}
    [[ $value == (#b)(#s)\"(*)\"(#e) || $value == (#b)(#s)\'(*)\'(#e) ]] && value=${match[1]}
    conf[$key]=$value
  done < install.conf
}

# Every value ends up in an ssh command line, a config file or
# authorized_keys: only plain characters are accepted.
check() { # name pattern description
  [[ ${conf[$1]:-} == (#s)${~2}(#e) ]] || die "install.conf: $1 must be $3"
}
readonly DESTINATION='[A-Za-z0-9._-]##@[A-Za-z0-9.:_-]##'

# Which Mac this is: the one install.conf names by this Mac's host name, else
# the role already installed here, else the model ("MacBook…" or not).
detect_role() {
  local key role
  for role in macbook mini; do
    key=${ROLE_KEY[$role]}
    [[ ${(L)${conf[$key]:-}#*@} == ${(L)SELF_HOST} ]] && { print $role; return; }
  done
  if [[ -f $HOME/.monitor-switch/config ]]; then
    role=$(sed -n 's/^role *= *\([a-z]*\).*/\1/p' "$HOME/.monitor-switch/config" | head -1)
    [[ $role == (macbook|mini) ]] && { print $role; return; }
  fi
  case $(system_profiler SPHardwareDataType 2>/dev/null | sed -n 's/.*Model Name: //p') in
    MacBook*) print macbook ;;
    "Mac mini") print mini ;;
    *) [[ $(ask "Is this Mac the MacBook or the Mac mini? (macbook/mini)" "") == mini ]] && print mini || print macbook ;;
  esac
}

check_conf() {
  local here=${ROLE_KEY[$ROLE]} there=${ROLE_KEY[$PEER_ROLE]}
  conf[$here]=${conf[$here]:-"$USER@$SELF_HOST"}
  # A .local name for this Mac's role that is not this Mac means the roles
  # in install.conf are the wrong way round.
  local here_host=${(L)${conf[$here]}#*@}
  [[ $here_host != *.local || $here_host == ${(L)SELF_HOST} ]] \
    || die "install.conf: $here=${conf[$here]}, but this Mac is ${ROLE_NAME[$ROLE]} ($SELF_HOST): fix MINI/MACBOOK or delete install.conf"
  conf[HDMI_INPUT]=${conf[HDMI_INPUT]:-18}
  check $there "$DESTINATION" "user@host of ${ROLE_NAME[$PEER_ROLE]}"
  check $here "$DESTINATION" "user@host of ${ROLE_NAME[$ROLE]}"
  check DEVICES '[0-9A-Fa-f:, -]##' "Bluetooth addresses separated by commas"
  check HDMI_INPUT '[0-9]##' "a number"
  [[ ${(L)${conf[$there]}#*@} != ${(L)SELF_HOST} ]] \
    || die "install.conf: $there is this Mac ($SELF_HOST), but this Mac is ${ROLE_NAME[$ROLE]}: fix MINI/MACBOOK"
  PEER=${conf[$there]}
}

# Paired Bluetooth keyboards and mice, keyboards first, as
# "address<TAB>name<TAB>connected" lines.
bluetooth_devices() {
  system_profiler SPBluetoothDataType -json 2>/dev/null | osascript -l JavaScript -e '
    function run() {
      const input = $.NSFileHandle.fileHandleWithStandardInput.readDataToEndOfFile;
      const text = $.NSString.alloc.initWithDataEncoding(input, $.NSUTF8StringEncoding).js;
      const out = [];
      for (const controller of JSON.parse(text).SPBluetoothDataType || []) {
        for (const [list, connected] of [["device_connected", "yes"], ["device_not_connected", "no"]]) {
          for (const entry of controller[list] || []) {
            for (const [name, d] of Object.entries(entry)) {
              const type = d.device_minorType || "";
              const hid = /HID/.test(d.device_services || "");
              const kind = /keyboard/i.test(type + name) ? 0 : /mouse|trackpad/i.test(type + name) ? 1 : -1;
              if (kind < 0 && !hid) continue;
              out.push([kind < 0 ? 2 : kind, d.device_address, name.replace(/\t/g, " "), connected]);
            }
          }
        }
      }
      out.sort((a, b) => a[0] - b[0]);
      return out.map(r => r.slice(1).join("\t")).join("\n");
    }' 2>/dev/null
}

# Other Macs on this network that accept ssh, as host names.
ssh_hosts() {
  local name
  local -a names
  names=(${(f)"$( (dns-sd -B _ssh._tcp local. & pid=$!; sleep 2; kill $pid) 2>/dev/null \
    | awk '$2 == "Add" {sub(/.*_ssh\._tcp\. +/, ""); print}' | sort -u)"})
  for name in $names; do
    ( dns-sd -L "$name" _ssh._tcp local. & pid=$!; sleep 1; kill $pid ) 2>/dev/null \
      | sed -n 's/.*can be reached at \([^:]*\)\.:.*/\1/p' | head -1
  done | grep -viF -- "$SELF_HOST" | sort -u
}

write_conf() {
  local peer_name=${ROLE_NAME[$PEER_ROLE]}
  step "install.conf is missing: writing it from what this Mac sees"
  local -a hosts
  hosts=(${(f)"$(ssh_hosts)"})
  local peer_host=
  if (( $#hosts )); then
    print -r -- "  Macs on this network with Remote Login on:"
    local i
    for i in {1..$#hosts}; do print -r -- "    $i) ${hosts[i]}"; done
    local pick=$(ask "Which one is $peer_name? Number, or type a host name" 1)
    [[ $pick == <1-> && $pick -le $#hosts ]] && peer_host=${hosts[pick]} || peer_host=$pick
  else
    warn "no other Mac with Remote Login found on this network"
    print -r -- "  On $peer_name turn on System Settings → General → Sharing → Remote Login,"
    print -r -- "  and see its name there (\"…local\")."
    peer_host=$(ask "Host name of $peer_name (e.g. Johns-Mac.local)" "")
  fi
  [[ -n $peer_host ]] || die "no host name given for $peer_name"
  local peer_user=$(ask "User name on $peer_name" "$USER")

  local -a devices lines
  lines=(${(f)"$(bluetooth_devices)"})
  (( $#lines )) || die "no paired Bluetooth keyboard or mouse found: pair them with this Mac first"
  print -r -- "  Bluetooth keyboards and mice paired with this Mac:"
  local i default=
  for i in {1..$#lines}; do
    local -a f=("${(@ps:\t:)lines[i]}")
    print -r -- "    $i) ${f[2]}  (${f[1]}${${f[3]:#no}:+, connected})"
    [[ ${f[3]} == yes ]] && default+="$i "
  done
  local picks=$(ask "Which ones to move between the Macs? Numbers" "${default% }")
  # The list shows keyboards first; keep that order whatever order the
  # numbers were typed in, because the agent pairs the keyboard first.
  for i in ${(nu)${(z)picks}}; do
    [[ $i == <1-> && $i -le $#lines ]] || die "no device number $i"
    devices+=(${${(@ps:\t:)lines[i]}[1]})
  done
  (( $#devices )) || die "no devices chosen"

  cat > install.conf <<CONF
# Written by install.sh; see install.conf.example for what each line means.
${ROLE_KEY[$ROLE]}=$USER@$SELF_HOST
${ROLE_KEY[$PEER_ROLE]}=$peer_user@$peer_host
DEVICES="${(j:, :)devices}"
HDMI_INPUT=18
CONF
  ok "install.conf written (edit it and run install.sh again to change anything)"
}

# ---------------------------------------------------------------- checks

ssh_peer() { ssh -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes -- "$PEER" "$@"; }

check_tools() {
  step "This Mac: ${ROLE_NAME[$ROLE]} ($SELF_HOST)"
  if [[ $binaries == bin ]]; then
    ok "ready binaries ($(cat VERSION 2>/dev/null || echo release), $(lipo -archs bin/monitor-switch))"
    return
  fi
  xcode-select -p >/dev/null 2>&1 || {
    xcode-select --install 2>/dev/null || true
    die "Xcode command line tools are missing: finish the installer that just opened, then run install.sh again"
  }
  ok "Xcode command line tools"
  [[ -x $HOME/.cargo/bin/cargo ]] && path=("$HOME/.cargo/bin" $path)
  if ! command -v cargo >/dev/null; then
    warn "Rust is not installed"
    confirm "Install Rust with rustup (https://rustup.rs) now?" || die "Rust is needed to build: install it and run install.sh again"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
    path=("$HOME/.cargo/bin" $path)
  fi
  ok "Rust ($(cargo --version))"
}

# Only the other Mac needs Remote Login, and only while installing.
check_remote_login() {
  nc -z -G 2 127.0.0.1 22 >/dev/null 2>&1 && { ok "Remote Login is on"; return; }
  warn "Remote Login is off; ${ROLE_NAME[$PEER_ROLE]} needs it to reach this Mac"
  if confirm "Turn it on now (asks for your password)?"; then
    sudo launchctl enable system/com.openssh.sshd || true
    sudo launchctl bootstrap system /System/Library/LaunchDaemons/ssh.plist 2>/dev/null || true
  fi
  if ! nc -z -G 2 127.0.0.1 22 >/dev/null 2>&1; then
    open "x-apple.systempreferences:com.apple.Sharing-Settings.extension" 2>/dev/null || true
    ask "Turn on Remote Login in the Settings window that opened, then press Enter" "" >/dev/null
    nc -z -G 2 127.0.0.1 22 >/dev/null 2>&1 || die "Remote Login is still off"
  fi
  ok "Remote Login is on"
}

# This Mac logs in to the other one with the user's own key, without a password.
check_ssh_to_peer() {
  local peer_name=${ROLE_NAME[$PEER_ROLE]} host=${PEER#*@}
  step "Other Mac: $peer_name ($PEER)"
  if ! ping -c 1 -t 3 "$host" >/dev/null 2>&1 && ! nc -z -G 3 "$host" 22 >/dev/null 2>&1; then
    die "$host does not answer: is $peer_name on, on the same network, and is ${ROLE_KEY[$PEER_ROLE]} in install.conf right?"
  fi
  nc -z -G 3 "$host" 22 >/dev/null 2>&1 \
    || die "$host does not accept ssh: on $peer_name turn on System Settings → General → Sharing → Remote Login"
  local error
  if error=$(ssh_peer true 2>&1); then ok "ssh without a password"; return; fi
  [[ $error == *"REMOTE HOST IDENTIFICATION HAS CHANGED"* ]] \
    && die "the host key of $peer_name changed: if you expected that, run 'ssh-keygen -R $host' and run install.sh again"
  local key
  for key in ~/.ssh/id_ed25519.pub ~/.ssh/id_ecdsa.pub ~/.ssh/id_rsa.pub; do [[ -f $key ]] && break; done
  if [[ ! -f $key ]]; then
    key=~/.ssh/id_ed25519.pub
    ssh-keygen -q -t ed25519 -N '' -f ~/.ssh/id_ed25519
    ok "created your ssh key ~/.ssh/id_ed25519"
  fi
  print -r -- "  Copying your ssh key to $peer_name. If ssh shows its key fingerprint,"
  print -r -- "  check it on $peer_name with"
  print -r -- "      ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub"
  print -r -- "  then type yes, and the password of the user there."
  ssh-copy-id -i "$key" -o StrictHostKeyChecking=ask -- "$PEER" < /dev/tty
  error=$(ssh_peer true 2>&1) || die "still cannot log in to $PEER: $error"
  ok "ssh without a password"
}

check_peer() {
  local peer_name=${ROLE_NAME[$PEER_ROLE]} remote_arch remote_role
  remote_arch=$(ssh_peer uname -m) || die "cannot run commands on $PEER"
  if [[ $binaries == bin ]]; then
    [[ " $(lipo -archs bin/monitor-switch) " == *" $remote_arch "* ]] \
      || die "the binaries do not run on $peer_name ($remote_arch)"
    ok "the binaries run on $peer_name ($remote_arch)"
  else
    [[ $remote_arch == $(uname -m) ]] || die "$peer_name is $remote_arch, this Mac is $(uname -m): both must be the same"
    ok "same CPU architecture ($remote_arch)"
  fi
  # Never install the same role on both Macs.
  remote_role=$(ssh_peer "sed -n 's/^role *= *\\([a-z]*\\).*/\\1/p' ~/.monitor-switch/config 2>/dev/null | head -1") || true
  [[ -z $remote_role || $remote_role == $PEER_ROLE ]] \
    || die "$PEER is installed as ${ROLE_NAME[$remote_role]}, like this Mac: check MINI and MACBOOK in install.conf"
}

# Sets ROLE, PEER_ROLE and, from install.conf, PEER.
setup_roles() {
  [[ -f install.conf ]] && load_conf
  ROLE=$(detect_role)
  PEER_ROLE=$(other_role $ROLE)
}

preflight() {
  check_tools
  if [[ ! -f install.conf ]]; then
    write_conf
    load_conf
  fi
  check_conf
  ok "install.conf"
  check_ssh_to_peer
  check_peer
  [[ -f $HOME/.monitor-switch/config ]] \
    || warn "set the monitor's Input Select to Auto in its own menu (needed to follow the Mac mini)"
}

# ---------------------------------------------------------------- uninstall

if [[ ${1:-} == uninstall ]]; then
  [[ ${2:-} == (|--purge) ]] || { echo "usage: install.sh uninstall [--purge]" >&2; exit 2; }
  [[ -f install.conf ]] || die "install.conf is missing: it needs at least the other Mac's MINI= or MACBOOK= line"
  setup_roles
  check ${ROLE_KEY[$PEER_ROLE]} "$DESTINATION" "user@host of ${ROLE_NAME[$PEER_ROLE]}"
  PEER=${conf[${ROLE_KEY[$PEER_ROLE]}]}
  echo "This Mac:"; zsh scripts/setup-mac.sh uninstall ${2:-}
  echo "${ROLE_TITLE[$PEER_ROLE]}:"
  ssh_peer "zsh -s uninstall ${2:-}" < scripts/setup-mac.sh || echo "  not reachable; run the uninstall again later"
  exit 0
fi
[[ ${1:-} == (|check) ]] || { echo "usage: install.sh [check] | uninstall [--purge]" >&2; exit 2; }

setup_roles
preflight
if [[ ${1:-} == check ]]; then
  step "Everything is ready: run install.sh to install"
  exit 0
fi

# ---------------------------------------------------------------- install

if [[ $binaries == target/release ]]; then
  step "Building"
  cargo test --release --locked --quiet
  cargo build --release --locked
  # Bind the embedded Info.plist to the signature under a stable identifier:
  # without it macOS refuses Bluetooth (and Local Network) without asking.
  codesign -s - -f -i local.monitor-switch target/release/monitor-switch
  codesign -s - -f -i local.monitor-switch-hotkey target/release/monitor-switch-hotkey
fi

stage=$(mktemp -d)
remote=
activated=
cleanup() {
  rm -rf -- "$stage"
  [[ -z $remote ]] || ssh_peer "rm -rf -- '$remote'" 2>/dev/null || true
}
failed() {
  [[ -z $activated ]] || echo "install stopped after starting on: $activated; fix the error above and run install.sh again" >&2
}
trap cleanup EXIT
trap failed ZERR
remote=$(ssh_peer mktemp -d)

# role_config <role>: the config of the Mac with that role.
role_config() {
  if [[ $1 == macbook ]]; then
    printf 'role = macbook\npeer = %s\ndevices = %s\n' "${conf[MINI]#*@}" "${conf[DEVICES]}"
  else
    printf 'role = mini\npeer = %s\ndevices = %s\nhdmi_input = %s\n' \
      "${conf[MACBOOK]#*@}" "${conf[DEVICES]}" "${conf[HDMI_INPUT]}"
  fi
}

cp "$binaries/monitor-switch" "$binaries/monitor-switch-hotkey" launchd/*.plist scripts/setup-mac.sh "$stage/"
# The secret both agents sign their calls with: kept from an earlier
# install on this Mac, else new.
if [[ -s $HOME/.monitor-switch/peer_secret ]]; then
  cp "$HOME/.monitor-switch/peer_secret" "$stage/peer_secret"
else
  openssl rand -hex 32 > "$stage/peer_secret"
fi

# Copy everything to the other Mac before starting anything on either Mac,
# so a network failure here leaves both Macs as they were.
role_config $PEER_ROLE > "$stage/config"
scp -q -o BatchMode=yes -o StrictHostKeyChecking=yes "$stage"/* "$PEER:$remote/"

here_name=${ROLE_TITLE[$ROLE]} peer_name=${ROLE_TITLE[$PEER_ROLE]}
step "Installing"
# The other Mac first: if the network fails there, this Mac is unchanged and
# both keep the same secret. Installing here needs no network.
echo "$peer_name:"
ssh_peer "zsh '$remote/setup-mac.sh' install '$remote'"
activated="${ROLE_NAME[$PEER_ROLE]}"

echo "$here_name (this Mac):"
role_config $ROLE > "$stage/config"
zsh "$stage/setup-mac.sh" install "$stage"
activated="both Macs"

# Each agent calls the other itself, so macOS may first ask on each Mac
# whether "monitor-switch" may find devices on the local network.
step "Network"
print -r -- "  Click Allow if macOS asks whether \"monitor-switch\" may find devices on"
print -r -- "  your local network (or accept incoming connections), on both Macs."
reach_here() { ~/bin/monitor-switch reach >/dev/null 2>&1; }
reach_there() { ssh_peer '~/bin/monitor-switch reach' >/dev/null 2>&1; }
for tries in {1..10}; do reach_here && break; sleep 3; done
reach_here && ok "this Mac's agent reaches ${ROLE_NAME[$PEER_ROLE]}" \
  || warn "this Mac's agent cannot reach $PEER: allow monitor-switch in System Settings → Privacy & Security → Local Network, then run ~/bin/monitor-switch reach"
for tries in {1..10}; do reach_there && break; sleep 3; done
reach_there && ok "the agent of ${ROLE_NAME[$PEER_ROLE]} reaches this Mac" \
  || warn "${ROLE_NAME[$PEER_ROLE]} cannot reach this Mac: allow monitor-switch under Local Network there, then run ~/bin/monitor-switch reach on it"

# macOS asks once per Mac, after every install that changed the binary,
# whether the agent may use Bluetooth; asking the agent for its status shows
# that question on the Mac's own screen.
step "Bluetooth"
print -r -- "  Click Allow if macOS asks whether \"monitor-switch\" may use Bluetooth,"
print -r -- "  on this Mac and on the screen of ${ROLE_NAME[$PEER_ROLE]}."
~/bin/monitor-switch status >/dev/null 2>&1 || true
ssh_peer '~/bin/monitor-switch status' >/dev/null 2>&1 || true

hotkey=$(<"$HOME/.monitor-switch/hotkey")
step "Installed: press $hotkey on either Mac"
print -r -- "  If a Mac did not ask about Bluetooth yet, run ~/bin/monitor-switch status on it."
