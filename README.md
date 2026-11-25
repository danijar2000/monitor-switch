# monitor-switch

Moves one monitor, a Magic Keyboard and a Magic Mouse between two Macs with a
single hotkey — no KVM switch, no Universal Control lag: the devices pair with
the active Mac directly.

Built for an MSI PRO MP271A shared by a MacBook (DisplayPort) and a Mac mini
(HDMI). Press **Control+Enter** on either Mac; about 9–12 seconds later the
monitor, keyboard and mouse are on the other one.

## How it works

| Piece | What it does |
|---|---|
| `monitor-switch agent` | Background agent on each Mac (launchd). Does all Bluetooth work: macOS grants Bluetooth only to a process launchd started, never to a shell or ssh session. |
| `monitor-switch-hotkey` | Registers the global hotkey and runs `monitor-switch switch`. A separate program, so changing it never resets the Bluetooth permission. |
| HTTP between the Macs | The agents call each other on port 47811 with `ping`, `held`, `arrive`, `handover`, `status`. Every call is signed with a secret both Macs share (`~/.monitor-switch/peer_secret`, HMAC-SHA256 over the command, the time and a random nonce): a call with a wrong signature, an old time or a reused signature is refused, and only home-network addresses are served. They find each other by their `.local` names, so new addresses from the router do not matter. |

A switch: the Mac holding the devices checks the other answers, unpairs them
and waits until they are free, then asks the other Mac to `arrive` — pair them
and, on the Mac mini, take the monitor. The arriving Mac reports what it got
(`took:`); anything it did not get is taken back, so a device is never left
on neither Mac.

The monitor cannot be switched to HDMI over DDC/CI (its firmware sends every
accepted input command to VGA). With **Input Select = Auto** in the monitor's
menu it follows the signal instead: the Mac mini sends any input command and
the monitor settles on HDMI; when the Mac mini hands over, it puts its display
to sleep and the monitor moves to DisplayPort.

## Install

Pair the keyboard and mouse with the Mac you run it on, set the monitor's **Input
Select** to **Auto** in its menu, and turn on **Remote Login** on the
**other** Mac (System Settings → General → Sharing) — the installer uses ssh
once to install there; switching itself does not need it, so you may turn it
off afterwards. Then run this in Terminal on **either Mac**; it installs on
both:

```sh
curl -fsSL https://github.com/danijar2000/monitor-switch/releases/latest/download/get.sh | zsh
```

It downloads the latest release (ready binaries for Apple silicon and Intel,
checked against its SHA-256), unpacks it into `~/.monitor-switch/installer`
and runs `install.sh` from there. Nothing to build, no Rust needed.

`install.sh` checks everything the install needs and sets up what is missing,
asking first:

| Check | If missing |
|---|---|
| Which Mac this is | from `install.conf`, the role already installed, or the model; asks if unsure |
| `install.conf` | writes it: finds the other Mac on the network and the paired keyboard and mouse |
| ssh to the other Mac without a password | copies your key (asks for its password once) |
| The other Mac | stops if the binaries do not run on it, or if it is installed with the same role |

Then it installs the programs to `~/bin`, the config
to `~/.monitor-switch/config` and the launchd agents on both Macs, puts the same
new secret on both, and checks that each agent reaches the other Mac (click
**Allow** if macOS asks whether "monitor-switch" may use the local network). Last, click **Allow** when macOS asks on
each Mac whether "monitor-switch" may use Bluetooth. Done: press
**Control+Enter**.

To change a setting, edit `~/.monitor-switch/installer/install.conf` (see
`install.conf.example`) and run `~/.monitor-switch/installer/install.sh`
again; `install.sh check` runs only the checks.

### From source

```sh
git clone https://github.com/danijar2000/monitor-switch.git
cd monitor-switch
./install.sh
```

In a source checkout `install.sh` also checks for the Xcode command line
tools and Rust (installing them if you agree), runs the tests and builds the
binaries.

### Update and uninstall

- **Update:** run the same `curl … | zsh` line again; it keeps your
  `install.conf`. Each install rewrites both Macs' configs from it (the
  previous one is kept as `config.bak`). A changed binary asks for Bluetooth
  again.
- **Uninstall:** `~/.monitor-switch/installer/install.sh uninstall` removes
  the programs and the agents on both
  Macs; add `--purge` to also delete the config, the shared secret, the logs and the
  Bluetooth permission. Then `rm -rf ~/.monitor-switch/installer`.

## Use

### Switching

Press **Control+Enter** on the Mac you are using. In about 9–12 seconds the
monitor, keyboard and mouse are on the other Mac; a notification says if
anything went wrong.

- Pressing it on the Mac that has **neither** device pulls them over — handy
  when the keyboard is on the other Mac. You can also run
  `~/bin/monitor-switch switch` from a terminal or over ssh.
- If the devices were left on neither Mac, the hotkey pairs them right there.
- Pressing it on both Macs at once leaves everything where it was.
- Right after a switch the devices need about 14 s before they can leave
  again; a switch pressed sooner waits, then runs.

### Checking what is going on

```sh
~/bin/monitor-switch status     # role, keyboard and mouse state, monitor input; "busy" during a switch
tail -f ~/.monitor-switch/agent.log ~/.monitor-switch/hotkey.log
```

Logs over 512 KB are moved to `agent.log.1` (hourly) and `hotkey.log.1`
(when the hotkey program starts).

### Changing the hotkey

```sh
echo ctrl+alt+cmd+m > ~/.monitor-switch/hotkey
launchctl kickstart -k gui/$(id -u)/local.monitor-switch-hotkey
```

Modifiers: `ctrl`, `alt` (`opt`), `cmd`, `shift`; keys: `a`–`z`, `0`–`9`,
`f1`–`f12`, `enter` (`return`), `tab`, `space`, `esc`. Set it on each Mac. While registered,
the combination no longer reaches apps.

## Limits and troubleshooting

- **Every rebuild asks for Bluetooth again.** macOS ties the permission of an
  unsigned binary to its exact bytes. Signing with a stable Developer ID
  certificate would avoid it (not done: it needs a paid Apple developer
  account); `install.sh` at least skips unchanged binaries.
- **About 1 switch in 10 the mouse does not answer the new pairing.** It is
  taken back by the Mac it came from, so press the hotkey again. If the mouse
  is ever on neither Mac and does not respond, connect it by cable to a Mac for
  ten seconds.
- **The devices stay about 14 s on a Mac after they arrive** before macOS
  will unpair them; a switch pressed sooner waits, then runs.
- **No VPN needed:** the Macs share a monitor, so they share a network too;
  both reach each other by their `.local` names (`MINI` and `MACBOOK` in
  `install.conf`), so a new address from the router does not matter for
  finding them.
- **"cannot connect to … (No route to host)":** macOS has not let the agent
  on the local network. Allow **monitor-switch** in System Settings → Privacy &
  Security → Local Network (on the Mac that shows the error), then run
  `~/bin/monitor-switch reach`, which makes the agent call the other Mac.
- **"refused: bad signature":** `~/.monitor-switch/peer_secret` differs
  between the Macs; run the installer again.
- **The Mac firewall** may ask whether monitor-switch may accept incoming
  connections: allow it, or the other Mac cannot call this one.

## Development

```sh
cargo test           # config, DDC replies, the agent protocol, call signing, hotkey parsing
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/build.yml`) runs both on macOS, builds universal (Apple silicon + Intel) binaries as the `monitor-switch-macos` artifact, checks Rust 1.80, and attaches the archive to a GitHub release for every `v*` tag;
`rust-toolchain.toml` pins the toolchain for rustup. The crate has no dependencies; IOKit, IOBluetooth (through the Objective-C
runtime), CoreFoundation and Carbon are called directly.
