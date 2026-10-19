//! The production per-block engine path: hardware first, pure-Rust software
//! fallback, no C library in either. [`KaeNative::open`] probes for a ZIP
//! engine and degrades to the lz4rip engine ([`crate::soft`]) when none is
//! present or the queue cannot be opened; on a live queue every block goes
//! through the engine synchronously (load slot → submit → wait → rebuild),
//! and a request that stalls past the timeout or completes with a hardware
//! error fails that whole block over to software — a finished LZ4 block
//! always comes from one engine, never a splice. The completion wait sleeps
//! on the uacce fd (`poll`), which the sweep measured at a quarter of spin's
//! CPU for equal throughput.
//!
//! Decompression is software in every mode (the engine has no LZ4 decode —
//! the driver rejects the decompress direction for LZ4 outright), so the
//! read path runs lz4rip regardless of the backend.

use std::time::Duration;

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

/// A hardware-first LZ4 block compressor with automatic software fallback.
/// One queue per handle; like [`crate::AsyncBatch`], it is `Send` (move it
/// to the worker thread that owns it) but not `Sync`, and compression takes
/// `&mut self` — one block in flight per handle, scale by handing each
/// worker thread its own.
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
    hw_requests: u64,
    sw_requests: u64,
    /// The hardware failure that caused the most recent software fallback.
    last_hw_error: Option<String>,
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
            .finish_non_exhaustive()
    }
}

impl KaeNative {
    /// Open with the zfile block granularity (4096) as the request size.
    pub fn open() -> Result<KaeNative, KaeError> {
        Self::open_with(4096)
    }

    /// Open with an explicit hardware request size (bench blocks cap at the
    /// engine's 128K lz77 flat limit; larger blocks chain requests).
    /// Without a `/dev/hisi_zip-*` device — or when every queue fails to
    /// open — this returns a software-mode handle rather than an error;
    /// [`KaeNative::backend`] tells the two apart.
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
                hw_requests: 0,
                sw_requests: 0,
                last_hw_error: None,
            });
        }
        // one queue, one slot in flight, poll waiting — the synchronous
        // per-block shape the zfile worker loop drives
        match NativePump::open(&devices, 1, req_size, 1, 1, true, NativeAlg::Lz77) {
            Ok(mut pump) => {
                pump.run(0.05); // settle the queue the way the bench does
                Ok(KaeNative {
                    backend: Backend::Native,
                    req_size,
                    note: format!(
                        "mode=native,req_size={req_size},queues=1,depth=1,wait=poll,timeout_ms={}",
                        timeout.as_millis()
                    ),
                    timeout,
                    pump: Some(pump),
                    scratch: Vec::new(),
                    hw_requests: 0,
                    sw_requests: 0,
                    last_hw_error: None,
                })
            }
            Err(e) => Ok(KaeNative {
                backend: Backend::Soft,
                req_size,
                note: format!("mode=soft,queue_open_failed:{e}"),
                timeout,
                pump: None,
                scratch: Vec::new(),
                hw_requests: 0,
                sw_requests: 0,
                last_hw_error: None,
            }),
        }
    }

    /// The backend actually in effect: [`Backend::Native`] compresses on the
    /// engine (software per-request on fallback), [`Backend::Soft`] is
    /// lz4rip throughout.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Open diagnostics: why the handle is in the mode it is (device
    /// absence, queue-open failure) plus the knob values.
    pub fn note(&self) -> &str {
        &self.note
    }

    /// Requests the engine completed (hardware-accepted blocks, including
    /// multi-request ones counted once per block).
    pub fn hw_requests(&self) -> u64 {
        self.hw_requests
    }

    /// Blocks that went to software: no device, too-small input, or a
    /// hardware stall/error at open or request time.
    pub fn sw_requests(&self) -> u64 {
        self.sw_requests
    }

    /// Why the most recent block fell back to software, if one did.
    pub fn last_hw_error(&self) -> Option<&str> {
        self.last_hw_error.as_deref()
    }

    /// Worst-case compressed size of `n` input bytes.
    pub fn bound(&self, n: usize) -> usize {
        soft::bound(n)
    }

    /// Compress one standard LZ4 block. Hardware mode runs the engine
    /// synchronously; a stalled or erroring request fails the whole block
    /// over to the software engine (never a mixed block), counted in
    /// [`KaeNative::sw_requests`] with the cause in
    /// [`KaeNative::last_hw_error`].
    pub fn compress(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize, KaeError> {
        if self.pump.is_none() || src.len() < MIN_HW_INPUT {
            self.sw_requests += 1;
            return soft_block(src, dst, None);
        }
        // a remainder chunk below the minimum input would underflow the
        // parse length: such a block goes to software whole
        let remainder = src.len() % self.req_size;
        if remainder != 0 && remainder < MIN_HW_INPUT {
            self.sw_requests += 1;
            return soft_block(src, dst, None);
        }
        match self.compress_hw(src, dst) {
            Ok(n) => {
                self.hw_requests += 1;
                Ok(n)
            }
            Err(e) => {
                self.sw_requests += 1;
                self.last_hw_error = Some(e.clone());
                soft_block(src, dst, Some(e))
            }
        }
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

    /// The synchronous engine path: one request per `req_size` slice, the
    /// triples of the whole block rebuilt in one walk (a finished LZ4 block
    /// is not composable, so per-request streams are never spliced — the
    /// same walk the bench's sample path uses).
    fn compress_hw(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
        let pump = self.pump.as_mut().expect("native mode has a pump");
        let req_size = self.req_size;
        let mut samples = Vec::with_capacity(src.len() / req_size + 1);
        let mut off = 0usize;
        while off < src.len() {
            let len = (src.len() - off).min(req_size);
            let sample = pump.request_triples(&src[off..off + len], self.timeout.as_secs_f64())?;
            samples.push(sample);
            off += len;
        }
        let requests: Vec<(&[u8], usize)> = samples
            .iter()
            .zip(src.chunks(req_size))
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
