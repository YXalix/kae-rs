# Benchmark guide (kae-bench)

`kae-bench` measures the KAE-patched liblz4 against the system liblz4 at
zfile granularity (LZ4 block format, per-block `LZ4_compress_default`), plus
the segment-level async batch. The same binary is the `cargo bench` entry
point — `cargo bench -- <args>` behaves identically. For what the results
mean about the hardware path — weaknesses and their root causes — see
[kae-limitations.md](kae-limitations.md).

## Running

```bash
cargo build --release
./target/release/kae-bench --probe     # environment & symbol probe, then exit
./target/release/kae-bench             # full scan: sw / hw / hwasync (~5 min)
```

Common invocations:

```bash
# a real workload instead of synthetic data (e.g. an exported VM memory image)
./target/release/kae-bench --input /path/to/memory.img

# shrink the matrix
./target/release/kae-bench --blocks 4096,65536,1048576 --ops compress
./target/release/kae-bench --data mixed --impl sw,hw

# machine-readable output
./target/release/kae-bench --tsv results.tsv --json results.json
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `--input FILE` | — | Benchmark a real file instead of synthetic data (dataset label becomes `input`) |
| `--size MIB` | 64 | Synthetic dataset size per dataset |
| `--blocks LIST` | `4096,8192,65536,262144,1048576` | Block sizes to scan |
| `--data LIST` | `zeros,mixed,random` | Datasets (ignored with `--input`) |
| `--impl LIST` | all present | `sw`, `hw`, `hwasync` (hwasync is compress-only) |
| `--ops LIST` | `compress,decompress` | Operations to run |
| `--sw-lz4-lib P` | `liblz4.so.1` | Software library path |
| `--hw-lz4-lib P` | probe | Patched library path |
| `--min-sec F` | 0.8 | Minimum timed seconds per cell |
| `--passes-min N` / `--passes-max N` | 3 / 99 | Pass bounds per cell (the max caps a pathologically slow implementation) |
| `--tsv PATH` | `./kae_bench_results.tsv` | TSV output (empty string disables) |
| `--json PATH` | off | Additional JSON output |
| `--probe` | off | Environment + symbol probe only |
| `-h, --help` | | Help |

Flags that libtest appends under `cargo bench` (`--bench`, `--nocapture`,
…) are accepted and ignored. Invalid list values are rejected up front with
an error: `--blocks` must parse to block sizes, `--data` must name known
datasets (`zeros,mixed,random`), `--impl` must be `sw`/`hw`/`hwasync`, and
`--ops` must be `compress`/`decompress`.

## What it does, in order

1. **Environment report** — host, kernel, CPU model, the uacce device
   inventory (`/dev/hisi_zip-*` and siblings), the software and hardware
   library paths, and the preload chain. Warns when no uacce device is
   present (the patched library then compresses in software internally, and
   "hw" rows may be software in fact).
2. **Data generation** — writes the synthetic datasets (or stages the
   `--input` file). All datasets are deterministic: a fixed-seed
   xorshift64*, 4K-page-interleaved layout, so numbers are comparable
   across machines and runs.
   - `zeros` — all-zero pages (best case for LZ4)
   - `mixed` — 50% zero + 20% random + 30% text pages, approximating a VM
     memory snapshot's compressibility profile
   - `random` — incompressible pages (worst case)
3. **Scan** — for every (dataset × block size × implementation × operation)
   cell, re-executes itself as a worker subprocess with exactly one library
   open, parses its `RESULT` line, and — for compress cells with `OK`
   status — runs a second subprocess that decodes the captured sample with
   the **system** liblz4 and compares block-by-block (`VERIFY`).
4. **Summary** — a results table, a verdict, and the TSV/JSON files.

### Why subprocess isolation

The patched library and the system liblz4 share the SONAME `liblz4.so.1`.
Once one of them is loaded in a process, every later `dlopen` of that
SONAME silently resolves to it — software and hardware numbers would
pollute each other inside a single process. One subprocess per cell is the
only reliable isolation, and its cost is negligible at second-scale timing.

## Metrics and how to read them

- **`GB/s`** — wall-clock throughput: raw input bytes for compress,
  decompressed raw bytes for decompress.
- **`ratio%`** — compressed size / input size (compress only).
- **`cpu s/GB`** — `getrusage` user+sys seconds per GB processed. This is
  the number that usually decides hardware offload adoption: the win is
  freed host CPU, not necessarily shorter wall time.
- **`verify`** — for hw compress rows: the sample decoded cleanly with the
  system liblz4 (`OK`), proving the on-disk format is unchanged and the
  read path can stay in software.
- **`status`** — `OK`, or a failure tag: `OPEN_FAIL` (library could not be
  opened), `COMPRESS_FAIL`, `DECOMPRESS_MISMATCH`, `ASYNC_FAIL`,
  `INPUT_SMALL`, `UNSUPPORTED` (e.g. hwasync + decompress), `WORKER_ERR`
  (the subprocess died; stderr tail is in the note). A failure inside the
  timed loop fails the cell with its error tag — no partial numbers are
  reported. When no patched library is found, `hw`/`hwasync` cells are
  skipped entirely and the environment report says so.

Timing per cell is adaptive: at least `--passes-min` passes and
`--min-sec` seconds, capped at `--passes-max`.

### The verdict

After the table, the bench picks a dataset (preferring `input`, then
`mixed`, then any dataset with a successful compress row) and, per block
size, reports the hw/sw speedup and the hw CPU cost as a percentage of sw.
Then a decision rule, applied at the 4K (zfile) granularity:

- hw throughput ≥ 1.1× sw → **outright win**; the synchronous drop-in is
  worth it.
- otherwise hw CPU ≤ 50% of sw → **CPU offload value** — worth it where
  host CPU is contended even at flat throughput.
- otherwise, if the 1 MiB rows show hw ≥ 1.5× → **small blocks lose, large
  blocks win** — consider larger blocks or the segment-level async batch.
- otherwise → **NO-GO** for this hardware at this granularity.

The printed criteria line repeats the thresholds; `cpu s/GB` is user+sys,
excluding wait time.

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
RESULT status=OK impl=hw op=compress data=mixed block=4096 lib=... blocks=... passes=... in_bytes=... out_bytes=... ratio_pct=... wall_s=... gbps=... user_s=... sys_s=... cpu_s_per_gb=... note=...
VERIFY status=OK impl=sw data=mixed block=4096 blocks=... fail=0
PROBE lib=...  +  SYMBOL <name> found|missing  +  HAS_ASYNC yes|no
```

Values are space-free by construction (diagnostics are flattened), so each
line parses with simple splitting. Worker exit codes: `0` OK, `1` usage or
I/O problem, `2` the library could not be opened, `4` compression failed,
`5` decompression/verification failed. `--probe` runs the symbol probe for
both libraries without any timing — the first thing to run on an
unfamiliar machine.
