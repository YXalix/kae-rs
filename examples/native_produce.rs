//! The produce shape: `KaeNative::compress_segment_from` hands the caller
//! the engine's own DMA-visible input buffers, so bytes land where the
//! chip reads them. A producer that fills the handed buffer — a file
//! read, an upstream stage's output — submits with no pump-side copy at
//! all, and the pump thread harvests and rebuilds instead of memcpying.
//! This demo reads a file with `pread` straight into the handed buffers
//! when given a path, and falls back to a memcpy producer over a
//! generated stream otherwise; either way every produced block is decoded
//! back and checked.
//!
//! Runs on any machine: without a zip device the handle opens in software
//! mode and the shape is unchanged (the produce calls then stage whole
//! blocks for lz4rip).
//!
//! ```bash
//! cargo run --example native_produce [path]
//! ```

use std::fs::File;
use std::os::unix::fs::FileExt;

use kae::{native_available, KaeError, KaeNative};

/// The producer contract `compress_segment_from` fills against: whole
/// fills only, an error ends the pass.
type Producer<'a> = Box<dyn FnMut(&mut [u8]) -> Result<(), KaeError> + 'a>;

/// 4K-page interleave: zeros / repeating pattern / pseudo-random, the
/// mixed shape zfile pages present.
fn make_mixed(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    let mut x = 0xDEAD_BEEFu64;
    while v.len() < n {
        let start = v.len();
        v.resize(start + 4096, 0);
        match start / 4096 % 3 {
            0 => {}                      // zeros
            1 => v[start..].fill(0xA5),  // repeating pattern
            _ => {
                for b in &mut v[start..] {
                    x ^= x >> 12;
                    x ^= x << 25;
                    x ^= x >> 27;
                    *b = x as u8;
                }
            }
        }
    }
    v.truncate(n);
    v
}

/// The zero-copy producer: `pread` straight into the handed buffer — the
/// bytes' only journey is kernel page cache → the engine's input slot.
fn pread_producer<'f>(
    file: &'f File,
    size: u64,
) -> impl FnMut(&mut [u8]) -> Result<(), KaeError> + 'f {
    let mut pos = 0u64;
    move |buf: &mut [u8]| {
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = file
                .read_at(&mut buf[filled..], pos + filled as u64)
                .map_err(|e| KaeError::Compress(format!("read_at at {}: {e}", pos + filled as u64)))?;
            if n == 0 {
                return Err(KaeError::Compress(format!(
                    "stream ended at {} of {size}B",
                    pos + filled as u64
                )));
            }
            filled += n;
        }
        pos += buf.len() as u64;
        Ok(())
    }
}

/// The memcpy control: same shape, one extra copy — what a producer whose
/// bytes already sit in memory does when it cannot target the buffer.
fn memcpy_producer<'d>(
    data: &'d [u8],
) -> impl FnMut(&mut [u8]) -> Result<(), KaeError> + 'd {
    let mut pos = 0usize;
    move |buf: &mut [u8]| {
        let end = pos + buf.len();
        data.get(pos..end)
            .ok_or_else(|| KaeError::Compress("producer past the data end".into()))?;
        buf.copy_from_slice(&data[pos..end]);
        pos = end;
        Ok(())
    }
}

fn main() {
    if !native_available() {
        println!("no /dev/hisi_zip-* device: the handle opens in software mode");
    }
    let mut nat = KaeNative::open().expect("open never fails without a device");
    println!("engine: {}", nat.engine_note());

    let block = 4096;
    let nblocks = 1 << 14; // 256 MiB of stream
    let bytes = nblocks * block;

    // the source and its verification image: a file when the demo is
    // given one, a generated stream otherwise
    let (file, source) = match std::env::args().nth(1) {
        Some(path) => {
            let f = File::open(&path).expect("open the given file");
            let len = f.metadata().expect("metadata").len();
            assert!(len >= bytes as u64, "{path} holds {len}B, need {bytes}B");
            let mut image = vec![0u8; bytes];
            f.read_exact_at(&mut image, 0).expect("read the image back");
            (Some(f), image)
        }
        None => (None, make_mixed(bytes)),
    };
    println!(
        "stream: {} blocks x {}B from {}",
        nblocks,
        block,
        if file.is_some() { "file (pread)" } else { "memory (memcpy)" }
    );

    let mut packed: Vec<u8> = Vec::with_capacity(bytes / 2);
    let mut produce: Producer = match file.as_ref() {
        Some(f) => Box::new(pread_producer(f, bytes as u64)),
        None => Box::new(memcpy_producer(&source)),
    };
    let lens = nat
        .compress_segment_from(nblocks, block, &mut packed, produce.as_mut())
        .expect("produce pass");
    assert_eq!(lens.len(), nblocks, "whole blocks only, no tail");

    let packed_bytes: usize = lens.iter().sum();
    println!(
        "packed {}B ({:.1}%), hw={}, sw={}",
        packed_bytes,
        packed_bytes as f64 / bytes as f64 * 100.0,
        nat.hw_requests(),
        nat.sw_requests()
    );

    // every produced block decodes back to its source bytes
    let mut off = 0usize;
    for (i, l) in lens.iter().enumerate() {
        let mut restored = vec![0u8; block];
        let n = nat
            .decompress(&packed[off..off + l], &mut restored)
            .expect("decode");
        assert_eq!(n, block, "block {i}");
        assert_eq!(
            &restored[..n],
            &source[i * block..(i + 1) * block],
            "block {i} bit-exact"
        );
        off += l;
    }
    println!("all {nblocks} blocks decode bit-exact");
}
