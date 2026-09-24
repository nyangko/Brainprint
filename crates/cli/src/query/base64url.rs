//! Minimal base64url (no padding) codec for the compact `CONTINUATION`
//! token (#24 §7). The only user of base64 in this codebase; a small
//! self-contained algorithm, not worth a dependency for.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();

        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        if let Some(b1) = b1 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char);
        }
        if let Some(b2) = b2 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

fn value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let values = bytes
        .iter()
        .map(|&b| value(b))
        .collect::<Option<Vec<u8>>>()?;
    let mut out = Vec::with_capacity(values.len() * 3 / 4);
    for chunk in values.chunks(4) {
        let v0 = chunk[0];
        let v1 = *chunk.get(1)?;
        out.push((v0 << 2) | (v1 >> 4));
        if let Some(&v2) = chunk.get(2) {
            out.push((v1 << 4) | (v2 >> 2));
            if let Some(&v3) = chunk.get(3) {
                out.push((v2 << 6) | v3);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_arbitrary_bytes() {
        for sample in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            b"the quick brown fox jumps over the lazy dog, 0123456789",
            &[0_u8, 255, 128, 1, 254, 17],
        ] {
            let encoded = encode(sample);
            assert!(!encoded.contains('='), "no padding");
            assert_eq!(decode(&encoded).as_deref(), Some(sample));
        }
    }

    #[test]
    fn rejects_non_alphabet_characters() {
        assert_eq!(decode("not valid base64url!!"), None);
    }
}
