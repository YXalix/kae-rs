//! # kae: Rust bindings for Kunpeng KAE2 hardware LZ4 (synchronous drop-in)
//!
//! Targets the AgentENV zfile pipeline (LZ4 block format, per-block
//! `LZ4_compress_default`): zero build-time dependencies (nothing linked, no
//! KAE headers needed); at runtime it dlopens the patched
//! `/usr/local/kaelz4/lib/liblz4.so.1.9.4` — a standard LZ4 ABI that
//! internally forwards libkaelz4.so.2.2.0 → libwd → the uacce ZIP engine.
//! Decompression remains software inside the patched library (KAE only
//! accelerates compression); this crate provides it too so the drop-in stays
//! complete.
//!
//! ```no_run
//! use kae::{KaeLz4, Backend};
//!
//! // hw first: probe the patched liblz4 on the default paths; without it,
//! // fall back to the system liblz4 (software).
//! let lz4 = KaeLz4::open().expect("no liblz4 at all");
//! match lz4.backend() {
//!     Backend::Hw => { /* zfile worker takes the hardware compress path */ }
//!     Backend::Sw => { /* keep the existing lz4-sys software path */ }
//! }
//! let block = vec![0u8; 4096]; // 4K per block, zfile granularity
//! let compressed = lz4.compress_vec(&block).expect("compress_vec");
//! let mut restored = vec![0u8; block.len()];
//! let n = lz4.decompress(&compressed, &mut restored).expect("decompress");
//! assert_eq!(&restored[..n], &block[..]);
//! ```
//!
//! Use [`KaeLz4::open_hw`] when only hardware is acceptable (it errors out
//! directly without hardware instead of silently falling back). Operational
//! hard rule (details in the README): the KAE patched library must never be
//! installed into /usr/local/lib or the global LD_LIBRARY_PATH — this
//! crate's RTLD_GLOBAL preloading stays process-local and carries no such
//! risk.
//!
//! Also bundled: the `kae-bench` binary and `benches/throughput`
//! (`cargo bench`) compare software/hardware throughput and CPU cost; see
//! the README.

// Everything below `orchestrator` is internal plumbing: the FFI loader, the
// bench harness and its support modules. The stable surface of this crate is
// the set of root re-exports (`KaeLz4`, `AsyncBatch`, `Backend`, `KaeError`,
// `hw_available`, `hw_lib_path`, the two path constants) plus
// `orchestrator::main` for the bundled bin/bench targets.
mod batch;
mod data;
mod loader;
mod lz4;
pub mod orchestrator;
mod rebuild;
mod sysres;
mod uacce;
mod worker;

pub use batch::AsyncBatch;
use lz4::Lz4Fns;
use std::os::raw::c_int;
use std::path::Path;

/// Default probe paths for the patched liblz4 (the first existing one wins).
pub const HW_LIB_CANDIDATES: [&str; 3] = [
    "/usr/local/kaelz4/lib/liblz4.so.1.9.4",
    "/usr/local/kaelz4/lib/liblz4.so.1",
    "/usr/local/kaelz4/lib/liblz4.so",
];

/// Dependency preload chain (order = dependency order: libwd first; only
/// entries that exist are used).
pub const HW_PRELOAD_CANDIDATES: [&str; 3] = [
    "/usr/local/lib/libwd.so.2",
    "/usr/local/lib/libwd_comp.so.2",
    "/usr/local/kaelz4/lib/libkaelz4.so.2.2.0",
];

/// The preload entries that exist on this machine, in dependency order
/// (missing entries are skipped).
pub(crate) fn existing_preload_paths() -> Vec<String> {
    HW_PRELOAD_CANDIDATES
        .iter()
        .filter(|p| Path::new(p).exists())
        .map(|s| s.to_string())
        .collect()
}

/// The backend actually in effect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// KAE patched liblz4: compression on hardware (the patched library
    /// silently falls back to software when the uacce device nodes are
    /// absent).
    Hw,
    /// System liblz4: pure software.
    Sw,
}

/// Every error variant of this crate. String payloads carry the underlying
/// diagnostic (dlerror text, FFI return codes, status names).
#[derive(Debug)]
pub enum KaeError {
    /// No hardware backend present (only returned by [`KaeLz4::open_hw`]).
    Unavailable,
    /// dlopen failed (including the reason after the preload retry).
    Open(String),
    /// The library lacks the standard LZ4 symbols (what was loaded is not a
    /// patched/system liblz4).
    Symbol(String),
    Compress(String),
    Decompress(String),
    /// Errors on the async batch path (init failure, submit failure,
    /// completion status other than SUCC, etc.).
    Async(String),
}

impl std::fmt::Display for KaeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KaeError::Unavailable => write!(f, "KAE hw backend unavailable"),
            KaeError::Open(e) => write!(f, "dlopen failed: {e}"),
            KaeError::Symbol(e) => write!(f, "missing LZ4 symbols: {e}"),
            KaeError::Compress(e) => write!(f, "compress failed: {e}"),
            KaeError::Decompress(e) => write!(f, "decompress failed: {e}"),
            KaeError::Async(e) => write!(f, "async batch failed: {e}"),
        }
    }
}

impl std::error::Error for KaeError {}

/// Whether this machine can compress on KAE hardware: the ZIP engine has a
/// device node (the driver registers it as `hisi_zip-*` under
/// /sys/class/uacce, with the matching `/dev/hisi_zip-*` character device)
/// and the patched liblz4 is on a default path. A false result does not
/// imply that [`KaeLz4::open`] fails (the software fallback remains).
pub fn hw_available() -> bool {
    hw_lib_path().is_some() && uacce_devices().iter().any(|n| n.starts_with("hisi_zip"))
}

/// The uacce device names present on this machine (e.g. `hisi_sec2-0`,
/// `hisi_zip-4`; empty without the accelerator driver). The driver lists the
/// engines under /sys/class/uacce and each entry is exposed as the matching
/// `/dev/<name>` character device; an entry without its /dev node (a
/// restricted /dev, e.g. inside a container) does not count — the engine
/// could not be opened without it.
pub(crate) fn uacce_devices() -> Vec<String> {
    use std::os::unix::fs::FileTypeExt;

    let Ok(entries) = std::fs::read_dir("/sys/class/uacce") else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            Path::new("/dev")
                .join(name)
                .metadata()
                .is_ok_and(|m| m.file_type().is_char_device())
        })
        .collect();
    names.sort();
    names
}

/// Path of the patched liblz4: the `KAE_HW_LZ4_LIB` environment variable
/// wins, then the default candidates.
pub fn hw_lib_path() -> Option<String> {
    if let Ok(p) = std::env::var("KAE_HW_LZ4_LIB") {
        if !p.is_empty() && Path::new(&p).exists() {
            return Some(p);
        }
    }
    HW_LIB_CANDIDATES
        .iter()
        .find(|p| Path::new(p).exists())
        .map(|s| s.to_string())
}

/// Open the given liblz4: try a direct dlopen first; on failure preload in
/// `preload` order with RTLD_GLOBAL (libwd → libwd_comp → libkaelz4; the
/// SONAME then matches already-loaded objects, working around missing
/// rpaths) and retry once. Returns the library handle plus a diagnostic
/// note (a non-empty note means the retry path was taken).
pub(crate) fn open_lib(lib: &str, preload: &[String]) -> Result<(loader::Lib, String), KaeError> {
    match loader::Lib::open(lib, false) {
        Ok(l) => Ok((l, String::new())),
        Err(first_err) => {
            let loaded = loader::Lib::preload_all(preload);
            match loader::Lib::open(lib, false) {
                Ok(l) => Ok((l, format!("preloaded={loaded};first_err={first_err}"))),
                Err(e) => Err(KaeError::Open(format!(
                    "{e};preload_tried={} first_direct_err={first_err}",
                    preload.len()
                ))),
            }
        }
    }
}

/// KAE LZ4 backend handle. Open once, reuse everywhere; the block API has no
/// interior mutable state, so sharing `&self` is fine.
pub struct KaeLz4 {
    backend: Backend,
    lib_path: String,
    note: String,
    fns: Lz4Fns,
}

impl KaeLz4 {
    /// Default open: hw first (default paths + `KAE_HW_LZ4_LIB`), falling
    /// back to the system liblz4 on failure. Distinguish the actual backend
    /// via [`KaeLz4::backend`]; when both sides fail, the returned error
    /// carries both diagnostics.
    pub fn open() -> Result<KaeLz4, KaeError> {
        match Self::open_hw() {
            Ok(k) => Ok(k),
            Err(hw_err) => Self::open_sw()
                .map_err(|sw_err| KaeError::Open(format!("hw: {hw_err}; sw: {sw_err}"))),
        }
    }

    /// Hardware only: returns [`KaeError::Unavailable`] without the patched
    /// library; no fallback.
    pub fn open_hw() -> Result<KaeLz4, KaeError> {
        let lib = hw_lib_path().ok_or(KaeError::Unavailable)?;
        Self::open_path(&lib, &existing_preload_paths(), Backend::Hw)
    }

    /// System liblz4 (software baseline / no-hardware environments).
    pub fn open_sw() -> Result<KaeLz4, KaeError> {
        Self::open_path("liblz4.so.1", &[], Backend::Sw)
    }

    /// Open the specified liblz4 (`backend` is a label only). On dlopen
    /// failure the `preload` entries are loaded RTLD_GLOBAL in order and the
    /// open is retried once; [`KaeLz4::note`] reports whether that happened.
    pub fn open_path(lib: &str, preload: &[String], backend: Backend) -> Result<KaeLz4, KaeError> {
        let (l, note) = open_lib(lib, preload)?;
        let fns = Lz4Fns::load(&l).map_err(KaeError::Symbol)?;
        Ok(KaeLz4 {
            backend,
            lib_path: lib.to_string(),
            note,
            fns,
        })
    }

    /// The backend actually in effect: what [`KaeLz4::open`] settled for, or
    /// the kind of path handed to [`KaeLz4::open_path`].
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The path or SONAME the library was opened from (exactly what was
    /// passed to the open call).
    pub fn lib_path(&self) -> &str {
        &self.lib_path
    }

    /// Diagnostics: non-empty means the preload retry path was taken
    /// (includes the first error).
    pub fn note(&self) -> &str {
        &self.note
    }

    /// Minimum dst capacity required to compress n input bytes
    /// (LZ4_compressBound).
    pub fn bound(&self, n: usize) -> usize {
        self.fns.bound(n)
    }

    /// Compress one block. The `dst` capacity must be >=
    /// [`KaeLz4::bound`] (use [`KaeLz4::compress_vec`] to avoid capacity
    /// management). Returns the compressed byte count.
    ///
    /// Both the input length and the dst capacity must fit LZ4's `c_int`
    /// block-size limit (~2 GiB); larger blocks are rejected with
    /// [`KaeError::Compress`] instead of being silently truncated.
    pub fn compress(&self, src: &[u8], dst: &mut [u8]) -> Result<usize, KaeError> {
        self.fns
            .compress_block(src, dst)
            .map_err(KaeError::Compress)
    }

    /// Compress one block into a freshly allocated `Vec` sized by
    /// [`KaeLz4::bound`]. The ~2 GiB block-size limit of [`KaeLz4::compress`]
    /// applies.
    pub fn compress_vec(&self, src: &[u8]) -> Result<Vec<u8>, KaeError> {
        let mut dst = vec![0u8; self.bound(src.len())];
        let n = self.compress(src, &mut dst)?;
        dst.truncate(n);
        Ok(dst)
    }

    /// Decompress one block (software inside the patched library). The
    /// `dst` capacity must be >= the original length; returns the number of
    /// bytes actually decompressed. `src`/`dst` lengths must fit LZ4's
    /// `c_int` limit (~2 GiB); larger buffers are rejected with
    /// [`KaeError::Decompress`] instead of being silently truncated.
    pub fn decompress(&self, src: &[u8], dst: &mut [u8]) -> Result<usize, KaeError> {
        if src.len() > c_int::MAX as usize || dst.len() > c_int::MAX as usize {
            return Err(KaeError::Decompress(format!(
                "buffer exceeds LZ4 block limit (src {}B, dst {}B, max {}B)",
                src.len(),
                dst.len(),
                c_int::MAX
            )));
        }
        let n = unsafe {
            (self.fns.decompress)(
                src.as_ptr(),
                dst.as_mut_ptr(),
                src.len() as i32,
                dst.len() as i32,
            )
        };
        if n < 0 {
            Err(KaeError::Decompress(format!(
                "LZ4_decompress_safe returned {n}"
            )))
        } else {
            Ok(n as usize)
        }
    }
}
