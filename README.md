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

Requirements: Rust 1.80+, both Macs on the same CPU architecture, ssh from the
MacBook to the Mac mini, Remote Login on the MacBook, each Mac's host key
already in the other's `~/.ssh/known_hosts` (connect once by hand and check
the fingerprint; the agents refuse unknown host keys), and the monitor's
Input Select set to Auto.

```sh
cp install.conf.example install.conf   # fill in addresses and devices
./install.sh
```

`install.sh` runs the tests, builds, copies everything to the Mac mini, then
installs the binaries to `~/bin`, the config to `~/.monitor-switch/config` and
the launchd agents on both Macs, and lets each agent's key into the other Mac
with the restrictions above. `install.conf` is the only place for settings:
each install rewrites both configs from it, keeping the previous one as
`config.bak`. Then, on each Mac while
you can see its screen, run `~/bin/monitor-switch status` and allow Bluetooth.

Uninstall with `./install.sh uninstall` (it also removes the agents' keys
from `authorized_keys`); add `--purge` to delete the config, the agent keys,
the logs and the Bluetooth permission too. Uninstall needs only `MINI` in
`install.conf`.

## Use

- **Control+Enter** (or the combination in `~/.monitor-switch/hotkey`) on either Mac — switch. Pressing it on the Mac that has
  neither device pulls them over; if they were left on neither Mac, it pairs
  them right there.
- `~/bin/monitor-switch status` — what the agent sees.
- `~/.monitor-switch/hotkey` — the combination (`ctrl+enter`, `ctrl+alt+cmd+m`,
  `shift+f12`…); run `launchctl kickstart -k gui/$(id -u)/local.monitor-switch-hotkey`
  after changing it. While registered, the combination no longer reaches apps.
- Logs: `~/.monitor-switch/agent.log` and `hotkey.log`. Past 512 KB the
  agent's log is moved to `agent.log.1` hourly, the hotkey's when it starts.
- While a switch runs, `status` answers `busy`; pressing the hotkey on both
  Macs at once leaves everything where it was.

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
