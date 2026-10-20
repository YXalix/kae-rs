//! Hardware-required tests: a real KAE machine (/dev/hisi_zip-* uacce
//! nodes plus the patched liblz4 on a default probe path). Marked
//! `#[ignore]` so the default suite stays green on hardware-less dev
//! machines; on the target machine run: `cargo test -- --ignored` (or
//! `--include-ignored` for both suites).

use kae::{AsyncBatch, Backend, KaeLz4, KaeNative};

fn require_hw() {
    assert!(
        kae::hw_available(),
        "needs a KAE machine (/dev/hisi_zip-* uacce nodes and the patched \
         liblz4); run on the target machine with: cargo test -- --ignored"
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

/// Bit-exact assertion with a compact failure: the label, the first
/// diverging offset and byte pair — never the full buffers.
fn assert_bit_exact(label: &str, got: &[u8], want: &[u8]) {
    if got.len() != want.len() {
        panic!("{label}: decoded {}B of {}B", got.len(), want.len());
    }
    if let Some(d) = got.iter().zip(want).position(|(a, b)| a != b) {
        panic!(
            "{label}: first diff at {d}B: got 0x{:02x} want 0x{:02x}",
            got[d], want[d]
        );
    }
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
            assert_bit_exact(&format!("{name}/{size}"), &restored[..n], &block);
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
            assert_bit_exact(&format!("{name}/{size}"), &restored[..n], block);
        }

        // and stock-compressed output must decode on the patched library
        for (name, block) in [("mixed", &mixed), ("random", &random)] {
            let packed = sw.compress_vec(block).expect("sw compress");
            let mut restored = vec![0u8; size];
            let n = hw
                .decompress(&packed, &mut restored)
                .expect("patched decompress of stock output");
            assert_eq!(n, size, "{name}/{size}: decompressed length");
            assert_bit_exact(&format!("{name}/{size}"), &restored[..n], block);
        }
    }
}

#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn nativesw_hw_roundtrip_and_software_fallback_accounting() {
    require_hw();
    let mut nat = KaeNative::open_with(4096).expect("native open on a KAE machine");
    assert_eq!(nat.backend(), Backend::Native, "device present: native mode");
    assert!(nat.note().contains("mode=native"), "note: {}", nat.note());

    for size in [4096, 8192, 65536, 131072, 200_000] {
        for (name, block) in [
            ("zeros", vec![0u8; size]),
            ("random", make_random(size, 0xDEAD_BEEF ^ size as u64)),
            ("mixed", make_mixed(size)),
        ] {
            let compressed = nat.compress_vec(&block).expect("compress_vec");
            assert!(
                compressed.len() <= nat.bound(size),
                "{name}/{size}: compressed size exceeds bound"
            );
            let mut restored = vec![0u8; size];
            let n = nat.decompress(&compressed, &mut restored).expect("decompress");
            assert_eq!(n, size, "{name}/{size}: decompressed length");
            assert_bit_exact(&format!("{name}/{size}"), &restored[..n], &block);
        }
    }

    // every block went through the engine (hw count), and none fell back —
    // all sizes here sit above the 16B software floor
    assert!(nat.hw_requests() >= 15, "hw={}", nat.hw_requests());
    assert_eq!(nat.sw_requests(), 0, "no fallback expected: {nat:?}");
    assert!(nat.last_hw_error().is_none());

    // a block below the 16B floor goes to software by design
    let tiny = vec![7u8; 10];
    let packed = nat.compress_vec(&tiny).expect("tiny compress");
    let mut restored = vec![0u8; 10];
    let n = nat.decompress(&packed, &mut restored).expect("tiny decompress");
    assert_eq!(&restored[..n], &tiny[..]);
    assert_eq!(nat.sw_requests(), 1);
}

/// The production read path: native-compressed blocks decode on the stock
/// library (the on-disk format is unchanged) and lz4rip decodes both
/// engines' output — the decompress side of KaeNative is software in every
/// mode.
#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn nativesw_output_roundtrips_through_stock_liblz4() {
    require_hw();
    // Stock handle first: once liblz4.so.1 (the patched library's SONAME)
    // is loaded, a soname dlopen resolves to it.
    let sw = KaeLz4::open_sw().expect("stock system liblz4");
    let mut nat = KaeNative::open_with(4096).expect("native open");
    assert_eq!(nat.backend(), Backend::Native);

    for size in [4096, 65536] {
        for (name, block) in [
            ("mixed", make_mixed(size)),
            ("random", make_random(size, 0xABCD ^ size as u64)),
        ] {
            let packed = nat.compress_vec(&block).expect("native compress");
            let mut restored = vec![0u8; size];
            let n = sw
                .decompress(&packed, &mut restored)
                .expect("stock decompress of native output");
            assert_eq!(n, size, "{name}/{size}: decompressed length");
            assert_bit_exact(&format!("{name}/{size}"), &restored[..n], &block);
        }
    }
}

/// The segment batch shape on hardware: whole blocks through the in-flight
/// ring, the tail through software, and every block decodable by the stock
/// library — the production read path consumes exactly this byteshape.
#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn nativesw_hw_segment_roundtrip() {
    require_hw();
    let sw = KaeLz4::open_sw().expect("stock system liblz4");
    let mut nat = KaeNative::open_with(4096).expect("native open");
    assert_eq!(nat.backend(), Backend::Native);

    for block in [4096, 65536, 131072] {
        let segment = make_mixed(8 * block + 100);
        let mut packed = Vec::new();
        let lens = nat
            .compress_segment(&segment, block, &mut packed)
            .expect("compress_segment");
        assert_eq!(lens.len(), 9, "block {block}: 8 whole + tail");
        assert_eq!(lens.iter().sum::<usize>(), packed.len());

        let mut off = 0;
        for (i, l) in lens.iter().enumerate() {
            let plain_len = if i < 8 { block } else { 100 };
            let mut restored = vec![0u8; plain_len];
            let n = sw
                .decompress(&packed[off..off + l], &mut restored)
                .unwrap_or_else(|e| {
                    panic!("block {block} entry {i} ({l}B): stock decode: {e}")
                });
            assert_eq!(n, plain_len, "block {block} entry {i}");
            assert_bit_exact(
                &format!("block {block} entry {i}"),
                &restored[..n],
                &segment[i * block..i * block + plain_len],
            );
            off += l;
        }
    }

    // a segment far deeper than the 1024-entry SQ ring: thousands of
    // refills cross the ring wrap and reuse every buffer many times
    {
        let block = 4096;
        let nblocks = 2050;
        let segment = make_mixed(nblocks * block);
        let mut packed = Vec::new();
        let lens = nat
            .compress_segment(&segment, block, &mut packed)
            .expect("deep compress_segment");
        assert_eq!(lens.len(), nblocks);
        let mut restored = vec![0u8; block];
        let mut off = 0;
        for (i, l) in lens.iter().enumerate() {
            let n = sw
                .decompress(&packed[off..off + l], &mut restored)
                .unwrap_or_else(|e| panic!("deep entry {i}: stock decode: {e}"));
            assert_eq!(n, block, "deep entry {i}");
            assert_bit_exact(
                &format!("deep entry {i}"),
                &restored[..n],
                &segment[i * block..(i + 1) * block],
            );
            off += l;
        }
    }

    // 24 whole blocks + one software tail per block size, plus the deep
    // segment whose blocks all went through the engine
    assert!(
        nat.hw_requests() >= 24 + 2050,
        "hw={} sw={} stuck={} last_hw_error={:?} engine={}",
        nat.hw_requests(),
        nat.sw_requests(),
        nat.stuck(),
        nat.last_hw_error(),
        nat.engine_note()
    );
    assert_eq!(
        nat.sw_requests(),
        4,
        "one tail per block size (last_hw_error={:?})",
        nat.last_hw_error()
    );
    assert!(!nat.stuck());
}

/// The worker-pool scaling shape on hardware: one handle per thread,
/// opened round-robin over the zip devices (`KaeNative::open_on`), the
/// segment sharded into contiguous shares and streamed concurrently —
/// the shape that turns a second zip device into a second queue. Every
/// packed entry must still decode on the stock library, in block order.
#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn nativesw_handle_pool_roundtrip() {
    require_hw();
    let sw = KaeLz4::open_sw().expect("stock system liblz4");
    let workers = 2.min(kae::native_device_count()).max(1);
    let block = 4096;
    let nblocks = 4096; // 16 MiB: deep enough to cross the ring wrap per share
    let segment = make_mixed(nblocks * block);
    let per = nblocks / workers;

    type Share = Result<(Vec<usize>, Vec<u8>), String>;
    let parts: Vec<Share> = std::thread::scope(|s| {
        (0..workers)
            .map(|t| {
                let segment = &segment;
                s.spawn(move || {
                    let mut nat = KaeNative::open_on(t, block).expect("open_on");
                    assert_eq!(nat.backend(), Backend::Native, "handle {t} must be native");
                    let hi = if t + 1 == workers {
                        nblocks * block
                    } else {
                        (t + 1) * per * block
                    };
                    let mut packed = Vec::new();
                    let lens = nat
                        .compress_segment(&segment[t * per * block..hi], block, &mut packed)
                        .expect("share");
                    assert_eq!(
                        lens.len(),
                        hi / block - t * per,
                        "handle {t}: one entry per share block"
                    );
                    assert!(
                        nat.sw_requests() == 0,
                        "handle {t}: no fallback, got sw={} hw={} stuck={} last_hw_error={:?} engine={}",
                        nat.sw_requests(),
                        nat.hw_requests(),
                        nat.stuck(),
                        nat.last_hw_error(),
                        nat.engine_note()
                    );
                    Ok((lens, packed))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|j| j.join().unwrap_or_else(|_| Err("worker panicked".into())))
            .collect()
    });

    // concatenate in thread order — the AsyncBatch byteshape
    let mut lens = Vec::with_capacity(nblocks);
    let mut packed = Vec::new();
    for p in parts {
        let (l, p) = p.expect("share ok");
        lens.extend_from_slice(&l);
        packed.extend_from_slice(&p);
    }
    assert_eq!(lens.len(), nblocks);

    let mut restored = vec![0u8; block];
    let mut off = 0;
    for (i, l) in lens.iter().enumerate() {
        let n = sw
            .decompress(&packed[off..off + l], &mut restored)
            .unwrap_or_else(|e| panic!("pool entry {i}: stock decode: {e}"));
        assert_eq!(n, block, "pool entry {i}");
        assert_bit_exact(
            &format!("pool entry {i}"),
            &restored[..n],
            &segment[i * block..(i + 1) * block],
        );
        off += l;
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

/// The produce path on real silicon: the producer fills the claimed input
/// buffers directly (memcpy here — a real producer reads/pipelines into
/// them), the rebuild reads its literals back out of the held slots, and
/// every block must decode bit-exact through the stock library.
#[test]
#[ignore = "requires a KAE machine: run with cargo test -- --ignored"]
fn hw_native_produce_segment_roundtrip() {
    require_hw();
    let sw = KaeLz4::open_sw().expect("stock system liblz4");
    let mut nat = KaeNative::open().expect("native open");
    assert_eq!(nat.backend(), Backend::Native, "needs the uacce engine path");
    eprintln!("engine={}", nat.engine_note());

    let block = 4096;
    for (name, nblocks) in [("mixed", 64), ("random", 9), ("small", 2)] {
        let data = make_mixed(nblocks * block);
        let mut produce = memcpy_producer(&data);
        let mut packed = Vec::new();
        let lens = nat
            .compress_segment_from(nblocks, block, &mut packed, &mut produce)
            .expect("produce pass");
        assert_eq!(lens.len(), nblocks, "{name}: whole blocks only, no tail");
        assert_eq!(
            lens.iter().sum::<usize>(),
            packed.len(),
            "{name}: entries must pack contiguously"
        );

        let mut off = 0;
        for (i, l) in lens.iter().enumerate() {
            let mut restored = vec![0u8; block];
            let n = sw
                .decompress(&packed[off..off + l], &mut restored)
                .expect("stock decompress of produce output");
            assert_eq!(n, block, "{name}: block {i} length");
            assert_bit_exact(
                &format!("{name}: block {i}"),
                &restored[..n],
                &data[i * block..(i + 1) * block],
            );
            off += l;
        }
        assert_eq!(off, packed.len(), "{name}: all bytes consumed");
    }
    assert!(
        nat.hw_requests() > 0 && nat.sw_requests() == 0,
        "every block rode the engine: hw={} sw={} stuck={} last_hw_error={:?} engine={}",
        nat.hw_requests(),
        nat.sw_requests(),
        nat.stuck(),
        nat.last_hw_error(),
        nat.engine_note()
    );
}

/// A producer that walks `data` in chunk order and memcpy-fills every
/// handed buffer.
fn memcpy_producer<'d>(
    data: &'d [u8],
) -> impl FnMut(&mut [u8]) -> Result<(), kae::KaeError> + 'd {
    let mut pos = 0usize;
    move |buf: &mut [u8]| {
        let end = pos + buf.len();
        data.get(pos..end)
            .ok_or_else(|| kae::KaeError::Compress("producer past the data end".into()))?;
        buf.copy_from_slice(&data[pos..end]);
        pos = end;
        Ok(())
    }
}
