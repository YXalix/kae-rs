//! `--_worker`: a subprocess spawned by the orchestrator by re-executing
//! itself; it opens exactly one liblz4 implementation at a time (avoiding
//! the patched/system same-SONAME conflict), goes through the public
//! `KaeLz4::open_path` API, and prints single-line RESULT/VERIFY/PROBE
//! protocol records to stdout for the parent to parse and aggregate. Not
//! for manual invocation.
//!
//! Throughput accounting: compress is "raw input bytes / wall clock",
//! decompress is "decompressed raw bytes / wall clock"; CPU cost is
//! getrusage(user+sys) normalized per GB of input — the criterion for a
//! hardware offload is a significant drop in CPU/GB, not necessarily a
//! faster wall clock.

use crate::lz4::PROBE_SYMBOLS;
use crate::sysres::{timed_loop, LoopStats};
use crate::{AsyncBatch, Backend, KaeLz4};
use std::fs;

pub(crate) const IMPL_SW: &str = "sw";
pub(crate) const IMPL_HW: &str = "hw";
pub(crate) const IMPL_HWASYNC: &str = "hwasync";

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
            "--data" => a.data = get(&mut i, args, "--data")?,
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
            "--probe" => a.probe = true,
            other => return Err(format!("worker: unknown arg {other}")),
        }
        i += 1;
    }
    if a.lib.is_empty() {
        return Err("worker: --lib required".into());
    }
    // probe mode never passes --impl
    if !a.probe {
        match a.impl_name.as_str() {
            IMPL_SW | IMPL_HW | IMPL_HWASYNC => {}
            "" => return Err("worker: --impl required".into()),
            other => {
                return Err(format!(
                    "worker: unknown --impl {other} (expected sw|hw|hwasync)"
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
    data: String,
    sample_out: String,
    sample: String,
    min_sec: f64,
    passes_min: u64,
    passes_max: u64,
    probe: bool,
}

/// Open for probe mode: the bare `Lib` is needed to call has_symbol, which
/// KaeLz4 does not expose; the direct-open-then-preload-retry logic itself
/// is the shared `open_lib`.
fn open_raw(lib: &str, preload: &str) -> Result<crate::loader::Lib, String> {
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
    match open_raw(&a.lib, &a.preload) {
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
    data: &'a str,
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
        "RESULT status=OK impl={} op={} data={} block={} lib={} blocks={} passes={} in_bytes={} out_bytes={} ratio_pct={:.2} wall_s={:.3} gbps={:.2} user_s={:.3} sys_s={:.3} cpu_s_per_gb={:.2} note={}",
        r.impl_name, r.op, r.data, r.block, r.lib, r.blocks, r.st.passes, r.in_bytes, r.out_bytes,
        r.ratio_pct, r.st.wall_s, gbps, r.st.user_s, r.st.sys_s, cpu_per_gb, flat(r.note),
    );
}

/// hwasync: one compress_segment = a batch submission of the whole segment's
/// n blocks; the accounting matches sync compress (raw input bytes / wall
/// clock + user+sys).
fn bench_async_compress(a: &WorkerArgs, data: &[u8], block: usize, nblocks: usize) -> i32 {
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
                "RESULT status=ASYNC_FAIL impl=hwasync op=compress data={} block={} err={}",
                a.data,
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
            "RESULT status=ASYNC_FAIL impl=hwasync op=compress data={} block={} err={}",
            a.data,
            block,
            flat(&e)
        );
        return EXIT_COMPRESS_FAIL;
    }
    emit_ok(OkResult {
        impl_name: IMPL_HWASYNC,
        op: "compress",
        data: &a.data,
        block,
        lib: &a.lib,
        blocks: nblocks,
        st: &st,
        in_bytes: in_bytes_one * st.passes,
        out_bytes: out_bytes_warm * st.passes,
        ratio_pct: out_bytes_warm as f64 / in_bytes_one as f64 * 100.0,
        note: "batch",
    });
    EXIT_OK
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
    let count = (SAMPLE_CAP_BYTES / block).max(1).min(nblocks);
    let mut buf = Vec::with_capacity(packed.len().min(SAMPLE_CAP_BYTES + SAMPLE_SLACK_BYTES));
    push_sample_header(&mut buf, block, count);
    let mut off = 0usize;
    for l in lens.iter().take(count) {
        push_sample_block(&mut buf, *l, &packed[off..]);
        off += *l;
    }
    std::fs::write(path, buf)
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
    let mut dst = vec![0u8; bound];

    // one warm-up pass: pre-fault pages, compute the ratio, capture the
    // cross-validation sample
    let sample_blocks = (SAMPLE_CAP_BYTES / block).max(1).min(nblocks);
    let mut sample = Vec::with_capacity(SAMPLE_CAP_BYTES + SAMPLE_SLACK_BYTES);
    push_sample_header(&mut sample, block, sample_blocks);
    let mut out_bytes_warm: u64 = 0;
    for b in 0..nblocks {
        let n = match kae.compress(&data[b * block..(b + 1) * block], &mut dst) {
            Ok(n) => n,
            Err(e) => {
                println!(
                    "RESULT status=COMPRESS_FAIL impl={} op=compress data={} block={} at={} err={}",
                    a.impl_name,
                    a.data,
                    block,
                    b,
                    flat(&e.to_string())
                );
                return EXIT_COMPRESS_FAIL;
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
            return EXIT_USAGE;
        }
    }

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
            "RESULT status=COMPRESS_FAIL impl={} op=compress data={} block={} at=timed_loop err={}",
            a.impl_name,
            a.data,
            block,
            flat(&e)
        );
        return EXIT_COMPRESS_FAIL;
    }
    emit_ok(OkResult {
        impl_name: &a.impl_name,
        op: "compress",
        data: &a.data,
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

fn bench_decompress(
    a: &WorkerArgs,
    kae: &KaeLz4,
    data: &[u8],
    block: usize,
    bound: usize,
    nblocks: usize,
    note: &str,
) -> i32 {
    // compress all blocks first (not counted in decompress timing)
    let mut blob: Vec<u8> = Vec::with_capacity(data.len() + nblocks * BOUND_SLACK_PER_BLOCK);
    let mut extents: Vec<(usize, usize)> = Vec::with_capacity(nblocks); // (off, len) into the compressed stream
    let mut tmp = vec![0u8; bound];
    let mut out_bytes: u64 = 0;
    for b in 0..nblocks {
        let n = match kae.compress(&data[b * block..(b + 1) * block], &mut tmp) {
            Ok(n) => n,
            Err(e) => {
                println!("RESULT status=COMPRESS_FAIL impl={} op=decompress data={} block={} at={} err={}", a.impl_name, a.data, block, b, flat(&e.to_string()));
                return EXIT_COMPRESS_FAIL;
            }
        };
        extents.push((blob.len(), n));
        blob.extend_from_slice(&tmp[..n]);
        out_bytes += block as u64;
    }

    // warm-up + self-check (this library decompresses what it compressed)
    let mut raw = vec![0u8; block];
    for b in 0..nblocks {
        let (off, len) = extents[b];
        let expect = &data[b * block..(b + 1) * block];
        match kae.decompress(&blob[off..off + len], &mut raw) {
            Ok(n) if n == block && raw.as_slice() == expect => {}
            other => {
                let err = other
                    .err()
                    .map(|e| flat(&e.to_string()))
                    .unwrap_or_else(|| "size_mismatch".into());
                println!("RESULT status=DECOMPRESS_MISMATCH impl={} op=decompress data={} block={} at={} err={}", a.impl_name, a.data, block, b, err);
                return EXIT_VERIFY_FAIL;
            }
        }
    }

    // timed decompress loop; throughput counts the decompressed raw bytes;
    // a mid-loop failure fails the cell instead of silently deflating them
    let mut decompress_err: Option<String> = None;
    let st = timed_loop(
        nblocks,
        |b| {
            if decompress_err.is_some() {
                return 0;
            }
            let (off, len) = extents[b];
            match kae.decompress(&blob[off..off + len], &mut raw) {
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
        println!("RESULT status=DECOMPRESS_MISMATCH impl={} op=decompress data={} block={} at=timed_loop err={}", a.impl_name, a.data, block, flat(&e));
        return EXIT_VERIFY_FAIL;
    }
    let in_bytes = (nblocks as u64) * (block as u64) * st.passes; // decompressed bytes
    emit_ok(OkResult {
        impl_name: &a.impl_name,
        op: "decompress",
        data: &a.data,
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
    let mut raw = vec![0u8; block];
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
            .decompress(&sample[off..off + clen], &mut raw)
            .map(|n| n == block && raw.as_slice() == expect)
            .unwrap_or(false);
        if !ok {
            fail += 1;
        }
        off += clen;
    }
    let status = if fail == 0 { "OK" } else { "FAIL" };
    println!(
        "VERIFY status={status} impl={} data={} block={} blocks={} fail={}",
        a.impl_name, a.data, block, count, fail
    );
    if fail == 0 {
        EXIT_OK
    } else {
        EXIT_VERIFY_FAIL
    }
}
