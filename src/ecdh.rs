//! An elliptic-curve Diffie-Hellman exchange (P-256) through the Security
//! framework, for pairing the two Macs without a shared secret to start from.

use crate::iokit::{CFRelease, CFTypeRef};
use std::ffi::c_void;

#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecAttrKeyType: CFTypeRef;
    static kSecAttrKeyTypeECSECPrimeRandom: CFTypeRef;
    static kSecAttrKeySizeInBits: CFTypeRef;
    static kSecAttrKeyClass: CFTypeRef;
    static kSecAttrKeyClassPublic: CFTypeRef;
    static kSecKeyAlgorithmECDHKeyExchangeStandard: CFTypeRef;
    fn SecKeyCreateRandomKey(parameters: CFTypeRef, error: *mut CFTypeRef) -> CFTypeRef;
    fn SecKeyCopyPublicKey(key: CFTypeRef) -> CFTypeRef;
    fn SecKeyCopyExternalRepresentation(key: CFTypeRef, error: *mut CFTypeRef) -> CFTypeRef;
    fn SecKeyCreateWithData(data: CFTypeRef, attributes: CFTypeRef, error: *mut CFTypeRef) -> CFTypeRef;
    fn SecKeyCopyKeyExchangeResult(
        private_key: CFTypeRef,
        algorithm: CFTypeRef,
        public_key: CFTypeRef,
        parameters: CFTypeRef,
        error: *mut CFTypeRef,
    ) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    fn CFNumberCreate(allocator: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: isize) -> CFTypeRef;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
}

/// A CoreFoundation object, released when dropped.
struct Owned(CFTypeRef);

impl Owned {
    fn new(object: CFTypeRef, what: &str) -> Result<Owned, String> {
        if object.is_null() {
            Err(format!("the Security framework could not {what}"))
        } else {
            Ok(Owned(object))
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the object is owned and released once.
        unsafe { CFRelease(self.0) }
    }
}

fn bytes(data: &Owned) -> Vec<u8> {
    // SAFETY: `data` is a CFData that lives while it is read.
    unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data.0), CFDataGetLength(data.0) as usize).to_vec() }
}

/// A P-256 key: `CFDictionary` of its attributes.
fn attributes(public: bool) -> Result<Owned, String> {
    const SINT32: isize = 3; // kCFNumberSInt32Type
    let size = 256i32;
    // SAFETY: the statics are framework constants; the number and the
    // dictionary copy what they are given.
    unsafe {
        let bits = Owned::new(CFNumberCreate(std::ptr::null(), SINT32, std::ptr::addr_of!(size).cast()), "make a number")?;
        let mut keys = vec![kSecAttrKeyType, kSecAttrKeySizeInBits];
        let mut values = vec![kSecAttrKeyTypeECSECPrimeRandom, bits.0];
        if public {
            keys.push(kSecAttrKeyClass);
            values.push(kSecAttrKeyClassPublic);
        }
        Owned::new(
            CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as isize,
                std::ptr::addr_of!(kCFTypeDictionaryKeyCallBacks),
                std::ptr::addr_of!(kCFTypeDictionaryValueCallBacks),
            ),
            "describe a key",
        )
    }
}

/// This side of the exchange: a fresh key pair, used once.
pub struct KeyPair {
    private: Owned,
    /// The public key as 65 bytes (uncompressed X9.63), sent to the other Mac.
    pub public: Vec<u8>,
}

impl KeyPair {
    pub fn new() -> Result<KeyPair, String> {
        let parameters = attributes(false)?;
        // SAFETY: the parameters describe a P-256 key; the results are owned.
        unsafe {
            let private = Owned::new(SecKeyCreateRandomKey(parameters.0, std::ptr::null_mut()), "make a key")?;
            let public = Owned::new(SecKeyCopyPublicKey(private.0), "read the public key")?;
            let data = Owned::new(SecKeyCopyExternalRepresentation(public.0, std::ptr::null_mut()), "export the public key")?;
            Ok(KeyPair { private, public: bytes(&data) })
        }
    }

    /// The secret both sides arrive at, from the other side's public key.
    pub fn shared(&self, theirs: &[u8]) -> Result<Vec<u8>, String> {
        if theirs.len() != 65 || theirs[0] != 4 {
            return Err("the other Mac sent a malformed key".into());
        }
        let attributes = attributes(true)?;
        // SAFETY: the data and keys are owned and live for the calls.
        unsafe {
            let data = Owned::new(CFDataCreate(std::ptr::null(), theirs.as_ptr(), theirs.len() as isize), "copy a key")?;
            let public = Owned::new(
                SecKeyCreateWithData(data.0, attributes.0, std::ptr::null_mut()),
                "read the other Mac's key",
            )?;
            let empty = Owned::new(
                CFDictionaryCreate(
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    std::ptr::addr_of!(kCFTypeDictionaryKeyCallBacks),
                    std::ptr::addr_of!(kCFTypeDictionaryValueCallBacks),
                ),
                "make a dictionary",
            )?;
            let result = Owned::new(
                SecKeyCopyKeyExchangeResult(
                    self.private.0,
                    kSecKeyAlgorithmECDHKeyExchangeStandard,
                    public.0,
                    empty.0,
                    std::ptr::null_mut(),
                ),
                "agree on a secret",
            )?;
            Ok(bytes(&result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KeyPair;

    #[test]
    fn both_sides_agree() {
        let (a, b) = (KeyPair::new().unwrap(), KeyPair::new().unwrap());
        assert_eq!(a.public.len(), 65);
        let shared = a.shared(&b.public).unwrap();
        assert_eq!(shared.len(), 32);
        assert_eq!(shared, b.shared(&a.public).unwrap());
        assert!(a.shared(&[4; 10]).is_err());
    }
}
