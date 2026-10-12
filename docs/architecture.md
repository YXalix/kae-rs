# Architecture

How `kae` is put together: the library chain it drives, the runtime loading
model that makes it a zero-build-dependency drop-in, and a map of the modules.

## The acceleration chain

```
kae crate (Rust)
  │  dlopen at runtime, standard LZ4 block ABI
  ▼
/usr/local/kaelz4/lib/liblz4.so.1.9.4   (KAE-patched liblz4, SONAME liblz4.so.1)
  │  forwards compression internally
  ▼
libkaelz4.so.2.2.0
  ▼
libwd (libwd_comp)                      (Huawei's warpdrive user-space driver)
  ▼
/dev/uacce* ZIP engine                  (Kunpeng KAE2 hardware accelerator)
```

Key properties:

- **Standard LZ4 ABI.** The patched library exports the ordinary
  `LZ4_compress_default` / `LZ4_decompress_safe` / `LZ4_compressBound`
  symbols and produces ordinary LZ4 block-format output. Anything that can
  decode LZ4 blocks (including the stock community liblz4) can decode its
  output — the bench cross-validates this explicitly.
- **Compression only.** The KAE path accelerates compression; decompression
  stays software inside the patched library. This crate wraps decompression
  too, so a consumer gets a complete drop-in from one handle.
- **Zero build-time dependencies.** Nothing is linked at build time and no
  KAE headers are needed; `cargo build` works on any Linux box. The hardware
  surface appears only at runtime, through `dlopen`.
- **`Backend::Hw` means "the patched library was loaded"**, no more. Without
  `/dev/uacce*` devices the patched library silently falls back to software
  internally, at hardware-API speed. Use `kae::hw_available()` (patched
  library present AND a `/dev/uacce*` device exists) when the distinction
  matters.

## Runtime loading model

`loader::Lib` is a minimal hand-written `dlopen`/`dlsym` wrapper
(`glibc >= 2.34` includes `dlopen` in libc, so there is no `-ldl` either).

Three deliberate design points:

1. **No `Drop` / `dlclose`, ever.** Function pointers obtained from a
   `Lib` must stay valid for the process lifetime. Closing the handle would
   dangle every pointer handed out, so `Lib` simply never closes; bench
   processes are short-lived and the leak is inconsequential.
2. **Direct open first, preload retry second.** `open_lib()` tries a plain
   `dlopen` of the target library. If that fails (typically a missing
   rpath in the patched library's dependency chain), it preloads the
   dependency chain `libwd → libwd_comp → libkaelz4` with `RTLD_GLOBAL` —
   only the entries that exist on disk — and retries once. Later `dlopen`s
   resolve `DT_NEEDED` entries by SONAME against already-loaded objects,
   which works around the missing rpaths. The diagnostics end up in
   `KaeLz4::note()` (non-empty = the retry path was taken).
3. **Same SONAME as the system library.** The patched library and the system
   liblz4 both claim `liblz4.so.1`. Inside one process, whichever loads
   first wins for every subsequent `dlopen` of that SONAME. This is why the
   bench runs every (implementation × operation) cell in a fresh subprocess;
   in-process switching is simply not reliable. It is also why the operational
   rules forbid exposing `/usr/local/kae*` through the global loader path —
   see [operations.md](operations.md).

## Module map

| Module | Role |
|---|---|
| `lib.rs` | Public handle `KaeLz4` (open/probe/compress/decompress), `Backend`, `KaeError`, `hw_available()`, `hw_lib_path()`, probe-path and preload-path constants, the shared `open_lib()` preload-retry helper |
| `loader.rs` | `dlopen` wrapper (`Lib`): open with `RTLD_NOW`/`RTLD_GLOBAL`, symbol lookup, dependency preloading |
| `lz4.rs` | `Lz4Fns`: the three standard block-ABI function pointers plus `PROBE_SYMBOLS` (standard ABI + KAE async surface) used by `kae-bench --probe` |
| `batch.rs` | `AsyncBatch`: segment-level async compression over `LZ4_compress_async` (see below) |
| `sysres.rs` | `getrusage`/`clock_gettime` FFI and the adaptive `timed_loop` the bench is built on |
| `data.rs` | Deterministic synthetic datasets (zeros / mixed / random, 4K-page interleaved, fixed-seed xorshift64*) |
| `worker.rs` | `--_worker` subprocess mode: opens exactly one liblz4, runs one cell, prints `RESULT`/`VERIFY`/`PROBE` line-protocol records; captures cross-validation samples |
| `orchestrator.rs` | Bench CLI: environment report → data generation → one subprocess per cell → summary table → verdict; TSV/JSON writers |

The `benches/throughput.rs` entry point calls `orchestrator::main()` directly,
so `cargo bench -- <args>` and the `kae-bench` binary are the same tool.
Visibility follows the usage: only `lib.rs`'s root items (`KaeLz4`,
`AsyncBatch`, `Backend`, `KaeError`, `hw_available()`, `hw_lib_path()`, the
path constants) are public library API; every module above except
`orchestrator` (published for the bin/bench targets to call `main()`) is
`pub(crate)`.

## The synchronous handle (`KaeLz4`)

Plain data: a backend label, the library path, a diagnostic note, and the
three resolved function pointers. The block API has no interior mutable
state, so a `KaeLz4` can be opened once and shared by reference from any
number of threads (`compress`/`decompress` take `&self`, and the C functions
they call are reentrant).

## The async batch path (`AsyncBatch`)

`batch.rs` binds the patched library's `LZ4_compress_async` (which forwards
to libkaelz4's `KAELZ4_compress_async`): a whole segment's blocks are
described as buffer lists and submitted to the KAE task queue in one call;
the library compresses them in parallel (hardware and its own thread pool)
and fires one callback per block; the submitting thread returns once every
callback has landed.

Binding decisions worth knowing when touching this code:

- **Slot layout.** One `Slot` per block holds the C-side structs
  (`KaeBufferList` → `KaeBuffer`, and `KaeResult` first). `KaeResult` sits
  at offset 0 so the callback can recover the owning `Slot` directly from
  the `result` pointer it is handed; `user_data` is not relied on. The
  structs are self-referential (`list.buf` points into the same slot) and
  are never moved while requests are in flight.
- **Trigger-agnostic completion counting.** Callbacks fire from library
  threads, but small blocks may take the library's software fallback and
  then the callback fires synchronously, inline on the submitting thread.
  Completion is therefore an `AtomicUsize` plus a `Mutex`/`Condvar`, correct
  under both trigger styles.
- **Queue-full backoff.** A submit returning `KAE_LZ4_TASK_QUEUE_FULL` (9)
  is retried after one in-flight request completes. A full queue with
  nothing in flight from this batch means the queue is hogged externally —
  that is an error, not an infinite wait (a bounded backoff caps it).
- **Drain before any error return.** Before `compress_segment` returns an
  error, it waits for every already-submitted request to call back, so no
  callback can touch freed stack memory (the completion state lives on the
  stack of the call).
- **`Send`, not `Sync`.** Scratch and slot storage is reused across calls,
  so the API takes `&mut self`: move one `AsyncBatch` to the worker thread
  that owns it, one segment at a time.
- **One-shot process init, never torn down.** The first open runs
  `LZ4_async_compress_init` (preferred: it registers the library's software
  fallbacks) or, if absent, `KAELZ4_async_compress_init` with the standard
  software symbols registered explicitly. The result is cached process-wide.
  `configure(threads, level)` is refused once init has started. The global
  teardown function is deliberately never called — it would destroy the
  shared KAE thread pool for every other user in the process; process exit
  reclaims everything.
- **Trailing partial block.** A segment tail below the block granularity is
  compressed synchronously through the block API and appended in order, so
  segments need not be block multiples. An empty segment is an error.

## Test layout

- `tests/roundtrip.rs` — end-to-end: open → compress → decompress, bit-exact,
  on both compressible and incompressible blocks; runs anywhere (falls back
  to the software backend on machines without KAE).
- `tests/async_gating.rs` — failure-mode gates: the stock liblz4 must be
  rejected with a `Symbol` error (never a silent software fallback), a
  missing patched library must yield `Unavailable`, and `configure()` sets
  its tuning environment variables.
- `src/batch.rs` unit test — the trailing-block path round-trips through the
  block ABI against the system liblz4.
