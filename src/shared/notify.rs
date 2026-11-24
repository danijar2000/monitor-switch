//! A macOS notification, used when a switch fails and nothing changes on
//! screen. Included by both binaries (`#[path]`), which share no library.

use std::process::{Command, Stdio};
use std::thread;

/// Shows `message` without blocking the caller. The text reaches AppleScript
/// as an argument, never as script source, so it needs no escaping.
pub fn notify(message: &str) {
    let mut command = Command::new("osascript");
    command
        .args(["-e", "on run argv"])
        .args(["-e", "display notification (item 1 of argv) with title \"monitor-switch\""])
        .args(["-e", "end run"])
        .arg(message)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
