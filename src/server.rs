//! The agent's HTTP endpoint for the other Mac.
//!
//! One call is `POST /<command>` with the headers `X-Time`, `X-Nonce` and
//! `X-Signature` (see `auth`). A call that passes is handed to this agent's
//! own Unix socket, exactly as a local client would send it, and the answer
//! lines are sent back as the body, ending with the verdict line (`ok` or
//! `error: …`). Only the peer commands are accepted, and only from private
//! or link-local addresses.

use crate::auth;
use crate::client::{self, PEER_COMMANDS};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The port both agents listen on.
pub const PORT: u16 = 47811;

/// Longest request head accepted.
const MAX_HEAD: u64 = 4096;

const MAX_CALLS: usize = 8;

/// The whole request head must arrive within this time, so a caller that
/// sends it a byte at a time cannot hold a slot.
const HEAD_DEADLINE: Duration = Duration::from_secs(5);

/// Listens on every interface, IPv4 and IPv6.
pub fn listen() -> Result<TcpListener, String> {
    TcpListener::bind(("::", PORT))
        .or_else(|_| TcpListener::bind(("0.0.0.0", PORT)))
        .map_err(|error| format!("cannot listen on port {PORT}: {error}"))
}

pub fn serve(listener: TcpListener, secret: Vec<u8>) {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let secret: &'static [u8] = Box::leak(secret.into_boxed_slice());
    for stream in listener.incoming().flatten() {
        if !stream
            .peer_addr()
            .is_ok_and(|address| is_local(address.ip()))
        {
            continue;
        }
        if CALLS.fetch_add(1, Ordering::SeqCst) >= MAX_CALLS {
            CALLS.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        thread::spawn(move || {
            handle(stream, secret);
            CALLS.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Home-network addresses only: private IPv4, IPv6 link-local and unique
/// local, and loopback.
fn is_local(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        other => other,
    };
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xffc0 == 0xfe80 || first & 0xfe00 == 0xfc00
        }
    }
}

struct Head {
    command: String,
    time: u64,
    nonce: String,
    signature: String,
}

/// Reads up to the blank line that ends the head, within `HEAD_DEADLINE`.
fn read_head_text(stream: &mut TcpStream) -> Option<String> {
    let deadline = Instant::now() + HEAD_DEADLINE;
    let mut head = Vec::new();
    let mut chunk = [0u8; 512];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let left = deadline.checked_duration_since(Instant::now())?;
        if left.is_zero() || head.len() as u64 >= MAX_HEAD {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(head).ok()
}

fn parse_head(text: &str) -> Option<Head> {
    let mut lines = text.split("\r\n");
    let mut parts = lines.next()?.split_whitespace();
    if parts.next()? != "POST" {
        return None;
    }
    let command = parts.next()?.strip_prefix('/')?.to_string();
    let (mut time, mut nonce, mut signature) = (None, None, None);
    for header in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = header.split_once(':')?;
        let value = value.trim().to_string();
        match name.trim().to_ascii_lowercase().as_str() {
            "x-time" => time = value.parse().ok(),
            "x-nonce" => nonce = Some(value),
            "x-signature" => signature = Some(value),
            _ => {}
        }
    }
    Some(Head {
        command,
        time: time?,
        nonce: nonce?,
        signature: signature?,
    })
}

/// Sends the answer, signed over the caller's nonce (see `auth`).
fn respond(stream: &mut TcpStream, secret: &[u8], nonce: &str, status: (u16, &str), lines: &[String]) {
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    let signature = auth::sign_answer(secret, nonce, status.0, body.trim());
    let _ = write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nX-Signature: {signature}\r\n\
         Connection: close\r\n\r\n{body}",
        status.0, status.1
    );
}

fn handle(mut stream: TcpStream, secret: &[u8]) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Some(head) = read_head_text(&mut stream).and_then(|text| parse_head(&text)) else {
        respond(&mut stream, secret, "", (400, "Bad Request"), &["error: bad request".into()]);
        return;
    };
    let Some(command) = PEER_COMMANDS
        .into_iter()
        .find(|allowed| *allowed == head.command)
    else {
        respond(
            &mut stream,
            secret,
            &head.nonce,
            (404, "Not Found"),
            &[format!("error: command not allowed: {:?}", head.command)],
        );
        return;
    };
    if let Err(reason) = auth::verify(secret, command, head.time, &head.nonce, &head.signature) {
        let from = stream
            .peer_addr()
            .map(|address| address.ip().to_string())
            .unwrap_or_default();
        eprintln!("refused a call from {from}: {reason}");
        respond(
            &mut stream,
            secret,
            &head.nonce,
            (403, "Forbidden"),
            &[format!("error: refused: {reason}")],
        );
        return;
    }
    // The answer may take minutes (`arrive`); the caller has its own deadline.
    let _ = stream.set_write_timeout(Some(client::timeout(command)));
    // The lines are passed on as they are, so a failed `arrive` still tells
    // the caller (in its `took:` line) which devices it got.
    let lines = match client::answer(command) {
        Ok(lines) if lines.last().is_some_and(|last| last == "ok" || last.starts_with("error: ")) => lines,
        Ok(mut lines) => {
            lines.push("error: the agent stopped before finishing its answer".into());
            lines
        }
        Err(reason) => vec![format!("error: {reason}")],
    };
    respond(&mut stream, secret, &head.nonce, (200, "OK"), &lines);
}

#[cfg(test)]
mod tests {
    use super::{is_local, parse_head};

    #[test]
    fn reads_the_head() {
        let head = parse_head("POST /arrive HTTP/1.1\r\nX-Time: 5\r\nX-Nonce: n\r\nx-signature: s\r\n\r\n").unwrap();
        assert_eq!((head.command.as_str(), head.time, head.nonce.as_str(), head.signature.as_str()), ("arrive", 5, "n", "s"));
        assert!(parse_head("GET /arrive HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_head("POST /arrive HTTP/1.1\r\nX-Time: 5\r\n\r\n").is_none());
    }

    #[test]
    fn only_home_addresses_are_local() {
        for ip in [
            "192.168.31.5",
            "10.0.0.2",
            "172.16.4.4",
            "169.254.1.1",
            "127.0.0.1",
            "fe80::1",
            "fd00::5",
            "::ffff:192.168.1.2",
            "::1",
        ] {
            assert!(is_local(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2a00:1450::1", "::ffff:8.8.8.8"] {
            assert!(!is_local(ip.parse().unwrap()), "{ip}");
        }
    }
}
