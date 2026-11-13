//! Checksums for chunk and file verification. They sit on the hot path of
//! every resume scan and verification, so they use SIMD/assembly backends.
use aws_lc_rs::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use simd_adler32::Adler32;

const ADLER_MODULUS: u32 = 65521;

/// Steam's checksum differs from RFC Adler32: its initial state is zero.
///
/// With `A`, `B` the RFC sums of `n` bytes (initial state `a = 1, b = 0`), the
/// zero-initialised sums are `a = A - 1` and `b = B - n`, both modulo 65521.
pub fn adler32(bytes: &[u8]) -> u32 {
    let mut state = Adler32::new();
    state.write(bytes);
    let rfc = state.finish();
    let (a, b) = (rfc & 0xffff, rfc >> 16);
    let length = (bytes.len() % ADLER_MODULUS as usize) as u32;
    let a = (a + ADLER_MODULUS - 1) % ADLER_MODULUS;
    let b = (b + ADLER_MODULUS - length) % ADLER_MODULUS;
    (b << 16) | a
}

/// SHA1 of `bytes`. SHA1 is used only because Steam's formats require it.
pub fn sha1(bytes: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1Stream::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Incremental SHA1 for files read in blocks.
pub struct Sha1Stream(Context);

impl Sha1Stream {
    pub fn new() -> Self {
        Self(Context::new(&SHA1_FOR_LEGACY_USE_ONLY))
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finalize(self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out.copy_from_slice(self.0.finish().as_ref());
        out
    }
}

impl Default for Sha1Stream {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition used by Steam: zero initial state, modulo 65521.
    fn reference_adler(bytes: &[u8]) -> u32 {
        let (mut a, mut b) = (0u64, 0u64);
        for byte in bytes {
            a = (a + u64::from(*byte)) % u64::from(ADLER_MODULUS);
            b = (b + a) % u64::from(ADLER_MODULUS);
        }
        ((b as u32) << 16) | a as u32
    }

    #[test]
    fn adler_matches_steams_zero_initialised_definition() {
        let data: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for length in [
            0, 1, 2, 17, 5551, 5552, 5553, 65_521, 65_522, 131_072, 200_000,
        ] {
            assert_eq!(
                adler32(&data[..length]),
                reference_adler(&data[..length]),
                "length {length}"
            );
        }
        assert_eq!(adler32(&[0xff; 100_000]), reference_adler(&[0xff; 100_000]));
    }

    #[test]
    fn sha1_matches_the_standard_vectors_in_one_shot_and_in_blocks() {
        let abc = [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ];
        assert_eq!(sha1(b"abc"), abc);
        let mut stream = Sha1Stream::new();
        stream.update(b"a");
        stream.update(b"bc");
        assert_eq!(stream.finalize(), abc);
        let empty = [
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
        ];
        assert_eq!(sha1(b""), empty);
    }
}
