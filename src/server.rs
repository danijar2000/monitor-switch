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
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// The port both agents listen on.
pub const PORT: u16 = 47811;

/// Longest request head accepted.
const MAX_HEAD: u64 = 4096;

const MAX_CALLS: usize = 8;

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

fn read_head(stream: &TcpStream) -> Option<Head> {
    let mut reader = BufReader::new(stream.try_clone().ok()?.take(MAX_HEAD));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "POST" {
        return None;
    }
    let command = parts.next()?.strip_prefix('/')?.to_string();
    let (mut time, mut nonce, mut signature) = (None, None, None);
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
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

fn respond(stream: &mut TcpStream, status: &str, lines: &[String]) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
    );
    for line in lines {
        let _ = writeln!(stream, "{line}");
    }
}

fn handle(mut stream: TcpStream, secret: &[u8]) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Some(head) = read_head(&stream) else {
        respond(
            &mut stream,
            "400 Bad Request",
            &["error: bad request".into()],
        );
        return;
    };
    let Some(command) = PEER_COMMANDS
        .into_iter()
        .find(|allowed| *allowed == head.command)
    else {
        respond(
            &mut stream,
            "404 Not Found",
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
            "403 Forbidden",
            &[format!("error: refused: {reason}")],
        );
        return;
    }
    // The answer may take minutes (`arrive`); the caller has its own deadline.
    let _ = stream.set_write_timeout(Some(client::timeout(command)));
    let lines = match client::request(command) {
        Ok(mut lines) => {
            lines.push("ok".into());
            lines
        }
        Err(reason) => vec![format!("error: {reason}")],
    };
    respond(&mut stream, "200 OK", &lines);
}

#[cfg(test)]
mod tests {
    use super::is_local;

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
