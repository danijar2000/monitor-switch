//! The shared secret both agents sign their calls with.
//!
//! `~/.monitor-switch/peer_secret` holds 32 random bytes as hex; install.sh
//! writes the same file on both Macs. A call carries the time, a random nonce
//! and `HMAC-SHA256(secret, "<command>\n<time>\n<nonce>")`. The receiving
//! agent accepts it only when the signature matches, the time is close to its
//! own and the signature was not seen before, so a call recorded on the
//! network cannot be sent again.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How far the two Macs' clocks (and the network) may be apart.
pub const MAX_SKEW: Duration = Duration::from_secs(120);

pub fn secret_path() -> std::path::PathBuf {
    crate::config::dir().join("peer_secret")
}

/// Reads the secret; it must be at least 32 bytes of hex.
pub fn load_secret() -> Result<Vec<u8>, String> {
    let path = secret_path();
    let text = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "cannot read {} ({error}): run install.sh again",
            path.display()
        )
    })?;
    let secret = from_hex(text.trim()).ok_or(format!("{} is not hex", path.display()))?;
    if secret.len() < 32 {
        return Err(format!("{} is too short", path.display()));
    }
    Ok(secret)
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0)
}

/// 16 random bytes as hex.
pub fn nonce() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = std::fs::File::open("/dev/urandom") {
        let _ = random.read_exact(&mut bytes);
    }
    to_hex(&bytes)
}

fn message(command: &str, time: u64, nonce: &str) -> String {
    format!("{command}\n{time}\n{nonce}")
}

pub fn sign(secret: &[u8], command: &str, time: u64, nonce: &str) -> String {
    to_hex(&hmac_sha256(
        secret,
        message(command, time, nonce).as_bytes(),
    ))
}

/// Signatures accepted lately, with their times: each is good only once.
static SEEN: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

/// Checks a signed call; `Err` says why it is refused.
pub fn verify(
    secret: &[u8],
    command: &str,
    time: u64,
    nonce: &str,
    signature: &str,
) -> Result<(), &'static str> {
    let expected = sign(secret, command, time, nonce);
    if !same(
        expected.as_bytes(),
        signature.to_ascii_lowercase().as_bytes(),
    ) {
        return Err("bad signature (is peer_secret the same on both Macs?)");
    }
    let now = now();
    if now.abs_diff(time) > MAX_SKEW.as_secs() {
        return Err("the clocks of the two Macs are too far apart");
    }
    let mut seen = SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    seen.retain(|(at, _)| now.abs_diff(*at) <= 2 * MAX_SKEW.as_secs());
    if seen.iter().any(|(_, known)| *known == expected) {
        return Err("this call was already used");
    }
    seen.push((time, expected));
    Ok(())
}

/// Compares in constant time.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    // CommonCrypto is part of libSystem.
    extern "C" {
        fn CCHmac(
            algorithm: u32,
            key: *const u8,
            key_length: usize,
            data: *const u8,
            data_length: usize,
            mac_out: *mut u8,
        );
    }
    const K_CC_HMAC_ALG_SHA256: u32 = 2;
    let mut out = [0u8; 32];
    // SAFETY: the pointers and lengths come from live slices, and `out` has
    // room for a SHA-256 digest.
    unsafe {
        CCHmac(
            K_CC_HMAC_ALG_SHA256,
            key.as_ptr(),
            key.len(),
            data.as_ptr(),
            data.len(),
            out.as_mut_ptr(),
        )
    };
    out
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn accepts_a_call_once() {
        let secret = [7u8; 32];
        let time = now();
        let signature = sign(&secret, "ping", time, "n1");
        assert_eq!(verify(&secret, "ping", time, "n1", &signature), Ok(()));
        assert!(verify(&secret, "ping", time, "n1", &signature).is_err());
        assert!(verify(
            &secret,
            "arrive",
            time,
            "n2",
            &sign(&secret, "ping", time, "n2")
        )
        .is_err());
        assert!(verify(
            &[8u8; 32],
            "ping",
            time,
            "n3",
            &sign(&secret, "ping", time, "n3")
        )
        .is_err());
        let old = time - 600;
        assert!(verify(
            &secret,
            "ping",
            old,
            "n4",
            &sign(&secret, "ping", old, "n4")
        )
        .is_err());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(from_hex("00ff10").unwrap(), [0, 255, 16]);
        assert_eq!(to_hex(&[0, 255, 16]), "00ff10");
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }
}
