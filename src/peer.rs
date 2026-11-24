//! Calls to the other Mac over ssh.
//!
//! Every call has a deadline: the two agents call each other, and a call that
//! waited forever would freeze the agent that made it. Waiting on the main
//! thread keeps the Bluetooth run loop turning, because IOBluetooth delivers
//! its callbacks there; any other thread just sleeps.

use crate::bt;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Extra time past the remote command's own timeout, so that the remote
/// side gives up first and its verdict (and `took:` line) still arrives.
const MARGIN: Duration = Duration::from_secs(10);

/// A call that did not succeed: what the remote command printed before it
/// stopped, and why it stopped.
#[derive(Debug)]
pub struct Failure {
    pub stdout: String,
    pub reason: String,
}

/// A call in flight; `wait` collects it.
pub struct Call {
    child: Child,
    stdout: JoinHandle<String>,
    stderr: JoinHandle<String>,
    deadline: Instant,
}

/// Starts `~/bin/monitor-switch <command>` on the peer; `timeout` is the
/// timeout the peer's own client uses for that command.
pub fn start(peer: &str, command: &str, timeout: Duration) -> Result<Call, Failure> {
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=4"])
        .args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"])
        // The peer's host key must already be in known_hosts (see README);
        // a new or changed one is refused, never accepted silently.
        .args(["-o", "StrictHostKeyChecking=yes"])
        // The agent's own key, which the other Mac lets run only the peer
        // commands; never the user's keys or agent.
        .arg("-i")
        .arg(crate::config::dir().join("peer_key"))
        .args(["-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none"])
        .arg("--")
        .arg(peer)
        .arg(format!("~/bin/monitor-switch {command}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Failure { stdout: String::new(), reason: format!("cannot run ssh: {error}") })?;
    // Drain both pipes on their own threads: a child that fills a pipe while
    // nobody reads it would never exit.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    Ok(Call { child, stdout, stderr, deadline: Instant::now() + timeout + MARGIN })
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    })
}

impl Call {
    /// Waits for the call; `Ok` carries what the remote command printed.
    pub fn wait(mut self) -> Result<String, Failure> {
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() >= self.deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break None;
                }
                Ok(None) if is_main_thread() => bt::spin(0.1),
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(_) => break None,
            }
        };
        let stdout = self.stdout.join().unwrap_or_default().trim().to_string();
        let stderr = self.stderr.join().unwrap_or_default().trim().to_string();
        match status {
            Some(status) if status.success() => Ok(stdout),
            Some(status) => {
                let reason = [stderr, stdout.lines().last().unwrap_or("").to_string()]
                    .into_iter()
                    .find(|text| !text.is_empty())
                    .unwrap_or_else(|| format!("ssh exited with {status}"));
                Err(Failure { stdout, reason })
            }
            None => Err(Failure { stdout, reason: "no answer in time".into() }),
        }
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
