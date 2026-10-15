# The native sweep runbook: what to run on the target machine

The native cells exist to answer one verdict question (see
[native-bench.md](native-bench.md)): **is the ZIP engine faster than the
~4 GB/s plateau the C userspace stack achieves, or is the engine itself
the ceiling?** This page is the concrete on-machine procedure — one
command per experiment, one TSV per run, roughly fifteen minutes end to
end. Run it from a checkout of 9018b49 ("raw: rebuild the lz77 triples
into standard LZ4 for the sample verify") or later.

Keep the two views straight while sweeping: the native GB/s numbers
below are the **lz77 ceiling view**; the kae-vs-sw adoption comparison
(throughput + CPU on the same LZ4 format) is read from the E1 anchor's
sw/hw rows against the native rows' LZ4 ratio and CPU.

## Preconditions

- One or more `/dev/hisi_zip-*` uacce nodes (QM v3 only). The
  queue-scaling experiment wants two devices; `ls -l /dev/hisi_zip-*`
  says what the machine has.
- The system liblz4 (`liblz4.so.1`) for the verify op.
- `cargo build --release`.

## Machine context (once)

```bash
git rev-parse HEAD          > rev.txt
lscpu                       > machine.txt
ls -l /dev/hisi_zip-*      >> machine.txt
ldconfig -p | grep liblz4  >> machine.txt
cat /sys/module/hisi_zip/parameters/perf_mode >> machine.txt 2>/dev/null || true
./target/release/kae-bench --probe
```

The `perf_mode` line records which driver mode every number below was
taken under: the default `perf_mode=0` is the high-compression-ratio,
low-throughput mode, so all of them are floors (see
[kae-limitations.md](kae-limitations.md)).

## The experiments

Each run fully rewrites its `--tsv` file, so every command names its
own. No extra flags are needed beyond what is shown: every native row's
`note` field carries the exact configuration
(`alg,queues,depth,db_batch,wait,req_size,nvcsw,nivcsw[,pre_ratio]`).

### E0 — plumbing check (seconds)

```bash
KAE_NATIVE_ALG=deflate ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2 --tsv /dev/null
```

Confirms the uacce dialog (open, SQE, doorbell, CQE) works on this
machine: `status=OK` and no hw errors before anything else is worth
running. `NO_DEVICE`/`OPEN_FAIL` here stops the sweep.

### E1 — full baseline: all impls, one scan (~2–3 min)

```bash
./target/release/kae-bench --tsv e1-base.tsv
```

The anchor: sw, hw, hwasync and native on the same synthetic input, same
driver mode. The 2.91 GB/s sync and ~4 GB/s async figures in
[kae-limitations.md](kae-limitations.md) predate the one-input bench —
re-anchor before comparing anything against them.

### E2 — the depth ladder (the verdict)

```bash
for k in 1 4 16 64 256; do
  KAE_NATIVE_DEPTH=$k ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2 --tsv e2-depth-$k.tsv
done
```

The decisive curve: single-queue throughput against in-flight depth.
Saturation well above 4 GB/s means the C-stack plateau is a userspace
artifact and the native backend is worth finishing; saturation at or
below it means the engine (or its current `perf_mode` — see the last
section) is the ceiling. Depth 1024 is skipped deliberately: at 64K
requests it asks for ~260 MiB of DMA-coherent SS memory per queue and
fails in the mapping ([native-bench.md](native-bench.md)).

### E3 — queue scaling

```bash
./target/release/kae-bench --impl native --workers 2 --blocks 65536 --min-sec 2 --tsv e3-queues2.tsv
# with two or more devices, also:
./target/release/kae-bench --impl native --workers 4 --blocks 65536 --min-sec 2 --tsv e3-queues4.tsv
```

Whether the second device's queues add throughput on top of E2's
saturation point — the difference between one engine and two.

### E4 — wait policy: spin vs poll

```bash
KAE_NATIVE_WAIT=poll ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2 --tsv e4-poll-d8.tsv
# repeat at the depth K where E2 saturated:
KAE_NATIVE_WAIT=poll KAE_NATIVE_DEPTH=<K> ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2 --tsv e4-poll-dbest.tsv
```

Same throughput at a fraction of the pump CPU (`cpu_s_per_gb`, and
`nvcsw`/`nivcsw` in the note) is the sleep-until-IRQ design working
end to end.

### E5 — the doorbell tax

```bash
KAE_NATIVE_DB_BATCH=1 ./target/release/kae-bench --impl native --blocks 65536 --min-sec 2 --tsv e5-db1.tsv
```

Per-request doorbell stores against E2's default full-depth batching —
the MMIO cost the C stack pays on every single request.

## Sending the results back

```bash
tar czf kae-native-data.tgz rev.txt machine.txt \
  e1-base.tsv e2-depth-*.tsv e3-*.tsv e4-*.tsv e5-db1.tsv
```

## If the ladder saturates at or below 4 GB/s

Do not conclude "the engine is the ceiling" until the sweep has been
repeated with the driver loaded `perf_mode=1`: under the default mode
every number is a floor, so a ≤4 GB/s saturation may be the mode rather
than the engine. Record how the driver is loaded (modprobe options,
module parameter file), reload with `perf_mode=1`, and re-run E1–E5.
A ladder that saturates above 4 GB/s under the default mode needs no
second pass — the verdict is already in.
