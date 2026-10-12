//! Segment-level asynchronous batch compression pool (KAE multi-threaded
//! async mode).
//!
//! Binds the patched liblz4's exported `LZ4_compress_async` (libkaelz4's
//! `KAELZ4_compress_async` underneath): submit a batch of 4K blocks to the
//! KAE task queue in one call, compressed in parallel by hardware, and the
//! submitting thread returns once every callback has fired — the shape used
//! at segment level in the zfile WorkerPool.
//!
//! Binding semantics the wrapper is designed around:
//! - Submit returning 0 = queued; `KAE_LZ4_TASK_QUEUE_FULL`(9) is returned
//!   at submit time → the wrapper backs off: wait for one in-flight request
//!   to complete, then resubmit the same block.
//! - Callbacks fire from the library's internal threads; small blocks may
//!   take the software fallback and trigger the callback **synchronously
//!   inline** on the submitting thread → completion counting must be
//!   thread-agnostic (atomics + condvar), correct under both trigger styles.
//! - On completion the adapter fills in `result.status` (0=SUCC) and
//!   `result.dst_len` and leaves `user_data` alone. This wrapper does not
//!   rely on user_data: `kaelz4_result` sits at Slot offset 0 and the
//!   callback recovers the Slot directly from the result pointer.
//! - `KAELZ4_async_compress_init` is a process-wide one-time initialization
//!   (starts the thread pool and registers the software fallback);
//!   `KAELZ4_teardown_async_compress` is a global teardown — this wrapper
//!   only inits, never tears down (process exit reclaims everything), to
//!   avoid disrupting other instances.
//!
//! Queue full + nothing in flight from this batch = the queue is hogged from
//! the outside; that is returned as an error. Before any error return, wait
//! for all submitted requests to finish calling back (drain), otherwise the
//! callbacks would touch freed stack memory.

use crate::loader::Lib;
use crate::lz4::Lz4Fns;
use crate::{open_lib, KaeError};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

pub(crate) const KAE_LZ4_SUCC: c_int = 0;
pub(crate) const KAE_LZ4_TASK_QUEUE_FULL: c_int = 9;

pub(crate) fn status_name(s: c_int) -> &'static str {
    match s {
        1 => "INVAL_PARA",
        2 => "INIT_FAIL",
        3 => "COMP_FAIL",
        4 => "RELEASE_FAIL",
        5 => "ALLOC_FAIL",
        6 => "SET_FAIL",
        7 => "HW_TIMEOUT_FAIL",
        8 => "DST_BUF_OVERFLOW",
        9 => "TASK_QUEUE_FULL",
        _ => "UNKNOWN",
    }
}

#[repr(C)]
struct KaeBuffer {
    buf_len: usize,
    data: *mut u8,
}

#[repr(C)]
struct KaeBufferList {
    buf_num: u32,
    rsvd: u32,
    buf: *mut KaeBuffer,
    usr_data: *mut c_void,
}

/// Must sit at Slot offset 0 (the callback uses it to recover the Slot
/// address; see the module docs).
#[repr(C)]
struct KaeResult {
    status: c_int,
    rsvd: u32,
    user_data: *mut c_void,
    src_size: usize,
    dst_len: usize,
    ibuf_crc: *mut u32,
    obuf_crc: *mut u32,
}

type AsyncCallback = unsafe extern "C" fn(*mut KaeResult);
type AsyncCompressFn = unsafe extern "C" fn(
    *const KaeBufferList,
    *mut KaeBufferList,
    AsyncCallback,
    *mut KaeResult,
) -> c_int;
type IovaMapFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type AsyncInitFn = unsafe extern "C" fn(Option<IovaMapFn>) -> c_int;
type SwCompressFn = unsafe extern "C" fn(*const c_char, *mut c_char, c_int, c_int) -> c_int;
type SwFrameFn =
    unsafe extern "C" fn(*mut c_void, usize, *const c_void, usize, *const c_void) -> usize;
type SwDecompressFn = unsafe extern "C" fn(*const c_char, *mut c_char, c_int, c_int) -> c_int;
type Kaelz4InitFn =
    unsafe extern "C" fn(Option<IovaMapFn>, SwCompressFn, SwFrameFn, SwDecompressFn) -> c_int;

/// One request slot per block. The C structs are self-referential (list.buf
/// points at the same slot's buffer); reinitialized on every
/// compress_segment and never moved while requests are in flight.
#[repr(C)]
struct Slot {
    result: KaeResult, // must be the first field
    src_buf: KaeBuffer,
    src_list: KaeBufferList,
    dst_buf: KaeBuffer,
    dst_list: KaeBufferList,
    shared: *const BatchShared,
}

struct BatchShared {
    completed: AtomicUsize,
    mutex: Mutex<()>,
    cond: Condvar,
}

unsafe extern "C" fn on_done(result: *mut KaeResult) {
    // result is the Slot's base address (see the Slot layout note)
    let slot = unsafe { &*(result as *const Slot) };
    let shared = unsafe { &*slot.shared };
    shared.completed.fetch_add(1, Ordering::AcqRel);
    // A poisoned lock must not panic across the FFI boundary (that would
    // abort the process); the guard only serves to serialize notify_all.
    let _g = shared.mutex.lock().unwrap_or_else(|p| p.into_inner());
    shared.cond.notify_all();
}

fn wait_completed_ge(shared: &BatchShared, target: usize) {
    let mut g = shared.mutex.lock().unwrap();
    while shared.completed.load(Ordering::Acquire) < target {
        g = shared.cond.wait(g).unwrap();
    }
}

/// Process-wide one-time init (the result is cached; repeated opens in the
/// same process reuse it).
fn global_init(lib: &Lib) -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| unsafe {
        // Once the init attempt begins, the engine reads its tuning from the
        // environment and configure() must refuse further changes
        ASYNC_ENGINE_INITIALIZED.store(true, Ordering::Release);
        // Prefer the patched liblz4's one-arg wrapper: it registers the
        // library's software fallbacks for us
        if let Ok(init1) = lib.sym_fn::<AsyncInitFn>("LZ4_async_compress_init") {
            let r = init1(None);
            return if r == 0 {
                Ok(())
            } else {
                Err(format!(
                    "LZ4_async_compress_init returned {r} ({})",
                    status_name(r)
                ))
            };
        }
        // Otherwise bind libkaelz4's four-arg version directly, taking the
        // software fallbacks from the same library's standard symbols
        let init4: Kaelz4InitFn = lib
            .sym_fn("KAELZ4_async_compress_init")
            .map_err(|e| format!("no async init symbol: {e}"))?;
        let sw: SwCompressFn = lib.sym_fn("LZ4_compress_default")?;
        let frame: SwFrameFn = lib.sym_fn("LZ4F_compressFrame")?;
        let dec: SwDecompressFn = lib.sym_fn("LZ4_decompress_safe")?;
        let r = init4(None, sw, frame, dec);
        if r == 0 {
            Ok(())
        } else {
            Err(format!(
                "KAELZ4_async_compress_init returned {r} ({})",
                status_name(r)
            ))
        }
    })
    .clone()
}

/// KAE LZ4 asynchronous batch compressor (requires the patched liblz4; the
/// system liblz4 has no async symbols, so open fails with
/// `KaeError::Symbol` instead of silently falling back to software —
/// segment-level async is an explicit choice).
pub struct AsyncBatch {
    _lib: Lib,
    fns: Lz4Fns,
    compress_async: AsyncCompressFn,
    note: String,
    scratch: Vec<u8>,
    slots: Vec<Slot>,
}

// Safety: every raw pointer inside (the `Lib` dlopen handle, the slot src/dst
// pointers) is dereferenced only while `&mut self` is held on the owning
// thread, and `Lib` never closes its handle, so the function pointers and
// slots stay valid for the process lifetime. The C async entry points are
// designed for concurrent submission from multiple threads. `Sync` stays
// unimplemented on purpose: the scratch/slots reuse requires exclusive
// access, matching the `&mut self` API.
unsafe impl Send for AsyncBatch {}

/// Whether the process-wide async engine init has started (set inside
/// [`global_init`]); [`AsyncBatch::configure`] refuses to run afterwards.
static ASYNC_ENGINE_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Backoff headroom on top of n (the other in-flight blocks): n waits are
/// far beyond enough for the queue to drain before giving up on a block.
const BACKOFF_CAP_SLACK: u32 = 64;

/// A `KaeResult` carrying the not-yet-completed sentinel status.
fn blank_result() -> KaeResult {
    KaeResult {
        status: -1,
        rsvd: 0,
        user_data: std::ptr::null_mut(),
        src_size: 0,
        dst_len: 0,
        ibuf_crc: std::ptr::null_mut(),
        obuf_crc: std::ptr::null_mut(),
    }
}

/// A Slot with sentinel state: every used field is overwritten per segment
/// before submission; pointers are null until then.
fn blank_slot() -> Slot {
    Slot {
        result: blank_result(),
        src_buf: KaeBuffer {
            buf_len: 0,
            data: std::ptr::null_mut(),
        },
        src_list: KaeBufferList {
            buf_num: 1,
            rsvd: 0,
            buf: std::ptr::null_mut(),
            usr_data: std::ptr::null_mut(),
        },
        dst_buf: KaeBuffer {
            buf_len: 0,
            data: std::ptr::null_mut(),
        },
        dst_list: KaeBufferList {
            buf_num: 1,
            rsvd: 0,
            buf: std::ptr::null_mut(),
            usr_data: std::ptr::null_mut(),
        },
        shared: std::ptr::null(),
    }
}

impl AsyncBatch {
    /// Set the async engine tuning programmatically instead of via the
    /// environment: `threads` maps to `KAE_LZ4_ASYNC_THREAD_NUM` (library
    /// thread pool size), `level` to `KAE_LZ4_LEVEL` (compression level);
    /// `None` leaves the value untouched. Must run before the first
    /// [`AsyncBatch::open`] of the process — the engine init is a one-shot
    /// step that reads these values and caches its result, so a later call
    /// returns [`KaeError::Async`]. Calling `configure` concurrently with an
    /// open races over tuning values only, never memory safety.
    pub fn configure(threads: Option<usize>, level: Option<i32>) -> Result<(), KaeError> {
        if ASYNC_ENGINE_INITIALIZED.load(Ordering::Acquire) {
            return Err(KaeError::Async(
                "async engine already initialized: configure() must run before the first AsyncBatch::open".into(),
            ));
        }
        if let Some(t) = threads {
            std::env::set_var("KAE_LZ4_ASYNC_THREAD_NUM", t.to_string());
        }
        if let Some(l) = level {
            std::env::set_var("KAE_LZ4_LEVEL", l.to_string());
        }
        Ok(())
    }

    /// Open via the default paths (patched-liblz4 probing +
    /// `KAE_HW_LZ4_LIB`); `KaeError::Unavailable` otherwise.
    pub fn open() -> Result<AsyncBatch, KaeError> {
        let lib = crate::hw_lib_path().ok_or(KaeError::Unavailable)?;
        Self::open_path(&lib, &crate::existing_preload_paths())
    }

    /// Open the specified patched liblz4 with an explicit preload list
    /// (same preload-retry semantics as [`KaeLz4::open_path`]; whether the
    /// retry fired is reported by [`AsyncBatch::note`]).
    pub fn open_path(lib_path: &str, preload: &[String]) -> Result<AsyncBatch, KaeError> {
        let (lib, note) = open_lib(lib_path, preload)?;
        let fns = Lz4Fns::load(&lib).map_err(KaeError::Symbol)?;
        let compress_async: AsyncCompressFn = unsafe {
            lib.sym_fn("LZ4_compress_async")
                .or_else(|_| lib.sym_fn("KAELZ4_compress_async"))
        }
        .map_err(KaeError::Symbol)?;
        global_init(&lib).map_err(KaeError::Async)?;
        Ok(AsyncBatch {
            _lib: lib,
            fns,
            compress_async,
            note,
            scratch: Vec::new(),
            slots: Vec::new(),
        })
    }

    /// Diagnostics: non-empty means the open went through the preload retry
    /// path (includes the first error).
    pub fn note(&self) -> &str {
        &self.note
    }

    /// Segment-level batch compression: split `src` into n blocks of `block`
    /// bytes each and submit them all to the KAE queue for parallel
    /// compression, blocking until completion; a trailing partial block
    /// below the granularity is compressed synchronously through the block
    /// API and appended, so `src` need not be a multiple of `block` (an
    /// empty segment is an error). Pack the results into `out` (cleared
    /// first) in block order and return the per-block compressed lengths.
    /// `&mut self`: scratch/slots are reused across calls.
    pub fn compress_segment(
        &mut self,
        src: &[u8],
        block: usize,
        out: &mut Vec<u8>,
    ) -> Result<Vec<usize>, KaeError> {
        if block == 0 {
            return Err(KaeError::Compress("block size 0".into()));
        }
        if src.is_empty() {
            return Err(KaeError::Compress("empty segment".into()));
        }
        let n = src.len() / block;
        let tail = &src[n * block..];
        let bp = self.fns.bound(block);
        if self.scratch.len() < n * bp {
            self.scratch.resize(n * bp, 0);
        }
        if self.slots.len() < n {
            self.slots.resize_with(n, blank_slot);
        }

        let shared = BatchShared {
            completed: AtomicUsize::new(0),
            mutex: Mutex::new(()),
            cond: Condvar::new(),
        };
        let mut submitted = 0usize;
        let mut fatal: Option<String> = None;

        for i in 0..n {
            let slot = &mut self.slots[i];
            slot.src_buf = KaeBuffer {
                buf_len: block,
                data: unsafe { src.as_ptr().add(i * block) as *mut u8 },
            };
            slot.src_list = KaeBufferList {
                buf_num: 1,
                rsvd: 0,
                buf: &mut slot.src_buf,
                usr_data: std::ptr::null_mut(),
            };
            slot.dst_buf = KaeBuffer {
                buf_len: bp,
                data: unsafe { self.scratch.as_mut_ptr().add(i * bp) },
            };
            slot.dst_list = KaeBufferList {
                buf_num: 1,
                rsvd: 0,
                buf: &mut slot.dst_buf,
                usr_data: std::ptr::null_mut(),
            };
            slot.result = blank_result();
            slot.result.src_size = block;
            slot.shared = &shared;

            // Queue-full backoff: wait for one in-flight completion, then
            // resubmit; nothing in flight from this batch means the queue is
            // hogged externally — error out
            let mut backoff = 0u32;
            loop {
                let r = unsafe {
                    (self.compress_async)(
                        &slot.src_list,
                        &mut slot.dst_list,
                        on_done,
                        &mut slot.result,
                    )
                };
                if r == 0 {
                    submitted += 1;
                    break;
                }
                let done = shared.completed.load(Ordering::Acquire);
                if r == KAE_LZ4_TASK_QUEUE_FULL && done < submitted {
                    backoff += 1;
                    if backoff > n as u32 + BACKOFF_CAP_SLACK {
                        fatal = Some(format!("block {i}: queue-full backoff exhausted"));
                        break;
                    }
                    wait_completed_ge(&shared, done + 1);
                    continue;
                }
                fatal = Some(format!(
                    "block {i}: submit returned {r} ({})",
                    status_name(r)
                ));
                break;
            }
            if fatal.is_some() {
                break;
            }
        }

        // Drain: even the error path must wait for all submitted callbacks
        // before the stack-allocated shared can be released safely
        wait_completed_ge(&shared, submitted);
        if let Some(e) = fatal {
            return Err(KaeError::Async(e));
        }

        out.clear();
        let mut lens = Vec::with_capacity(n);
        for i in 0..n {
            let slot = &self.slots[i];
            if slot.result.status != KAE_LZ4_SUCC {
                return Err(KaeError::Async(format!(
                    "block {i}: status {} ({})",
                    slot.result.status,
                    status_name(slot.result.status)
                )));
            }
            let l = slot.result.dst_len;
            if l > bp {
                return Err(KaeError::Async(format!(
                    "block {i}: dst_len {l} > bound {bp}"
                )));
            }
            out.extend_from_slice(&self.scratch[i * bp..i * bp + l]);
            lens.push(l);
        }
        if !tail.is_empty() {
            lens.push(compress_tail(&self.fns, tail, out)?);
        }
        Ok(lens)
    }
}

/// Synchronous compression for the trailing partial block (below the async
/// batch granularity); appends to `out` and returns the compressed length.
fn compress_tail(fns: &Lz4Fns, tail: &[u8], out: &mut Vec<u8>) -> Result<usize, KaeError> {
    let mut buf = vec![0u8; fns.bound(tail.len())];
    let n = fns
        .compress_block(tail, &mut buf)
        .map_err(KaeError::Compress)?;
    out.extend_from_slice(&buf[..n]);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_int;

    // The tail path runs through the same block ABI as the public KaeLz4
    // API, so the system liblz4 suffices to exercise it.
    #[test]
    fn tail_block_roundtrips_through_the_block_api() {
        let lib = crate::loader::Lib::open("liblz4.so.1", false).expect("system liblz4");
        let fns = Lz4Fns::load(&lib).expect("standard LZ4 symbols");
        let tail: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut out = Vec::new();
        let l = compress_tail(&fns, &tail, &mut out).expect("tail compress");
        assert_eq!(l, out.len());
        let mut restored = vec![0u8; tail.len() + 64];
        let n = unsafe {
            (fns.decompress)(
                out.as_ptr(),
                restored.as_mut_ptr(),
                out.len() as c_int,
                restored.len() as c_int,
            )
        };
        assert_eq!(n as usize, tail.len(), "decompressed length");
        assert_eq!(&restored[..tail.len()], &tail[..], "bit-exact match");
    }
}
