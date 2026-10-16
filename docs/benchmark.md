# Benchmark guide (kae-bench)

`kae-bench` measures the KAE-patched liblz4 against the system liblz4 at
zfile granularity (LZ4 block format, per-block `LZ4_compress_default`),
plus the segment-level async batch and the `--workers`-threaded pool form
of the sw/hw cells (the AgentENV zfile WorkerPool shape).
The same binary is the `cargo bench` entry
point — `cargo bench -- <args>` behaves identically. For what the results
mean about the hardware path — weaknesses and their root causes — see
[kae-limitations.md](kae-limitations.md).

## Running

```bash
cargo build --release
./target/release/kae-bench --probe     # environment & symbol probe, then exit
./target/release/kae-bench             # full scan: sw / hw / hwasync / native (~2 min)
./target/release/kae-bench --sweep     # the whole native experiment series into ./kae-sweep.tsv
```

Common invocations:

```bash
# a real workload instead of synthetic data (e.g. an exported VM memory image)
./target/release/kae-bench --input /path/to/memory.img

# shrink the matrix
./target/release/kae-bench --blocks 4096,65536,1048576 --ops compress
./target/release/kae-bench --impl sw,hw

# the production-shape comparison: hardware vs an 8-thread software pool
./target/release/kae-bench --impl sw,hw,hwasync --workers 8

# machine-readable output
./target/release/kae-bench --tsv results.tsv --json results.json
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `--input FILE` | — | Benchmark a real file instead of synthetic data (dataset label becomes `input`) |
| `--size MIB` | 64 | Synthetic dataset size |
| `--blocks LIST` | `4096,8192,65536,262144,1048576` | Block sizes to scan |
| `--impl LIST` | `sw,hw,hwasync,native` | `sw`, `hw`, `hwasync` (compress only), `native` (the crate's own uacce engine path; its decompress cell decodes in software, in-crate — see [native-bench.md](native-bench.md)) |
| `--workers N` | 1 | Threads per sw/hw cell: 1 is the sync drop-in, >1 the threaded pool form (see below); for `hwasync`, values >1 set the library's internal async thread pool (`KAE_LZ4_ASYNC_THREAD_NUM`); for `native`, N is the number of hardware queues |
| `--ops LIST` | `compress,decompress` | Operations to run |
| `--sw-lz4-lib P` | `liblz4.so.1` | Software library path |
| `--hw-lz4-lib P` | probe | Patched library path |
| `--min-sec F` | 0.8 | Minimum timed seconds per cell |
| `--passes-min N` / `--passes-max N` | 3 / 99 | Pass bounds per cell (the max caps a pathologically slow implementation) |
| `--tsv PATH` | `./kae_bench_results.tsv` | TSV output (empty string disables) |
| `--json PATH` | off | Additional JSON output |
| `--sweep [FILE]` | off | Run the whole native experiment series once into ONE file (`kae-sweep.tsv`): machine context + baseline + depth ladder + queues + wait policy + doorbell + small blocks + readings; see [native-sweep.md](native-sweep.md) |
| `--probe` | off | Environment + symbol probe only |
| `-h, --help` | | Help |

Flags that libtest appends under `cargo bench` (`--bench`, `--nocapture`,
…) are accepted and ignored. Invalid list values are rejected up front with
an error: `--blocks` must parse to block sizes, `--impl` must be
`sw`/`hw`/`hwasync`/`native`, `--ops` must be `compress`/`decompress`, and
`--workers` must be 1–256.

Cells are skipped (with a printed reason) when their backend is absent:
`hw`/`hwasync` need the patched library, `native` needs a
`/dev/hisi_zip-*` uacce device node.

## What it does, in order

1. **Environment report** — host, kernel, CPU model, the uacce device
   inventory (`/dev/hisi_zip-*` and siblings), the software and hardware
   library paths, and the preload chain. Warns when no uacce device is
   present (the patched library then compresses in software internally, and
   "hw" rows may be software in fact).
2. **Data generation** — writes the synthetic dataset (or stages the
   `--input` file). The dataset is deterministic: a fixed-seed xorshift64*,
   4K-page-interleaved layout of 50% zero + 20% random + 30% text pages,
   approximating a VM memory snapshot's compressibility profile, so numbers
   are comparable across machines and runs. Best/worst-case data
   (all-zero, incompressible) comes from `--input` with a file of your
   choosing.
3. **Scan** — for every (block size × implementation × operation) cell,
   re-executes itself as a worker subprocess with exactly one library
   open, parses its `RESULT` line, and — for compress cells with `OK`
   status — runs a second subprocess that decodes the captured sample with
   the **system** liblz4 and compares block-by-block (`VERIFY`). The
   `native` cells produce the sample by rebuilding their lz77 triples into
   standard LZ4 blocks inside the worker, so the same verify op applies
   (see [native-bench.md](native-bench.md)).
4. **Summary** — a results table, a verdict, and the TSV/JSON files.

### Why subprocess isolation

The patched library and the system liblz4 share the SONAME `liblz4.so.1`.
Once one of them is loaded in a process, every later `dlopen` of that
SONAME silently resolves to it — software and hardware numbers would
pollute each other inside a single process. One subprocess per cell is the
only reliable isolation, and its cost is negligible at second-scale timing.

### Pool mode (--workers >1)

`--workers N` with N > 1 turns every sw/hw cell into the N-threaded block
pool — the shape of the AgentENV zfile WorkerPool at block granularity.
Each worker thread compresses (or decompresses) its interleaved share of
the file's blocks per pass (`block % workers == thread`) through the block
API, into its own dst buffer. The library choice is unchanged (`sw` opens
the system liblz4, `hw` the patched library plus the preload chain); since
LZ4 blocks are independent, the pool produces the same on-disk format and
the same per-block output sizes. `--workers 1` (the default) keeps the
sync drop-in benches.

Accounting stays comparable with the sync rows: warm-up (ratio + sample
capture) is single-threaded and untimed; the timed loop counts the blocks
actually processed; wall clock spans pool start to pool join; CPU is the
process-wide getrusage delta, which covers all pool threads. The pass
bounds keep their sync meaning (full passes over the file) — internally
they scale with the pool size — and the `note` field of pool rows records
the pool size (`workers=N`). For `hwasync`, `--workers` >1 instead
maps onto the library's internal async thread pool
(`KAE_LZ4_ASYNC_THREAD_NUM`, set via `AsyncBatch::configure` before the
open; 1 leaves the library default or any externally set env var alone).

## Metrics and how to read them

- **`GB/s`** — wall-clock throughput: uncompressed input bytes for
  compress, decompressed bytes for decompress.
- **`ratio%`** — compressed size / input size (compress only).
- **`cpu s/GB`** — `getrusage` user+sys seconds per GB processed. This is
  the number that usually decides hardware offload adoption: the win is
  freed host CPU, not necessarily shorter wall time.
- **`verify`** — for hw compress rows: the sample decoded cleanly with the
  system liblz4 (`OK`), proving the on-disk format is unchanged and the
  read path can stay in software.

Native rows carry both views at once (see
[native-bench.md](native-bench.md)): the `GB/s` times the engine's lz77
parse — the hardware ceiling, a format no software implementation
produces — while `ratio_pct` and `verify` describe the rebuilt standard
LZ4 blocks, the view that kae-vs-sw comparisons are read on. When the
sample rebuild fails, the timed engine numbers stand: the row keeps
`OK`, `ratio_pct` falls back to the pre-rebuild engine output and the
note records `sample=failed:<cause>` (and no verify runs). The native
decompress row is in-crate software decode of those rebuilt blocks —
liblz4's job in the other implementations, just without the C library.

- **`status`** — `OK`, or a failure tag: `OPEN_FAIL` (library could not be
  opened), `COMPRESS_FAIL`, `DECOMPRESS_MISMATCH`, `ASYNC_FAIL`,
  `INPUT_SMALL`, `UNSUPPORTED` (e.g. hwasync + decompress),
  `WORKER_ERR`
  (the subprocess died; stderr tail is in the note). A failure inside the
  timed loop fails the cell with its error tag — no partial numbers are
  reported. When no patched library is found, `hw`/`hwasync` cells are
  skipped entirely and the environment report says so. Failed rows keep
  their diagnostics in `note`: the worker's `at=`/`err=` fields, plus the
  first words of its stderr.

Timing per cell is adaptive: at least `--passes-min` passes and
`--min-sec` seconds, capped at `--passes-max`.

### The verdict

After the table, the bench reports, per block size, the hw/sw speedup and
the hw CPU cost as a percentage of sw.
With `--workers` >1 both sides are N-thread pools, so those numbers
compare equal thread budgets — the production shape a thread-pooled zfile
would actually run; at the default 1 they answer the single-thread
drop-in question. Then a decision rule, applied at the 4K (zfile)
granularity:

- hw throughput ≥ 1.1× sw → **outright win**; the synchronous drop-in is
  worth it.
- otherwise hw CPU ≤ 50% of sw → **CPU offload value** — worth it where
  host CPU is contended even at flat throughput.
- otherwise, if the 1 MiB rows show hw ≥ 1.5× → **small blocks lose, large
  blocks win** — consider larger blocks or the segment-level async batch.
- otherwise → **NO-GO** for this hardware at this granularity.

The printed criteria line repeats the thresholds; `cpu s/GB` is user+sys,
excluding wait time.

The verdict is read on the **LZ4 view**: `sw` and `hw` produce standard
LZ4 blocks end to end, so their throughput and CPU are the kae-vs-sw
comparison. `native` rows are not a verdict input — they are the hardware
ceiling reference in the lz77 view (see
[native-bench.md](native-bench.md)); read them against the plateau the
sw/hw rows show, through their LZ4 ratio and CPU.

## Output files

TSV (`--tsv`), with a comment header carrying the benchmark's host:

```
# kae-bench LZ4 host=<hostname>
data	block	impl	op	gbps	ratio_pct	cpu_s_per_gb	wall_s	verify	status	note
```

JSON (`--json`) is an array of objects with the same fields plus `wall_s`
and `note`. Verify values are `-` for non-compress rows. Failed cells keep
their diagnostics in `note`: the worker's flattened error for error tags,
the first words of its stderr for `WORKER_ERR`.

## The internal line protocol

Workers are not for manual invocation, but their stdout is the debug
surface when a cell misbehaves:

```
RESULT status=OK impl=hw op=compress block=4096 lib=... blocks=... passes=... in_bytes=... out_bytes=... ratio_pct=... wall_s=... gbps=... user_s=... sys_s=... cpu_s_per_gb=... note=...
VERIFY status=OK impl=sw block=4096 blocks=... fail=0
PROBE lib=...  +  SYMBOL <name> found|missing  +  HAS_ASYNC yes|no
```

Values are space-free by construction (diagnostics are flattened), so each
line parses with simple splitting. Worker exit codes: `0` OK, `1` usage or
I/O problem, `2` the library could not be opened, `4` compression failed,
`5` decompression/verification failed. `--probe` runs the symbol probe for
both libraries without any timing — the first thing to run on an
unfamiliar machine.
