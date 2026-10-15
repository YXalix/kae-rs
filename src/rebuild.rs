//! Rebuild the ZIP engine's lz77 flat output into a standard LZ4 block.
//!
//! The engine returns, per request: a literals area (never read — the
//! literals are taken from the original input, as kaelz4 does) followed by
//! 8-byte sequence triples in the HW_LZ77_ZSTD layout (KAELz4's
//! include/kaelz4.h seqDef). One triple, little-endian:
//!
//! ```text
//! [u32 offBase][u16 litLength][u16 mlBase]
//! ```
//!
//! `offBase > 3` encodes `offset + 3`; offBase 1..3 read the engine's
//! register of the last three explicitly-coded offsets (a fresh request
//! starts it at [1, 4, 8]) and leave it alone — see `resolve_offset`.
//! `mlBase 1` means a 4-byte match; `mlBase 0` carries
//! no match, and its `litLength` plus 3 bytes fold into the next literal
//! run. The rebuild emits the LZ4 block sequences (token, literals, 2-byte
//! offset, match-length extension) and ends the block with the unmatched
//! input tail as the final literal run — the same arithmetic as the
//! kaelz4 rebuild, so the output decodes with the stock liblz4. The
//! engine never parses the last 12 bytes of the input (the request
//! programs `in_len - 12`; see uacce::MFLIMIT), so the tail run is always
//! at least 12 bytes and the last-match restriction holds.

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

    // the engine's register of the last three explicitly-coded offsets,
    // as a fresh request starts out
    let mut rep = [1usize, 4, 8];
    let mut pending_lit = 0usize;
    let mut ip = 0usize;
    for (ti, t) in seqs.chunks_exact(8).enumerate() {
        let off_base = u32::from_le_bytes(t[0..4].try_into().unwrap()) as usize;
        let lit_len = u16::from_le_bytes(t[4..6].try_into().unwrap()) as usize;
        let ml_base = u16::from_le_bytes(t[6..8].try_into().unwrap()) as usize;
        if ml_base == 0 {
            pending_lit += lit_len + 3;
            continue;
        }
        // the literal run covers the literal-only triples folded in ahead
        // of this match
        let lit = lit_len + pending_lit;
        pending_lit = 0;
        let off = resolve_offset(off_base, &mut rep).map_err(|e| {
            format!("{e} (triple {ti}, rep=[{}, {}, {}])", rep[0], rep[1], rep[2])
        })?;
        // the token nibble carries mlBase - 1; the decoder's match length
        // is nibble + 4 = mlBase + 3
        let nib = ml_base - 1;
        let ml = nib + 4;
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

/// Resolve one triple's `offBase` against the engine's register of the
/// last three explicitly-coded offsets in `rep` (most recent first) and
/// return the match offset. `offBase > 3` is `offset + 3` and shifts the
/// register; offBase 1..3 read its entries and leave them alone — a
/// repeat-coded match never changes what a later repeat code reads.
fn resolve_offset(off_base: usize, rep: &mut [usize; 3]) -> Result<usize, String> {
    if off_base == 0 {
        return Err("triple offBase is 0".into());
    }
    if off_base > 3 {
        let off = off_base - 3;
        rep[2] = rep[1];
        rep[1] = rep[0];
        rep[0] = off;
        return Ok(off);
    }
    Ok(rep[off_base - 1])
}

/// 255-style extension bytes after a saturating 4-bit token nibble.
fn push_extension(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}


#[cfg(test)]
mod tests {
    use super::*;

    fn triple(off_base: u32, lit_len: u16, ml_base: u16) -> [u8; 8] {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&off_base.to_le_bytes());
        t[4..6].copy_from_slice(&lit_len.to_le_bytes());
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
        let mut dst = vec![0u8; input.len()];
        let n = crate::lz4dec::decode_block(&block, &mut dst).expect("decode");
        assert_eq!(n, input.len());
        assert_eq!(&dst[..n], input);
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
        // "ab" + a 14-byte match at offset 2 (offBase 5, mlBase 11): token
        // 0x2A, LE16 offset, zero-length tail token 0x00 (the kaelz4
        // rebuild writes it unconditionally)
        let input = b"abababababababab";
        let seqs = triple(2 + 3, 2, 11);
        let block = rebuild(input, &seqs);
        assert_eq!(block, [0x2A, b'a', b'b', 0x02, 0x00, 0x00]);
        roundtrip(input, &seqs);
    }

    #[test]
    fn match_zero_triple_credits_literals_plus_three() {
        // mlBase 0 with litLength 5 puts 8 literals into the next match's
        // literal run (5 + 3): literals "abcdabcd", then a 4-byte match at
        // offset 8 (offBase 11), then the 8-byte tail as the final literal
        // run
        let input = b"abcdabcdabcd";
        let seqs = [triple(0, 5, 0), triple(8 + 3, 0, 1)].concat();
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
        let seqs = [triple(2 + 3, 300, 297)].concat();
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
        let seqs = triple(1 + 3, 4, 1); // offset 1 after 4 literals: fine
        roundtrip(&input, &seqs);
        let seqs = [triple(0, 4, 0), triple(1 + 3, 0, 1)].concat(); // offset 1 at 7B output: fine
        roundtrip(&input, &seqs);
        let bad = [triple(1 + 3, 0, 1)].concat(); // offset 1 at 0B output
        assert!(lz77_block(&input, &bad, &mut Vec::new()).is_err());
    }

    #[test]
    fn oversized_offset_and_literal_overrun_are_rejected() {
        let input = [0u8; 32];
        let seqs = [triple(0xFFFF + 3, 4, 1)].concat(); // offset 65536
        assert!(lz77_block(&input, &seqs, &mut Vec::new()).is_err());
        let seqs = [triple(1 + 3, 33, 1)].concat(); // 33 literals in a 32B input
        assert!(lz77_block(&input, &seqs, &mut Vec::new()).is_err());
    }

    #[test]
    fn long_input_tail_beyond_the_last_match() {
        // match in the middle, 40-byte literal tail after it
        let mut input = vec![9u8; 50];
        input.extend_from_slice(b"0123456789abcdefghij0123456789abcdefghij01234567");
        let seqs = [triple(1 + 3, 3, 29)].concat(); // 3 literals + 32-byte match at offset 1
        roundtrip(&input, &seqs);
    }

    /// The register holds the last three explicitly-coded offsets:
    /// offBase 1..3 read its entries and leave them alone. The hardware
    /// proof is the text -> zeros page boundary: after an explicit
    /// offset 16 and sixteen code-1 matches, the boundary match is
    /// (code 3, lit 1) — the fresh-frame third entry (offset 1) must
    /// have survived every code-1 use untouched.
    #[test]
    fn repeat_codes_read_the_register_without_updating_it() {
        let seqs = [
            triple(6 + 3, 8, 1), // explicit offset 6 -> register [6, 1, 4]
            triple(1, 0, 1),     // code 1 -> 6, register unchanged
            triple(3, 0, 1),     // code 3 -> 4, register unchanged
            triple(2 + 3, 0, 1), // explicit offset 2 -> register [2, 6, 1]
            triple(3, 0, 1),     // code 3 -> 1 (RLE)
        ]
        .concat();
        let input = [
            b"abcdefgh".as_slice(),
            b"cdef", // offset 6 match
            b"ghcd", // code 1 -> offset 6
            b"ghcd", // code 3 -> offset 4
            b"cdcd", // offset 2 match
            b"dddd", // code 3 -> offset 1 (RLE)
            b"Z",    // tail
        ]
        .concat();
        roundtrip(&input, &seqs);
    }

    /// A literal-only triple (mlBase 0) folds its bytes into the next
    /// match's literal run and leaves the match-offset register alone:
    /// the codes after it still read the matches before it.
    #[test]
    fn repeat_codes_after_a_literal_only_triple_see_the_folded_literal_run() {
        let seqs = [
            triple(6 + 3, 8, 1), // literals "abcdefgh", match at offset 6
            triple(4 + 3, 0, 1), // explicit offset 4 -> rep [4, 6, 1]
            triple(0, 5, 0),     // literal-only triple: 8 pending literals
            triple(3, 0, 1),     // code 3 -> rep[2] = 1 (RLE)
            triple(0, 2, 0),     // literal-only triple: 5 pending literals
            triple(1, 0, 1),     // code 1 -> rep[0] = 4
        ]
        .concat();
        let input = [
            b"abcdefgh".as_slice(),
            b"cdef",     // offset 6 match
            b"cdef",     // offset 4 match
            b"ijklmnop", // folded literals
            b"pppp",     // offset 1 RLE match
            b"abcde",    // folded literals
            b"bcde",     // offset 4 match
            b"Z",        // tail
        ]
        .concat();
        roundtrip(&input, &seqs);
    }

    /// Known answer from a hardware sample: the engine's parse of a
    /// 256-byte page tiled to 4096B — 256 literals, one explicit offset
    /// 256, then repeat codes to 4080, leaving a 16-byte tail for the
    /// final literal run.
    #[test]
    fn hardware_sample_parse_rebuilds_to_a_decodable_block() {
        let page: Vec<u8> = (0..256usize).map(|i| (i * 31 + 7) as u8).collect();
        let input = page.repeat(16);
        let mut seqs = Vec::new();
        seqs.extend(triple(259, 256, 189));
        seqs.extend(triple(259, 0, 249));
        for _ in 0..13 {
            seqs.extend(triple(1, 0, 249));
        }
        seqs.extend(triple(1, 0, 101));
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
        let seqs = [triple(0, 2055, 0), triple(10 + 3, 0, 4083)].concat();
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
