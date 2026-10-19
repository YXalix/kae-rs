# KAE hardware path: measured weaknesses and root causes

This document consolidates the case against the KAE hardware path as
shipped: what the bench numbers show, why the CPU accounting looks the way
it does, and the code-level reason the hardware path cannot currently
deliver CPU offload. Use it when deciding whether to place the hardware
path in a pipeline, and as the evidence base when "why not just use the
hardware?" comes up.

Everything here is reproducible with `kae-bench` (see
[benchmark.md](benchmark.md)) and verifiable in the two upstream trees:
Huawei's KAE user-space stack (`KAELz4`, `uadk` — the `libkaelz4.so.2` /
`libwd.so.2` pair this crate dlopens) and the openEuler kernel
(`drivers/misc/uacce`, `drivers/crypto/hisilicon`). Paths below are given
relative to those trees; paths like `src/...` refer to this crate.

## Verdict in one table

Numbers from full scans (64 MiB inputs: all-zero, mixed, and
incompressible random data; blocks 4K–1M; host with 2× SEC2 + 2× ZIP
uacce devices):

| Aspect (all-zero data, 1M blocks unless noted) | software | hw sync | hw async batch |
|---|---|---|---|
| compress throughput   | 15.75 GB/s | 2.91 GB/s | 3.97 GB/s |
| compress CPU per GB   | 0.06 s     | 0.34 s    | 3.17 s    |
| cores burned          | 0.94       | 0.99      | 12.58     |
| compress ratio        | 0.4%       | 1.6%      | 1.6%      |
| decompress throughput | 6.01 GB/s  | 16.01 GB/s| unsupported|

By the documented bench criteria (`src/orchestrator.rs:30` — speedup
> 1.1x is an outright win, cpu <= 50% of software counts as offload
value, large-block fallback needs >= 1.5x), every measured data profile
(all-zero, mixed, incompressible) prints **NO-GO** for the synchronous
path.

## What the numbers say

- **Compression loses on every axis.** Sync hardware reaches 0.11x–0.50x
  of software throughput across every data profile and block size, burns 2–8x
  more CPU per GB, and emits more bytes (see ratio below). The async batch
  beats sync hardware but still only reaches 0.25x–0.68x of software.
- **Decompression is the one win.** Hardware decompress of compressible
  data runs 2.7x faster than software (16 vs 6 GB/s on all-zero data, 8K blocks
  and up) at CPU parity. On random data both sides are memcpy-bound at
  ~20 GB/s. This is a real placement argument for compress-once /
  read-many pipelines — but note the async batch does not cover it (see
  below).
- **Small blocks collapse.** At 4K the sync path delivers ~1 GB/s at
  ~1 s/GB CPU (10x the software cost at that size). Per-request
  submit/poll overhead dominates the hardware service time.
- **The async batch trades CPU for throughput.** Over sync hardware it
  buys 2.7–3.0x at 4K but only ~1.35x at 64K and up — for 12–13 busy
  cores (see next section). Throughput plateaus at ~4 GB/s at 64K+
  regardless of compressibility (all-zero 3.97, mixed 3.76,
  incompressible 3.07), which points at the submit/complete path
  saturating before the engine does. The second SEC2 device is left idle.
- **Ratio regresses.** On all-zero data the hardware emits ~4x more
  bytes than software (1.6% vs 0.4% at 64K+; 2.3% at 4K). On mixed data
  it is 21.5% vs 20.5%. Immaterial in absolute terms, relevant when
  network or storage is the bottleneck.
- **Async decompression does not exist.** `hwasync` decompress rows
  report `UNSUPPORTED async_compress_only`: decompression inside the
  patched library is software regardless of implementation, so there is
  nothing to batch (`src/worker.rs:224`).

## The CPU accounting is not a measurement bug

CPU time is `getrusage(RUSAGE_SELF)` user+sys, taken as a delta around
the adaptive timing loop (`src/sysres.rs:67`, `src/sysres.rs:110`) — the
standard method, covering every thread in the worker process. The
counterintuitive part (hardware "offload" showing *higher* CPU per GB
than software) has a mechanical explanation:

**Every sync cell in the scan burns exactly one core.** Multiply each
row's GB/s by its cpu s/GB and the product is the average core count:

| cell                        | GB/s  | cpu s/GB | cores |
|---|---|---|---|
| all-zero 1M sw compress     | 15.75 | 0.06     | 0.94  |
| all-zero 1M hw compress     | 2.91  | 0.34     | 0.99  |
| mixed 4K hw compress        | 1.04  | 0.96     | 1.00  |
| incompressible 4K hw compress | 0.98 | 1.02    | 1.00  |
| all-zero 1M hw decompress   | 16.01 | 0.06     | 0.96  |
| all-zero 1M hwasync compress | 3.97 | 3.17     | 12.58 |

The law holds across all 75 measured cells (the 15 `hwasync` decompress
cells are unsupported): sw and hw sit at 0.90–1.08 cores,
hwasync at 12.58–13.21. Three consequences:

- For software, that one core *is* the compression. It is efficient, so
  the per-GB cost is low.
- For hardware, that same one core is host-side overhead: DMA setup and
  copies, request submission, and — dominant — **busy-polling the
  completion queue**. The core is not idle while "waiting"; it spins.
- Since cpu s/GB normalizes by data volume, a slower path with a pinned
  core mathematically shows a higher number. "Software should use more
  CPU" would only hold if hardware waits slept. They do not — which is
  precisely why the "cpu <= 50% of software" offload criterion fails.

## Root cause: completion is polled, never awaited

The sync call chain, each hop verified in the upstream trees:

1. Patched liblz4 sync entry → `kaelz4_compress()` dispatch
   (`KAELz4/src/kaelz4_adapter.c:110`) → `kaelz4_compress_v1` →
   `wcrypto_do_comp(ctx, op_data, NULL)` with `tag = NULL`, i.e. sync
   (`KAELz4/src/v1/kaelz4_comp.c:83`).
2. The sync wait loop is a pure user-space spin
   (`uadk/v1/wd_comp.c:250`):
   ```c
   do {
       ret = wd_recv(cctx->q, (void **)&resp);   /* reads the mmap'd CQE */
       ...
   } while (true);   /* MAX_RETRY_COUNTS = 200,000,000 — v1/wd_comp.c:28 */
   ```
   `wd_recv` peeks the completion ring mapped into user space; the
   steady state is zero syscalls. There is no sleep, no poll, no yield
   point anywhere in the loop.
3. **Blocking primitives exist but are not used.** The same v1 library
   ships `wd_recv_sync` / `wd_wait`, which `poll()` the uacce fd and
   sleep (`uadk/v1/wd.c:662`, `uadk/v1/wd.c:689`). `wcrypto_do_comp`
   calls `wd_recv` in the tight loop instead. The v2 API has the same
   knob behind the `WD_COMP_EPOLL_EN` environment variable
   (`uadk/wd_comp.c:154`, `uadk/wd_util.c:1930`) — default off, and the
   kaelz4 sync entry only dispatches to v1, so the knob is unreachable
   from this path.
4. **The async batch is spin with more threads.** The pool default is
   12 threads (`KAELZ4_TASK_THREAD_NUM`, `KAELz4/src/kaelz4_adapter.h:23`).
   A pool thread sleeps on `pthread_cond_timedwait` only while its task
   queue is empty (`KAELz4/src/kaelz4_adapter.c:258`); with work in
   flight it runs budgeted busy-polling
   (`kaelz4_async_compress_polling`, `kaelz4_adapter.c:273`).
   Completions are pulled by polling threads, never pushed by
   interrupts — "async" here means *someone else spins for you*. Twelve
   pool threads plus the submitter is exactly the measured ~13 cores.

## The kernel side already supports blocking

The sleep-until-IRQ model the CPU numbers assume is implemented end to
end in the kernel — it is simply never invoked by these user-space
paths:

- The uacce character device implements `poll`:
  `.poll = uacce_fops_poll` (`drivers/misc/uacce/uacce.c:681`,
  implementation at `:652`).
- Hardware completion raises an interrupt into the shared hisi-QM layer,
  which wakes pollers: `qm_qp_event_notifier()` → `uacce_wake_up()` →
  `wake_up_interruptible(&q->wait)`
  (`drivers/crypto/hisilicon/qm.c:2593`, `drivers/misc/uacce/uacce.c:158`).
- In user space, `wd_ctx_wait` (v2) and `wd_wait` (v1) are thin `poll()`
  wrappers over that fd (`uadk/wd.c:567`, `uadk/v1/wd.c:662`).

So the gap is a user-space policy choice, not a missing kernel feature.

## Why the stack is built this way

The polling contract is a deliberate trade, and stating it fairly
sharpens the criticism:

- **Per-request latency is microseconds.** From this crate's own scan,
  one 64K request occupies a single SEC2 queue for ~22 µs and a 4K
  request for ~3.5 µs. An interrupt → wake → reschedule → return-to-user
  round trip costs 5–20 µs and two context switches on its own — for 4K
  requests it would rival the service time. Polling a cache-hot ring
  entry is sub-microsecond.
- **The steady-state path is designed to be syscall-free**: SQE write,
  doorbell store, CQE read, all through mappings. A wait per request
  breaks that model.
- **The platform assumption is abundant cores.** The goal is to keep the
  accelerator's queue drained (a sleeping harvester lets the shared
  hardware queue back up and stalls other submitters — this crate's
  batch pool documents the same hazard at `src/batch.rs:32`), not to
  return CPU to the host. The bench's "cpu <= 50% = offload value"
  criterion encodes a goal this stack was never aimed at.

## The weakness list

1. **Compression is lose-lose-lose today**: lower throughput (0.1–0.5x),
   higher CPU (2–8x), worse ratio (up to 4x the bytes on compressible
   data) than plain software LZ4 at zfile granularity.
2. **No CPU-offload semantics.** The sync path spin-waits through the
   entire hardware service time; offloading the work costs exactly the
   core it was supposed to free.
3. **The async batch buys throughput with cores**: 12–13 busy cores for
   1.24–1.39x over sync hardware at 64K+ — and still 0.25–0.68x of
   software.
4. **Decompression is not accelerated** (software inside the patched
   library), and async decompression is unsupported by design — the 2.7x
   decompress win is sync-only.
5. **Small-block collapse**: ~1 GB/s and ~10x software CPU at 4K; per-op
   overhead dominates below 64K.
6. **The blocking wait is unreachable from the shipped sync path**: v1
   has no wait knob at all; v2's `WD_COMP_EPOLL_EN` defaults off and the
   kaelz4 sync entry dispatches only to v1.
7. **Single-queue ceiling**: sync plateaus at ~2.9 GB/s and the async
   pool at ~4 GB/s at 64K+ regardless of the data — the submit/complete
   path saturates first; the second SEC2 device sits idle in the default
   configuration.
8. **Silent software fallback** at hardware-API speed when the patched
   library loads without a uacce device node (see
   [architecture.md](architecture.md)) — throughput quietly degrades
   ~5x with no error.

## What would have to change

- **Wait side**: sleep on the uacce fd instead of spinning —
  `wd_recv_sync` (v1) or `wd_ctx_wait` + `WD_COMP_EPOLL_EN=1` (v2). The
  kernel IRQ machinery is already there. Sensible once requests are
  large enough that an occasional 10 µs wake is noise. (Demonstrated
  end to end by the native driver's `poll` mode: equal throughput at
  0.040 vs 0.160 cpu s/GB through uacce.)
- **Pool side**: bound the number of spinning harvesters
  (`KAE_LZ4_ASYNC_THREAD_NUM`), or make pool threads sleep when the
  hardware queue is drained mid-batch rather than spinning to the next
  task.
- **Request side**: amortize by batching many blocks into one hardware
  request (the direction this crate's async batch already takes); larger
  effective requests shrink the wait overhead to noise.
- **Capacity side**: spread queues across both SEC2 devices; the ~4 GB/s
  async plateau is a submit-path limit, not an engine limit.
- **Accounting side**: quantify the spin with `ru_nvcsw`/`ru_nivcsw` —
  `switch_counts()` (`src/sysres.rs:81`); the native rows already print
  `nvcsw`/`nivcsw` in their notes, and the spin-vs-poll check reads as
  expected: 0 voluntary switches spinning, 64839 sleeping, at equal
  throughput. This is the one-number check that a wait-side change
  actually worked.

## The engine is not the ceiling

The native uacce path (`--impl native`) removes the C stack and drives
the ZIP engine directly. Its `perf_mode=1` sweep (2026-10-19, `rev
0b3b7ab`, same box, mixed data, 64K blocks unless noted) re-opens the
case the tables above close:

| path | GB/s | cpu s/GB |
|---|---|---|
| sw (system liblz4, one thread) | 8.41 | 0.12 |
| hw sync (patched liblz4) | 1.74 | 0.58 |
| hw async batch (12-thread library pool) | 4.02 | 3.08 |
| native, 1 queue (lz77 view) | 6.40 | 0.16 spin / 0.04 poll |
| native, 2 queues | 12.79 | one pump thread |
| native, 2 queues, 8K requests | 23.20 | 0.04 |

One queue at depth ≥4 sustains 6.40 GB/s — 3.7× the sync path and 1.6×
the async batch of the shipped stack on the same box — so the ~4 GB/s
plateau is the userspace stack, not the engine. Sleeping on the uacce fd
is measured, not proposed: equal throughput at a quarter of the pump
CPU. Adding the measured rebuild cost (`rebuild_s_per_gb` 0.027–0.038,
the triple→LZ4 conversion an end-to-end native pipeline performs) to the
engine time estimates ~9.4 GB/s at 2 queues × 64K and ~13.5 GB/s at
2 queues × 8K — above software on the same box — with pump+rebuild CPU
(~0.07 s/GB under `poll`) below software's 0.11–0.17.

So the NO-GO verdict for the *shipped* path stands, but the conclusion
it feeds has changed: the engine justifies a native backend; the silicon
was never the problem. The decode side, the other native gap, is closed
by adoption rather than invention: the crate's own decoder managed
1.4 GB/s where the C libraries decompress at 5.5–10.7 GB/s, and the
pure-Rust lz4rip engine decodes the same corpus faster than the C
libraries on an aarch64 dev box (blocks produced by `LZ4_compress_default`
verified block-by-block). The native decompress cell and every
`KaeNative` read path now decode through lz4rip (`src/soft.rs`); the
authoritative on-target numbers are one `--impl native,nativesw,sw`
run away. Full readings: [native-sweep.md](native-sweep.md).

## Reproducing

```bash
./target/release/kae-bench                  # full scan (~2 min)

# cores per cell = GB/s x cpu s/GB  (≈1.0 for sw/hw, ≈13 for hwasync)
./target/release/kae-bench --tsv /tmp/b.tsv --json /tmp/b.json

# watch the spin live during a hw compress cell
top -H -p <worker pid>                      # one thread at ~100%, ~3 GB/s

# async pool size (default 12) — the 13-core line item
KAE_LZ4_ASYNC_THREAD_NUM=4 ./target/release/kae-bench --impl hwasync

# v2 blocking knob — exists, but unreachable from the v1 sync path
WD_COMP_EPOLL_EN=1 ./target/release/kae-bench
```
