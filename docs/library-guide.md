# Library guide

Using `kae` as a Rust dependency: opening a backend, the block API, the
segment-level async batch API, error handling, and the environment variables
that tune behavior. For how it works underneath, see
[architecture.md](architecture.md). Runnable versions of the snippets live
in `examples/`: `cargo run --example block` (block API, runs anywhere) and
`cargo run --example async_batch` (segment-level async, needs a KAE
machine).

## Choosing an open strategy

| Call | Behavior |
|---|---|
| `KaeLz4::open()` | Hardware first (patched liblz4 via `KAE_HW_LZ4_LIB` or the default probe paths), falling back to the system liblz4. Use when "accelerate if present" is the policy. When both sides fail, the error carries both diagnostics. |
| `KaeLz4::open_hw()` | Hardware only; `KaeError::Unavailable` without the patched library — no silent fallback. Use for "enable acceleration only when hardware is present, otherwise keep the existing path". |
| `KaeLz4::open_sw()` | System liblz4, software baseline. |
| `KaeLz4::open_path(lib, preload, backend)` | Open an explicit library; `backend` is a label only. |
| `AsyncBatch::open()` / `open_path()` | Hardware async batch only — the system liblz4 has no async symbols, so a stock library fails with `KaeError::Symbol` rather than degrading. |
| `KaeNative::open()` / `open_with(req)` | The crate's own engine path: hardware first, lz4rip software fallback — no C library, and the fallback is automatic (mode visible via `backend()`/`note()`). Decompression is lz4rip in every mode. |

Related probes:

- `kae::hw_available()` — `true` when the patched library is findable **and**
  a ZIP engine device node (`/dev/hisi_zip-*`) exists. A `false` from this
  does not mean `open()` fails; the software fallback remains.
- `kae::hw_lib_path()` — the resolved patched-library path (`KAE_HW_LZ4_LIB`
  wins, then the built-in candidates), `None` if absent.
- `KaeLz4::backend()` — which backend actually came up (`Backend::Hw` means
  the patched library was loaded; without uacce devices that library itself
  compresses in software).
- `KaeLz4::lib_path()` / `KaeLz4::note()` — what was opened and whether the
  preload retry path was taken (with the first error embedded);
  `AsyncBatch::note()` reports the same for the async open.

## Block API (per-block, `KaeLz4`)

```rust
use kae::{KaeLz4, Backend};

let lz4 = KaeLz4::open()?;

// capacity-managed variant
let compressed: Vec<u8> = lz4.compress_vec(&block)?;

// caller-managed variant: dst.len() >= lz4.bound(src.len())
let mut dst = vec![0u8; lz4.bound(block.len())];
let n = lz4.compress(&block, &mut dst)?;   // n = compressed bytes

// dst.len() must be >= the original length; returns decompressed bytes
let mut restored = vec![0u8; block.len()];
let m = lz4.decompress(&compressed, &mut restored)?;
assert_eq!(&restored[..m], &block[..]);
```

Notes:

- The format is the LZ4 **block** format with one `LZ4_compress_default`
  call per block — the granularity the AgentENV zfile pipeline uses (4K).
  Output is decodable by any standard LZ4 block decoder.
- `bound(n)` is `LZ4_compressBound(n)`; `compress` fails if `dst` is
  smaller.
- Block sizes are limited by LZ4's `c_int` ABI (~2 GiB); larger inputs are
  rejected with an explicit error instead of being silently truncated.
- `decompress` returns the number of bytes actually produced; a negative
  return from the C layer surfaces as `KaeError::Decompress`.
- `compress_vec`/`decompress` are safe APIs; the unsafe FFI lives behind the
  handle.
- A `KaeLz4` is opened once and shared freely: the block API has no interior
  mutable state and the C functions are reentrant.

## The zero-C handle with software fallback (`KaeNative`)

The crate's own engine path wraps the uacce ZIP driver in the same
block-level API shape — hardware first, pure-Rust software fallback, no C
library in either mode:

```rust
use kae::{native_available, Backend, KaeNative};

let mut nat = KaeNative::open()?;          // request size = 4096 (zfile granularity)
match nat.backend() {
    Backend::Native => { /* engine path; note() carries the knobs */ }
    Backend::Soft => { /* no /dev/hisi_zip-* or queue open failed: lz4rip throughout */ }
    _ => unreachable!(),
}
let packed = nat.compress_vec(&block)?;    // standard LZ4 block out
let n = nat.decompress(&packed, &mut restored)?;  // software in every mode
```

Semantics:

- **Open-time fallback**: without a device (or when the queue cannot be
  opened) `open` succeeds in software mode; `backend()`/`note()` say which
  mode came up and why. `kae::native_available()` reports device presence
  without opening anything.
- **Per-request fallback on hardware**: each block compresses through the
  engine synchronously (one request per ≤128K slice, rebuilt into one LZ4
  block). A request that stalls past `KAE_NATIVE_TIMEOUT_MS` (default
  500) or completes with a hardware error fails the whole block over to
  the lz4rip engine — a block always comes from one engine, never a
  splice. `hw_requests()`/`sw_requests()` count the split and
  `last_hw_error()` names the most recent fallback cause.
- **Inputs below 16 bytes** (and remainders that would leave such a tail
  request) go to software whole: the engine's lz77 parse programs
  `in_len - 12`.
- **Segment batch is the throughput shape**: `compress_segment(&segment,
  block, &mut packed)` drives the whole segment's blocks through the
  in-flight ring (depth from `KAE_NATIVE_DEPTH`, default 8) in one call —
  the depth is what hides the engine's per-request latency, which the
  per-block `compress` loop structurally cannot (it always drains before
  the next block). Packing and returned lengths mirror
  `AsyncBatch::compress_segment`: concatenated block bytes in order, the
  tail last, one length per entry. Any block whose requests fail goes to
  software whole; the tail always does.
- **Decompression is software in every mode** — the engine has no LZ4
  decode direction (the driver rejects it), so the read path is lz4rip
  regardless of the backend.
- **A wedged queue sticks**: a timed-out request or a stuck ring sets
  `stuck()`, and every further block goes to software until the handle is
  reopened. A per-request hardware status error only fails its own block.
- `Send`, not `Sync`, like `AsyncBatch`: move one handle to the worker
  thread that owns it; scale by handing each worker thread its own (each
  opens its own queue).

## Segment-level async batch (`AsyncBatch`)

One call submits all whole blocks of a segment to the KAE task queue; the
library compresses them in parallel and the call returns after every
callback has completed.

```rust
let mut batch = kae::AsyncBatch::open()?;   // hardware only, no fallback

let mut packed = Vec::new();
let lens = batch.compress_segment(&segment, 4096, &mut packed)?;
// packed  = the blocks' compressed bytes concatenated in block order,
//           followed by the compressed trailing partial block (if any)
// lens[i] = compressed length of block i (last entry may be the tail)
```

Semantics:

- `&mut self` on purpose: scratch/slot storage is reused across calls. Move
  one instance to the worker thread that owns it (one segment at a time).
- Segments need not be block multiples; the tail below the granularity is
  compressed synchronously and appended. An empty segment or a zero block
  size is an error.
- Queue-full submits are retried after an in-flight completion; a queue
  hogged by someone else is an error, not a hang.
- On any error return, all submitted requests have already completed
  (drain-before-error), so callers need no cleanup logic.
- Decompression has no async path — the hardware accelerates compression
  only, so `compress_segment` is the whole async surface.

### Tuning

The async engine reads its tuning once, at the process's first
`AsyncBatch::open` (engine init is a one-shot step). Set values either as
environment variables or programmatically **before the first open**:

```rust
kae::AsyncBatch::configure(
    Some(16),  // threads  -> KAE_LZ4_ASYNC_THREAD_NUM (library thread pool)
    Some(1),   // level    -> KAE_LZ4_LEVEL (compression level)
)?;            // after the first open this returns KaeError::Async
```

`None` leaves a value untouched. Concurrent `configure` + `open` races over
tuning values only, never memory safety.

## Error handling

All failures surface as `kae::KaeError` (implements `std::error::Error` +
`Display`):

| Variant | When |
|---|---|
| `Unavailable` | Hardware required but the patched library is absent (`open_hw`, `AsyncBatch::open`) |
| `Open` | `dlopen` failed; the message includes the preload-retry outcome and the first direct error |
| `Symbol` | The library lacks the standard LZ4 symbols — or, for `AsyncBatch`, lacks the async surface (a working synchronous ABI is not enough) |
| `Compress` | `LZ4_compress_default` returned a non-positive count, or bad arguments (block size 0, empty segment) |
| `Decompress` | `LZ4_decompress_safe` returned negative (corrupt input or undersized `dst`) |
| `Async` | Async-path failures: engine init failure, submit errors other than queue-full (with the library's status name, e.g. `HW_TIMEOUT_FAIL`), a completion status other than success, or `configure()` after the engine already initialized |

## Environment variables

| Variable | Read by | Meaning |
|---|---|---|
| `KAE_HW_LZ4_LIB` | `hw_lib_path()` | Overrides the patched-liblz4 probe path (must exist to win) |
| `KAE_LZ4_LEVEL` | async engine init | Compression level |
| `KAE_LZ4_ASYNC_THREAD_NUM` | async engine init | Library thread pool size |
| `KAE_LZ4_ASYNC_DC_THREAD_NUM` | libkaelz4 | Additional libkaelz4 tuning (not set by `configure`) |
| `KAE_NATIVE_TIMEOUT_MS` | `KaeNative::open` | Per-request engine completion wait before the block falls back to software (default 500) |
| `KAE_NATIVE_DEPTH` / `KAE_NATIVE_DB_BATCH` / `KAE_NATIVE_WAIT` / `KAE_NATIVE_ALG` | the bench worker | Native bench pump knobs (see [native-bench.md](native-bench.md)) |

## Integration shape (AgentENV zfile WorkerPool)

With `kae` as an optional dependency, the natural insertion points are:

- **Per block (sync):** in the worker loop's init, call `KaeLz4::open()`
  once; per 4K block call `compress(&src, &mut dst)` (reuse the `dst`
  buffer, sized via `bound`). Check `backend()` once to decide whether the
  hardware path is actually in effect.
- **Per block (zero-C):** the same loop shape over `KaeNative::open()` —
  one handle per worker thread (`Send`), `compress`/`decompress` per 4K
  block. The hardware/software split needs no policy code: `backend()`
  reports the mode, `hw_requests()`/`sw_requests()` report what actually
  happened, and read-side blocks decode with lz4rip in every mode.
- **Per segment (async):** in each worker thread, open one `AsyncBatch`,
  then per segment call `compress_segment(&segment, 4096, &mut packed)` and
  split `packed` by the returned lengths. Errors are explicit — the
  crate never silently degrades the async path to software.
