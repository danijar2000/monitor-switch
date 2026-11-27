//! DDC/CI over the display cable, through the private IOAVService API that
//! Apple Silicon Macs expose for external displays.

use crate::iokit::{find_service, CFRelease, CFTypeRef};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

/// 7-bit I2C address of the DDC/CI device in every monitor.
const DDC_CHIP: u32 = 0x37;
/// "Host" source address that goes into every DDC/CI packet.
const DDC_HOST: u32 = 0x51;
/// 8-bit write address of the monitor, the seed of the request checksum.
const DDC_DEST: u8 = 0x6E;
/// Virtual host address the monitor answers to, the seed of the reply checksum.
const DDC_REPLY_DEST: u8 = 0x50;
/// Length byte of a "get VCP feature" reply: 8 data bytes, high bit set.
const GET_REPLY_LENGTH: u8 = 0x88;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOAVServiceCreateWithService(allocator: CFTypeRef, service: u32) -> CFTypeRef;
    fn IOAVServiceReadI2C(svc: CFTypeRef, chip: u32, offset: u32, buf: *mut u8, len: u32) -> i32;
    fn IOAVServiceWriteI2C(svc: CFTypeRef, chip: u32, offset: u32, buf: *const u8, len: u32)
        -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
}

/// True while this Mac's display is asleep and sends no picture.
pub fn is_asleep() -> bool {
    // SAFETY: both calls take no pointers and have no preconditions.
    unsafe { CGDisplayIsAsleep(CGMainDisplayID()) != 0 }
}

/// One exchange with the monitor at a time: the main loop and the thread
/// that answers `state` both talk to it.
static BUS: Mutex<()> = Mutex::new(());

fn bus() -> std::sync::MutexGuard<'static, ()> {
    BUS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reply to a "get VCP feature" request.
pub enum Reply {
    Value { current: u16 },
    Unsupported,
    NoAnswer,
}

/// The external display of this Mac.
pub struct Display {
    service: CFTypeRef,
}

impl Display {
    /// Finds the AV service of the external display (the built-in panel of a
    /// MacBook has `Location = Embedded` and is skipped).
    pub fn external() -> Option<Display> {
        let mut found = None;
        find_service("DCPAVServiceProxy", "Location", "External", |entry| {
            let service = unsafe { IOAVServiceCreateWithService(std::ptr::null(), entry) };
            if !service.is_null() {
                found = Some(Display { service });
            }
            found.is_some()
        });
        found
    }

    /// Reads a VCP feature, e.g. 0x60 (input source) or 0x10 (brightness).
    /// The I2C timing pauses are tens of milliseconds, short enough not to
    /// matter to the Bluetooth run loop of the calling thread.
    pub fn get(&self, vcp: u8) -> Reply {
        let _bus = bus();
        let pause = Duration::from_millis(80);
        for _ in 0..3 {
            let mut request = [0x82, 0x01, vcp, 0];
            request[3] = checksum(DDC_DEST ^ DDC_HOST as u8, &request[..3]);
            let mut reply = [0u8; 12];
            unsafe {
                if IOAVServiceWriteI2C(self.service, DDC_CHIP, DDC_HOST, request.as_ptr(), 4) != 0 {
                    sleep(pause);
                    continue;
                }
                sleep(Duration::from_millis(60));
                if IOAVServiceReadI2C(self.service, DDC_CHIP, DDC_HOST, reply.as_mut_ptr(), 12) != 0
                {
                    sleep(pause);
                    continue;
                }
            }
            if let Some(answer) = parse_reply(&reply, vcp) {
                return answer;
            }
            sleep(pause);
        }
        Reply::NoAnswer
    }

    /// Writes a VCP feature. Returns false if the I2C write itself failed;
    /// the monitor does not acknowledge the command either way.
    pub fn set(&self, vcp: u8, value: u16) -> bool {
        let _bus = bus();
        let [hi, lo] = value.to_be_bytes();
        let mut packet = [0x84, 0x03, vcp, hi, lo, 0];
        packet[5] = checksum(DDC_DEST ^ DDC_HOST as u8, &packet[..5]);
        unsafe { IOAVServiceWriteI2C(self.service, DDC_CHIP, DDC_HOST, packet.as_ptr(), 6) == 0 }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        unsafe { CFRelease(self.service) }
    }
}

fn checksum(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |acc, byte| acc ^ byte)
}

/// Decodes a "get VCP feature" reply: source 6E, length 88, opcode 02,
/// result, vcp, type, max hi/lo, current hi/lo, checksum. `None` for a reply
/// that is malformed, garbled or about another feature, which is retried.
fn parse_reply(reply: &[u8; 12], vcp: u8) -> Option<Reply> {
    let valid = reply[0] == DDC_DEST
        && reply[1] == GET_REPLY_LENGTH
        && reply[2] == 0x02
        && reply[4] == vcp
        && checksum(DDC_REPLY_DEST, &reply[..10]) == reply[10];
    if !valid {
        return None;
    }
    if reply[3] != 0 {
        return Some(Reply::Unsupported);
    }
    Some(Reply::Value { current: u16::from_be_bytes([reply[8], reply[9]]) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply read from the MSI PRO MP271A: input source is 15 (DisplayPort).
    const INPUT_IS_DP: [u8; 12] = [0x6e, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0xff, 0x00, 0x0f, 0x24, 0x00];

    #[test]
    fn decodes_a_real_reply() {
        assert!(matches!(parse_reply(&INPUT_IS_DP, 0x60), Some(Reply::Value { current: 15 })));
    }

    #[test]
    fn rejects_garbled_or_foreign_replies() {
        let mut bad_checksum = INPUT_IS_DP;
        bad_checksum[10] ^= 1;
        assert!(parse_reply(&bad_checksum, 0x60).is_none());
        assert!(parse_reply(&INPUT_IS_DP, 0x10).is_none());
        let null_message = [0x6e, 0x80, 0xbe, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(parse_reply(&null_message, 0x60).is_none());
    }

    #[test]
    fn request_checksum_matches_the_protocol() {
        // "set input 15": 84 03 60 00 0f, checksum seeded with 6E ^ 51.
        assert_eq!(checksum(DDC_DEST ^ DDC_HOST as u8, &[0x84, 0x03, 0x60, 0x00, 0x0f]), 0xd7);
    }
}
