//! Tells whether a Bluetooth device is really usable as an input device.
//!
//! `IOBluetoothDevice.isConnected` only reports the radio link. A Magic Mouse
//! can hold that link for fifteen seconds without ever attaching as a mouse,
//! so the check that counts is whether a HID event driver exists for it.

use crate::iokit::{find_service, number_property};

/// `address` is "aa-bb-cc-dd-ee-02"; HID services carry it as their serial
/// number in the form "AA:BB:CC:DD:EE:02".
pub fn is_live(address: &str) -> bool {
    find_service("IOHIDEventService", "SerialNumber", &address.replace('-', ":"), |_| true)
}

/// The charge of a device attached to this Mac, in percent. Only one of a
/// device's HID services carries it; `None` while the device is elsewhere or
/// has not reported yet.
pub fn battery(address: &str) -> Option<u8> {
    let mut percent = None;
    find_service("IOHIDEventService", "SerialNumber", &address.replace('-', ":"), |entry| {
        percent = number_property(entry, "BatteryPercent").and_then(|value| u8::try_from(value).ok());
        percent.is_some()
    });
    percent.filter(|percent| *percent <= 100)
}
