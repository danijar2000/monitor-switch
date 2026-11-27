//! The progress panel shown in the bottom right corner during a switch.
//!
//! The panel is a separate process (`monitor-switch hud`), so drawing never
//! holds up the agent's Bluetooth run loop and a fault in it cannot take the
//! agent down. The agent starts it for one switch and writes one line per
//! change to its standard input, fields separated by tabs:
//!
//! ```text
//! row    <id>  <label>  <busy|ok|fail|warn>  <detail>   add or change a row
//! error  <text>                                         show what went wrong
//! close  <seconds>                                      close after that long
//! ```
//!
//! An empty label keeps the row's label. The row `title` is drawn first, in
//! bold. The panel also closes when its input ends.

use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

pub const BUSY: &str = "busy";
pub const OK: &str = "ok";
pub const FAIL: &str = "fail";
pub const WARN: &str = "warn";

/// The row that names the Mac being switched to.
pub const TITLE: &str = "title";

// ---- the agent's side ------------------------------------------------------

static PANEL: Mutex<Option<Child>> = Mutex::new(None);

/// Set when the switch goes on after the request that started it has been
/// answered, so `done` must leave the panel open.
static KEEP: AtomicBool = AtomicBool::new(false);

fn panel() -> std::sync::MutexGuard<'static, Option<Child>> {
    PANEL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reap(mut child: Child) {
    thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Tabs and line ends would break the line format.
fn clean(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

fn send(line: &str) {
    let mut panel = panel();
    let sent = panel
        .as_mut()
        .and_then(|child| child.stdin.as_mut())
        .map(|stdin| writeln!(stdin, "{line}").and_then(|()| stdin.flush()));
    if matches!(sent, Some(Err(_))) {
        if let Some(child) = panel.take() {
            reap(child);
        }
    }
}

/// Opens a fresh panel with `title` in progress, replacing any panel left
/// from an earlier step of the same switch.
pub fn open(title: &str) {
    KEEP.store(false, Ordering::SeqCst);
    let mut panel = panel();
    if let Some(mut old) = panel.take() {
        let _ = old.kill();
        reap(old);
    }
    let Ok(program) = std::env::current_exe() else {
        return;
    };
    *panel = Command::new(program)
        .arg("hud")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
    drop(panel);
    row(TITLE, title, BUSY, "");
}

/// Adds the row `id`, or changes it; an empty `label` keeps its label.
pub fn row(id: &str, label: &str, state: &str, detail: &str) {
    send(&format!("row\t{}\t{}\t{state}\t{}", clean(id), clean(label), clean(detail)));
}

fn close(seconds: f64) {
    send(&format!("close\t{seconds}"));
    if let Some(child) = panel().take() {
        // Dropping the input lets the panel end on its own after `seconds`.
        reap(child);
    }
}

/// The switch goes on after this request: `done` leaves the panel open.
pub fn keep_open() {
    KEEP.store(true, Ordering::SeqCst);
}

/// The request ended well: the panel closes shortly.
pub fn done() {
    if !KEEP.swap(false, Ordering::SeqCst) {
        close(4.5);
    }
}

/// The request failed: the panel says why and stays for a while. With no
/// panel up yet, one is opened, so every failure is shown in the same place.
pub fn failed(reason: &str) {
    if panel().is_none() {
        open("Switch failed");
    }
    KEEP.store(false, Ordering::SeqCst);
    send(&format!("error\t{}", clean(reason)));
    close(10.0);
}

// ---- the panel process -----------------------------------------------------

use std::ffi::{c_char, c_void, CString};

type Id = *mut c_void;
type Sel = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Insets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    /// On Intel a method that returns a large struct goes through this.
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_after_f(when: u64, queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_time(when: u64, delta: i64) -> u64;
}

fn sel(name: &str) -> Sel {
    let c = CString::new(name).unwrap();
    unsafe { sel_registerName(c.as_ptr()) }
}

fn class(name: &str) -> Id {
    let c = CString::new(name).unwrap();
    unsafe { objc_getClass(c.as_ptr()) }
}

/// `msg!(ReturnType, receiver, "selector:", arg => ArgType, ...)`
macro_rules! msg {
    ($ret:ty, $obj:expr, $sel:expr $(, $arg:expr => $ty:ty)*) => {{
        // SAFETY: every call site names a selector the receiver answers,
        // with the argument and return types that selector has.
        #[allow(unused_unsafe)]
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f($obj, sel($sel) $(, $arg)*)
        }
    }};
}

/// A message that returns an `NSRect`.
fn rect_of(object: Id, selector: &str) -> Rect {
    #[cfg(target_arch = "x86_64")]
    let send = objc_msgSend_stret as unsafe extern "C" fn();
    #[cfg(not(target_arch = "x86_64"))]
    let send = objc_msgSend as unsafe extern "C" fn();
    // SAFETY: `selector` is one that returns an NSRect.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Rect = std::mem::transmute(send);
        f(object, sel(selector))
    }
}

fn nsstring(text: &str) -> Id {
    let c = CString::new(text.replace('\0', " ")).unwrap();
    msg!(Id, class("NSString"), "stringWithUTF8String:", c.as_ptr() => *const c_char)
}

const NIL: Id = std::ptr::null_mut();

/// Width of the panel and the room its texts may take.
const WIDTH: f64 = 300.0;
const TEXT_WIDTH: f64 = WIDTH - 62.0;
const MARGIN: f64 = 16.0;

/// A device still in progress after this long gets a note.
const SLOW: Duration = Duration::from_secs(12);

/// The panel never outlives a switch, whatever happens to the agent.
const LONGEST: Duration = Duration::from_secs(300);

struct Row {
    id: String,
    label: String,
    state: String,
    detail: String,
    since: Instant,
    slow: bool,
}

struct Ui {
    rows: Vec<Row>,
    error: String,
    close_at: Option<Instant>,
    started: Instant,
    /// One `(panel, stack)` per screen; touched on the main thread only.
    panels: Vec<(usize, usize)>,
}

static UI: Mutex<Option<Ui>> = Mutex::new(None);
static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn main_queue() -> *const c_void {
    std::ptr::addr_of!(_dispatch_main_q)
}

/// Runs the panel until it is told to close or its input ends.
pub fn run() {
    *UI.lock().unwrap() = Some(Ui {
        rows: Vec::new(),
        error: String::new(),
        close_at: None,
        started: Instant::now(),
        panels: Vec::new(),
    });
    thread::spawn(|| {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            LINES.lock().unwrap().push(line);
            unsafe { dispatch_async_f(main_queue(), NIL, apply) };
        }
        LINES.lock().unwrap().push("eof".into());
        unsafe { dispatch_async_f(main_queue(), NIL, apply) };
    });
    let app = msg!(Id, class("NSApplication"), "sharedApplication");
    // Accessory: no Dock icon, no menu bar, never takes the focus.
    let _ = msg!(bool, app, "setActivationPolicy:", 1isize => isize);
    tick(NIL);
    msg!((), app, "run");
}

/// Takes in the lines read so far and redraws. Main thread.
extern "C" fn apply(_context: *mut c_void) {
    let lines: Vec<String> = std::mem::take(&mut *LINES.lock().unwrap());
    let mut guard = UI.lock().unwrap();
    let Some(ui) = guard.as_mut() else { return };
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["row", id, label, state, detail] => {
                match ui.rows.iter_mut().find(|row| row.id == *id) {
                    Some(row) => {
                        if !label.is_empty() {
                            row.label = label.to_string();
                        }
                        if row.state != *state {
                            row.since = Instant::now();
                            row.slow = false;
                        }
                        row.state = state.to_string();
                        row.detail = detail.to_string();
                    }
                    None => ui.rows.push(Row {
                        id: id.to_string(),
                        label: label.to_string(),
                        state: state.to_string(),
                        detail: detail.to_string(),
                        since: Instant::now(),
                        slow: false,
                    }),
                }
            }
            ["error", text] => {
                ui.error = text.to_string();
                // Nothing is in progress any more.
                for row in ui.rows.iter_mut().filter(|row| row.state == BUSY) {
                    row.state = FAIL.into();
                }
            }
            ["close", seconds] => {
                let seconds = seconds.parse::<f64>().unwrap_or(2.5).clamp(0.0, 60.0);
                ui.close_at = Some(Instant::now() + Duration::from_secs_f64(seconds));
            }
            ["eof"] => {
                ui.close_at.get_or_insert(Instant::now() + Duration::from_millis(300));
            }
            _ => {}
        }
    }
    draw(ui);
}

/// Comes round four times a second: closes the panel when its time is up,
/// notes devices that take long, and follows screens coming and going (the
/// monitor arrives in the middle of a switch).
extern "C" fn tick(_context: *mut c_void) {
    {
        let mut guard = UI.lock().unwrap();
        if let Some(ui) = guard.as_mut() {
            let now = Instant::now();
            if ui.close_at.is_some_and(|at| now >= at) || ui.started.elapsed() > LONGEST {
                std::process::exit(0);
            }
            let mut changed = false;
            for row in &mut ui.rows {
                if row.state == BUSY && row.id != TITLE && !row.slow && row.since.elapsed() > SLOW {
                    row.slow = true;
                    changed = true;
                }
            }
            let screens = msg!(usize, msg!(Id, class("NSScreen"), "screens"), "count");
            if changed || screens != ui.panels.len() {
                draw(ui);
            }
        }
    }
    unsafe {
        dispatch_after_f(dispatch_time(0, 250_000_000), main_queue(), NIL, tick);
    }
}

fn color(name: &str) -> Id {
    msg!(Id, class("NSColor"), name)
}

fn label(text: &str, size: f64, bold: bool, color_name: &str) -> Id {
    let field = msg!(Id, class("NSTextField"), "wrappingLabelWithString:", nsstring(text) => Id);
    let font = if bold {
        msg!(Id, class("NSFont"), "boldSystemFontOfSize:", size => f64)
    } else {
        msg!(Id, class("NSFont"), "systemFontOfSize:", size => f64)
    };
    msg!((), field, "setFont:", font => Id);
    msg!((), field, "setTextColor:", color(color_name) => Id);
    msg!((), field, "setSelectable:", false => bool);
    msg!((), field, "setPreferredMaxLayoutWidth:", TEXT_WIDTH => f64);
    field
}

/// The mark in front of a row: a spinner while busy, else a symbol.
fn mark(state: &str) -> Id {
    let (symbol, tint) = match state {
        OK => ("checkmark.circle.fill", "systemGreenColor"),
        FAIL => ("xmark.circle.fill", "systemRedColor"),
        WARN => ("exclamationmark.triangle.fill", "systemOrangeColor"),
        _ => {
            let frame = Rect { x: 0.0, y: 0.0, width: 16.0, height: 16.0 };
            let spinner = msg!(Id, msg!(Id, class("NSProgressIndicator"), "alloc"), "initWithFrame:", frame => Rect);
            msg!((), spinner, "setStyle:", 1usize => usize);
            msg!((), spinner, "setControlSize:", 1usize => usize);
            msg!((), spinner, "setIndeterminate:", true => bool);
            msg!((), spinner, "startAnimation:", NIL => Id);
            return spinner;
        }
    };
    let image = msg!(
        Id,
        class("NSImage"),
        "imageWithSystemSymbolName:accessibilityDescription:",
        nsstring(symbol) => Id,
        NIL => Id
    );
    let setup = msg!(
        Id,
        class("NSImageSymbolConfiguration"),
        "configurationWithPointSize:weight:",
        15.0f64 => f64,
        0.0f64 => f64
    );
    let image = msg!(Id, image, "imageWithSymbolConfiguration:", setup => Id);
    let view = msg!(Id, class("NSImageView"), "imageViewWithImage:", image => Id);
    msg!((), view, "setContentTintColor:", color(tint) => Id);
    view
}

fn stack(vertical: bool, spacing: f64, alignment: isize) -> Id {
    let view = msg!(Id, msg!(Id, class("NSStackView"), "alloc"), "init");
    msg!((), view, "setOrientation:", isize::from(vertical) => isize);
    msg!((), view, "setSpacing:", spacing => f64);
    msg!((), view, "setAlignment:", alignment => isize);
    view
}

// NSLayoutAttribute values.
const LEADING: isize = 5;
const CENTER_Y: isize = 10;
const TOP: isize = 3;

fn row_view(row: &Row) -> Id {
    let detail = if row.detail.is_empty() && row.slow {
        "Taking longer than usual…"
    } else {
        row.detail.as_str()
    };
    let line = stack(false, 10.0, if detail.is_empty() { CENTER_Y } else { TOP });
    msg!((), line, "addArrangedSubview:", mark(&row.state) => Id);
    let title = label(&row.label, 13.0, row.id == TITLE, "labelColor");
    if detail.is_empty() {
        msg!((), line, "addArrangedSubview:", title => Id);
    } else {
        let texts = stack(true, 1.0, LEADING);
        msg!((), texts, "addArrangedSubview:", title => Id);
        let tint = if row.state == FAIL { "systemRedColor" } else { "secondaryLabelColor" };
        msg!((), texts, "addArrangedSubview:", label(detail, 11.0, false, tint) => Id);
        msg!((), line, "addArrangedSubview:", texts => Id);
    }
    line
}

/// One rounded, translucent panel that floats over everything and takes no
/// clicks. Returns `(panel, stack)`.
fn new_panel() -> (Id, Id) {
    let frame = Rect { x: 0.0, y: 0.0, width: WIDTH, height: 60.0 };
    // Borderless (0) and non-activating (1 << 7); buffered backing (2).
    let panel = msg!(
        Id,
        msg!(Id, class("NSPanel"), "alloc"),
        "initWithContentRect:styleMask:backing:defer:",
        frame => Rect,
        1usize << 7 => usize,
        2usize => usize,
        false => bool
    );
    msg!((), panel, "setLevel:", 25isize => isize); // NSStatusWindowLevel
    msg!((), panel, "setOpaque:", false => bool);
    msg!((), panel, "setBackgroundColor:", color("clearColor") => Id);
    msg!((), panel, "setHasShadow:", true => bool);
    msg!((), panel, "setIgnoresMouseEvents:", true => bool);
    msg!((), panel, "setHidesOnDeactivate:", false => bool);
    // On every Space, not moved by Mission Control, over full-screen apps.
    msg!((), panel, "setCollectionBehavior:", 1usize | 16 | 256 => usize);

    let effect = msg!(Id, msg!(Id, class("NSVisualEffectView"), "alloc"), "initWithFrame:", frame => Rect);
    msg!((), effect, "setMaterial:", 6isize => isize); // popover
    msg!((), effect, "setBlendingMode:", 0isize => isize); // behind the window
    msg!((), effect, "setState:", 1isize => isize); // always active
    msg!((), effect, "setWantsLayer:", true => bool);
    let layer = msg!(Id, effect, "layer");
    msg!((), layer, "setCornerRadius:", 14.0f64 => f64);
    msg!((), layer, "setMasksToBounds:", true => bool);
    msg!((), panel, "setContentView:", effect => Id);

    let rows = stack(true, 10.0, LEADING);
    let insets = Insets { top: 14.0, left: 16.0, bottom: 14.0, right: 16.0 };
    msg!((), rows, "setEdgeInsets:", insets => Insets);
    msg!((), rows, "setFrame:", frame => Rect);
    msg!((), rows, "setAutoresizingMask:", 18usize => usize); // width and height
    msg!((), effect, "addSubview:", rows => Id);
    (panel, rows)
}

/// Brings the panels in line with the screens and the rows. Main thread.
fn draw(ui: &mut Ui) {
    let screens = msg!(Id, class("NSScreen"), "screens");
    let count = msg!(usize, screens, "count");
    while ui.panels.len() > count {
        let (panel, _) = ui.panels.pop().unwrap();
        msg!((), panel as Id, "close");
    }
    while ui.panels.len() < count {
        let (panel, rows) = new_panel();
        ui.panels.push((panel as usize, rows as usize));
    }
    for (index, &(panel, rows)) in ui.panels.iter().enumerate() {
        let (panel, rows) = (panel as Id, rows as Id);
        let old = msg!(Id, msg!(Id, rows, "arrangedSubviews"), "copy");
        for at in 0..msg!(usize, old, "count") {
            msg!((), msg!(Id, old, "objectAtIndex:", at => usize), "removeFromSuperview");
        }
        msg!((), old, "release");
        // The title first, then the rows as they came.
        for row in ui.rows.iter().filter(|row| row.id == TITLE) {
            msg!((), rows, "addArrangedSubview:", row_view(row) => Id);
        }
        for row in ui.rows.iter().filter(|row| row.id != TITLE) {
            msg!((), rows, "addArrangedSubview:", row_view(row) => Id);
        }
        if !ui.error.is_empty() {
            let text = label(&ui.error, 11.0, false, "systemRedColor");
            msg!((), text, "setPreferredMaxLayoutWidth:", WIDTH - 32.0 => f64);
            msg!((), rows, "addArrangedSubview:", text => Id);
        }
        let fitting = msg!(Size, rows, "fittingSize");
        let height = fitting.height.max(44.0);
        let screen = msg!(Id, screens, "objectAtIndex:", index => usize);
        let visible = rect_of(screen, "visibleFrame");
        let frame = Rect {
            x: visible.x + visible.width - WIDTH - MARGIN,
            y: visible.y + MARGIN,
            width: WIDTH,
            height,
        };
        msg!((), panel, "setFrame:display:", frame => Rect, true => bool);
        msg!((), panel, "invalidateShadow");
        msg!((), panel, "orderFrontRegardless");
    }
}
