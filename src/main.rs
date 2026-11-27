mod agent;
#[macro_use]
mod appkit;
mod auth;
mod bt;
mod client;
mod config;
mod ddc;
mod hid;
mod hud;
#[path = "shared/home.rs"]
mod home;
mod iokit;
#[path = "shared/logfile.rs"]
mod logfile;
mod menu;
mod peer;
mod server;
mod setup;

use std::process::ExitCode;

const USAGE: &str = "\
monitor-switch — share one monitor, keyboard and mouse between two Macs

  monitor-switch switch    move the monitor, keyboard and mouse to the other Mac
  monitor-switch monitor   move only the monitor to the other Mac, or back
  monitor-switch status    what this Mac's agent sees
  monitor-switch overview  where the monitor, keyboard and mouse are
  monitor-switch reach     check that this Mac's agent reaches the other one
  monitor-switch setup     install or update on both Macs (also: setup check,
                           setup uninstall [--purge])
  monitor-switch agent     run the background agent (launchd starts it)

Internal: ping, held, state, wake, arrive, handover (one Mac's agent to the
other's, over HTTP on port 47811, signed with ~/.monitor-switch/peer_secret);
hud and menu (the panel and the menu bar icon, started by the agent). The
hotkeys are bound by monitor-switch-hotkey: Control+Enter switches,
Control+Shift+Enter moves only the monitor.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["agent"] => return run_agent(),
        // The progress panel; the agent starts it for the length of a switch.
        ["hud"] => {
            hud::run();
            return ExitCode::SUCCESS;
        }
        // The menu bar icon; the agent keeps it running.
        ["menu"] => {
            menu::run();
            return ExitCode::SUCCESS;
        }
        ["setup", rest @ ..] => setup::run(rest),
        [command @ ("switch" | "monitor" | "status" | "overview" | "reach" | "ping" | "held" | "state" | "wake"
        | "arrive" | "handover")] => client::run(command),
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
