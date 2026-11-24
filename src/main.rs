mod agent;
mod bt;
mod client;
mod config;
mod ddc;
mod hid;
#[path = "shared/home.rs"]
mod home;
mod iokit;
#[path = "shared/logfile.rs"]
mod logfile;
#[path = "shared/notify.rs"]
mod notify;
mod peer;

use std::process::ExitCode;

const USAGE: &str = "\
monitor-switch — moves an MSI monitor, Magic Keyboard and Magic Mouse between two Macs

  monitor-switch switch    move the monitor, keyboard and mouse to the other Mac
  monitor-switch status    what this Mac's agent sees
  monitor-switch agent     run the background agent (launchd starts it)

Internal: ping, held, arrive, handover (one Mac to the other, through ssh-gate) and
ssh-gate (the forced command of the other Mac's ssh key). The switch is bound
to a hotkey by monitor-switch-hotkey, Control+Enter by default.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["agent"] => return run_agent(),
        ["ssh-gate"] => client::ssh_gate(),
        [command @ ("switch" | "status" | "ping" | "held" | "arrive" | "handover")] => client::run(command),
        ["help" | "--help" | "-h"] => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("monitor-switch: {message}");
            ExitCode::FAILURE
        }
    }
}

/// launchd restarts the agent only after a failed exit (`SuccessfulExit` is
/// false): a problem a restart cannot fix ends with success, so it does not
/// turn into a restart loop.
fn run_agent() -> ExitCode {
    match agent::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(agent::StartError::Permanent(message)) => {
            eprintln!("monitor-switch agent: {message}; not restarting");
            ExitCode::SUCCESS
        }
        Err(agent::StartError::Transient(message)) => {
            eprintln!("monitor-switch agent: {message}");
            ExitCode::FAILURE
        }
    }
}
