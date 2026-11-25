//! Calls to the other Mac's agent over HTTP on the home network (see
//! `server` for the other end and `auth` for the signature).
//!
//! Every call has a deadline: the two agents call each other, and a call that
//! waited forever would freeze the agent that made it. The call itself runs
//! on its own thread; waiting on the main thread keeps the Bluetooth run loop
//! turning, because IOBluetooth delivers its callbacks there.

use crate::{auth, bt, server};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Extra time past the remote command's own timeout, so that the remote
/// side gives up first and its verdict (and `took:` line) still arrives.
const MARGIN: Duration = Duration::from_secs(10);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// A call that did not succeed: what the other Mac sent before it stopped,
/// and why it stopped.
#[derive(Debug)]
pub struct Failure {
    pub stdout: String,
    pub reason: String,
}

/// A call in flight; `wait` collects it.
pub struct Call {
    worker: JoinHandle<Result<String, Failure>>,
    deadline: Instant,
}

/// Sends `command` to the agent at `peer` (a host name such as
/// `other-mac.local`, optionally with `:port`); `timeout` is how long that
/// command may take there.
pub fn start(peer: &str, command: &str, timeout: Duration) -> Result<Call, Failure> {
    let secret = auth::load_secret().map_err(|reason| Failure {
        stdout: String::new(),
        reason,
    })?;
    let (peer, command) = (peer.to_string(), command.to_string());
    let worker = thread::spawn(move || exchange(&peer, &command, &secret, timeout + MARGIN));
    Ok(Call {
        worker,
        deadline: Instant::now() + timeout + MARGIN + CONNECT_TIMEOUT,
    })
}

fn fail(reason: impl Into<String>) -> Failure {
    Failure {
        stdout: String::new(),
        reason: reason.into(),
    }
}

fn addresses(peer: &str) -> Result<Vec<SocketAddr>, Failure> {
    let resolved = match peer.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => {
            (host, port.parse::<u16>().unwrap()).to_socket_addrs()
        }
        _ => (peer, server::PORT).to_socket_addrs(),
    };
    let addresses: Vec<_> = resolved
        .map_err(|error| fail(format!("cannot find {peer} on the network ({error})")))?
        .collect();
    if addresses.is_empty() {
        return Err(fail(format!("cannot find {peer} on the network")));
    }
    Ok(addresses)
}

fn connect(peer: &str) -> Result<TcpStream, Failure> {
    let mut last = String::new();
    for address in addresses(peer)? {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = format!("{address}: {error}"),
        }
    }
    Err(fail(format!(
        "cannot connect to {peer} ({last}); is its agent running?"
    )))
}

fn exchange(peer: &str, command: &str, secret: &[u8], limit: Duration) -> Result<String, Failure> {
    let mut stream = connect(peer)?;
    let _ = stream.set_read_timeout(Some(limit));
    let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
    let time = auth::now();
    let nonce = auth::nonce();
    let signature = auth::sign(secret, command, time, &nonce);
    write!(
        stream,
        "POST /{command} HTTP/1.1\r\nHost: {peer}\r\nX-Time: {time}\r\nX-Nonce: {nonce}\r\n\
         X-Signature: {signature}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| fail(format!("cannot send to {peer}: {error}")))?;
    let mut answer = String::new();
    let read = stream.take(1 << 20).read_to_string(&mut answer);
    let body = answer
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
        .trim()
        .to_string();
    match read {
        Err(error) if body.is_empty() => Err(fail(format!("no answer from {peer}: {error}"))),
        _ => verdict(body),
    }
}

/// `Ok` only when the last line is `ok`; the body is kept either way.
fn verdict(body: String) -> Result<String, Failure> {
    let last = body.lines().last().unwrap_or("").to_string();
    if last == "ok" {
        return Ok(body);
    }
    let reason = match last.strip_prefix("error: ") {
        Some(reason) => reason.to_string(),
        None if body.is_empty() => "the other Mac closed the connection without an answer".into(),
        None => "the other Mac stopped before finishing its answer".into(),
    };
    Err(Failure {
        stdout: body,
        reason,
    })
}

impl Call {
    /// Waits for the call; `Ok` carries what the other Mac answered.
    pub fn wait(self) -> Result<String, Failure> {
        while !self.worker.is_finished() {
            if Instant::now() >= self.deadline {
                // The worker ends on its own read timeout.
                return Err(fail("no answer in time"));
            }
            if is_main_thread() {
                bt::spin(0.1);
            } else {
                thread::sleep(Duration::from_millis(100));
            }
        }
        self.worker
            .join()
            .unwrap_or_else(|_| Err(fail("internal error in the call")))
    }
}

fn is_main_thread() -> bool {
    extern "C" {
        fn pthread_main_np() -> std::ffi::c_int;
    }
    // SAFETY: pthread_main_np has no preconditions.
    unsafe { pthread_main_np() == 1 }
}

/// Starts a call and waits for it.
pub fn call(peer: &str, command: &str, timeout: Duration) -> Result<String, Failure> {
    start(peer, command, timeout)?.wait()
}

#[cfg(test)]
mod tests {
    use super::verdict;

    #[test]
    fn only_a_final_ok_is_success() {
        assert_eq!(verdict("a\nok".into()).unwrap(), "a\nok");
        let failure = verdict("took: x\nerror: busy".into()).unwrap_err();
        assert_eq!(failure.reason, "busy");
        assert_eq!(failure.stdout, "took: x\nerror: busy");
        assert!(verdict("a".into()).is_err());
        assert!(verdict(String::new()).is_err());
    }
}
