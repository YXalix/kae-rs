//! Orchestrator: CLI entry point. Environment check -> generate data -> for
//! every (block, impl, op) combination re-execute itself to spawn a
//! worker subprocess -> parse RESULT/VERIFY into a summary table -> verdict.
//!
//! Why one subprocess per combination: the KAE patched liblz4 and the system
//! liblz4 share the SONAME liblz4.so.1, so successive dlopens in one process
//! silently hit whichever loaded first and software/hardware numbers would
//! pollute each other. Subprocess isolation is the only reliable approach,
//! and its cost (process cold start) is negligible at second-scale timing.
//!
//! This module is the harness for the bundled `kae-bench` bin and the
//! `cargo bench` target; it is published only so those targets can reach
//! [`main`] and is not part of the library's stable API.

use crate::data;
use crate::hw_lib_path;
use crate::uacce_devices;
use crate::worker;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_BLOCKS: &str = "4096,8192,65536,262144,1048576";
const OPS: [&str; 2] = ["compress", "decompress"];
/// Upper bound on --workers, matching the worker-side cap (the AgentENV
/// zfile WorkerPool cap).
const WORKERS_MAX: usize = 256;

/// Verdict thresholds (criteria are documented in docs/benchmark.md):
/// a >=1.1x sync speedup is an outright win, cpu <=50% of software counts as
/// offload value, and the large-block fallback needs >=1.5x.
const SPEEDUP_WIN: f64 = 1.1;
const CPU_OFFLOAD_RATIO: f64 = 0.5;
const LARGE_BLOCK_SPEEDUP: f64 = 1.5;
/// Words kept from a failing worker's stderr for the row note.
const STDERR_TAIL_WORDS: usize = 12;

#[derive(Default, Clone)]
struct Row {
    data: String,
    block: usize,
    impl_name: String,
    op: String,
    gbps: f64,
    ratio_pct: f64,
    cpu_per_gb: f64,
    wall_s: f64,
    verify: String,
    status: String,
    note: String,
}

impl Row {
    /// The verify column value: only compress rows carry one.
    fn verify_display(&self) -> String {
        if self.op == "compress" {
            self.verify.clone()
        } else {
            "-".into()
        }
    }
}

struct Config {
    input: String,
    size_mib: usize,
    blocks: Vec<usize>,
    impls: Vec<String>,
    ops: Vec<String>,
    workers: usize,
    sw_lib: String,
    hw_lib: String,
    hw_preload: Vec<String>,
    min_sec: f64,
    passes_min: u64,
    passes_max: u64,
    tsv: String,
    json: String,
    probe: bool,
    host: String,
}

fn usage() {
    println!(
        "kae-bench: KAE2 hardware LZ4 vs software LZ4 block compression benchmark (zfile granularity)

usage: kae-bench [options]
  --input FILE      benchmark against a real file (e.g. an exported VM memory image) instead of the synthetic mixed dataset
  --size MIB        synthetic dataset size, MiB (default 64)
  --blocks LIST     comma-separated block sizes (default {DEFAULT_BLOCKS})
  --impl LIST       implementations: sw,hw,hwasync,native (default: all present;
                    hwasync = segment-level async batch, compress only; native =
                    the crate's own uacce engine path (compress: the raw lz77
                    pump; decompress: in-crate software decode of the rebuilt
                    blocks), no C library in the process)
  --workers N       threads per sw/hw cell (default 1 = the sync drop-in); >1 turns sw/hw into
                    --workers-threaded block pools (the zfile WorkerPool shape) and sets hwasync's
                    library thread pool (KAE_LZ4_ASYNC_THREAD_NUM)
  --ops LIST        operations: compress,decompress (default both)
  --sw-lz4-lib P    software liblz4 path (default liblz4.so.1 via loader search)
  --hw-lz4-lib P    KAE patched liblz4 path (default: probe /usr/local/kaelz4/lib/)
  --min-sec F       minimum timed seconds per cell (default 0.8)
  --passes-min N    minimum passes per cell (default 3)    --passes-max N  maximum passes (default 99)
  --tsv PATH        results TSV (default ./kae_bench_results.tsv; empty string disables)
  --json PATH       additional JSON output
  --probe           only probe the environment/symbols, then exit
  -h, --help        this help

cargo bench equivalent: cargo bench -- [same options as above]"
    );
}

/// Comma-separated list with whitespace trimmed and empty entries dropped.
fn parse_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_impl_list(s: &str) -> Result<Vec<String>, String> {
    let impls = parse_list(s);
    for imp in &impls {
        match imp.as_str() {
            worker::IMPL_SW | worker::IMPL_HW | worker::IMPL_HWASYNC | worker::IMPL_NATIVE => {}
            other => {
                return Err(format!(
                    "unknown --impl {other} (expected sw|hw|hwasync|native)"
                ))
            }
        }
    }
    if impls.is_empty() {
        return Err("--impl parsed to an empty list".into());
    }
    Ok(impls)
}

/// Impls that need the patched liblz4 (skipped entirely when it is absent).
fn hw_backed(impl_name: &str) -> bool {
    impl_name == worker::IMPL_HW || impl_name == worker::IMPL_HWASYNC
}

/// The native cells need only the uacce device nodes (no patched library).
fn native_available() -> bool {
    !crate::uacce::zip_devices().is_empty()
}

/// The machine's hostname (the TSV header and the env report carry it).
fn read_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn parse_config(args: &[String]) -> Result<Config, String> {
    let mut c = Config {
        input: String::new(),
        size_mib: 64,
        blocks: DEFAULT_BLOCKS
            .split(',')
            .filter_map(|s| s.parse().ok())
            .collect(),
        impls: vec![
            worker::IMPL_SW.into(),
            worker::IMPL_HW.into(),
            worker::IMPL_HWASYNC.into(),
            worker::IMPL_NATIVE.into(),
        ],
        ops: OPS.iter().map(|s| s.to_string()).collect(),
        workers: 1,
        sw_lib: "liblz4.so.1".into(),
        hw_lib: String::new(),
        hw_preload: Vec::new(),
        min_sec: 0.8,
        passes_min: 3,
        passes_max: 99,
        tsv: "kae_bench_results.tsv".into(),
        json: String::new(),
        probe: false,
        host: read_hostname(),
    };
    let mut i = 0;
    let val = |i: &mut usize, name: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("missing value for {name}"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--input" => c.input = val(&mut i, "--input")?,
            "--size" => {
                c.size_mib = val(&mut i, "--size")?
                    .parse()
                    .map_err(|e| format!("--size: {e}"))?
            }
            "--blocks" => {
                let list = val(&mut i, "--blocks")?;
                c.blocks = list
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if c.blocks.is_empty() {
                    return Err("--blocks parsed to an empty list".into());
                }
            }
            "--impl" => c.impls = parse_impl_list(&val(&mut i, "--impl")?)?,
            "--ops" => {
                c.ops = parse_list(&val(&mut i, "--ops")?);
                for o in &c.ops {
                    if !OPS.contains(&o.as_str()) {
                        return Err(format!(
                            "unknown --ops {o} (expected one of: {})",
                            OPS.join(", ")
                        ));
                    }
                }
                if c.ops.is_empty() {
                    return Err("--ops parsed to an empty list".into());
                }
            }
            "--workers" => {
                c.workers = val(&mut i, "--workers")?
                    .parse()
                    .map_err(|e| format!("--workers: {e}"))?;
                if c.workers == 0 || c.workers > WORKERS_MAX {
                    return Err(format!("--workers must be 1..={WORKERS_MAX}"));
                }
            }
            "--sw-lz4-lib" => c.sw_lib = val(&mut i, "--sw-lz4-lib")?,
            "--hw-lz4-lib" => c.hw_lib = val(&mut i, "--hw-lz4-lib")?,
            "--min-sec" => {
                c.min_sec = val(&mut i, "--min-sec")?
                    .parse()
                    .map_err(|e| format!("--min-sec: {e}"))?
            }
            "--passes-min" => {
                c.passes_min = val(&mut i, "--passes-min")?
                    .parse()
                    .map_err(|e| format!("--passes-min: {e}"))?
            }
            "--passes-max" => {
                c.passes_max = val(&mut i, "--passes-max")?
                    .parse()
                    .map_err(|e| format!("--passes-max: {e}"))?
            }
            "--tsv" => c.tsv = val(&mut i, "--tsv")?,
            "--json" => c.json = val(&mut i, "--json")?,
            "--probe" => c.probe = true,
            // cargo bench appends libtest's customary flags to a harness=false binary; ignore them
            "--bench" | "--test" | "--nocapture" | "--exact" | "--include-ignored" | "--list" => {}
            "-h" | "--help" => {
                usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other} (see --help)")),
        }
        i += 1;
    }
    if !c.input.is_empty() && !Path::new(&c.input).is_file() {
        return Err(format!("--input file not found: {}", c.input));
    }
    if c.hw_lib.is_empty() {
        c.hw_lib = hw_lib_path().unwrap_or_default();
    }
    c.hw_preload = crate::existing_preload_paths();
    Ok(c)
}

fn first_line_with(stdout: &str, prefix: &str) -> Option<String> {
    stdout
        .lines()
        .find(|l| l.starts_with(prefix))
        .map(|l| l.to_string())
}

/// Parse a "k=v k=v ..." line (values contain no spaces; the worker side
/// already flattens err/note).
fn field(line: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=");
    for tok in line.split(' ') {
        if let Some(v) = tok.strip_prefix(&pat) {
            return Some(v.to_string());
        }
    }
    None
}

fn spawn_worker<I, S>(exe: &Path, args: I) -> (Option<String>, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new(exe).args(args).output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
            (Some(stdout), stderr)
        }
        Err(e) => (None, format!("spawn failed: {e}")),
    }
}

/// The stderr tail kept as the note of a WORKER_ERR row (space-flattened to
/// keep the k=v protocol intact).
fn stderr_tail(stderr: &str) -> String {
    stderr
        .split_whitespace()
        .take(STDERR_TAIL_WORDS)
        .collect::<Vec<_>>()
        .join("_")
}

/// One (block x impl x op) benchmark cell against the run's single input.
struct Cell<'a> {
    dataset: &'a str,
    data_file: &'a Path,
    block: usize,
    impl_name: &'a str,
    op: &'a str,
    sample_out: Option<&'a Path>,
}

fn run_row(exe: &Path, cfg: &Config, cell: &Cell<'_>) -> Row {
    // the native cells carry no library (their engine path is in-crate)
    let (lib, preload) = if cell.impl_name == worker::IMPL_NATIVE {
        (String::new(), String::new())
    } else if cell.impl_name == worker::IMPL_SW {
        (cfg.sw_lib.clone(), String::new())
    } else {
        (cfg.hw_lib.clone(), cfg.hw_preload.join(","))
    };
    let mut args: Vec<String> = vec![
        "--_worker".into(),
        "--lib".into(),
        lib,
        "--preload".into(),
        preload,
        "--input".into(),
        cell.data_file.display().to_string(),
        "--block".into(),
        cell.block.to_string(),
        "--op".into(),
        cell.op.into(),
        "--impl".into(),
        cell.impl_name.into(),
        "--min-sec".into(),
        cfg.min_sec.to_string(),
        "--passes-min".into(),
        cfg.passes_min.to_string(),
        "--passes-max".into(),
        cfg.passes_max.to_string(),
        "--workers".into(),
        cfg.workers.to_string(),
    ];
    if let Some(p) = cell.sample_out {
        args.push("--sample-out".into());
        args.push(p.display().to_string());
    }
    let (stdout, stderr) = spawn_worker(exe, &args);
    let mut row = Row {
        data: cell.dataset.into(),
        block: cell.block,
        impl_name: cell.impl_name.into(),
        op: cell.op.into(),
        ..Default::default()
    };
    match stdout.as_deref().and_then(|s| first_line_with(s, "RESULT")) {
        Some(line) => {
            row.status = field(&line, "status").unwrap_or_default();
            row.gbps = field(&line, "gbps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            row.ratio_pct = field(&line, "ratio_pct")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            row.cpu_per_gb = field(&line, "cpu_s_per_gb")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            row.wall_s = field(&line, "wall_s")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            row.note = field(&line, "note").unwrap_or_default();
            // failure lines carry at=/err= instead of note=; keep the cause
            // in the row (table, TSV, JSON) instead of dropping it
            if row.status != "OK" && row.note.is_empty() {
                row.note = ["at", "err"]
                    .iter()
                    .filter_map(|k| field(&line, k))
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            // the worker's stderr (hw status reports, diagnostics) is part
            // of the failure evidence
            if row.status != "OK" {
                let tail = stderr_tail(&stderr);
                if !tail.is_empty() {
                    row.note = if row.note.is_empty() {
                        tail
                    } else {
                        format!("{} ({tail})", row.note)
                    };
                }
            }
        }
        None => {
            row.status = "WORKER_ERR".into();
            row.note = stderr_tail(&stderr);
        }
    }
    row
}

fn run_verify(exe: &Path, cfg: &Config, data_file: &Path, block: usize, sample: &Path) -> String {
    let args: Vec<String> = vec![
        "--_worker".into(),
        "--lib".into(),
        cfg.sw_lib.clone(),
        "--input".into(),
        data_file.display().to_string(),
        "--block".into(),
        block.to_string(),
        "--op".into(),
        "verify".into(),
        "--impl".into(),
        worker::IMPL_SW.into(),
        "--sample".into(),
        sample.display().to_string(),
    ];
    let (stdout, stderr) = spawn_worker(exe, &args);
    match stdout.as_deref().and_then(|s| first_line_with(s, "VERIFY")) {
        Some(line) => field(&line, "status").unwrap_or_else(|| "PARSE_ERR".into()),
        None => format!("WORKER_ERR_{}", stderr_tail(&stderr)),
    }
}

fn env_report(cfg: &Config) {
    println!("== 1. Environment check ==");
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let kernel = read("/proc/sys/kernel/osrelease").trim().to_string();
    println!("host:           {}", cfg.host);
    println!("kernel:         {kernel}");
    let cpuinfo = read("/proc/cpuinfo");
    let model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .or_else(|| cpuinfo.lines().find(|l| l.starts_with("CPU part")))
        .map(|l| l.split(':').nth(1).unwrap_or("?").trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cpu:            {model}");
    let uacce = uacce_devices();
    println!(
        "uacce devices:  {}",
        if uacce.is_empty() {
            "none".into()
        } else {
            uacce.join(" ")
        }
    );
    println!("sw lib:         {}", cfg.sw_lib);
    if cfg.hw_lib.is_empty() {
        println!("hw lib:         NOT FOUND (hw combinations will be marked NO_LIB; install KAE2.0 or point --hw-lz4-lib at it)");
    } else {
        println!("hw lib:         {}", cfg.hw_lib);
        println!(
            "hw preload:     {}",
            if cfg.hw_preload.is_empty() {
                "-".into()
            } else {
                cfg.hw_preload.join(" ")
            }
        );
        if uacce.is_empty() {
            println!("!! warning: no /dev/hisi_zip-* device found; the patched library may silently fall back to software compression — hw rows may in fact be software.");
        }
    }
    println!();
}

fn probe(cfg: &Config) {
    println!("== Symbol probe ==");
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kae-bench"));
    for (label, lib, preload) in [
        (worker::IMPL_SW, cfg.sw_lib.clone(), String::new()),
        (
            worker::IMPL_HW,
            cfg.hw_lib.clone(),
            cfg.hw_preload.join(","),
        ),
    ] {
        if lib.is_empty() {
            println!("[{label}] lib not found, skipping");
            continue;
        }
        let args = ["--_worker", "--probe", "--lib", &lib, "--preload", &preload];
        let (stdout, _stderr) = spawn_worker(&exe, args);
        match stdout {
            Some(s) => {
                for line in s.lines() {
                    println!("[{label}] {line}");
                }
            }
            None => println!("[{label}] spawn failed"),
        }
    }
}

fn print_table(rows: &[Row]) {
    let headers = [
        "data", "block", "impl", "op", "GB/s", "ratio%", "cpu s/GB", "verify", "status",
    ];
    let mut table: Vec<Vec<String>> = vec![headers.iter().map(|s| s.to_string()).collect()];
    for r in rows {
        let verify = r.verify_display();
        // pool rows carry their size in note (workers=N); surface it in the
        // label so the table is self-describing (TSV/JSON keep the impl name)
        let impl_disp = match field(&r.note, "workers") {
            Some(w) => format!("{}×{}", r.impl_name, w),
            None => r.impl_name.clone(),
        };
        table.push(vec![
            r.data.clone(),
            r.block.to_string(),
            impl_disp,
            r.op.clone(),
            format!("{:.2}", r.gbps),
            format!("{:.1}", r.ratio_pct),
            format!("{:.2}", r.cpu_per_gb),
            verify,
            r.status.clone(),
        ]);
    }
    let mut widths = vec![0usize; headers.len()];
    for row in &table {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    for row in &table {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect();
        println!("  {}", line.join("  "));
    }
}

fn verdict(cfg: &Config, rows: &[Row]) {
    if cfg.workers > 1 {
        println!(
            "== Verdict (compress, sw/hw are {}-thread pools) ==",
            cfg.workers
        );
    } else {
        println!("== Verdict (compress) ==");
    }
    // one benchmark input per run; its row label is "input" or "mixed"
    let Some(dataset) = rows
        .iter()
        .find(|r| r.op == "compress" && r.status == "OK")
        .map(|r| r.data.as_str())
    else {
        println!("  no usable compress rows.");
        return;
    };
    let mut blocks: Vec<usize> = rows
        .iter()
        .filter(|r| r.data == dataset && r.op == "compress" && r.status == "OK")
        .map(|r| r.block)
        .collect();
    blocks.sort_unstable();
    blocks.dedup();
    for b in &blocks {
        let sw = rows.iter().find(|r| {
            r.data == dataset
                && r.block == *b
                && r.impl_name == worker::IMPL_SW
                && r.op == "compress"
                && r.status == "OK"
        });
        let hw = rows.iter().find(|r| {
            r.data == dataset
                && r.block == *b
                && r.impl_name == worker::IMPL_HW
                && r.op == "compress"
                && r.status == "OK"
        });
        match (sw, hw) {
            (Some(s), Some(h)) if s.gbps > 0.0 => {
                print!("  block {b:<7} speedup hw/sw = {:>5.2}x", h.gbps / s.gbps);
                if s.cpu_per_gb > 0.0 {
                    print!(
                        ", hw cpu = {:>3.0}% of sw",
                        h.cpu_per_gb / s.cpu_per_gb * 100.0
                    );
                }
                println!();
            }
            (Some(_), None) => println!("  block {b:<7} hw missing/failed"),
            _ => println!("  block {b:<7} sw missing"),
        }
    }
    let get = |b: usize, imp: &str| -> Option<&Row> {
        rows.iter().find(|r| {
            r.data == dataset
                && r.block == b
                && r.impl_name == imp
                && r.op == "compress"
                && r.status == "OK"
        })
    };
    if let (Some(s4), Some(h4)) = (get(4096, worker::IMPL_SW), get(4096, worker::IMPL_HW)) {
        if h4.gbps >= s4.gbps * SPEEDUP_WIN {
            println!(
                "  => 4K wins outright: the synchronous drop-in at zfile granularity is worth it."
            );
        } else if s4.cpu_per_gb > 0.0 && h4.cpu_per_gb <= s4.cpu_per_gb * CPU_OFFLOAD_RATIO {
            println!("  => 4K CPU OFFLOAD: throughput flat or worse but CPU cost halved or better — worth it where host CPU is contended.");
        } else if let (Some(s1m), Some(h1m)) =
            (get(1048576, worker::IMPL_SW), get(1048576, worker::IMPL_HW))
        {
            if h1m.gbps >= s1m.gbps * LARGE_BLOCK_SPEEDUP {
                println!("  => small blocks lose, large blocks win: consider larger blocks or the segment-level async batch pool.");
            } else {
                println!("  => NO-GO for this hardware at this granularity.");
            }
        } else {
            println!("  => no 4K advantage and no 1M comparison row; treating as NO-GO.");
        }
    } else {
        println!("  => missing the 4K sw/hw comparison rows; no verdict possible.");
    }
    println!(
        "  criteria: speedup>{} is an outright win; flat speedup with cpu<={}% counts as offload value;",
        SPEEDUP_WIN,
        CPU_OFFLOAD_RATIO * 100.0
    );
    println!("        otherwise look at the large-block trend. cpu s/GB is user+sys, excluding wait time.");
}

fn write_tsv(path: &str, cfg: &Config, rows: &[Row]) {
    let mut s = format!("# kae-bench LZ4 host={}\n", cfg.host);
    s.push_str(
        "data\tblock\timpl\top\tgbps\tratio_pct\tcpu_s_per_gb\twall_s\tverify\tstatus\tnote\n",
    );
    for r in rows {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{:.3}\t{:.2}\t{:.3}\t{:.3}\t{}\t{}\t{}\n",
            r.data,
            r.block,
            r.impl_name,
            r.op,
            r.gbps,
            r.ratio_pct,
            r.cpu_per_gb,
            r.wall_s,
            r.verify_display(),
            r.status,
            r.note
        ));
    }
    if let Err(e) = std::fs::write(path, s) {
        eprintln!("failed to write TSV {path}: {e}");
    }
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn write_json(path: &str, rows: &[Row]) {
    let mut s = String::from("[\n");
    for (i, r) in rows.iter().enumerate() {
        s.push_str(&format!(
            "  {{\"data\": \"{}\", \"block\": {}, \"impl\": \"{}\", \"op\": \"{}\", \"gbps\": {:.3}, \"ratio_pct\": {:.2}, \"cpu_s_per_gb\": {:.3}, \"wall_s\": {:.3}, \"verify\": \"{}\", \"status\": \"{}\", \"note\": \"{}\"}}{}\n",
            json_escape(&r.data), r.block, json_escape(&r.impl_name), json_escape(&r.op),
            r.gbps, r.ratio_pct, r.cpu_per_gb, r.wall_s,
            json_escape(&r.verify), json_escape(&r.status), json_escape(&r.note),
            if i + 1 < rows.len() { "," } else { "" }
        ));
    }
    s.push_str("]\n");
    if let Err(e) = std::fs::write(path, s) {
        eprintln!("failed to write JSON {path}: {e}");
    }
}

/// A private scratch dir for the synthetic dataset and cross-validation
/// samples, unique per run.
fn make_tmp_dir() -> Result<PathBuf, String> {
    let tmp = std::env::temp_dir().join(format!(
        "kae-bench.{}.{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("failed to create temp dir: {e}"))?;
    Ok(tmp)
}

/// Section 2: resolve the benchmark input — a real file via `--input`, or
/// the generated synthetic mixed dataset.
fn prepare_data(cfg: &Config, tmp: &Path) -> Result<(String, PathBuf), String> {
    if !cfg.input.is_empty() {
        println!("== 2. Input: {} ==", cfg.input);
        return Ok(("input".into(), PathBuf::from(&cfg.input)));
    }
    println!("== 2. Synthetic data ({} MiB, mixed) ==", cfg.size_mib);
    let p = tmp.join("mixed.bin");
    data::generate(cfg.size_mib, &p)
        .map_err(|e| format!("failed to generate {}: {e}", p.display()))?;
    println!("  generated {}", p.display());
    Ok(("mixed".into(), p))
}

/// Section 3: the full (block x impl x op) scan, one worker subprocess per
/// cell, plus the stock-liblz4 cross-verification of every compress cell
/// that produced a sample.
fn scan(exe: &Path, cfg: &Config, dataset: &str, data_file: &Path, tmp: &Path) -> Vec<Row> {
    println!(
        "== 3. Scan (adaptive timing >= {:.1}s per cell, subprocess isolation of sw/hw) ==",
        cfg.min_sec
    );
    let impls: Vec<&String> = cfg
        .impls
        .iter()
        .filter(|i| !hw_backed(i.as_str()) || !cfg.hw_lib.is_empty())
        .filter(|i| i.as_str() != worker::IMPL_NATIVE || native_available())
        .collect();
    if cfg.impls.iter().any(|i| hw_backed(i.as_str())) && cfg.hw_lib.is_empty() {
        println!("  (hw lib not found; hw/hwasync combinations skipped)");
    }
    if cfg.impls.iter().any(|i| i == worker::IMPL_NATIVE) && !native_available() {
        println!("  (no /dev/hisi_zip-* device; native combinations skipped)");
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut sample_idx = 0usize;
    for block in &cfg.blocks {
        for impl_name in &impls {
            for op in &cfg.ops {
                let sample_out = if op == "compress" {
                    sample_idx += 1;
                    Some(tmp.join(format!("sample.{sample_idx}.lz4")))
                } else {
                    None
                };
                let cell = Cell {
                    dataset,
                    data_file,
                    block: *block,
                    impl_name,
                    op,
                    sample_out: sample_out.as_deref(),
                };
                let mut row = run_row(exe, cfg, &cell);
                // verify whenever the worker wrote a cross-validation
                // sample (native lz77 rebuilds one from its triples; the
                // deflate dialect and block shapes that are not request
                // multiples legitimately skip it)
                if row.status == "OK" && cell.op == "compress" {
                    let sample_path = cell
                        .sample_out
                        .expect("compress cells always get a sample path");
                    if sample_path.exists() {
                        row.verify = run_verify(exe, cfg, cell.data_file, cell.block, sample_path);
                    }
                }
                let vdisp = if row.verify.is_empty() {
                    "-".into()
                } else {
                    row.verify.clone()
                };
                if row.status == "OK" {
                    println!(
                        "  {} {} {} {} -> {:.2} GB/s, ratio {:.1}%, cpu {:.2} s/GB, verify {} [{}]",
                        dataset, block, impl_name, op, row.gbps, row.ratio_pct, row.cpu_per_gb, vdisp, row.status
                    );
                } else {
                    println!(
                        "  {} {} {} {} -> [{}] {}",
                        dataset, block, impl_name, op, row.status, row.note
                    );
                }
                rows.push(row);
            }
        }
    }
    println!();
    rows
}

/// Section 4: summary table, verdict, and the TSV/JSON result files.
fn summarize(cfg: &Config, rows: &[Row]) {
    println!("== 4. Summary ==");
    print_table(rows);
    println!();
    verdict(cfg, rows);
    if !cfg.tsv.is_empty() {
        write_tsv(&cfg.tsv, cfg, rows);
        println!("TSV:  {}", cfg.tsv);
    }
    if !cfg.json.is_empty() {
        write_json(&cfg.json, rows);
        println!("JSON: {}", cfg.json);
    }
}

pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(|s| s.as_str()) == Some("--_worker") {
        return worker::run(&argv[1..]);
    }
    let cfg = match parse_config(&argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kae-bench: {e}");
            return 1;
        }
    };
    env_report(&cfg);
    if cfg.probe {
        probe(&cfg);
        return 0;
    }

    let tmp = match make_tmp_dir() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let (dataset, data_file) = match prepare_data(&cfg, &tmp) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            let _ = std::fs::remove_dir_all(&tmp);
            return 1;
        }
    };

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kae-bench"));
    let rows = scan(&exe, &cfg, &dataset, &data_file, &tmp);
    summarize(&cfg, &rows);
    let _ = std::fs::remove_dir_all(&tmp);
    0
}
