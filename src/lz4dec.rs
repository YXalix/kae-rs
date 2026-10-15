//! Pure-Rust standard LZ4 block decoder: the decode side of the native
//! path, which keeps no C library in the process. The blocks it decodes
//! are the rebuild's output ([`crate::rebuild`]), the same standard LZ4
//! block format every implementation produces — so the native decompress
//! cell measures the same software-decode work the `sw`/`hw` cells do
//! (KAE accelerates compression only; their decode is software too, just
//! liblz4's instead of this).

/// Decode one standard LZ4 block into `dst` and return the byte count
/// written. Mirrors `LZ4_decompress_safe` semantics: tokens until the
/// input is consumed, a block may end on a literal run, matches copy
/// byte-by-byte so overlaps encode runs, and a zero or forward offset is
/// rejected. Errors name the violated rule; `dst` too small is an error,
/// never a silent truncation.
pub(crate) fn decode_block(src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
    let mut s = 0usize;
    let mut d = 0usize;
    while s < src.len() {
        let token = src[s];
        s += 1;

        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src.get(s).ok_or("truncated literal extension")?;
                s += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        if lit > src.len() - s {
            return Err("truncated literals".into());
        }
        if lit > dst.len() - d {
            return Err(format!("literals overrun the output buffer at {d}B"));
        }
        dst[d..d + lit].copy_from_slice(&src[s..s + lit]);
        d += lit;
        s += lit;
        if s == src.len() {
            return Ok(d); // final literal run: end of block
        }

        if src.len() - s < 2 {
            return Err("truncated offset".into());
        }
        let off = u16::from_le_bytes(src[s..s + 2].try_into().unwrap()) as usize;
        s += 2;
        if off == 0 {
            return Err("zero match offset".into());
        }
        if off > d {
            return Err(format!("match offset {off} reaches before the block start"));
        }

        let mut ml = (token & 15) as usize + 4;
        if token & 15 == 15 {
            loop {
                let b = *src.get(s).ok_or("truncated match extension")?;
                s += 1;
                ml += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        if ml > dst.len() - d {
            return Err(format!("match overruns the output buffer at {d}B"));
        }
        let start = d - off;
        for i in 0..ml {
            dst[d + i] = dst[start + i];
        }
        d += ml;
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebuild;

    fn decode(src: &[u8], expect: &[u8]) {
        let mut dst = vec![0u8; expect.len()];
        let n = decode_block(src, &mut dst).expect("decode");
        assert_eq!(n, expect.len());
        assert_eq!(&dst[..n], expect);
    }

    #[test]
    fn literals_only_and_rle_match() {
        // token 0x30: 3 literals, no match
        decode(&[0x30, b'a', b'b', b'c'], b"abc");
        // token 0x10: 1 literal + a 4-byte match at offset 1 — an RLE run
        // (a zero-literal match at the block start has nothing to reference)
        decode(&[0x10, b'x', 0x01, 0x00], b"xxxxx");
    }

    #[test]
    fn overlapping_match_copies_forward() {
        // "abc" + a 9-byte match at offset 3 → "abcabcabcabc" (the last
        // six copied bytes are themselves copies)
        decode(&[0x35, b'a', b'b', b'c', 0x03, 0x00, 0x00], b"abcabcabcabc");
    }

    #[test]
    fn extension_bytes_and_long_runs() {
        // 300 literals then a 300-byte match at offset 2: the copy starts
        // two bytes back and settles into a period-2 run of the last two
        // literals (LZ4's byte-by-byte overlap copy)
        let lit: Vec<u8> = (0..300).map(|i| (i * 7) as u8).collect();
        let mut src = vec![0xFFu8, 255, 30]; // token: 300 literals
        src.extend_from_slice(&lit);
        src.extend_from_slice(&2u16.to_le_bytes());
        src.extend_from_slice(&[255, 26]); // match extension: 300-15
        let mut expect = lit.clone();
        for _ in 0..300 {
            let n = expect.len();
            expect.push(expect[n - 2]);
        }
        decode(&src, &expect);
    }

    #[test]
    fn empty_block_decodes_to_nothing() {
        assert_eq!(decode_block(&[], &mut []).unwrap(), 0);
    }

    #[test]
    fn truncated_and_bad_offsets_are_rejected() {
        let mut dst = vec![0u8; 64];
        assert!(decode_block(&[0x80, b'a'], &mut dst).is_err()); // short literals
        assert!(decode_block(&[0x01, 0xAA], &mut dst).is_err()); // missing offset
        assert!(decode_block(&[0x01, b'x', 0x00, 0x00], &mut dst).is_err()); // offset 0
        assert!(decode_block(&[0x01, b'x', 0x02, 0x00], &mut dst).is_err()); // before start
        assert!(decode_block(&[0x0F, 0x01, 0x00, 255], &mut dst).is_err()); // short match ext
        assert!(decode_block(&[0x81, b'x'], &mut [0u8; 0]).is_err()); // dst too small
    }

    /// The decoder is the roundtrip partner of the rebuild: triples into
    /// `lz77_block`, the block back through here.
    #[test]
    fn decodes_rebuild_output() {
        let triple = |off_base: u32, lit_len: u16, ml_base: u16| -> [u8; 8] {
            let mut t = [0u8; 8];
            t[0..4].copy_from_slice(&off_base.to_le_bytes());
            t[4..6].copy_from_slice(&lit_len.to_le_bytes());
            t[6..8].copy_from_slice(&ml_base.to_le_bytes());
            t
        };
        let input = b"abcdabcdabcd";
        let seqs = [triple(0, 5, 0), triple(8 + 3, 0, 1)].concat();
        let mut block = Vec::new();
        let n = rebuild::lz77_block(input, &seqs, &mut block, &mut rebuild::RepRegister::fresh()).expect("rebuild");
        decode(&block[..n], input);
    }
}
