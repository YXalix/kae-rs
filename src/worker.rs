//! `--_worker`: a subprocess spawned by the orchestrator by re-executing
//! itself; it opens exactly one liblz4 implementation at a time (avoiding
//! the patched/system same-SONAME conflict), goes through the public
//! `KaeLz4::open_path` API, and prints single-line RESULT/VERIFY/PROBE
//! protocol records to stdout for the parent to parse and aggregate. Not
//! for manual invocation.
//!
//! Throughput accounting: compress is "uncompressed input bytes / wall clock",
//! decompress is "decompressed bytes / wall clock"; CPU cost is
//! getrusage(user+sys) normalized per GB of input — the criterion for a
//! hardware offload is a significant drop in CPU/GB, not necessarily a
//! faster wall clock.

use crate::lz4::PROBE_SYMBOLS;
use crate::rebuild;
use crate::sysres::{timed_loop, timed_pool_loop, LoopStats};
use crate::uacce;
use crate::{AsyncBatch, Backend, KaeLz4};
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

pub(crate) const IMPL_SW: &str = "sw";
pub(crate) const IMPL_HW: &str = "hw";
pub(crate) const IMPL_HWASYNC: &str = "hwasync";
pub(crate) const IMPL_NATIVE: &str = "native";

/// Upper bound on --workers, matching the AgentENV zfile WorkerPool cap
/// (keeps a typo from spawning thousands of threads).
const POOL_WORKERS_MAX: usize = 256;

/// Worker exit codes: 0 OK, 1 usage/IO, 2 open failure, 4 compression
/// failure, 5 decompress/verify failure (mirrored in docs/benchmark.md).
const EXIT_OK: i32 = 0;
const EXIT_USAGE: i32 = 1;
const EXIT_OPEN_FAIL: i32 = 2;
const EXIT_COMPRESS_FAIL: i32 = 4;
const EXIT_VERIFY_FAIL: i32 = 5;

const SAMPLE_MAGIC: u32 = 0x3245_414B; // "KAE2" LE
/// Cross-validation sample cap: the blocks covering the first 32 MiB of
/// input — enough to surface format problems.
const SAMPLE_CAP_BYTES: usize = 32 << 20;
/// Capacity slack over the sample's compressed payload for its header.
const SAMPLE_SLACK_BYTES: usize = 4096;
/// Worst-case per-block compressed growth beyond the input size
/// (LZ4_compressBound adds ~input/255 + 16 bytes; this is the floor term),
/// used only as a Vec capacity hint.
const BOUND_SLACK_PER_BLOCK: usize = 16;

/// The k=v line protocol requires space-free values; dlerror and the like
/// are flattened.
fn flat(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join("_")
}

/// Blocks covered by the cross-validation sample: the first
/// `SAMPLE_CAP_BYTES` of input, at least one block, at most all of them.
fn sample_block_count(block: usize, nblocks: usize) -> usize {
    (SAMPLE_CAP_BYTES / block).max(1).min(nblocks)
}

/// Record the first pool-cell failure; later ones are dropped and the pool
/// drains without doing timed work.
fn fail_cell(first_err: &Mutex<Option<String>>, failed: &AtomicBool, msg: String) {
    let mut g = first_err.lock().unwrap();
    if g.is_none() {
        *g = Some(msg);
    }
    failed.store(true, Ordering::Relaxed);
}

/// The `--preload` value is a comma-separated path list (empty entries are
/// allowed and skipped).
fn split_preload(s: &str) -> Vec<String> {
    s.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn parse_args(args: &[String]) -> Result<WorkerArgs, String> {
    let mut a = WorkerArgs::default();
    let mut i = 0;
    let get = |i: &mut usize, args: &[String], name: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("missing value for {name}"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--lib" => a.lib = get(&mut i, args, "--lib")?,
            "--preload" => a.preload = get(&mut i, args, "--preload")?,
            "--input" => a.input = get(&mut i, args, "--input")?,
            "--block" => {
                a.block = get(&mut i, args, "--block")?
                    .parse()
                    .map_err(|e| format!("--block: {e}"))?
            }
            "--op" => a.op = get(&mut i, args, "--op")?,
            "--impl" => a.impl_name = get(&mut i, args, "--impl")?,
            "--sample-out" => a.sample_out = get(&mut i, args, "--sample-out")?,
            "--sample" => a.sample = get(&mut i, args, "--sample")?,
            "--min-sec" => {
                a.min_sec = get(&mut i, args, "--min-sec")?
                    .parse()
                    .map_err(|e| format!("--min-sec: {e}"))?
            }
            "--passes-min" => {
                a.passes_min = get(&mut i, args, "--passes-min")?
                    .parse()
                    .map_err(|e| format!("--passes-min: {e}"))?
            }
            "--passes-max" => {
                a.passes_max = get(&mut i, args, "--passes-max")?
                    .parse()
                    .map_err(|e| format!("--passes-max: {e}"))?
            }
            "--workers" => {
                a.workers = get(&mut i, args, "--workers")?
                    .parse()
                    .map_err(|e| format!("--workers: {e}"))?;
                if a.workers == 0 {
                    return Err("--workers must be >= 1".into());
                }
            }
            "--probe" => a.probe = true,
            other => return Err(format!("worker: unknown arg {other}")),
        }
        i += 1;
    }
    if a.lib.is_empty() && a.impl_name != IMPL_NATIVE {
        return Err("worker: --lib required".into());
    }
    if a.workers > POOL_WORKERS_MAX {
        return Err(format!(
            "worker: --workers {} out of range (1..={POOL_WORKERS_MAX})",
            a.workers
        ));
    }
    // probe mode never passes --impl
    if !a.probe {
        match a.impl_name.as_str() {
            IMPL_SW | IMPL_HW | IMPL_HWASYNC | IMPL_NATIVE => {}
            "" => return Err("worker: --impl required".into()),
            other => {
                return Err(format!(
                    "worker: unknown --impl {other} (expected sw|hw|hwasync|native)"
                ))
            }
        }
    }
    Ok(a)
}

#[derive(Default)]
struct WorkerArgs {
    lib: String,
    preload: String,
    input: String,
    block: usize,
    op: String,
    impl_name: String,
    sample_out: String,
    sample: String,
    min_sec: f64,
    passes_min: u64,
    passes_max: u64,
    workers: usize,
    probe: bool,
}

/// Open for probe mode: the bare `Lib` is needed to call has_symbol, which
/// KaeLz4 does not expose; the direct-open-then-preload-retry logic itself
/// is the shared `open_lib`.
fn open_bare(lib: &str, preload: &str) -> Result<crate::loader::Lib, String> {
    crate::open_lib(lib, &split_preload(preload))
        .map(|(l, _)| l)
        .map_err(|e| e.to_string())
}

pub fn run(args: &[String]) -> i32 {
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };

    if a.probe {
        return run_probe(&a);
    }

    if a.input.is_empty() || a.block == 0 || a.op.is_empty() {
        eprintln!("worker: --input/--block/--op required");
        return EXIT_USAGE;
    }

    let data = match fs::read(&a.input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("worker: read {}: {e}", a.input);
            return EXIT_USAGE;
        }
    };
    if data.len() < a.block {
        println!(
            "RESULT status=INPUT_SMALL input={}B block={}B",
            a.input, a.block
        );
        return EXIT_USAGE;
    }
    let nblocks = data.len() / a.block;
    let block = a.block;

    // Segment-level async batch (compress only; decompression is software
    // even inside the patched library, so async gains nothing)
    if a.impl_name == IMPL_HWASYNC {
        if a.op != "compress" {
            println!(
                "RESULT status=UNSUPPORTED impl=hwasync op={} note=async_compress_only",
                a.op
            );
            return EXIT_USAGE;
        }
        return bench_async_compress(&a, &data, block, nblocks);
    }

    // native uacce engine path (compress only; no C library in the process)
    if a.impl_name == IMPL_NATIVE {
        if a.op != "compress" {
            println!(
                "RESULT status=UNSUPPORTED impl=native op={} note=native_compress_only",
                a.op
            );
            return EXIT_USAGE;
        }
        return bench_native(&a, &data, block, nblocks);
    }

    // --workers >1 turns sw/hw into the threaded block pool form (the
    // AgentENV zfile WorkerPool shape); the backend label only decides
    // which library is opened
    let backend = if a.impl_name == IMPL_HW {
        Backend::Hw
    } else {
        Backend::Sw
    };
    let kae = match KaeLz4::open_path(&a.lib, &split_preload(&a.preload), backend) {
        Ok(k) => k,
        Err(e) => {
            println!(
                "RESULT status=OPEN_FAIL lib={} err={}",
                a.lib,
                flat(&e.to_string())
            );
            return EXIT_OPEN_FAIL;
        }
    };
    let note = kae.note().to_string();
    let bound = kae.bound(block);

    match a.op.as_str() {
        "compress" if a.workers > 1 => {
            bench_pool_compress(&a, &kae, &data, block, bound, nblocks, &note)
        }
        "decompress" if a.workers > 1 => {
            bench_pool_decompress(&a, &kae, &data, block, bound, nblocks, &note)
        }
        "compress" => bench_compress(&a, &kae, &data, block, bound, nblocks, &note),
        "decompress" => bench_decompress(&a, &kae, &data, block, bound, nblocks, &note),
        "verify" => verify_sample(&a, &kae, &data, block),
        other => {
            eprintln!("worker: unknown --op {other}");
            EXIT_USAGE
        }
    }
}

/// `--probe`: report symbol availability of one library through the
/// PROBE/SYMBOL/HAS_ASYNC line protocol.
fn run_probe(a: &WorkerArgs) -> i32 {
    match open_bare(&a.lib, &a.preload) {
        Ok(lib) => {
            println!("PROBE lib={}", a.lib);
            let mut async_any = false;
            for name in PROBE_SYMBOLS {
                let found = lib.has_symbol(name);
                if found && name.contains("async") {
                    async_any = true;
                }
                println!("SYMBOL {name} {}", if found { "found" } else { "missing" });
            }
            println!("HAS_ASYNC {}", if async_any { "yes" } else { "no" });
            EXIT_OK
        }
        Err(e) => {
            println!("PROBE lib={} status=OPEN_FAIL err={}", a.lib, flat(&e));
            EXIT_OPEN_FAIL
        }
    }
}

/// One OK RESULT record — the single formatter for all bench ops (compress,
/// decompress and the async batch emit the same field list).
struct OkResult<'a> {
    impl_name: &'a str,
    op: &'a str,
    block: usize,
    lib: &'a str,
    blocks: usize,
    st: &'a LoopStats,
    in_bytes: u64,
    out_bytes: u64,
    ratio_pct: f64,
    note: &'a str,
}

fn emit_ok(r: OkResult<'_>) {
    let gbps = if r.st.wall_s > 0.0 {
        r.in_bytes as f64 / r.st.wall_s / 1e9
    } else {
        0.0
    };
    let cpu_per_gb = if r.in_bytes > 0 {
        r.st.cpu_s() / (r.in_bytes as f64 / 1e9)
    } else {
        0.0
    };
    println!(
        "RESULT status=OK impl={} op={} block={} lib={} blocks={} passes={} in_bytes={} out_bytes={} ratio_pct={:.2} wall_s={:.3} gbps={:.2} user_s={:.3} sys_s={:.3} cpu_s_per_gb={:.2} note={}",
        r.impl_name, r.op, r.block, r.lib, r.blocks, r.st.passes, r.in_bytes, r.out_bytes,
        r.ratio_pct, r.st.wall_s, gbps, r.st.user_s, r.st.sys_s, cpu_per_gb, flat(r.note),
    );
}

/// hwasync: one compress_segment = a batch submission of the whole segment's
/// n blocks; the accounting matches sync compress (uncompressed input bytes / wall
/// clock + user+sys).
fn bench_async_compress(a: &WorkerArgs, data: &[u8], block: usize, nblocks: usize) -> i32 {
    // A --workers >1 maps onto the library's internal async thread pool
    // (KAE_LZ4_ASYNC_THREAD_NUM); 1 leaves the library default or any
    // externally set env var untouched. Fresh worker process, so the
    // one-shot engine init has not run yet.
    if a.workers > 1 {
        if let Err(e) = AsyncBatch::configure(Some(a.workers), None) {
            println!(
                "RESULT status=ASYNC_FAIL impl=hwasync op=compress err={}",
                flat(&e.to_string())
            );
            return EXIT_OPEN_FAIL;
        }
    }
    let mut batch = match AsyncBatch::open_path(&a.lib, &split_preload(&a.preload)) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "RESULT status=OPEN_FAIL lib={} err={}",
                a.lib,
                flat(&e.to_string())
            );
            return EXIT_OPEN_FAIL;
        }
    };

    // Whole blocks only: the timed loop and the byte accounting count
    // nblocks * block input bytes, so the trailing partial block stays out
    let full = &data[..nblocks * block];
    let in_bytes_one = (nblocks as u64) * (block as u64);

    // warm-up + capture ratio/sample
    let mut packed: Vec<u8> = Vec::new();
    let lens = match batch.compress_segment(full, block, &mut packed) {
        Ok(l) => l,
        Err(e) => {
            println!(
                "RESULT status=ASYNC_FAIL impl=hwasync op=compress block={} err={}",
                block,
                flat(&e.to_string())
            );
            return EXIT_COMPRESS_FAIL;
        }
    };
    if !a.sample_out.is_empty() {
        if let Err(e) = write_sample(&a.sample_out, block, &lens, &packed, nblocks) {
            eprintln!("worker: write sample: {e}");
            return EXIT_USAGE;
        }
    }
    let out_bytes_warm = packed.len() as u64;

    // Timing: one whole-segment batch submission per pass (timed_loop is
    // driven with a single "block", the block being the whole segment); a
    // mid-loop failure fails the cell instead of deflating the numbers
    let mut async_err: Option<String> = None;
    let st = timed_loop(
        1,
        |_| {
            if async_err.is_some() {
                return 0;
            }
            match batch.compress_segment(full, block, &mut packed) {
                Ok(l) => l.iter().sum::<usize>(),
                Err(e) => {
                    async_err = Some(e.to_string());
                    0
                }
            }
        },
        a.min_sec,
        a.passes_min,
        a.passes_max,
    );
    if let Some(e) = async_err {
        println!(
            "RESULT status=ASYNC_FAIL impl=hwasync op=compress block={} err={}",
            block,
            flat(&e)
        );
        return EXIT_COMPRESS_FAIL;
    }
    let async_note = if a.workers > 1 {
        format!("batch,workers={}", a.workers)
    } else {
        "batch".to_string()
    };
    emit_ok(OkResult {
        impl_name: IMPL_HWASYNC,
        op: "compress",
        block,
        lib: &a.lib,
        blocks: nblocks,
        st: &st,
        in_bytes: in_bytes_one * st.passes,
        out_bytes: out_bytes_warm * st.passes,
        ratio_pct: out_bytes_warm as f64 / in_bytes_one as f64 * 100.0,
        note: &async_note,
    });
    EXIT_OK
}

/// `native`: drive the ZIP engine through the crate's own uacce module — no
/// libwd, no patched library, and no payload copies in the timed loop (the
/// workload sits in DMA-visible SS memory once at setup). The request size
/// is the bench block capped at the engine's 128K lz77 flat limit;
/// `--workers` maps to hardware queues (one fd each, round-robin over the
/// devices). Knobs via environment:
/// `KAE_NATIVE_DEPTH` in-flight requests per queue (default 8; throughput
/// saturates at 4),
/// `KAE_NATIVE_DB_BATCH` SQEs per doorbell (default = depth),
/// `KAE_NATIVE_WAIT` `spin` (default) or `poll` (sleep on the uacce fd),
/// `KAE_NATIVE_ALG` `lz77` (default) or `deflate` (the plumbing-check dialect:
/// no 128K input cap, and completion sanity only).
///
/// The lz77 triples are rebuilt into standard LZ4 blocks for the sample
/// file (up to `SAMPLE_CAP_BYTES` of input, one hw request at a time), so
/// the orchestrator's verify op decodes native output with the stock library
/// and `ratio_pct` is the true LZ4 block ratio of the sample blocks. Falls
/// back to the pre-rebuild engine output ratio when no sample is possible
/// (deflate dialect, block not a request multiple, manual run without
/// `--sample-out`); the note field names the fallback.
fn bench_native(a: &WorkerArgs, data: &[u8], block: usize, nblocks: usize) -> i32 {
    let devices = uacce::zip_devices();
    if devices.is_empty() {
        println!("RESULT status=NO_DEVICE impl=native op=compress note=no_hisi_zip_uacce_node");
        return EXIT_OPEN_FAIL;
    }
    let env_num = |name: &str, default: usize| -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default)
    };
    let depth = env_num("KAE_NATIVE_DEPTH", 8).min(uacce::QM_Q_DEPTH);
    let batch = env_num("KAE_NATIVE_DB_BATCH", depth);
    let wait_poll = std::env::var("KAE_NATIVE_WAIT").map(|v| v == "poll").unwrap_or(false);
    let alg = match std::env::var("KAE_NATIVE_ALG").as_deref() {
        Ok("deflate") => uacce::NativeAlg::Deflate,
        _ => uacce::NativeAlg::Lz77,
    };
    let req_size = match alg {
        uacce::NativeAlg::Lz77 => block.min(uacce::MAX_LZ77_REQ),
        uacce::NativeAlg::Deflate => block,
    };

    let mut pump = match uacce::NativePump::open(
        &devices,
        a.workers.max(1),
        req_size,
        depth,
        batch,
        wait_poll,
        alg,
    ) {
        Ok(p) => p,
        Err(e) => {
            println!(
                "RESULT status=OPEN_FAIL impl=native op=compress err={}",
                flat(&e)
            );
            return EXIT_OPEN_FAIL;
        }
    };
    pump.load_data(data);

    // warm-up: settles queues and pages, totals the triples output for the
    // ratio; a warm-up without completions means the engine never answered
    let warm = pump.run(1.0);
    if warm.reqs_done == 0 || warm.hw_errors > 0 {
        println!(
            "RESULT status=NATIVE_FAIL impl=native op=compress block={} reqs={} hw_err={} err=engine_dead",
            block, warm.reqs_done, warm.hw_errors
        );
        return EXIT_COMPRESS_FAIL;
    }

    let st_native = pump.run(a.min_sec);
    if st_native.reqs_done == 0 || st_native.hw_errors > 0 {
        println!(
            "RESULT status=NATIVE_FAIL impl=native op=compress block={} reqs={} hw_err={} err=engine_dead",
            block, st_native.reqs_done, st_native.hw_errors
        );
        return EXIT_COMPRESS_FAIL;
    }
    // one continuous run = one pass; CPU covers the whole pump (this
    // process is single-threaded)
    let st = LoopStats {
        wall_s: st_native.wall_s,
        user_s: st_native.user_s,
        sys_s: st_native.sys_s,
        passes: 1,
    };
    // ratio starts as the pre-rebuild engine output; the sample rebuild
    // replaces it with the true LZ4 block ratio (and names the fallback in
    // the note when no sample is possible)
    let mut ratio_pct = st_native.bytes_out as f64 / st_native.bytes_in as f64 * 100.0;
    let mut sample_note = String::new();
    if !a.sample_out.is_empty() {
        if alg != uacce::NativeAlg::Lz77 {
            sample_note = ",sample=skipped:deflate".into();
        } else if block % req_size != 0 {
            sample_note = ",sample=skipped:block_not_request_multiple".into();
        } else {
            match build_native_sample(&mut pump, data, block, nblocks, req_size, &a.sample_out) {
                Ok((packed, count)) => {
                    sample_note = format!(",pre_ratio={ratio_pct:.1}");
                    ratio_pct = packed as f64 / (count as u64 * block as u64) as f64 * 100.0;
                }
                Err(e) => {
                    println!(
                        "RESULT status=COMPRESS_FAIL impl=native op=compress block={} at=sample err={}",
                        block, e
                    );
                    return EXIT_COMPRESS_FAIL;
                }
            }
        }
    }
    let note = format!(
        "native,alg={},queues={},depth={},db_batch={},wait={},req_size={},nvcsw={},nivcsw={}{}",
        match alg {
            uacce::NativeAlg::Lz77 => "lz77",
            uacce::NativeAlg::Deflate => "deflate",
        },
        a.workers.max(1),
        depth,
        batch,
        if wait_poll { "poll" } else { "spin" },
        req_size,
        st_native.nvcsw,
        st_native.nivcsw,
        sample_note
    );
    emit_ok(OkResult {
        impl_name: IMPL_NATIVE,
        op: "compress",
        block,
        lib: &a.lib,
        blocks: nblocks,
        st: &st,
        in_bytes: st_native.bytes_in,
        out_bytes: st_native.bytes_out,
        ratio_pct,
        note: &note,
    });
    EXIT_OK
}

/// Rebuild the first blocks of the workload into standard LZ4 blocks (one
/// hardware request per `req_size` slice, engine idle after the timed run)
/// and write the packed sample file. Returns (rebuilt bytes, blocks
/// covered). `block` must be a `req_size` multiple — the caller checks.
fn build_native_sample(
    pump: &mut uacce::NativePump,
    data: &[u8],
    block: usize,
    nblocks: usize,
    req_size: usize,
    path: &str,
) -> Result<(u64, usize), String> {
    let count = sample_block_count(block, nblocks);
    let per_block = block / req_size;
    let mut packed: Vec<u8> = Vec::new();
    let mut lens = Vec::with_capacity(count);
    let mut stream = Vec::new();
    for b in 0..count {
        let base = packed.len();
        for r in 0..per_block {
            let off = b * block + r * req_size;
            let chunk = &data[off..off + req_size];
            let sample = pump.sample_triples(chunk)?;
            let n = rebuild::lz77_block(chunk, &sample.seqs, &mut stream).map_err(|e| {
                format!(
                    "rebuild: {e} (lit={} seqs={} ll_overflow_cnt={} pos={})",
                    sample.lit,
                    sample.seqs.len() / 8,
                    sample.ll_overflow_cnt,
                    sample.ll_overflow_pos
                )
            })?;
            packed.extend_from_slice(&stream[..n]);
        }
        lens.push(packed.len() - base);
    }
    write_sample(path, block, &lens, &packed, count).map_err(|e| format!("write_sample: {e}"))?;
    Ok((packed.len() as u64, count))
}

/// Packed sample file layout: [magic][block][count] + per-block (clen +
/// blob), shared by the sync and async capture paths.
fn push_sample_header(buf: &mut Vec<u8>, block: usize, count: usize) {
    buf.extend_from_slice(&SAMPLE_MAGIC.to_le_bytes());
    buf.extend_from_slice(&(block as u32).to_le_bytes());
    buf.extend_from_slice(&(count as u32).to_le_bytes());
}

fn push_sample_block(buf: &mut Vec<u8>, clen: usize, blob: &[u8]) {
    buf.extend_from_slice(&(clen as u32).to_le_bytes());
    buf.extend_from_slice(&blob[..clen]);
}

/// Write the packed cross-validation sample for the async path: the first
/// `count` blocks of `packed` in order. `lens` covers nblocks whole blocks
/// (the trailing tail entry is excluded — the verifier compares whole
/// blocks only).
fn write_sample(
    path: &str,
    block: usize,
    lens: &[usize],
    packed: &[u8],
    nblocks: usize,
) -> std::io::Result<()> {
    let count = sample_block_count(block, nblocks);
    let mut buf = Vec::with_capacity(packed.len().min(SAMPLE_CAP_BYTES + SAMPLE_SLACK_BYTES));
    push_sample_header(&mut buf, block, count);
    let mut off = 0usize;
    for l in lens.iter().take(count) {
        push_sample_block(&mut buf, *l, &packed[off..]);
        off += *l;
    }
    std::fs::write(path, buf)
}

/// The untimed warm-up pass shared by the sync and pool compress benches:
/// pre-faults pages, totals the compressed output (the ratio source), and
/// captures the cross-validation sample file. On failure the RESULT line is
/// printed here and the worker exit code is returned as Err.
fn warm_up_compress(
    a: &WorkerArgs,
    kae: &KaeLz4,
    data: &[u8],
    block: usize,
    nblocks: usize,
) -> Result<u64, i32> {
    let mut dst = vec![0u8; kae.bound(block)];
    let sample_blocks = sample_block_count(block, nblocks);
    let mut sample = Vec::with_capacity(SAMPLE_CAP_BYTES + SAMPLE_SLACK_BYTES);
    push_sample_header(&mut sample, block, sample_blocks);
    let mut out_bytes_warm: u64 = 0;
    for b in 0..nblocks {
        let n = match kae.compress(&data[b * block..(b + 1) * block], &mut dst) {
            Ok(n) => n,
            Err(e) => {
                println!(
                    "RESULT status=COMPRESS_FAIL impl={} op=compress block={} at={} err={}",
                    a.impl_name,
                    block,
                    b,
                    flat(&e.to_string())
                );
                return Err(EXIT_COMPRESS_FAIL);
            }
        };
        out_bytes_warm += n as u64;
        if b < sample_blocks {
            push_sample_block(&mut sample, n, &dst);
        }
    }
    if !a.sample_out.is_empty() {
        if let Err(e) = fs::write(&a.sample_out, &sample) {
            eprintln!("worker: write sample: {e}");
            return Err(EXIT_USAGE);
        }
    }
    Ok(out_bytes_warm)
}

fn bench_compress(
    a: &WorkerArgs,
    kae: &KaeLz4,
    data: &[u8],
    block: usize,
    bound: usize,
    nblocks: usize,
    note: &str,
) -> i32 {
    let out_bytes_warm = match warm_up_compress(a, kae, data, block, nblocks) {
        Ok(n) => n,
        Err(code) => return code,
    };
    let mut dst = vec![0u8; bound];

    // timed compress loop; a mid-loop failure fails the cell instead of
    // silently deflating the throughput numbers
    let mut compress_err: Option<String> = None;
    let st = timed_loop(
        nblocks,
        |b| {
            if compress_err.is_some() {
                return 0;
            }
            match kae.compress(&data[b * block..(b + 1) * block], &mut dst) {
                Ok(n) => n,
                Err(e) => {
                    compress_err = Some(e.to_string());
                    0
                }
            }
        },
        a.min_sec,
        a.passes_min,
        a.passes_max,
    );
    if let Some(e) = compress_err {
        println!(
            "RESULT status=COMPRESS_FAIL impl={} op=compress block={} at=timed_loop err={}",
            a.impl_name,
            block,
            flat(&e)
        );
        return EXIT_COMPRESS_FAIL;
    }
    emit_ok(OkResult {
        impl_name: &a.impl_name,
        op: "compress",
        block,
        lib: &a.lib,
        blocks: nblocks,
        st: &st,
        in_bytes: (nblocks as u64) * (block as u64) * st.passes,
        out_bytes: out_bytes_warm * st.passes,
        ratio_pct: out_bytes_warm as f64 / ((nblocks as u64) * (block as u64)) as f64 * 100.0,
        note,
    });
    EXIT_OK
}

/// What [`prepare_decompress`] hands the timed decompress loops: the
/// compressed blob, the per-block (offset, len) extents into it, and the
/// uncompressed byte total.
struct DecompressInput {
    blob: Vec<u8>,
    extents: Vec<(usize, usize)>,
    out_bytes: u64,
}

/// The untimed preparation shared by the sync and pool decompress benches:
/// compress all blocks into a contiguous blob (not counted in decompress
/// timing) and run the single-threaded warm-up self-check (this library
/// decompresses what it compressed). On failure the RESULT line is printed
/// here and the exit code comes back as Err.
fn prepare_decompress(
    a: &WorkerArgs,
    kae: &KaeLz4,
    data: &[u8],
    block: usize,
    bound: usize,
    nblocks: usize,
) -> Result<DecompressInput, i32> {
    // compress all blocks first (not counted in decompress timing)
    let mut blob: Vec<u8> = Vec::with_capacity(data.len() + nblocks * BOUND_SLACK_PER_BLOCK);
    let mut extents: Vec<(usize, usize)> = Vec::with_capacity(nblocks); // (off, len) into the compressed stream
    let mut tmp = vec![0u8; bound];
    let mut out_bytes: u64 = 0;
    for b in 0..nblocks {
        let n = match kae.compress(&data[b * block..(b + 1) * block], &mut tmp) {
            Ok(n) => n,
            Err(e) => {
                println!("RESULT status=COMPRESS_FAIL impl={} op=decompress block={} at={} err={}", a.impl_name, block, b, flat(&e.to_string()));
                return Err(EXIT_COMPRESS_FAIL);
            }
        };
        extents.push((blob.len(), n));
        blob.extend_from_slice(&tmp[..n]);
        out_bytes += block as u64;
    }

    // warm-up + self-check (this library decompresses what it compressed)
    let mut plain = vec![0u8; block];
    for b in 0..nblocks {
        let (off, len) = extents[b];
        let expect = &data[b * block..(b + 1) * block];
        match kae.decompress(&blob[off..off + len], &mut plain) {
            Ok(n) if n == block && plain.as_slice() == expect => {}
            other => {
                let err = other
                    .err()
                    .map(|e| flat(&e.to_string()))
                    .unwrap_or_else(|| "size_mismatch".into());
                println!("RESULT status=DECOMPRESS_MISMATCH impl={} op=decompress block={} at={} err={}", a.impl_name, block, b, err);
                return Err(EXIT_VERIFY_FAIL);
            }
        }
    }
    Ok(DecompressInput {
        blob,
        extents,
        out_bytes,
    })
}

fn bench_decompress(
    a: &WorkerArgs,
    kae: &KaeLz4,
    data: &[u8],
    block: usize,
    bound: usize,
    nblocks: usize,
    note: &str,
) -> i32 {
    let DecompressInput { blob, extents, out_bytes } =
        match prepare_decompress(a, kae, data, block, bound, nblocks)
        {
            Ok(v) => v,
            Err(code) => return code,
        };

    // timed decompress loop; throughput counts the decompressed bytes;
    // a mid-loop failure fails the cell instead of silently deflating them
    let mut plain = vec![0u8; block];
    let mut decompress_err: Option<String> = None;
    let st = timed_loop(
        nblocks,
        |b| {
            if decompress_err.is_some() {
                return 0;
            }
            let (off, len) = extents[b];
            match kae.decompress(&blob[off..off + len], &mut plain) {
                Ok(n) if n == block => n,
                Ok(_) => {
                    decompress_err = Some("size_mismatch".into());
                    0
                }
                Err(e) => {
                    decompress_err = Some(e.to_string());
                    0
                }
            }
        },
        a.min_sec,
        a.passes_min,
        a.passes_max,
    );
    if let Some(e) = decompress_err {
        println!("RESULT status=DECOMPRESS_MISMATCH impl={} op=decompress block={} at=timed_loop err={}", a.impl_name, block, flat(&e));
        return EXIT_VERIFY_FAIL;
    }
    let in_bytes = (nblocks as u64) * (block as u64) * st.passes; // decompressed bytes
    emit_ok(OkResult {
        impl_name: &a.impl_name,
        op: "decompress",
        block,
        lib: &a.lib,
        blocks: nblocks,
        st: &st,
        in_bytes,
        out_bytes,
        // the ratio is a compression metric; it stays 0 for decompress rows
        ratio_pct: 0.0,
        note,
    });
    EXIT_OK
}

/// The note for pool rows: the lib note (preload diagnostics, usually
/// empty) plus the requested pool size, so TSV/JSON rows carry the thread
/// count the numbers were produced with.
fn pool_note(lib_note: &str, workers: usize) -> String {
    if lib_note.is_empty() {
        format!("workers={workers}")
    } else {
        format!("{},workers={workers}", flat(lib_note))
    }
}

/// Pool compress (--workers >1 on sw/hw): the AgentENV zfile WorkerPool
/// shape at block granularity — `workers` threads, each compressing its
/// interleaved share of the file's blocks per sweep
/// (`block % workers == thread`) into its own dst buffer. Warm-up and
/// sample capture stay single-threaded (identical to the sync bench); only
/// the timed loop is pooled.
fn bench_pool_compress(
    a: &WorkerArgs,
    kae: &KaeLz4,
    data: &[u8],
    block: usize,
    bound: usize,
    nblocks: usize,
    note: &str,
) -> i32 {
    let out_bytes_warm = match warm_up_compress(a, kae, data, block, nblocks) {
        Ok(n) => n,
        Err(code) => return code,
    };
    let workers = a.workers.min(nblocks);
    // per-thread counters: thread t is the only writer of counts[t], so the
    // accounting adds no cross-core traffic to the timed loop
    let counts: Vec<AtomicU64> = (0..workers).map(|_| AtomicU64::new(0)).collect();
    let failed = AtomicBool::new(false);
    let first_err: Mutex<Option<String>> = Mutex::new(None);
    let st = timed_pool_loop(
        workers,
        nblocks,
        // each thread compresses into its own dst, created on the owning
        // thread (LZ4_compress writes, so buffers must not be shared)
        |_| vec![0u8; bound],
        |t, b, dst| {
            if failed.load(Ordering::Relaxed) {
                return 0;
            }
            match kae.compress(&data[b * block..(b + 1) * block], dst) {
                Ok(_) => {
                    counts[t].fetch_add(1, Ordering::Relaxed);
                    block
                }
                Err(e) => {
                    fail_cell(&first_err, &failed, e.to_string());
                    0
                }
            }
        },
        a.min_sec,
        a.passes_min,
        a.passes_max,
    );
    if let Some(e) = first_err.into_inner().unwrap() {
        println!(
            "RESULT status=COMPRESS_FAIL impl={} op=compress block={} at=timed_pool err={}",
            a.impl_name,
            block,
            flat(&e)
        );
        return EXIT_COMPRESS_FAIL;
    }
    let blocks_done: u64 = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    emit_ok(OkResult {
        impl_name: &a.impl_name,
        op: "compress",
        block,
        lib: &a.lib,
        blocks: nblocks,
        st: &st,
        in_bytes: blocks_done * block as u64,
        out_bytes: out_bytes_warm * blocks_done / nblocks as u64,
        ratio_pct: out_bytes_warm as f64 / ((nblocks as u64) * (block as u64)) as f64 * 100.0,
        note: &pool_note(note, a.workers),
    });
    EXIT_OK
}

/// Pool decompress (--workers >1 on sw/hw): the same pool shape for
/// decompression (software code in every backend). Pre-compression and the
/// warm-up self-check stay single-threaded, identical to the sync bench;
/// only the timed loop pools.
fn bench_pool_decompress(
    a: &WorkerArgs,
    kae: &KaeLz4,
    data: &[u8],
    block: usize,
    bound: usize,
    nblocks: usize,
    note: &str,
) -> i32 {
    let DecompressInput { blob, extents, out_bytes } =
        match prepare_decompress(a, kae, data, block, bound, nblocks)
        {
            Ok(v) => v,
            Err(code) => return code,
        };
    let workers = a.workers.min(nblocks);
    let counts: Vec<AtomicU64> = (0..workers).map(|_| AtomicU64::new(0)).collect();
    let failed = AtomicBool::new(false);
    let first_err: Mutex<Option<String>> = Mutex::new(None);
    let st = timed_pool_loop(
        workers,
        nblocks,
        // one scratch buffer per thread, created on the owning thread
        |_| vec![0u8; block],
        |t, b, plain| {
            if failed.load(Ordering::Relaxed) {
                return 0;
            }
            let (off, len) = extents[b];
            match kae.decompress(&blob[off..off + len], plain) {
                Ok(n) if n == block => {
                    counts[t].fetch_add(1, Ordering::Relaxed);
                    block
                }
                other => {
                    let err = other
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "size_mismatch".into());
                    fail_cell(&first_err, &failed, err);
                    0
                }
            }
        },
        a.min_sec,
        a.passes_min,
        a.passes_max,
    );
    if let Some(e) = first_err.into_inner().unwrap() {
        println!(
            "RESULT status=DECOMPRESS_MISMATCH impl={} op=decompress block={} at=timed_pool err={}",
            a.impl_name,
            block,
            flat(&e)
        );
        return EXIT_VERIFY_FAIL;
    }
    let blocks_done: u64 = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    emit_ok(OkResult {
        impl_name: &a.impl_name,
        op: "decompress",
        block,
        lib: &a.lib,
        blocks: nblocks,
        st: &st,
        in_bytes: blocks_done * block as u64, // decompressed bytes
        out_bytes,
        // the ratio is a compression metric; it stays 0 for decompress rows
        ratio_pct: 0.0,
        note: &pool_note(note, a.workers),
    });
    EXIT_OK
}

/// Decode the sample file with the current library (expected to be the
/// system liblz4) and compare block-by-block against the original input —
/// answers "can the community library decode hardware output, and is the
/// on-disk format unchanged".
fn verify_sample(a: &WorkerArgs, kae: &KaeLz4, data: &[u8], block: usize) -> i32 {
    let sample = match fs::read(&a.sample) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("worker: read sample: {e}");
            return EXIT_USAGE;
        }
    };
    let rd_u32 = |off: usize| -> u32 {
        u32::from_le_bytes([
            sample[off],
            sample[off + 1],
            sample[off + 2],
            sample[off + 3],
        ])
    };
    if sample.len() < 12 || rd_u32(0) != SAMPLE_MAGIC {
        println!("VERIFY status=BAD_SAMPLE sample={}", a.sample);
        return EXIT_USAGE;
    }
    let sblock = rd_u32(4) as usize;
    let count = rd_u32(8) as usize;
    if sblock != block {
        println!(
            "VERIFY status=BLOCK_MISMATCH sample_block={} expect={}",
            sblock, block
        );
        return EXIT_USAGE;
    }
    let mut off = 12;
    let mut plain = vec![0u8; block];
    let mut fail = 0usize;
    for i in 0..count {
        if off + 4 > sample.len() {
            fail += 1;
            break;
        }
        let clen = rd_u32(off) as usize;
        off += 4;
        if off + clen > sample.len() {
            fail += 1;
            break;
        }
        let expect = &data[i * block..(i + 1) * block];
        let ok = kae
            .decompress(&sample[off..off + clen], &mut plain)
            .map(|n| n == block && plain.as_slice() == expect)
            .unwrap_or(false);
        if !ok {
            fail += 1;
        }
        off += clen;
    }
    let status = if fail == 0 { "OK" } else { "FAIL" };
    println!(
        "VERIFY status={status} impl={} block={} blocks={} fail={}",
        a.impl_name, block, count, fail
    );
    if fail == 0 {
        EXIT_OK
    } else {
        EXIT_VERIFY_FAIL
    }
}
