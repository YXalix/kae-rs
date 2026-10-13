# kae

Rust bindings for Kunpeng KAE2 hardware LZ4, targeting the AgentENV zfile
pipeline (LZ4 block format, per-block `LZ4_compress_default`): **zero
build-time dependencies** (nothing is linked, no KAE headers needed); at
runtime it dlopens the patched `/usr/local/kaelz4/lib/liblz4.so.1.9.4` — a
standard LZ4 ABI that internally forwards libkaelz4.so.2.2.0 → libwd → the
uacce ZIP engine. Decompression remains a software implementation inside the
patched library (KAE only accelerates compression); this crate wraps it as
well so the drop-in stays complete.

## Documentation

- [docs/architecture.md](docs/architecture.md) — the acceleration chain,
  runtime loading model, module map, async batch internals
- [docs/library-guide.md](docs/library-guide.md) — open strategies, block
  and async batch APIs, error handling, environment variables,
  AgentENV integration shape
- [docs/benchmark.md](docs/benchmark.md) — kae-bench options, methodology,
  metrics, verdict criteria, output formats
- [docs/kae-limitations.md](docs/kae-limitations.md) — measured weaknesses
  of the hardware path, the spin-wait root cause behind the CPU numbers,
  and what would have to change
- [docs/raw-bench.md](docs/raw-bench.md) — the raw uacce engine path
  (`src/uacce.rs`, `--impl raw`): the crate's own ZIP-engine driver, no C
  library in the process; measures the hardware ceiling behind the C stack
- [docs/operations.md](docs/operations.md) — KAE 2.x stack installation,
  deployment layout, hard rules, health checks, troubleshooting, upstream
  references

## Library usage

```rust
use kae::{KaeLz4, Backend};

// hw first (default path probing, overridable via KAE_HW_LZ4_LIB);
// falls back to the system liblz4 when no hardware is present
let lz4 = KaeLz4::open().expect("no liblz4 at all");
match lz4.backend() {
    Backend::Hw => { /* zfile worker takes the hardware compress path */ }
    Backend::Sw => { /* keep the existing lz4-sys software path */ }
}

let block = vec![0u8; 4096];                    // zfile granularity: 4K per block
let compressed = lz4.compress_vec(&block)?;     // or compress(src, &mut dst)
let mut restored = vec![0u8; block.len()];
let n = lz4.decompress(&compressed, &mut restored)?;
assert_eq!(&restored[..n], &block[..]);
```

- `KaeLz4::open_hw()`: hardware only; returns `KaeError::Unavailable` when
  absent, with **no** silent fallback — fits an adoption policy of "enable
  acceleration only when hardware is present, otherwise keep the current
  path".
- `kae::hw_available()`: a ZIP engine device node (`/dev/hisi_zip-*`) exists
  and the patched library is in place (note: without uacce devices the
  patched library silently falls back to software compression, so
  `Backend::Hw` only means "the patched library was loaded").
- When a plain open fails, dependencies are preloaded RTLD_GLOBAL in
  libwd → libwd_comp → libkaelz4 order and the open is retried (`note()`
  returns the diagnostics), working around missing rpaths.

### AgentENV integration shape

With this crate as an optional dependency, the WorkerPool worker loop
(zfile.rs, per segment) calls `open()` once, then `compress()` per block.

## Segment-level async batching (AsyncBatch)

Binds the patched liblz4's `LZ4_compress_async` (multi-threaded async mode):
submit all n blocks of a whole segment to the KAE task queue in one call,
compressed in parallel, blocking until every callback has completed — the
insertion shape for the zfile WorkerPool at segment level.

```rust
let mut batch = kae::AsyncBatch::open()?;      // hardware only; errors without the patched library, no fallback
let mut packed = Vec::new();
let lens: Vec<usize> = batch.compress_segment(&segment, 4096, &mut packed)?;
```

Semantics of the binding:

- **Queue-full backoff**: `KAE_LZ4_TASK_QUEUE_FULL` is returned at submit
  time — wait for one in-flight request to complete and resubmit the same
  block; a full queue with nothing in flight from this batch means the queue
  is hogged externally: raise an error.
- **Callback-thread agnostic**: callbacks fire from the library thread pool;
  small blocks taking the software fallback may trigger the callback
  synchronously inline on the submitting thread — completion counting uses
  atomics + a condvar, correct under both trigger styles.
- **Trailing partial block**: a segment tail below the block granularity is
  compressed synchronously through the block API and appended in order, so
  segments need not be block-multiples (an empty segment is an error).
- **`Send`, not `Sync`**: an `AsyncBatch` can be moved to the worker thread
  that owns it (one instance per worker, one segment at a time — the
  `compress_segment` API takes `&mut self`).
- **Returns only after draining**: before any error return, wait for all
  submitted requests to call back, so callbacks never touch freed memory.
- Process-wide init runs once (`LZ4_async_compress_init` preferred: it
  registers the library's software fallback automatically; falls back to
  `KAELZ4_async_compress_init` with the standard symbols registered
  explicitly). Global teardown is deliberately never called (it would destroy
  the entire KAE thread pool; process exit reclaims it naturally).
- Library thread pool size via `KAE_LZ4_ASYNC_THREAD_NUM`, compression level
  via `KAE_LZ4_LEVEL` — set as environment variables, or programmatically
  via `AsyncBatch::configure(threads, level)` before the first
  `AsyncBatch::open` of the process (the engine init is one-shot and reads
  them there; later calls return `KaeError::Async`).
- bench: `kae-bench --impl hwasync` (compress only); `--workers N` with
  N > 1 sets the library thread pool size for the run; samples are
  cross-validated through the system liblz4 as usual.

## Runnable examples

```bash
cargo run --example block         # open strategies + backend probe + block roundtrip (runs anywhere)
cargo run --example async_batch   # segment-level AsyncBatch (needs a KAE machine; explains itself otherwise)
```

## Testing

```bash
cargo test                 # everywhere: software paths, fallback and gating contracts
cargo test -- --ignored    # KAE machines only: hardware block matrix,
                           # stock-liblz4 cross-validation, async batch roundtrip
```

The default suite is green on hardware-less dev machines; the tests that need
the device are marked `#[ignore]` (`tests/hw_required.rs`) and run only with
`--ignored`.

## Bundled: the kae-bench benchmark

```bash
cargo build --release
./target/release/kae-bench --probe        # environment & symbol probe (incl. the async surface)
./target/release/kae-bench                # full scan: sw / hw / hwasync / raw (~2 min)
./target/release/kae-bench --input /path/to/memory.img \
    --blocks 4096,65536,1048576 --ops compress
./target/release/kae-bench --impl sw,hw,hwasync --workers 8 \
    --ops compress                        # hardware vs an 8-thread software pool
cargo bench -- --ops compress             # bench entry point (same logic)
```

- Every (block size × sw/hw × compress/decompress) combination runs
  in a **subprocess**: the patched library and the system library share the
  SONAME liblz4.so.1 and would pollute each other inside one process. The
  `raw` cells carry no library at all (in-crate uacce engine path); their
  lz77 triples are rebuilt into standard LZ4 blocks in the worker, so the
  same LZ4 cross-validation sample applies to them.
- `--workers N` with N > 1 turns the sw/hw cells into N-threaded block
  pools — the AgentENV zfile WorkerPool shape (each thread compresses its
  interleaved share of the file's blocks) — so `--impl sw,hw --workers 8`
  measures hardware against equal-budget multi-threaded software
  compression, not just the single-thread drop-in baseline. Pool rows
  record their size in the `note` column (`workers=N`); for `hwasync`,
  `--workers` >1 sets the library's internal async thread pool
  (`KAE_LZ4_ASYNC_THREAD_NUM`).
- Metrics: wall-clock GB/s + CPU (user+sys) s/GB — the criterion for a
  hardware offload is usually saved CPU rather than saved time; the closing
  verdict applies these criteria.
- Cross-validation: hw-compressed samples (first 32 MiB) are decoded and
  compared by the system liblz4, confirming the on-disk format is unchanged
  and the read path can stay in software.
- The synthetic dataset is a 4K-page-interleaved mix (50% zero + 20% random
  + 30% text pages, fixed seed) approximating a VM memory snapshot's
  compressibility; `--input` swaps in a real file (a VM memory image, an
  all-zero or incompressible file for best/worst cases); results
  are written as TSV/JSON (`--tsv/--json`).

Compile on a dev machine and copy to production: the binary's highest glibc
symbol is GLIBC_2.34 (fine on Debian 12); verify with
`objdump -T kae-bench | grep GLIBC | sort -u`, and if that is not satisfied
just `cargo build --release` on the target machine (rustc 1.70+ suffices,
zero dependencies).

## Raw uacce engine path (the native backend seed)

`src/uacce.rs` drives the ZIP engine directly over the uacce queue ABI —
ioctl setup, mmap'd SQ/CQ rings, doorbell stores, CQE harvest, DMA from
the kernel-allocated SS share region — with **no libwd, no libkaelz4 and
no patched liblz4 in the process**, and still zero crate dependencies
(the FFI is hand-bound like the getrusage shim in sysres). The bench
exposes it as a fourth implementation:

```bash
./target/release/kae-bench --impl raw                          # engine ceiling cells
./target/release/kae-bench --impl raw --workers 2              # two hardware queues
KAE_RAW_WAIT=poll ./target/release/kae-bench --impl raw        # sleep-until-IRQ waiting
KAE_RAW_ALG=deflate ./target/release/kae-bench --impl raw      # plumbing-check dialect
```

In-flight depth, doorbell batching and spin-vs-poll waiting are all knobs
here, so the same tool that measures the engine's true ceiling doubles as
the testbed for the policies the C stack never had (deep pipelining,
batched doorbells, sleep-until-IRQ). Compress only; QM v3 devices;
`/dev/hisi_zip-*` required (cells skip with a printed reason otherwise).
The raw lz77 output (literal/sequence triples) is rebuilt into standard
LZ4 blocks for the cross-validation sample, so raw rows report a true LZ4
ratio and get the same system-liblz4 verify as every other implementation.
See [docs/raw-bench.md](docs/raw-bench.md) for the full matrix and the
ceiling-interpretation framework.

## Operational hard rules

Never put the KAE patched library into `/usr/local/lib`, and never add
`/usr/local/kae*` to the global `LD_LIBRARY_PATH` or `ld.so.conf.d` — that
would hijack every host process that depends on liblz4/libzstd/libz (gcc cc1
and readelf both break). This crate's RTLD_GLOBAL preloading happens only
inside its own process. libkaelz4-related environment variables:
`KAE_LZ4_LEVEL`, `KAE_LZ4_ASYNC_THREAD_NUM`, `KAE_LZ4_ASYNC_DC_THREAD_NUM`.
