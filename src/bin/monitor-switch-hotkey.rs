//! A global hotkey that runs `monitor-switch switch`.
//!
//! It is a separate program on purpose: macOS ties the Bluetooth permission
//! to the exact build of `monitor-switch`, so the hotkey can change without
//! the agent having to be allowed again. It needs no permission of its own:
//! Carbon hotkeys work without Accessibility access.
//!
//! The combination is read from `~/.monitor-switch/hotkey`, for example
//! `ctrl+enter` (the default) or `ctrl+alt+cmd+m`. A registered combination
//! is taken away from every application, so `ctrl+enter` no longer reaches
//! any app while this program runs.

#[path = "../shared/home.rs"]
mod home;
#[path = "../shared/logfile.rs"]
mod logfile;
#[path = "../shared/notify.rs"]
mod notify;

use notify::notify;
use std::ffi::c_void;
use home::home;
use std::io::Read;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_HOTKEY: &str = "ctrl+enter";

/// Longest a switch may run before the press is given up on. The agent has
/// its own deadlines; this only keeps the hotkey usable if it never answers.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(330);

// Carbon modifier masks.
const CMD: u32 = 0x0100;
const SHIFT: u32 = 0x0200;
const ALT: u32 = 0x0800;
const CTRL: u32 = 0x1000;

const EVENT_CLASS_KEYBOARD: u32 = 0x6B65_7962; // 'keyb'
const EVENT_HOTKEY_PRESSED: u32 = 5;

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: *mut c_void,
        options: u32,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: Handler,
        type_count: usize,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn RunApplicationEventLoop();
}

/// ANSI virtual key codes for the keys a hotkey may end with.
fn key_code(name: &str) -> Option<u32> {
    const KEYS: &[(&str, u32)] = &[
        ("a", 0), ("s", 1), ("d", 2), ("f", 3), ("h", 4), ("g", 5), ("z", 6), ("x", 7),
        ("c", 8), ("v", 9), ("b", 11), ("q", 12), ("w", 13), ("e", 14), ("r", 15),
        ("y", 16), ("t", 17), ("1", 18), ("2", 19), ("3", 20), ("4", 21), ("6", 22),
        ("5", 23), ("9", 25), ("7", 26), ("8", 28), ("0", 29), ("o", 31), ("u", 32),
        ("i", 34), ("p", 35), ("l", 37), ("j", 38), ("k", 40), ("n", 45), ("m", 46),
        ("f1", 122), ("f2", 120), ("f3", 99), ("f4", 118), ("f5", 96), ("f6", 97),
        ("f7", 98), ("f8", 100), ("f9", 101), ("f10", 109), ("f11", 103), ("f12", 111),
        ("enter", 36), ("return", 36), ("tab", 48), ("space", 49), ("esc", 53),
    ];
    KEYS.iter().find(|(key, _)| *key == name).map(|(_, code)| *code)
}

/// "ctrl+alt+cmd+m" -> (key code, modifier mask)
fn parse(hotkey: &str) -> Result<(u32, u32), String> {
    let mut modifiers = 0;
    let mut key = None;
    for part in hotkey.to_lowercase().split('+').map(str::trim) {
        match part {
            "cmd" | "command" => modifiers |= CMD,
            "shift" => modifiers |= SHIFT,
            "alt" | "opt" | "option" => modifiers |= ALT,
            "ctrl" | "control" => modifiers |= CTRL,
            name => key = Some(key_code(name).ok_or(format!("unknown key: {name}"))?),
        }
    }
    let key = key.ok_or("the hotkey names no key")?;
    if modifiers == 0 {
        return Err("the hotkey needs at least one modifier".into());
    }
    Ok((key, modifiers))
}

/// One switch at a time: a second press while the first still runs is ignored.
static RUNNING: AtomicBool = AtomicBool::new(false);

extern "C" fn pressed(_call: *mut c_void, _event: *mut c_void, _data: *mut c_void) -> i32 {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return 0;
    }
    // The switch takes several seconds; never block the event loop on it.
    thread::spawn(|| {
        match switch() {
            Ok(()) => eprintln!("switch ok"),
            Err(message) => {
                eprintln!("switch failed: {message}");
                // The agent shows its own failures in the panel; a
                // notification is only for when it could not.
                if !agent_answered(&message) {
                    notify(&message);
                }
            }
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
    0
}

/// Whether the failure is the agent's own verdict (shown in its panel), not
/// a failure to reach it or to hear its answer.
fn agent_answered(message: &str) -> bool {
    const NOT_FROM_AGENT: [&str; 6] = [
        "cannot run monitor-switch",
        "the switch did not finish in time",
        "the agent is not running",
        "the agent did not answer in time",
        "the agent closed the connection",
        "the agent stopped before finishing",
    ];
    !message.is_empty() && !NOT_FROM_AGENT.iter().any(|start| message.starts_with(start))
}

/// Runs `monitor-switch switch`, giving up after `SWITCH_TIMEOUT`.
fn switch() -> Result<(), String> {
    let mut child = Command::new(home().join("bin/monitor-switch"))
        .arg("switch")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run monitor-switch: {error}"))?;
    // Read stderr while waiting: a child that filled the pipe would block.
    let mut pipe = child.stderr.take();
    let stderr = thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = &mut pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + SWITCH_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the switch did not finish in time".into());
            }
        }
    };
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(stderr.trim().trim_start_matches("monitor-switch: ").to_string())
    }
}

/// launchd restarts this program only after a failed exit. A bad hotkey or
/// one another app owns will not get better by restarting, so those end with
/// success after saying why.
fn main() -> ExitCode {
    logfile::rotate(&home().join(".monitor-switch/hotkey.log"));
    let configured = std::fs::read_to_string(home().join(".monitor-switch/hotkey"))
        .map(|text| text.trim().to_string())
        .unwrap_or_default();
    let hotkey = if configured.is_empty() { DEFAULT_HOTKEY.to_string() } else { configured };
    let (key, modifiers) = match parse(&hotkey) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("monitor-switch-hotkey: {hotkey}: {error}; not restarting");
            return ExitCode::SUCCESS;
        }
    };
    unsafe {
        let target = GetApplicationEventTarget();
        let types = [EventTypeSpec { class: EVENT_CLASS_KEYBOARD, kind: EVENT_HOTKEY_PRESSED }];
        let status = InstallEventHandler(
            target,
            pressed,
            1,
            types.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if status != 0 {
            eprintln!("monitor-switch-hotkey: cannot install the handler ({status})");
            return ExitCode::FAILURE;
        }
        let mut reference = std::ptr::null_mut();
        let id = EventHotKeyID { signature: 0x6D73_7768, id: 1 }; // 'mswh'
        let status = RegisterEventHotKey(key, modifiers, id, target, 0, &mut reference);
        if status != 0 {
            eprintln!(
                "monitor-switch-hotkey: cannot register {hotkey} ({status}); \
                 another app owns it; not restarting"
            );
            return ExitCode::SUCCESS;
        }
        eprintln!("hotkey {hotkey} registered");
        RunApplicationEventLoop();
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotkeys() {
        assert_eq!(parse("ctrl+enter"), Ok((36, CTRL)));
        assert_eq!(parse("Ctrl + Alt + Cmd + M"), Ok((46, CTRL | ALT | CMD)));
        assert_eq!(parse("shift+f12"), Ok((111, SHIFT)));
    }

    #[test]
    fn rejects_unusable_hotkeys() {
        assert!(parse("enter").is_err());
        assert!(parse("ctrl+shift").is_err());
        assert!(parse("ctrl+hyper").is_err());
    }
}
