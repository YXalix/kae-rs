//! Block API walkthrough: open a backend, see which one came up, roundtrip
//! blocks through compress/decompress. Runs on any machine with a liblz4 —
//! the hardware path engages automatically when the patched library is
//! present.
//!
//! ```bash
//! cargo run --example block
//! ```

use kae::{hw_available, Backend, KaeLz4};

fn main() -> Result<(), kae::KaeError> {
    // Probe: true when the patched library is findable AND a ZIP engine
    // device node (/dev/hisi_zip-*) exists; false does not prevent open() —
    // the software fallback remains.
    println!("hw_available: {}", hw_available());

    // "Accelerate if present": hardware first, system liblz4 as fallback.
    let lz4 = KaeLz4::open().expect("no usable liblz4 at all");
    println!("backend:      {:?}", lz4.backend());
    println!("lib:          {}", lz4.lib_path());
    if !lz4.note().is_empty() {
        // Non-empty means the RTLD_GLOBAL preload retry was needed
        // (missing rpaths on the patched library).
        println!("note:         {}", lz4.note());
    }
    match lz4.backend() {
        Backend::Hw => println!("=> patched liblz4 loaded (KAE compress path)"),
        Backend::Sw => println!("=> system liblz4 (software baseline)"),
        // unreachable from KaeLz4 — these are KaeNative's modes
        Backend::Native | Backend::Soft => println!("=> {:?}", lz4.backend()),
    }

    // The policy alternative: hardware only, no silent fallback — when this
    // fails, keep the caller's existing software path instead.
    match KaeLz4::open_hw() {
        Ok(hw) => println!("open_hw:      {}", hw.lib_path()),
        Err(e) => println!("open_hw:      {e}"),
    }

    // One block at zfile granularity (4K).
    let block: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

    // Capacity-managed variant: returns a fresh Vec sized to the result.
    let compressed = lz4.compress_vec(&block)?;
    println!(
        "compress:     {} B -> {} B ({:.1}% of original)",
        block.len(),
        compressed.len(),
        100.0 * compressed.len() as f64 / block.len() as f64
    );

    // Caller-managed variant: dst capacity must be >= bound(src.len());
    // reusing one dst across blocks avoids reallocation in a worker loop.
    let mut dst = vec![0u8; lz4.bound(block.len())];
    let n = lz4.compress(&block, &mut dst)?;
    println!(
        "bound({}) = {}, compress wrote {}",
        block.len(),
        lz4.bound(block.len()),
        n
    );

    // Decompression is software in every backend; dst capacity must be >=
    // the original length and the return value is the bytes produced.
    let mut restored = vec![0u8; block.len()];
    let m = lz4.decompress(&compressed, &mut restored)?;
    assert_eq!(m, block.len());
    assert_eq!(&restored[..m], &block[..]);
    println!("roundtrip OK: {m} bytes restored bit-exact");

    // The zero-C alternative: the crate's own engine path, hardware first
    // with per-request software fallback; without a ZIP device it opens in
    // software mode (lz4rip) and says so in its note. Decompression is
    // software here in every mode (the engine has no LZ4 decode).
    let mut nat = kae::KaeNative::open()?;
    println!(
        "native:       {:?} ({}) hw={} sw={}",
        nat.backend(),
        nat.note(),
        nat.hw_requests(),
        nat.sw_requests()
    );
    let native_compressed = nat.compress_vec(&block)?;
    let mut restored2 = vec![0u8; block.len()];
    let m2 = nat.decompress(&native_compressed, &mut restored2)?;
    assert_eq!(m2, block.len());
    assert_eq!(&restored2[..m2], &block[..]);
    println!("native roundtrip OK: {m2} bytes restored bit-exact");

    Ok(())
}
