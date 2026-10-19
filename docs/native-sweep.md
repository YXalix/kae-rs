# The native sweep: one command, one file

The sweep answers one verdict question (see
[native-bench.md](native-bench.md)): **is the ZIP engine faster than the
~4 GB/s plateau the C userspace stack achieves, or is the engine itself
the ceiling?** One command runs the whole experiment series and leaves
every number in a single file:

```bash
cargo build --release
./target/release/kae-bench --sweep              # data lands in ./kae-sweep.tsv
./target/release/kae-bench --sweep /mnt/s1.tsv  # or any path
```

About ten minutes. The file is the deliverable — send it back as is.

## The verdict, from the 2026-10-19 run

The first full green sweep (`rev 0b3b7ab`, host `debian-auto`, 2×
`hisi_zip` + 2× `hisi_sec2`, `perf_mode=1`, sustained 2 s windows,
synthetic mixed data) answered the question. Every row verified OK —
including the 256K/1M blocks whose sample rebuild used to trip the seam
fold-credit bug fixed in `ca34e71` — so every number below is backed by
a decodable LZ4 sample.

- **The engine is faster than the C stack; the plateau was userspace.**
  One queue sustains **6.40 GB/s** from depth 4 up (lz77 view) — 3.7×
  the same run's `hw` sync cell (1.74 GB/s at 64K) and 1.6× its
  `hwasync` batch (4.02 GB/s) — while software compresses at 8.41 GB/s.
- **The depth ladder saturates at depth 4**: d1 2.92 → d4 6.39 → flat
  6.40 through d256. Depth 1 is latency-bound; four slots hide the
  service time.
- **Queues double once, then flatten at 64K**: 1q 6.40 → 2q 12.79 → 4q
  12.80 GB/s. At 8K requests — the per-queue sweet spot at 11.6–11.8
  GB/s — the doubling is clean: **2 queues × 8K = 23.20 GB/s** vs 11.57
  at one queue.
- **`poll` wins the wait side**: equal throughput to `spin` (6.39 vs
  6.40 GB/s) at 0.040 vs 0.160 cpu s/GB, `nvcsw` 64839 vs 0 — the
  sleep-until-IRQ design works end to end through uacce.
- **The doorbell tax is not measurable at 64K**: `db_batch=1` matches
  the full-depth batch at 6.40 GB/s.
- **Sustained small blocks, one queue**: 4K 9.3–10.4, 8K 11.6–11.8, 16K
  7.98, 64K 6.40, 128K 6.19 GB/s.
- **The LZ4-view reading** — engine time plus the measured rebuild cost
  (`rebuild_s_per_gb` 0.027–0.038; estimate `1/(1/GB/s + rebuild)`,
  submit-side copies not modeled): a two-queue native pipeline lands at
  ~9.4 GB/s at 64K blocks and ~13.5 GB/s at 8K, above software's 8.41 /
  8.94 on the same box, and with `poll` waiting its pump+rebuild CPU
  (~0.07 s/GB) undercuts software's 0.11–0.17. For compression, the
  native backend is worth finishing.
- **The open gap is decode**: the in-crate decoder runs 1.4 GB/s where
  the C libraries decompress at 5.5–10.7 GB/s on the same box — the
  native read path needs decoder work before it can replace the
  library.

## The file's shape

- Leading `#` comment block: the machine context every number was taken
  under — kernel, CPU, zip device inventory, `perf_mode`, libraries,
  code revision, and the symbol probe.
- One shared table with an `experiment` first column, then the usual
  TSV columns (data/block/impl/op/gbps/ratio_pct/cpu_s_per_gb/wall_s/
  verify/status/note). All experiments' rows are in that one table; an
  experiment that produced no rows leaves a `#` comment with its stderr
  tail instead.
- Trailing `# readings` block: the computed summary — depth-ladder
  saturation point, spin-vs-poll CPU at equal throughput, queue
  scaling, the doorbell tax (the same lines print to the console at the
  end of the run).

The one exception that stays outside the file: `native-fail-*.txt` —
a failed block check's hex dumps (every request's triples and literal
areas, plus the input and the rebuilt block; the input line alone is
hundreds of KB). They appear only when a run recorded `sample=failed`
in its note; the console keeps the per-request audit verdicts and the
request holding the first decode diff, the file carries the bytes.
Send them along with the file when they appear.

## Preconditions

- One or more `/dev/hisi_zip-*` uacce nodes (QM v3 only). Without one
  only the e1 baseline runs and everything native is skipped with a
  printed reason.
- The system liblz4 (`liblz4.so.1`) for the verify op.

## What it runs

Every experiment is a full `kae-bench` run re-executed as a subprocess
(sw/hw keep their library isolation); every native run pins the three
knobs explicitly (`KAE_NATIVE_DEPTH`, `KAE_NATIVE_DB_BATCH`,
`KAE_NATIVE_WAIT`) so nothing inherits ambient environment. Every
experiment runs in sustained 2s windows — the default 0.8s windows
read boost clocks (the first sweep's 64K rows read 9.6 GB/s at 0.8s
and 6.4 GB/s at 2s).

| experiment | what it answers |
|---|---|
| `e1-base` | the anchor: sw, hw, hwasync and native on the same synthetic input (or `--input` file) |
| `e2-depth-{1,4,16,64,256}` | the verdict: single-queue throughput against in-flight depth, 64K, spin |
| `e3-queues2`, `e3-queues4` | queue scaling at depth 8 (`--workers` = queues, round-robin over devices) |
| `e4-spin-d8`, `e4-poll-d8` | wait policy: identical runs differing only in spin vs `poll()`-the-fd |
| `e5-db1` | the doorbell tax: `KAE_NATIVE_DB_BATCH=1` against the full-depth batch |
| `e6-small` | sustained small-block scan: 4K, 8K, 16K, 64K, 128K requests |
| `e7-queues2-8k` | queue scaling at the best small request size: does 8K double like 64K, or saturate the device pair earlier |

`--sweep` composes with the ordinary options where they make sense:
`--input FILE` sweeps a real file instead of the synthetic dataset,
`--size`, the library paths and the pass bounds pass through to every
experiment.

## Reading the results

- **Depth ladder saturates well above 4 GB/s** → the engine is faster
  than the C stack; the kaelz4 async plateau is submit/harvest overhead,
  and finishing the native backend is worth it.
- **Depth ladder saturates at or below 4 GB/s** → the engine (or its
  current `perf_mode` — see the next section) is the ceiling; no
  userspace rewrite changes the placement verdict.
- **`e4-poll-d8` at a fraction of `e4-spin-d8`'s CPU with equal
  throughput** (`cpu_s_per_gb`, and `nvcsw` in the row note) → the
  sleep-until-IRQ design works end to end through uacce.
- **`e3-queues*` vs `e4-spin-d8`** → whether a second device's queues
  add throughput on top of the single-queue saturation point.
- **`nvcsw`/`nivcsw` in every native row's note**: spin runs show ~zero
  voluntary switches, poll runs many — the same one-number check
  proposed for any wait-side rewrite.

Keep the two views straight: the native GB/s numbers are the **lz77
ceiling view** (the engine's parse alone); the kae-vs-sw adoption
comparison (throughput + CPU on the same LZ4 format) is read from the
e1 sw/hw rows against the native rows' LZ4 ratio and CPU.

## If the ladder saturates at or below 4 GB/s

Do not conclude "the engine is the ceiling" until the sweep has been
repeated with the driver loaded `perf_mode=1`: under the default mode
every number is a floor, so a ≤4 GB/s saturation may be the mode rather
than the engine. The file's header records which mode every number was
taken under. Reload the driver with `perf_mode=1` and re-run the sweep
to a different file.

## Custom runs

The sweep covers the standard series. For anything else the same
commands work standalone — the knob reference and the per-experiment
rationale are [native-bench.md](native-bench.md):

```bash
# a single experiment by hand
KAE_NATIVE_DEPTH=4 ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2 --tsv /dev/null

# plumbing check before anything else: deflate dialect completes
KAE_NATIVE_ALG=deflate ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2 --tsv /dev/null
```
