//! The background agent, one per Mac, started by launchd in the GUI session.
//!
//! It exists because macOS grants Bluetooth access only to a process that is
//! its own "responsible" application: a command run from ssh or from another
//! app's shell is refused. Everything that touches Bluetooth runs here, on
//! the main thread, which also turns the run loop IOBluetooth calls back on.
//! Clients talk to it over a Unix socket (see `client`).
//!
//! The two Macs call each other over ssh (`peer_ssh` in the config) with the
//! same commands; only the monitor handling differs by role. A hand-over:
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

use crate::bt;
use crate::client;
use crate::config::{self, Config, Role};
use crate::ddc;
use crate::notify::notify;
use crate::peer;
use std::ffi::c_int;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{self, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

/// A queued switch older than this is dropped instead of acted on late.
const STALE_REQUEST: Duration = Duration::from_secs(10);

/// Most client connections served at once.
const MAX_CLIENTS: usize = 16;

/// Longest request line a client may send.
const MAX_REQUEST: u64 = 64;

/// Longest a single request may keep the main loop: a hand-over at its worst
/// (see `client::timeout`) plus a margin. Past this the loop is taken to be
/// hung and the agent exits with failure, so launchd starts a fresh one.
const HUNG_AFTER: Duration = Duration::from_secs(400);

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
    let (sender, receiver) = mpsc::channel::<Request>();
    let _ = REQUESTS.set(sender.clone());
    let _ = DEVICES.set(config.devices.clone());

    tick();
    thread::spawn(move || accept_clients(listener, sender));
    thread::spawn(watchdog);
    if config.role == Role::MacBook {
        keep_awake();
    }
    eprintln!("agent started");

    let log = config::dir().join("agent.log");
    let mut rotated = Instant::now();
    loop {
        tick();
        if let Ok(request) = receiver.recv_timeout(Duration::from_millis(200)) {
            bt::with_pool(|| serve(&config, request));
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
        "ping" | "held" | "status" if busy => {
            let _ = writeln!(reply, "{BUSY_REPLY}");
        }
        "ping" => {
            let _ = writeln!(reply, "ok");
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
        "switch" | "handover" | "arrive" if !claim_busy() => {
            let _ = writeln!(reply, "{BUSY_REPLY}");
        }
        _ => {
            let _ = sender.send(Request { command, reply: Some(reply), received: Instant::now() });
        }
    }
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
/// would otherwise idle-sleep and stop answering the Mac mini's ssh calls.
fn keep_awake() {
    let pid = std::process::id().to_string();
    spawn_detached(Command::new("caffeinate").args(["-i", "-s", "-w", &pid]));
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

/// Runs one request and always ends the answer with `ok` or `error: …`.
fn serve(config: &Config, request: Request) {
    let mut out = Output(request.reply);
    let command = request.command.as_str();
    eprintln!("command: {command}");
    // A hand-changing request claimed BUSY when it was accepted; it is
    // released here once the request has run or been dropped.
    let changes_hands = matches!(command, "switch" | "handover" | "arrive");
    if changes_hands && request.received.elapsed() > STALE_REQUEST {
        BUSY.store(false, Ordering::SeqCst);
        out.line("error: dropped: the request waited too long");
        return;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| match command {
        "status" => status(config, &mut out),
        "arrive" => arrive(config, &mut out),
        "switch" if !holds_devices(config) => ask_peer_to_hand_over(config, &mut out),
        "switch" | "handover" => hand_over(config, &mut out),
        other => Err(format!("unknown command: {other}")),
    }))
    .unwrap_or_else(|_| Err("internal error (see agent.log)".into()));
    if changes_hands {
        BUSY.store(false, Ordering::SeqCst);
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
        out.line(&format!(
            "{}  paired={} connected={} usable={}  {}",
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
    // Wake the display: in Auto mode the monitor only settles on an input
    // that carries a signal.
    spawn_detached(Command::new("caffeinate").args(["-u", "-t", "3"]));
    if config.role == Role::Mini {
        bt::spin(0.3);
        // Any accepted input command makes this monitor leave its current
        // input; in Auto mode it then settles on HDMI.
        match ddc::Display::external() {
            Some(display) if display.set(0x60, config.hdmi_input) => out.line("monitor: switching"),
            _ => out.line("monitor: DDC command failed"),
        }
    }
    let result = take_devices(&config.devices, out);
    mark_handover();
    result
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
        match device.take(TAKE_TIMEOUT) {
            Ok(()) => {
                out.line(&format!("{} connected", device.address));
                took.push(device.address);
            }
            Err(error) => failures.push(format!("{}: {error}", device.address)),
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
    // Never let go of the keyboard before the other side is known to answer
    // and not to be switching itself.
    peer::call(&config.peer_ssh, "ping", client::timeout("ping")).map_err(|failure| {
        format!("the other Mac is not ready, nothing changed ({})", failure.reason)
    })?;

    if let Err(error) = release(&devices, out) {
        out.line("taking the devices back");
        let _ = take_devices(&config.devices, out);
        return Err(error);
    }
    mark_handover();

    // The other Mac pairs the devices while the Mac mini gets the monitor off
    // its input; doing the two one after the other would add seconds.
    let arrival = peer::start(&config.peer_ssh, "arrive", client::timeout("arrive"));
    if config.role == Role::Mini {
        leave_hdmi(config, out);
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
        return Ok(());
    };

    // Take back only what the other Mac did not get: pulling away a device
    // it holds would leave the two Macs fighting over it. With no `took:`
    // line (the call broke off), ask it what it holds now. While it is still
    // pairing it answers busy rather than a partial list; if it cannot say,
    // take nothing.
    let held = match parse_took(&stdout) {
        Some(held) => held,
        // Refused before it began: it took nothing, so everything comes back
        // here. Asking `held` would only get busy again.
        None if refused_busy(&stdout) => Vec::new(),
        None => match peer::call(&config.peer_ssh, "held", client::timeout("held")) {
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
    if !missing.is_empty() {
        out.line("the other Mac did not take everything, taking the rest back");
        if let Err(error) = take_devices(&missing, out) {
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
    let call = peer::start(&config.peer_ssh, "handover", client::timeout("handover"))
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
                notify(&format!("The other Mac did not hand over: {}", failure.reason));
            }
        }
    });
    out.line("asked the other Mac to hand over");
    Ok(())
}

/// Mac mini only: gets the monitor off this Mac's HDMI input.
///
/// With its HDMI signal gone the monitor (Input Select = Auto) moves to
/// DisplayPort by itself, but only after about six seconds without a signal.
/// Anything that wakes this display in that window keeps the monitor here,
/// so the display is put back to sleep until the monitor reports another
/// input.
fn leave_hdmi(config: &Config, out: &mut Output) {
    let sleep_display = || {
        let _ = Command::new("pmset").arg("displaysleepnow").status();
    };
    let Some(display) = ddc::Display::external() else {
        sleep_display();
        out.line("monitor: no DDC display found; display put to sleep once");
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        sleep_display();
        bt::spin(1.0);
        if matches!(display.get(0x60), ddc::Reply::Value { current } if current != config.hdmi_input) {
            out.line("monitor: left for the MacBook");
            return;
        }
        if Instant::now() >= deadline {
            out.line("monitor: still on this Mac after 20 s; is its display being woken?");
            return;
        }
    }
}

/// The addresses in a `took:` line, or `None` if there is no such line.
fn parse_took(stdout: &str) -> Option<Vec<String>> {
    stdout.lines().find_map(|line| line.strip_prefix(TOOK)).map(|list| {
        list.split(',').map(str::trim).filter(|address| !address.is_empty()).map(String::from).collect()
    })
}

/// True when the peer refused the request because it is running a switch
/// of its own: through `ssh-gate` its verdict line is `BUSY_REPLY`.
fn refused_busy(stdout: &str) -> bool {
    stdout.lines().any(|line| line == BUSY_REPLY)
}

/// The addresses in a `held` answer. Through `ssh-gate` the answer ends with
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
