//! Global hotkeys: one runs `monitor-switch switch`, one `monitor-switch
//! monitor` (only the monitor moves).
//!
//! It is a separate program on purpose: macOS ties the Bluetooth permission
//! to the exact build of `monitor-switch`, so the hotkey can change without
//! the agent having to be allowed again. It needs no permission of its own:
//! Carbon hotkeys work without Accessibility access.
//!
//! The combinations are read from `~/.monitor-switch/hotkey`, for example
//! `ctrl+enter` (the default) or `ctrl+alt+cmd+m`, and from
//! `~/.monitor-switch/hotkey-monitor` (no hotkey without that file, or when
//! it says `off`). A registered combination is taken away from every
//! application, so `ctrl+enter` no longer reaches any app while this program
//! runs.

#[path = "../shared/home.rs"]
mod home;
#[path = "../shared/logfile.rs"]
mod logfile;
#[path = "../shared/notify.rs"]
mod notify;
#[path = "../shared/hotkey_spec.rs"]
mod hotkey_spec;

use hotkey_spec::parse;
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

const EVENT_CLASS_KEYBOARD: u32 = 0x6B65_7962; // 'keyb'
const EVENT_HOTKEY_PRESSED: u32 = 5;
const PARAM_DIRECT_OBJECT: u32 = 0x2D2D_2D2D; // '----'
const TYPE_HOTKEY_ID: u32 = 0x686B_6964; // 'hkid'

/// Our signature on the hotkeys, and the id of each.
const SIGNATURE: u32 = 0x6D73_7768; // 'mswh'
const SWITCH_ID: u32 = 1;
const MONITOR_ID: u32 = 2;

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
    fn GetEventParameter(
        event: *mut c_void,
        name: u32,
        kind: u32,
        actual_kind: *mut u32,
        size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> i32;
    fn RunApplicationEventLoop();
}

/// One switch at a time: a second press while the first still runs is ignored.
static RUNNING: AtomicBool = AtomicBool::new(false);

extern "C" fn pressed(_call: *mut c_void, event: *mut c_void, _data: *mut c_void) -> i32 {
    let mut id = EventHotKeyID { signature: 0, id: 0 };
    // SAFETY: `id` is a buffer of the size and type asked for.
    let read = unsafe {
        GetEventParameter(
            event,
            PARAM_DIRECT_OBJECT,
            TYPE_HOTKEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(id).cast(),
        )
    };
    let command = match (read, id.signature, id.id) {
        (0, SIGNATURE, MONITOR_ID) => "monitor",
        _ => "switch",
    };
    if RUNNING.swap(true, Ordering::SeqCst) {
        return 0;
    }
    // The switch takes several seconds; never block the event loop on it.
    thread::spawn(move || {
        match switch(command) {
            Ok(()) => eprintln!("{command} ok"),
            Err(message) => {
                eprintln!("{command} failed: {message}");
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

/// Runs `monitor-switch <command>`, giving up after `SWITCH_TIMEOUT`.
fn switch(command: &str) -> Result<(), String> {
    let mut child = Command::new(home().join("bin/monitor-switch"))
        .arg(command)
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

/// Registers the hotkey that moves only the monitor, if one is set. A
/// problem with it is logged and leaves the main hotkey working.
///
/// # Safety
/// `target` is the application event target.
unsafe fn register_monitor_hotkey(target: *mut c_void) {
    let hotkey = std::fs::read_to_string(home().join(".monitor-switch/hotkey-monitor"))
        .map(|text| text.trim().to_string())
        .unwrap_or_default();
    if hotkey.is_empty() || hotkey == "off" {
        return;
    }
    let (key, modifiers) = match parse(&hotkey) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("monitor-switch-hotkey: hotkey-monitor: {hotkey}: {error}");
            return;
        }
    };
    let mut reference = std::ptr::null_mut();
    let id = EventHotKeyID { signature: SIGNATURE, id: MONITOR_ID };
    // SAFETY: the caller passes the application event target.
    let status = unsafe { RegisterEventHotKey(key, modifiers, id, target, 0, &mut reference) };
    if status == 0 {
        eprintln!("hotkey {hotkey} registered (monitor only)");
    } else {
        eprintln!("monitor-switch-hotkey: cannot register {hotkey} ({status}); another app or macOS has it");
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
        let id = EventHotKeyID { signature: SIGNATURE, id: SWITCH_ID };
        let status = RegisterEventHotKey(key, modifiers, id, target, 0, &mut reference);
        if status != 0 {
            eprintln!(
                "monitor-switch-hotkey: cannot register {hotkey} ({status}); \
                 another app or macOS has it; not restarting"
            );
            return ExitCode::SUCCESS;
        }
        eprintln!("hotkey {hotkey} registered");
        register_monitor_hotkey(target);
        RunApplicationEventLoop();
    }
    ExitCode::SUCCESS
}
