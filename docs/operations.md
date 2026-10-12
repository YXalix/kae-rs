# Operations guide

Installing the KAE LZ4 stack, the deployment layout the crate probes, the
environment variables, the rules that keep the host healthy, and what to
check when something does not work.

## Installing the KAE 2.x stack

Nothing in this crate installs or downloads anything — the hardware surface
appears purely at runtime, through `dlopen`. The deployment described below
is produced by the official KAE 2.x build: the `KAELz4` module applies
`KAELz4/open_source/kaelz4_1_9_4.patch` to upstream LZ4 v1.9.4 and installs
the result — the patched `liblz4.so.1.9.4` plus `libkaelz4.so.2.x` — into
`/usr/local/kaelz4/lib`. The patched library is a stock upstream artifact,
not a custom build; the `2.2.0` in `HW_PRELOAD_CANDIDATES` is simply the
KAE release deployed on the target machines.

Condensed install order for a KAELz4 machine (root; full detail in the
[official installation guide](https://github.com/kunpengcompute/KAE/blob/kae2/docs/en/installation_guide.md)):

1. Hardware presence: KAELz4 runs only on the new Kunpeng 920 processor
   model and Kunpeng 950 processors (BIOS V105+, iBMC V365+; K-series
   servers ship license-enabled, the new 920 model needs BIOS 21.23+;
   disable SMMU). `lspci | grep ZIP` must list a HiSilicon ZIP Engine.
2. Supported OS with build dependencies — openEuler 22.03 LTS SP1–SP4 /
   24.03 LTS SP1–SP3, EulerOS V2.0 SP12, TencentOS 5.4 (on kernel 4.19 only
   user-mode interfaces work):

   ```bash
   yum install -y make kernel-devel-`uname -r` libtool numactl-devel \
       openssl-devel lz4-devel libzstd-devel chrpath cmake libunwind-devel patch
   ```

3. Source:

   ```bash
   git clone https://gitcode.com/boostkit/KAE.git -b kae2
   ```

4. Driver, UADK framework, then the module (from the KAE source directory):

   ```bash
   sh build.sh driver   # uacce.ko, hisi_qm.ko, hisi_zip.ko -> /lib/modules/`uname -r`/extra
   sh build.sh uadk     # libwd.so.2, libwd_comp.so.2       -> /usr/local/lib
   sh build.sh lz4      # liblz4.so.1.9.4, libkaelz4.so.2.x -> /usr/local/kaelz4/lib
   ```

   Driver check: `/sys/class/uacce/` lists `hisi_zip-*` entries and
   `lsmod | grep hisi_qm` shows the `hisi_qm` and `uacce` modules.
   `sh build.sh all` installs every module in one shot (requires OpenSSL
   1.1.1x); RPM packages exist for openEuler 22.03 SP1–SP4 and EulerOS
   V2.0 SP12.
5. Verify exactly what this crate probes:

   ```bash
   ls /dev/uacce*
   ls /usr/local/kaelz4/lib/         # liblz4.so.1.9.4 + libkaelz4.so.2.x
   ./target/release/kae-bench --probe
   ```

Two places where the official flow and this crate differ, deliberately:

- The official test procedure exports `LD_LIBRARY_PATH=/usr/local/kaelz4/lib`
  per shell for its vendor test tools. Per-process use is fine; the global
  loader path remains forbidden (hard rules below). This crate needs neither:
  it dlopens by absolute path and preloads its dependencies itself.
- A different KAE release bumps the libkaelz4 soname (the guide shows 1.0.0
  and 2.0.4 in its examples). The `liblz4.so.1` symlink keeps the direct
  open working across releases; only the preload-retry candidate in
  `HW_PRELOAD_CANDIDATES` is pinned to `libkaelz4.so.2.2.0`. On another
  release with broken rpaths, extend that list.

Do not confuse the hardware forwarder with the other Kunpeng LZ4: the
[kunpengcompute/lz4](https://github.com/kunpengcompute/lz4) repository (patch
`kunpeng_lz4_v1.0.patch`) is a NEON software optimization of the same
upstream v1.9.4 — same version string, no hardware path.

## Expected deployment layout

The crate probes these paths at runtime (first existing entry wins):

```
/usr/local/kaelz4/lib/liblz4.so.1.9.4   # patched liblz4 (probe candidates:
/usr/local/kaelz4/lib/liblz4.so.1       #   HW_LIB_CANDIDATES, overridable
/usr/local/kaelz4/lib/liblz4.so         #   via KAE_HW_LZ4_LIB)
/usr/local/lib/libwd.so.2               # preload candidates (dependency
/usr/local/lib/libwd_comp.so.2          #   chain, only existing entries
/usr/local/kaelz4/lib/libkaelz4.so.2.2.0#   are preloaded RTLD_GLOBAL)
/dev/uacce*                             # hardware devices (ZIP engine)
```

Direct `dlopen` of the patched library is tried first; if it fails —
typically missing rpaths in the dependency chain — the crate preloads
libwd → libwd_comp → libkaelz4 with `RTLD_GLOBAL` and retries once inside
its own process. `KaeLz4::note()` reports when the retry path was taken.

## Hard rules

**Never put the KAE patched library into `/usr/local/lib`, and never add
`/usr/local/kae*` to the global `LD_LIBRARY_PATH` or `ld.so.conf.d`.**

The patched library shares the SONAME `liblz4.so.1` with the system liblz4
(and declares similar dependencies for libzstd/libz). Exposing it globally
hijacks every host process that links those libraries — gcc's `cc1` and
`readelf` are known to break. This crate's `RTLD_GLOBAL` preloading is
scoped to its own process and carries no such risk; that is precisely why
the loader work happens here instead of in the global configuration.

## Environment variables

| Variable | Set by whom | Effect |
|---|---|---|
| `KAE_HW_LZ4_LIB` | you | Overrides the patched-library probe path (must exist) |
| `KAE_LZ4_LEVEL` | you, or `AsyncBatch::configure` | Compression level for the async engine |
| `KAE_LZ4_ASYNC_THREAD_NUM` | you, or `AsyncBatch::configure` | Library thread pool size for the async engine |
| `KAE_LZ4_ASYNC_DC_THREAD_NUM` | you | Additional libkaelz4 tuning |

The async engine reads its tuning once, at the first `AsyncBatch::open` of
the process; later changes (env or `configure()`) have no effect there.

## Health checks

```bash
ls /dev/uacce*                          # hardware present?
./target/release/kae-bench --probe      # libraries load? symbols resolve? async surface?
./target/release/kae-bench --data mixed --blocks 4096 --ops compress   # quick sanity scan
```

`--probe` opens each library in its own subprocess and lists every standard
and async symbol as found/missing, plus a `HAS_ASYNC` summary line. Run it
first on any unfamiliar machine; it exercises the same preload-retry logic
as the real paths.

Also worth remembering: `Backend::Hw` means the patched library loaded —
not that the ZIP engine answered. With no `/dev/uacce*` devices the patched
library silently compresses in software internally, at hardware-API speed.
The bench prints a warning in exactly this situation.

## Troubleshooting

| Symptom | Likely cause / what to do |
|---|---|
| `KaeError::Open` on `open_hw`/`open()` | The patched library is missing (check `hw_lib_path()`, `KAE_HW_LZ4_LIB`) or its dependencies do not resolve. The error text contains the first direct error and what the preload retry tried. Check that `libwd.so.2` / `libwd_comp.so.2` / `libkaelz4.so.2.2.0` exist at the paths above. |
| `KaeError::Symbol` from `AsyncBatch::open` | The loaded library has no async surface — you pointed at a stock liblz4 (which must be rejected: async is an explicit hardware choice) or an incompletely patched one. Run `--probe` and check the `SYMBOL ... LZ4_compress_async` line. |
| `KaeError::Async` "already initialized" | `AsyncBatch::configure()` ran after the process's first `AsyncBatch::open`. Move it before all opens. |
| `hw` rows show ~same speed (and ~same CPU) as `sw` | No `/dev/uacce*` device: the patched library is silently taking its internal software fallback. Check the driver (`ls /dev/uacce*`); the bench prints an explicit warning. |
| hw throughput fine but CPU saving smaller than expected | Look at `cpu s/GB`, not only GB/s, and prefer `--input` with the real workload; small blocks and the synchronous per-block call pattern cap the offload — compare against `hwasync` to see what segment-level batching buys. |
| `RESULT status=ASYNC_FAIL ... TASK_QUEUE_FULL` | The KAE task queue was full with nothing in flight from this batch — another process is hogging the queue. Retry when the co-tenant finishes, or lower concurrency. |
| `RESULT status=WORKER_ERR` in a cell | The worker subprocess died; the note carries the first words of its stderr. Re-run that single cell (narrow `--blocks/--data/--impl`) to see the full output. |
| gcc (`cc1`) or `readelf` suddenly broken host-wide | Someone violated a hard rule — the patched library is on the global loader path. Remove `/usr/local/kae*` from `LD_LIBRARY_PATH` / `ld.so.conf.d` / `/usr/local/lib`. |
| `GLIBC_2.34 not found` when copying the binary to another host | The binary was built on a newer glibc. Build on the target machine (`cargo build --release` needs nothing beyond rustc 1.70+ — the crate has zero dependencies) or verify requirements with `objdump -T kae-bench \| grep GLIBC \| sort -u`. |

## Library behavior guarantees to rely on

- **No silent fallback on the hardware paths.** `open_hw` and
  `AsyncBatch::open` return explicit errors without the patched library;
  `AsyncBatch` additionally rejects any library without the async symbols.
  Only `KaeLz4::open()` falls back, and only down to the system liblz4.
- **Format compatibility is checked, not assumed.** Every successful hw
  compress cell is cross-validated: the first 32 MiB of blocks is decoded
  by the system liblz4 and compared bit-exactly.
- **Process-exit safety.** The async engine is never torn down explicitly
  (a teardown would break other library users in the same process);
  process exit reclaims it. The dlopen handles are likewise never closed.

## References

- KAE 2.x source and the authoritative install guide:
  [gitcode.com/boostkit/KAE](https://gitcode.com/boostkit/KAE), branch
  `kae2`; GitHub mirror
  [kunpengcompute/KAE](https://github.com/kunpengcompute/KAE), guide at
  `docs/en/installation_guide.md` (KAELz4 perf tool: `KAELz4/test/perftest`)
- Kunpeng NEON-optimized software LZ4 — not the hardware forwarder:
  [kunpengcompute/lz4](https://github.com/kunpengcompute/lz4)
  ([gitcode.com/boostkit/lz4](https://gitcode.com/boostkit/lz4))
- KAEzip, the zlib sibling module:
  [kunpengcompute/KAEzip](https://github.com/kunpengcompute/KAEzip)
- Kunpeng documentation portal:
  [hikunpeng.com/document](https://www.hikunpeng.com/document)
