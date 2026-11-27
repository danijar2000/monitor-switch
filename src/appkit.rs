//! The few AppKit and libdispatch pieces shared by the panel (`hud`) and the
//! menu bar icon (`menu`), called through the Objective-C runtime directly.

use std::ffi::{c_char, c_void, CString};

pub type Id = *mut c_void;
pub type Sel = *const c_void;

pub const NIL: Id = std::ptr::null_mut();

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    pub fn objc_msgSend();
    /// On Intel a method that returns a large struct goes through this.
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    pub fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra_bytes: usize) -> Id;
    pub fn objc_registerClassPair(class: Id);
    pub fn class_addMethod(class: Id, sel: Sel, imp: *const c_void, types: *const c_char) -> bool;
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

extern "C" {
    static _dispatch_main_q: c_void;
    pub fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    pub fn dispatch_after_f(when: u64, queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    pub fn dispatch_time(when: u64, delta: i64) -> u64;
}

pub fn sel(name: &str) -> Sel {
    let c = CString::new(name).unwrap();
    unsafe { sel_registerName(c.as_ptr()) }
}

pub fn class(name: &str) -> Id {
    let c = CString::new(name).unwrap();
    unsafe { objc_getClass(c.as_ptr()) }
}

/// `msg!(ReturnType, receiver, "selector:", arg => ArgType, ...)`
macro_rules! msg {
    ($ret:ty, $obj:expr, $sel:expr $(, $arg:expr => $ty:ty)*) => {{
        // SAFETY: every call site names a selector the receiver answers,
        // with the argument and return types that selector has.
        #[allow(unused_unsafe)]
        unsafe {
            let f: unsafe extern "C" fn($crate::appkit::Id, $crate::appkit::Sel $(, $ty)*) -> $ret =
                std::mem::transmute($crate::appkit::objc_msgSend as unsafe extern "C" fn());
            f($obj, $crate::appkit::sel($sel) $(, $arg)*)
        }
    }};
}

/// A message that returns an `NSRect`.
pub fn rect_of(object: Id, selector: &str) -> Rect {
    #[cfg(target_arch = "x86_64")]
    let send = objc_msgSend_stret as unsafe extern "C" fn();
    #[cfg(not(target_arch = "x86_64"))]
    let send = objc_msgSend as unsafe extern "C" fn();
    // SAFETY: `selector` is one that returns an NSRect.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Rect = std::mem::transmute(send);
        f(object, sel(selector))
    }
}

pub fn nsstring(text: &str) -> Id {
    let c = CString::new(text.replace('\0', " ")).unwrap();
    msg!(Id, class("NSString"), "stringWithUTF8String:", c.as_ptr() => *const c_char)
}

pub fn color(name: &str) -> Id {
    msg!(Id, class("NSColor"), name)
}

pub fn main_queue() -> *const c_void {
    // SAFETY: only the address is taken; the static is never read here.
    // (Rust before 1.82 needs the unsafe block for that too.)
    #[allow(unused_unsafe)]
    unsafe {
        std::ptr::addr_of!(_dispatch_main_q)
    }
}

/// An SF Symbol as a template image, which the menu bar tints itself.
pub fn symbol(name: &str) -> Id {
    let image = msg!(
        Id,
        class("NSImage"),
        "imageWithSystemSymbolName:accessibilityDescription:",
        nsstring(name) => Id,
        NIL => Id
    );
    if !image.is_null() {
        msg!((), image, "setTemplate:", true => bool);
    }
    image
}
