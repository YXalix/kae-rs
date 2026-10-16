//! Rebuild the ZIP engine's lz77 flat output into a standard LZ4 block.
//!
//! The engine returns, per request: a literals area (never read — the
//! literals are taken from the original input, as kaelz4 does) followed by
//! 8-byte sequence triples in the LZ77_ONLY layout (KAELz4's
//! include/kaelz4.h seqDef). One triple, little-endian:
//!
//! ```text
//! [u32 litLength][u16 offBase][u16 mlBase]
//! ```
//!
//! Every offset is explicit: `offBase` is the match offset minus one, and
//! the dialect has no repeat codes (the repeat-coded flavors are the
//! driver's 0x42/0x43 LZ77_ZSTD algorithms; the kaelz4 production path
//! requests LZ77_ONLY, which the driver programs as 0x40 — see
//! uacce::HW_LZ77_ZSTD_NOREPCODE). `mlBase` is the match length minus 3;
//! `mlBase 0` carries no match, and its `litLength` plus 3 bytes fold into
//! the next literal run. `aux` does not exist in this layout — the u32
//! literal length spans the first four bytes. The rebuild emits the LZ4
//! block sequences (token, literals, 2-byte offset, match-length
//! extension) and ends the block with the unmatched input tail as the
//! final literal run — the same arithmetic as kaelz4's own
//! `kaelz4_triples_rebuild`, so the output decodes with the stock liblz4.
//! The engine never parses the last 12 bytes of the input (the request
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

    let mut pending_lit = 0usize;
    let mut ip = 0usize;
    for (ti, t) in seqs.chunks_exact(8).enumerate() {
        let lit_len = u32::from_le_bytes(t[0..4].try_into().unwrap()) as usize;
        let off_base = u16::from_le_bytes(t[4..6].try_into().unwrap()) as usize;
        let ml_base = u16::from_le_bytes(t[6..8].try_into().unwrap()) as usize;
        if ml_base == 0 {
            pending_lit += lit_len + 3;
            continue;
        }
        // the literal run covers the literal-only triples folded in ahead
        // of this match
        let lit = lit_len + pending_lit;
        pending_lit = 0;
        let off = off_base + 1;
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

        // token + literal-length extension + literals
        let token = push_literal_run(out, &input[ip..ip + lit]);
        ip += lit;

        // 2-byte offset. Decoded output so far is exactly the consumed
        // input, so `ip` is the reference the decoder would validate
        // against.
        if off > ip {
            return Err(format!("match offset {off} reaches before the block start (triple {ti})"));
        }
        if off > u16::MAX as usize {
            return Err(format!("match offset {off} does not fit the 2-byte LZ4 field"));
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
    push_literal_run(out, &input[ip..]);
    Ok(out.len())
}

/// Walk the triples exactly the way `lz77_block` does and return one line
/// per triple: the raw fields, the match offset, and the input span
/// consumed. The hardware oracle test prints this so one run on the target
/// machine pins the engine's boundary semantics without any
/// terminal-wrapped hex to transcribe.
#[cfg(test)]
pub(crate) fn trace_triples(input_len: usize, seqs: &[u8]) -> String {
    if seqs.len() % 8 != 0 {
        return format!("triple area is {}B, not a multiple of 8", seqs.len());
    }
    let mut pending_lit = 0usize;
    let mut ip = 0usize;
    let mut lines = Vec::new();
    for (ti, t) in seqs.chunks_exact(8).enumerate() {
        let lit_len = u32::from_le_bytes(t[0..4].try_into().unwrap()) as usize;
        let off_base = u16::from_le_bytes(t[4..6].try_into().unwrap()) as usize;
        let ml_base = u16::from_le_bytes(t[6..8].try_into().unwrap()) as usize;
        if ml_base == 0 {
            pending_lit += lit_len + 3;
            lines.push(format!(
                "t{ti}: fold off_base={off_base} lit={lit_len} pending={pending_lit}"
            ));
            continue;
        }
        let lit = lit_len + pending_lit;
        pending_lit = 0;
        let off = off_base + 1;
        let ml = ml_base + 3;
        let lit_at = ip;
        ip += lit + ml;
        lines.push(format!(
            "t{ti}: match off_base={off_base} lit={lit} at {lit_at} ml={ml} off={off} span {lit_at}..{}",
            (lit_at + lit + ml).min(input_len),
        ));
    }
    lines.push(format!("tail at {ip}: {}B", input_len.saturating_sub(ip)));
    lines.join("\n")
}

/// 255-style extension bytes after a saturating 4-bit token nibble.
fn push_extension(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

/// One literal run: token (match nibble still 0, added by the caller),
/// literal-length extension, then the literal bytes. Returns the token
/// position.
fn push_literal_run(out: &mut Vec<u8>, lits: &[u8]) -> usize {
    let token = out.len();
    out.push(0);
    let lit = lits.len();
    if lit >= RUN_MASK {
        out[token] = (RUN_MASK << ML_BITS) as u8;
        push_extension(out, lit - RUN_MASK);
    } else {
        out[token] = (lit << ML_BITS) as u8;
    }
    out.extend_from_slice(lits);
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One LZ77_ONLY triple: match offset, literals, match length minus 3.
    /// Offset 0 stands for a literal-only triple, whose offset field the
    /// rebuild ignores.
    fn triple(offset: usize, lit_len: u32, ml_base: u16) -> [u8; 8] {
        let mut t = [0u8; 8];
        t[0..4].copy_from_slice(&lit_len.to_le_bytes());
        t[4..6].copy_from_slice(&(offset.saturating_sub(1) as u16).to_le_bytes());
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
        // "ab" + a 14-byte match at offset 2 (offBase 1, mlBase 11): token
        // 0x2A, LE16 offset, zero-length tail token 0x00 (the kaelz4
        // rebuild writes it unconditionally)
        let input = b"abababababababab";
        let seqs = triple(2, 2, 11);
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
        let seqs = [triple(0, 5, 0), triple(8, 0, 1)].concat();
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
        let seqs = [triple(2, 300, 297)].concat();
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
        let seqs = triple(1, 4, 1); // offset 1 after 4 literals: fine
        roundtrip(&input, &seqs);
        let seqs = [triple(0, 4, 0), triple(1, 0, 1)].concat(); // offset 1 at 7B output: fine
        roundtrip(&input, &seqs);
        let bad = [triple(1, 0, 1)].concat(); // offset 1 at 0B output
        assert!(lz77_block(&input, &bad, &mut Vec::new()).is_err());
    }

    #[test]
    fn oversized_offset_and_literal_overrun_are_rejected() {
        let input = [0u8; 32];
        let seqs = [triple(1, 4, 1), triple(1, 0, 1)].concat(); // two offset-1 matches: fine
        assert!(lz77_block(&input, &seqs, &mut Vec::new()).is_ok());
        let seqs = [triple(1, 33, 1)].concat(); // 33 literals in a 32B input
        assert!(lz77_block(&input, &seqs, &mut Vec::new()).is_err());
        // an offset past the 2-byte LZ4 field must be rejected, not
        // truncated: fold 65536 literals, then a match at offset 65536
        let big = vec![0u8; 70_000];
        let seqs = [triple(0, 65_536, 0), triple(65_536, 0, 1)].concat();
        assert!(lz77_block(&big, &seqs, &mut Vec::new()).is_err());
    }

    #[test]
    fn long_input_tail_beyond_the_last_match() {
        // match in the middle, 40-byte literal tail after it
        let mut input = vec![9u8; 50];
        input.extend_from_slice(b"0123456789abcdefghij0123456789abcdefghij01234567");
        let seqs = [triple(1, 3, 29)].concat(); // 3 literals + 32-byte match at offset 1
        roundtrip(&input, &seqs);
    }

    /// A literal-only triple (mlBase 0) folds its bytes into the next
    /// match's literal run.
    #[test]
    fn literal_only_triple_folds_into_the_next_literal_run() {
        let seqs = [
            triple(6, 8, 1), // literals "abcdefgh", match at offset 6
            triple(4, 0, 1), // explicit offset 4
            triple(0, 5, 0), // literal-only triple: 8 pending literals
            triple(1, 0, 1), // offset 1 RLE
            triple(0, 2, 0), // literal-only triple: 5 pending literals
            triple(4, 0, 1), // offset 4 match
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
    /// 256-byte page tiled to 4096B — 256 literals, then explicit offset
    /// 256 matches to 4080, leaving a 16-byte tail for the final literal
    /// run.
    #[test]
    fn hardware_sample_parse_rebuilds_to_a_decodable_block() {
        let page: Vec<u8> = (0..256usize).map(|i| (i * 31 + 7) as u8).collect();
        let input = page.repeat(16);
        let mut seqs = Vec::new();
        seqs.extend(triple(256, 256, 189));
        seqs.extend(triple(256, 0, 249));
        for _ in 0..13 {
            seqs.extend(triple(256, 0, 249));
        }
        seqs.extend(triple(256, 0, 101));
        roundtrip(&input, &seqs);
    }

    /// The trace walks triples exactly like `lz77_block`: same fold
    /// credit, same offsets, same consumed spans.
    #[test]
    fn trace_walks_matches_and_folds_like_the_rebuild() {
        let seqs = [
            triple(6, 8, 1), // literals "abcdefgh", match at offset 6
            triple(6, 0, 1), // offset 6 again
            triple(0, 5, 0), // literal-only: 8 pending literals
            triple(2, 0, 1), // explicit offset 2
        ]
        .concat();
        let lines = trace_triples(64, &seqs);
        assert!(lines.contains(
            "t0: match off_base=5 lit=8 at 0 ml=4 off=6 span 0..12"
        ));
        assert!(lines.contains(
            "t1: match off_base=5 lit=0 at 12 ml=4 off=6 span 12..16"
        ));
        assert!(lines.contains("t2: fold off_base=0 lit=5 pending=8"));
        assert!(lines.contains(
            "t3: match off_base=1 lit=8 at 16 ml=4 off=2 span 16..28"
        ));
        assert!(lines.contains("tail at 28: 36B"));
    }

    /// The property the rebuild exists for: the system liblz4 decodes the
    /// rebuilt block back to the original bytes. Skipped where no system
    /// liblz4 is installed (the hw path re-checks it via the bench verify
    /// op).
    #[test]
    fn rebuild_decodes_with_system_liblz4() {
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
        let seqs = [triple(0, 2055, 0), triple(10, 0, 4083)].concat();
        let block = rebuild(&input, &seqs);
        let mut dst = vec![0u8; input.len()];
        let n = fns
            .decompress_block(&block, &mut dst)
            .expect("liblz4 rejected the rebuilt block");
        assert_eq!(&dst[..n], &input[..]);
    }
}
