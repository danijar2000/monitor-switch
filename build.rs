//! Embeds an Info.plist in the `monitor-switch` binary: macOS refuses
//! Bluetooth access to a process whose bundle has no
//! NSBluetoothAlwaysUsageDescription. The plist is generated so its version
//! always matches Cargo.toml.

use std::{env, fs, path::PathBuf};

fn main() {
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>local.monitor-switch</string>
    <key>CFBundleName</key>
    <string>monitor-switch</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
    <key>NSBluetoothAlwaysUsageDescription</key>
    <string>monitor-switch moves the keyboard and mouse to the Mac shown on the monitor.</string>
    <key>NSLocalNetworkUsageDescription</key>
    <string>monitor-switch talks to your other Mac over the home network to hand over the keyboard, mouse and monitor.</string>
</dict>
</plist>
"#
    );
    let path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("Info.plist");
    fs::write(&path, plist).unwrap();
    // Separate -Xlinker arguments: a path with commas or spaces cannot break
    // them apart the way it would break a single -Wl,... argument.
    for arg in ["-sectcreate", "__TEXT", "__info_plist", path.to_str().unwrap()] {
        println!("cargo:rustc-link-arg-bin=monitor-switch=-Xlinker");
        println!("cargo:rustc-link-arg-bin=monitor-switch={arg}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
