//! The production per-block engine path: hardware first, pure-Rust software
//! fallback, no C library in either. [`KaeNative::open`] probes for a ZIP
//! engine and degrades to the lz4rip engine ([`crate::soft`]) when none is
//! present or the queue cannot be opened; on a live queue compression goes
//! through the engine as a streaming ring of in-flight requests (refilling
//! each buffer the moment its completion is harvested, so the depth hides
//! the chip's per-request latency across the whole segment — the sweep
//! measured 2.9 GB/s at one request in flight vs 6.4 from depth 4), and any
//! request that stalls past the timeout or completes with a hardware error
//! fails its whole block over to software — a finished LZ4 block always
//! comes from one engine, never a splice. A wedged queue (timeout, stuck
//! ring) marks the handle [`KaeNative::stuck`]: every further block goes to
//! software until the handle is reopened. Decompression is software in
//! every mode (the engine has no LZ4 decode — the driver rejects the
//! decompress direction for LZ4 outright), so the read path runs lz4rip
//! regardless of the backend.

use std::time::{Duration, Instant};

use crate::lz4dec;
use crate::rebuild;
use crate::soft;
use crate::uacce::{self, NativeAlg, NativePump};
use crate::{Backend, KaeError};

/// Below this size the engine request makes no sense (the lz77 parse
/// programs `in_len - 12`, so a request must leave room for it) and the
/// block goes to software whole.
const MIN_HW_INPUT: usize = 16;

/// Where a streaming pass's blocks come from: a segment the caller owns
/// (chunks are its slices, copied into the engine's buffers on submit;
/// literals rebuild from the segment), or a producer callback that fills
/// the engine's own claimed input buffers (chunks are produced in stream
/// order; the literals rebuild from the held slots, which keep the only
/// copy of the bytes until their block resolves).
enum PassSource<'a, 'p> {
    Slices(&'a [u8]),
    Produce(&'p mut dyn FnMut(&mut [u8]) -> Result<(), KaeError>),
}

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

/// Exposed so bench RESULT notes can record whether the ring-debug knob
/// was on — a debug pass copies full samples and decodes every block, and
/// a throughput number taken under it is not a throughput number.
pub(crate) fn ring_debug_on() -> bool {
    debug_ring()
}

/// Decode-verify one rebuilt block against its input; the report names the
/// chunk holding the first divergence, never the bytes themselves. Three
/// decoders judge (lz4dec, lz4rip, and — via the walk audit — the triple
/// rules themselves), because a block that decodes under one and fails
/// another names the violated rule exactly. Any content fault dumps the
/// block's full replay kit (per-chunk triples + literal areas + the block
/// bytes) under /tmp — the input regenerates from the deterministic test
/// generator, so the triples are the only missing piece offline.
fn verify_debug(
    input: &[u8],
    block: &[u8],
    samples: &[Result<uacce::Lz77Sample, String>],
    b: usize,
    req_size: usize,
) {
    let mut plain = vec![0u8; input.len()];
    let mut fault: Option<String> = None;
    match lz4dec::decode_block(block, &mut plain) {
        Err(e) => {
            eprintln!("native: block {b} ({}B) lz4dec: {e}", block.len());
            fault = Some(format!("lz4dec: {e}"));
        }
        Ok(m) if &plain[..m] != input => {
            let d = plain[..m].iter().zip(input).position(|(a, b)| a != b);
            eprintln!(
                "native: block {b} ({}B) lz4dec: decodes {m}B, first diff at {d:?} (chunk {})",
                block.len(),
                d.map(|d| d / req_size).unwrap_or(usize::MAX),
            );
            fault = Some("lz4dec content diff".into());
        }
        Ok(_) => {}
    }
    match soft::decompress(block, &mut plain) {
        Err(e) => {
            eprintln!("native: block {b} lz4rip: {e}");
            fault.get_or_insert_with(|| format!("lz4rip: {e}"));
        }
        Ok(m) if &plain[..m] != input => {
            let d = plain[..m].iter().zip(input).position(|(a, b)| a != b);
            eprintln!(
                "native: block {b} lz4rip: decodes {m}B, first diff at {d:?} (chunk {})",
                d.map(|d| d / req_size).unwrap_or(usize::MAX),
            );
            fault.get_or_insert("lz4rip content diff".into());
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
    if let Some(v) = rebuild::stock_rule_violation(input.len(), block) {
        eprintln!("native: block {b} stock-rule walk: {v}");
        fault.get_or_insert(format!("stock rule: {v}"));
    }
    let Some(v) = fault else { return };
    eprintln!("native: block {b}: fault {v:?}; dumping replay kit");
    let dir = format!("/tmp/kae_fail_block{b}");
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("native: block {b}: cannot create {dir}");
        return;
    }
    let _ = std::fs::write(format!("{dir}/block.hex"), hex(block));
    for (r, s) in samples.iter().enumerate() {
        let Ok(s) = s else { continue };
        let len = req_size.min(input.len() - r * req_size);
        let (walked, folds) = rebuild::walk_stats(&s.seqs);
        eprintln!(
            "native: block {b} chunk {r}: walk end {walked}/{len} (tail {}B), folds {folds}, {} triples",
            len.saturating_sub(walked),
            s.seqs.len() / 8,
        );
        let _ = std::fs::write(
            format!("{dir}/chunk{r}.txt"),
            format!(
                "input {len}B\nseqs {} triples: {}\nlits {}B: {}\n",
                s.seqs.len() / 8,
                hex(&s.seqs),
                s.lits.len(),
                hex(&s.lits),
            ),
        );
    }
    eprintln!(
        "native: block {b}: kit in {dir} (block {}B, req_size {req_size}; input is the test's make_mixed segment)",
        block.len()
    );
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
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
    /// Per-input-buffer triples staging for the segment pass, reused
    /// across calls (sized to the pump's depth on first use).
    stream_seqs: Vec<Vec<u8>>,
    /// Streaming-submit doorbell batch (`KAE_NATIVE_DB_BATCH`, default the
    /// in-flight depth).
    db_batch: usize,
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
    /// the engine's queue depth), the streaming doorbell batch from
    /// `KAE_NATIVE_DB_BATCH` (default the depth), and the wait policy from
    /// `KAE_NATIVE_WAIT` (`spin`, else poll — the sweep measured poll at a
    /// quarter of spin's CPU for equal throughput). Without a
    /// `/dev/hisi_zip-*` device — or when every queue fails to open — this
    /// returns a software-mode handle rather than an error;
    /// [`KaeNative::backend`] tells the two apart.
    pub fn open_with(req_size: usize) -> Result<KaeNative, KaeError> {
        Self::open_in(&uacce::zip_devices(), req_size)
    }

    /// Open on one specific engine device (a `/dev/hisi_zip-*` index,
    /// wrapped modulo [`crate::native_device_count`]). The worker-pool
    /// scaling unit: each handle's queue context is assigned at its own
    /// device open, so every handle is one queue, and handles opened on
    /// the same device never share one — a device hosts many queues.
    /// Opening handle `t` on device `t % devices` therefore scales the
    /// pool to the thread count (queues pile up per device as
    /// `t` grows) while spreading the load over the machine's devices.
    pub fn open_on(device: usize, req_size: usize) -> Result<KaeNative, KaeError> {
        let devices = uacce::zip_devices();
        if devices.is_empty() {
            return Self::open_in(&devices, req_size);
        }
        Self::open_in(&devices[device % devices.len()..device % devices.len() + 1], req_size)
    }

    /// The shared open path: probe `devices` (in order) for one live queue.
    fn open_in(devices: &[String], req_size: usize) -> Result<KaeNative, KaeError> {
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
        let db_batch = std::env::var("KAE_NATIVE_DB_BATCH")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(depth);
        let wait_poll = std::env::var("KAE_NATIVE_WAIT").map(|v| v != "spin").unwrap_or(true);
        let req_size = req_size.min(uacce::MAX_LZ77_REQ);
        if devices.is_empty() {
            return Ok(KaeNative {
                backend: Backend::Soft,
                req_size,
                note: "mode=soft,no_hisi_zip_uacce_node".into(),
                timeout,
                pump: None,
                scratch: Vec::new(),
                stream_seqs: Vec::new(),
                db_batch,
                soft_dst: vec![0u8; soft::bound(req_size)],
                hw_requests: 0,
                sw_requests: 0,
                last_hw_error: None,
                stuck: false,
            });
        }
        // one queue, `depth` requests in flight, streaming refills — the
        // shape the sweep measured at 6.4 GB/s per queue from depth 4
        match NativePump::open(devices, 1, req_size, depth, db_batch, wait_poll, NativeAlg::Lz77) {
            Ok(mut pump) => {
                pump.run(0.05); // settle the queue the way the bench does
                Ok(KaeNative {
                    backend: Backend::Native,
                    req_size,
                    note: format!(
                        "mode=native,req_size={req_size},queues=1,depth={depth},db_batch={db_batch},wait={},timeout_ms={}",
                        if wait_poll { "poll" } else { "spin" },
                        timeout.as_millis()
                    ),
                    timeout,
                    pump: Some(pump),
                    scratch: Vec::new(),
                    stream_seqs: Vec::new(),
                    db_batch,
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
                stream_seqs: Vec::new(),
                db_batch,
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

    /// The open-time engine note: mode, request size, queue depth,
    /// doorbell batch, wait policy, timeout — the fields a RESULT line
    /// should carry so a run's shape is readable from its own output.
    pub fn engine_note(&self) -> &str {
        &self.note
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

    /// Compress a whole segment as one streaming pass over the engine:
    /// every hardware-eligible block's requests enter the in-flight ring
    /// continuously — a completion frees its buffer and the next chunk
    /// refills it, so the depth hides the chip's request latency across
    /// the whole segment. Completions pair by the CQE's `sq_head`, never
    /// by arrival order; a block rebuilds from its triples straight into
    /// `packed` as soon as its last request has landed and every earlier
    /// block is packed (blocks emit in block order whatever the arrival
    /// pattern). The trailing partial block, ineligible blocks, and any
    /// block whose requests fail or misbuild go through the software
    /// engine whole — a finished LZ4 block always comes from one engine,
    /// never a splice. `packed` (cleared first) receives the blocks'
    /// compressed bytes concatenated in block order followed by the tail,
    /// and the returned lengths match one-to-one (one entry per whole
    /// block, the tail's length last) — the `AsyncBatch::compress_segment`
    /// shape.
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
        self.compress_pass(PassSource::Slices(segment), nblocks, block, packed)
    }

    /// The produce form of [`KaeNative::compress_segment`]: the producer
    /// callback fills each claimed input buffer directly — the engine's
    /// own DMA-visible slots — so a caller that produces into the handed
    /// buffer (a file read, an upstream stage's output) submits with zero
    /// payload copies, and the pump thread spends its time harvesting and
    /// rebuilding instead of memcpying. `nblocks` blocks of `block` bytes
    /// are consumed through `produce` in stream order, one call per chunk
    /// (`req_size` except a block's tail request), whole fills only — an
    /// error ends the pass with that error, the producer's stream
    /// position being undefined past a failed fill. The trailing partial
    /// block of [`KaeNative::compress_segment`] has no counterpart here:
    /// size `nblocks` to the full blocks the stream holds, and the
    /// returned lengths carry exactly `nblocks` entries.
    ///
    /// Completion, pairing, rebuild and fallback semantics are the
    /// segment pass's, with one structural difference: a block's input
    /// buffers stay claimed until the block resolves, because until then
    /// they hold the only copy of its bytes — the rebuild reads the
    /// literals back out of the slots they were parsed from. A block
    /// that fails over stages itself first (unsubmitted chunks
    /// re-produced, submitted ones gathered from their slots), then
    /// compresses through the software engine — still one engine per
    /// finished block, never a splice.
    pub fn compress_segment_from(
        &mut self,
        nblocks: usize,
        block: usize,
        packed: &mut Vec<u8>,
        produce: &mut dyn FnMut(&mut [u8]) -> Result<(), KaeError>,
    ) -> Result<Vec<usize>, KaeError> {
        if block == 0 {
            return Err(KaeError::Compress("block size 0".into()));
        }
        if nblocks == 0 {
            return Err(KaeError::Compress("no blocks".into()));
        }
        self.compress_pass(PassSource::Produce(produce), nblocks, block, packed)
    }

    /// The streaming pass behind both sources. Every hardware-eligible
    /// block's requests enter the in-flight ring continuously — a
    /// completion frees its buffer and the next chunk refills it, so the
    /// depth hides the chip's request latency across the whole pass.
    /// Completions pair by the CQE's `sq_head`, never by arrival order; a
    /// block rebuilds from its triples straight into `packed` as soon as
    /// its last request has landed and every earlier block is packed
    /// (blocks emit in block order whatever the arrival pattern). Any
    /// block whose requests fail or misbuild goes through the software
    /// engine whole — a finished LZ4 block always comes from one engine,
    /// never a splice.
    fn compress_pass(
        &mut self,
        mut source: PassSource<'_, '_>,
        nblocks: usize,
        block: usize,
        packed: &mut Vec<u8>,
    ) -> Result<Vec<usize>, KaeError> {
        packed.clear();
        let mut lens = Vec::with_capacity(nblocks + 1);
        let debug = debug_ring();

        // hardware eligibility per block, and the flat chunk ladder every
        // eligible block contributes: the slices pass chunks its segment;
        // the produce pass derives the identical ladder from the block
        // size (every block is `block` bytes) — chunks are lengths only,
        // their bytes live in the segment or the claimed input slots
        let req_size = self.req_size;
        let hw: Vec<bool> = match &source {
            PassSource::Slices(segment) => (0..nblocks)
                .map(|b| self.hw_eligible(&segment[b * block..(b + 1) * block]))
                .collect(),
            PassSource::Produce(_) => vec![self.hw_eligible_len(block); nblocks],
        };
        let mut chunk_block: Vec<usize> = Vec::new();
        let mut chunk_lens: Vec<usize> = Vec::new();
        let mut ranges: Vec<(usize, usize)> = vec![(0, 0); nblocks];
        for b in 0..nblocks {
            if !hw[b] {
                continue;
            }
            let start = chunk_lens.len();
            let mut off = 0usize;
            while off < block {
                let len = (block - off).min(req_size);
                chunk_lens.push(len);
                chunk_block.push(b);
                off += len;
            }
            ranges[b] = (start, chunk_lens.len() - start);
        }

        // streaming state: per-buffer triples staging (sized once), the
        // ring position of each chunk and its reverse, per-block
        // outstanding requests and first failure, and — under
        // KAE_NATIVE_DEBUG_RING — the full sample of every chunk (literal
        // area included) for the verifier
        let depth = self.pump.as_ref().map(|p| p.depth()).unwrap_or(0);
        if self.stream_seqs.len() != depth {
            self.stream_seqs = (0..depth).map(|_| Vec::new()).collect();
        }
        let mut chunk_ring = vec![usize::MAX; chunk_lens.len()];
        let mut ring_chunk: Vec<Option<usize>> = vec![None; depth];
        let mut pending: Vec<usize> = (0..nblocks).map(|b| ranges[b].1).collect();
        let mut block_err: Vec<Option<String>> = vec![None; nblocks];
        let mut samples: Option<Vec<Result<uacce::Lz77Sample, String>>> = debug
            .then(|| (0..chunk_lens.len()).map(|_| Err("not_completed".into())).collect());

        // a queue still holding requests from an abandoned pass takes no
        // new work: every hardware block fails over and the wedge sticks
        // (a soft-mode handle has no pump and simply never enters a pass)
        let pump_busy = self.pump.as_ref().is_some_and(|p| p.busy());
        let hw_pass = !chunk_lens.is_empty() && !pump_busy;
        if pump_busy {
            for b in 0..nblocks {
                if hw[b] {
                    block_err[b] = Some("queue_busy".into());
                }
            }
            self.last_hw_error = Some("queue_busy".into());
            self.stuck = true;
        }
        let db_batch = self.db_batch;
        let mut next_chunk = 0usize;
        let mut next_emit = 0usize;
        let mut stalled = Instant::now();
        let mut timed_out = false;
        let mut rings: Vec<usize> = Vec::new();
        while next_emit < nblocks {
            if hw_pass && !timed_out {
                // refill: claim and submit until every buffer is in flight
                let pump = self.pump.as_mut().expect("hw pass holds a pump");
                while next_chunk < chunk_lens.len() {
                    let claimed = match &mut source {
                        PassSource::Slices(segment) => {
                            let b = chunk_block[next_chunk];
                            let (start, _) = ranges[b];
                            let off = (next_chunk - start) * req_size;
                            let len = chunk_lens[next_chunk];
                            let chunk = &segment[b * block + off..b * block + off + len];
                            pump.try_submit(db_batch, chunk)
                        }
                        PassSource::Produce(produce) => match pump.claim() {
                            None => None,
                            Some((ring, buf)) => {
                                let len = chunk_lens[next_chunk];
                                match produce(&mut buf[..len]) {
                                    Ok(()) => {
                                        pump.submit_claimed(ring, len, db_batch);
                                        Some(ring)
                                    }
                                    Err(e) => {
                                        // the producer's stream position is
                                        // undefined past a failed fill: hand
                                        // the claim back and end the pass
                                        pump.release(ring);
                                        return Err(e);
                                    }
                                }
                            }
                        },
                    };
                    let Some(ring) = claimed else {
                        break;
                    };
                    ring_chunk[ring % depth] = Some(next_chunk);
                    chunk_ring[next_chunk] = ring;
                    next_chunk += 1;
                    stalled = Instant::now();
                }
                pump.flush_doorbell();
                // harvest: pair each completion by sq_head, copy its
                // triples (never the literal area), and drop its block's
                // outstanding count — the block resolves at zero
                rings.clear();
                if pump.harvest_completions(&mut rings) > 0 {
                    stalled = Instant::now();
                }
                for ring in rings.drain(..) {
                    let slot = ring % depth;
                    let c = match ring_chunk[slot].take() {
                        Some(c) => c,
                        None => continue,
                    };
                    let b = chunk_block[c];
                    if block_err[b].is_some() {
                        // slices mode: the segment still owns the bytes —
                        // the position refills at once. produce mode: the
                        // slot holds the only copy of a failing block's
                        // bytes — it hands back once the block's fallback
                        // has staged them
                        if matches!(source, PassSource::Slices(_)) {
                            pump.release(ring);
                        }
                        continue;
                    }
                    let in_len = chunk_lens[c];
                    let outcome = if let Some(v) = samples.as_mut() {
                        match pump.read_sample(ring, in_len) {
                            Ok(s) => {
                                v[c] = Ok(s);
                                Ok(())
                            }
                            Err(e) => {
                                v[c] = Err(e.clone());
                                Err(e)
                            }
                        }
                    } else {
                        pump.read_seqs(ring, in_len, &mut self.stream_seqs[slot])
                    };
                    if matches!(source, PassSource::Slices(_)) {
                        pump.release(ring);
                    }
                    match outcome {
                        Ok(()) => pending[b] -= 1,
                        Err(e) => {
                            self.last_hw_error = Some(e.clone());
                            block_err[b] = Some(e);
                        }
                    }
                }
            }
            // emit: blocks pack in block order; a block rebuilds the
            // moment its turn comes and all of its requests are done
            while next_emit < nblocks {
                let b = next_emit;
                if hw[b] && block_err[b].is_none() && pending[b] > 0 {
                    break; // this block's requests are still landing
                }
                if hw[b] && block_err[b].is_none() {
                    let (start, count) = ranges[b];
                    let base = packed.len();
                    let requests: Vec<(&[u8], usize)> = (start..start + count)
                        .map(|c| {
                            let seqs: &[u8] = match &samples {
                                Some(v) => {
                                    v[c].as_ref().map(|s| s.seqs.as_slice()).unwrap_or(&[])
                                }
                                None => &self.stream_seqs[chunk_ring[c] % depth],
                            };
                            (seqs, chunk_lens[c])
                        })
                        .collect();
                    let rebuilt = match &source {
                        PassSource::Slices(segment) => rebuild::lz77_block_from_requests(
                            &segment[b * block..(b + 1) * block],
                            &requests,
                            packed,
                        ),
                        PassSource::Produce(_) => {
                            // the literals come back out of the held input
                            // slots: until the block resolves, they hold
                            // the only copy of its bytes
                            let pump = self.pump.as_ref().expect("hw pass holds a pump");
                            let parts: Vec<&[u8]> = (start..start + count)
                                .map(|c| pump.slot_input(chunk_ring[c], chunk_lens[c]))
                                .collect();
                            rebuild::lz77_block_from_views(
                                &rebuild::ScatteredBytes { chunks: &parts },
                                &requests,
                                packed,
                            )
                        }
                    };
                    match rebuilt {
                        Ok(n) => {
                            if debug {
                                // the decode-verify reads the block's input
                                // bytes: the segment's slice, or the held
                                // slots gathered into one block (debug is a
                                // diagnostic; the copy is the point)
                                let input: Vec<u8> = match &source {
                                    PassSource::Slices(segment) => {
                                        segment[b * block..(b + 1) * block].to_vec()
                                    }
                                    PassSource::Produce(_) => {
                                        let pump =
                                            self.pump.as_ref().expect("hw pass holds a pump");
                                        let mut input = vec![0u8; block];
                                        let mut pos = 0usize;
                                        for c in start..start + count {
                                            let len = chunk_lens[c];
                                            input[pos..pos + len].copy_from_slice(
                                                pump.slot_input(chunk_ring[c], len),
                                            );
                                            pos += len;
                                        }
                                        input
                                    }
                                };
                                verify_debug(
                                    &input,
                                    &packed[base..base + n],
                                    &samples
                                        .as_deref()
                                        .expect("debug pass holds samples")[start..start + count],
                                    b,
                                    req_size,
                                );
                            }
                            // the block resolved: its input slots refill
                            if matches!(source, PassSource::Produce(_)) {
                                let pump = self.pump.as_mut().expect("hw pass holds a pump");
                                for &ring in &chunk_ring[start..start + count] {
                                    pump.release(ring);
                                }
                            }
                            self.hw_requests += 1;
                            lens.push(n);
                            next_emit += 1;
                            continue;
                        }
                        Err(e) => {
                            packed.truncate(base);
                            block_err[b] = Some(e);
                        }
                    }
                }
                // software whole-block fallback: the slices pass reads the
                // segment; the produce pass re-gathers the block first —
                // unsubmitted chunks re-produced from the stream, submitted
                // ones copied out of their still-held slots — then hands
                // the slots back
                let n = match &mut source {
                    PassSource::Slices(segment) => self.soft_append(
                        &segment[b * block..(b + 1) * block],
                        packed,
                        block_err[b].take(),
                    )?,
                    PassSource::Produce(produce) => {
                        let mut staging = vec![0u8; block];
                        let (start, count) = ranges[b];
                        let mut pos = 0usize;
                        if count == 0 {
                            while pos < block {
                                let len = (block - pos).min(req_size);
                                produce(&mut staging[pos..pos + len])?;
                                pos += len;
                            }
                        }
                        for c in start..start + count {
                            let len = chunk_lens[c];
                            match chunk_ring[c] {
                                usize::MAX => produce(&mut staging[pos..pos + len])?,
                                ring => {
                                    let piece = self
                                        .pump
                                        .as_ref()
                                        .expect("hw pass holds a pump")
                                        .slot_input(ring, len);
                                    staging[pos..pos + len].copy_from_slice(piece);
                                }
                            }
                            pos += len;
                        }
                        let n = self.soft_append(&staging, packed, block_err[b].take())?;
                        for &ring in &chunk_ring[start..start + count] {
                            if ring != usize::MAX {
                                self.pump
                                    .as_mut()
                                    .expect("hw pass holds a pump")
                                    .release(ring);
                            }
                        }
                        n
                    }
                };
                lens.push(n);
                next_emit += 1;
            }
            if !self.pump.as_ref().is_some_and(|p| p.busy()) {
                // nothing in flight and the emission above left blocks
                // unresolved: their missing requests can never arrive —
                // fail them over so the lengths stay one-per-block
                for b in next_emit..nblocks {
                    if hw[b] && block_err[b].is_none() && pending[b] > 0 {
                        block_err[b] = Some("request_not_completed".into());
                        self.last_hw_error
                            .get_or_insert_with(|| "request_not_completed".into());
                    }
                }
                continue;
            }
            if stalled.elapsed() >= self.timeout {
                // the queue stopped answering: every block still waiting on
                // requests fails over to software and the handle sticks
                // (the per-block path's timeout semantics); blocks whose
                // requests all landed rebuild from their triples as usual.
                // The abandoned requests keep the queue busy, which keeps
                // every later pass in software too
                self.last_hw_error = Some("request_did_not_complete_in_time".into());
                self.stuck = true;
                timed_out = true;
                for b in next_emit..nblocks {
                    if hw[b] && block_err[b].is_none() && pending[b] > 0 {
                        block_err[b] = Some("request_did_not_complete_in_time".into());
                    }
                }
                continue;
            }
            self.pump.as_ref().expect("hw pass holds a pump").wait_once();
        }
        if let PassSource::Slices(segment) = source {
            let tail = &segment[nblocks * block..];
            if !tail.is_empty() {
                let n = self.soft_append(tail, packed, None)?;
                lens.push(n);
            }
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

    /// Whether an input of `len` bytes can go through the engine: a live,
    /// unstuck queue, an input the parse can program, and no trailing
    /// remainder that would underflow it.
    fn hw_eligible_len(&self, len: usize) -> bool {
        if self.pump.is_none() || self.stuck || len < MIN_HW_INPUT {
            return false;
        }
        let remainder = len % self.req_size;
        remainder == 0 || remainder >= MIN_HW_INPUT
    }

    /// Whether a block can go through the engine: a live, unstuck queue,
    /// an input the parse can program, and no trailing remainder that would
    /// underflow it.
    fn hw_eligible(&self, src: &[u8]) -> bool {
        self.hw_eligible_len(src.len())
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
        self.scratch.clear();
        let n = rebuild::lz77_block_from_requests(src, &requests, &mut self.scratch)?;
        if n > dst.len() {
            return Err(format!("rebuilt block is {n}B, dst holds {}", dst.len()));
        }
        dst[..n].copy_from_slice(&self.scratch[..n]);
        Ok(n)
    }
}
