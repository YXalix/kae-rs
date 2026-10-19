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
/// `input` into `out` (cleared first); returns the LZ4 block length. The
/// test-side reference for the walk [`lz77_block_from_requests`] applies
/// per request.
#[cfg(test)]
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

    // the unmatched tail is the closing literal run (no match after it)
    push_literal_run(out, &input[ip..]);
    Ok(out.len())
}

/// Check one request's triples against its input — and, when the engine's
/// literal area is available, against the parse it describes — without
/// producing output. A valid lz77 parse satisfies two local rules the
/// rebuild's correctness stands on, and each failure names the side at
/// fault:
///
/// - every match's covered bytes equal the bytes at `position - offset`
///   (the decoder copies output to output, and the output is the input).
///   A violation means the engine emitted a match that does not match.
/// - the engine's literal area, read linearly, is exactly the walk's
///   literal spans (the area never covers the MFLIMIT tail past the parse
///   length). A divergence means the fold credit or literal-length
///   reading is wrong, not the offsets.
///
/// `Ok` with both rules holding and a failing decode would mean the LZ4
/// encoding itself (the token/extension arithmetic) is at fault.
pub(crate) fn audit_triples(
    input: &[u8],
    seqs: &[u8],
    lits: Option<&[u8]>,
) -> Result<(), String> {
    if seqs.len() % 8 != 0 {
        return Err(format!("triple area is {}B, not a multiple of 8", seqs.len()));
    }
    let mut lit_cur = 0usize;
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
        let lit = lit_len + pending_lit;
        pending_lit = 0;
        let at = ip + lit; // the decoded position the match starts at
        if at > input.len() {
            return Err(format!("triple {ti} literals overrun the input"));
        }
        if let Some(area) = lits {
            let n = (area.len() - lit_cur).min(lit);
            if area[lit_cur..lit_cur + n] != input[ip..ip + n] {
                let d = area[lit_cur..lit_cur + n]
                    .iter()
                    .zip(&input[ip..ip + n])
                    .position(|(a, b)| a != b)
                    .unwrap_or(0);
                return Err(format!(
                    "literal area diverges at {cur}B (triple {ti}): engine 0x{engine:02x} vs walk 0x{walk:02x} at +{d}B",
                    cur = lit_cur + d,
                    engine = area[lit_cur + d],
                    walk = input[ip + d],
                ));
            }
            lit_cur += n;
            if n < lit {
                return Err(format!(
                    "literal area ends after {lit_cur}B; triple {ti} still expects literals"
                ));
            }
        }
        let off = off_base + 1;
        let ml = ml_base + 3;
        if off > at {
            return Err(format!(
                "triple {ti} offset {off} reaches before the block start ({at}B decoded)"
            ));
        }
        if at + ml > input.len() {
            return Err(format!(
                "triple {ti} match overruns the input ({at}+{ml}>{}B)",
                input.len()
            ));
        }
        if input[at..at + ml] != input[at - off..at + ml - off] {
            let d = input[at..at + ml]
                .iter()
                .zip(&input[at - off..at + ml - off])
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            return Err(format!(
                "triple {ti} match does not match its offset {off} (ml {ml}): input[{}+{d}] is 0x{:02x}, input[{}+{d}] is 0x{:02x}",
                at,
                input[at + d],
                at - off,
                input[at - off + d],
            ));
        }
        ip = at + ml;
    }
    if ip > input.len() {
        return Err(format!(
            "triples consume {}B of a {}B input",
            ip,
            input.len()
        ));
    }
    if let Some(area) = lits {
        // the tail run covers everything up to the input end, so any
        // literal bytes the walk did not reach are extra engine output
        let tail = (area.len() - lit_cur).min(input.len() - ip);
        if area[lit_cur..lit_cur + tail] != input[ip..ip + tail] {
            let d = area[lit_cur..lit_cur + tail]
                .iter()
                .zip(&input[ip..ip + tail])
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            return Err(format!(
                "literal area diverges at {}B in the tail run: engine 0x{:02x} vs walk 0x{:02x}",
                lit_cur + d,
                area[lit_cur + d],
                input[ip + d],
            ));
        }
        if lit_cur + tail < area.len() {
            return Err(format!(
                "literal area has {} trailing bytes the walk never reached",
                area.len() - lit_cur - tail
            ));
        }
    }
    Ok(())
}

/// Rebuild one bench block from its requests' triples. The requests'
/// inputs are consecutive slices of `input`; `requests` carries each
/// request's (triples, input length) in order. A finished LZ4 block is
/// not composable (its closing literal run is a parser-level end
/// marker), so the per-request streams are never spliced: the triples
/// walk ONCE over the whole block, and at each request boundary the
/// previous request's unconsumed tail (its parse never covers the
/// MFLIMIT bytes) is charged into the next match's literal length. A
/// fold chain left dangling by the request's end is dropped there: its
/// bytes sit inside that tail, so charging both would shift the next
/// request's whole parse. That keeps every match sitting at `prior
/// request lengths + its local position`, which is exactly where its
/// offset references the same data it referenced inside its own
/// request. Returns the block length.
pub(crate) fn lz77_block_from_requests(
    input: &[u8],
    requests: &[(&[u8], usize)],
    out: &mut Vec<u8>,
) -> Result<usize, String> {
    out.clear();
    out.reserve(input.len() + input.len() / 255 + 16);

    let mut pending = 0usize;
    let mut ip = 0usize; // global walk position
    let mut skip = 0usize; // boundary shortfall charged to the next match
    let mut boundary = 0usize; // input bytes the finished requests cover
    for (seqs, len) in requests {
        boundary += len;
        if seqs.len() % 8 != 0 {
            return Err(format!("triple area is {}B, not a multiple of 8", seqs.len()));
        }
        for (ti, t) in seqs.chunks_exact(8).enumerate() {
            let mut lit_len = u32::from_le_bytes(t[0..4].try_into().unwrap()) as usize;
            let off_base = u16::from_le_bytes(t[4..6].try_into().unwrap()) as usize;
            let ml_base = u16::from_le_bytes(t[6..8].try_into().unwrap()) as usize;
            if ml_base == 0 {
                pending += lit_len + 3;
                continue;
            }
            if skip > 0 {
                lit_len += skip;
                skip = 0;
            }
            let lit = lit_len + pending;
            pending = 0;
            let off = off_base + 1;
            let nib = ml_base - 1;
            let ml = nib + 4;
            if ip + lit > input.len() {
                return Err(format!(
                    "triple literals overrun the input (at {ip}+{lit}/{}B)",
                    input.len()
                ));
            }
            let token = push_literal_run(out, &input[ip..ip + lit]);
            ip += lit;
            if off > ip {
                return Err(format!(
                    "match offset {off} reaches before the block start (triple {ti})"
                ));
            }
            if off > u16::MAX as usize {
                return Err(format!("match offset {off} does not fit the 2-byte LZ4 field"));
            }
            out.extend_from_slice(&(off as u16).to_le_bytes());
            if nib >= ML_MASK {
                out[token] += ML_MASK as u8;
                push_extension(out, nib - ML_MASK);
            } else {
                out[token] += nib as u8;
            }
            ip += ml;
        }
        // a fold chain open at the request's end claims bytes the tail
        // charge below already covers; the per-request rebuild drops it
        // into the closing tail run the same way
        pending = 0;
        skip += boundary.saturating_sub(ip);
    }
    if ip > input.len() {
        return Err(format!(
            "triples consume {}B of a {}B input",
            ip,
            input.len()
        ));
    }
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

/// Walk one request's triples exactly the way the rebuild does and
/// report the consumed end and the fold-row count — the numbers a failed
/// rebuild check needs to judge each request's boundary charge. The
/// pending fold credit rides into the next match within the request and
/// is dropped at the request's end (its bytes are tail bytes), so it is
/// not part of the consumed end.
pub(crate) fn walk_stats(seqs: &[u8]) -> (usize, usize) {
    let mut pending = 0usize;
    let mut ip = 0usize;
    let mut folds = 0usize;
    for t in seqs.chunks_exact(8) {
        let lit = u32::from_le_bytes(t[0..4].try_into().unwrap()) as usize;
        let ml_base = u16::from_le_bytes(t[6..8].try_into().unwrap()) as usize;
        if ml_base == 0 {
            pending += lit + 3;
            folds += 1;
            continue;
        }
        ip += lit + pending + ml_base + 3;
        pending = 0;
    }
    (ip, folds)
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

    /// One block, several requests: the requests' triples rebuild through
    /// [`lz77_block_from_requests`], which charges each request's
    /// unconsumed tail into the next match's literal length so every
    /// offset keeps referencing the data it referenced in its own
    /// request. (Plain triple concatenation would shift request 2's
    /// matches by request 1's tail and misread every early offset.)
    #[test]
    fn request_triples_rebuild_as_one_block() {
        let half = b"abcdefghabcdefghzzzzzzzz"; // a real off-8 match at 10
        let input1 = half.as_slice();
        let input2 = half.as_slice();
        let input = [input1, input2].concat();
        // fold credit 4 + 6 literals puts the match at 10; it covers
        // 10..16 copying from 2..8, and the 8-byte tail closes the request
        // (the fold's credit is its lit plus 3)
        let t = [triple(1, 1, 0), triple(8, 6, 3)].concat();
        audit_triples(input1, &t, None).expect("self-consistent request");
        // each request alone rebuilds and decodes
        let mut b1 = Vec::new();
        let n1 = lz77_block(input1, &t, &mut b1).expect("request 1");
        let mut dst1 = vec![0u8; input1.len()];
        assert_eq!(
            crate::lz4dec::decode_block(&b1[..n1], &mut dst1).expect("decode 1"),
            input1.len()
        );
        assert_eq!(dst1, input1);
        // the two-request block rebuilds as one walk over the whole input
        let mut block = Vec::new();
        let n = lz77_block_from_requests(
            &input,
            &[(t.as_slice(), input1.len()), (t.as_slice(), input2.len())],
            &mut block,
        )
        .expect("combined rebuild");
        let mut dst = vec![0u8; input.len()];
        assert_eq!(
            crate::lz4dec::decode_block(&block[..n], &mut dst).expect("combined decode"),
            input.len()
        );
        assert_eq!(dst, input);
    }

    /// The audit passes a valid parse — including its literal area — and
    /// pins each fault class to its side.
    #[test]
    fn audit_judges_matches_and_the_literal_area() {
        let input = b"abcdefgh abcdefgh zzzzzzzz";
        let seqs = [
            triple(9, 10, 4), // "abcdefgh " + 7-byte match at offset 9
            triple(1, 3, 0),  // fold: its literals land in the closing tail
        ]
        .concat();
        // walk: lit 10 + ml 7 cover 0..17; the fold's credit rides in
        // pending and the 9-byte tail closes the input
        audit_triples(input, &seqs, None).expect("self-consistent parse");

        // a match that does not match: offset 8 instead of 9
        let bad_off = [triple(8, 10, 4)].concat();
        let e = audit_triples(input, &bad_off, None).unwrap_err();
        assert!(e.contains("does not match its offset"), "{e}");

        // a match overrunning the input
        let over = [triple(9, 10, 40)].concat();
        let e = audit_triples(input, &over, None).unwrap_err();
        assert!(e.contains("overruns the input"), "{e}");

        // the engine's literal area diverges from the walked spans
        let mut area = input[..10].to_vec();
        area[3] ^= 0xff;
        let e = audit_triples(input, &seqs, Some(&area)).unwrap_err();
        assert!(e.contains("literal area diverges"), "{e}");

        // the correct area agrees and the trailing bytes are the tail
        let area = input[..10].to_vec();
        audit_triples(input, &seqs, Some(&area)).expect("area agrees");
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

    /// `walk_stats` mirrors the rebuild's accounting: folds pile credit
    /// without advancing, matches consume literals plus match length.
    #[test]
    fn walk_stats_mirrors_the_rebuild_walk() {
        let seqs = [triple(1, 1, 0), triple(8, 6, 3)].concat();
        assert_eq!(walk_stats(&seqs), (16, 1));
        assert_eq!(walk_stats(&[]), (0, 0));
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

    /// A minimal greedy lz77 parser over the input, emitting triples in
    /// the engine's dialect: real back-references only, explicit offsets,
    /// folds for the literal gaps, and — sometimes — a trailing fold
    /// chain that closes the input exactly (the empty-tail shape). The
    /// point is coverage of the segment shapes the engine produces, not
    /// its parse quality.
    struct Gen {
        input: Vec<u8>,
        head: Vec<usize>,
        rng: u64,
    }

    impl Gen {
        fn new(seed: u64) -> Gen {
            let mut x = seed | 1;
            let mut next = move || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            let mut input = Vec::new();
            for _ in 0..12 {
                match next() % 3 {
                    0 => input.extend(std::iter::repeat_n(0u8, 4096)),
                    1 => {
                        let period = 3 + (next() % 29) as usize;
                        let ch = b'a' + (next() % 26) as u8;
                        let p = period as u32;
                        let c = ch;
                        input.extend((0..4096u32).map(move |i| ((i * p) as u8).wrapping_add(c)));
                    }
                    _ => {
                        for _ in 0..4096 {
                            input.push(next() as u8);
                        }
                    }
                }
            }
            Gen { input, head: vec![usize::MAX; 1 << 16], rng: next() | 1 }
        }

        fn rand(&mut self) -> usize {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng >> 17) as usize
        }

        /// One request's triples, mirroring the rebuild walk's accounting
        /// exactly: folds only pile credit (lit+3) without advancing the
        /// position, matches flush the credit plus their own literals and
        /// then consume the match. Greedy 4-byte-hash matching, and a 10%
        /// chance of letting the last match run into the MFLIMIT tail so
        /// the walk lands exactly on the input end (empty tail).
        fn parse(&mut self, mflimit_tail: bool) -> (usize, Vec<u8>) {
            let end = self.input.len() - if mflimit_tail { 12 } else { 0 };
            let mut triples: Vec<u8> = Vec::new();
            let mut ip = 0usize; // literals + matches consumed so far
            let mut pending = 0usize; // fold credit awaiting the next match
            loop {
                if end <= ip {
                    break; // an MFLIMIT-overrun match ended the request
                }
                // sometimes drop a fold bridging toward the next match
                if self.rand() % 5 == 0 && end - ip > pending + 4 {
                    let lit = 1 + self.rand() % 40;
                    let lit = lit.min(end - ip - pending - 1);
                    triples.extend(triple(0, lit as u32, 0));
                    pending += lit + 3;
                    continue;
                }
                let lit0 = if pending == 0 && self.rand() % 2 == 0 { self.rand() % 32 } else { 0 };
                let lit0 = lit0.min(end.saturating_sub(ip + pending)).min(31);
                let at = ip + pending + lit0;
                if at + 4 > end {
                    break;
                }
                let key = (self.input[at] as usize) | ((self.input[at + 1] as usize) << 8);
                let mut off = 0usize;
                let mut ml = 0usize;
                let cand = self.head[key];
                if cand != usize::MAX && cand < at {
                    let off_c = at - cand;
                    if off_c <= 16384 {
                        let max = (end - at).min(65536);
                        let mut l = 0usize;
                        while l < max && self.input[at + l] == self.input[at - off_c + l] {
                            l += 1;
                        }
                        if l >= 4 {
                            off = off_c;
                            ml = l;
                        }
                    }
                }
                self.head[key] = at;
                if ml >= 4 {
                    // 10% of matches overrun MFLIMIT onto the input end
                    // when the bytes continue, leaving no tail
                    if mflimit_tail && self.rand() % 10 == 0 {
                        let mut l = ml;
                        while at + l < self.input.len()
                            && self.input[at + l] == self.input[at - off + l]
                        {
                            l += 1;
                        }
                        if at + l == self.input.len() {
                            ml = l;
                        }
                    }
                    triples.extend(triple(off, lit0 as u32, (ml - 3) as u16));
                    ip = at + ml;
                    pending = 0;
                } else if end - at >= 4 {
                    // no match here: a 1-literal fold consumes 4 bytes
                    triples.extend(triple(0, 1, 0));
                    pending += 4;
                } else {
                    break;
                }
            }
            (ip, triples)
        }
    }

    /// A request that ends ON a fold must not carry its credit into the
    /// next request: the fold's bytes sit inside the MFLIMIT tail the
    /// boundary charge already covers, so carrying it shifts the whole
    /// next request and every offset copies from before its true source.
    #[test]
    fn request_ending_on_a_fold_drops_its_credit_at_the_boundary() {
        // request 1: a match to 10, then a fold whose credit claims
        // 10..17 — all inside the 22-byte unparsed tail of the 32B input
        let t1 = [triple(2, 2, 5), triple(0, 4, 0)].concat();
        let input1 = b"abababababzzzzzzzzzzzzzzzzzzzzzz";
        audit_triples(input1, &t1, None).expect("request 1 parse");

        // request 2: a match at 4 (offset 3) whose source moves if the
        // walk shifts by the 7-byte credit
        let t2 = [triple(3, 4, 1)].concat();
        let input2 = b"ABCDBCDBEFGHIJKLMNOPQRSTUVWXYZ012345";
        audit_triples(input2, &t2, None).expect("request 2 parse");

        let input = [input1.as_slice(), input2.as_slice()].concat();
        let mut block = Vec::new();
        let n = lz77_block_from_requests(
            &input,
            &[(&t1, input1.len()), (&t2, input2.len())],
            &mut block,
        )
        .unwrap_or_else(|e| panic!("combined rebuild: {e}"));
        let mut dst = vec![0u8; input.len()];
        assert_eq!(
            crate::lz4dec::decode_block(&block[..n], &mut dst).expect("combined decode"),
            input.len()
        );
        assert_eq!(&dst, &input);
    }

    /// The property the bench's block assembly stands on: per-request
    /// triples, concatenated, rebuild the concatenated inputs as ONE walk
    /// that decodes back to the whole block — across random parse shapes,
    /// empty tails (a match landing exactly on the input end) included.
    #[test]
    fn joined_random_parses_rebuild_as_one_block() {
        let mut g = Gen::new(0x5EED_2026);
        let mut checked = 0usize;
        let mut empty_tails = 0usize;
        for _ in 0..300 {
            let (ip1, t1) = g.parse(true);
            let (_ip2, t2) = g.parse(false);
            let input1 = g.input.clone();
            let input2 = g.input.clone();
            if ip1 == input1.len() {
                empty_tails += 1;
            }
            // each request's triples are self-consistent lz77 of its input
            audit_triples(&input1, &t1, None)
                .unwrap_or_else(|e| panic!("request 1 audit: {e}"));
            audit_triples(&input2, &t2, None)
                .unwrap_or_else(|e| panic!("request 2 audit: {e}"));
            // the two requests rebuild as one walk over the whole input
            let input = [input1.as_slice(), input2.as_slice()].concat();
            let mut block = Vec::new();
            let n = lz77_block_from_requests(
                &input,
                &[(&t1, input1.len()), (&t2, input2.len())],
                &mut block,
            )
            .unwrap_or_else(|e| panic!("combined rebuild: {e}"));
            let mut dst = vec![0u8; input.len()];
            let decoded = crate::lz4dec::decode_block(&block[..n], &mut dst)
                .unwrap_or_else(|e| panic!("combined decode failed: {e}"));
            assert_eq!(decoded, input.len());
            assert_eq!(&dst, &input);
            checked += 1;
        }
        assert!(
            checked > 100 && empty_tails > 0,
            "checked {checked}, empty tails {empty_tails}"
        );
    }
}
