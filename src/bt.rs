//! Bluetooth hand-over through IOBluetooth, called with the Objective-C
//! runtime directly so the crate needs no dependencies.
//!
//! A Magic Keyboard or Magic Mouse remembers a single host. To move it, the
//! Mac that has it removes the pairing (`begin_release`), which makes the
//! device pairable again, and the other Mac pairs and connects (`take`).
//! Everything here must run on the thread that turns the run loop (`spin`).

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::time::{Duration, Instant};

type Id = *mut c_void;
type Sel = *const c_void;

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra_bytes: usize) -> Id;
    fn objc_registerClassPair(class: Id);
    fn class_addMethod(class: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> bool;
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "Foundation", kind = "framework")]
extern "C" {}

#[link(name = "IOBluetooth", kind = "framework")]
extern "C" {}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    fn CFRunLoopGetCurrent() -> *const c_void;
    fn CFRunLoopStop(run_loop: *const c_void);
}

fn sel(name: &str) -> Sel {
    let c = CString::new(name).unwrap();
    unsafe { sel_registerName(c.as_ptr()) }
}

fn class(name: &str) -> Id {
    let c = CString::new(name).unwrap();
    unsafe { objc_getClass(c.as_ptr()) }
}

/// `msg!(ReturnType, receiver, "selector:", arg => ArgType, ...)`
macro_rules! msg {
    ($ret:ty, $obj:expr, $sel:expr $(, $arg:expr => $ty:ty)*) => {{
        let f: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f($obj, sel($sel) $(, $arg)*)
    }};
}

fn nsstring(s: &str) -> Id {
    let c = CString::new(s).unwrap();
    unsafe { msg!(Id, class("NSString"), "stringWithUTF8String:", c.as_ptr() => *const c_char) }
}

fn rust_string(ns: Id) -> String {
    if ns.is_null() {
        return String::new();
    }
    unsafe {
        let p = msg!(*const c_char, ns, "UTF8String");
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// `CFRunLoopRunInMode` result when the mode has no sources to wait on.
const RUN_LOOP_FINISHED: i32 = 1;

/// Lets IOBluetooth deliver its callbacks for `seconds`. Before IOBluetooth
/// has registered any source the run loop returns at once, so the remaining
/// time is slept instead of spun.
pub fn spin(seconds: f64) {
    let start = Instant::now();
    let result = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 0) };
    if result == RUN_LOOP_FINISHED {
        let left = Duration::from_secs_f64(seconds).saturating_sub(start.elapsed());
        std::thread::sleep(left);
    }
}

/// Runs `f` inside an autorelease pool, so the Objective-C objects that the
/// Bluetooth calls hand back are freed when it returns, even if it panics.
/// The agent wraps each turn of its loop in one; a single pool for the whole
/// process would only ever grow.
pub fn with_pool<T>(f: impl FnOnce() -> T) -> T {
    struct Pool(*mut c_void);
    impl Drop for Pool {
        fn drop(&mut self) {
            unsafe { objc_autoreleasePoolPop(self.0) }
        }
    }
    let _pool = Pool(unsafe { objc_autoreleasePoolPush() });
    f()
}

/// "AA:BB:CC:DD:EE:02", "aa-bb-cc-dd-ee-02" or "AABBCCDDEE02" -> "aa-bb-cc-dd-ee-02"
pub fn normalize(address: &str) -> Option<String> {
    let hex: String = address.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    let pairs: Vec<String> = hex
        .to_lowercase()
        .as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).into_owned())
        .collect();
    Some(pairs.join("-"))
}

pub struct Device {
    object: Id,
    pub address: String,
}

impl Device {
    pub fn with_address(address: &str) -> Result<Device, String> {
        let address = normalize(address).ok_or(format!("bad Bluetooth address: {address}"))?;
        let object = unsafe {
            msg!(Id, class("IOBluetoothDevice"), "deviceWithAddressString:", nsstring(&address) => Id)
        };
        if object.is_null() {
            return Err(format!("IOBluetooth does not know {address}"));
        }
        Ok(Device { object, address })
    }

    pub fn name(&self) -> String {
        unsafe { rust_string(msg!(Id, self.object, "nameOrAddress")) }
    }

    pub fn is_paired(&self) -> bool {
        unsafe { msg!(bool, self.object, "isPaired") }
    }

    pub fn is_connected(&self) -> bool {
        unsafe { msg!(bool, self.object, "isConnected") }
    }

    fn connect(&self) -> i32 {
        unsafe { msg!(i32, self.object, "openConnection") }
    }

    /// Removes the pairing, which also drops the connection. `remove` is a
    /// private IOBluetoothDevice method; there is no public way to unpair.
    fn unpair(&self) -> Result<(), String> {
        unsafe {
            if !msg!(bool, self.object, "respondsToSelector:", sel("remove") => Sel) {
                return Err("this macOS has no IOBluetoothDevice.remove".into());
            }
            let _ = msg!(Id, self.object, "remove");
        }
        Ok(())
    }

    /// One pairing attempt; returns the IOReturn the pairing finished with.
    fn pair_once(&self, timeout: f64) -> i32 {
        PAIR_RESULT.store(PAIR_PENDING, Ordering::SeqCst);
        unsafe {
            let pair = msg!(Id, class("IOBluetoothDevicePair"), "pairWithDevice:", self.object => Id);
            if pair.is_null() {
                return -1;
            }
            CURRENT_PAIR.store(pair, Ordering::SeqCst);
            let delegate = msg!(Id, msg!(Id, pair_delegate_class(), "alloc"), "init");
            let _ = msg!(Id, pair, "setDelegate:", delegate => Id);
            let started = msg!(i32, pair, "start");
            if started == 0 {
                spin(timeout);
            }
            // The pair object does not own its delegate: detach it first, so
            // a late callback cannot reach a freed object, then free it.
            let _ = msg!(Id, pair, "setDelegate:", std::ptr::null_mut::<c_void>() => Id);
            let _ = msg!(Id, delegate, "release");
            CURRENT_PAIR.store(std::ptr::null_mut(), Ordering::SeqCst);
            if started != 0 {
                return started;
            }
            let result = PAIR_RESULT.load(Ordering::SeqCst);
            // Stop only an attempt that did not succeed. After a successful
            // pairing bluetoothd is still attaching the device as an input
            // device on the same link; stopping here drops that link, and a
            // Magic Mouse cut off at this point forgets the key it just agreed.
            if result != 0 {
                let _ = msg!(Id, pair, "stop");
            }
            result
        }
    }

    /// Starts giving the device up so the other Mac can pair with it.
    /// bluetoothd carries the unpairing out on its own time (it unplugs the
    /// HID "virtual cable" first), so follow with `is_released`.
    pub fn begin_release(&self) -> Result<(), String> {
        if self.is_paired() {
            self.unpair()?;
        }
        Ok(())
    }

    /// True once this Mac has fully let go: no pairing, no link, no driver.
    pub fn is_released(&self) -> bool {
        !self.is_paired() && !self.is_connected() && !self.is_usable()
    }

    /// True when the device is attached as an input device, not merely
    /// holding a radio link.
    pub fn is_usable(&self) -> bool {
        crate::hid::is_live(&self.address)
    }

    /// Waits until the device is usable; false if `seconds` pass first.
    fn wait_usable(&self, seconds: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        loop {
            if self.is_usable() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            spin(0.25);
        }
    }

    /// Pairs with the device if needed and brings it up as an input device.
    /// Keeps trying until `timeout`, because a device needs a few seconds
    /// after the other Mac released it before it accepts a new host. No step
    /// waits past the deadline.
    pub fn take(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let left = || deadline.saturating_duration_since(Instant::now()).as_secs_f64();
        let mut last_error = String::from("no time to try");
        while left() > 0.0 {
            if self.is_paired() && self.is_usable() {
                return Ok(());
            }
            if !self.is_paired() {
                let result = self.pair_once(left().min(15.0));
                if result != 0 {
                    last_error = format!("pairing failed ({})", io_return(result));
                } else if self.wait_usable(left().min(8.0)) {
                    // bluetoothd finished the connection it began with the pairing.
                    return Ok(());
                }
            }
            if self.is_paired() && left() > 0.0 {
                let mut result = 0;
                for _ in 0..3 {
                    result = self.connect();
                    if self.wait_usable(left().min(4.0)) {
                        return Ok(());
                    }
                }
                last_error = format!(
                    "paired, but it does not come up as an input device ({})",
                    io_return(result)
                );
                // A pairing that cannot be used is stale on one side:
                // forget it so the next round pairs from scratch.
                // Wait for the unpairing to finish before pairing again, or
                // the new pairing races the old one being torn down.
                let _ = self.unpair();
                let unpaired_by = Instant::now() + Duration::from_secs_f64(left().min(3.0));
                while self.is_paired() && Instant::now() < unpaired_by {
                    spin(0.1);
                }
            }
            spin(left().min(0.7));
        }
        Err(last_error)
    }
}

fn io_return(code: i32) -> String {
    if code == PAIR_PENDING {
        "timed out".into()
    } else {
        format!("IOReturn 0x{:08x}", code as u32)
    }
}

// ---- IOBluetoothDevicePair delegate ---------------------------------------

/// Sentinel: the pairing has not reported a result yet.
const PAIR_PENDING: i32 = i32::MIN;
static PAIR_RESULT: AtomicI32 = AtomicI32::new(PAIR_PENDING);

/// The pairing `PAIR_RESULT` belongs to. A callback from any other pairing
/// (a late one from an attempt that was given up) is ignored.
static CURRENT_PAIR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn pairing_finished(_this: Id, _cmd: Sel, sender: Id, error: i32) {
    if sender != CURRENT_PAIR.load(Ordering::SeqCst) {
        return;
    }
    PAIR_RESULT.store(error, Ordering::SeqCst);
    unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) }
}

/// Keyboards and mice pair with "just works" confirmation: accept it.
extern "C" fn pairing_confirm(_this: Id, _cmd: Sel, sender: Id, _numeric_value: u32) {
    unsafe {
        msg!((), sender, "replyUserConfirmation:", true => bool);
    }
}

fn pair_delegate_class() -> Id {
    let existing = class("MonitorSwitchPairDelegate");
    if !existing.is_null() {
        return existing;
    }
    unsafe {
        let name = CString::new("MonitorSwitchPairDelegate").unwrap();
        let new_class = objc_allocateClassPair(class("NSObject"), name.as_ptr(), 0);
        let finished_types = CString::new("v@:@i").unwrap();
        class_addMethod(
            new_class,
            sel("devicePairingFinished:error:"),
            pairing_finished as *const c_void,
            finished_types.as_ptr(),
        );
        let confirm_types = CString::new("v@:@I").unwrap();
        class_addMethod(
            new_class,
            sel("devicePairingUserConfirmationRequest:numericValue:"),
            pairing_confirm as *const c_void,
            confirm_types.as_ptr(),
        );
        objc_registerClassPair(new_class);
        new_class
    }
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalizes_addresses() {
        for form in ["AA:BB:CC:DD:EE:02", "aa-bb-cc-dd-ee-02", "AABBCCDDEE02"] {
            assert_eq!(normalize(form).as_deref(), Some("aa-bb-cc-dd-ee-02"));
        }
        assert_eq!(normalize("AA:BB:CC"), None);
        assert_eq!(normalize("AA:BB:CC:DD:EE:02:11"), None);
    }
}
