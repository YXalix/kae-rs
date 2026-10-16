# The native sweep: one command, the complete dataset

The sweep answers one verdict question (see
[native-bench.md](native-bench.md)): **is the ZIP engine faster than the
~4 GB/s plateau the C userspace stack achieves, or is the engine itself
the ceiling?** One command runs the whole experiment series and leaves
every number in a single directory:

```bash
cargo build --release
./target/release/kae-bench --sweep            # data lands in ./kae-sweep
./target/release/kae-bench --sweep /mnt/s1    # or any directory
```

About ten minutes. The directory is the deliverable — send it back
whole (`tar cf kae-native-data.tar kae-sweep`), nothing else is needed.

## Preconditions

- One or more `/dev/hisi_zip-*` uacce nodes (QM v3 only). Without one
  only the e1 baseline runs and everything native is skipped with a
  printed reason.
- The system liblz4 (`liblz4.so.1`) for the verify op.

## What it runs

Every experiment is a full `kae-bench` run re-executed as a subprocess
with its own TSV and log; every native run pins the three knobs
explicitly (`KAE_NATIVE_DEPTH`, `KAE_NATIVE_DB_BATCH`, `KAE_NATIVE_WAIT`)
so nothing inherits ambient environment. Native experiments use 64K
requests and sustained 2s windows — the default 0.8s windows read boost
clocks (the first sweep's 64K rows read 9.6 GB/s at 0.8s and 6.4 GB/s
at 2s).

| files | experiment |
|---|---|
| `machine.txt`, `rev.txt`, `probe.txt` | host, devices, `perf_mode`, library inventory, code revision, symbol probe |
| `e1-base.tsv` | the anchor: sw, hw, hwasync and native on the same synthetic input (or `--input` file), default windows |
| `e2-depth-{1,4,16,64,256}.tsv` | the verdict: single-queue throughput against in-flight depth, 64K, spin |
| `e3-queues2.tsv`, `e3-queues4.tsv` | queue scaling at depth 8 (`--workers` = queues, round-robin over devices) |
| `e4-spin-d8.tsv`, `e4-poll-d8.tsv` | wait policy: identical runs differing only in spin vs `poll()`-the-fd |
| `e5-db1.tsv` | the doorbell tax: `KAE_NATIVE_DB_BATCH=1` against the full-depth batch |
| `e6-small.tsv` | sustained small-block scan: 4K, 8K, 16K, 64K, 128K requests |
| `native-fail-*.txt` | rebuild-check dumps, if any sample failed (the audit verdict inside names the side at fault) |
| `summary.md` | every TSV as a table plus the computed readings (ladder saturation, spin-vs-poll CPU, queue scaling, doorbell) |

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
  throughput** (`cpu s/GB`, and `nvcsw` in the row note) → the
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
than the engine. `machine.txt` records which mode every number was
taken under. Reload the driver with `perf_mode=1` and re-run the sweep
into a fresh directory.

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
