# monitor-switch

One hotkey moves a monitor, a Magic Keyboard and a Magic Mouse between two
Macs (a MacBook on DisplayPort and a Mac mini on HDMI). Press
**Control+Enter**; about 10 seconds later everything is on the other Mac.

## Install

1. Pair the keyboard and mouse with the Mac you install from.
2. In the monitor's menu set **Input Select** to **Auto**.
3. On the **other** Mac turn on **Remote Login** (System Settings → General →
   Sharing). It is needed only while installing.
4. Run in Terminal on either Mac; it installs on both:

```sh
curl -fsSL https://github.com/danijar2000/monitor-switch/releases/latest/download/get.sh | zsh
```

The installer finds the other Mac and the devices, asks what it is unsure
of, and sets up both Macs. When macOS asks whether **monitor-switch** may use
**Bluetooth** and the **local network**, click **Allow** on each Mac.

- **Update:** run the same line again.
- **Change settings:** edit `~/.monitor-switch/installer/install.conf` and
  run `~/.monitor-switch/installer/install.sh`.
- **Uninstall:** `~/.monitor-switch/installer/install.sh uninstall`
  (`--purge` also deletes settings and logs).

## Use

Press **Control+Enter** on either Mac. If something goes wrong, a
notification says what.

```sh
~/bin/monitor-switch status    # what this Mac holds
~/bin/monitor-switch reach     # can this Mac's agent reach the other one?
tail -f ~/.monitor-switch/agent.log
```

Change the hotkey (on each Mac):

```sh
echo ctrl+alt+cmd+m > ~/.monitor-switch/hotkey
launchctl kickstart -k gui/$(id -u)/local.monitor-switch-hotkey
```

## Troubleshooting

- **"No route to host":** allow **monitor-switch** in System Settings →
  Privacy & Security → Local Network, then run `~/bin/monitor-switch reach`.
- **"refused: bad signature":** the Macs have different secrets; run the
  installer again.
- **The mouse stays on the old Mac:** press the hotkey again. If it is on
  neither Mac, connect it by cable for ten seconds.

## How it works

A background agent on each Mac does the Bluetooth work and talks to the
other Mac's agent over HTTP on the home network (port 47811). Every call is
signed with a secret both Macs share. The Mac giving up the devices unpairs
them, and the other Mac pairs them; anything it fails to get is taken back.
The monitor follows the active signal (Input Select = Auto).

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
./install.sh    # build from source and install on both Macs
```
