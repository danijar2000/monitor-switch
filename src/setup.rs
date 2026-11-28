//! `monitor-switch setup`: sets up this Mac. Run it on both Macs.
//!
//! The first time it pairs the two Macs over the home network (see
//! `pairing`): no Remote Login, password or user name. It finds the
//! keyboard and mouse by itself and asks only when there is a choice, reads
//! the Mac mini's monitor input over DDC, installs the programs into
//! `~/bin` and starts them. Run again, it shows the settings and changes
//! what is asked for; after `brew upgrade` it installs the new version.
//!
//! ```text
//! monitor-switch setup [--role mini|macbook]   set up, or change settings
//! monitor-switch setup uninstall [--purge]     remove from this Mac
//! ```

use crate::config::{self, Config, Role};
use crate::pairing::{self, Device};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const AGENT: &str = "local.monitor-switch";
const HOTKEY: &str = "local.monitor-switch-hotkey";
const PLISTS: [(&str, &str, &str); 2] = [
    (AGENT, "monitor-switch", include_str!("../launchd/local.monitor-switch.plist")),
    (HOTKEY, "monitor-switch-hotkey", include_str!("../launchd/local.monitor-switch-hotkey.plist")),
];

pub fn mac_name(role: Role) -> &'static str {
    match role {
        Role::Mini => "Mac mini",
        Role::MacBook => "MacBook",
    }
}

fn ask(question: &str, default: &str) -> String {
    if default.is_empty() {
        print!("  {question}: ");
    } else {
        print!("  {question} [{default}]: ");
    }
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    let answer = answer.trim();
    if answer.is_empty() { default.to_string() } else { answer.to_string() }
}

fn run_quietly(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn domain() -> String {
    format!("gui/{}", config::uid())
}

/// This Mac's Bonjour name, e.g. `Daniiars-Mac-mini.local`.
fn host_name() -> Result<String, String> {
    let output = Command::new("/usr/sbin/scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .map_err(|error| format!("cannot read this Mac's name: {error}"))?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        return Err("this Mac has no local host name (System Settings → General → Sharing)".into());
    }
    Ok(format!("{name}.local"))
}

/// The role from the model: a MacBook is the MacBook, anything else the Mac
/// mini. `--role` overrides it.
fn detect_role(args: &[&str]) -> Result<Role, String> {
    if let Some(at) = args.iter().position(|arg| *arg == "--role") {
        return match args.get(at + 1) {
            Some(&"mini") => Ok(Role::Mini),
            Some(&"macbook") => Ok(Role::MacBook),
            _ => Err("--role takes mini or macbook".into()),
        };
    }
    // Newer MacBooks report a model id like "Mac15,3", as other Macs do:
    // the model name tells them apart.
    let profile = Command::new("/usr/sbin/system_profiler").arg("SPHardwareDataType").output();
    let text = profile.map(|out| String::from_utf8_lossy(&out.stdout).to_string()).unwrap_or_default();
    let laptop = text.lines().any(|line| line.trim().starts_with("Model Name:") && line.contains("MacBook"));
    Ok(if laptop { Role::MacBook } else { Role::Mini })
}

/// Paired Bluetooth keyboards and mice, keyboards first, with whether each
/// is connected to this Mac now. From system_profiler, which needs no
/// Bluetooth permission.
fn bluetooth_devices() -> Vec<(Device, bool)> {
    const SCRIPT: &str = r#"
function run() {
  const input = $.NSFileHandle.fileHandleWithStandardInput.readDataToEndOfFile;
  const text = $.NSString.alloc.initWithDataEncoding(input, $.NSUTF8StringEncoding).js;
  const out = [];
  for (const controller of JSON.parse(text).SPBluetoothDataType || []) {
    for (const [list, connected] of [["device_connected", "yes"], ["device_not_connected", "no"]]) {
      for (const entry of controller[list] || []) {
        for (const [name, d] of Object.entries(entry)) {
          const type = d.device_minorType || "";
          const kind = /keyboard/i.test(type + name) ? 0 : /mouse|trackpad/i.test(type + name) ? 1 : -1;
          if (kind < 0) continue;
          out.push([kind, d.device_address, name.replace(/[\t\n]/g, " "), connected]);
        }
      }
    }
  }
  out.sort((a, b) => a[0] - b[0]);
  return out.map(r => r.join("\t")).join("\n");
}"#;
    let Ok(profile) = Command::new("/usr/sbin/system_profiler")
        .args(["SPBluetoothDataType", "-json"])
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let Ok(mut script) = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    if let Some(mut input) = script.stdin.take() {
        let _ = input.write_all(&profile.stdout);
    }
    let Ok(output) = script.wait_with_output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [kind, address, name, connected] = fields.as_slice() else { return None };
            let device = Device { address: crate::bt::normalize(address)?, kind: kind.parse().ok()?, name: name.to_string() };
            Some((device, *connected == "yes"))
        })
        .collect()
}

/// The keyboards and mice to hand over, from those connected here. Asks
/// only when there is more than one keyboard or more than one mouse.
fn choose_devices() -> Vec<Device> {
    let all = bluetooth_devices();
    let connected: Vec<Device> = all.iter().filter(|(_, on)| *on).map(|(device, _)| device.clone()).collect();
    let keyboards = connected.iter().filter(|device| device.kind == 0).count();
    let mice = connected.len() - keyboards;
    if connected.is_empty() {
        println!("  No Bluetooth keyboard or mouse is connected to this Mac: the other Mac brings them.");
        return Vec::new();
    }
    if keyboards <= 1 && mice <= 1 {
        for device in &connected {
            println!("  Found: {}", device.name);
        }
        return connected;
    }
    println!("  Bluetooth keyboards and mice on this Mac:");
    for (index, device) in connected.iter().enumerate() {
        println!("    {}) {}", index + 1, device.name);
    }
    let default: Vec<String> = (1..=connected.len()).map(|n| n.to_string()).collect();
    let picks = ask("Which ones move between the Macs? Numbers", &default.join(" "));
    let mut chosen: Vec<usize> =
        picks.split([' ', ',']).filter_map(|pick| pick.parse::<usize>().ok()).filter(|n| (1..=connected.len()).contains(n)).collect();
    chosen.sort_unstable();
    chosen.dedup();
    chosen.into_iter().map(|n| connected[n - 1].clone()).collect()
}

/// Both Macs' devices together, keyboards first, each once.
fn merge(mine: &[Device], theirs: &[Device]) -> Vec<Device> {
    let mut all: Vec<Device> = Vec::new();
    for device in mine.iter().chain(theirs) {
        if !all.iter().any(|known| known.address == device.address) {
            all.push(device.clone());
        }
    }
    all.sort_by_key(|device| device.kind);
    all
}

/// Mac mini only: the input the monitor shows now, which is this Mac's,
/// since the user is at it.
fn monitor_input(old: Option<u16>) -> u16 {
    let read = crate::ddc::Display::external().and_then(|display| match display.get(0x60) {
        crate::ddc::Reply::Value { current } => Some(current),
        _ => None,
    });
    match read {
        Some(value) => {
            println!("  Monitor: this Mac is on input {value}");
            value
        }
        None => {
            let value = old.unwrap_or(18);
            println!("  Monitor: cannot read its input over DDC; using {value}");
            value
        }
    }
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot protect {}: {error}", path.display()))
}

fn state_dir() -> Result<PathBuf, String> {
    let dir = config::dir();
    std::fs::create_dir_all(&dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot protect {}: {error}", dir.display()))?;
    Ok(dir)
}

/// Copies the programs next to this one into `~/bin`, unless they are the
/// same: macOS asks for the Bluetooth permission again for a changed file.
/// Returns whether anything changed.
fn install_programs() -> Result<bool, String> {
    // Left by versions up to 0.0.9, which installed over ssh.
    let _ = std::fs::remove_dir_all(config::dir().join("installer"));
    let source = std::env::current_exe()
        .and_then(|program| program.canonicalize())
        .map_err(|error| format!("cannot find this program: {error}"))?;
    let from = source.parent().ok_or("this program has no folder")?;
    let bin = config::home().join("bin");
    std::fs::create_dir_all(&bin).map_err(|error| format!("cannot create {}: {error}", bin.display()))?;
    let mut changed = false;
    for (label, name, _) in PLISTS {
        let (new, old) = (from.join(name), bin.join(name));
        if !new.is_file() {
            return Err(format!("{} is missing next to this program", new.display()));
        }
        let same_file = old.canonicalize().is_ok_and(|old| old == new);
        if same_file || std::fs::read(&new).ok() == std::fs::read(&old).ok() {
            continue;
        }
        // Into place by renaming, so a running agent keeps its old file.
        let staged = bin.join(format!(".{name}.new"));
        std::fs::copy(&new, &staged).map_err(|error| format!("cannot copy {name}: {error}"))?;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
        // A build straight from cargo lacks the identifier macOS binds the
        // Bluetooth permission and the embedded Info.plist to.
        let signed = Command::new("/usr/bin/codesign").arg("-dv").arg(&staged).output();
        let identified = signed.is_ok_and(|out| String::from_utf8_lossy(&out.stderr).contains(&format!("Identifier={label}\n")));
        if !identified && !run_quietly("/usr/bin/codesign", &["-s", "-", "-f", "-i", label, staged.to_str().unwrap_or("")]) {
            return Err(format!("cannot sign {name}"));
        }
        std::fs::rename(&staged, &old).map_err(|error| format!("cannot put {name} into {}: {error}", bin.display()))?;
        changed = true;
    }
    Ok(changed)
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn stop(label: &str) {
    let target = format!("{}/{label}", domain());
    run_quietly("/bin/launchctl", &["bootout", &target]);
    for _ in 0..50 {
        if !run_quietly("/bin/launchctl", &["print", &target]) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Writes the launchd jobs and (re)starts the agent and the hotkeys.
fn start_programs() -> Result<(), String> {
    let home = config::home();
    let agents = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents).map_err(|error| format!("cannot create {}: {error}", agents.display()))?;
    let home_text = xml_escape(&home.to_string_lossy());
    for (label, _, template) in PLISTS {
        stop(label);
        let plist = agents.join(format!("{label}.plist"));
        std::fs::write(&plist, template.replace("__HOME__", &home_text))
            .map_err(|error| format!("cannot write {}: {error}", plist.display()))?;
        let mut started = false;
        for _ in 0..5 {
            if run_quietly("/bin/launchctl", &["bootstrap", &domain(), plist.to_str().unwrap_or("")]) {
                started = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        if !started {
            return Err(format!("launchctl could not start {label}"));
        }
    }
    Ok(())
}

fn restart(label: &str) {
    run_quietly("/bin/launchctl", &["kickstart", "-k", &format!("{}/{label}", domain())]);
}

fn hotkeys() -> (String, String) {
    let read = |name: &str, default: &str| {
        std::fs::read_to_string(config::dir().join(name)).map(|text| text.trim().to_string()).ok().filter(|text| !text.is_empty()).unwrap_or(default.into())
    };
    (read("hotkey", "ctrl+enter"), read("hotkey-monitor", "ctrl+shift+enter"))
}

fn save_hotkeys(switch: &str, monitor: &str) -> Result<(), String> {
    let dir = state_dir()?;
    write_private(&dir.join("hotkey"), &format!("{switch}\n"))?;
    write_private(&dir.join("hotkey-monitor"), &format!("{monitor}\n"))
}

fn save_config(config: &Config) -> Result<(), String> {
    let path = state_dir()?.join("config");
    let text = config.to_text();
    if let Ok(old) = std::fs::read_to_string(&path) {
        if old != text {
            let _ = std::fs::write(config::dir().join("config.bak"), old);
        }
    }
    write_private(&path, &text)
}

/// Waits until this Mac's agent reaches the other one.
fn check_reach() {
    print!("  Checking that this Mac reaches the other one");
    let _ = std::io::stdout().flush();
    for _ in 0..20 {
        let lines = crate::client::answer("reach").unwrap_or_default();
        if lines.last().is_some_and(|line| line == "ok") {
            println!(": yes");
            return;
        }
        print!(".");
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    println!();
    println!("  Not yet. Finish setup on the other Mac. If it still fails, allow monitor-switch");
    println!("  in System Settings → Privacy & Security → Local Network, then run");
    println!("  `monitor-switch reach`.");
}

fn finish_install(new_programs: bool) -> Result<(), String> {
    start_programs()?;
    println!("  Started: the agent, the menu bar icon and the hotkeys.");
    check_reach();
    println!();
    if new_programs {
        println!("  If macOS asks whether monitor-switch may use Bluetooth or find devices on your");
        println!("  local network, click Allow.");
    }
    let (switch, monitor) = hotkeys();
    println!("  Done. Press {switch} to switch, {monitor} to move only the monitor.");
    Ok(())
}

/// First setup, or pairing again: pairs and installs.
fn pair_and_install(role: Role, old: Option<&Config>) -> Result<(), String> {
    let host = host_name()?;
    println!("Setting up this Mac as the {}.", mac_name(role));
    let mine = choose_devices();
    let me = pairing::Me { role, host, devices: mine.clone() };
    let peer = pairing::pair(&me)?;
    let devices = merge(&mine, &peer.devices);
    if devices.is_empty() {
        return Err("neither Mac has a Bluetooth keyboard or mouse connected: connect them to one Mac and run setup again on both".into());
    }
    let hdmi_input = if role == Role::Mini { monitor_input(old.map(|config| config.hdmi_input)) } else { 18 };
    let config = Config { role, peer: peer.host.clone(), devices: devices.iter().map(|device| device.address.clone()).collect(), hdmi_input };
    println!("  Paired with {} ({}). Devices: {}.", peer.host, mac_name(peer.role), names(&devices));
    let dir = state_dir()?;
    save_config(&config)?;
    write_private(&dir.join("peer_secret"), &format!("{}\n", peer.secret))?;
    let (switch, monitor) = hotkeys();
    save_hotkeys(&switch, &monitor)?;
    let new_programs = install_programs()?;
    finish_install(new_programs)
}

fn names(devices: &[Device]) -> String {
    devices.iter().map(|device| device.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// The names this Mac knows for the configured addresses.
fn device_names(addresses: &[String]) -> String {
    let known = bluetooth_devices();
    addresses
        .iter()
        .map(|address| {
            known.iter().find(|(device, _)| &device.address == address).map_or(address.clone(), |(device, _)| device.name.clone())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Already set up: shows the settings and changes what is asked for.
fn settings(config: Config) -> Result<(), String> {
    crate::auth::set_role(pairing::role_name(config.role));
    let new_programs = install_programs()?;
    if new_programs {
        println!("Installing this version on this Mac…");
        start_programs()?;
        println!("  Done. Run `monitor-switch setup` on the other Mac too, so both have the same version.");
        println!();
    }
    let reaches = crate::client::answer("reach").unwrap_or_default().last().is_some_and(|line| line == "ok");
    let (switch, monitor) = hotkeys();
    println!("monitor-switch is set up on this Mac, the {}.", mac_name(config.role));
    println!("  Other Mac:  {} ({})", config.peer, if reaches { "answers" } else { "does not answer" });
    println!("  Devices:    {}", device_names(&config.devices));
    println!("  Hotkeys:    {switch} switches, {monitor} moves only the monitor");
    if config.role == Role::Mini {
        println!("  Monitor:    this Mac is on input {}", config.hdmi_input);
    }
    println!();
    println!("  1) Choose the keyboard and mouse again");
    println!("  2) Change the hotkeys");
    println!("  3) Pair with the other Mac again");
    if config.role == Role::Mini {
        println!("  4) Read this Mac's monitor input again");
    }
    println!("  u) Uninstall from this Mac");
    match ask("What to change? Enter for nothing", "").as_str() {
        "" => Ok(()),
        "1" => change_devices(config),
        "2" => change_hotkeys(),
        "3" => pair_and_install(config.role, Some(&config)),
        "4" if config.role == Role::Mini => {
            let config = Config { hdmi_input: monitor_input(Some(config.hdmi_input)), ..config };
            save_config(&config)?;
            restart(AGENT);
            println!("  Saved.");
            Ok(())
        }
        "u" | "U" => uninstall(false),
        other => Err(format!("no choice {other:?}")),
    }
}

fn change_devices(config: Config) -> Result<(), String> {
    let chosen = choose_devices();
    if chosen.is_empty() {
        return Err("connect the keyboard and mouse to this Mac, then choose again".into());
    }
    let addresses: Vec<String> = chosen.iter().map(|device| device.address.clone()).collect();
    // The other Mac must hand over the same devices.
    let command = format!("devices={}", addresses.join(","));
    crate::peer::call(&config.peer, &command, crate::client::timeout("devices"))
        .map_err(|failure| format!("the other Mac did not take the new devices ({}); nothing changed", failure.reason))?;
    save_config(&Config { devices: addresses, ..config })?;
    restart(AGENT);
    println!("  Saved on both Macs: {}.", names(&chosen));
    Ok(())
}

/// Asks for a hotkey until it is one the hotkey program can register.
fn ask_hotkey(question: &str, current: &str, may_be_off: bool) -> String {
    loop {
        let answer = ask(question, current).to_lowercase();
        if may_be_off && answer == "off" {
            return answer;
        }
        match crate::hotkey_spec::parse(&answer) {
            Ok(_) => return answer,
            Err(error) => println!("  {answer}: {error}"),
        }
    }
}

fn change_hotkeys() -> Result<(), String> {
    let (switch, monitor) = hotkeys();
    println!("  For example ctrl+enter or ctrl+alt+cmd+m; off turns the monitor-only hotkey off.");
    let switch = ask_hotkey("Hotkey that switches", &switch, false);
    let monitor = ask_hotkey("Hotkey that moves only the monitor", &monitor, true);
    if switch == monitor {
        return Err("the two hotkeys must differ; nothing changed".into());
    }
    save_hotkeys(&switch, &monitor)?;
    restart(HOTKEY);
    println!("  Saved on this Mac. Set the same on the other Mac with `monitor-switch setup` there.");
    Ok(())
}

/// Removes monitor-switch from this Mac.
fn uninstall(purge: bool) -> Result<(), String> {
    let home = config::home();
    for (label, name, _) in PLISTS {
        stop(label);
        let _ = std::fs::remove_file(home.join(format!("Library/LaunchAgents/{label}.plist")));
        let _ = std::fs::remove_file(home.join("bin").join(name));
    }
    let dir = config::dir();
    let _ = std::fs::remove_file(dir.join("agent.sock"));
    if purge {
        let _ = std::fs::remove_dir_all(&dir);
        run_quietly("/usr/bin/tccutil", &["reset", "Bluetooth", AGENT]);
        println!("Removed from this Mac, with the settings, logs and the Bluetooth permission.");
    } else {
        println!("Removed from this Mac (settings kept in {}; `setup uninstall --purge` deletes them).", dir.display());
    }
    println!("Run the same on the other Mac.");
    Ok(())
}

pub fn run(args: &[&str]) -> Result<(), String> {
    match args {
        ["uninstall"] => uninstall(false),
        ["uninstall", "--purge"] => uninstall(true),
        _ => {
            let unknown = args.iter().enumerate().find(|(at, arg)| {
                !(**arg == "--role" || (*at > 0 && args[at - 1] == "--role"))
            });
            if let Some((_, arg)) = unknown {
                return Err(format!("unknown setup argument {arg:?}"));
            }
            match Config::load() {
                Ok(config) if !args.contains(&"--role") => settings(config),
                Ok(config) => pair_and_install(detect_role(args)?, Some(&config)),
                Err(_) => pair_and_install(detect_role(args)?, None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(address: &str, kind: u8) -> Device {
        Device { address: address.into(), kind, name: address.into() }
    }

    #[test]
    fn merges_keyboards_first_each_once() {
        let mine = [device("m", 1)];
        let theirs = [device("m", 1), device("k", 0)];
        let merged: Vec<String> = merge(&mine, &theirs).into_iter().map(|device| device.address).collect();
        assert_eq!(merged, ["k", "m"]);
    }
}
