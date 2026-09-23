//! The digest a finished transfer is checked against.
//!
//! Two different hashes live in this engine and they answer different
//! questions. Per-chunk BLAKE3 in the journal answers "did this range land
//! intact", is never shown to anyone, and stays BLAKE3 because it is on the
//! hot path. The digest here answers "is this the file the publisher meant",
//! comes from a human pasting what a release page told them, and therefore has
//! to speak whatever that page speaks: which in practice is SHA-256.
//!
//! MD5 is here for the same reason: it is still what a great many mirrors
//! publish. It is not collision-resistant and must not be treated as proof of
//! anything but an accidental corruption.

use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Algorithm {
    #[default]
    Blake3,
    Sha256,
    Md5,
}

impl Algorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blake3 => "blake3",
            Self::Sha256 => "sha256",
            Self::Md5 => "md5",
        }
    }

    /// The display name, as a checksum file would spell it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Blake3 => "BLAKE3",
            Self::Sha256 => "SHA-256",
            Self::Md5 => "MD5",
        }
    }

    /// Digest length in bytes.
    pub fn byte_len(self) -> usize {
        match self {
            Self::Blake3 | Self::Sha256 => 32,
            Self::Md5 => 16,
        }
    }

    /// Digest length in hex characters, which is what a user pastes.
    pub fn hex_len(self) -> usize {
        self.byte_len() * 2
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().replace('-', "").as_str() {
            "blake3" | "b3" => Some(Self::Blake3),
            "sha256" => Some(Self::Sha256),
            "md5" => Some(Self::Md5),
            _ => None,
        }
    }

    /// The only algorithm whose length is unambiguous is MD5; SHA-256 and
    /// BLAKE3 are both 32 bytes, so a bare 64-character hex string cannot be
    /// identified by length alone and this returns `None` for it.
    pub fn guess_from_hex(hex: &str) -> Option<Self> {
        match hex.trim().len() {
            32 => Some(Self::Md5),
            _ => None,
        }
    }
}

/// An expected whole-file digest, and the algorithm that produced it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    algorithm: Algorithm,
    bytes: Vec<u8>,
}

impl Digest {
    /// Build from raw bytes, rejecting a length the algorithm cannot produce.
    pub fn new(algorithm: Algorithm, bytes: Vec<u8>) -> Option<Self> {
        (bytes.len() == algorithm.byte_len()).then_some(Self { algorithm, bytes })
    }

    /// Read what a person pasted.
    ///
    /// Tolerates the shapes checksum files actually come in: surrounding
    /// whitespace, an `sha256:` prefix, upper case, and the
    /// `<hex>  <filename>` form of a `.sha256sum` line. Rejects anything whose
    /// length is not exactly right, because a truncated digest that still
    /// parsed would silently check fewer bytes than the user believes.
    pub fn parse(algorithm: Algorithm, text: &str) -> Option<Self> {
        let text = text.trim();
        // "sha256:abc…" or "SHA256 (file) = abc…"
        let text = text.rsplit_once('=').map(|(_, rest)| rest).unwrap_or(text).trim();
        let text = text.rsplit_once(':').map(|(_, rest)| rest).unwrap_or(text).trim();
        // "<hex>  filename"
        let text = text.split_whitespace().next()?;
        if text.len() != algorithm.hex_len() {
            return None;
        }
        let mut bytes = Vec::with_capacity(algorithm.byte_len());
        let raw = text.as_bytes();
        for pair in raw.chunks(2) {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            bytes.push((hi * 16 + lo) as u8);
        }
        Some(Self { algorithm, bytes })
    }

    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn to_hex(&self) -> String {
        self.bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm.as_str(), self.to_hex())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl From<blake3::Hash> for Digest {
    fn from(hash: blake3::Hash) -> Self {
        Self { algorithm: Algorithm::Blake3, bytes: hash.as_bytes().to_vec() }
    }
}

/// A streaming hasher for whichever algorithm was asked for.
pub enum Hasher {
    Blake3(Box<blake3::Hasher>),
    Sha256(sha2::Sha256),
    Md5(md5::Md5),
}

impl Hasher {
    pub fn new(algorithm: Algorithm) -> Self {
        use sha2::Digest as _;
        match algorithm {
            Algorithm::Blake3 => Self::Blake3(Box::new(blake3::Hasher::new())),
            Algorithm::Sha256 => Self::Sha256(sha2::Sha256::new()),
            Algorithm::Md5 => Self::Md5(md5::Md5::new()),
        }
    }

    pub fn algorithm(&self) -> Algorithm {
        match self {
            Self::Blake3(_) => Algorithm::Blake3,
            Self::Sha256(_) => Algorithm::Sha256,
            Self::Md5(_) => Algorithm::Md5,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        use sha2::Digest as _;
        match self {
            Self::Blake3(hasher) => {
                hasher.update(data);
            }
            Self::Sha256(hasher) => hasher.update(data),
            Self::Md5(hasher) => hasher.update(data),
        }
    }

    pub fn finalize(self) -> Digest {
        use sha2::Digest as _;
        let (algorithm, bytes) = match self {
            Self::Blake3(hasher) => (Algorithm::Blake3, hasher.finalize().as_bytes().to_vec()),
            Self::Sha256(hasher) => (Algorithm::Sha256, hasher.finalize().to_vec()),
            Self::Md5(hasher) => (Algorithm::Md5, hasher.finalize().to_vec()),
        };
        Digest { algorithm, bytes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const EMPTY_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";

    #[test]
    fn each_algorithm_matches_its_published_empty_digest() {
        assert_eq!(Hasher::new(Algorithm::Sha256).finalize().to_hex(), EMPTY_SHA256);
        assert_eq!(Hasher::new(Algorithm::Md5).finalize().to_hex(), EMPTY_MD5);
        assert_eq!(
            Hasher::new(Algorithm::Blake3).finalize().to_hex(),
            blake3::hash(b"").to_hex().to_string()
        );
    }

    #[test]
    fn hashing_in_pieces_matches_hashing_at_once() {
        for algorithm in [Algorithm::Blake3, Algorithm::Sha256, Algorithm::Md5] {
            let mut split = Hasher::new(algorithm);
            split.update(b"the quick brown ");
            split.update(b"fox");
            let mut whole = Hasher::new(algorithm);
            whole.update(b"the quick brown fox");
            assert_eq!(split.finalize(), whole.finalize(), "{algorithm:?}");
        }
    }

    #[test]
    fn a_pasted_digest_survives_the_shapes_checksums_come_in() {
        let plain = Digest::parse(Algorithm::Sha256, EMPTY_SHA256).unwrap();
        for text in [
            &format!("  {EMPTY_SHA256}  "),
            &format!("sha256:{EMPTY_SHA256}"),
            &EMPTY_SHA256.to_ascii_uppercase(),
            &format!("{EMPTY_SHA256}  ubuntu-24.04.iso"),
            &format!("SHA256 (ubuntu.iso) = {EMPTY_SHA256}"),
        ] {
            assert_eq!(Digest::parse(Algorithm::Sha256, text), Some(plain.clone()), "{text}");
        }
    }

    #[test]
    fn a_digest_of_the_wrong_length_is_refused() {
        // Accepting a short digest would check a prefix of the file's hash and
        // report success, which is worse than not checking at all.
        assert_eq!(Digest::parse(Algorithm::Sha256, &EMPTY_SHA256[..40]), None);
        assert_eq!(Digest::parse(Algorithm::Md5, EMPTY_SHA256), None);
        assert_eq!(Digest::parse(Algorithm::Sha256, EMPTY_MD5), None);
        assert_eq!(Digest::parse(Algorithm::Sha256, ""), None);
        assert_eq!(Digest::new(Algorithm::Sha256, vec![0; 16]), None);
    }

    #[test]
    fn non_hex_is_refused_rather_than_read_as_zero() {
        let not_hex = "z".repeat(64);
        assert_eq!(Digest::parse(Algorithm::Sha256, &not_hex), None);
    }

    #[test]
    fn two_algorithms_never_compare_equal_even_on_the_same_bytes() {
        // Both are 32 bytes, so without the algorithm in the comparison a
        // BLAKE3 digest could satisfy a SHA-256 expectation.
        let bytes = vec![7u8; 32];
        let a = Digest::new(Algorithm::Blake3, bytes.clone()).unwrap();
        let b = Digest::new(Algorithm::Sha256, bytes).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn only_md5_can_be_identified_by_length_alone() {
        assert_eq!(Algorithm::guess_from_hex(EMPTY_MD5), Some(Algorithm::Md5));
        assert_eq!(Algorithm::guess_from_hex(EMPTY_SHA256), None);
    }
}
