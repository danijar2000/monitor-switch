//! `monitor-switch setup`: runs the installer, which sets up both Macs.
//!
//! The installer (`install.sh` with the binaries, scripts and launchd files
//! beside it) lives in `~/.monitor-switch/installer`, where `install.conf`
//! is kept between updates. When this program is part of a package that
//! carries a newer copy (Homebrew puts one beside it), that copy replaces
//! the installer first; this is how `brew upgrade` reaches both Macs.

use crate::config;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The package this program was started from, if it carries an installer:
/// the directory above the `bin` it is in.
fn packaged(installer: &Path) -> Option<PathBuf> {
    let program = std::env::current_exe().ok()?.canonicalize().ok()?;
    let package = program.parent()?.parent()?;
    let carries = package.join("install.sh").is_file() && package.join("bin/monitor-switch-hotkey").is_file();
    let same = installer.canonicalize().is_ok_and(|installer| installer == package);
    (carries && !same).then(|| package.to_path_buf())
}

/// Puts a copy of `package` in place of the installer, keeping `install.conf`.
/// The old installer is deleted only once the new one is in place.
fn replace(package: &Path, installer: &Path) -> Result<(), String> {
    let state = config::dir();
    std::fs::create_dir_all(&state).map_err(|error| format!("cannot create {}: {error}", state.display()))?;
    let _ = std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700));
    let (new, old) = (state.join("installer.new"), state.join("installer.old"));
    let _ = std::fs::remove_dir_all(&new);
    let _ = std::fs::remove_dir_all(&old);
    let copied = Command::new("/bin/cp").arg("-R").arg(package).arg(&new).status();
    if !copied.is_ok_and(|status| status.success()) {
        return Err(format!("cannot copy {} to {}", package.display(), new.display()));
    }
    // A package may be read-only; the copy is replaced on the next update.
    let _ = Command::new("/bin/chmod").args(["-R", "u+w"]).arg(&new).status();
    let kept = installer.join("install.conf");
    if kept.is_file() {
        std::fs::copy(&kept, new.join("install.conf")).map_err(|error| format!("cannot keep install.conf: {error}"))?;
    }
    let had = std::fs::rename(installer, &old).is_ok();
    if let Err(error) = std::fs::rename(&new, installer) {
        if had {
            let _ = std::fs::rename(&old, installer);
        }
        return Err(format!("cannot put the installer into {}: {error}", installer.display()));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Runs the installer with `args`; returns only if it could not be started.
pub fn run(args: &[&str]) -> Result<(), String> {
    let installer = config::dir().join("installer");
    if let Some(package) = packaged(&installer) {
        replace(&package, &installer)?;
    }
    let script = installer.join("install.sh");
    if !script.is_file() {
        return Err(format!(
            "no installer in {}: install with Homebrew, or run\n  \
             curl -fsSL https://github.com/danijar2000/monitor-switch/releases/latest/download/get.sh | zsh",
            installer.display()
        ));
    }
    let error = Command::new("/bin/zsh").arg(&script).args(args).exec();
    Err(format!("cannot run {}: {error}", script.display()))
}
