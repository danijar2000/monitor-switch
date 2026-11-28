# monitor-switch

**Share one monitor, keyboard and mouse between two Macs.** Press
**Control+Enter** and all three move to the other Mac in about 10 seconds.

<p align="center"><a href="docs/demo.mp4"><img src="docs/demo.gif" width="800" alt="Animation: one monitor with the Mac mini on HDMI and the MacBook on DisplayPort; Control+Enter moves the monitor, the keyboard and the mouse to the other Mac"></a><br>
<sub><a href="docs/demo.mp4">Watch the MP4</a></sub></p>

- Both Macs stay plugged into the same monitor, each on its own input.
- The monitor changes its input, and the keyboard and mouse pair with the
  other Mac.
- No KVM switch, no cables to move, nothing to buy.

**You need:** a Mac mini on the monitor's HDMI input and a MacBook on its
DisplayPort input, both on the same home network, and a Bluetooth Magic
Keyboard and Magic Mouse.

## Install

1. Pair the keyboard and mouse with one of the Macs.
2. In the monitor's menu set **Input Select** to **Auto**.
3. On **each** Mac run:

```sh
brew install danijar2000/tap/monitor-switch
monitor-switch setup
```

Without Homebrew, run this on each Mac instead:

```sh
curl -fsSL https://github.com/danijar2000/monitor-switch/releases/latest/download/get.sh | zsh
```

4. The two Macs find each other and show a six-digit code. Check that it is
   the same on both and type **y** on both.

That is all: setup finds the keyboard and mouse and the monitor input by
itself, and asks only when there is a choice. When macOS asks whether
**monitor-switch** may use **Bluetooth** or the **local network**, click
**Allow** on each Mac. macOS asks about Bluetooth again after every update.

| | With Homebrew | Without Homebrew |
|---|---|---|
| **Update** (on both Macs) | `brew upgrade monitor-switch && monitor-switch setup` | run the `curl` line again |
| **Change settings** | `monitor-switch setup` | `~/bin/monitor-switch setup` |
| **Uninstall** (on both Macs) | `monitor-switch setup uninstall && brew uninstall monitor-switch` | `~/bin/monitor-switch setup uninstall` |

Run again, `monitor-switch setup` shows the settings and lets you choose
the keyboard and mouse again (on both Macs at once), change the hotkeys,
pair the Macs again or uninstall. `setup uninstall --purge` also deletes the
settings and logs.

## Use

Press **Control+Enter** on either Mac. A panel in the corner shows the
progress, the charge of the keyboard and mouse, and what went wrong if
something did.

**Control+Shift+Enter** moves only the monitor to the other Mac, and back:
the keyboard and mouse stay where they are.

The icon in the menu bar shows which Mac has the monitor, the keyboard and
the mouse, and does both switches with a click. It is dimmed while the
keyboard and mouse are on the other Mac.

```sh
~/bin/monitor-switch status    # what this Mac holds
~/bin/monitor-switch overview  # where everything is, on both Macs
~/bin/monitor-switch reach     # can this Mac's agent reach the other one?
tail -f ~/.monitor-switch/agent.log
```

Change the hotkeys (on each Mac); `off` in `hotkey-monitor` turns that one
off:

```sh
echo ctrl+alt+cmd+m > ~/.monitor-switch/hotkey
echo ctrl+alt+cmd+n > ~/.monitor-switch/hotkey-monitor
launchctl kickstart -k gui/$(id -u)/local.monitor-switch-hotkey
```

## Troubleshooting

- **"No route to host":** allow **monitor-switch** in System Settings →
  Privacy & Security → Local Network, then run `~/bin/monitor-switch reach`.
- **"refused: bad signature":** the Macs have different secrets: run
  `monitor-switch setup` on both and choose **Pair with the other Mac again**.
- **The mouse stays on the old Mac:** press the hotkey again. If it is on
  neither Mac, connect it by cable for ten seconds.

## How it works

A background agent on each Mac does the Bluetooth work and talks to the
other Mac's agent over HTTP on the home network (port 47811). Every call is
signed with a secret both Macs share. Setup agrees on that secret by
Diffie-Hellman over port 47812, and the code you compare on both Macs
proves nobody stood in between. The Mac giving up the devices unpairs
them, and the other Mac pairs them; anything it fails to get is taken back.
The monitor follows the active signal (Input Select = Auto).

The Mac mini keeps the monitor with the keyboard and mouse. When a Mac puts
its display to sleep, the monitor moves to the other input by itself and
does not come back. So the Mac mini calls the monitor back when it wakes up
holding the devices, and puts its own display to sleep when the monitor
shows it while the devices are on the MacBook. To look at the other Mac on
purpose, use **Control+Shift+Enter**, not the monitor's buttons.
