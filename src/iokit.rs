//! The few IOKit and CoreFoundation calls shared by the DDC and HID code:
//! walking the I/O registry and comparing a string property.

use std::ffi::{c_char, c_void, CString};

pub type CFTypeRef = *const c_void;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_COMPARE_CASE_INSENSITIVE: u64 = 1;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFTypeRef;
    fn IOServiceGetMatchingServices(port: u32, matching: CFTypeRef, iter: *mut u32) -> i32;
    fn IOIteratorNext(iter: u32) -> u32;
    fn IOObjectRelease(obj: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CFTypeRef,
        allocator: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFTypeRef;
    fn CFStringCompare(a: CFTypeRef, b: CFTypeRef, options: u64) -> isize;
    fn CFStringGetTypeID() -> usize;
    fn CFGetTypeID(obj: CFTypeRef) -> usize;
    pub fn CFRelease(obj: CFTypeRef);
}

/// An owned CFString, released when dropped.
struct CfString(CFTypeRef);

impl CfString {
    fn new(s: &str) -> CfString {
        let c = CString::new(s).unwrap();
        CfString(unsafe {
            CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        })
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

/// Calls `visit` with every registry entry of `class` whose string property
/// `key` equals `value` (ignoring case) until `visit` returns true. The entry
/// is only valid during the call. Returns whether `visit` returned true.
pub fn find_service(class: &str, key: &str, value: &str, mut visit: impl FnMut(u32) -> bool) -> bool {
    let class = CString::new(class).unwrap();
    let key = CfString::new(key);
    let value = CfString::new(value);
    let mut done = false;
    unsafe {
        let mut iter = 0u32;
        // kIOMainPortDefault is 0; the matching dictionary is consumed.
        if IOServiceGetMatchingServices(0, IOServiceMatching(class.as_ptr()), &mut iter) != 0 {
            return false;
        }
        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 {
                break;
            }
            if !done {
                let property = IORegistryEntryCreateCFProperty(entry, key.0, std::ptr::null(), 0);
                if !property.is_null() {
                    let matches = CFGetTypeID(property) == CFStringGetTypeID()
                        && CFStringCompare(property, value.0, K_CF_COMPARE_CASE_INSENSITIVE) == 0;
                    CFRelease(property);
                    if matches {
                        done = visit(entry);
                    }
                }
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iter);
    }
    done
}
