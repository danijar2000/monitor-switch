//! Tells whether a Bluetooth device is really usable as an input device.
//!
//! `IOBluetoothDevice.isConnected` only reports the radio link. A Magic Mouse
//! can hold that link for fifteen seconds without ever attaching as a mouse,
//! so the check that counts is whether a HID event driver exists for it.

use crate::iokit::find_service;

/// `address` is "aa-bb-cc-dd-ee-02"; HID services carry it as their serial
/// number in the form "AA:BB:CC:DD:EE:02".
pub fn is_live(address: &str) -> bool {
    find_service("IOHIDEventService", "SerialNumber", &address.replace('-', ":"), |_| true)
}
