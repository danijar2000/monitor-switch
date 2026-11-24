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
| ssh between the Macs | The agents call each other with `ping`, `held`, `arrive`, `handover`, `status`. Each agent has its own key (`~/.monitor-switch/peer_key`), which the other Mac accepts only for exactly those commands (`ssh-gate`) and only from the addresses in `install.conf`. |

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

Pair the keyboard and mouse with the MacBook, set the monitor's **Input
Select** to **Auto** in its menu, and turn on **Remote Login** on the Mac mini
(System Settings → General → Sharing). Then on the **MacBook**:

```sh
git clone https://github.com/danijar2000/monitor-switch.git
cd monitor-switch
./install.sh
```

`install.sh` checks everything the install needs and sets up what is missing,
asking first:

| Check | If missing |
|---|---|
| Xcode command line tools | opens their installer |
| Rust | installs it with rustup |
| Remote Login on the MacBook | turns it on (asks for your password) |
| `install.conf` | writes it: finds the Mac mini on the network and the paired keyboard and mouse, and allows the home network (`192.168.31.*,fe80::*`) |
| ssh to the Mac mini without a password | copies your key (asks for the Mac mini's password once) |
| Mac mini's architecture | stops if it differs from the MacBook's |
| MacBook's host key on the Mac mini | adds it, taken from the MacBook itself |

Then it runs the tests, builds, installs the programs to `~/bin`, the config
to `~/.monitor-switch/config` and the launchd agents on both Macs, lets each
agent's key into the other Mac with the restrictions above, and checks that
each agent reaches the other Mac. Last, click **Allow** when macOS asks on
each Mac whether "monitor-switch" may use Bluetooth. Done: press
**Control+Enter**.

`./install.sh check` runs only the checks. To change a setting, edit
`install.conf` (see `install.conf.example`) and run `./install.sh` again.

### Update and uninstall

- **Update:** `git pull && ./install.sh` on the MacBook. `install.conf` is the
  only place for settings: each install rewrites both Macs' configs from it
  (the previous one is kept as `config.bak`). A changed binary asks for
  Bluetooth again.
- **Uninstall:** `./install.sh uninstall` removes the programs, the agents and
  the agents' keys from `authorized_keys` on both Macs; add `--purge` to also
  delete the config, the keys, the logs and the Bluetooth permission. It needs
  only `MINI` in `install.conf`.

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
- **"Permission denied (publickey…)" after the router restarted:** the Mac
  that refuses checks the other's address against `MINI_FROM`/`MACBOOK_FROM`.
  Allow the home network rather than exact addresses
  (`MINI_FROM="192.168.31.*,fe80::*"`) and run `./install.sh` again; or
  reserve each Mac's address on the router and keep exact addresses.

## Development

```sh
cargo test           # config, DDC replies, the agent protocol, the ssh gate, hotkey parsing
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/build.yml`) runs both on macOS, builds universal (Apple silicon + Intel) binaries as the `monitor-switch-macos` artifact, checks Rust 1.80, and attaches the archive to a GitHub release for every `v*` tag;
`rust-toolchain.toml` pins the toolchain for rustup. The crate has no dependencies; IOKit, IOBluetooth (through the Objective-C
runtime), CoreFoundation and Carbon are called directly.
