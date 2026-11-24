//! The user's home directory and id, shared by both programs.

use std::path::PathBuf;

/// `HOME`, or without it the home directory from the user database; never
/// the current directory.
pub fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        return PathBuf::from(home);
    }
    #[repr(C)]
    struct Passwd {
        name: *const std::ffi::c_char,
        password: *const std::ffi::c_char,
        uid: u32,
        gid: u32,
        change: isize,
        class: *const std::ffi::c_char,
        gecos: *const std::ffi::c_char,
        dir: *const std::ffi::c_char,
    }
    extern "C" {
        fn getpwuid(uid: u32) -> *const Passwd;
    }
    // SAFETY: getpwuid returns null or a record valid until the next call;
    // the directory is copied out at once.
    unsafe {
        let entry = getpwuid(uid());
        if entry.is_null() || (*entry).dir.is_null() {
            return PathBuf::from("/var/empty");
        }
        let dir = std::ffi::CStr::from_ptr((*entry).dir);
        PathBuf::from(std::ffi::OsStr::from_encoded_bytes_unchecked(dir.to_bytes()))
    }
}

pub fn uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { getuid() }
}
