# The native engine path: the ceiling probe and its LZ4 view

The `native` bench implementation drives the KAE2 ZIP engine through the
crate's own uacce module (`src/uacce.rs`) — open, ioctl, mmap, SQE fill,
doorbell store, CQE harvest — with no libwd, no libkaelz4 and no patched
liblz4 in the process. It is the first brick of the native hardware
backend this crate is growing toward (the end goal being a complete
replacement of the C KAE userspace stack).

## The two views (the project's shared vocabulary)

Every hardware number in this project is read through one of two views,
and they answer different questions:

- **The LZ4 view — the kae-vs-sw comparison.** The engine parses input
  into lz77 triples and the worker rebuilds those triples into standard
  LZ4 blocks (`src/rebuild.rs`), the exact format the `sw`, `hw` and
  `hwasync` cells produce. LZ4 is the comparison currency: when judging
  kae against software, compare throughput (GB/s) and CPU cost
  (cpu s/GB) on LZ4 — that is the adoption verdict. A native row's
  `ratio_pct` is the rebuilt blocks' true LZ4 ratio, and its sample is
  decoded by the system liblz4 like every other implementation's.
- **The lz77 view — the hardware ceiling only.** The engine's own triple
  output is not an LZ4 stream and is not comparable to any software
  implementation. It exists to answer the one question the
  libkaelz4-backed cells cannot: **how fast is the hardware when the C
  userspace stack is removed from the path?**

One honesty rule keeps the views apart: the timed loop times the engine's
lz77 parse alone (the ceiling instrument — the rebuild is a software step
run once, untimed, to produce the verify sample). A native row's GB/s is
therefore engine throughput, not an end-to-end LZ4 pipeline; use its LZ4
ratio and CPU alongside the sw/hw rows' throughput when weighing kae vs
sw, and treat the GB/s itself as the hardware ceiling.

The measured facts it builds on (see [kae-limitations.md](kae-limitations.md)):
the kaelz4 sync path is one request in flight per queue plus a 100%-core
spin (2.91 GB/s at 64K blocks), and the async pool plateaus at ~4 GB/s
across 12 spinning threads. If the native path with `k` in-flight requests
per queue exceeds both by a wide margin, the plateau is a userspace-stack
artifact and a thin native backend is worth finishing; if it does not,
the engine itself is the limit and the placement verdict is final.
**The `perf_mode=1` sweep of 2026-10-19 answered it — the first branch
holds.** One queue sustains 6.40 GB/s from depth 4, two queues 12.79 GB/s
at 64K requests and 23.20 GB/s at 8K requests, against the same run's
kaelz4 cells at 1.74 (sync) and 4.02 GB/s (async batch): the plateau is a
userspace-stack artifact, and the native backend is worth finishing (full
readings: [native-sweep.md](native-sweep.md)).

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
- **Standard LZ4 output via the triple rebuild.** After the timed run the
  worker rebuilds the first sample blocks' triples into standard LZ4
  format (`src/rebuild.rs`, same arithmetic as the kaelz4 rebuild) and
  writes the packed sample file. A block spanning several hardware
  requests rebuilds from its requests' triples in ONE walk, charging
  each request's unconsumed tail into the next match's literal length —
  a finished LZ4 block is not composable (its closing literal run is a
  parser-level end marker), so streams are never spliced. The
  orchestrator's verify op decodes it with the system liblz4 exactly like
  the library-backed cells, and the row's `ratio_pct` is the rebuilt
  blocks' true LZ4 ratio (the pre-rebuild triples ratio lands in the
  `note` as `pre_ratio`, next to `rebuild_s_per_gb` — the software
  triple-to-LZ4 conversion cost, the number an end-to-end native LZ4
  pipeline would add to the engine's parse time). Where no
  sample is possible — the `deflate` dialect, a block that is not a
  request-size multiple, a manual worker run without `--sample-out` — the
  ratio falls back to the pre-rebuild engine output and the `note` names
  it. The same applies when a sample rebuild **fails**: the timed engine
  numbers stand, the row stays `OK`, and the note records
  `sample=failed:<cause>` (the row's err= and the worker's stderr also
  surface in the summary/TSV note). A failed block's check runs the
  triple audit on EVERY request it spans (`rebuild::audit_triples`: each
  match's bytes must equal the bytes at `position - offset`, and the
  engine's literal area must equal the walk's literal spans) — the
  request holding the first decode diff leads the note, and every
  request carries its own verdict: a violated rule names that request's
  engine output inconsistent, while a `triples consistent` verdict on
  the failing range points at the boundary assembly. A literal-area
  fault never explains a decode diff (the rebuild reads its literals
  from the input), which is exactly why the audit must not stop at the
  first inconsistent request. The full evidence (every request's
  triples and literal areas, the rebuilt block, the input) lands in
  `native-fail-<block>.txt` next to the run; the console keeps the
  verdicts and the failing request's triples. The
  decompress cell depends on the rebuild and fails its cell instead —
  the decode has nothing to decode.

## Running

```bash
cargo build --release
./target/release/kae-bench --impl native                     # full scan cells
./target/release/kae-bench --impl native --blocks 4096,65536,1048576
```

Needs a machine with `/dev/hisi_zip-*` uacce nodes; without one the
native cells are skipped with a printed reason. Compiles and tests
everywhere (the ABI logic is unit-tested against fabricated rings, no
hardware needed). Only QM v3 devices are supported (api `hisi_qm_v3`, the
KAE2/SEC2 generation); anything else is refused at open with a clear
error.

Knobs (environment variables read by the native worker):

| var | default | meaning |
|---|---|---|
| `KAE_NATIVE_DEPTH` | 8 | in-flight requests per queue (max 1024, the engine's queue depth; throughput saturates at 4) |
| `KAE_NATIVE_DB_BATCH` | = depth | SQEs per doorbell store (1 reproduces the per-request ring; measured flat at 64K — both 6.40 GB/s) |
| `KAE_NATIVE_WAIT` | `spin` | `spin` harvests the CQ in a tight loop; `poll` sleeps on the uacce fd (the IRQ-arm path) — measured: equal throughput at 0.040 vs 0.160 cpu s/GB |
| `KAE_NATIVE_ALG` | `lz77` | `deflate` runs the deflate dialect instead — the plumbing-check algorithm, without the 128K input cap |

`--workers N` selects the number of hardware queues, round-robin over all
`/dev/hisi_zip-*` devices. The `note` field of every native row records
the exact configuration
(`alg,queues,depth,db_batch,wait,req_size,nvcsw,nivcsw`).

## Suggested sweep

The concrete on-target runbook — per-experiment commands, one TSV per
run, and the send-back checklist — is [native-sweep.md](native-sweep.md).

```bash
# plumbing check first: deflate dialect, shallow, short
KAE_NATIVE_ALG=deflate KAE_NATIVE_DEPTH=8 ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2

# the depth ladder: where does throughput stop responding to in-flight depth?
for k in 1 4 16 64 256 1024; do KAE_NATIVE_DEPTH=$k ./target/release/kae-bench --impl native --blocks 65536 --tsv /dev/null | grep 'native compress'; done

# queue scaling across both ZIP devices (--workers = queues)
./target/release/kae-bench --impl native --workers 2 --blocks 65536

# wait-policy comparison: spin CPU vs poll() CPU at the same throughput
KAE_NATIVE_WAIT=spin ./target/release/kae-bench --impl native --blocks 65536
KAE_NATIVE_WAIT=poll ./target/release/kae-bench --impl native --blocks 65536

# doorbell cost: per-request ring vs batched
KAE_NATIVE_DB_BATCH=1 ./target/release/kae-bench --impl native --blocks 65536
# (default batches a full depth per doorbell)

# small-block economics (min-sec 2: the default 0.8s windows read
# boost clocks — the same input read 9.6 GB/s at 0.8s and 6.4 GB/s
# sustained at 2s in the first sweep)
./target/release/kae-bench --impl native --blocks 4096,16384,65536,131072 --min-sec 2
```

Repeat the sweep after loading the driver with `perf_mode=1` (see
[kae-limitations.md](kae-limitations.md)): the default `perf_mode=0` is
the high-compression-ratio, low-throughput hardware mode, and every
number taken under it is a floor.

## Reading the results

Resolved for `perf_mode=1` on 2026-10-19 — engine faster than the C
stack, `poll` delivers the wait-side win (see
[native-sweep.md](native-sweep.md)). The rules below stand for any
re-run:

- **Depth ladder saturates well above 4 GB/s** → the engine is faster
  than the C stack; the kaelz4 async plateau is submit/harvest overhead,
  and finishing the native backend is worth it.
- **Depth ladder saturates at or below 4 GB/s** → the engine (or its
  current `perf_mode`/firmware) is the ceiling; no userspace rewrite
  changes the placement verdict.
- **`KAE_NATIVE_DB_BATCH=1` vs the default divergence** → quantifies the
  doorbell/MMIO tax the C stack pays on every single request.
- **`KAE_NATIVE_WAIT=poll` at a fraction of the spin CPU with equal
  throughput** → the sleep-until-IRQ design works end to end through
  uacce; that is the waiting policy the C stack should have shipped.
- **`nvcsw`/`nivcsw` in the note**: spin runs show ~zero voluntary
  switches, poll runs many — the same one-number check proposed for any
  wait-side rewrite.

## Scope and caveats

- Stateless flat requests only, window fixed at 16K for lz77 (kaelz4's
  defaults). Compress times the engine's lz77 parse; decompress times
  lz4rip decoding (`src/soft.rs`) of the rebuilt standard LZ4 blocks —
  the decode side is software in every implementation (KAE accelerates
  compression only; the patched library decompresses in software too),
  and the native path keeps a C library out of the process entirely. The
  decompress cell needs the lz77 dialect (deflate output is not LZ4) and
  blocks that are request-size multiples. The `soft` and `nativesw` cells
  run the same lz4rip decode on the other side of the fence: `soft`
  decodes lz4rip's own compression, `nativesw` decodes the production
  handle's engine-rebuilt blocks (or its own, in software mode).
- The decompress setup rebuilds **every** block from live hardware
  triples (one request per `req_size` slice, untimed, one walk per
  block) and bit-checks each block's rebuilt bytes against its original
  before the timed decode loop — a full-pipeline self-test per run. The
  first failing block dumps its first inconsistent request's triples,
  the rebuilt bytes, the engine's literals area and the input to the
  worker's stderr and `native-fail-<request>.txt`, with the audit
  verdict in both. The setup is one request in flight
  on one queue; `KAE_NATIVE_DEPTH`/`KAE_NATIVE_DB_BATCH`/`KAE_NATIVE_WAIT`
  still shape that queue.
- The SS region needs ~`depth × 4 × req_size` bytes per queue; at the
  default depth 8 and 64K requests that is ~2 MiB. Forcing
  `KAE_NATIVE_DEPTH` to 1024 at 64K requests asks for ~260 MiB of
  DMA-coherent memory per queue and fails in the SS mapping — lower the
  depth instead.
- The rebuild is unit-tested against the system liblz4
  (`rebuild_decodes_with_system_liblz4`) and against hardware by the
  `#[ignore]`d tests `hw_lz77_sample_rebuilds_to_decodable_lz4` and
  `hw_lz77_page_boundary_rebuilds_to_decodable_lz4` (a request spanning a
  random -> text page pair of the mixed layout, the shape that exercises
  literal-only triples ahead of repeat codes; `cargo test -- --ignored`
  on the target machine); the bench verify op re-checks every run's
  sample.
- `src/uacce.rs` is the module of record for the ABI: every constant is
  copied, not guessed, with its source called out next to the definition
  (uacce UAPI, `hisi_qm.h` ioctls, `uadk/v1/drv/hisi_qm_udrv.{c,h}` for
  doorbell/CQE, `uadk/v1/drv/hisi_zip_udrv.{c,h}` for the v3 SQE layout).
