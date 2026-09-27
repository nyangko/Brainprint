//! Small dependency-free helpers. #26 limits this crate to
//! brainprint-core + serde/serde_json/clap/tokio, so the digest and the
//! request id are hand-rolled rather than pulled from sha2/uuid.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// FNV-1a 128 over length-prefixed parts, as 32 lowercase hex chars.
/// Used only to compare two compact descriptors for equality (never as a
/// security boundary); the length prefix keeps `["ab","c"]` and
/// `["a","bc"]` distinct.
pub fn fingerprint<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut hash = OFFSET_BASIS;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    for part in parts {
        feed(&(part.len() as u64).to_le_bytes());
        feed(part.as_bytes());
    }
    format!("{hash:032x}")
}

/// A canonical-form v4 UUID string for Task 11 `request_id` (transport
/// correlation only, never an identity). Randomness comes from std's
/// per-process randomly seeded `RandomState`.
pub fn request_id() -> String {
    let mut words = [0_u64; 2];
    for (index, word) in words.iter_mut().enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(now_unix_nanos());
        hasher.write_usize(index);
        hasher.write_u32(std::process::id());
        *word = hasher.finish();
    }
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    let mut bytes: [u8; 16] = bytes.try_into().unwrap_or([0; 16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn now_unix_ms() -> u64 {
    u64::try_from(now_unix_nanos() / 1_000_000).unwrap_or(u64::MAX)
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

pub fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic_and_boundary_aware() {
        assert_eq!(fingerprint(["a", "b"]), fingerprint(["a", "b"]));
        assert_ne!(fingerprint(["ab", "c"]), fingerprint(["a", "bc"]));
        assert_eq!(fingerprint(["x"]).len(), 32);
    }

    #[test]
    fn request_id_is_canonical_v4() {
        let id = request_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, request_id());
    }
}
