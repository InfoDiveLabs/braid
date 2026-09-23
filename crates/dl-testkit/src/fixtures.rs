//! Deterministic test payloads, generated from a position-indexed hash rather
//! than stored on disk.
//!
//! Each byte depends only on its own offset, so a range fetched on its own is
//! identical to that slice of the whole payload. Chunked downloads can be
//! checked against the same generator, at any size, with no stored fixtures.

/// Mixing function from splitmix64.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut r = z;
    r = (r ^ (r >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    r = (r ^ (r >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    r ^ (r >> 31)
}

/// The byte this payload has at `offset`. Depends only on `seed` and `offset`.
pub fn byte_at(seed: u64, offset: u64) -> u8 {
    let block = mix(seed ^ (offset / 8));
    ((block >> (8 * (offset % 8))) & 0xFF) as u8
}

/// Fill `buf` with the payload bytes starting at `offset`.
pub fn fill(seed: u64, offset: u64, buf: &mut [u8]) {
    for (i, slot) in buf.iter_mut().enumerate() {
        *slot = byte_at(seed, offset + i as u64);
    }
}

/// The payload bytes in `[offset, offset + len)`.
pub fn range(seed: u64, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    fill(seed, offset, &mut buf);
    buf
}

/// BLAKE3 of the whole payload, computed without materialising it.
pub fn digest(seed: u64, size: u64) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    let mut offset = 0u64;
    let mut buf = [0u8; 64 * 1024];
    while offset < size {
        let n = buf.len().min((size - offset) as usize);
        fill(seed, offset, &mut buf[..n]);
        hasher.update(&buf[..n]);
        offset += n as u64;
    }
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_matches_the_same_span_of_the_whole_payload() {
        let whole = range(7, 0, 4096);
        // The property chunked downloads depend on: any slice fetched on its own
        // is byte-identical to that slice of the full payload.
        for (start, len) in [(0, 1), (1, 7), (8, 8), (13, 100), (4000, 96)] {
            assert_eq!(range(7, start, len), whole[start as usize..start as usize + len]);
        }
    }

    #[test]
    fn different_seeds_produce_different_payloads() {
        assert_ne!(range(1, 0, 256), range(2, 0, 256));
    }

    #[test]
    fn digest_matches_hashing_the_materialised_payload() {
        let size = 200_000;
        assert_eq!(digest(3, size), blake3::hash(&range(3, 0, size as usize)));
    }
}
