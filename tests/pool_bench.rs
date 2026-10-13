//! Pool-mode bench contracts (--workers >1 on sw/hw) through the worker
//! line protocol. Everything runs against the system liblz4 so the suite
//! stays green on hardware-less machines: for a directly invoked worker the
//! backend label only decides which library is opened, and the pool code
//! path is identical for both labels.

use std::path::{Path, PathBuf};
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_kae-bench");
const BLOCK: usize = 4096;
const BLOCKS: usize = 64; // 256 KiB input

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

fn worker(args: &[&str]) -> Out {
    let o = Command::new(EXE).args(args).output().expect("spawn kae-bench");
    Out {
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        code: o.status.code().unwrap_or(-1),
    }
}

/// The k=v token of the single RESULT line (mirrors the worker protocol).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=");
    line.split(' ').find_map(|t| t.strip_prefix(&pat))
}

fn result_line(stdout: &str) -> String {
    stdout
        .lines()
        .find(|l| l.starts_with("RESULT"))
        .expect("worker prints a RESULT line")
        .to_string()
}

/// Deterministic input: half zero pages, half xorshift noise (compressible
/// ratio strictly between 0 and 100).
fn make_input(dir: &Path) -> PathBuf {
    let mut v = vec![0u8; BLOCK * BLOCKS];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for chunk in v[BLOCK * BLOCKS / 2..].chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    let p = dir.join("pool_input.bin");
    std::fs::write(&p, &v).expect("write input");
    p
}

fn tmp_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("kae-pool-test.{}.{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

#[test]
fn pool_compress_and_decompress_report_pool_numbers() {
    let dir = tmp_dir();
    let input = make_input(&dir);
    for impl_name in ["sw", "hw"] {
        for op in ["compress", "decompress"] {
            let o = worker(&[
                "--_worker",
                "--lib",
                "liblz4.so.1",
                "--preload",
                "",
                "--input",
                input.to_str().unwrap(),
                "--block",
                "4096",
                "--op",
                op,
                "--impl",
                impl_name,
                "--data",
                "pool",
                "--workers",
                "3",
                "--min-sec",
                "0.05",
                "--passes-min",
                "2",
                "--passes-max",
                "1000",
            ]);
            assert_eq!(o.code, 0, "{impl_name} {op}: {}", o.stderr);
            let line = result_line(&o.stdout);
            assert_eq!(field(&line, "status"), Some("OK"), "{line}");
            assert_eq!(field(&line, "impl"), Some(impl_name));
            assert_eq!(field(&line, "note"), Some("workers=3"), "{line}");
            // accounting counts actual blocks: at least passes_min full
            // passes over all blocks
            let in_bytes: u64 = field(&line, "in_bytes").unwrap().parse().unwrap();
            assert!(
                in_bytes >= (BLOCK * BLOCKS * 2) as u64,
                "{impl_name} {op}: {line}"
            );
            if op == "compress" {
                let ratio: f64 = field(&line, "ratio_pct").unwrap().parse().unwrap();
                assert!((0.0..100.0).contains(&ratio), "{line}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workers_default_1_stays_on_the_sync_path() {
    let dir = tmp_dir();
    let input = make_input(&dir);
    // no --workers flag: the sync bench, whose note never carries workers=N
    let o = worker(&[
        "--_worker",
        "--lib",
        "liblz4.so.1",
        "--input",
        input.to_str().unwrap(),
        "--block",
        "4096",
        "--op",
        "compress",
        "--impl",
        "sw",
        "--data",
        "pool",
        "--min-sec",
        "0.05",
        "--passes-min",
        "1",
        "--passes-max",
        "10",
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let line = result_line(&o.stdout);
    assert_eq!(field(&line, "status"), Some("OK"), "{line}");
    assert_eq!(field(&line, "note"), Some(""), "sync rows have no note: {line}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workers_out_of_bounds_is_a_usage_error() {
    let dir = tmp_dir();
    let input = make_input(&dir);
    for w in ["0", "257"] {
        let o = worker(&[
            "--_worker",
            "--lib",
            "liblz4.so.1",
            "--input",
            input.to_str().unwrap(),
            "--block",
            "4096",
            "--op",
            "compress",
            "--impl",
            "sw",
            "--data",
            "pool",
            "--workers",
            w,
            "--min-sec",
            "0.05",
            "--passes-min",
            "1",
            "--passes-max",
            "10",
        ]);
        assert_eq!(o.code, 1, "--workers {w} must be a usage error");
        assert!(
            o.stderr.contains("workers"),
            "stderr should name the flag: {}",
            o.stderr
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn orchestrator_emits_pool_rows_with_cross_validation() {
    let dir = tmp_dir();
    let tsv = dir.join("out.tsv");
    let o = worker(&[
        "--size",
        "1",
        "--blocks",
        "4096",
        "--data",
        "zeros",
        "--impl",
        "sw",
        "--workers",
        "2",
        "--ops",
        "compress",
        "--min-sec",
        "0.05",
        "--passes-min",
        "1",
        "--passes-max",
        "50",
        "--tsv",
        tsv.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let body = std::fs::read_to_string(&tsv).expect("tsv written");
    let row = body
        .lines()
        .find(|l| l.starts_with("zeros\t"))
        .expect("a zeros sw row");
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(cols[2], "sw", "the impl column keeps the raw name: {row}");
    assert_eq!(cols[9], "OK", "status column: {row}");
    assert_eq!(cols[10], "workers=2", "note column: {row}");
    // compress rows cross-validate the sample through the system liblz4
    assert_eq!(cols[8], "OK", "verify column: {row}");
    let _ = std::fs::remove_dir_all(&dir);
}
