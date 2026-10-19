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
