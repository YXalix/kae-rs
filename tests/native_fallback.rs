//! `KaeNative`'s open-time fallback contract, enforced on hardware-less
//! machines (the same shape as `async_gating.rs`): without a ZIP device the
//! handle opens in software mode (lz4rip, no C library) instead of failing,
//! reports why, and roundtrips blocks like any other engine. On a machine
//! with devices these tests are no-ops; the hardware behavior is pinned in
//! `hw_required.rs`.

use kae::{native_available, Backend, KaeNative};

#[test]
fn open_without_device_lands_in_soft_mode() {
    if native_available() {
        return;
    }
    let nat = KaeNative::open().expect("soft-mode open never fails");
    assert_eq!(nat.backend(), Backend::Soft);
    assert!(
        nat.note().contains("mode=soft"),
        "the note must say the handle degraded: {}",
        nat.note()
    );
    assert_eq!(nat.hw_requests(), 0);
}

#[test]
fn open_on_lands_in_soft_mode_without_devices() {
    if native_available() {
        return;
    }
    // any device index on a hardware-less machine: the wrapped lookup has
    // nothing to wrap onto, so the handle degrades exactly like open()
    let mut nat = KaeNative::open_on(3, 4096).expect("soft-mode open never fails");
    assert_eq!(nat.backend(), Backend::Soft);
    let compressed = nat.compress_vec(&vec![0xA5u8; 4096]).expect("compress");
    let mut restored = vec![0u8; 4096];
    let n = nat.decompress(&compressed, &mut restored).expect("decompress");
    assert_eq!(n, 4096);
    assert!(restored.iter().all(|&b| b == 0xA5));
}

#[test]
fn open_with_rejects_zero_request_size() {
    assert!(KaeNative::open_with(0).is_err());
}

#[test]
fn soft_mode_roundtrips_every_block_shape() {
    if native_available() {
        return;
    }
    let mut nat = KaeNative::open().unwrap();

    let zero = vec![0u8; 4096];
    let text: Vec<u8> = b"snapshot-page-3 "
        .iter()
        .copied()
        .cycle()
        .take(8192)
        .collect();
    let random = {
        let mut v = vec![0u8; 4096];
        let mut x = 0xDEAD_BEEFu64;
        for chunk in v.chunks_mut(8) {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let b = x.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes();
            chunk.copy_from_slice(&b[..chunk.len()]);
        }
        v
    };
    let tiny = vec![7u8; 10]; // below the 16B hardware floor
    let multi_request = text.repeat(24); // 192K: would chain 128K + 64K requests

    for (name, input) in [
        ("zeros", &zero),
        ("text", &text),
        ("random", &random),
        ("tiny", &tiny),
        ("multi_request", &multi_request),
    ] {
        let compressed = nat.compress_vec(input).expect(name);
        assert!(
            compressed.len() <= nat.bound(input.len()),
            "{name}: compressed size exceeds bound"
        );
        let mut restored = vec![0u8; input.len()];
        let n = nat.decompress(&compressed, &mut restored).expect(name);
        assert_eq!(n, input.len(), "{name}: decompressed length");
        assert_eq!(&restored[..n], &input[..], "{name}: bit-exact match");
    }

    // soft mode never touches hardware and counts every block as software
    assert_eq!(nat.hw_requests(), 0);
    assert!(nat.sw_requests() >= 5, "every block counts: {nat:?}");
}

#[test]
fn soft_mode_compress_segment_packs_blocks_and_tail() {
    if native_available() {
        return;
    }
    let mut nat = KaeNative::open().unwrap();

    let block = 4096;
    let mut segment = vec![0u8; 10 * block + 100];
    // chunks_mut yields the 10 whole blocks plus the 100B tail; each gets
    // its own pattern so every decode is checkable against the segment
    for (page, p) in segment.chunks_mut(block).enumerate() {
        let byte = match page % 3 {
            0 => 0u8,
            1 => 0xA5,
            _ => (page % 251) as u8,
        };
        p.iter_mut().for_each(|b| *b = byte);
    }

    let mut packed = Vec::new();
    let lens = nat.compress_segment(&segment, block, &mut packed).expect("segment");
    assert_eq!(lens.len(), 11, "one entry per whole block, the tail last");
    assert_eq!(lens.iter().sum::<usize>(), packed.len(), "contiguous packing");

    // every entry decodes back to its plain bytes
    let mut off = 0;
    for (i, l) in lens.iter().enumerate() {
        let plain_len = if i < 10 { block } else { 100 };
        let mut restored = vec![0u8; plain_len];
        let n = nat.decompress(&packed[off..off + l], &mut restored).expect("decode");
        assert_eq!(n, plain_len, "entry {i}");
        assert_eq!(&restored[..n], &segment[i * block..i * block + plain_len], "entry {i}");
        off += l;
    }
    assert_eq!(off, packed.len());

    // rejections mirror the async batch contract
    let mut out = Vec::new();
    assert!(matches!(
        nat.compress_segment(&segment, 0, &mut out),
        Err(kae::KaeError::Compress(_))
    ));
    assert!(matches!(
        nat.compress_segment(&[], block, &mut out),
        Err(kae::KaeError::Compress(_))
    ));
    assert_eq!(nat.hw_requests(), 0, "soft mode never touches hardware");
}

/// A producer that walks `data` in chunk order and memcpy-fills every
/// handed buffer — the produce path's control producer (the bytes a real
/// zfile producer would `pread` straight into the handed buffer).
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

#[test]
fn soft_mode_produce_matches_segment_and_roundtrips() {
    let mut nat = KaeNative::open().unwrap();

    let block = 4096;
    let nblocks = 7;
    let mut data = vec![0u8; nblocks * block];
    for (page, p) in data.chunks_mut(block).enumerate() {
        let byte = match page % 3 {
            0 => 0u8,
            1 => 0xA5,
            _ => (page * 7 % 251) as u8,
        };
        p.iter_mut().for_each(|b| *b = byte);
    }

    // the produce pass consumes exactly nblocks blocks, one length each
    let mut produce = memcpy_producer(&data);
    let mut packed = Vec::new();
    let lens = nat
        .compress_segment_from(nblocks, block, &mut packed, &mut produce)
        .expect("produce");
    assert_eq!(lens.len(), nblocks, "no tail: one entry per block");
    assert_eq!(lens.iter().sum::<usize>(), packed.len(), "contiguous packing");

    let mut off = 0;
    for (i, l) in lens.iter().enumerate() {
        let mut restored = vec![0u8; block];
        let n = nat
            .decompress(&packed[off..off + l], &mut restored)
            .expect("decode");
        assert_eq!(n, block, "block {i}");
        assert_eq!(&restored[..n], &data[i * block..(i + 1) * block], "block {i}");
        off += l;
    }
    assert_eq!(off, packed.len());

    // in software mode both sources feed the same lz4rip engine block by
    // block, so the packed bytes match the segment pass exactly (on
    // hardware the two passes can disagree per block only through a
    // transient fallback, so the equality is pinned in soft mode alone)
    if nat.backend() == Backend::Soft {
        let mut seg_packed = Vec::new();
        let seg_lens = nat
            .compress_segment(&data, block, &mut seg_packed)
            .expect("segment");
        assert_eq!(seg_lens.len(), nblocks, "whole blocks only: no tail");
        assert_eq!(packed, seg_packed, "same engine, same bytes");
    }
    assert_eq!(nat.hw_requests(), 0, "soft mode never touches hardware");
}

#[test]
fn produce_rejections_mirror_the_segment_contract() {
    let mut nat = KaeNative::open().unwrap();
    let data = vec![0u8; 4096];
    let mut out = Vec::new();
    assert!(matches!(
        nat.compress_segment_from(1, 0, &mut out, &mut memcpy_producer(&data)),
        Err(kae::KaeError::Compress(_))
    ));
    assert!(matches!(
        nat.compress_segment_from(0, 4096, &mut out, &mut memcpy_producer(&data)),
        Err(kae::KaeError::Compress(_))
    ));
}

#[test]
fn produce_error_ends_the_pass_with_that_error() {
    let mut nat = KaeNative::open().unwrap();
    let block = 4096;

    // a producer that refuses the third chunk: in soft mode the first
    // block stages through it (blocks are ineligible), so the failure
    // surfaces there; on hardware the refill hits it at chunk three.
    // Either way the error is the producer's, not a fallback cause.
    let mut refusing = |buf: &mut [u8]| -> Result<(), kae::KaeError> {
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n == 2 {
            Err(kae::KaeError::Compress("producer gave up".into()))
        } else {
            buf.fill(0);
            Ok(())
        }
    };
    let mut out = Vec::new();
    let err = nat
        .compress_segment_from(4, block, &mut out, &mut refusing)
        .expect_err("the producer's error ends the pass");
    assert!(err.to_string().contains("producer gave up"), "{err}");
}
