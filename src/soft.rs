//! Pure-Rust software LZ4 engine over the `lz4rip` crate: the no-C path of
//! this crate. It is the software side of [`crate::KaeNative`] (the fallback
//! when the ZIP engine is unavailable or a request stalls) and the `soft`
//! bench implementation; the native decompress cells decode through here
//! too. Blocks in and out are the same standard LZ4 block format every
//! implementation produces, so soft-compressed samples cross-verify with
//! the system liblz4 like every other implementation's.

/// Worst-case compressed size of `n` input bytes (lz4rip's bound).
pub(crate) fn bound(n: usize) -> usize {
    lz4rip::get_maximum_output_size(n)
}

/// Compress one standard LZ4 block into `dst` and return the byte count
/// written. `dst` smaller than [`bound`] of `src.len()` is an error, never a
/// silent truncation.
pub(crate) fn compress(src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
    lz4rip::compress_into(src, dst).map_err(|e| format!("lz4rip compress: {e}"))
}

/// Decompress one standard LZ4 block into `dst` and return the byte count
/// written. `dst` too small is an error; corrupt input names the violation
/// through lz4rip's error type. Two edge semantics relative to
/// `LZ4_decompress_safe`: an empty block decodes to zero bytes (lz4rip
/// alone rejects it), and a block that ends exactly on a match — legal
/// LZ4, but produced by no compressor in play (C libraries and
/// [`crate::rebuild`] always close with the trailing literal run) — is
/// rejected rather than decoded.
pub(crate) fn decompress(src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
    if src.is_empty() {
        return Ok(0);
    }
    lz4rip::decompress_into(src, dst).map_err(|e| format!("lz4rip decompress: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lz4dec;

    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        fn fill(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                let v = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&v[..chunk.len()]);
            }
        }
    }

    fn roundtrip(input: &[u8]) {
        let mut comp = vec![0u8; bound(input.len())];
        let n = compress(input, &mut comp).expect("compress");
        let mut restored = vec![0u8; input.len()];
        let m = decompress(&comp[..n], &mut restored).expect("decompress");
        assert_eq!(m, input.len());
        assert_eq!(&restored[..m], input);
    }

    #[test]
    fn roundtrips_zeros_random_and_text() {
        roundtrip(&[]);
        roundtrip(&[0u8; 4096]);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 | 1);
        let mut random = vec![0u8; 4096];
        rng.fill(&mut random);
        roundtrip(&random);
        let text: Vec<u8> = b"snapshot-page-7 ".iter().copied().cycle().take(8192).collect();
        roundtrip(&text);
    }

    #[test]
    fn dst_sized_by_bound_never_fails_and_smaller_dst_errors() {
        let input = vec![7u8; 4096];
        let mut dst = vec![0u8; bound(input.len())];
        let n = compress(&input, &mut dst).expect("bound-sized dst");
        assert!(n > 0 && n < input.len());
        let mut tight = vec![0u8; n - 1];
        assert!(compress(&input, &mut tight).is_err());
        let mut short = vec![0u8; input.len() - 1];
        assert!(decompress(&dst[..n], &mut short).is_err());
    }

    /// The two decoders in the tree are independent implementations of the
    /// same format: each decodes the other's partner compressor output.
    #[test]
    fn soft_and_lz4dec_decode_each_others_blocks() {
        let input: Vec<u8> = b"abcdabcdabcd".iter().copied().cycle().take(4096).collect();
        let mut comp = vec![0u8; bound(input.len())];
        let n = compress(&input, &mut comp).expect("compress");
        let mut restored = vec![0u8; input.len()];
        let m = lz4dec::decode_block(&comp[..n], &mut restored).expect("lz4dec reads soft output");
        assert_eq!(&restored[..m], &input[..]);

        // a literals-closing block in lz4dec style, decoded by soft
        let block = [0x30u8, b'a', b'b', b'c'];
        let mut out = vec![0u8; 3];
        let m = decompress(&block, &mut out).expect("soft reads lz4dec-style block");
        assert_eq!(&out[..m], b"abc");
    }

    /// A block ending exactly on a match decodes with lz4dec and is
    /// rejected by soft — the one format edge where the two decoders
    /// disagree (see `decompress`'s doc).
    #[test]
    fn match_closing_block_is_lz4dec_only() {
        let block = [0x10u8, b'x', 0x01, 0x00]; // 1 literal + a 4-byte run
        let mut out = vec![0u8; 5];
        let m = lz4dec::decode_block(&block, &mut out).expect("lz4dec accepts it");
        assert_eq!(&out[..m], b"xxxxx");
        assert!(decompress(&block, &mut out).is_err());
    }

    #[test]
    fn corrupt_input_is_an_error() {
        let mut out = vec![0u8; 64];
        assert!(decompress(&[0x80, b'a'], &mut out).is_err()); // short literals
        assert!(decompress(&[0x01, 0xAA], &mut out).is_err()); // missing offset
        assert!(decompress(&[], &mut out).is_ok_and(|n| n == 0));
    }
}
