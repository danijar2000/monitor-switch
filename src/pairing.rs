//! Pairs the two Macs over the home network, the way a Bluetooth device
//! pairs: `monitor-switch setup` runs on both, they find each other by
//! Bonjour, agree on a secret by Diffie-Hellman and show the same six-digit
//! code, which the user confirms on both. No password, ssh or user name.
//!
//! The Mac whose host name sorts first calls the other on `PORT`. Its first
//! line commits to its public key by a hash, so a machine in the middle
//! cannot try keys until the codes happen to match:
//!
//! ```text
//! caller   monitor-switch-pair 1 <role> <host> <sha256 of its key>
//! answer   monitor-switch-pair 1 <role> <host> <its key>
//! caller   key <its key>
//!          ... both show the code; the user confirms ...
//! both     ok <devices> <hmac>        or   no
//! ```
//!
//! `<devices>` lists the keyboards and mice the Mac offers, as
//! `address/kind/name-in-hex` separated by commas, or `-` for none. The
//! secret is `SHA-256("monitor-switch pairing 1" | shared | caller key |
//! answer key | both hello lines)`, so a host or role changed on the way
//! changes the code; the HMAC binds the device list and the role to it.
//!
//! Everything the other side sends is checked before it is shown: a host is
//! letters, digits, dots and dashes only, so it cannot carry terminal
//! control sequences that would draw a code over the real one.

use crate::auth::{from_hex, hmac_sha256, same, sha256, to_hex};
use crate::config::Role;
use crate::ecdh::KeyPair;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// The port pairing listens on, only while `setup` runs.
pub const PORT: u16 = 47812;

const SERVICE: &str = "_monitor-switch._tcp";
const HELLO: &str = "monitor-switch-pair 1";

/// A Bluetooth keyboard or mouse: normalized address, kind (0 keyboard,
/// 1 mouse or trackpad) and name.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub address: String,
    pub kind: u8,
    pub name: String,
}

/// What this Mac brings to the pairing.
pub struct Me {
    pub role: Role,
    pub host: String,
    pub devices: Vec<Device>,
}

/// What pairing learned about the other Mac.
pub struct Peer {
    pub role: Role,
    pub host: String,
    pub devices: Vec<Device>,
    /// The secret both agents sign with, as hex.
    pub secret: String,
}

pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Mini => "mini",
        Role::MacBook => "macbook",
    }
}

fn parse_role(text: &str) -> Option<Role> {
    match text {
        "mini" => Some(Role::Mini),
        "macbook" => Some(Role::MacBook),
        _ => None,
    }
}

fn encode_devices(devices: &[Device]) -> String {
    if devices.is_empty() {
        return "-".into();
    }
    let entries: Vec<String> =
        devices.iter().map(|device| format!("{}/{}/{}", device.address, device.kind, to_hex(device.name.as_bytes()))).collect();
    entries.join(",")
}

fn decode_devices(text: &str) -> Option<Vec<Device>> {
    if text == "-" {
        return Some(Vec::new());
    }
    text.split(',')
        .map(|entry| {
            let mut fields = entry.split('/');
            let address = crate::bt::normalize(fields.next()?)?;
            let kind = fields.next()?.parse().ok()?;
            let name = String::from_utf8(from_hex(fields.next()?)?).ok()?;
            Some(Device { address, kind, name: printable(&name) })
        })
        .collect()
}

/// The code the user compares: six digits from the secret.
fn code(secret: &[u8]) -> String {
    let mac = hmac_sha256(secret, b"code");
    format!("{:06}", u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]) % 1_000_000)
}

fn confirmation(secret: &[u8], role: Role, devices: &str) -> String {
    to_hex(&hmac_sha256(secret, format!("ok\n{}\n{devices}", role_name(role)).as_bytes()))
}

/// A helper process that is stopped when dropped.
struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn dns_sd(args: &[&str]) -> Result<Helper, String> {
    Command::new("/usr/bin/dns-sd")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map(Helper)
        .map_err(|error| format!("cannot run dns-sd: {error}"))
}

/// Where a Bonjour instance can be reached, as `host.local`.
fn resolve(instance: &str) -> Option<String> {
    let mut helper = dns_sd(&["-L", instance, SERVICE, "local"]).ok()?;
    let (sender, receiver) = mpsc::channel();
    let output = helper.0.stdout.take()?;
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            // "... can be reached at Daniiars-Mac-mini.local.:47812 (interface 14)"
            if let Some(rest) = line.split("can be reached at ").nth(1) {
                let host = rest.split(':').next().unwrap_or("").trim_end_matches('.').to_string();
                let _ = sender.send(host);
                return;
            }
        }
    });
    receiver.recv_timeout(Duration::from_secs(5)).ok().filter(|host| !host.is_empty())
}

/// Reports every other Mac running `setup`, by host name, on `found`.
fn browse(own_instance: String, found: Sender<String>) -> Result<Helper, String> {
    let mut helper = dns_sd(&["-B", SERVICE, "local"])?;
    let output = helper.0.stdout.take().ok_or("dns-sd gave no output")?;
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            // "Timestamp  A/R  Flags  if  Domain  Service Type  Instance Name"
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(1) != Some(&"Add") {
                continue;
            }
            let Some(at) = line.find(SERVICE) else { continue };
            let instance = line[at + SERVICE.len()..].trim().trim_start_matches('.').trim().to_string();
            if instance.is_empty() || instance == own_instance {
                continue;
            }
            if let Some(host) = resolve(&instance) {
                let _ = found.send(host);
            }
        }
    });
    Ok(helper)
}

fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return v4.is_private() || v4.is_link_local() || v4.is_loopback();
            }
            let first = ip.segments()[0];
            ip.is_loopback() || (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
        }
    }
}

/// Events the pairing waits for.
enum Event {
    /// A Mac found by Bonjour.
    Found(String),
    /// A name the user typed: always called, whatever the order.
    Typed(String),
    Called(TcpStream),
}

/// How long the other side may take to greet and send its key. Only the
/// user's answer to the code may take long.
const HANDSHAKE: Duration = Duration::from_secs(15);
const CONFIRMATION: Duration = Duration::from_secs(300);

/// Longest line the other side may send.
const MAX_LINE: u64 = 4096;

fn line(reader: &mut BufReader<TcpStream>) -> Result<String, String> {
    let mut text = String::new();
    // Bounded, so a sender that never ends the line cannot fill memory.
    match reader.by_ref().take(MAX_LINE).read_line(&mut text) {
        Ok(0) => Err("the other Mac broke off the pairing".into()),
        Ok(_) if !text.ends_with('\n') => Err("the other Mac sent too long a line".into()),
        Ok(_) => Ok(text.trim_end().to_string()),
        Err(error) => Err(format!("the other Mac does not answer ({error})")),
    }
}

/// A host name as pairing accepts it: letters, digits, dots and dashes.
fn valid_host(host: &str) -> bool {
    (1..=253).contains(&host.len())
        && !host.starts_with('-')
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Text from the network or Bonjour, safe to print: no control characters.
fn printable(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

fn send(stream: &mut TcpStream, text: &str) -> Result<(), String> {
    writeln!(stream, "{text}").map_err(|error| format!("cannot reach the other Mac ({error})"))
}

/// The other side's hello: role, host and its last field.
fn hello(text: &str) -> Result<(Role, String, String), String> {
    let rest = text.strip_prefix(HELLO).ok_or("the other side is not monitor-switch, or another version of it")?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    match fields.as_slice() {
        [_, host, _] if !valid_host(host) => Err("the other Mac sent a malformed host name".into()),
        [role, host, last] => Ok((parse_role(role).ok_or("the other Mac sent an unknown role")?, host.to_string(), last.to_string())),
        _ => Err("the other Mac sent a malformed greeting".into()),
    }
}

/// The terminal's lines, read by one thread, so the name typed while
/// looking and the answer to the code never go to the wrong reader.
fn typed_lines() -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    receiver
}

/// Asks a yes/no question on the terminal.
fn confirm(typed: &Receiver<String>, question: &str) -> bool {
    print!("{question} (y/n): ");
    let _ = std::io::stdout().flush();
    typed.recv().is_ok_and(|answer| answer.trim().to_lowercase().starts_with('y'))
}

/// Finds the other Mac, pairs with it and returns what it learned.
pub fn pair(me: &Me) -> Result<Peer, String> {
    let listener = TcpListener::bind(("::", PORT))
        .or_else(|_| TcpListener::bind(("0.0.0.0", PORT)))
        .map_err(|error| format!("cannot listen on port {PORT} ({error}); is setup already running?"))?;
    let short = me.host.trim_end_matches(".local");
    let instance = format!("monitor-switch {short}");
    let _advert = dns_sd(&["-R", &instance, SERVICE, "local", &PORT.to_string(), &format!("role={}", role_name(me.role))])?;
    let (events, waiting): (Sender<Event>, Receiver<Event>) = mpsc::channel();
    let (found, hosts) = mpsc::channel();
    let _browser = browse(instance, found)?;
    {
        let events = events.clone();
        thread::spawn(move || {
            for host in hosts {
                let _ = events.send(Event::Found(host));
            }
        });
    }
    {
        let events = events.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if stream.peer_addr().is_ok_and(|address| is_local(address.ip())) {
                    let _ = events.send(Event::Called(stream));
                }
            }
        });
    }
    drop(events);
    let typed = typed_lines();

    println!("  This Mac: {} ({})", me.host, crate::setup::mac_name(me.role));
    println!("  Looking for the other Mac: run `monitor-switch setup` on it too…");
    let started = Instant::now();
    let mut hinted = false;
    // A Mac this one calls, tried again after a failure: the other Mac may
    // have been busy with a call of its own, or not ready yet.
    let mut retry: Option<(String, bool, Instant)> = None;
    loop {
        // Without Bonjour the user can type the other Mac's name.
        if !hinted && started.elapsed() > Duration::from_secs(20) {
            hinted = true;
            println!("  Still looking. If the other Mac does not show up, type its name as its setup");
            println!("  shows it, then Enter (or check that both Macs are on the same network):");
        }
        let event = match waiting.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => match typed.try_recv() {
                Ok(name) if !name.trim().is_empty() => {
                    let name = name.trim().to_string();
                    let host = if name.contains('.') || name.contains(':') { name } else { format!("{name}.local") };
                    if !valid_host(&host) && host.parse::<IpAddr>().is_err() {
                        println!("  {} is not a host name; type it as its setup shows it", printable(&host));
                        continue;
                    }
                    Event::Typed(host)
                }
                _ => match &retry {
                    Some((host, typed_by_user, at)) if Instant::now() >= *at => {
                        let host = host.clone();
                        if *typed_by_user { Event::Typed(host) } else { Event::Found(host) }
                    }
                    _ => continue,
                },
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("pairing stopped".into()),
        };
        let mut attempt = |host: String, typed_by_user: bool| match call(me, &host, typed_by_user, &typed) {
            Ok(peer) => Some(peer),
            Err(error) => {
                println!("  {}: {error}; trying again in a moment", printable(&host));
                // Apart in time on the two Macs, so two calls do not keep
                // meeting each other.
                let jitter = u64::from(std::process::id() % 7) * 400;
                retry = Some((host, typed_by_user, Instant::now() + Duration::from_millis(2000 + jitter)));
                None
            }
        };
        match event {
            Event::Found(host) | Event::Typed(host) if host.eq_ignore_ascii_case(&me.host) => {}
            Event::Typed(host) => {
                if let Some(peer) = attempt(host, true) {
                    return Ok(peer);
                }
            }
            // The name that sorts first calls; the other one waits.
            Event::Found(host) if me.host.to_lowercase() < host.to_lowercase() => {
                if !valid_host(&host) {
                    continue;
                }
                if let Some(peer) = attempt(host, false) {
                    return Ok(peer);
                }
            }
            Event::Found(_) => {}
            Event::Called(stream) => match answer(me, stream, &typed) {
                Ok(peer) => return Ok(peer),
                Err(error) => println!("  {error}; still looking"),
            },
        }
    }
}

fn connect(host: &str) -> Result<TcpStream, String> {
    let addresses: Vec<SocketAddr> = (host, PORT)
        .to_socket_addrs()
        .map_err(|error| format!("cannot find it on the network ({error})"))?
        .collect();
    let mut last = String::from("no address");
    for address in addresses {
        match TcpStream::connect_timeout(&address, Duration::from_secs(4)) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error.to_string(),
        }
    }
    Err(format!("cannot connect ({last})"))
}

/// Calls `host`. A name the user typed is kept as the address to reach the
/// other Mac by: it is what works on this network.
fn call(me: &Me, host: &str, typed_by_user: bool, typed: &Receiver<String>) -> Result<Peer, String> {
    let mut stream = connect(host)?;
    let _ = stream.set_read_timeout(Some(HANDSHAKE));
    let mut reader = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
    let keys = KeyPair::new()?;
    let mine = format!("{HELLO} {} {} {}", role_name(me.role), me.host, to_hex(&sha256(&keys.public)));
    send(&mut stream, &mine)?;
    let greeting = line(&mut reader)?;
    let (role, their_host, theirs) = hello(&greeting)?;
    let theirs = from_hex(&theirs).ok_or("the other Mac sent a malformed key")?;
    send(&mut stream, &format!("key {}", to_hex(&keys.public)))?;
    let reach_by = if typed_by_user { host.to_string() } else { their_host };
    finish(me, (role, reach_by), &keys, (&keys.public, &theirs), (&mine, &greeting), (stream, reader), typed)
}

fn answer(me: &Me, mut stream: TcpStream, typed: &Receiver<String>) -> Result<Peer, String> {
    let _ = stream.set_read_timeout(Some(HANDSHAKE));
    let mut reader = BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
    let greeting = line(&mut reader)?;
    let (role, their_host, commitment) = hello(&greeting)?;
    let keys = KeyPair::new()?;
    let mine = format!("{HELLO} {} {} {}", role_name(me.role), me.host, to_hex(&keys.public));
    send(&mut stream, &mine)?;
    let reply = line(&mut reader)?;
    let theirs = reply.strip_prefix("key ").and_then(from_hex).ok_or("the other Mac sent a malformed key")?;
    let committed = from_hex(&commitment).unwrap_or_default();
    if !same(&sha256(&theirs), &committed) {
        return Err("the other Mac's key does not match what it promised".into());
    }
    finish(me, (role, their_host), &keys, (&theirs, &keys.public), (&greeting, &mine), (stream, reader), typed)
}

fn finish(
    me: &Me,
    (role, host): (Role, String),
    keys: &KeyPair,
    (caller_key, answer_key): (&[u8], &[u8]),
    (caller_hello, answer_hello): (&str, &str),
    (mut stream, mut reader): (TcpStream, BufReader<TcpStream>),
    typed: &Receiver<String>,
) -> Result<Peer, String> {
    if role == me.role {
        let _ = send(&mut stream, "no");
        let name = crate::setup::mac_name(role);
        return Err(format!(
            "both Macs are set up as the {name}: run `monitor-switch setup --role {}` on the other one",
            role_name(crate::agent::other_role(role))
        ));
    }
    let theirs = if caller_key == keys.public.as_slice() { answer_key } else { caller_key };
    let shared = keys.shared(theirs)?;
    let mut material = b"monitor-switch pairing 1".to_vec();
    material.extend_from_slice(&shared);
    material.extend_from_slice(caller_key);
    material.extend_from_slice(answer_key);
    for greeting in [caller_hello, answer_hello] {
        material.extend_from_slice(greeting.as_bytes());
        material.push(b'\n');
    }
    let secret = sha256(&material);
    // The user may take a while to compare the codes.
    let _ = stream.set_read_timeout(Some(CONFIRMATION));

    println!();
    println!("  Found {} ({}).", printable(&host), crate::setup::mac_name(role));
    println!();
    println!("      Code:  {}", code(&secret));
    println!();
    // Lines typed before the code was shown answer nothing.
    while typed.try_recv().is_ok() {}
    if !confirm(typed, "  Does the other Mac show the same code?") {
        let _ = send(&mut stream, "no");
        return Err("the codes did not match: nothing was saved".into());
    }
    let devices = encode_devices(&me.devices);
    send(&mut stream, &format!("ok {devices} {}", confirmation(&secret, me.role, &devices)))?;
    println!("  Waiting for the other Mac to confirm…");
    let reply = line(&mut reader)?;
    let fields: Vec<&str> = reply.split(' ').collect();
    let their_devices = match fields.as_slice() {
        ["ok", list, mac] if same(mac.as_bytes(), confirmation(&secret, role, list).as_bytes()) => {
            decode_devices(list).ok_or("the other Mac sent a malformed device list")?
        }
        ["no"] => return Err("the other Mac did not confirm the code: nothing was saved".into()),
        _ => return Err("the other Mac's confirmation is not genuine: nothing was saved".into()),
    };
    Ok(Peer { role, host, devices: their_devices, secret: to_hex(&secret) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_lists_round_trip() {
        let devices = vec![
            Device { address: "aa-bb-cc-dd-ee-01".into(), kind: 0, name: "Daniiar’s Magic Keyboard".into() },
            Device { address: "aa-bb-cc-dd-ee-02".into(), kind: 1, name: "Mouse, with/odd chars".into() },
        ];
        assert_eq!(decode_devices(&encode_devices(&devices)).unwrap(), devices);
        assert_eq!(decode_devices("-").unwrap(), Vec::new());
        assert!(decode_devices("zz/0/00").is_none());
    }

    #[test]
    fn codes_have_six_digits() {
        assert_eq!(code(&[1; 32]).len(), 6);
        assert_ne!(code(&[1; 32]), code(&[2; 32]));
    }

    #[test]
    fn reads_the_greeting() {
        let (role, host, last) = hello("monitor-switch-pair 1 mini a.local ff").unwrap();
        assert_eq!((role, host.as_str(), last.as_str()), (Role::Mini, "a.local", "ff"));
        assert!(hello("monitor-switch-pair 2 mini a.local ff").is_err());
        assert!(hello("monitor-switch-pair 1 laptop a.local ff").is_err());
        // A host that could carry terminal control sequences or a comment.
        assert!(hello("monitor-switch-pair 1 mini a.local#\u{1b}[8m ff").is_err());
        assert!(hello("monitor-switch-pair 1 mini -a.local ff").is_err());
    }
}
