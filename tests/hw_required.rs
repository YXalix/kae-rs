//! Hardware-required tests: a real KAE machine (/dev/uacce* + the patched
//! liblz4 on a default probe path). Marked `#[ignore]` so the default suite
//! stays green on hardware-less dev machines; on the target machine run:
//! `cargo test -- --ignored` (or `--include-ignored` for both suites).

use kae::{AsyncBatch, Backend, KaeLz4};

fn require_hw() {
    assert!(
        kae::hw_available(),
        "needs a KAE machine (/dev/uacce* and the patched liblz4); \
         run on the target machine with: cargo test -- --ignored"
    );
}

fn make_random(n: usize, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; n];
    let mut x = seed | 1;
    for chunk in v.chunks_mut(8) {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let b = x.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    v
}

/// 4K-page interleave: zeros / pseudo-random / repeating pattern.
fn make_mixed(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    for (page, p) in v.chunks_mut(4096).enumerate() {
        match page % 3 {
            0 => {}
            1 => p.copy_from_slice(&make_random(p.len(), 0xC0FF_EE00 + page as u64)),
            _ => {
                for (i, b) in p.iter_mut().enumerate() {
                    *b = (i % 251) as u8;
                }
            }
        }
    }
    v
}

#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn hw_block_roundtrip_matrix() {
    require_hw();
    let kae = KaeLz4::open_hw().expect("open_hw on a KAE machine");
    assert_eq!(kae.backend(), Backend::Hw);
    eprintln!("lib={} note={}", kae.lib_path(), kae.note());

    for size in [1, 100, 4095, 4096, 4097, 65536, 1_048_576] {
        for (name, block) in [
            ("zeros", vec![0u8; size]),
            ("random", make_random(size, 0xDEAD_BEEF ^ size as u64)),
            ("mixed", make_mixed(size)),
        ] {
            let compressed = kae.compress_vec(&block).expect("compress_vec");
            assert!(
                compressed.len() <= kae.bound(size),
                "{name}/{size}: compressed size exceeds bound"
            );
            let mut restored = vec![0u8; size];
            let n = kae
                .decompress(&compressed, &mut restored)
                .expect("decompress");
            assert_eq!(n, size, "{name}/{size}: decompressed length");
            assert_eq!(&restored[..n], &block[..], "{name}/{size}: bit-exact match");
        }
    }

    // The dst-capacity API with an exact-bound buffer
    let block = make_mixed(4096);
    let mut dst = vec![0u8; kae.bound(4096)];
    let n = kae.compress(&block, &mut dst).expect("compress");
    let mut restored = vec![0u8; 4096];
    let n2 = kae
        .decompress(&dst[..n], &mut restored)
        .expect("decompress");
    assert_eq!(n2, 4096, "decompressed length");
    assert_eq!(&restored[..n2], &block[..], "bit-exact match");
}

#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn hw_output_roundtrips_through_stock_liblz4() {
    require_hw();
    // The patched library carries the SONAME liblz4.so.1: once it is loaded,
    // a dlopen by that soname resolves to it, so the stock handle must be
    // opened first. All calls go through per-handle function pointers, so
    // both copies coexist safely (RTLD_LOCAL).
    let sw = KaeLz4::open_sw().expect("stock system liblz4");
    let hw = KaeLz4::open_hw().expect("patched liblz4");
    assert_eq!(hw.backend(), Backend::Hw);

    for size in [4096, 65536, 1_048_576] {
        let mixed = make_mixed(size);
        let random = make_random(size, 0xABCD ^ size as u64);

        // hw-compressed output must decode unchanged on the stock library
        for (name, block) in [("mixed", &mixed), ("random", &random)] {
            let packed = hw.compress_vec(block).expect("hw compress");
            let mut restored = vec![0u8; size];
            let n = sw
                .decompress(&packed, &mut restored)
                .expect("stock decompress of hw output");
            assert_eq!(n, size, "{name}/{size}: decompressed length");
            assert_eq!(&restored[..n], &block[..], "{name}/{size}: bit-exact match");
        }

        // and stock-compressed output must decode on the patched library
        for (name, block) in [("mixed", &mixed), ("random", &random)] {
            let packed = sw.compress_vec(block).expect("sw compress");
            let mut restored = vec![0u8; size];
            let n = hw
                .decompress(&packed, &mut restored)
                .expect("patched decompress of stock output");
            assert_eq!(n, size, "{name}/{size}: decompressed length");
            assert_eq!(&restored[..n], &block[..], "{name}/{size}: bit-exact match");
        }
    }
}

#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn async_batch_segment_roundtrip() {
    require_hw();
    // Tuning must precede the first AsyncBatch::open of the process (the
    // engine init is one-shot and reads it there)
    AsyncBatch::configure(Some(2), Some(1)).expect("configure before open");

    // Stock handle first: after AsyncBatch::open the patched library (SONAME
    // liblz4.so.1) is loaded, and a soname dlopen would resolve to it
    let sw = KaeLz4::open_sw().expect("stock system liblz4");
    let mut batch = AsyncBatch::open().expect("AsyncBatch on a KAE machine");

    for (name, segment) in [
        ("mixed", make_mixed(100_000)), // 24 whole blocks + tail
        ("random", make_random(100_000, 0xFEED)),
        ("exact", make_mixed(8192)), // block multiple, no tail
    ] {
        let block = 4096;
        let mut packed = Vec::new();
        let lens = batch
            .compress_segment(&segment, block, &mut packed)
            .expect("compress_segment");
        let expected = segment.len() / block + usize::from(segment.len() % block != 0);
        assert_eq!(lens.len(), expected, "{name}: one entry per block (+tail)");
        assert_eq!(
            lens.iter().sum::<usize>(),
            packed.len(),
            "{name}: entries must pack contiguously"
        );

        let mut restored = Vec::with_capacity(segment.len());
        let mut off = 0;
        for l in &lens {
            let mut out = vec![0u8; block];
            let n = sw
                .decompress(&packed[off..off + l], &mut out)
                .expect("stock decompress of async output");
            restored.extend_from_slice(&out[..n]);
            off += l;
        }
        assert_eq!(off, packed.len(), "{name}: all bytes consumed");
        assert_eq!(restored, segment, "{name}: bit-exact match");
    }

    // Rejection paths before anything reaches the queue
    let mut out = Vec::new();
    assert!(
        matches!(
            batch.compress_segment(&[0u8; 16], 0, &mut out),
            Err(kae::KaeError::Compress(_))
        ),
        "block size 0 must be rejected"
    );
    assert!(
        matches!(
            batch.compress_segment(&[], 4096, &mut out),
            Err(kae::KaeError::Compress(_))
        ),
        "empty segment must be rejected"
    );
}
