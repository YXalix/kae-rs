//! The production per-block engine path: hardware first, pure-Rust software
//! fallback, no C library in either. [`KaeNative::open`] probes for a ZIP
//! engine and degrades to the lz4rip engine ([`crate::soft`]) when none is
//! present or the queue cannot be opened; on a live queue compression goes
//! through the engine as a ring of in-flight requests (the depth that hides
//! the chip's per-request latency — the sweep measured 2.9 GB/s at one
//! request in flight vs 6.4 from depth 4), and any request that stalls past
//! the timeout or completes with a hardware error fails its whole block
//! over to software — a finished LZ4 block always comes from one engine,
//! never a splice. A wedged queue (timeout, stuck ring) marks the handle
//! [`KaeNative::stuck`]: every further block goes to software until the
//! handle is reopened. Decompression is software in every mode (the engine
//! has no LZ4 decode — the driver rejects the decompress direction for LZ4
//! outright), so the read path runs lz4rip regardless of the backend.

use std::time::Duration;

use crate::lz4dec;
use crate::rebuild;
use crate::soft;
use crate::uacce::{self, NativeAlg, NativePump};
use crate::{Backend, KaeError};

/// Below this size the engine request makes no sense (the lz77 parse
/// programs `in_len - 12`, so a request must leave room for it) and the
/// block goes to software whole.
const MIN_HW_INPUT: usize = 16;

/// The software whole-block fallback: the lz4rip engine's own result,
/// with a hardware fallback cause prefixed when there was one.
fn soft_block(src: &[u8], dst: &mut [u8], hw_cause: Option<String>) -> Result<usize, KaeError> {
    soft::compress(src, dst).map_err(|e| match hw_cause {
        Some(hw) => KaeError::Compress(format!("{e} (hw fallback cause: {hw})")),
        None => KaeError::Compress(e),
    })
}

/// Timeout and queue-busy failures mean the engine (or its queue) is not
/// answering: they stick the handle in software mode, unlike a per-request
/// hardware status error, which only fails its own block.
fn is_wedge(err: &str) -> bool {
    err.contains("queue_busy") || err.contains("in_time")
}

/// Whether the ring-debug knob is on: per-wave completion logging plus a
/// decode-verify of every hardware block, so one run localizes a misbuild
/// to its wave and chunk.
fn debug_ring() -> bool {
    std::env::var("KAE_NATIVE_DEBUG_RING").as_deref() == Ok("1")
}

/// Decode-verify one rebuilt block against its input; the report names the
/// chunk holding the first divergence, never the bytes themselves. Three
/// decoders judge (lz4dec, lz4rip, and — via the walk audit — the triple
/// rules themselves), because a block that decodes under one and fails
/// another names the violated rule exactly.
fn verify_debug(
    input: &[u8],
    block: &[u8],
    samples: &[Result<uacce::Lz77Sample, String>],
    b: usize,
    req_size: usize,
) {
    let mut plain = vec![0u8; input.len()];
    match lz4dec::decode_block(block, &mut plain) {
        Err(e) => eprintln!("native: block {b} ({}B) lz4dec: {e}", block.len()),
        Ok(m) if &plain[..m] != input => {
            let d = plain[..m].iter().zip(input).position(|(a, b)| a != b);
            eprintln!(
                "native: block {b} ({}B) lz4dec: decodes {m}B, first diff at {d:?} (chunk {})",
                block.len(),
                d.map(|d| d / req_size).unwrap_or(usize::MAX),
            );
        }
        Ok(_) => {}
    }
    match soft::decompress(block, &mut plain) {
        Err(e) => eprintln!("native: block {b} lz4rip: {e}"),
        Ok(m) if &plain[..m] != input => {
            let d = plain[..m].iter().zip(input).position(|(a, b)| a != b);
            eprintln!(
                "native: block {b} lz4rip: decodes {m}B, first diff at {d:?} (chunk {})",
                d.map(|d| d / req_size).unwrap_or(usize::MAX),
            );
        }
        Ok(_) => {}
    }
    for (r, s) in samples.iter().enumerate() {
        let s = match s {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Err(a) = rebuild::audit_triples(
            &input[r * req_size..(r + 1) * req_size],
            &s.seqs,
            Some(&s.lits),
        ) {
            eprintln!("native: block {b} chunk {r}: {a}");
        }
    }
}

/// A hardware-first LZ4 block compressor with automatic software fallback.
/// One queue per handle; like [`crate::AsyncBatch`], it is `Send` (move it
/// to the worker thread that owns it) but not `Sync`, and compression takes
/// `&mut self` — scale by handing each worker thread its own handle.
pub struct KaeNative {
    backend: Backend,
    /// The hardware request size (the opened queue's slot capacity); blocks
    /// chain as many requests as their length needs.
    req_size: usize,
    note: String,
    timeout: Duration,
    pump: Option<NativePump>,
    /// Rebuild staging, reused across calls.
    scratch: Vec<u8>,
    /// Software-engine output staging, reused across calls.
    soft_dst: Vec<u8>,
    hw_requests: u64,
    sw_requests: u64,
    /// The hardware failure that caused the most recent software fallback.
    last_hw_error: Option<String>,
    /// The queue stopped answering (a request timed out or the ring stuck
    /// busy): every further block goes to software until the handle is
    /// reopened.
    stuck: bool,
}

impl std::fmt::Debug for KaeNative {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KaeNative")
            .field("backend", &self.backend)
            .field("req_size", &self.req_size)
            .field("note", &self.note)
            .field("hw_requests", &self.hw_requests)
            .field("sw_requests", &self.sw_requests)
            .field("last_hw_error", &self.last_hw_error)
            .field("stuck", &self.stuck)
            .finish_non_exhaustive()
    }
}

impl KaeNative {
    /// Open with the zfile block granularity (4096) as the request size.
    pub fn open() -> Result<KaeNative, KaeError> {
        Self::open_with(4096)
    }

    /// Open with an explicit hardware request size (bench blocks cap at the
    /// engine's 128K lz77 flat limit; larger blocks chain requests). The
    /// in-flight depth comes from `KAE_NATIVE_DEPTH` (default 8, capped at
    /// the engine's queue depth). Without a `/dev/hisi_zip-*` device — or
    /// when every queue fails to open — this returns a software-mode handle
    /// rather than an error; [`KaeNative::backend`] tells the two apart.
    pub fn open_with(req_size: usize) -> Result<KaeNative, KaeError> {
        if req_size == 0 {
            return Err(KaeError::Compress("request size 0".into()));
        }
        let timeout = Duration::from_millis(
            std::env::var("KAE_NATIVE_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|v| *v > 0)
                .unwrap_or(500),
        );
        let depth = std::env::var("KAE_NATIVE_DEPTH")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(8)
            .min(uacce::QM_Q_DEPTH);
        let req_size = req_size.min(uacce::MAX_LZ77_REQ);
        let devices = uacce::zip_devices();
        if devices.is_empty() {
            return Ok(KaeNative {
                backend: Backend::Soft,
                req_size,
                note: "mode=soft,no_hisi_zip_uacce_node".into(),
                timeout,
                pump: None,
                scratch: Vec::new(),
                soft_dst: vec![0u8; soft::bound(req_size)],
                hw_requests: 0,
                sw_requests: 0,
                last_hw_error: None,
                stuck: false,
            });
        }
        // one queue, `depth` requests in flight, poll waiting — the shape
        // the sweep measured at 6.4 GB/s per queue from depth 4
        match NativePump::open(&devices, 1, req_size, depth, 1, true, NativeAlg::Lz77) {
            Ok(mut pump) => {
                pump.run(0.05); // settle the queue the way the bench does
                Ok(KaeNative {
                    backend: Backend::Native,
                    req_size,
                    note: format!(
                        "mode=native,req_size={req_size},queues=1,depth={depth},wait=poll,timeout_ms={}",
                        timeout.as_millis()
                    ),
                    timeout,
                    pump: Some(pump),
                    scratch: Vec::new(),
                    soft_dst: vec![0u8; soft::bound(req_size)],
                    hw_requests: 0,
                    sw_requests: 0,
                    last_hw_error: None,
                    stuck: false,
                })
            }
            Err(e) => Ok(KaeNative {
                backend: Backend::Soft,
                req_size,
                note: format!("mode=soft,queue_open_failed:{e}"),
                timeout,
                pump: None,
                scratch: Vec::new(),
                soft_dst: vec![0u8; soft::bound(req_size)],
                hw_requests: 0,
                sw_requests: 0,
                last_hw_error: None,
                stuck: false,
            }),
        }
    }

    /// The backend actually in effect: [`Backend::Native`] compresses on the
    /// engine (software per-block on fallback), [`Backend::Soft`] is
    /// lz4rip throughout.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Open diagnostics: why the handle is in the mode it is (device
    /// absence, queue-open failure) plus the knob values.
    pub fn note(&self) -> &str {
        &self.note
    }

    /// Blocks the engine completed (one per block, however many requests it
    /// chained).
    pub fn hw_requests(&self) -> u64 {
        self.hw_requests
    }

    /// Blocks that went to software: no device, too-small input, a hardware
    /// stall/error, or the segment tail.
    pub fn sw_requests(&self) -> u64 {
        self.sw_requests
    }

    /// Why the most recent block fell back to software, if one did.
    pub fn last_hw_error(&self) -> Option<&str> {
        self.last_hw_error.as_deref()
    }

    /// The queue stopped answering (a request timed out, the ring stuck
    /// busy): every further block goes to software until the handle is
    /// reopened.
    pub fn stuck(&self) -> bool {
        self.stuck
    }

    /// Worst-case compressed size of `n` input bytes.
    pub fn bound(&self, n: usize) -> usize {
        soft::bound(n)
    }

    /// Compress one standard LZ4 block. Hardware mode runs the block's
    /// requests through the engine synchronously; a stalled or erroring
    /// request fails the whole block over to the software engine (never a
    /// mixed block), counted in [`KaeNative::sw_requests`] with the cause
    /// in [`KaeNative::last_hw_error`]. A wedged queue sticks the handle
    /// in software mode ([`KaeNative::stuck`]).
    pub fn compress(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize, KaeError> {
        if !self.hw_eligible(src) {
            self.sw_requests += 1;
            return soft_block(src, dst, self.stuck.then(|| "engine_unresponsive".into()));
        }
        match self.compress_hw(src, dst) {
            Ok(n) => {
                self.hw_requests += 1;
                Ok(n)
            }
            Err(e) => {
                self.sw_requests += 1;
                self.stuck |= is_wedge(&e);
                self.last_hw_error = Some(e.clone());
                soft_block(src, dst, Some(e))
            }
        }
    }

    /// Compress a whole segment in ONE ring pass: every hardware-eligible
    /// block's requests enter the in-flight ring together (waved at the
    /// queue depth inside `request_batch` — the depth, not the call
    /// granularity, is what hides the chip's per-request latency), then
    /// each block rebuilds from its samples in one walk. The trailing
    /// partial block, ineligible blocks, and any block whose requests fail
    /// or misbuild go through the software engine whole — a finished LZ4
    /// block always comes from one engine, never a splice. `packed`
    /// (cleared first) receives the blocks' compressed bytes concatenated
    /// in block order followed by the tail, and the returned lengths match
    /// one-to-one (one entry per whole block, the tail's length last) —
    /// the `AsyncBatch::compress_segment` shape.
    pub fn compress_segment(
        &mut self,
        segment: &[u8],
        block: usize,
        packed: &mut Vec<u8>,
    ) -> Result<Vec<usize>, KaeError> {
        if block == 0 {
            return Err(KaeError::Compress("block size 0".into()));
        }
        if segment.is_empty() {
            return Err(KaeError::Compress("empty segment".into()));
        }
        let nblocks = segment.len() / block;
        packed.clear();
        let mut lens = Vec::with_capacity(nblocks + 1);
        let debug = debug_ring();

        // hardware eligibility per block, and the flat chunk list of every
        // eligible block — ONE ring pass for the whole segment, waved at
        // the queue depth inside request_batch (per-block batching would
        // leave a req-size block one request deep, latency-bound again)
        let req_size = self.req_size;
        let hw: Vec<bool> = (0..nblocks)
            .map(|b| self.hw_eligible(&segment[b * block..(b + 1) * block]))
            .collect();
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut ranges: Vec<(usize, usize)> = vec![(0, 0); nblocks];
        for b in 0..nblocks {
            if !hw[b] {
                continue;
            }
            let blk = &segment[b * block..(b + 1) * block];
            let start = chunks.len();
            let mut off = 0usize;
            while off < blk.len() {
                let len = (blk.len() - off).min(req_size);
                chunks.push(&blk[off..off + len]);
                off += len;
            }
            ranges[b] = (start, chunks.len() - start);
        }
        let results = match chunks.is_empty() {
            true => Vec::new(),
            false => {
                let pump = self.pump.as_mut().expect("eligible blocks need a pump");
                pump.request_batch(&chunks, self.timeout.as_secs_f64())
            }
        };
        // a wedge-class failure sticks the handle: the queue stopped
        // answering, so later segments should not retry it either
        for r in &results {
            if let Err(e) = r {
                self.stuck |= is_wedge(e);
                self.last_hw_error = Some(e.clone());
            }
        }

        // rebuild every hw block from its samples in one walk; anything
        // else (ineligible, failed requests, failed rebuild) goes to
        // software whole — a block never mixes engines
        for b in 0..nblocks {
            let blk = &segment[b * block..(b + 1) * block];
            let n = if hw[b] {
                let (start, count) = ranges[b];
                let mine = &results[start..start + count];
                if mine.iter().all(|r| r.is_ok()) {
                    let requests: Vec<(&[u8], usize)> = mine
                        .iter()
                        .zip(blk.chunks(req_size))
                        .map(|(r, c)| (r.as_ref().unwrap().seqs.as_slice(), c.len()))
                        .collect();
                    match rebuild::lz77_block_from_requests(blk, &requests, &mut self.scratch) {
                        Ok(n) => {
                            if debug {
                                verify_debug(blk, &self.scratch[..n], mine, b, req_size);
                            }
                            packed.extend_from_slice(&self.scratch[..n]);
                            self.hw_requests += 1;
                            lens.push(n);
                            continue;
                        }
                        Err(e) => Some(e),
                    }
                } else {
                    mine.iter()
                        .find_map(|r| r.as_ref().err().cloned())
                }
            } else {
                None
            };
            let n = self.soft_append(blk, packed, n)?;
            lens.push(n);
        }
        let tail = &segment[nblocks * block..];
        if !tail.is_empty() {
            let n = self.soft_append(tail, packed, None)?;
            lens.push(n);
        }
        Ok(lens)
    }

    /// Compress into a fresh vector, sized by [`KaeNative::bound`].
    pub fn compress_vec(&mut self, src: &[u8]) -> Result<Vec<u8>, KaeError> {
        let mut dst = vec![0u8; self.bound(src.len())];
        let n = self.compress(src, &mut dst)?;
        dst.truncate(n);
        Ok(dst)
    }

    /// Decompress one standard LZ4 block: software in every mode (the
    /// engine has no LZ4 decode direction).
    pub fn decompress(&self, src: &[u8], dst: &mut [u8]) -> Result<usize, KaeError> {
        soft::decompress(src, dst).map_err(KaeError::Decompress)
    }

    /// Whether a block can go through the engine: a live, unstuck queue,
    /// an input the parse can program, and no trailing remainder that would
    /// underflow it.
    fn hw_eligible(&self, src: &[u8]) -> bool {
        if self.pump.is_none() || self.stuck || src.len() < MIN_HW_INPUT {
            return false;
        }
        let remainder = src.len() % self.req_size;
        remainder == 0 || remainder >= MIN_HW_INPUT
    }

    /// Software-compress `src` and append it to `packed`; `hw_cause`
    /// prefixes the fallback reason when the block was diverted from the
    /// engine. The staging buffer grows to the input's bound on first use.
    fn soft_append(
        &mut self,
        src: &[u8],
        packed: &mut Vec<u8>,
        hw_cause: Option<String>,
    ) -> Result<usize, KaeError> {
        let need = soft::bound(src.len());
        if self.soft_dst.len() < need {
            self.soft_dst.resize(need, 0);
        }
        let n = soft::compress(src, &mut self.soft_dst)
            .map_err(|e| match hw_cause {
                Some(hw) => KaeError::Compress(format!("{e} (hw fallback cause: {hw})")),
                None => KaeError::Compress(e),
            })?;
        packed.extend_from_slice(&self.soft_dst[..n]);
        self.sw_requests += 1;
        Ok(n)
    }

    /// The synchronous engine path: one request per `req_size` slice, the
    /// triples of the whole block rebuilt in one walk (a finished LZ4 block
    /// is not composable, so per-request streams are never spliced — the
    /// same walk the bench's sample path uses).
    fn compress_hw(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
        let pump = self.pump.as_mut().expect("native mode has a pump");
        let req_size = self.req_size;
        let mut chunks: Vec<&[u8]> = Vec::with_capacity(src.len() / req_size + 1);
        let mut off = 0usize;
        while off < src.len() {
            let len = (src.len() - off).min(req_size);
            chunks.push(&src[off..off + len]);
            off += len;
        }
        let results = pump.request_batch(&chunks, self.timeout.as_secs_f64());
        // every request of the block must succeed for the block to rebuild;
        // otherwise the whole block falls to software
        let mut samples = Vec::with_capacity(chunks.len());
        for r in &results {
            match r {
                Ok(s) => samples.push(s),
                Err(e) => return Err(e.clone()),
            }
        }
        let requests: Vec<(&[u8], usize)> = samples
            .iter()
            .zip(chunks.iter())
            .map(|(s, c)| (s.seqs.as_slice(), c.len()))
            .collect();
        let n = rebuild::lz77_block_from_requests(src, &requests, &mut self.scratch)?;
        if n > dst.len() {
            return Err(format!("rebuilt block is {n}B, dst holds {}", dst.len()));
        }
        dst[..n].copy_from_slice(&self.scratch[..n]);
        Ok(n)
    }
}
