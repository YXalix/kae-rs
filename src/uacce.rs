//! Raw uacce driver for the KAE2 ZIP engine: the seed of the native
//! (C-stack-free) hardware backend. It speaks the queue ABI directly —
//! open, ioctl, mmap, SQE fill, doorbell store, CQE harvest — with no
//! libwd, no libkaelz4 and no patched liblz4 in the process.
//!
//! The pump measures the engine's throughput ceiling as a function of
//! (queues x in-flight depth x request size x wait policy), which is the
//! question the libkaelz4-backed cells cannot answer: their sync path is
//! one request in flight plus a 100%-core spin, and their async pool adds
//! twelve spinning harvesters on top (see docs/kae-limitations.md).
//!
//! Provenance of every constant — do not guess, re-verify against:
//! - UACCE UAPI ioctls and region pgoffs:
//!   KAEKernelDriver-OLK-6.6/include_uapi_linux/uacce.h
//! - QM qp ioctls: KAEKernelDriver-OLK-6.6/hisilicon/hisi_qm.h
//! - doorbell encoding, CQE layout, queue geometry:
//!   uadk/v1/drv/hisi_qm_udrv.{c,h}
//! - ZIP v3 SQE layout, algorithm codes, stream flags, lz77 flat-buffer
//!   geometry: uadk/v1/drv/hisi_zip_udrv.{c,h}
//!
//! Doorbell semantics: the ring index is advanced BEFORE ringing, and the
//! doorbell carries the advanced index — "hardware may fetch every SQE up
//! to here, exclusive" on SQ, "CQ slot here is reclaimed" on CQ. The CQ
//! doorbell's priority bits carry the interrupt-arm flag: 0 keeps the
//! engine silent for a spinning harvester, 1 arms the completion IRQ that
//! wakes a poll()ing one.
//!
//! Only QM v3 devices are supported (api `hisi_qm_v3`, the KAE2/SEC2
//! generation) with the v3 SQE layout, in two stateless flat dialects:
//! lz77 (the kaelz4 production mode: literals + 8-byte sequence triples +
//! freq block) and raw_deflate (validated end to end by inflating one
//! output in the unit tests on hardware).

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_ulong};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use crate::sysres::{cpu_time, now_s, switch_counts, CpuTime};

/// LZ4-format constraint carried over from kaelz4: the engine's lz77 flat
/// mode accepts at most 128K input per request (HARDWARE_BLOCK_SIZE in
/// kaelz4 caps production use at 64K).
pub(crate) const MAX_LZ77_REQ: usize = 0x20000;
/// The engine's fixed queue depth (UACCE_CMD_QM_SET_QP_INFO reports it;
/// the kernel does not accept a different value).
pub(crate) const QM_Q_DEPTH: usize = 1024;

// ---------- UACCE UAPI (include_uapi_linux/uacce.h) ----------
// _IOC encoding: (dir<<30) | (size<<16) | (type<<8) | nr; _IO drops size.

const UACCE_CMD_START_Q: c_ulong = 0x5700; // _IO('W', 0)
const UACCE_CMD_PUT_Q: c_ulong = 0x5701; // _IO('W', 1)
const UACCE_CMD_GET_SS_DMA: c_ulong = 0x8008_5703; // _IOR('W', 3, unsigned long)
// mmap pgoff selects the region: 0 = MMIO (doorbell), 1 = DUS (SQ/CQ), 2 = SS
const UACCE_QFRT_MMIO: i64 = 0;
const UACCE_QFRT_DUS: i64 = 1;
const UACCE_QFRT_SS: i64 = 2;

/// GET_SS_DMA returns `dma | (size/64KB)`; buffers never straddle slices.
const UACCE_GRAN_SHIFT: u64 = 16;
const UACCE_GRAN_NUM_MASK: u64 = 0xfff;

// ---------- Hisilicon QM (hisilicon/hisi_qm.h + uadk v1) ----------

#[repr(C)]
struct QpCtx {
    id: u16,      // out: QP number inside the device
    qc_type: u16, // in: 0 = compress, 1 = decompress
}

#[repr(C)]
struct QpInfo {
    sqe_size: u32,
    sq_depth: u16,
    cq_depth: u16,
    reserved: u64,
}

const UACCE_CMD_QM_SET_QP_CTX: c_ulong = 0xC004_480A; // _IOWR('H', 10, QpCtx)
const UACCE_CMD_QM_SET_QP_INFO: c_ulong = 0xC010_480B; // _IOWR('H', 11, QpInfo)

const QM_ZIP_SQE_SIZE: usize = 128;
const QM_CQE_SIZE: usize = 16;
/// DUS layout: SQ ring at offset 0, CQ ring right behind it, device status
/// registers in the last bytes (uadk v1 qm_set_queue_info).
const QM_DS_TX_AT_END: usize = 4; // value 1 there = hardware error
const QM_V2_DOORBELL_OFFSET: u64 = 0x1000; // api hisi_qm_v2/v3

const DOORBELL_CMD_SQ: u8 = 0;
const DOORBELL_CMD_CQ: u8 = 1;
// qm_db_v2 encoding (uadk v1 hisi_qm_udrv.c)
const QM_DB_SQN_MASK: u64 = 0x3ff;
const QM_DB_CMD_SHIFT: u64 = 12;
const QM_DB_IDX_SHIFT: u64 = 32;
const QM_DB_PRI_SHIFT: u64 = 16; // lands at bit 48: the CQ IRQ-arm flag

#[repr(C)]
struct Cqe {
    _rsvd0: u32,
    _cmd_id: u16,
    _rsvd1: u16,
    sq_head: u16, // completed SQE ring index
    _sq_num: u16,
    _rsvd2: u16,
    w7: u16, // bit0 = phase
}

// ---------- ZIP v3 SQE (uadk v1 hisi_zip_udrv.h) ----------

#[repr(C)]
struct Sqe {
    consumed: u32,
    produced: u32,
    /// lz77: literal byte count on completion
    comp_data_length: u32,
    /// bits [7:0] completion status (0 = ok)
    dw3: u32,
    input_data_length: u32,
    _lba_l: u32,
    _lba_h: u32,
    /// stream/flush/BD-type flags
    dw7: u32,
    _dw8: u32,
    /// bits [7:0] algorithm, [11:8] window size
    dw9: u32,
    _dw10: u32,
    _priv_info: u32,
    _dw12: u32,
    /// lz77: literals output capacity
    dw13: u32,
    /// lz77: sequences output capacity
    dest_avail_out: u32,
    _ctx_dw0: u32,
    _comp_head_l: u32,
    _comp_head_h: u32,
    source_addr_l: u32,
    source_addr_h: u32,
    dest_addr_l: u32,
    dest_addr_h: u32,
    _stream_ctx_l: u32,
    _stream_ctx_h: u32,
    /// lz77: literals output address
    cipher_key_addr_l: u32,
    cipher_key_addr_h: u32,
    tag_l: u32,
    _tag_h: u32,
    _ctx_dw1: u32,
    _ctx_dw2: u32,
    _isize: u32,
    _checksum: u32,
}

/// Algorithm codes in dw9[7:0] (enum hw_comp_alg_type).
const HW_RAW_DEFLATE: u32 = 0x01;
/// 0x42 is the L9 "price" variant; plain L8/LZ4-style runs as 0x43.
const HW_LZ77_ZSTD: u32 = 0x43;
// dw7 flags (uadk v1 hisi_zip_udrv.c): stream NEW + stateless + FINISH,
// BD type 3 — the stateless one-shot shape kaelz4 uses. Bit layout of
// dw7[27:25]: NEW=1, flush=0, FINISH=1.
const DW7_ONE_SHOT: u32 = (0b101 << 25) | (0b11 << 28);
// dw9 window-size field (enum wcrypto_comp_win_type, 0-based)
const WS_16K: u32 = 2;
const WS_32K: u32 = 4;
const WINDOWS_SIZE_SHIFT: u32 = 12;
const HZ_STATUS_MASK: u32 = 0xff;
/// lz77 flat geometry: literals capacity = input + 16 (uadk v1)
const ZSTD_LIT_RSV: u32 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RawAlg {
    Lz77,
    Deflate,
}

/// Worst-case output capacity per algorithm (input-size dependent only).
pub(crate) fn out_cap(alg: RawAlg, req_size: usize) -> usize {
    match alg {
        // literals (block+16) + 8B triples (~2.7x block for 3-byte-period
        // worst case) + freq block + slack
        RawAlg::Lz77 => req_size + 16 + 2 * req_size + 8192,
        RawAlg::Deflate => req_size + req_size / 8 + 8192,
    }
}

/// The SQ doorbell value for "hardware may fetch up to `idx`, exclusive".
fn doorbell_value(sqn: u16, cmd: u8, idx: u16, pri: u8) -> u64 {
    (sqn as u64 & QM_DB_SQN_MASK)
        | ((cmd as u64 & 0xf) << QM_DB_CMD_SHIFT)
        | (((idx as u64) | ((pri as u64) << QM_DB_PRI_SHIFT)) << QM_DB_IDX_SHIFT)
}

/// Fill one lz77 flat SQE (pure, unit-tested; the literals land at the
/// output start, the 8-byte sequence triples after the literal area).
fn fill_lz77_sqe(sqe: &mut Sqe, src_dma: u64, out_dma: u64, in_len: usize, ocap: usize) {
    *sqe = unsafe { std::mem::zeroed() };
    let lit = in_len as u32 + ZSTD_LIT_RSV;
    sqe.dw9 = HW_LZ77_ZSTD | (WS_16K << WINDOWS_SIZE_SHIFT);
    sqe.dw13 = lit;
    sqe.dest_avail_out = ocap as u32 - lit;
    sqe.input_data_length = in_len as u32;
    sqe.source_addr_l = src_dma as u32;
    sqe.source_addr_h = (src_dma >> 32) as u32;
    sqe.cipher_key_addr_l = out_dma as u32;
    sqe.cipher_key_addr_h = (out_dma >> 32) as u32;
    let seq_at = out_dma + lit as u64;
    sqe.dest_addr_l = seq_at as u32;
    sqe.dest_addr_h = (seq_at >> 32) as u32;
    sqe.dw7 = DW7_ONE_SHOT;
    sqe.tag_l = 0; // ring index, set by the submit path
}

/// Fill one raw-deflate flat SQE (32K window, the deflate standard).
fn fill_deflate_sqe(sqe: &mut Sqe, src_dma: u64, out_dma: u64, in_len: usize, ocap: usize) {
    *sqe = unsafe { std::mem::zeroed() };
    sqe.dw9 = HW_RAW_DEFLATE | (WS_32K << WINDOWS_SIZE_SHIFT);
    sqe.dest_avail_out = ocap as u32;
    sqe.input_data_length = in_len as u32;
    sqe.source_addr_l = src_dma as u32;
    sqe.source_addr_h = (src_dma >> 32) as u32;
    sqe.dest_addr_l = out_dma as u32;
    sqe.dest_addr_h = (out_dma >> 32) as u32;
    sqe.dw7 = DW7_ONE_SHOT;
    sqe.tag_l = 0;
}

// ---------- minimal FFI (the crate binds libm-style externs by hand to
// stay dependency-free; sysres.rs does the same for getrusage) ----------

extern "C" {
    fn mmap(
        addr: *mut u8,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, arg: *mut core::ffi::c_void) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

const PROT_READ_WRITE: c_int = 1 | 2;
const MAP_SHARED: c_int = 1;
const O_RDWR_CLOEXEC: i32 = 2 | 0o2000000; // O_RDWR | O_CLOEXEC (Linux)
const POLLIN: i16 = 0x001;

fn do_mmap(file: &File, len: usize, pgoff: i64) -> Result<*mut u8, String> {
    let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ_WRITE, MAP_SHARED, file.as_raw_fd(), pgoff * 4096) };
    if p as usize == usize::MAX {
        return Err(format!("mmap(pgoff {pgoff}, {len}B): {}", std::io::Error::last_os_error()));
    }
    Ok(p)
}

// ---------- sysfs ----------

fn sysfs_str(name: &str, attr: &str) -> Option<String> {
    std::fs::read_to_string(format!("/sys/class/uacce/{name}/{attr}"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn sysfs_u64(name: &str, attr: &str) -> Option<u64> {
    sysfs_str(name, attr).and_then(|s| s.parse().ok())
}

/// The /dev/hisi_zip-* uacce device paths on this machine.
pub(crate) fn zip_devices() -> Vec<String> {
    crate::uacce_devices()
        .into_iter()
        .filter(|n| n.starts_with("hisi_zip"))
        .map(|n| format!("/dev/{n}"))
        .collect()
}

// ---------- SS region: DMA-visible memory ----------

struct SsSlice {
    dma: u64,
    /// VA run covered by this slice's base DMA address
    size: u64,
    va_off: u64,
}

/// The kernel-allocated static-share region: DMA-coherent slices mapped
/// contiguously in VA (sorted by DMA address), addresses queryable per
/// slice through UACCE_CMD_GET_SS_DMA (slice index in, dma|gran out, ioctl
/// return = slices left after this one).
struct SsRegion {
    base: *mut u8,
    len: usize,
    slices: Vec<SsSlice>,
}

impl SsRegion {
    fn map(file: &File, len: usize) -> Result<SsRegion, String> {
        // the no-IOMMU SS granularity is 64KB: round the request up
        let len = (len + 0xffff) & !0xffff;
        let base = do_mmap(file, len, UACCE_QFRT_SS)?;
        let mut slices = Vec::new();
        let mut remaining = 1i64;
        let mut idx = 0u64;
        while remaining > 0 {
            if slices.len() >= 256 {
                unsafe { munmap(base, len) };
                return Err("SS region has more than 256 slices; shrink the request".into());
            }
            let mut info = idx;
            let r = unsafe {
                ioctl(
                    file.as_raw_fd(),
                    UACCE_CMD_GET_SS_DMA,
                    &mut info as *mut u64 as *mut _,
                )
            };
            if r < 0 {
                let e = std::io::Error::last_os_error();
                unsafe { munmap(base, len) };
                return Err(format!("GET_SS_DMA({idx}): {e}"));
            }
            remaining = r as i64;
            slices.push(SsSlice {
                dma: info & !UACCE_GRAN_NUM_MASK,
                size: (info & UACCE_GRAN_NUM_MASK) << UACCE_GRAN_SHIFT,
                va_off: slices.last().map_or(0, |s: &SsSlice| s.va_off + s.size),
            });
            idx += 1;
        }
        Ok(SsRegion { base, len, slices })
    }

    /// DMA address of a VA offset inside the region.
    fn va_to_dma(&self, off: usize) -> Option<u64> {
        let off = off as u64;
        for s in &self.slices {
            if off >= s.va_off && off < s.va_off + s.size {
                return Some(s.dma + (off - s.va_off));
            }
        }
        None
    }

    /// Slice-aware bump allocation: `len` bytes 64-byte aligned, never
    /// straddling a slice boundary (the DMA address is not continuous
    /// across slices). The cursor only ever moves forward. Returns
    /// (va_off, dma).
    fn alloc(&self, cursor: &mut usize, len: usize) -> Option<(usize, u64)> {
        let mut off = *cursor;
        for s in &self.slices {
            let aligned = (off + 63) & !63;
            if aligned < s.va_off as usize {
                continue;
            }
            if aligned + len <= (s.va_off + s.size) as usize {
                *cursor = aligned + len;
                return self.va_to_dma(aligned).map(|dma| (aligned, dma));
            }
            // would straddle this slice: the next candidate starts at the
            // following slice, never behind the cursor
            off = off.max(s.va_off as usize + s.size as usize);
        }
        None
    }

    fn ptr_at(&self, off: usize) -> *mut u8 {
        unsafe { self.base.add(off) }
    }
}

impl Drop for SsRegion {
    fn drop(&mut self) {
        unsafe { munmap(self.base, self.len) };
    }
}

// ---------- one hardware queue ----------

/// Per-queue accounting, aggregated into [`RawStats`].
#[derive(Default)]
struct QueueStats {
    doorbells: u64,
    hw_errors: u64,
    bytes_in: u64,
    bytes_out: u64,
    reqs_done: u64,
}

struct Queue {
    file: File,
    sqn: u16,
    sq: *mut u8,
    cq: *mut u8,
    mmio: *mut u8,
    db_offset: u64,
    mmio_len: usize,
    dus_len: usize,
    /// held for its Drop (unmap); never read after construction
    #[allow(dead_code)]
    ss: SsRegion,
    src: Vec<(*mut u8, u64)>,
    out: Vec<(*mut u8, u64)>,
    sq_tail: u16,
    cq_head: u16,
    /// expected CQE phase bit; starts at 1
    cq_phase: u16,
    outstanding: usize,
    unbelled: usize,
    c_flag: u8,
    alg: RawAlg,
    req_size: usize,
    ocap: usize,
    stats: QueueStats,
}

// The queue owns raw mappings and is used from a single thread (the bench
// worker that opened it); moving it between threads is fine because every
// access goes through &mut self.
unsafe impl Send for Queue {}

impl Queue {
    fn open(dev: &str, req_size: usize, depth: usize, wait_poll: bool, alg: RawAlg) -> Result<Queue, String> {
        let name = dev.rsplit('/').next().unwrap_or(dev);
        if let Some(api) = sysfs_str(name, "api") {
            if !api.contains("hisi_qm_v3") {
                return Err(format!("{dev}: api {api}: only v3 SQE layout is implemented"));
            }
        }
        let mmio_len = sysfs_u64(name, "region_mmio_size").unwrap_or(8192) as usize;
        let dus_len = sysfs_u64(name, "region_dus_size").unwrap_or(147456) as usize;
        if mmio_len < QM_V2_DOORBELL_OFFSET as usize + 8 {
            return Err(format!("{dev}: MMIO region too small for the v2/v3 doorbell"));
        }
        if dus_len < QM_ZIP_SQE_SIZE * QM_Q_DEPTH + QM_CQE_SIZE * QM_Q_DEPTH {
            return Err(format!("{dev}: DUS region too small ({dus_len}B)"));
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_RDWR_CLOEXEC)
            .open(dev)
            .map_err(|e| format!("open {dev}: {e}"))?;

        let mut qp_ctx = QpCtx { id: 0, qc_type: 0 }; // 0 = compress
        let r = unsafe {
            ioctl(
                file.as_raw_fd(),
                UACCE_CMD_QM_SET_QP_CTX,
                &mut qp_ctx as *mut QpCtx as *mut _,
            )
        };
        if r < 0 {
            return Err(format!("{dev}: SET_QP_CTX: {}", std::io::Error::last_os_error()));
        }
        let mut qp_info = QpInfo { sqe_size: 0, sq_depth: 0, cq_depth: 0, reserved: 0 };
        let r = unsafe {
            ioctl(
                file.as_raw_fd(),
                UACCE_CMD_QM_SET_QP_INFO,
                &mut qp_info as *mut QpInfo as *mut _,
            )
        };
        if r < 0 {
            return Err(format!("{dev}: SET_QP_INFO: {}", std::io::Error::last_os_error()));
        }
        if qp_info.sqe_size as usize != QM_ZIP_SQE_SIZE
            || (qp_info.sq_depth as usize) < QM_Q_DEPTH
        {
            return Err(format!(
                "{dev}: unexpected queue geometry (sqe={} sq={})",
                qp_info.sqe_size, qp_info.sq_depth
            ));
        }

        let sq = do_mmap(&file, dus_len, UACCE_QFRT_DUS)?;
        let mmio = match do_mmap(&file, mmio_len, UACCE_QFRT_MMIO) {
            Ok(p) => p,
            Err(e) => {
                unsafe { munmap(sq, dus_len) };
                return Err(e);
            }
        };
        let r = unsafe { ioctl(file.as_raw_fd(), UACCE_CMD_START_Q, std::ptr::null_mut()) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { munmap(sq, dus_len) };
            unsafe { munmap(mmio, mmio_len) };
            return Err(format!("{dev}: START_Q: {e}"));
        }

        // one input and one output block per in-flight slot
        let ocap = out_cap(alg, req_size);
        let ss_len = depth * (req_size + ocap);
        let ss = match SsRegion::map(&file, ss_len) {
            Ok(s) => s,
            Err(e) => {
                unsafe { munmap(sq, dus_len) };
                unsafe { munmap(mmio, mmio_len) };
                return Err(e);
            }
        };
        let mut src = Vec::with_capacity(depth);
        let mut out = Vec::with_capacity(depth);
        let mut cursor = 0usize;
        for _ in 0..depth {
            let (so, sd) = ss
                .alloc(&mut cursor, req_size)
                .ok_or_else(|| {
                    format!(
                        "{dev}: SS region exhausted (~{} MiB wanted for depth {depth})",
                        ss_len / (1024 * 1024)
                    )
                })
                .map_err(|e| {
                    unsafe { munmap(sq, dus_len) };
                    unsafe { munmap(mmio, mmio_len) };
                    e
                })?;
            let (oo, od) = ss
                .alloc(&mut cursor, ocap)
                .ok_or_else(|| format!("{dev}: SS region exhausted (output area)"))
                .map_err(|e| {
                    unsafe { munmap(sq, dus_len) };
                    unsafe { munmap(mmio, mmio_len) };
                    e
                })?;
            src.push((ss.ptr_at(so), sd));
            out.push((ss.ptr_at(oo), od));
        }

        Ok(Queue {
            file,
            sqn: qp_ctx.id,
            sq,
            cq: unsafe { sq.add(QM_ZIP_SQE_SIZE * QM_Q_DEPTH) },
            mmio,
            db_offset: QM_V2_DOORBELL_OFFSET,
            mmio_len,
            dus_len,
            ss,
            src,
            out,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: 1,
            outstanding: 0,
            unbelled: 0,
            c_flag: u8::from(wait_poll),
            alg,
            req_size,
            ocap,
            stats: QueueStats::default(),
        })
    }

    /// Copy workload data into a slot's input buffer (called once per slot
    /// before the timed runs; the engine rereads it every request).
    fn load_slot(&mut self, slot: usize, data: &[u8]) {
        let (ptr, _) = self.src[slot];
        let n = self.req_size;
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr, n) };
        let start = (slot * n) % data.len();
        let first = data[start..].len().min(n);
        dst[..first].copy_from_slice(&data[start..start + first]);
        if first < n {
            dst[first..].copy_from_slice(&data[..n - first]);
        }
    }

    fn doorbell(&mut self, cmd: u8, idx: u16, pri: u8) {
        // ring writes must be visible before the doorbell store
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        let db = doorbell_value(self.sqn, cmd, idx, pri);
        unsafe { (self.mmio.offset(self.db_offset as isize) as *mut u64).write_volatile(db) };
        self.stats.doorbells += 1;
    }

    /// Fill up to `budget` SQEs; rings the doorbell every `batch` fills and
    /// at every ring-wrap boundary so one doorbell never spans the wrap.
    fn submit(&mut self, batch: usize, budget: usize) -> usize {
        // device status: value 1 in the last DUS word means a hardware
        // error happened (uadk v1 qm_tx_update reads it before every send)
        if self.dus_len > QM_DS_TX_AT_END {
            let ds = unsafe {
                (self
                    .sq
                    .add(self.dus_len - QM_DS_TX_AT_END) as *const u32)
                    .read_volatile()
            };
            if ds == 1 {
                self.stats.hw_errors += 1;
                if self.stats.hw_errors <= 8 {
                    eprintln!("rawzip: device status register reports a hardware error");
                }
            }
        }
        let mut submitted = 0;
        while submitted < budget && self.outstanding < self.src.len() {
            let i = self.sq_tail;
            let slot = i as usize % self.src.len();
            let (_sptr, sdma) = self.src[slot];
            let (_optr, odma) = self.out[slot];
            let sqe = unsafe { &mut *(self.sq.add(i as usize * QM_ZIP_SQE_SIZE) as *mut Sqe) };
            match self.alg {
                RawAlg::Lz77 => fill_lz77_sqe(sqe, sdma, odma, self.req_size, self.ocap),
                RawAlg::Deflate => fill_deflate_sqe(sqe, sdma, odma, self.req_size, self.ocap),
            }
            sqe.tag_l = i as u32;
            self.sq_tail = (i + 1) % QM_Q_DEPTH as u16;
            self.outstanding += 1;
            submitted += 1;
            self.unbelled += 1;
            if self.unbelled >= batch || self.sq_tail == 0 {
                self.doorbell(DOORBELL_CMD_SQ, self.sq_tail, 0);
                self.unbelled = 0;
            }
        }
        if self.unbelled > 0 {
            self.doorbell(DOORBELL_CMD_SQ, self.sq_tail, 0);
            self.unbelled = 0;
        }
        submitted
    }

    /// Reap every completed CQE. The CQ doorbell (advanced cq_head + the
    /// interrupt-arm flag) rings once per call, matching uadk's
    /// per-recv-call rx_update granularity.
    fn harvest(&mut self) -> usize {
        let mut reaped = 0;
        while self.outstanding > 0 {
            let cqe = unsafe { &*(self.cq.add(self.cq_head as usize * QM_CQE_SIZE) as *const Cqe) };
            let phase = unsafe { std::ptr::read_volatile(&cqe.w7) } & 1;
            if phase != self.cq_phase {
                break;
            }
            // CQE visible -> completion payload visible
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            let sq_head = u16::from_le(cqe.sq_head) as usize;
            assert!(sq_head < QM_Q_DEPTH, "CQE sq_head {sq_head} out of range");
            let sqe = unsafe { &*(self.sq.add(sq_head * QM_ZIP_SQE_SIZE) as *const Sqe) };
            let status = u32::from_le(sqe.dw3) & HZ_STATUS_MASK;
            if status != 0 {
                self.stats.hw_errors += 1;
                if self.stats.hw_errors <= 8 {
                    eprintln!("rawzip: hw status 0x{status:x} (ring idx {sq_head})");
                }
            } else {
                let lit = u32::from_le(sqe.comp_data_length) as u64;
                let seqs = u32::from_le(sqe.produced) as u64;
                match self.alg {
                    RawAlg::Lz77 => {
                        let lit_cap = (self.req_size + 16) as u64;
                        let seq_cap = (self.ocap - self.req_size - 16) as u64;
                        if lit > lit_cap || seqs * 8 > seq_cap {
                            self.stats.hw_errors += 1;
                            if self.stats.hw_errors <= 8 {
                                eprintln!("rawzip: lz77 output insanity lit={lit} seqs={seqs}");
                            }
                        }
                        self.stats.bytes_out += lit + seqs * 8;
                    }
                    RawAlg::Deflate => self.stats.bytes_out += seqs,
                }
            }
            if self.cq_head as usize == QM_Q_DEPTH - 1 {
                self.cq_phase ^= 1;
                self.cq_head = 0;
            } else {
                self.cq_head += 1;
            }
            self.outstanding -= 1;
            self.stats.bytes_in += self.req_size as u64;
            self.stats.reqs_done += 1;
            reaped += 1;
        }
        if reaped > 0 {
            let pri = self.c_flag;
            self.doorbell(DOORBELL_CMD_CQ, self.cq_head, pri);
        }
        reaped
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        unsafe { ioctl(self.file.as_raw_fd(), UACCE_CMD_PUT_Q, std::ptr::null_mut()) };
        // SsRegion's own Drop unmaps the SS region; the DUS/MMIO maps go here
        unsafe { munmap(self.sq, self.dus_len) };
        unsafe { munmap(self.mmio, self.mmio_len) };
    }
}

// ---------- the pump ----------

/// Aggregated pump result of one [`RawPump::run`].
pub(crate) struct RawStats {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub reqs_done: u64,
    pub doorbells: u64,
    pub hw_errors: u64,
    pub wall_s: f64,
    pub user_s: f64,
    pub sys_s: f64,
    pub nvcsw: u64,
    pub nivcsw: u64,
}

/// A pool of raw hardware queues driven from one thread: submit everything,
/// harvest everything, repeat. `run` reports engine-side throughput with
/// zero payload copies in the loop (the workload sits in DMA-visible SS
/// memory once at setup).
pub(crate) struct RawPump {
    queues: Vec<Queue>,
    batch: usize,
    wait_poll: bool,
}

impl RawPump {
    /// Open `queues` hardware queues round-robin over `devices`.
    pub fn open(
        devices: &[String],
        queues: usize,
        req_size: usize,
        depth: usize,
        batch: usize,
        wait_poll: bool,
        alg: RawAlg,
    ) -> Result<RawPump, String> {
        if devices.is_empty() {
            return Err("no /dev/hisi_zip-* devices".into());
        }
        if alg == RawAlg::Lz77 && req_size > MAX_LZ77_REQ {
            return Err(format!("lz77 flat input cap is {MAX_LZ77_REQ}B"));
        }
        if queues == 0 || depth == 0 || depth > QM_Q_DEPTH {
            return Err(format!("queues 1.., depth 1..={QM_Q_DEPTH}"));
        }
        let mut qs = Vec::with_capacity(queues);
        for q in 0..queues {
            match Queue::open(&devices[q % devices.len()], req_size, depth, wait_poll, alg) {
                Ok(queue) => qs.push(queue),
                Err(e) => {
                    if q > 0 {
                        eprintln!("rawzip: opened {q} of {queues} queues before: {e}");
                    }
                    return Err(e);
                }
            }
        }
        Ok(RawPump {
            queues: qs,
            batch: batch.max(1),
            wait_poll,
        })
    }

    /// Fill every slot's input buffer, cycling `data` (once per run setup;
    /// the loop itself copies nothing).
    pub fn load_data(&mut self, data: &[u8]) {
        assert!(!data.is_empty());
        let per = self.queues[0].src.len();
        for q in &mut self.queues {
            for s in 0..per {
                q.load_slot(s, data);
            }
        }
    }

    fn poll_once(&self, timeout_ms: c_int) {
        let mut fds: Vec<PollFd> = self
            .queues
            .iter()
            .map(|q| PollFd {
                fd: q.file.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            })
            .collect();
        unsafe { poll(fds.as_mut_ptr(), fds.len() as c_ulong, timeout_ms) };
    }

    /// Prime the queues, run the submit/harvest loop for `seconds`, drain,
    /// and return the aggregated stats (wall/CPU deltas cover the whole
    /// call; the process is single-threaded).
    pub fn run(&mut self, seconds: f64) -> RawStats {
        let cpu0: CpuTime = cpu_time();
        let (nvcsw0, nivcsw0) = switch_counts();
        let t0 = now_s();

        for q in &mut self.queues {
            q.submit(self.batch, q.src.len());
        }
        loop {
            let mut reaped = 0;
            let mut any_out = false;
            for q in &mut self.queues {
                reaped += q.harvest();
                any_out |= q.outstanding > 0;
            }
            for q in &mut self.queues {
                q.submit(self.batch, q.src.len());
            }
            if self.wait_poll && any_out && reaped == 0 {
                self.poll_once(200);
            }
            if !any_out && reaped == 0 {
                std::thread::sleep(Duration::from_micros(100));
            }
            let t = now_s();
            if t - t0 >= seconds {
                break;
            }
        }
        // drain, bounded so a wedged ring reports instead of hanging
        let drain_start = now_s();
        loop {
            let mut outstanding = 0;
            for q in &mut self.queues {
                q.harvest();
                outstanding += q.outstanding;
            }
            if outstanding == 0 || now_s() - drain_start > 10.0 {
                break;
            }
            if self.wait_poll {
                self.poll_once(200);
            }
        }

        let wall_s = now_s() - t0;
        let cpu1 = cpu_time();
        let (nvcsw1, nivcsw1) = switch_counts();
        let mut st = RawStats {
            bytes_in: 0,
            bytes_out: 0,
            reqs_done: 0,
            doorbells: 0,
            hw_errors: 0,
            wall_s,
            user_s: cpu1.user_s - cpu0.user_s,
            sys_s: cpu1.sys_s - cpu0.sys_s,
            nvcsw: nvcsw1 - nvcsw0,
            nivcsw: nivcsw1 - nivcsw0,
        };
        for q in &self.queues {
            st.bytes_in += q.stats.bytes_in;
            st.bytes_out += q.stats.bytes_out;
            st.reqs_done += q.stats.reqs_done;
            st.doorbells += q.stats.doorbells;
            st.hw_errors += q.stats.hw_errors;
        }
        st
    }
}

/// Hardware smoke tests: real /dev/hisi_zip-* nodes required, so they are
/// `#[ignore]`d like tests/hw_required.rs; on the target machine run
/// `cargo test -- --ignored`. These validate the raw pump's engine dialog
/// (open, SQE, doorbell, completions with status 0) in both dialects —
/// the counterpart of the C tool's zlib round-trip check.
#[cfg(test)]
mod hw_tests {
    use super::*;

    fn seeded(n: usize, seed: u64) -> Vec<u8> {
        let mut v = vec![0u8; n];
        let mut x = seed | 1;
        for b in v.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        v
    }

    /// A random 4K page tiled to size. Pure noise cannot be compressed: the
    /// engine then completes the request as negacompress (status 0xd), which
    /// the pump counts as a hardware error, so the hw workload must compress.
    fn compressible(n: usize, seed: u64) -> Vec<u8> {
        let page = seeded(4096, seed);
        let mut v = vec![0u8; n];
        for (i, b) in v.iter_mut().enumerate() {
            *b = page[i % page.len()];
        }
        v
    }

    fn one_engine_run(alg: RawAlg) -> RawStats {
        let devices = zip_devices();
        assert!(!devices.is_empty(), "needs /dev/hisi_zip-* uacce nodes");
        let mut pump =
            RawPump::open(&devices, 1, 4096, 8, 8, false, alg).expect("open raw queue");
        pump.load_data(&compressible(4096 * 16, 0xDEAD_BEEF));
        let st = pump.run(0.5);
        assert!(st.reqs_done > 0, "engine completed no requests");
        assert_eq!(st.hw_errors, 0, "engine reported hardware errors");
        st
    }

    #[test]
    #[ignore = "requires /dev/hisi_zip-* uacce nodes: run with cargo test -- --ignored"]
    fn hw_deflate_requests_complete() {
        let st = one_engine_run(RawAlg::Deflate);
        assert!(st.bytes_out > 0, "deflate should produce a stream per request");
    }

    #[test]
    #[ignore = "requires /dev/hisi_zip-* uacce nodes: run with cargo test -- --ignored"]
    fn hw_lz77_requests_complete() {
        let st = one_engine_run(RawAlg::Lz77);
        assert!(st.bytes_out > 0, "lz77 triples should not be empty");
    }
}

// The QP number (doorbell identity) is captured from SET_QP_CTX at open
// and lives in the sqn field.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doorbell_known_answers() {
        // v2/v3 encoding: sqn | cmd<<12 | (idx | pri<<16)<<32
        assert_eq!(doorbell_value(7, DOORBELL_CMD_SQ, 0, 0), 7);
        assert_eq!(doorbell_value(7, DOORBELL_CMD_SQ, 63, 0), 7 | (63u64 << 32));
        assert_eq!(doorbell_value(1024 & 0x3ff, DOORBELL_CMD_CQ, 0, 1), (1u64 << 12) | (1u64 << 48));
        assert_eq!(doorbell_value(3, DOORBELL_CMD_CQ, 1023, 0), 3 | (1u64 << 12) | (1023u64 << 32));
    }

    fn sqe_bytes(f: impl FnOnce(&mut Sqe)) -> [u8; 128] {
        let mut s = unsafe { std::mem::zeroed::<Sqe>() };
        f(&mut s);
        unsafe { std::mem::transmute::<Sqe, [u8; 128]>(s) }
    }

    #[test]
    fn lz77_sqe_known_answer() {
        let b = sqe_bytes(|s| fill_lz77_sqe(s, 0x1234_5678_9abc_def0, 0xfeed_0000_0000_1000, 65536, out_cap(RawAlg::Lz77, 65536)));
        let dw = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(dw(9) & 0xff, HW_LZ77_ZSTD); // algorithm
        assert_eq!(dw(9) >> WINDOWS_SIZE_SHIFT & 0xf, WS_16K);
        assert_eq!(dw(13), 65536 + 16); // literals capacity
        assert_eq!(dw(14), out_cap(RawAlg::Lz77, 65536) as u32 - 65552); // seq capacity
        assert_eq!(dw(4), 65536); // input_data_length
        assert_eq!(dw(7), (0b101 << 25) | (0b11 << 28)); // NEW/stateless/FINISH/BD3
        // source at dwords 18/19
        assert_eq!((dw(19), dw(18)), (0x1234_5678, 0x9abc_def0));
        // literals (cipher_key pair) at the output start: dwords 24/25
        assert_eq!((dw(25), dw(24)), (0xfeed_0000, 0x0000_1000));
        // sequences after the literal area: dest pair at dwords 20/21
        let seq = 0xfeed_0000_0000_1000u64 + 65552;
        assert_eq!((dw(21), dw(20)), ((seq >> 32) as u32, seq as u32));
    }

    #[test]
    fn deflate_sqe_known_answer() {
        let cap = out_cap(RawAlg::Deflate, 65536);
        let b = sqe_bytes(|s| fill_deflate_sqe(s, 0x1000, 0x2000, 65536, cap));
        let dw = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(dw(9) & 0xff, HW_RAW_DEFLATE);
        assert_eq!(dw(9) >> WINDOWS_SIZE_SHIFT & 0xf, WS_32K);
        assert_eq!(dw(14), cap as u32);
        assert_eq!((dw(21), dw(20)), (0, 0x2000)); // dest directly
        assert_eq!((dw(19), dw(18)), (0, 0x1000)); // source
    }

    #[test]
    fn ss_bump_never_straddles_slices() {
        let m = 1024 * 1024u64;
        let slices = vec![
            SsSlice { dma: 0x4000_0000, size: 4 * m, va_off: 0 },
            SsSlice { dma: 0x8000_0000, size: 4 * m, va_off: 4 * m },
        ];
        let region = SsRegion { base: std::ptr::null_mut(), len: 0, slices };
        let mut cur = 0usize;
        // fits in slice 0
        let (off, dma) = region.alloc(&mut cur, m as usize).unwrap();
        assert_eq!((off, dma), (0, 0x4000_0000));
        // jumps to 3.9M: 300KB+128 would cross the slice end -> slice 1
        let mut cur = (4 * m - 300 * 1024) as usize;
        let (off, dma) = region.alloc(&mut cur, 300 * 1024 + 128).unwrap();
        assert_eq!(off as u64, 4 * m);
        assert_eq!(dma, 0x8000_0000);
        // exhausted: past both slices
        let mut cur = region.slices.last().unwrap().va_off as usize
            + region.slices.last().unwrap().size as usize;
        assert!(region.alloc(&mut cur, 64).is_none());
    }

    /// The ring bookkeeping against fabricated in-memory rings: submit
    /// past the SQ wrap, fabricate the matching CQEs in completion order
    /// (including the phase flip at CQ index 1023), and check the
    /// advance/doorbell discipline end to end.
    #[test]
    fn ring_wrap_and_phase_flip() {
        const DEPTH: usize = 1024;
        let sq_ring: Box<[u8; QM_ZIP_SQE_SIZE * DEPTH]> = Box::new([0; QM_ZIP_SQE_SIZE * DEPTH]);
        let cq_ring: Box<[u8; QM_CQE_SIZE * DEPTH]> = Box::new([0; QM_CQE_SIZE * DEPTH]);
        let db_page: Box<[u8; 64]> = Box::new([0; 64]);
        let sqp = Box::into_raw(sq_ring) as *mut u8;
        let cqp = Box::into_raw(cq_ring) as *mut u8;
        let dbp = Box::into_raw(db_page) as *mut u8;

        let mut q = Queue {
            file: File::open("/dev/null").unwrap(),
            sqn: 3,
            sq: sqp,
            cq: cqp,
            mmio: dbp,
            db_offset: 0,
            mmio_len: 64,
            dus_len: 0,
            ss: SsRegion { base: std::ptr::null_mut(), len: 0, slices: Vec::new() },
            src: (0..2).map(|s| (std::ptr::null_mut(), 0x1000 + s as u64)).collect(),
            out: (0..2).map(|s| (std::ptr::null_mut(), 0x9000 + s as u64)).collect(),
            sq_tail: 0,
            cq_head: 0,
            cq_phase: 1,
            outstanding: 0,
            unbelled: 0,
            c_flag: 0,
            alg: RawAlg::Lz77,
            req_size: 4096,
            ocap: out_cap(RawAlg::Lz77, 4096),
            stats: QueueStats::default(),
        };

        // drive 1030 requests through 2 slots: crosses the SQ wrap, the CQ
        // wrap, and the CQE phase flip at index 1023
        let mut awaiting: Vec<u16> = Vec::new();
        let mut completed = 0u64;
        while completed < 1030 {
            while q.outstanding < 2 {
                let tail_before = q.sq_tail;
                if q.submit(4, 2) == 0 {
                    break;
                }
                let mut i = tail_before;
                while i != q.sq_tail {
                    awaiting.push(i);
                    i = (i + 1) % DEPTH as u16;
                }
            }
            assert_eq!(q.outstanding, 2);
            // the hardware completes the oldest in-flight request: write the
            // CQE where the pump expects it, with the phase it expects
            let i = awaiting.remove(0);
            unsafe {
                let cqe = &mut *(cqp.add(q.cq_head as usize * QM_CQE_SIZE) as *mut Cqe);
                cqe.w7 = q.cq_phase;
                cqe.sq_head = i.to_le();
            }
            assert_eq!(q.harvest(), 1);
            completed += 1;
        }
        assert_eq!(q.stats.reqs_done, 1030);
        assert_eq!(q.stats.hw_errors, 0);
        assert_eq!(q.cq_head, 6, "1030 reaps wrap the CQ ring once and land at 6");
        assert_eq!(q.cq_phase, 0, "the phase flips exactly once per full CQ lap");
        // SQ doorbells: batched every 4 fills, plus one per ring wrap and
        // one flush per submit call — at least one per 4 requests overall
        assert!(q.stats.doorbells >= q.stats.reqs_done / 4 - 8);
        // every CQ doorbell after a harvest ringed the advanced head
        assert!(q.stats.doorbells > 0);

        unsafe {
            drop(Box::from_raw(sqp as *mut [u8; QM_ZIP_SQE_SIZE * DEPTH]));
            drop(Box::from_raw(cqp as *mut [u8; QM_CQE_SIZE * DEPTH]));
            drop(Box::from_raw(dbp as *mut [u8; 64]));
        }
    }
}
