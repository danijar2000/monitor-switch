//! The menu bar icon: shows which Mac holds the monitor, the keyboard and the
//! mouse, and switches them with a click.
//!
//! Like the panel it is a separate process (`monitor-switch menu`) that the
//! agent starts, so AppKit never runs inside the agent. It asks the agent
//! over its socket like any client: `state` every few seconds, which costs
//! no network call, and `overview` (both Macs) when that changed, when the
//! menu is opened, and once a minute. It ends when its input does, which is
//! when the agent goes away.

use crate::appkit::{
    class, class_addMethod, dispatch_async_f, main_queue, nsstring, objc_allocateClassPair, objc_registerClassPair,
    sel, symbol, Id, Sel, NIL,
};
use crate::client;
use std::ffi::{c_void, CString};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// The refusal the agent gives while a switch runs (`agent::BUSY_REPLY`).
const BUSY: &str = "error: busy: a switch is already running";

/// What the menu shows.
#[derive(Clone, Default, PartialEq)]
struct View {
    /// Why there is nothing to show, if so.
    note: String,
    this: String,
    other: String,
    monitor: String,
    /// Label, place and battery percent of each device.
    devices: Vec<(String, String, String)>,
}

static VIEW: Mutex<Option<View>> = Mutex::new(None);

/// Set to have `overview` asked again right away.
static REFRESH: AtomicBool = AtomicBool::new(true);

/// The status item, its menu and the object its items call; made once on
/// the main thread and used only there.
static ITEM: AtomicUsize = AtomicUsize::new(0);
static MENU: AtomicUsize = AtomicUsize::new(0);
static HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Reads the answer to `overview`.
fn view(answer: Result<Vec<String>, String>) -> View {
    let lines = match answer {
        Ok(lines) => lines,
        Err(_) => return View { note: "monitor-switch is not running".into(), ..View::default() },
    };
    match lines.last().map(String::as_str) {
        Some("ok") => {}
        Some(BUSY) => return View { note: "Switching…".into(), ..View::default() },
        _ => return View { note: "monitor-switch does not answer".into(), ..View::default() },
    }
    let mut view = View::default();
    for line in &lines {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["self", name] => view.this = name.to_string(),
            ["other", name] => view.other = name.to_string(),
            ["monitor", place] => view.monitor = place.to_string(),
            ["device", label, place, battery] => {
                view.devices.push((label.to_string(), place.to_string(), battery.to_string()))
            }
            _ => {}
        }
    }
    view
}

/// A place as the menu words it.
fn place(place: &str) -> &str {
    match place {
        "neither" => "not connected",
        "unknown" => "unknown",
        mac => mac,
    }
}

impl View {
    /// The lines above the separator.
    fn facts(&self) -> Vec<String> {
        if !self.note.is_empty() {
            return vec![self.note.clone()];
        }
        let mut facts = vec![format!("Monitor: {}", place(&self.monitor))];
        for (label, at, battery) in &self.devices {
            let charge = if battery.is_empty() { String::new() } else { format!(" · {battery}%") };
            facts.push(format!("{label}: {}{charge}", place(at)));
        }
        facts
    }

    fn devices_here(&self) -> bool {
        self.devices.iter().any(|(_, at, _)| *at == self.this)
    }

    /// The Mac a switch moves everything to.
    fn switch_target(&self) -> &str {
        if self.devices_here() {
            &self.other
        } else {
            &self.this
        }
    }

    /// The Mac `monitor` moves the monitor to, when known.
    fn monitor_target(&self) -> Option<&str> {
        if self.monitor == self.this {
            Some(&self.other)
        } else if self.monitor == self.other {
            Some(&self.this)
        } else {
            None
        }
    }
}

/// Keeps `VIEW` current and has the menu redrawn when it changes.
fn watch() {
    let mut last_state = None;
    let mut last_overview: Option<Instant> = None;
    loop {
        let state = client::answer("state").ok();
        let due = last_overview.map_or(true, |at| at.elapsed() > Duration::from_secs(60));
        if REFRESH.swap(false, Ordering::SeqCst) || state != last_state || due {
            last_state = state;
            last_overview = Some(Instant::now());
            let new = view(client::answer("overview"));
            let mut current = VIEW.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if current.as_ref() != Some(&new) {
                *current = Some(new);
                drop(current);
                unsafe { dispatch_async_f(main_queue(), NIL, redraw) };
            }
        }
        for _ in 0..15 {
            if REFRESH.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

fn add_item(menu: Id, title: &str, action: Option<&str>) {
    let item = msg!(
        Id,
        msg!(Id, class("NSMenuItem"), "alloc"),
        "initWithTitle:action:keyEquivalent:",
        nsstring(title) => Id,
        action.map_or(std::ptr::null(), sel) => Sel,
        nsstring("") => Id
    );
    msg!((), item, "setTarget:", HANDLER.load(Ordering::SeqCst) as Id => Id);
    msg!((), item, "setEnabled:", action.is_some() => bool);
    msg!((), menu, "addItem:", item => Id);
    msg!((), item, "release");
}

/// Rebuilds the menu from `VIEW`. Main thread.
extern "C" fn redraw(_context: *mut c_void) {
    // AppKit empties its own pool only when an event arrives.
    crate::bt::with_pool(rebuild);
}

fn rebuild() {
    let Some(view) = VIEW.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone() else {
        return;
    };
    let menu = MENU.load(Ordering::SeqCst) as Id;
    msg!((), menu, "removeAllItems");
    for fact in view.facts() {
        add_item(menu, &fact, None);
    }
    if view.note.is_empty() {
        msg!((), menu, "addItem:", msg!(Id, class("NSMenuItem"), "separatorItem") => Id);
        add_item(menu, &format!("Switch to {}", view.switch_target()), Some("switchAll:"));
        let only_monitor = match view.monitor_target() {
            Some(target) => format!("Move Only the Monitor to {target}"),
            None => "Move Only the Monitor".into(),
        };
        add_item(menu, &only_monitor, Some("switchMonitor:"));
    }
    // Dimmed while the keyboard and mouse are on the other Mac.
    let button = msg!(Id, ITEM.load(Ordering::SeqCst) as Id, "button");
    let away = view.note.is_empty() && !view.devices_here();
    msg!((), button, "setAppearsDisabled:", away => bool);
}

/// Sends `command` to the agent, which shows the progress in its panel.
fn act(command: &'static str) {
    thread::spawn(move || {
        let _ = client::answer(command);
        REFRESH.store(true, Ordering::SeqCst);
    });
}

extern "C" fn switch_all(_this: Id, _cmd: Sel, _sender: Id) {
    act("switch");
}

extern "C" fn switch_monitor(_this: Id, _cmd: Sel, _sender: Id) {
    act("monitor");
}

extern "C" fn menu_will_open(_this: Id, _cmd: Sel, _menu: Id) {
    REFRESH.store(true, Ordering::SeqCst);
}

/// The object the menu and its items send their messages to.
fn handler() -> Id {
    let name = CString::new("MonitorSwitchMenuHandler").unwrap();
    let types = CString::new("v@:@").unwrap();
    unsafe {
        let new_class = objc_allocateClassPair(class("NSObject"), name.as_ptr(), 0);
        for (selector, function) in [
            ("switchAll:", switch_all as *const c_void),
            ("switchMonitor:", switch_monitor as *const c_void),
            ("menuWillOpen:", menu_will_open as *const c_void),
        ] {
            class_addMethod(new_class, sel(selector), function, types.as_ptr());
        }
        objc_registerClassPair(new_class);
        msg!(Id, msg!(Id, new_class, "alloc"), "init")
    }
}

/// Runs the menu bar icon until its input ends.
pub fn run() {
    thread::spawn(|| {
        let mut rest = Vec::new();
        let _ = std::io::stdin().lock().read_to_end(&mut rest);
        std::process::exit(0);
    });
    let app = msg!(Id, class("NSApplication"), "sharedApplication");
    // Accessory: no Dock icon, never takes the focus.
    let _ = msg!(bool, app, "setActivationPolicy:", 1isize => isize);

    let bar = msg!(Id, class("NSStatusBar"), "systemStatusBar");
    // Variable length (-1); the status bar does not keep the item alive.
    let item = msg!(Id, msg!(Id, bar, "statusItemWithLength:", -1.0f64 => f64), "retain");
    let button = msg!(Id, item, "button");
    let image = symbol("display");
    if image.is_null() {
        msg!((), button, "setTitle:", nsstring("⌨") => Id);
    } else {
        msg!((), button, "setImage:", image => Id);
    }
    msg!((), button, "setToolTip:", nsstring("monitor-switch") => Id);

    let handler = handler();
    let menu = msg!(Id, msg!(Id, class("NSMenu"), "alloc"), "initWithTitle:", nsstring("monitor-switch") => Id);
    msg!((), menu, "setAutoenablesItems:", false => bool);
    msg!((), menu, "setDelegate:", handler => Id);
    msg!((), item, "setMenu:", menu => Id);
    ITEM.store(item as usize, Ordering::SeqCst);
    MENU.store(menu as usize, Ordering::SeqCst);
    HANDLER.store(handler as usize, Ordering::SeqCst);
    *VIEW.lock().unwrap() = Some(View { note: "Starting…".into(), ..View::default() });
    redraw(NIL);

    thread::spawn(watch);
    msg!((), app, "run");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Result<Vec<String>, String> {
        Ok(text.iter().map(|line| line.to_string()).collect())
    }

    #[test]
    fn words_the_menu() {
        let view = view(lines(&[
            "self\tMacBook",
            "other\tMac mini",
            "monitor\tMac mini",
            "device\tKeyboard\tMac mini\t50",
            "device\tMouse\tneither\t",
            "ok",
        ]));
        assert_eq!(view.facts(), ["Monitor: Mac mini", "Keyboard: Mac mini · 50%", "Mouse: not connected"]);
        assert_eq!(view.switch_target(), "MacBook");
        assert_eq!(view.monitor_target(), Some("MacBook"));
    }

    #[test]
    fn says_why_there_is_nothing_to_show() {
        assert_eq!(view(lines(&[BUSY])).facts(), ["Switching…"]);
        assert_eq!(view(Err("no socket".into())).facts(), ["monitor-switch is not running"]);
        assert_eq!(view(lines(&["self\tMacBook"])).facts(), ["monitor-switch does not answer"]);
    }
}
