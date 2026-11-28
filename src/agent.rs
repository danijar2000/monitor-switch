//! The background agent, one per Mac, started by launchd in the GUI session.
//!
//! It exists because macOS grants Bluetooth access only to a process that is
//! its own "responsible" application: a command run from ssh or from another
//! app's shell is refused. Everything that touches Bluetooth runs here, on
//! the main thread, which also turns the run loop IOBluetooth calls back on.
//! Clients talk to it over a Unix socket (see `client`).
//!
//! The two Macs call each other over HTTP on the home network (`peer` in the
//! config; see `server` and `peer`), signing every call with a shared secret.
//! Both send the same commands; only the monitor handling differs by role. A hand-over:
//!
//! 1. the Mac holding the devices checks the peer answers (`ping`),
//! 2. unpairs the devices and waits until they are really free,
//! 3. asks the peer to `arrive`: pair them (and on the Mac mini, take the
//!    monitor), which answers with a `took:` line listing what it got,
//! 4. takes back whatever the peer did not get, so no device is left on
//!    neither Mac. A peer that refused as busy got nothing. Without a
//!    `took:` line the peer is asked what it holds (`held`); if it cannot
//!    say, nothing is taken back: grabbing a device the peer holds would set
//!    the Macs fighting.
//!
//! `monitor` moves only the monitor and leaves the devices. Only the Mac mini
//! can tell the monitor where to go, so the MacBook passes the request on.
//! The Mac mini also keeps the monitor with the devices (`keep_monitor`).

use crate::bt;
use crate::client;
use crate::config::{self, Config, Role};
use crate::ddc;
use crate::hud;
use crate::peer;
use std::ffi::c_int;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// How long one device may take to pair and come up on this Mac.
const TAKE_TIMEOUT: Duration = Duration::from_secs(45);

/// A device that just arrived gets this long before it is sent on again.
/// bluetoothd itself defers an unpairing until a fresh connection is about
/// 14 s old, so a switch asked for during that time still waits; this short
/// guard only keeps two presses from overlapping.
const SETTLE_TIME: Duration = Duration::from_secs(3);

/// How soon each device is first contacted after the other Mac let go, by
/// position in the list. A Magic Mouse reached about 3.5 s after it was
/// unpaired took the radio link but answered nothing; the keyboard goes
/// first because it tolerates an early call.
const FIRST_CONTACT: Duration = Duration::from_millis(200);
const CONTACT_SPACING: Duration = Duration::from_millis(6000);

/// How long the devices may take to be fully unpaired here.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(6);

/// How long a release that missed `RELEASE_TIMEOUT` may still take to finish
/// before the devices are taken back.
const LATE_RELEASE: Duration = Duration::from_secs(10);

/// A queued switch older than this is dropped instead of acted on late.
const STALE_REQUEST: Duration = Duration::from_secs(10);

/// Most client connections served at once.
const MAX_CLIENTS: usize = 16;

/// Longest request line a client may send (`devices=` with a few addresses).
const MAX_REQUEST: u64 = 512;

/// Longest a single request may keep the main loop: a hand-over at its worst
/// (see `client::timeout`) plus a margin. Past this the loop is taken to be
/// hung and the agent exits with failure, so launchd starts a fresh one.
const HUNG_AFTER: Duration = Duration::from_secs(400);

/// How long the Mac mini may take over `monitor`: waking the MacBook 5 s,
/// the monitor leaving 20 s, plus a margin.
const MONITOR_MOVE: Duration = Duration::from_secs(45);

/// Prefix of the line that lists the devices a Mac holds after `arrive`.
const TOOK: &str = "took: ";

/// The error a Mac gives when asked to hand over devices it does not hold.
/// The Mac that asked recognises it in the verdict line and pairs the
/// devices itself.
pub const NOT_HOLDING: &str = "none of the devices is connected to this Mac; nothing changed";

/// The refusal while a switch runs.
const BUSY_REPLY: &str = "error: busy: a switch is already running";

/// When the devices last changed hands on this Mac.
static LAST_HANDOVER: Mutex<Option<Instant>> = Mutex::new(None);

/// Set from the moment a switch, hand-over or arrival is accepted until it
/// has run, so that any other one is refused at once instead of queueing.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Set once IOBluetooth has answered its first call. Until the user allows
/// Bluetooth, that call waits on the main thread for the system's question,
/// and nothing that needs the main thread can run: a Mac in that state must
/// not be handed the devices.
static BLUETOOTH_READY: AtomicBool = AtomicBool::new(false);

/// What a Mac answers while it waits for that permission.
const NO_BLUETOOTH: &str =
    "error: not ready: it waits for the Bluetooth permission: click Allow for monitor-switch on its screen";

/// Claims `BUSY`; false if a hand-changing request is already accepted.
fn claim_busy() -> bool {
    BUSY.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Seconds since `EPOCH` when the main loop last came round.
static LOOP_TICK: AtomicU64 = AtomicU64::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn tick() {
    let epoch = EPOCH.get_or_init(Instant::now);
    LOOP_TICK.store(epoch.elapsed().as_secs(), Ordering::SeqCst);
}

/// The configured device addresses, for the queries answered off the main
/// thread.
static DEVICES: OnceLock<Vec<String>> = OnceLock::new();

/// What the menu calls each device, in the order of `DEVICES`. The names
/// come from IOBluetooth, which only the main thread may ask, and which
/// stops to ask the user for permission the first time: so only from the
/// main loop, never before the agent answers its clients. Until then a
/// device is called by its place in the list.
static LABELS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn remember_labels(config: &Config) {
    let labels = config.devices.iter().enumerate().map(|(index, address)| device_label(address, index)).collect();
    *LABELS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = labels;
}

/// This Mac's role and, on the Mac mini, the VCP value of its monitor input.
static ROLE: OnceLock<(Role, u16)> = OnceLock::new();

/// Mac mini only. The devices were handed to the MacBook and it took them.
static AWAY: AtomicBool = AtomicBool::new(false);

/// Mac mini only. `monitor` put the monitor on the Mac that does not hold
/// the devices: it is left there until the next switch.
static PEEK: AtomicBool = AtomicBool::new(false);

/// Mac mini only. The VCP value the monitor reports while it shows the
/// MacBook; 0 until it has been seen once. Kept in `peer_input` between runs.
static PEER_INPUT: AtomicU16 = AtomicU16::new(0);

fn learn_peer_input(value: u16) {
    if value != 0 && PEER_INPUT.swap(value, Ordering::SeqCst) != value {
        let _ = std::fs::write(config::dir().join("peer_input"), format!("{value}\n"));
    }
}

/// The other Mac, for `reach`, which is answered off the main thread.
static PEER: OnceLock<String> = OnceLock::new();

/// Where requests that the agent sends itself go.
static REQUESTS: OnceLock<Sender<Request>> = OnceLock::new();

struct Request {
    command: String,
    /// The client waiting for the answer; `None` for the agent's own requests.
    reply: Option<UnixStream>,
    received: Instant,
}

/// Answers go to the waiting client, if any, and to the agent's log.
struct Output(Option<UnixStream>);

impl Output {
    fn line(&mut self, text: &str) {
        eprintln!("{text}");
        if let Some(stream) = &mut self.0 {
            let _ = writeln!(stream, "{text}");
        }
    }
}

/// Why the agent could not start.
pub enum StartError {
    /// Nothing a restart would change (bad config, unsafe state directory).
    Permanent(String),
    Transient(String),
}

pub fn run() -> Result<(), StartError> {
    crate::logfile::rotate(&config::dir().join("agent.log"));
    let config = Config::load().map_err(StartError::Permanent)?;
    let listener = bind_socket()?;
    let secret = crate::auth::load_secret().map_err(StartError::Permanent)?;
    crate::auth::set_role(match config.role {
        Role::MacBook => "macbook",
        Role::Mini => "mini",
    });
    let network = crate::server::listen().map_err(StartError::Transient)?;
    let (sender, receiver) = mpsc::channel::<Request>();
    let _ = REQUESTS.set(sender.clone());
    let _ = DEVICES.set(config.devices.clone());
    let _ = ROLE.set((config.role, config.hdmi_input));
    if let Ok(text) = std::fs::read_to_string(config::dir().join("peer_input")) {
        PEER_INPUT.store(text.trim().parse().unwrap_or(0), Ordering::SeqCst);
    }

    tick();
    thread::spawn(move || accept_clients(listener, sender));
    thread::spawn(move || crate::server::serve(network, secret));
    let _ = PEER.set(config.peer.clone());
    thread::spawn(watchdog);
    if config.role == Role::MacBook {
        keep_awake();
    }
    thread::spawn(run_menu);
    eprintln!("agent started");

    let log = config::dir().join("agent.log");
    let mut rotated = Instant::now();
    let mut watch = MonitorWatch::new();
    loop {
        tick();
        if !BLUETOOTH_READY.load(Ordering::SeqCst) {
            // The first call makes macOS ask for the permission, if it has
            // to, and returns once the user has answered.
            bt::with_pool(|| {
                let _ = bt::Device::with_address(&config.devices[0]);
                remember_labels(&config);
            });
            BLUETOOTH_READY.store(true, Ordering::SeqCst);
            eprintln!("Bluetooth answers");
        }
        if let Ok(request) = receiver.recv_timeout(Duration::from_millis(200)) {
            bt::with_pool(|| serve(&config, request));
        }
        if config.role == Role::Mini {
            keep_monitor(&config, &mut watch);
        }
        bt::with_pool(|| bt::spin(0.05));
        if rotated.elapsed() > Duration::from_secs(3600) {
            crate::logfile::rotate(&log);
            rotated = Instant::now();
        }
    }
}

/// Ends the agent with failure when the main loop stops coming round, so a
/// hung Bluetooth or DDC call does not leave an agent that answers `ping`
/// but never serves a switch again.
fn watchdog() {
    loop {
        thread::sleep(Duration::from_secs(10));
        let now = EPOCH.get_or_init(Instant::now).elapsed().as_secs();
        if now.saturating_sub(LOOP_TICK.load(Ordering::SeqCst)) > HUNG_AFTER.as_secs() {
            eprintln!("the main loop has not come round for {} s; exiting so launchd restarts the agent", HUNG_AFTER.as_secs());
            std::process::exit(1);
        }
    }
}

/// Creates the socket, private to this user. A live agent already listening
/// keeps it: only a socket nobody answers on is replaced.
fn bind_socket() -> Result<UnixListener, StartError> {
    let dir = config::dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| StartError::Transient(format!("cannot create {}: {error}", dir.display())))?;
    // Only a real directory of this user: chmod and the socket must not be
    // redirected through a link somebody else placed.
    let metadata = std::fs::symlink_metadata(&dir)
        .map_err(|error| StartError::Transient(format!("cannot read {}: {error}", dir.display())))?;
    if !metadata.is_dir() || metadata.uid() != config::uid() {
        return Err(StartError::Permanent(format!("{} is not a directory owned by this user", dir.display())));
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| StartError::Transient(format!("cannot protect {}: {error}", dir.display())))?;
    let socket = config::socket_path();
    if UnixStream::connect(&socket).is_ok() {
        // Usually the old agent is still on its way out: launchd tries again.
        return Err(StartError::Transient("another agent is already running".into()));
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|error| StartError::Transient(format!("cannot bind {}: {error}", socket.display())))?;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

fn accept_clients(listener: UnixListener, sender: Sender<Request>) {
    static CLIENTS: AtomicUsize = AtomicUsize::new(0);
    for mut stream in listener.incoming().flatten() {
        // Another user's process gets nothing, not even a refusal.
        if !same_user(&stream) {
            continue;
        }
        let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
        // Take the slot first, then check: two clients can never both get
        // the last one.
        if CLIENTS.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
            CLIENTS.fetch_sub(1, Ordering::SeqCst);
            let _ = writeln!(stream, "error: busy: too many clients");
            continue;
        }
        let sender = sender.clone();
        thread::spawn(move || {
            read_request(stream, &sender);
            CLIENTS.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Reads one command. Everything that needs no Bluetooth call is answered
/// right here, so the other Mac always gets an answer even while the main
/// loop is busy, and nothing but real work waits in the queue.
fn read_request(stream: UnixStream, sender: &Sender<Request>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut line = String::new();
    let mut reply = stream;
    match BufReader::new(reader.take(MAX_REQUEST)).read_line(&mut line) {
        Ok(_) if line.ends_with('\n') => {}
        Ok(_) => {
            let _ = writeln!(reply, "error: the request is not one line of at most {MAX_REQUEST} bytes");
            return;
        }
        Err(_) => return,
    }
    let command = line.trim().to_string();
    let busy = BUSY.load(Ordering::SeqCst);
    match command.as_str() {
        // The peer pings before it lets go of the devices: a Mac in the
        // middle of its own switch says so, and both stay as they are
        // instead of each releasing and then taking everything back.
        "ping" | "held" | "status" | "state" | "overview" if busy => {
            let _ = writeln!(reply, "{BUSY_REPLY}");
        }
        "ping" | "handover" | "arrive" if !BLUETOOTH_READY.load(Ordering::SeqCst) => {
            let _ = writeln!(reply, "{NO_BLUETOOTH}");
        }
        "switch" | "monitor" if !BLUETOOTH_READY.load(Ordering::SeqCst) => {
            let _ = writeln!(reply, "error: this Mac waits for the Bluetooth permission: click Allow for monitor-switch");
        }
        "ping" => {
            let _ = writeln!(reply, "ok");
        }
        // Checks that this agent reaches the other one (setup runs it).
        // It is the agent that must be allowed on the local network, not the
        // terminal, so the call is made from here.
        "reach" => {
            let peer = PEER.get().cloned().unwrap_or_default();
            match peer::call(&peer, "ping", client::timeout("ping")) {
                Ok(_) => {
                    let _ = writeln!(reply, "{peer} answers");
                    let _ = writeln!(reply, "ok");
                }
                // It was reached; what it waits for is not the network.
                Err(failure) if NO_BLUETOOTH.ends_with(&failure.reason) => {
                    let _ = writeln!(reply, "{peer} answers, but waits for the Bluetooth permission");
                    let _ = writeln!(reply, "ok");
                }
                Err(failure) => {
                    let _ = writeln!(reply, "error: {}", failure.reason);
                }
            }
        }
        // Answered from the HID registry, which needs no Bluetooth call and
        // so no main thread.
        "held" => {
            for address in DEVICES.get().into_iter().flatten() {
                if crate::hid::is_live(address) {
                    let _ = writeln!(reply, "{address}");
                }
            }
            let _ = writeln!(reply, "ok");
        }
        // The other Mac is about to show its picture to nobody: the monitor
        // comes only to an input that carries a signal.
        "wake" => {
            wake_display();
            let _ = writeln!(reply, "ok");
        }
        // What this Mac holds, for the menu; no Bluetooth call, so no main
        // thread.
        "state" => {
            for line in state() {
                let _ = writeln!(reply, "{line}");
            }
            let _ = writeln!(reply, "ok");
        }
        // The same for both Macs together.
        "overview" => {
            let peer = PEER.get().cloned().unwrap_or_default();
            let theirs = peer::call(&peer, "state", client::timeout("state")).ok();
            for line in overview(&state(), theirs.as_deref()) {
                let _ = writeln!(reply, "{line}");
            }
            let _ = writeln!(reply, "ok");
        }
        // The other Mac's setup chose other devices: take the same, then
        // restart, so the agent runs on the new config.
        devices if devices.starts_with("devices=") => {
            if busy {
                let _ = writeln!(reply, "{BUSY_REPLY}");
                return;
            }
            match save_devices(&devices["devices=".len()..]) {
                Ok(()) => {
                    let _ = writeln!(reply, "ok");
                    drop(reply);
                    eprintln!("devices changed by the other Mac; restarting");
                    thread::sleep(Duration::from_millis(300));
                    std::process::exit(1);
                }
                Err(error) => {
                    let _ = writeln!(reply, "error: {error}");
                }
            }
        }
        "switch" | "handover" | "arrive" | "monitor" if !claim_busy() => {
            let _ = writeln!(reply, "{BUSY_REPLY}");
        }
        _ => {
            let _ = sender.send(Request { command, reply: Some(reply), received: Instant::now() });
        }
    }
}

/// Writes the devices the other Mac sent into this Mac's config.
fn save_devices(list: &str) -> Result<(), String> {
    let devices = list
        .split(',')
        .map(|address| crate::bt::normalize(address).ok_or(format!("bad Bluetooth address {address:?}")))
        .collect::<Result<Vec<_>, _>>()?;
    if devices.is_empty() || devices.len() > 8 {
        return Err("expected one to eight devices".into());
    }
    let config = Config { devices, ..Config::load()? };
    let path = config::dir().join("config");
    let _ = std::fs::copy(&path, config::dir().join("config.bak"));
    std::fs::write(&path, config.to_text()).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// True when the client runs as the same user as the agent.
fn same_user(stream: &UnixStream) -> bool {
    extern "C" {
        fn getpeereid(socket: c_int, uid: *mut u32, gid: *mut u32) -> c_int;
    }
    let (mut uid, mut gid) = (0, 0);
    unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == config::uid() }
}

/// MacBook only: with the lid closed and the monitor showing the other Mac it
/// would otherwise idle-sleep and stop answering the Mac mini's calls.
fn keep_awake() {
    let pid = std::process::id().to_string();
    spawn_detached(Command::new("caffeinate").args(["-i", "-s", "-w", &pid]));
}

/// Lights this Mac's display for a moment.
fn wake_display() {
    spawn_detached(Command::new("caffeinate").args(["-u", "-t", "3"]));
}

/// Keeps the menu bar icon running: a separate process, like the panel. It
/// ends when its input does, which is when this agent goes away.
fn run_menu() {
    loop {
        let Ok(program) = std::env::current_exe() else {
            return;
        };
        let child = Command::new(program)
            .arg("menu")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            // `wait` would close the input, and with it the menu.
            let input = child.stdin.take();
            let _ = child.wait();
            drop(input);
        }
        thread::sleep(Duration::from_secs(10));
    }
}

/// Starts a helper process and reaps it when it ends.
fn spawn_detached(command: &mut Command) {
    let spawned = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// The local time of day, for the log.
fn clock() -> String {
    Command::new("date")
        .arg("+%F %T")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Runs one request and always ends the answer with `ok` or `error: …`.
fn serve(config: &Config, request: Request) {
    let mut out = Output(request.reply);
    let command = request.command.as_str();
    eprintln!(
        "command: {command} at {} (waited {:.1}s)",
        clock(),
        request.received.elapsed().as_secs_f64()
    );
    // A hand-changing request claimed BUSY when it was accepted; it is
    // released here once the request has run or been dropped.
    let changes_hands = matches!(command, "switch" | "handover" | "arrive" | "monitor");
    // `arrive` is never dropped: the other Mac has already let go of the
    // devices and waits for this one to take them, so dropping it would
    // leave them on neither Mac (this happens when this Mac was asleep).
    if changes_hands && command != "arrive" && request.received.elapsed() > STALE_REQUEST {
        BUSY.store(false, Ordering::SeqCst);
        out.line("error: dropped: the request waited too long");
        return;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| match command {
        "status" => status(config, &mut out),
        "arrive" => arrive(config, &mut out),
        "switch" if !holds_devices(config) => ask_peer_to_hand_over(config, &mut out),
        "switch" | "handover" => hand_over(config, &mut out),
        "monitor" => monitor(config, &mut out),
        other => Err(format!("unknown command: {other}")),
    }))
    .unwrap_or_else(|_| Err("internal error (see agent.log)".into()));
    if changes_hands || command == "status" {
        remember_labels(config);
    }
    if changes_hands {
        BUSY.store(false, Ordering::SeqCst);
        match &result {
            Ok(()) => hud::done(),
            // The other Mac asked this one, which has nothing: it says so to
            // that Mac, which goes on from there; no panel here.
            Err(error) if error == NOT_HOLDING => {}
            Err(error) => hud::failed(&readable(config, error)),
        }
    }
    match result {
        Ok(()) => out.line("ok"),
        Err(error) => out.line(&format!("error: {error}")),
    }
}

fn status(config: &Config, out: &mut Output) -> Result<(), String> {
    let role = match config.role {
        Role::MacBook => "macbook",
        Role::Mini => "mini",
    };
    out.line(&format!("role: {role}"));
    for address in &config.devices {
        let device = bt::Device::with_address(address)?;
        let battery = crate::hid::battery(address).map(|percent| format!(" battery={percent}%")).unwrap_or_default();
        out.line(&format!(
            "{}  paired={} connected={} usable={}{battery}  {}",
            device.address,
            device.is_paired(),
            device.is_connected(),
            device.is_usable(),
            device.name()
        ));
    }
    if let Some(display) = ddc::Display::external() {
        if let ddc::Reply::Value { current } = display.get(0x60) {
            out.line(&format!("monitor input: {current}"));
        }
    }
    Ok(())
}

fn holds_devices(config: &Config) -> bool {
    config.devices.iter().any(|address| {
        bt::Device::with_address(address).is_ok_and(|device| device.is_paired() && device.is_usable())
    })
}

/// This Mac is becoming the active one: show its picture and take the devices.
fn arrive(config: &Config, out: &mut Output) -> Result<(), String> {
    hud::open(&format!("Switching to {}", mac_name(config.role)));
    show_devices(&config.devices);
    AWAY.store(false, Ordering::SeqCst);
    PEEK.store(false, Ordering::SeqCst);
    // Wake the display: in Auto mode the monitor only settles on an input
    // that carries a signal.
    wake_display();
    if config.role == Role::Mini {
        bt::spin(0.3);
        // Any accepted input command makes this monitor leave its current
        // input; in Auto mode it then settles on HDMI. So none is sent when
        // it is here already (`monitor` brought it earlier).
        match ddc::Display::external() {
            Some(display) if monitor_place(&display, config.hdmi_input) == "here" => {
                out.line("monitor: already here")
            }
            Some(display) if display.set(0x60, config.hdmi_input) => out.line("monitor: switching"),
            _ => out.line("monitor: DDC command failed"),
        }
    }
    let result = take_devices(&config.devices, out);
    mark_handover();
    // On failure the title stays in progress and turns red with the error.
    if result.is_ok() {
        hud::row(hud::TITLE, &format!("Switched to {}", mac_name(config.role)), hud::OK, "");
    }
    result
}

/// An error for the panel: device names in place of their addresses.
fn readable(config: &Config, error: &str) -> String {
    let mut text = error.to_string();
    for (index, address) in config.devices.iter().enumerate() {
        text = text.replace(address.as_str(), &device_label(address, index));
    }
    sentence(&text)
}

/// An error text as the panel shows it: with a capital first letter.
fn sentence(text: &str) -> String {
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// What the panel calls a Mac.
fn mac_name(role: Role) -> &'static str {
    match role {
        Role::Mini => "Mac mini",
        Role::MacBook => "MacBook",
    }
}

pub fn other_role(role: Role) -> Role {
    match role {
        Role::Mini => Role::MacBook,
        Role::MacBook => Role::Mini,
    }
}

/// The name the panel shows for the device at `index` of the configured
/// list. A Mac that never had the device paired knows only its address;
/// the list is keyboard first, so the place in it says what the device is.
fn device_label(address: &str, index: usize) -> String {
    let name = bt::Device::with_address(address).map(|device| device.name()).unwrap_or_default();
    if name.is_empty() || bt::normalize(&name).is_some() {
        return generic_label(address, index);
    }
    name
}

fn generic_label(address: &str, index: usize) -> String {
    match index {
        0 => "Keyboard".into(),
        1 => "Mouse".into(),
        _ => address.to_string(),
    }
}

/// A battery at or under this many percent is pointed out in the panel.
const LOW_BATTERY: u8 = 10;

/// The label with the device's charge, when this Mac can read it.
fn with_battery(label: &str, address: &str) -> String {
    match crate::hid::battery(address) {
        Some(percent) => format!("{label} · {percent}%"),
        None => label.to_string(),
    }
}

/// Puts a row for every device on the panel, in progress.
fn show_devices(addresses: &[String]) {
    for (index, address) in addresses.iter().enumerate() {
        hud::row(address, &with_battery(&device_label(address, index), address), hud::BUSY, "");
    }
}

/// Marks a device that came up on this Mac, with its charge: a mouse about
/// to run flat otherwise looks like a switch that failed.
fn show_arrived(address: &str, index: usize) {
    let label = with_battery(&device_label(address, index), address);
    let low = crate::hid::battery(address).is_some_and(|percent| percent <= LOW_BATTERY);
    hud::row(address, &label, hud::OK, if low { "Battery low: charge it soon" } else { "" });
}

/// Pairs the devices, keyboard first, and always reports which ones arrived.
fn take_devices(addresses: &[String], out: &mut Output) -> Result<(), String> {
    let started = Instant::now();
    let mut failures = Vec::new();
    let mut took = Vec::new();
    for (index, address) in addresses.iter().enumerate() {
        let device = match bt::Device::with_address(address) {
            Ok(device) => device,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        if !(device.is_paired() && device.is_usable()) {
            let earliest = FIRST_CONTACT + CONTACT_SPACING * index as u32;
            while started.elapsed() < earliest {
                bt::spin(0.2);
            }
        }
        hud::row(address, "", hud::BUSY, "");
        let mut ask_to_wake = || hud::row(address, "", hud::BUSY, "Not answering. Click it, or switch it off and on");
        match device.take(TAKE_TIMEOUT, &mut ask_to_wake) {
            Ok(()) => {
                show_arrived(address, DEVICES.get().and_then(|all| all.iter().position(|known| known == address)).unwrap_or(index));
                out.line(&format!("{} connected", device.address));
                took.push(device.address);
            }
            Err(error) => {
                hud::row(address, "", hud::FAIL, &sentence(&error));
                failures.push(format!("{}: {error}", device.address));
            }
        }
    }
    out.line(&format!("{TOOK}{}", took.join(",")));
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Moves the devices (and the monitor) from this Mac to the other one.
fn hand_over(config: &Config, out: &mut Output) -> Result<(), String> {
    wait_settled();
    // Resolve every device before touching any, so a bad address cannot
    // stop the release halfway.
    let devices = config
        .devices
        .iter()
        .map(|address| bt::Device::with_address(address))
        .collect::<Result<Vec<_>, _>>()?;
    if !devices.iter().any(|device| device.is_paired() && device.is_usable()) {
        return Err(NOT_HOLDING.into());
    }
    let other = mac_name(other_role(config.role));
    hud::open(&format!("Switching to {other}"));
    show_devices(&config.devices);
    // Never let go of the keyboard before the other side is known to answer
    // and not to be switching itself.
    peer::call(&config.peer, "ping", client::timeout("ping")).map_err(|failure| {
        format!("the other Mac is not ready, nothing changed ({})", failure.reason)
    })?;

    if let Err(error) = release(&devices, out) {
        // bluetoothd still carries out the unpairing it was asked for. Taking
        // the devices back now would look fine, and then that late unpairing
        // would drop them: let it finish first.
        let deadline = Instant::now() + LATE_RELEASE;
        while Instant::now() < deadline && devices.iter().any(bt::Device::is_paired) {
            bt::spin(0.1);
        }
        out.line("taking the devices back");
        if let Err(back) = take_devices(&config.devices, out) {
            return Err(format!("{error}; taking back: {back}"));
        }
        return Err(error);
    }
    mark_handover();

    // The other Mac pairs the devices while the Mac mini gets the monitor off
    // its input; doing the two one after the other would add seconds.
    let arrival = peer::start(&config.peer, "arrive", client::timeout("arrive"));
    if config.role == Role::Mini {
        let _ = leave_hdmi(config, out);
    }
    let reply = arrival.and_then(peer::Call::wait);
    let stdout = match &reply {
        Ok(stdout) => stdout.clone(),
        Err(failure) => failure.stdout.clone(),
    };
    for line in stdout.lines().filter(|line| *line != "ok" && !line.starts_with(TOOK)) {
        out.line(&format!("peer: {line}"));
    }
    let Err(failure) = reply else {
        AWAY.store(true, Ordering::SeqCst);
        PEEK.store(false, Ordering::SeqCst);
        hud::row(hud::TITLE, &format!("Switched to {other}"), hud::OK, "");
        for address in &config.devices {
            hud::row(address, "", hud::OK, "");
        }
        return Ok(());
    };

    // Take back only what the other Mac did not get: pulling away a device
    // it holds would leave the two Macs fighting over it. With no `took:`
    // line (the call broke off), ask it what it holds now. While it is still
    // pairing it answers busy rather than a partial list; if it cannot say,
    // take nothing.
    let held = match parse_took(&stdout) {
        Some(held) => held,
        // Refused before it began, or never got the call at all: it took
        // nothing, so everything comes back here. Asking `held` would only
        // get busy again, or fail the same way.
        None if refused_busy(&stdout) || !failure.delivered => Vec::new(),
        None => match peer::call(&config.peer, "held", client::timeout("held")) {
            Ok(held) => parse_held(&held),
            Err(query) => {
                return Err(format!(
                    "the other Mac: {}; it cannot say what it holds ({}), so nothing was \
                     taken back: check `monitor-switch status` on both Macs",
                    failure.reason, query.reason
                ))
            }
        },
    };
    let missing: Vec<String> =
        config.devices.iter().filter(|address| !held.contains(address)).cloned().collect();
    for address in &held {
        hud::row(address, "", hud::OK, &format!("On {other}"));
    }
    if !missing.is_empty() {
        out.line("the other Mac did not take everything, taking the rest back");
        let back = take_devices(&missing, out);
        for address in &missing {
            let here = bt::Device::with_address(address)
                .is_ok_and(|device| device.is_paired() && device.is_usable());
            if here {
                hud::row(address, "", hud::WARN, "Stayed on this Mac");
            }
        }
        if let Err(error) = back {
            return Err(format!("the other Mac: {}; taking back: {error}", failure.reason));
        }
    }
    Err(format!("the other Mac: {}", failure.reason))
}

/// Waits until the devices have been here for `SETTLE_TIME`. A switch asked
/// for sooner is not refused, only carried out a moment later.
fn wait_settled() {
    let last = *LAST_HANDOVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(last) = last {
        while last.elapsed() < SETTLE_TIME {
            bt::spin(0.1);
        }
    }
}

fn mark_handover() {
    *LAST_HANDOVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
}

/// Unpairs the devices and waits until this Mac has fully let go of them.
///
/// The other Mac must not be called earlier: a mouse reached while it is
/// still attached here refuses the new pairing. Unpairing completes on this
/// thread's run loop, so the wait keeps it turning.
fn release(devices: &[bt::Device], out: &mut Output) -> Result<(), String> {
    for device in devices {
        // A device learns that it was unpaired only over a live link. One
        // that is forgotten while disconnected keeps waiting for this Mac.
        if device.is_paired() && !device.is_usable() {
            out.line(&format!(
                "{} was not connected: switch it off and on so the other Mac can pair with it",
                device.name()
            ));
        }
        device.begin_release().map_err(|error| format!("{}: {error}", device.address))?;
    }
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    while Instant::now() < deadline && !devices.iter().all(bt::Device::is_released) {
        bt::spin(0.1);
    }
    let stuck: Vec<&str> =
        devices.iter().filter(|device| !device.is_released()).map(|device| device.address.as_str()).collect();
    if !stuck.is_empty() {
        return Err(format!("still attached here after unpairing: {}", stuck.join(", ")));
    }
    for device in devices {
        out.line(&format!("{} released", device.address));
    }
    Ok(())
}

/// The hotkey was pressed on the Mac that holds neither device. The other
/// Mac is asked to hand over; this request is not waited for, because the
/// other Mac answers by asking this agent to `arrive`, which it can only do
/// once it is free again. The wait runs on its own thread and so sleeps
/// instead of turning the run loop (see `peer::Call::wait`).
fn ask_peer_to_hand_over(config: &Config, out: &mut Output) -> Result<(), String> {
    // The panel stays up after this request is answered: the other Mac's
    // `arrive` replaces it, or the thread below reports the failure in it.
    hud::open(&format!("Switching to {}", mac_name(config.role)));
    let call = peer::start(&config.peer, "handover", client::timeout("handover"))
        .map_err(|failure| failure.reason)?;
    thread::spawn(move || {
        if let Err(failure) = call.wait() {
            eprintln!("handover on the other Mac failed: {}", failure.reason);
            if failure.stdout.lines().any(|line| line.strip_prefix("error: ") == Some(NOT_HOLDING)) {
                // Neither Mac holds them: they were left in between. Pair
                // them here; the user is at this Mac.
                if let Some(requests) = REQUESTS.get().filter(|_| claim_busy()) {
                    let _ = requests.send(Request {
                        command: "arrive".into(),
                        reply: None,
                        received: Instant::now(),
                    });
                }
            } else {
                hud::failed(&format!("The other Mac did not hand over: {}", failure.reason));
            }
        }
    });
    out.line("asked the other Mac to hand over");
    hud::keep_open();
    Ok(())
}

/// Mac mini only: gets the monitor off this Mac's HDMI input.
///
/// With its HDMI signal gone the monitor (Input Select = Auto) moves to
/// DisplayPort by itself, but only after about six seconds without a signal.
/// Anything that wakes this display in that window keeps the monitor here,
/// so the display is put back to sleep until the monitor reports another
/// input. Returns whether the monitor left.
fn leave_hdmi(config: &Config, out: &mut Output) -> bool {
    let sleep_display = || {
        let _ = Command::new("pmset").arg("displaysleepnow").status();
    };
    let Some(display) = ddc::Display::external() else {
        sleep_display();
        out.line("monitor: no DDC display found; display put to sleep once");
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        sleep_display();
        bt::spin(1.0);
        if let ddc::Reply::Value { current } = display.get(0x60) {
            if current != config.hdmi_input {
                learn_peer_input(current);
                out.line("monitor: left for the MacBook");
                return true;
            }
        }
        if Instant::now() >= deadline {
            out.line("monitor: still on this Mac after 20 s; is its display being woken?");
            return false;
        }
    }
}

/// Where the monitor is, as the Mac mini sees it: `here` on its own input,
/// `away` on another one, `unknown` when the monitor does not answer.
fn monitor_place(display: &ddc::Display, hdmi_input: u16) -> &'static str {
    match display.get(0x60) {
        ddc::Reply::Value { current } if current == hdmi_input => "here",
        ddc::Reply::Value { .. } => "away",
        _ => "unknown",
    }
}

/// Moves only the monitor to the other Mac and back; the keyboard and mouse
/// stay where they are.
fn monitor(config: &Config, out: &mut Output) -> Result<(), String> {
    if config.role == Role::MacBook {
        hud::open("Moving the monitor");
        // If it comes here, it needs a picture to come to.
        wake_display();
        let answer = peer::call(&config.peer, "monitor", MONITOR_MOVE)
            .map_err(|failure| format!("the other Mac: {}", failure.reason))?;
        let shows = answer.lines().find_map(|line| line.strip_prefix(SHOWS)).unwrap_or("the other Mac");
        out.line(&format!("{SHOWS}{shows}"));
        hud::row(hud::TITLE, &format!("The monitor shows {shows}"), hud::OK, "");
        return Ok(());
    }
    let display = ddc::Display::external().ok_or("no monitor found over DDC")?;
    let target = match monitor_place(&display, config.hdmi_input) {
        "here" => Role::MacBook,
        "away" => Role::Mini,
        _ => return Err("the monitor does not answer over DDC".into()),
    };
    hud::open(&format!("Moving the monitor to {}", mac_name(target)));
    if target == Role::MacBook {
        peer::call(&config.peer, "wake", client::timeout("wake"))
            .map_err(|failure| format!("the other Mac is not ready, nothing changed ({})", failure.reason))?;
        if !leave_hdmi(config, out) {
            return Err("the monitor stayed on this Mac".into());
        }
    } else {
        wake_display();
        bt::spin(0.3);
        if !display.set(0x60, config.hdmi_input) {
            return Err("the monitor did not take the DDC command".into());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while monitor_place(&display, config.hdmi_input) != "here" {
            if Instant::now() >= deadline {
                return Err("the monitor did not come to this Mac".into());
            }
            bt::spin(0.5);
        }
    }
    // Away from the devices on purpose: `keep_monitor` leaves it alone.
    let devices_here = config.devices.iter().any(|address| crate::hid::is_live(address));
    PEEK.store((target == Role::Mini) != devices_here, Ordering::SeqCst);
    out.line(&format!("{SHOWS}{}", mac_name(target)));
    hud::row(hud::TITLE, &format!("The monitor shows {}", mac_name(target)), hud::OK, "");
    Ok(())
}

/// Prefix of the line in which `monitor` says where the monitor went.
const SHOWS: &str = "monitor shows: ";

/// When `keep_monitor` next looks, and what it saw last.
struct MonitorWatch {
    next: Instant,
    was_asleep: bool,
    /// Looks left after this Mac's display woke up.
    after_wake: u8,
}

impl MonitorWatch {
    fn new() -> MonitorWatch {
        MonitorWatch { next: Instant::now(), was_asleep: false, after_wake: 0 }
    }
}

/// Mac mini only: keeps the monitor with the keyboard and mouse.
///
/// The monitor (Input Select = Auto) moves by itself to the other input when
/// the Mac it shows puts its display to sleep, and does not come back when
/// that Mac wakes: the user returns to a keyboard on one Mac and a picture of
/// the other. Two cases, both only while this Mac's display is awake:
///
/// - the devices are here and this display just woke up, but the monitor
///   shows the MacBook: it is called back;
/// - the devices went to the MacBook, but the monitor shows this Mac: this
///   display is put to sleep, so the monitor returns to the MacBook, or
///   waits for it.
///
/// Nothing is done after `monitor` parted the two on purpose (`PEEK`), nor
/// when the monitor is on an input that is neither Mac's.
fn keep_monitor(config: &Config, watch: &mut MonitorWatch) {
    let now = Instant::now();
    if now < watch.next || BUSY.load(Ordering::SeqCst) || PEEK.load(Ordering::SeqCst) {
        return;
    }
    watch.next = now + Duration::from_secs(2);
    if ddc::is_asleep() {
        watch.was_asleep = true;
        return;
    }
    let here = config.devices.iter().any(|address| crate::hid::is_live(address));
    if here {
        AWAY.store(false, Ordering::SeqCst);
        if std::mem::take(&mut watch.was_asleep) {
            watch.after_wake = 3;
        }
        if watch.after_wake == 0 {
            return;
        }
        watch.after_wake -= 1;
        watch.next = now + Duration::from_secs(4);
        let peer_input = PEER_INPUT.load(Ordering::SeqCst);
        let Some(display) = ddc::Display::external() else {
            return;
        };
        if matches!(display.get(0x60), ddc::Reply::Value { current } if peer_input != 0 && current == peer_input) {
            eprintln!("the monitor shows the MacBook, the devices are here: calling it back");
            let _ = display.set(0x60, config.hdmi_input);
        }
    } else if AWAY.load(Ordering::SeqCst) {
        watch.was_asleep = false;
        watch.next = now + Duration::from_secs(10);
        let Some(display) = ddc::Display::external() else {
            return;
        };
        if monitor_place(&display, config.hdmi_input) == "here" {
            eprintln!("the monitor shows this Mac, the devices are on the MacBook: putting this display to sleep");
            let _ = Command::new("pmset").arg("displaysleepnow").status();
        }
    }
}

/// What this Mac holds, one fact per line, fields separated by tabs:
///
/// ```text
/// role     <mini|macbook>
/// device   <address>  <label>  <here|away>  <battery percent, or empty>
/// monitor  <here|away|unknown>                 (Mac mini only)
/// ```
fn state() -> Vec<String> {
    let Some(&(role, hdmi_input)) = ROLE.get() else {
        return Vec::new();
    };
    let mut lines = vec![format!("role\t{}", if role == Role::Mini { "mini" } else { "macbook" })];
    let labels = LABELS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    for (index, address) in DEVICES.get().into_iter().flatten().enumerate() {
        let label = labels.get(index).cloned().unwrap_or_else(|| generic_label(address, index));
        let place = if crate::hid::is_live(address) { "here" } else { "away" };
        let battery = crate::hid::battery(address).map(|percent| percent.to_string()).unwrap_or_default();
        lines.push(format!("device\t{address}\t{label}\t{place}\t{battery}"));
    }
    if role == Role::Mini {
        let place = ddc::Display::external().map_or("unknown", |display| monitor_place(&display, hdmi_input));
        lines.push(format!("monitor\t{place}"));
    }
    lines
}

/// Both Macs' `state` put together for the menu; `theirs` is `None` when the
/// other Mac did not answer. A place is the name of a Mac, `neither` or
/// `unknown`:
///
/// ```text
/// self     <this Mac's name>
/// other    <the other Mac's name>
/// monitor  <place>
/// device   <label>  <place>  <battery percent, or empty>
/// ```
fn overview(ours: &[String], theirs: Option<&str>) -> Vec<String> {
    let fields = |line: &str| line.split('\t').map(String::from).collect::<Vec<_>>();
    let ours: Vec<Vec<String>> = ours.iter().map(|line| fields(line)).collect();
    let theirs: Option<Vec<Vec<String>>> = theirs.map(|text| text.lines().map(fields).collect());
    let mini = ours.iter().any(|line| line[..] == ["role", "mini"]);
    let (this, other) = if mini { ("Mac mini", "MacBook") } else { ("MacBook", "Mac mini") };
    let mut lines = vec![format!("self\t{this}"), format!("other\t{other}")];

    // Only the Mac mini knows where the monitor is.
    let seen = if mini { Some(&ours) } else { theirs.as_ref() };
    let place = seen
        .and_then(|lines| lines.iter().find(|line| line.len() == 2 && line[0] == "monitor"))
        .map_or("unknown", |line| match line[1].as_str() {
            "here" => "Mac mini",
            "away" => "MacBook",
            _ => "unknown",
        });
    lines.push(format!("monitor\t{place}"));

    let device = |line: &&Vec<String>| line.len() == 5 && line[0] == "device";
    for mine in ours.iter().filter(device) {
        let (address, label) = (&mine[1], &mine[2]);
        let peer = theirs.as_ref().map(|lines| lines.iter().filter(device).find(|line| &line[1] == address));
        let line = match peer {
            _ if mine[3] == "here" => format!("device\t{label}\t{this}\t{}", mine[4]),
            Some(Some(peer)) if peer[3] == "here" => format!("device\t{}\t{other}\t{}", peer[2], peer[4]),
            Some(_) => format!("device\t{label}\tneither\t"),
            None => format!("device\t{label}\tunknown\t"),
        };
        lines.push(line);
    }
    lines
}

/// The addresses in a `took:` line, or `None` if there is no such line.
fn parse_took(stdout: &str) -> Option<Vec<String>> {
    stdout.lines().find_map(|line| line.strip_prefix(TOOK)).map(|list| {
        list.split(',').map(str::trim).filter(|address| !address.is_empty()).map(String::from).collect()
    })
}

/// True when the peer refused the request because it is running a switch
/// of its own: its verdict line is `BUSY_REPLY`.
fn refused_busy(stdout: &str) -> bool {
    stdout.lines().any(|line| line == BUSY_REPLY)
}

/// The addresses in a `held` answer. The answer ends with
/// the verdict line, which is not an address.
fn parse_held(answer: &str) -> Vec<String> {
    answer.lines().filter_map(crate::bt::normalize).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_refusal_means_nothing_was_taken() {
        assert!(refused_busy("error: busy: a switch is already running"));
        assert!(!refused_busy("a connected\ntook: a\nerror: x"));
        assert!(!refused_busy("error: busy: too many clients"));
    }

    #[test]
    fn puts_both_macs_together() {
        let ours: Vec<String> = [
            "role\tmacbook",
            "device\ta1\tKeyboard\there\t50",
            "device\ta2\tMouse\taway\t",
            "device\ta3\tPad\taway\t",
        ]
        .map(String::from)
        .to_vec();
        let theirs = "role\tmini\ndevice\ta1\tKeyboard\taway\t\ndevice\ta2\tMy mouse\there\t10\n\
                      device\ta3\tPad\taway\t\nmonitor\there\nok";
        assert_eq!(
            overview(&ours, Some(theirs)),
            [
                "self\tMacBook",
                "other\tMac mini",
                "monitor\tMac mini",
                "device\tKeyboard\tMacBook\t50",
                "device\tMy mouse\tMac mini\t10",
                "device\tPad\tneither\t",
            ]
        );
        let alone = overview(&ours, None);
        assert_eq!(alone[2], "monitor\tunknown");
        assert_eq!(alone[4], "device\tMouse\tunknown\t");
    }

    #[test]
    fn reads_the_held_answer() {
        assert_eq!(parse_held("aa-bb-cc-dd-ee-01\nok"), ["aa-bb-cc-dd-ee-01"]);
        assert_eq!(parse_held("ok"), Vec::<String>::new());
    }

    #[test]
    fn reads_the_took_line() {
        let stdout = "a connected\ntook: aa-bb-cc-dd-ee-01,aa-bb-cc-dd-ee-02\nerror: x";
        assert_eq!(parse_took(stdout).unwrap(), ["aa-bb-cc-dd-ee-01", "aa-bb-cc-dd-ee-02"]);
        assert_eq!(parse_took("took: ").unwrap(), Vec::<String>::new());
        assert_eq!(parse_took("monitor: switching"), None);
    }
}
