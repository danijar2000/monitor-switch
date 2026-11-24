//! Keeps a launchd log file bounded. launchd opens the file for appending
//! and hands it to the process as stderr, so it cannot be renamed under the
//! process; at startup an oversized log is copied to `<name>.1` and emptied.

use std::fs::{self, OpenOptions};
use std::path::Path;

const LIMIT: u64 = 512 * 1024;

pub fn rotate(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() <= LIMIT {
        return;
    }
    let mut old = path.as_os_str().to_owned();
    old.push(".1");
    if fs::copy(path, &old).is_ok() {
        if let Ok(file) = OpenOptions::new().write(true).open(path) {
            // The descriptor is in append mode: later writes land at the new end.
            let _ = file.set_len(0);
        }
    }
}
