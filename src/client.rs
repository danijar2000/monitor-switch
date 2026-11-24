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

/// Commands one Mac sends the other through `ssh-gate`.
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

/// The forced command of the ssh key the other Mac logs in with: it may run
/// only the peer commands, nothing else on this Mac.
pub fn ssh_gate() -> Result<(), String> {
    let original = std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default();
    let command = gate_command(&original).ok_or(format!("command not allowed: {original:?}"))?;
    // Unlike `run`, print the verdict line too: the caller parses it.
    match request(command) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            println!("ok");
            Ok(())
        }
        Err(reason) => {
            println!("error: {reason}");
            Err(reason)
        }
    }
}

/// `~/bin/monitor-switch arrive` -> `arrive`, if it is a peer command.
fn gate_command(original: &str) -> Option<&'static str> {
    let mut words = original.split_whitespace();
    let program = words.next()?;
    let command = words.next()?;
    if words.next().is_some() || !program.ends_with("monitor-switch") {
        return None;
    }
    PEER_COMMANDS.into_iter().find(|allowed| *allowed == command)
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

    #[test]
    fn the_gate_lets_through_only_peer_commands() {
        assert_eq!(gate_command("~/bin/monitor-switch arrive"), Some("arrive"));
        assert_eq!(gate_command("/Users/x/bin/monitor-switch ping"), Some("ping"));
        assert_eq!(gate_command("~/bin/monitor-switch switch"), None);
        assert_eq!(gate_command("~/bin/monitor-switch ping; rm -rf ~"), None);
        assert_eq!(gate_command("sh -c ping"), None);
        assert_eq!(gate_command(""), None);
    }
}
