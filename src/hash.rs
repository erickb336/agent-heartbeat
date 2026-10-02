//! SHA-256 from CommonCrypto, which is part of macOS libSystem, so no hashing crate is needed.

unsafe extern "C" {
    fn CC_SHA256(data: *const u8, len: u32, md: *mut u8) -> *mut u8;
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    // CC_SHA256 takes a 32-bit length; callers hash short identifiers and secrets only.
    assert!(data.len() <= u32::MAX as usize);
    unsafe { CC_SHA256(data.as_ptr(), data.len() as u32, out.as_mut_ptr()) };
    out
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// A local dedup key for a request identity: the first 8 bytes of its SHA-256.
/// Among the few thousand requests of two days, a collision has a chance of about 1 in 10^12.
pub fn request_key(parts: &[&str]) -> u64 {
    let joined = parts.join("\u{1f}");
    let digest = sha256(joined.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn request_key_separates_parts() {
        assert_ne!(request_key(&["ab", "c"]), request_key(&["a", "bc"]));
    }
}
