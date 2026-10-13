# The raw uacce engine path: measuring the ceiling behind the C stack

The `raw` bench implementation drives the KAE2 ZIP engine through the
crate's own uacce module (`src/uacce.rs`) — open, ioctl, mmap, SQE fill,
doorbell store, CQE harvest — with no libwd, no libkaelz4 and no patched
liblz4 in the process. It is the first brick of the native hardware
backend this crate is growing toward (the end goal being a complete
replacement of the C KAE userspace stack), and it exists to answer one
question the libkaelz4-backed cells cannot: **how fast is the hardware
when the C userspace stack is removed from the path?**

The measured facts it builds on (see [kae-limitations.md](kae-limitations.md)):
the kaelz4 sync path is one request in flight per queue plus a 100%-core
spin (2.91 GB/s at 64K blocks), and the async pool plateaus at ~4 GB/s
across 12 spinning threads. If the raw path with `k` in-flight requests
per queue exceeds both by a wide margin, the plateau is a userspace-stack
artifact and a thin native backend is worth finishing; if it does not,
the engine itself is the limit and the placement verdict is final.

## What is different from the kaelz4 cells

- **Zero payload copies in the timed loop.** The workload is placed once
  into the kernel-allocated SS share-dma region
  (`UACCE_CMD_GET_SS_DMA`); every request then DMAs from and to that
  memory. The kaelz4 path by contrast bounces every 64K block through a
  staging pool twice.
- **Depth, batching and wait policy are knobs.** In-flight requests per
  queue, SQEs per doorbell, and spin vs `poll()`-the-fd waiting map the
  design space the C stack never exposed (it is fixed at "one in flight,
  spin" for sync and "twelve spinners" for async).
- **One thread drives all queues.** The pump is single-threaded; its CPU
  (user+sys, plus `nvcsw`/`nivcsw` in the `note`) is the honest cost of a
  well-formed submit/harvest loop.
- **No cross-validation sample.** The engine's lz77 output is literals +
  8-byte sequence triples, not a decodable LZ4 stream; rows report the
  pre-rebuild size as `ratio_pct` and skip the `VERIFY` step. The
  plumbing is validated on hardware by the `#[ignore]`d unit tests
  (`cargo test -- --ignored`, engine requests must complete with status 0).

## Running

```bash
cargo build --release
./target/release/kae-bench --impl raw                     # full scan cells
./target/release/kae-bench --impl raw --data mixed --blocks 4096,65536,1048576
```

Needs a machine with `/dev/hisi_zip-*` uacce nodes; without one the raw
cells are skipped with a printed reason. Compiles and tests everywhere
(the ABI logic is unit-tested against fabricated rings, no hardware
needed). Only QM v3 devices are supported (api `hisi_qm_v3`, the
KAE2/SEC2 generation); anything else is refused at open with a clear
error.

Knobs (environment variables read by the raw worker):

| var | default | meaning |
|---|---|---|
| `KAE_RAW_DEPTH` | 64 | in-flight requests per queue (max 1024, the engine's queue depth) |
| `KAE_RAW_DB_BATCH` | = depth | SQEs per doorbell store (1 reproduces the per-request ring) |
| `KAE_RAW_WAIT` | `spin` | `spin` harvests the CQ in a tight loop; `poll` sleeps on the uacce fd (the IRQ-arm path) |
| `KAE_RAW_ALG` | `lz77` | `deflate` runs the deflate dialect instead — the plumbing-check algorithm, without the 128K input cap |

`--workers N` selects the number of hardware queues, round-robin over all
`/dev/hisi_zip-*` devices. The `note` field of every raw row records the
exact configuration (`alg,queues,depth,db_batch,wait,req_size,nvcsw,nivcsw`).

## Suggested sweep

```bash
# plumbing check first: deflate dialect, shallow, short
KAE_RAW_ALG=deflate KAE_RAW_DEPTH=8 ./target/release/kae-bench --impl raw --blocks 65536 --data zeros --min-sec 2

# the depth ladder: where does throughput stop responding to in-flight depth?
for k in 1 4 16 64 256 1024; do KAE_RAW_DEPTH=$k ./target/release/kae-bench --impl raw --blocks 65536 --data zeros --tsv /dev/null | grep 'raw zeros'; done

# queue scaling across both ZIP devices (--workers = queues)
./target/release/kae-bench --impl raw --workers 2 --blocks 65536 --data zeros

# wait-policy comparison: spin CPU vs poll() CPU at the same throughput
KAE_RAW_WAIT=spin ./target/release/kae-bench --impl raw --blocks 65536 --data zeros
KAE_RAW_WAIT=poll ./target/release/kae-bench --impl raw --blocks 65536 --data zeros

# doorbell cost: per-request ring vs batched
KAE_RAW_DB_BATCH=1 ./target/release/kae-bench --impl raw --blocks 65536 --data zeros
# (default batches a full depth per doorbell)

# small-block economics
./target/release/kae-bench --impl raw --blocks 4096,16384,65536,131072 --data mixed
```

Repeat the sweep after loading the driver with `perf_mode=1` (see
[kae-limitations.md](kae-limitations.md)): the default `perf_mode=0` is
the high-compression-ratio, low-throughput hardware mode, and every
number taken under it is a floor.

## Reading the results

- **Depth ladder saturates well above 4 GB/s** → the engine is faster
  than the C stack; the kaelz4 async plateau is submit/harvest overhead,
  and finishing the native backend is worth it.
- **Depth ladder saturates at or below 4 GB/s** → the engine (or its
  current `perf_mode`/firmware) is the ceiling; no userspace rewrite
  changes the placement verdict.
- **`KAE_RAW_DB_BATCH=1` vs the default divergence** → quantifies the
  doorbell/MMIO tax the C stack pays on every single request.
- **`KAE_RAW_WAIT=poll` at a fraction of the spin CPU with equal
  throughput** → the sleep-until-IRQ design works end to end through
  uacce; that is the waiting policy the C stack should have shipped.
- **`nvcsw`/`nivcsw` in the note**: spin runs show ~zero voluntary
  switches, poll runs many — the same one-number check proposed for any
  wait-side rewrite.

## Scope and caveats

- Stateless flat requests only, window fixed at 16K for lz77 (kaelz4's
  defaults); no decompression, no SGL, no checksum, no stream mode.
- The SS region needs ~`depth × 4 × req_size` bytes per queue; at depth
  1024 and 64K requests that is ~260 MiB of DMA-coherent memory per
  queue — lower `KAE_RAW_DEPTH` if allocation fails.
- The engine's compression ratio at these settings matches the kaelz4
  path (same engine mode); ratio is not this tool's metric.
- `src/uacce.rs` is the module of record for the ABI: every constant is
  copied, not guessed, with its source called out next to the definition
  (uacce UAPI, `hisi_qm.h` ioctls, `uadk/v1/drv/hisi_qm_udrv.{c,h}` for
  doorbell/CQE, `uadk/v1/drv/hisi_zip_udrv.{c,h}` for the v3 SQE layout).
