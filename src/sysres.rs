//! Minimal FFI for getrusage / clock_gettime: the bench's core is not just
//! wall-clock throughput but also the "hardware offload saves CPU" criterion
//! (user+sys time).

use std::os::raw::c_int;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rusage {
    ru_utime: Timeval,
    ru_stime: Timeval,
    ru_maxrss: i64,
    ru_ixrss: i64,
    ru_idrss: i64,
    ru_isrss: i64,
    ru_minflt: i64,
    ru_majflt: i64,
    ru_nswap: i64,
    ru_inblock: i64,
    ru_oublock: i64,
    ru_msgsnd: i64,
    ru_msgrcv: i64,
    ru_nsignals: i64,
    ru_nvcsw: i64,
    ru_nivcsw: i64,
}

extern "C" {
    fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
    fn clock_gettime(clk_id: c_int, tp: *mut Timespec) -> c_int;
}

const RUSAGE_SELF: c_int = 0;
const CLOCK_MONOTONIC: c_int = 1;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Per-process CPU time from getrusage, split by user/sys.
#[derive(Default, Clone, Copy)]
pub struct CpuTime {
    pub user_s: f64,
    pub sys_s: f64,
}

/// Monotonic wall-clock seconds (CLOCK_MONOTONIC).
pub fn now_s() -> f64 {
    let mut ts = Timespec::default();
    let r = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(r, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

/// Current per-process CPU time.
pub fn cpu_time() -> CpuTime {
    let mut ru = Rusage::default();
    let r = unsafe { getrusage(RUSAGE_SELF, &mut ru) };
    assert_eq!(r, 0, "getrusage(RUSAGE_SELF) failed");
    let t = |tv: &Timeval| tv.tv_sec as f64 + tv.tv_usec as f64 * 1e-6;
    CpuTime {
        user_s: t(&ru.ru_utime),
        sys_s: t(&ru.ru_stime),
    }
}

/// Accumulated timing of one adaptive loop.
pub struct LoopStats {
    /// Wall-clock seconds the whole loop took.
    pub wall_s: f64,
    /// User CPU seconds consumed inside the loop.
    pub user_s: f64,
    /// System CPU seconds consumed inside the loop.
    pub sys_s: f64,
    /// Completed passes over all nblocks items.
    pub passes: u64,
}

impl LoopStats {
    /// user+sys CPU seconds — the hardware-offload criterion.
    pub fn cpu_s(&self) -> f64 {
        self.user_s + self.sys_s
    }
}

/// Adaptive timing loop: calls `f(0..nblocks)` per pass, stopping after at
/// least `passes_min` passes and `min_sec` seconds; the `passes_max` cap
/// keeps a very slow implementation from stalling the scan.
pub fn timed_loop<F>(
    nblocks: usize,
    mut f: F,
    min_sec: f64,
    passes_min: u64,
    passes_max: u64,
) -> LoopStats
where
    F: FnMut(usize) -> usize,
{
    let cpu0 = cpu_time();
    let t0 = now_s();
    let mut passes: u64 = 0;
    loop {
        for b in 0..nblocks {
            // black_box keeps the compiler from eliding the timed work even
            // when the result is unused
            std::hint::black_box(f(b));
        }
        passes += 1;
        let elapsed = now_s() - t0;
        if (passes >= passes_min && elapsed >= min_sec) || passes >= passes_max {
            break;
        }
    }
    let wall_s = now_s() - t0;
    let cpu1 = cpu_time();
    LoopStats {
        wall_s,
        user_s: cpu1.user_s - cpu0.user_s,
        sys_s: cpu1.sys_s - cpu0.sys_s,
        passes,
    }
}
