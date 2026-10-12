//! End-to-end library test: open -> compress -> decompress -> bit-exact.
//! On machines without KAE hardware, open() falls back to the Sw backend and
//! still exercises the software path and the ABI wrapper.

use kae::{Backend, KaeLz4};

fn make_block(n: usize, seed: u64) -> Vec<u8> {
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

#[test]
fn roundtrip_and_backend_report() {
    let kae = KaeLz4::open().expect("open() must at least fall back to the system liblz4");
    eprintln!(
        "backend={:?} lib={} note={}",
        kae.backend(),
        kae.lib_path(),
        kae.note()
    );

    // zfile granularity: 4K blocks; both compressible and incompressible content
    for (name, block) in [
        ("compressible", vec![0u8; 4096]),
        ("random", make_block(4096, 0xDEAD_BEEF)),
    ] {
        let compressed = kae.compress_vec(&block).expect("compress_vec");
        assert!(compressed.len() <= kae.bound(block.len()));
        let mut restored = vec![0u8; block.len() + 64];
        let n = kae
            .decompress(&compressed, &mut restored)
            .expect("decompress");
        assert_eq!(n, block.len(), "{name}: decompressed length");
        assert_eq!(&restored[..n], &block[..], "{name}: bit-exact match");
    }
}

#[test]
fn open_hw_never_silently_falls_back() {
    // On machines without hardware this must report Unavailable, never hand
    // out a software backend silently
    match KaeLz4::open_hw() {
        Ok(k) => assert_eq!(k.backend(), Backend::Hw, "open_hw can only return Hw"),
        Err(e) => assert!(
            matches!(e, kae::KaeError::Unavailable),
            "expected Unavailable, got {e}"
        ),
    }
}

#[test]
fn hw_available_matches_open_hw() {
    match KaeLz4::open_hw() {
        Ok(k) => {
            assert!(
                kae::hw_available(),
                "open_hw succeeded but hw_available() is false"
            );
            assert_eq!(k.backend(), Backend::Hw, "open_hw can only return Hw");
        }
        Err(e) => {
            assert!(
                !kae::hw_available(),
                "hw_available() is true but open_hw failed: {e}"
            );
            assert!(
                matches!(e, kae::KaeError::Unavailable),
                "expected Unavailable, got {e}"
            );
        }
    }
}
