//! The command-line side: passes one command to the local agent over its
//! Unix socket and relays the answer.
//!
//! The agent answers with any number of lines and a last line that is either
//! `ok` or `error: <reason>`. Anything else, including the agent going away
//! mid-answer, is a failure: success is never inferred from silence.

use crate::config;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Commands one Mac may send the other (see `server`).
pub const PEER_COMMANDS: [&str; 5] = ["ping", "held", "status", "arrive", "handover"];

/// How long to wait for the agent's answer to `command`.
pub fn timeout(command: &str) -> Duration {
    match command {
        "ping" | "held" => Duration::from_secs(5),
        "status" => Duration::from_secs(30),
        // Pairing the devices: up to 45 s each plus the contact spacing.
        "arrive" => Duration::from_secs(150),
        // A hand-over at its worst: settle 3 s, ping 5 s, release 6 s, the
        // peer's `arrive` 150 s, `held` 5 s and taking back two devices
        // (2 x 45 s + 6 s) — about 265 s, plus a margin.
        _ => Duration::from_secs(300),
    }
}

/// Sends `command` and returns the lines before the final `ok`.
pub fn request(command: &str) -> Result<Vec<String>, String> {
    let mut stream = UnixStream::connect(config::socket_path())
        .map_err(|error| format!("the agent is not running ({error})"))?;
    stream
        .set_read_timeout(Some(timeout(command)))
        .map_err(|error| error.to_string())?;
    writeln!(stream, "{command}").map_err(|error| error.to_string())?;
    let mut lines = Vec::new();
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "the agent did not answer in time".to_string()
            }
            _ => error.to_string(),
        })?;
        lines.push(line);
    }
    finish(lines)
}

/// Splits the answer into its body and verdict.
fn finish(mut lines: Vec<String>) -> Result<Vec<String>, String> {
    match lines.pop().as_deref() {
        Some("ok") => Ok(lines),
        Some(last) => match last.strip_prefix("error: ") {
            Some(reason) => Err(reason.to_string()),
            None => Err("the agent stopped before finishing its answer".into()),
        },
        None => Err("the agent closed the connection without an answer".into()),
    }
}

/// Runs a command from the terminal, printing what the agent says.
pub fn run(command: &str) -> Result<(), String> {
    let lines = request(command)?;
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn only_a_final_ok_is_success() {
        assert_eq!(finish(lines(&["a", "ok"])).unwrap(), ["a"]);
        assert_eq!(finish(lines(&["a", "error: busy"])).unwrap_err(), "busy");
        assert!(finish(lines(&["a", "took: x"])).is_err());
        assert!(finish(Vec::new()).is_err());
    }
}
