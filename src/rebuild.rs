//! Rebuild the ZIP engine's lz77 flat output into a standard LZ4 block.
//!
//! The engine returns, per request: a literals area (never read — the
//! literals are taken from the original input, as kaelz4 does) followed by
//! 8-byte sequence triples. One triple, little-endian:
//!
//! ```text
//! [u32 litLength][u16 (offset - 1)][u16 mlBase]
//! ```
//!
//! `mlBase 1` means a 4-byte match; `mlBase 0` is not a match but a
//! literal-only marker whose `litLength` the rebuild credits as
//! `litLength + 3`. The rebuild emits the LZ4 block sequences (token,
//! literals, 2-byte offset, match-length extension) and ends the block
//! with the unmatched input tail as the final literal run — the same
//! arithmetic as the kaelz4 rebuild, so the output decodes with the
//! stock liblz4.

/// LZ4 block-format constants.
const RUN_MASK: usize = 15;
const ML_BITS: usize = 4;
const ML_MASK: usize = 15;

/// Rebuild one request's triples (`seqs*8` bytes) against its original
/// `input` into `out` (cleared first); returns the LZ4 block length.
pub(crate) fn lz77_block(input: &[u8], seqs: &[u8], out: &mut Vec<u8>) -> Result<usize, String> {
    if seqs.len() % 8 != 0 {
        return Err(format!("triple area is {}B, not a multiple of 8", seqs.len()));
    }
    out.clear();
    out.reserve(input.len() + input.len() / 255 + 16);

    let mut pending_lit = 0usize;
    let mut ip = 0usize;
    for t in seqs.chunks_exact(8) {
        let ll = u32::from_le_bytes(t[0..4].try_into().unwrap()) as usize;
        let off = u16::from_le_bytes(t[4..6].try_into().unwrap()) as usize + 1;
        let ml_base = u16::from_le_bytes(t[6..8].try_into().unwrap()) as usize;
        if ml_base == 0 {
            pending_lit += ll + 3;
            continue;
        }
        // the token nibble carries mlBase - 1; the decoder's match length
        // is nibble + 4 = mlBase + 3
        let nib = ml_base - 1;
        let ml = nib + 4;
        let lit = ll + pending_lit;
        pending_lit = 0;
        if ip + lit > input.len() {
            return Err(format!(
                "triple literals overrun the input (at {}+{}/{}B)",
                ip, lit, input.len()
            ));
        }

        // token + literal-length extension
        let token = out.len();
        out.push(0);
        if lit >= RUN_MASK {
            out[token] = (RUN_MASK << ML_BITS) as u8;
            push_extension(out, lit - RUN_MASK);
        } else {
            out[token] = (lit << ML_BITS) as u8;
        }
        out.extend_from_slice(&input[ip..ip + lit]);
        ip += lit;

        // 2-byte offset; the 16K request window keeps it far below 64K.
        // Decoded output so far is exactly the consumed input, so `ip` is
        // the reference the decoder would validate against.
        if off > u16::MAX as usize {
            return Err(format!("match offset {off} exceeds 64KB"));
        }
        if off > ip {
            return Err(format!("match offset {off} reaches before the block start"));
        }
        out.extend_from_slice(&(off as u16).to_le_bytes());

        // match-length extension
        if nib >= ML_MASK {
            out[token] += ML_MASK as u8;
            push_extension(out, nib - ML_MASK);
        } else {
            out[token] += nib as u8;
        }
        ip += ml;
    }
    if ip > input.len() {
        return Err(format!(
            "triples consume {}B of a {}B input",
            ip,
            input.len()
        ));
    }

    // the unmatched tail is the final literal run (no match, block ends)
    let lit = input.len() - ip;
    let token = out.len();
    out.push(0);
    if lit >= RUN_MASK {
        out[token] = (RUN_MASK << ML_BITS) as u8;
        push_extension(out, lit - RUN_MASK);
    } else {
        out[token] = (lit << ML_BITS) as u8;
    }
    out.extend_from_slice(&input[ip..]);
    Ok(out.len())
}

/// 255-style extension bytes after a saturating 4-bit token nibble.
fn push_extension(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

/// Test-only pure-Rust LZ4 block decoder (the unit tests round-trip the
/// rebuild through it without any library dependency).
#[cfg(test)]
pub(crate) mod test_support {
    /// Decode one LZ4 block into `dst`; mirrors what
    /// `LZ4_decompress_safe` accepts, including a trailing empty token.
    pub(crate) fn decode(src: &[u8], dst: &mut Vec<u8>) -> Result<(), String> {
        let mut s = 0usize;
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
            if s + lit > src.len() {
                return Err("truncated literals".into());
            }
            dst.extend_from_slice(&src[s..s + lit]);
            s += lit;
            if s == src.len() {
                return Ok(()); // final literal run: end of block
            }
            if s + 2 > src.len() {
                return Err("truncated offset".into());
            }
            let off = u16::from_le_bytes(src[s..s + 2].try_into().unwrap()) as usize;
            s += 2;
            if off == 0 || off > dst.len() {
                return Err(format!("bad offset {off} at {}B output", dst.len()));
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
            let start = dst.len() - off;
            for i in 0..ml {
                let b = dst[start + i];
                dst.push(b);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triple(ll: u32, off_m1: u16, ml_base: u16) -> [u8; 8] {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&ll.to_le_bytes());
        t[4..6].copy_from_slice(&off_m1.to_le_bytes());
        t[6..8].copy_from_slice(&ml_base.to_le_bytes());
        t
    }

    fn rebuild(input: &[u8], seqs: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let n = lz77_block(input, seqs, &mut out).expect("rebuild");
        out.truncate(n);
        out
    }

    fn roundtrip(input: &[u8], seqs: &[u8]) {
        let block = rebuild(input, seqs);
        let mut dst = Vec::new();
        test_support::decode(&block, &mut dst).expect("decode");
        assert_eq!(dst, input);
    }

    #[test]
    fn all_literals_empty_triples() {
        let input = b"abcdefgh";
        let block = rebuild(input, &[]);
        assert_eq!(block, [0x80, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h']);
        roundtrip(input, &[]);
    }

    #[test]
    fn known_answer_one_match() {
        // "ab" + a 14-byte match at offset 2 (mlBase 11): token 0x2E,
        // LE16 offset, zero-length tail token 0x00 (the kaelz4 rebuild
        // writes it unconditionally)
        let input = b"abababababababab";
        let seqs = triple(2, 1, 11);
        let block = rebuild(input, &seqs);
        assert_eq!(block, [0x2A, b'a', b'b', 0x02, 0x00, 0x00]);
        roundtrip(input, &seqs);
    }

    #[test]
    fn match_zero_triple_credits_literals_plus_three() {
        // mlBase 0 with litLength 5 puts 8 literals into the next match's
        // literal run (5 + 3): literals "abcdabcd", then a 4-byte match at
        // offset 8, then the 8-byte tail as the final literal run
        let input = b"abcdabcdabcd";
        let seqs = [triple(5, 0, 0), triple(0, 7, 1)].concat();
        let block = rebuild(input, &seqs);
        assert_eq!(
            block,
            [0x80, b'a', b'b', b'c', b'd', b'a', b'b', b'c', b'd', 0x08, 0x00, 0x00]
        );
        roundtrip(input, &seqs);
    }

    #[test]
    fn long_literals_and_long_match_use_extension_bytes() {
        // 298 pseudo-random literals + an 'x' run; a 300-byte match at
        // offset 2 starts two bytes inside the 'x' run so the back
        // reference agrees with the input
        let mut input = (0..298).map(|i| (i * 7) as u8).collect::<Vec<u8>>();
        input.extend(std::iter::repeat_n(b'x', 609));
        let seqs = [triple(300, 1, 297)].concat();
        roundtrip(&input, &seqs);
        let block = rebuild(&input, &seqs);
        // one sequence, both nibbles saturated: token 0xFF, literal
        // extension 255,30 (300-15=285)
        assert_eq!(&block[..3], &[0xFF, 255, 30]);
        assert_eq!(&block[303..305], &[2, 0]); // offset 2 after 300 literals
        assert_eq!(&block[305..307], &[255, 26]); // match extension: 296-15=281
    }

    #[test]
    fn offset_beyond_block_start_is_rejected() {
        let input = [0u8; 64];
        let seqs = triple(4, 0, 1); // offset 1 after 4 literals: fine
        roundtrip(&input, &seqs);
        let seqs = [triple(4, 0, 0), triple(0, 0, 1)].concat(); // offset 1 at 4B output: fine
        roundtrip(&input, &seqs);
        let bad = [triple(0, 0, 1)].concat(); // offset 1 at 0B output
        assert!(lz77_block(&input, &bad, &mut Vec::new()).is_err());
    }

    #[test]
    fn oversized_offset_and_literal_overrun_are_rejected() {
        let input = [0u8; 32];
        let seqs = [triple(4, 0xFFFF, 1)].concat(); // offset 65536
        assert!(lz77_block(&input, &seqs, &mut Vec::new()).is_err());
        let seqs = [triple(33, 0, 1)].concat(); // 33 literals in a 32B input
        assert!(lz77_block(&input, &seqs, &mut Vec::new()).is_err());
    }

    #[test]
    fn long_input_tail_beyond_the_last_match() {
        // match in the middle, 40-byte literal tail after it
        let mut input = vec![9u8; 50];
        input.extend_from_slice(b"0123456789abcdefghij0123456789abcdefghij01234567");
        let seqs = [triple(3, 0, 29)].concat(); // 3 literals + 32-byte match
        roundtrip(&input, &seqs);
    }

    /// The property the rebuild exists for: the system liblz4 decodes the
    /// rebuilt block back to the original bytes. Skipped where no system
    /// liblz4 is installed (the hw path re-checks it via the bench verify
    /// op).
    #[test]
    fn rebuild_decodes_with_system_liblz4() {
        use std::os::raw::c_int;
        let Ok((lib, _)) = crate::open_lib("liblz4.so.1", &[]) else {
            eprintln!("no system liblz4; skipped");
            return;
        };
        let Ok(fns) = crate::lz4::Lz4Fns::load(&lib) else {
            eprintln!("no LZ4_decompress_safe; skipped");
            return;
        };
        // 2048 pseudo-random bytes + 4096 'z' + a 13-byte tail; the match
        // sits 10 bytes into the 'z' run and covers all of it
        let mut input = (0..2048).map(|i| (i * 31) as u8).collect::<Vec<u8>>();
        input.extend(std::iter::repeat_n(b'z', 4096));
        input.extend_from_slice(b"trailing-tail");
        // ll 2055 + the +3 credit puts 2058 literals before the match
        let seqs = [triple(2055, 0, 0), triple(0, 9, 4083)].concat();
        let block = rebuild(&input, &seqs);
        let mut dst = vec![0u8; input.len()];
        let n = unsafe {
            (fns.decompress)(
                block.as_ptr(),
                dst.as_mut_ptr(),
                block.len() as c_int,
                dst.len() as c_int,
            )
        };
        assert!(n >= 0, "liblz4 rejected the rebuilt block");
        assert_eq!(&dst[..n as usize], &input[..]);
    }
}
