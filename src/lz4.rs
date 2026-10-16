//! LZ4 block-format FFI. The same standard ABI symbols resolve from two
//! libraries:
//! - software: the system liblz4.so.1 (loader default search path)
//! - hardware: the KAE-patched /usr/local/kaelz4/lib/liblz4.so.1.9.4;
//!   compression forwards to libkaelz4.so.2.2.0 -> libwd -> the uacce ZIP
//!   engine; decompression is still software code.
//!
//! zfile's on-disk format is LZ4 block format (per-block
//! LZ4_compress_default), so the bench targets exactly this ABI; KAE output
//! is post-processed inside the library into the standard format, and verify
//! cross-checks that the community liblz4 can decode it.

use crate::loader::Lib;
use std::os::raw::c_int;

/// C ABI of `LZ4_compress_default`: returns the compressed byte count or 0
/// on failure (dst_cap too small).
pub type CompressFn =
    unsafe extern "C" fn(src: *const u8, dst: *mut u8, src_size: c_int, dst_cap: c_int) -> c_int;
/// C ABI of `LZ4_decompress_safe`: returns the decompressed byte count or a
/// negative value on error/mismatched content.
pub type DecompressFn = unsafe extern "C" fn(
    src: *const u8,
    dst: *mut u8,
    compressed_size: c_int,
    dst_cap: c_int,
) -> c_int;
/// C ABI of `LZ4_compressBound`: maximum compressed size for n input bytes,
/// 0 when n exceeds LZ4's block-size limit.
pub type BoundFn = unsafe extern "C" fn(input_size: c_int) -> c_int;

/// The LZ4 block-ABI function pointers resolved from one library handle
/// (patched or system liblz4 expose the same signatures).
#[derive(Clone, Copy)]
pub struct Lz4Fns {
    pub compress: CompressFn,
    pub decompress: DecompressFn,
    pub bound: BoundFn,
}

/// Symbol probe list: the standard ABI plus the KAE 2.0 async surface
/// (candidate bindings for the async pool; the async symbols are exported by
/// the patched liblz4 and forward libkaelz4's KAELZ4_* implementations).
pub const PROBE_SYMBOLS: &[&str] = &[
    "LZ4_compress_default",
    "LZ4_compress_fast",
    "LZ4_compress_HC",
    "LZ4_decompress_safe",
    "LZ4_compressBound",
    "LZ4F_compressFrame",
    "LZ4_compress_async",
    "LZ4_decompress_async",
    "LZ4_async_compress_init",
    "LZ4_teardown_async_compress",
    "KAELZ4_compress_async",
    "KAELZ4_create_async_compress_session",
    "kaelz4_async_compress_polling",
];

impl Lz4Fns {
    /// Resolve the standard block-ABI symbols (`LZ4_compress_default`,
    /// `LZ4_decompress_safe`, `LZ4_compressBound`); the error carries the
    /// dlerror text of the first missing symbol.
    pub fn load(lib: &Lib) -> Result<Lz4Fns, String> {
        unsafe {
            Ok(Lz4Fns {
                compress: lib.sym_fn("LZ4_compress_default")?,
                decompress: lib.sym_fn("LZ4_decompress_safe")?,
                bound: lib.sym_fn("LZ4_compressBound")?,
            })
        }
    }

    /// Maximum compressed size for n input bytes; 0 when n exceeds LZ4's
    /// `c_int` block-size limit (no bound exists for such inputs).
    pub fn bound(&self, n: usize) -> usize {
        if n > c_int::MAX as usize {
            return 0;
        }
        unsafe { (self.bound)(n as c_int) as usize }
    }

    /// Returns the compressed byte count; <=0 counts as failure (matching
    /// liblz4 return-value semantics). Inputs beyond the `c_int` block-size
    /// limit are rejected instead of silently truncating the length.
    pub fn compress_block(&self, src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
        if src.len() > c_int::MAX as usize || dst.len() > c_int::MAX as usize {
            return Err(format!(
                "buffer exceeds LZ4 block limit (src {}B, dst_cap {}B, max {}B)",
                src.len(),
                dst.len(),
                c_int::MAX
            ));
        }
        let n = unsafe {
            (self.compress)(
                src.as_ptr(),
                dst.as_mut_ptr(),
                src.len() as c_int,
                dst.len() as c_int,
            )
        };
        if n <= 0 {
            Err(format!(
                "LZ4_compress_default returned {n} (src {}B, dst_cap {}B)",
                src.len(),
                dst.len()
            ))
        } else {
            Ok(n as usize)
        }
    }

    /// Returns the decompressed byte count; a negative return counts as
    /// failure (matching liblz4 return-value semantics). Inputs beyond the
    /// `c_int` block-size limit are rejected instead of silently truncating
    /// the length.
    pub fn decompress_block(&self, src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
        if src.len() > c_int::MAX as usize || dst.len() > c_int::MAX as usize {
            return Err(format!(
                "buffer exceeds LZ4 block limit (src {}B, dst {}B, max {}B)",
                src.len(),
                dst.len(),
                c_int::MAX
            ));
        }
        let n = unsafe {
            (self.decompress)(
                src.as_ptr(),
                dst.as_mut_ptr(),
                src.len() as c_int,
                dst.len() as c_int,
            )
        };
        if n < 0 {
            Err(format!("LZ4_decompress_safe returned {n}"))
        } else {
            Ok(n as usize)
        }
    }
}
