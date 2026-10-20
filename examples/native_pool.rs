//! The worker-pool scaling shape: one `KaeNative` handle per worker
//! thread, every handle its own engine queue (the queue context is
//! assigned at each device open). A device hosts many queues — 256 on
//! this silicon — and machines carry 2-4 zip devices, so the pool width
//! is the worker count, not the device count; `open_on(t)` wraps the
//! handles round-robin over the devices to spread the load. This is the
//! shape the AgentENV zfile WorkerPool maps onto: open the handles once,
//! move one to each worker thread, stream every segment shard through
//! its own handle.
//!
//! Runs on any machine: without a zip device every handle opens in
//! software mode and the shape is unchanged.
//!
//! ```bash
//! cargo run --example native_pool
//! ```

use kae::{native_available, Backend, KaeNative};

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

fn main() {
    if !native_available() {
        println!("no /dev/hisi_zip-* device: handles open in software mode");
    }
    // the demo keeps a small pool; a real zfile WorkerPool sizes this to
    // its thread count — every extra handle is one more queue
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8);
    println!("worker handles: {workers}");

    let block = 4096;
    let nblocks = 1 << 16;
    let segment = make_mixed(nblocks * block);

    // open_on(t) wraps over the device list: handle t opens its own queue
    // on device t % devices — handles sharing a device never share a queue
    let mut handles: Vec<KaeNative> = (0..workers)
        .map(|t| KaeNative::open_on(t, block).expect("open"))
        .collect();
    for (t, h) in handles.iter().enumerate() {
        println!(
            "handle {t}: {:?} ({}); move it to worker thread {t}",
            h.backend(),
            h.note()
        );
    }

    // contiguous shards; outputs concatenate in shard order — the packed
    // byteshape `AsyncBatch::compress_segment` returns for one segment
    let per = nblocks / workers;
    let packed: Vec<u8> = std::thread::scope(|s| {
        let mut js = Vec::new();
        for (t, h) in handles.iter_mut().enumerate() {
            let segment = &segment;
            js.push(s.spawn(move || {
                let hi = if t + 1 == workers {
                    nblocks * block
                } else {
                    (t + 1) * per * block
                };
                let mut out = Vec::new();
                let lens = h
                    .compress_segment(&segment[t * per * block..hi], block, &mut out)
                    .expect("compress_segment");
                (lens, out)
            }));
        }
        let mut lens_all = Vec::new();
        let mut packed_all = Vec::new();
        for j in js {
            let (lens, mut bytes) = j.join().expect("worker");
            lens_all.extend_from_slice(&lens);
            packed_all.append(&mut bytes);
        }
        println!("lens: {} entries (one per block)", lens_all.len());
        packed_all
    });

    let hw: u64 = handles.iter().map(|h| h.hw_requests()).sum();
    let sw: u64 = handles.iter().map(|h| h.sw_requests()).sum();
    println!(
        "compressed {} MiB -> {} MiB ({:.1}%), blocks hw={} sw={}",
        (nblocks * block) >> 20,
        packed.len() >> 20,
        packed.len() as f64 / (nblocks * block) as f64 * 100.0,
        hw,
        sw,
    );
    assert_eq!(handles.iter().any(|h| h.backend() == Backend::Native), hw > 0);
}
