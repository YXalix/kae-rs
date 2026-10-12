//! Segment-level async batch: submit a whole segment's blocks to the KAE
//! task queue in one call and collect the per-block compressed lengths.
//! Requires a KAE machine (the patched liblz4, which has the async
//! symbols); on anything else the open fails with no software fallback —
//! this example prints why and exits.
//!
//! ```bash
//! cargo run --example async_batch
//! ```

use kae::{AsyncBatch, KaeLz4};

fn main() {
    // Optional tuning: library thread pool size and compression level.
    // Must precede the first AsyncBatch::open of the process — engine init
    // is one-shot and reads these values there. (Environment variables
    // KAE_LZ4_ASYNC_THREAD_NUM / KAE_LZ4_LEVEL set the same things.)
    AsyncBatch::configure(Some(8), Some(1)).expect("configure before open");

    let mut batch = match AsyncBatch::open() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("AsyncBatch::open failed on this machine: {e}");
            eprintln!(
                "the async batch API needs the patched liblz4 (default probe paths or \
                 KAE_HW_LZ4_LIB); there is deliberately no software fallback."
            );
            return;
        }
    };

    // A segment need not be a block multiple: the trailing partial block is
    // compressed synchronously and appended, so its lengths vector has one
    // entry per whole block plus one for the tail when present.
    let segment: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let block = 4096;

    // One call: every whole block goes to the KAE queue in parallel; the
    // call returns only after all callbacks completed. `&mut self` because
    // scratch/slot storage is reused across segments — one instance per
    // worker thread, one segment at a time.
    let mut packed = Vec::new();
    let lens = match batch.compress_segment(&segment, block, &mut packed) {
        Ok(lens) => lens,
        Err(e) => {
            eprintln!("compress_segment failed: {e}");
            return;
        }
    };
    println!(
        "compress: {} B -> {} B across {} block entries",
        segment.len(),
        packed.len(),
        lens.len()
    );

    // There is no async decompress path: split `packed` by the returned
    // lengths and decode each entry through the block API.
    let lz4 = KaeLz4::open_sw().expect("system liblz4");
    let mut off = 0;
    let mut restored = Vec::with_capacity(segment.len());
    for l in &lens {
        let src = &packed[off..off + l];
        off += l;
        // Every entry decompresses to at most one block.
        let mut out = vec![0u8; block];
        let n = lz4.decompress(src, &mut out).expect("decompress block");
        restored.extend_from_slice(&out[..n]);
    }
    assert_eq!(restored, segment);
    println!("roundtrip OK: {} bytes restored bit-exact", restored.len());
}
