//! The native backend's uacce driver for the KAE2 ZIP engine: the
//! C-stack-free hardware path. It speaks the queue ABI directly —
//! open, ioctl, mmap, SQE fill, doorbell store, CQE harvest — with no
//! libwd, no libkaelz4 and no patched liblz4 in the process.
//!
//! The pump measures the engine's throughput ceiling as a function of
//! (queues x in-flight depth x request size x wait policy), which is the
//! question the libkaelz4-backed cells cannot answer: their sync path is
//! one request in flight plus a 100%-core spin, and their async pool adds
//! twelve spinning harvesters on top (see docs/kae-limitations.md).
//!
//! Views (docs/native-bench.md): the pump's GB/s is the lz77 ceiling view
//! — a format no software implementation produces — while the triples
//! rebuild into standard LZ4 blocks (src/rebuild.rs), the LZ4 view that
//! kae-vs-sw comparisons (throughput, CPU) are read on.
//!
//! Provenance of every constant — do not guess, re-verify against:
//! - UACCE UAPI ioctls and region pgoffs:
//!   KAEKernelDriver-OLK-6.6/include_uapi_linux/uacce.h
//! - QM qp ioctls: KAEKernelDriver-OLK-6.6/hisilicon/hisi_qm.h
//! - doorbell encoding, CQE layout, queue geometry:
//!   uadk/v1/drv/hisi_qm_udrv.{c,h}
//! - ZIP v3 SQE layout, algorithm codes, stream flags, lz77 flat-buffer
//!   geometry: uadk/v1/drv/hisi_zip_udrv.{c,h} plus the KAE uadk patch
//!   scripts/patches/0007-uadk-support-lz77-only-for-kaelz4.patch (the
//!   0x40/0x41 no-repeat algorithm codes) and KAELz4's session defaults
//!   (src/v1/kaelz4_init.c, src/v1/kaelz4_ctx.c)
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
//! lz77 (the kaelz4 production mode: the LZ77_ONLY algorithm 0x40, output
//! as literals + 8-byte sequence triples) and deflate (algorithm code
//! HW_RAW_DEFLATE, validated end to end by inflating one output in the
//! unit tests on hardware).

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
    /// completion: [31:24] literal-length overflow count, [23:0] position
    checksum: u32,
}

/// Algorithm codes in dw9[7:0] (enum hw_comp_alg_type).
const HW_RAW_DEFLATE: u32 = 0x01;
/// The kaelz4 production path requests the LZ77_ONLY dialect at comp
/// level 8, which the driver programs as 0x40: no repeat codes, every
/// triple offset explicit. Sources: uadk patch
/// scripts/patches/0007-uadk-support-lz77-only-for-kaelz4.patch and
/// KAELz4/src/v1/kaelz4_init.c. The 0x42/0x43 LZ77_ZSTD flavors are a
/// different dialect kaelz4 does not use.
const HW_LZ77_ZSTD_NOREPCODE: u32 = 0x40;
// dw7 flags (uadk v1 hisi_zip_udrv.c): stream NEW + stateless + FINISH,
// BD type 3 — the stateless one-shot shape kaelz4 uses. Bit layout of
// dw7[27:25]: NEW=1, flush=0, FINISH=1.
const DW7_ONE_SHOT: u32 = (0b101 << 25) | (0b11 << 28);
// dw9 window-size field (enum wcrypto_comp_win_type, 0-based)
const WS_16K: u32 = 2;
const WS_32K: u32 = 4;
const WINDOWS_SIZE_SHIFT: u32 = 12;
const HZ_STATUS_MASK: u32 = 0xff;
/// lz77 flat geometry: literals area slack (uadk v1 ZSTD_LIT_RSV_SIZE)
const ZSTD_LIT_RSV: u32 = 16;
/// LZ4 block parsing restriction: a match may not start within the last
/// 12 bytes of the block. The engine does not enforce this itself, so the
/// request programs `in_len - MFLIMIT` as the parse length and the rebuild
/// emits the reserved tail as the final literal run — the same contract
/// kaelz4 sends the chip under.
const MFLIMIT: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NativeAlg {
    Lz77,
    Deflate,
}

/// Worst-case output capacity per algorithm (input-size dependent only).
pub(crate) fn out_cap(alg: NativeAlg, req_size: usize) -> usize {
    match alg {
        // literals (block+16) + 8B triples (~2.7x block for 3-byte-period
        // worst case) + freq block + slack
        NativeAlg::Lz77 => req_size + 16 + 2 * req_size + 8192,
        NativeAlg::Deflate => req_size + req_size / 8 + 8192,
    }
}

/// The SQ doorbell value for "hardware may fetch up to `idx`, exclusive".
fn doorbell_value(sqn: u16, cmd: u8, idx: u16, pri: u8) -> u64 {
    (sqn as u64 & QM_DB_SQN_MASK)
        | ((cmd as u64 & 0xf) << QM_DB_CMD_SHIFT)
        | (((idx as u64) | ((pri as u64) << QM_DB_PRI_SHIFT)) << QM_DB_IDX_SHIFT)
}

/// The literals-area size and the sequence-triples offset within one lz77
/// output buffer: uadk v1 sizes both off the programmed parse length
/// (`dw13 = in_size + ZSTD_LIT_RSV`, triples at `dst + in_size +
/// ZSTD_LIT_RSV`, fill_zip_buffer_size_zstd / fill_zip_addr_lz77_zstd),
/// and kaelz4 hands the chip `src_size - MFLIMIT` as that in_size.
fn lz77_lit_area(req_size: usize) -> usize {
    req_size.saturating_sub(MFLIMIT) + ZSTD_LIT_RSV as usize
}

/// Source and output DMA addresses plus the one-shot flags shared by both
/// dialects (tag_l is the ring index, set by the submit path).
fn set_sqe_addrs(sqe: &mut Sqe, src_dma: u64, out_dma: u64) {
    sqe.source_addr_l = src_dma as u32;
    sqe.source_addr_h = (src_dma >> 32) as u32;
    sqe.dest_addr_l = out_dma as u32;
    sqe.dest_addr_h = (out_dma >> 32) as u32;
    sqe.dw7 = DW7_ONE_SHOT;
    sqe.tag_l = 0;
}

/// Fill one lz77 flat SQE (pure, unit-tested; the literals land at the
/// output start, the 8-byte sequence triples after the literal area). The
/// dialect is the one kaelz4's LZ4_compress_default drives: LZ77_ONLY at
/// comp level 8, so the triples carry explicit offsets only. The engine
/// parses `in_len - MFLIMIT` bytes; the last 12 bytes of the input belong
/// to the rebuild's tail literal run.
fn fill_lz77_sqe(sqe: &mut Sqe, src_dma: u64, out_dma: u64, in_len: usize, ocap: usize) {
    *sqe = unsafe { std::mem::zeroed() };
    let lit = lz77_lit_area(in_len) as u32;
    sqe.dw9 = HW_LZ77_ZSTD_NOREPCODE | (WS_16K << WINDOWS_SIZE_SHIFT);
    sqe.dw13 = lit;
    sqe.dest_avail_out = ocap as u32 - lit;
    sqe.input_data_length = in_len.saturating_sub(MFLIMIT) as u32;
    set_sqe_addrs(sqe, src_dma, out_dma + lit as u64);
    sqe.cipher_key_addr_l = out_dma as u32; // the literals area
    sqe.cipher_key_addr_h = (out_dma >> 32) as u32;
}

/// Fill one deflate flat SQE (32K window; headerless deflate, the
/// HW_RAW_DEFLATE code — no zlib wrapper).
fn fill_deflate_sqe(sqe: &mut Sqe, src_dma: u64, out_dma: u64, in_len: usize, ocap: usize) {
    *sqe = unsafe { std::mem::zeroed() };
    sqe.dw9 = HW_RAW_DEFLATE | (WS_32K << WINDOWS_SIZE_SHIFT);
    sqe.dest_avail_out = ocap as u32;
    sqe.input_data_length = in_len as u32;
    set_sqe_addrs(sqe, src_dma, out_dma);
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

/// Unmaps every (ptr, len) still held when dropped; disarmed with
/// `mem::forget` once the opened Queue takes ownership of the mappings.
struct MapGuard(Vec<(*mut u8, usize)>);

impl Drop for MapGuard {
    fn drop(&mut self) {
        for &(p, len) in &self.0 {
            unsafe { munmap(p, len) };
        }
    }
}

/// Per-queue accounting, aggregated into [`NativeStats`].
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
    /// QP number (doorbell identity), captured from SET_QP_CTX at open.
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
    /// Streaming-submit occupancy per input buffer: set when a request
    /// enters the buffer, held until its samples are read and the caller
    /// releases the position ([`Queue::try_submit`] / [`Queue::release`]).
    /// The drained paths ([`Queue::request`], `request_batch`) never touch
    /// it — they own every buffer for their whole call.
    held: Vec<bool>,
    c_flag: u8,
    alg: NativeAlg,
    req_size: usize,
    ocap: usize,
    stats: QueueStats,
}

// The queue owns its mappings and is used from a single thread (the bench
// worker that opened it); moving it between threads is fine because every
// access goes through &mut self.
unsafe impl Send for Queue {}

impl Queue {
    fn open(dev: &str, req_size: usize, depth: usize, wait_poll: bool, alg: NativeAlg) -> Result<Queue, String> {
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
        let mut maps = MapGuard(vec![(sq, dus_len)]);
        let mmio = do_mmap(&file, mmio_len, UACCE_QFRT_MMIO)?;
        maps.0.push((mmio, mmio_len));
        let r = unsafe { ioctl(file.as_raw_fd(), UACCE_CMD_START_Q, std::ptr::null_mut()) };
        if r < 0 {
            return Err(format!("{dev}: START_Q: {}", std::io::Error::last_os_error()));
        }

        // one input and one output block per in-flight slot
        let ocap = out_cap(alg, req_size);
        let ss_len = depth * (req_size + ocap);
        let ss = SsRegion::map(&file, ss_len)?;
        let exhausted = || {
            format!(
                "{dev}: SS region exhausted (~{} MiB wanted for depth {depth})",
                ss_len / (1024 * 1024)
            )
        };
        let mut src = Vec::with_capacity(depth);
        let mut out = Vec::with_capacity(depth);
        let mut cursor = 0usize;
        for _ in 0..depth {
            let (so, sd) = ss.alloc(&mut cursor, req_size).ok_or_else(exhausted)?;
            let (oo, od) = ss.alloc(&mut cursor, ocap).ok_or_else(exhausted)?;
            src.push((ss.ptr_at(so), sd));
            out.push((ss.ptr_at(oo), od));
        }

        let queue = Queue {
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
            held: vec![false; depth],
            c_flag: u8::from(wait_poll),
            alg,
            req_size,
            ocap,
            stats: QueueStats::default(),
        };
        std::mem::forget(maps);
        Ok(queue)
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

    /// Run one lz77 request on the next ring slot carrying exactly `input`
    /// (up to the request size; a shorter input programs its own parse
    /// length) and return its sequence triples — the rebuild input for the
    /// cross-validation sample and for [`crate::native`]'s per-block path.
    /// Idle queue only, outside the timed loop; `timeout_s` bounds the
    /// completion wait, spent in `poll` on the uacce fd when the queue was
    /// opened for it (the sweep measured equal throughput at a quarter of
    /// spin's CPU) and spinning otherwise.
    fn request(&mut self, input: &[u8], batch: usize, timeout_s: f64) -> Result<Lz77Sample, String> {
        if self.outstanding != 0 {
            return Err("queue_busy".into());
        }
        if self.alg != NativeAlg::Lz77 {
            return Err("sample_is_lz77_only".into());
        }
        if input.len() > self.req_size {
            return Err(format!(
                "request_input_{}B_req_{}B",
                input.len(),
                self.req_size
            ));
        }
        let slot = self.sq_tail as usize % self.src.len();
        let (sptr, _) = self.src[slot];
        let slot_buf = unsafe { std::slice::from_raw_parts_mut(sptr, self.req_size) };
        slot_buf[..input.len()].copy_from_slice(input);
        let idx = self.sq_tail;
        self.submit_len(batch.max(1), 1, input.len(), true);
        if let Err(e) = self.wait_idle(timeout_s) {
            return Err(e);
        }
        self.read_result(idx as usize, input.len())
    }

    /// Harvest until nothing is outstanding: `poll` on the uacce fd when
    /// the queue waits that way (the CQ doorbell's IRQ-arm flag, rung by
    /// every reaping harvest, wakes it), spinning otherwise. `timeout_s`
    /// bounds the whole wait.
    fn wait_idle(&mut self, timeout_s: f64) -> Result<(), String> {
        let t0 = now_s();
        while self.outstanding > 0 {
            let reaped = self.harvest();
            if reaped == 0 && self.c_flag != 0 {
                self.poll_once(100);
            }
            if now_s() - t0 > timeout_s {
                return Err("request_did_not_complete_in_time".into());
            }
        }
        Ok(())
    }

    /// Sleep on the queue's fd for a completion IRQ (no-op timeout expiry
    /// is fine; the caller re-harvests either way).
    fn poll_once(&self, timeout_ms: c_int) {
        let mut fd = PollFd {
            fd: self.file.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        unsafe { poll(&mut fd, 1, timeout_ms) };
    }

    /// Read one completed request's sample off its SQE and output slot.
    /// `ring` is the SQE ring index the request was submitted at, `in_len`
    /// the input length it was programmed with.
    fn read_result(&self, ring: usize, in_len: usize) -> Result<Lz77Sample, String> {
        let sqe = unsafe { &*(self.sq.add(ring * QM_ZIP_SQE_SIZE) as *const Sqe) };
        let status = u32::from_le(sqe.dw3) & HZ_STATUS_MASK;
        if status != 0 {
            return Err(format!("hw_status_0x{status:x}"));
        }
        let lit = u32::from_le(sqe.comp_data_length) as usize;
        let ntriples = u32::from_le(sqe.produced) as usize;
        let lit_cap = lz77_lit_area(in_len);
        if lit > lit_cap || ntriples * 8 + 32 > self.ocap - lit_cap {
            return Err(format!("lz77_output_insanity_lit={lit}_seqs={ntriples}"));
        }
        let slot = ring % self.src.len();
        let (optr, _) = self.out[slot];
        let seq_area = unsafe { optr.add(lit_cap) };
        let ck = u32::from_le(sqe.checksum);
        let overflow = unsafe { std::slice::from_raw_parts(seq_area.add(ntriples * 8), 8) }.to_vec();
        Ok(Lz77Sample {
            lits: unsafe { std::slice::from_raw_parts(optr, lit) }.to_vec(),
            seqs: unsafe { std::slice::from_raw_parts(seq_area, ntriples * 8) }.to_vec(),
            lit,
            ll_overflow_cnt: ck >> 24,
            ll_overflow_pos: ck & 0xff_ffff,
            overflow,
        })
    }

    /// Copy a completed request's triples into `seqs` (cleared first) —
    /// the streaming counterpart of [`Queue::read_result`] that skips the
    /// literal area: the rebuild reads literals from the input, so the
    /// copy would be dead weight on every request of a segment pass (the
    /// debug verifier uses [`Queue::read_result`]).
    fn read_seqs(&self, ring: usize, in_len: usize, seqs: &mut Vec<u8>) -> Result<(), String> {
        let sqe = unsafe { &*(self.sq.add(ring * QM_ZIP_SQE_SIZE) as *const Sqe) };
        let status = u32::from_le(sqe.dw3) & HZ_STATUS_MASK;
        if status != 0 {
            return Err(format!("hw_status_0x{status:x}"));
        }
        let ntriples = u32::from_le(sqe.produced) as usize;
        let lit_cap = lz77_lit_area(in_len);
        if ntriples * 8 + 32 > self.ocap - lit_cap {
            return Err(format!("lz77_output_insanity_seqs={ntriples}"));
        }
        let slot = ring % self.src.len();
        let (optr, _) = self.out[slot];
        let seq_area = unsafe { optr.add(lit_cap) };
        seqs.clear();
        seqs.extend_from_slice(unsafe { std::slice::from_raw_parts(seq_area, ntriples * 8) });
        Ok(())
    }

    fn doorbell(&mut self, cmd: u8, idx: u16, pri: u8) {
        // ring writes must be visible before the doorbell store
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        let db = doorbell_value(self.sqn, cmd, idx, pri);
        unsafe { (self.mmio.offset(self.db_offset as isize) as *mut u64).write_volatile(db) };
        self.stats.doorbells += 1;
    }

    /// Fill up to `budget` SQEs at the queue's request size; rings the
    /// doorbell every `batch` fills and at every ring-wrap boundary so one
    /// doorbell never spans the wrap.
    fn submit(&mut self, batch: usize, budget: usize) -> usize {
        self.submit_len(batch, budget, self.req_size, true)
    }

    /// [`Queue::submit`] with the programmed parse input length explicit —
    /// a request carrying less than the slot's capacity programs its own
    /// length (the lz77 parse is `in_len - MFLIMIT`).
    fn submit_len(&mut self, batch: usize, budget: usize, in_len: usize, flush: bool) -> usize {
        let mut submitted = 0;
        while submitted < budget && self.outstanding < self.src.len() {
            self.submit_one(in_len, batch);
            submitted += 1;
        }
        if flush {
            self.flush_doorbell();
        }
        submitted
    }

    /// The DUS device-status word before a send: a 1 means a hardware
    /// error happened (uadk v1 qm_tx_update reads it before every send).
    fn check_device_status(&mut self) {
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
                    eprintln!("native: device status register reports a hardware error");
                }
            }
        }
    }

    /// Fill the SQE at `sq_tail` for one request of `in_len`, advance the
    /// tail, and defer the doorbell to `batch` policy.
    fn submit_one(&mut self, in_len: usize, batch: usize) {
        self.check_device_status();
        let i = self.sq_tail;
        let slot = i as usize % self.src.len();
        let (_sptr, sdma) = self.src[slot];
        let (_optr, odma) = self.out[slot];
        let sqe = unsafe { &mut *(self.sq.add(i as usize * QM_ZIP_SQE_SIZE) as *mut Sqe) };
        match self.alg {
            NativeAlg::Lz77 => fill_lz77_sqe(sqe, sdma, odma, in_len, self.ocap),
            NativeAlg::Deflate => fill_deflate_sqe(sqe, sdma, odma, in_len, self.ocap),
        }
        sqe.tag_l = i as u32;
        self.sq_tail = (i + 1) % QM_Q_DEPTH as u16;
        self.outstanding += 1;
        self.unbelled += 1;
        if self.unbelled >= batch || self.sq_tail == 0 {
            self.doorbell(DOORBELL_CMD_SQ, self.sq_tail, 0);
            self.unbelled = 0;
        }
    }

    /// Ring the SQ doorbell if streaming submits left requests unannounced
    /// (`submit_len` with `flush=false` defers to the batch size); a
    /// submitted request must never sit unbelled while its caller waits.
    fn flush_doorbell(&mut self) {
        if self.unbelled > 0 {
            self.doorbell(DOORBELL_CMD_SQ, self.sq_tail, 0);
            self.unbelled = 0;
        }
    }

    /// Claim the queue's next ring position for one streaming request
    /// (FIFO from `sq_tail`), or None when every input buffer is still
    /// occupied — the caller re-tries after harvesting completions. The
    /// position stays claimed until [`Queue::release`], which is what
    /// lets a completion's buffer outlive its CQE until its caller has
    /// read it — and, in the produce pass, until its block has rebuilt
    /// from the input bytes the slot still holds.
    fn claim_slot(&mut self) -> Option<usize> {
        if self.outstanding >= self.src.len() {
            return None;
        }
        let ring = self.sq_tail as usize;
        let slot = ring % self.src.len();
        if self.held[slot] {
            return None;
        }
        self.held[slot] = true;
        Some(ring)
    }

    /// The claimed position's input buffer (`req_size` wide): fill up to
    /// the chunk length, then announce with [`Queue::submit_claimed`].
    fn slot_buf_mut(&mut self, ring: usize) -> &mut [u8] {
        let (sptr, _) = self.src[ring % self.src.len()];
        unsafe { std::slice::from_raw_parts_mut(sptr, self.req_size) }
    }

    /// A still-claimed position's input as filled: its first `len` bytes.
    /// Until the position is released nothing else writes the buffer, so
    /// these are exactly the bytes the engine parsed.
    fn slot_buf(&self, ring: usize, len: usize) -> &[u8] {
        let (sptr, _) = self.src[ring % self.src.len()];
        unsafe { std::slice::from_raw_parts(sptr, len) }
    }

    /// Announce a claimed position whose buffer the caller filled with
    /// `len` bytes (within the request size; the lz77 parse programs
    /// `len - MFLIMIT`): fill its SQE and defer the doorbell to `batch`
    /// policy. Claims submit in FIFO order, so `ring` must be `sq_tail`.
    fn submit_claimed(&mut self, ring: usize, len: usize, batch: usize) {
        debug_assert!(self.held[ring % self.src.len()]);
        debug_assert_eq!(ring, self.sq_tail as usize);
        debug_assert!(len <= self.req_size);
        self.submit_one(len, batch.max(1));
    }

    /// [`Queue::claim_slot`] plus the copy into the claimed position's
    /// buffer and [`Queue::submit_claimed`] — the slice-mode streaming
    /// submit, where the caller's bytes move on the pump thread.
    fn try_submit(&mut self, batch: usize, chunk: &[u8]) -> Option<usize> {
        let ring = self.claim_slot()?;
        let buf = self.slot_buf_mut(ring);
        buf[..chunk.len()].copy_from_slice(chunk);
        self.submit_claimed(ring, chunk.len(), batch);
        Some(ring)
    }

    /// Hand a streaming position back: the buffer at `ring` is refillable.
    fn release(&mut self, ring: usize) {
        let slot = ring % self.src.len();
        self.held[slot] = false;
    }

    /// Reap every completed CQE. The CQ doorbell (advanced cq_head + the
    /// interrupt-arm flag) rings once per call, matching uadk's
    /// per-recv-call rx_update granularity.
    fn harvest(&mut self) -> usize {
        self.harvest_rings(None)
    }

    /// [`Queue::harvest`] that also reports each completion's SQE ring
    /// index (the CQE's `sq_head`) as arrived. The vendor stack treats
    /// completions as FIFO and matches responses by CQ position alone;
    /// the ring logging keeps that assumption checkable against the
    /// hardware's own words.
    fn harvest_rings(&mut self, mut rings: Option<&mut Vec<usize>>) -> usize {
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
                    eprintln!("native: hw status 0x{status:x} (ring idx {sq_head})");
                }
            } else {
                let lit = u32::from_le(sqe.comp_data_length) as u64;
                let seqs = u32::from_le(sqe.produced) as u64;
                match self.alg {
                    NativeAlg::Lz77 => {
                        let lit_cap = lz77_lit_area(self.req_size) as u64;
                        let seq_cap = (self.ocap as u64).saturating_sub(lit_cap);
                        if lit > lit_cap || seqs * 8 > seq_cap {
                            self.stats.hw_errors += 1;
                            if self.stats.hw_errors <= 8 {
                                eprintln!("native: lz77 output insanity lit={lit} seqs={seqs}");
                            }
                        }
                        self.stats.bytes_out += lit + seqs * 8;
                    }
                    NativeAlg::Deflate => self.stats.bytes_out += seqs,
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
            if let Some(v) = rings.as_deref_mut() {
                v.push(sq_head);
            }
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

/// Aggregated pump result of one [`NativePump::run`].
pub(crate) struct NativeStats {
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

/// One completed lz77 sample request: the sequence triples plus the
/// completion fields the rebuild reports on failure.
pub struct Lz77Sample {
    /// `triples * 8` bytes from the sequence area
    pub seqs: Vec<u8>,
    /// literal byte count (comp_data_length)
    pub lit: usize,
    /// the engine's literals area (`lit` bytes): the rebuild takes
    /// literals from the input instead, so the area is the ground truth
    /// to diff against when a rebuilt block decodes wrong
    pub lits: Vec<u8>,
    /// literal-length overflow report (checksum dw): count and position
    pub ll_overflow_cnt: u32,
    pub ll_overflow_pos: u32,
    /// the 8 overflow bytes the engine writes right after the triples
    /// (uadk OVERFLOW_DATA_SIZE; their contents feed the freq block)
    pub overflow: Vec<u8>,
}

/// A pool of hardware queues driven from one thread: submit everything,
/// harvest everything, repeat. `run` reports engine-side throughput with
/// zero payload copies in the loop (the workload sits in DMA-visible SS
/// memory once at setup).
pub(crate) struct NativePump {
    queues: Vec<Queue>,
    batch: usize,
    wait_poll: bool,
}

impl NativePump {
    /// Open `queues` hardware queues round-robin over `devices`.
    pub fn open(
        devices: &[String],
        queues: usize,
        req_size: usize,
        depth: usize,
        batch: usize,
        wait_poll: bool,
        alg: NativeAlg,
    ) -> Result<NativePump, String> {
        if devices.is_empty() {
            return Err("no /dev/hisi_zip-* devices".into());
        }
        if alg == NativeAlg::Lz77 && req_size > MAX_LZ77_REQ {
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
                        eprintln!("native: opened {q} of {queues} queues before: {e}");
                    }
                    return Err(e);
                }
            }
        }
        Ok(NativePump {
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

    /// Compress one lz77 request on queue 0 and return its sequence
    /// triples; `input` may fill the request size up to full capacity. For
    /// the cross-validation sample, after a run has drained; the 10 s
    /// wait bounds a wedged ring instead of timing a live request.
    pub fn sample_triples(&mut self, input: &[u8]) -> Result<Lz77Sample, String> {
        self.request_triples(input, 10.0)
    }

    /// [`NativePump::sample_triples`] with the completion wait explicit —
    /// the per-block engine path times out on `timeout_s` and fails the
    /// request to its software fallback instead of hanging.
    pub fn request_triples(&mut self, input: &[u8], timeout_s: f64) -> Result<Lz77Sample, String> {
        self.queues[0].request(input, self.batch, timeout_s)
    }

    /// Run `chunks` (each within the request size) through queue 0 as a
    /// ring of in-flight requests — waves of the queue depth, every wave
    /// waited out (poll when the queue was opened for it) before its
    /// results are read — and return one result per chunk, in chunk order.
    /// The queue must be idle at entry (`&mut self` callers guarantee it).
    /// A chunk that cannot complete within `timeout_s` fails itself and
    /// every later chunk; completed chunks keep their samples, so the
    /// caller can soft-fallback exactly the failed requests' blocks.
    pub fn request_batch(
        &mut self,
        chunks: &[&[u8]],
        timeout_s: f64,
    ) -> Vec<Result<Lz77Sample, String>> {
        if chunks.is_empty() {
            return Vec::new();
        }
        let q = &mut self.queues[0];
        if q.outstanding != 0 {
            return (0..chunks.len())
                .map(|_| Err("queue_busy".into()))
                .collect();
        }
        let depth = q.src.len();
        let mut out: Vec<Result<Lz77Sample, String>> = Vec::with_capacity(chunks.len());
        let mut k = 0usize;
        while k < chunks.len() {
            let n = (chunks.len() - k).min(depth);
            let start = q.sq_tail as usize;
            let wave = &chunks[k..k + n];
            if wave.iter().any(|c| c.len() > q.req_size) {
                // caller bug: fail this wave and everything after it
                while out.len() < chunks.len() {
                    out.push(Err("chunk_over_request_size".into()));
                }
                return out;
            }
            for (j, chunk) in wave.iter().enumerate() {
                let slot = (start + j) % depth;
                let (sptr, _) = q.src[slot];
                let slot_buf = unsafe { std::slice::from_raw_parts_mut(sptr, q.req_size) };
                slot_buf[..chunk.len()].copy_from_slice(chunk);
                q.submit_len(self.batch.max(1), 1, chunk.len(), true);
            }
            // wait the whole wave out. Completions are FIFO on one queue
            // (the vendor stack matches responses by CQ position alone),
            // so wave position j reads the ring submitted at j — the ring
            // logging under KAE_NATIVE_DEBUG_RING exists to re-examine
            // that assumption from the hardware's own words if a block
            // ever misbuilds again.
            let wave_start = start % QM_Q_DEPTH;
            let debug_ring = std::env::var("KAE_NATIVE_DEBUG_RING").as_deref() == Ok("1");
            let mut done: Vec<usize> = Vec::with_capacity(n);
            let t0 = now_s();
            let mut timed_out = false;
            while q.outstanding > 0 {
                let reaped = q.harvest_rings(Some(&mut done));
                if reaped == 0 && q.c_flag != 0 {
                    q.poll_once(100);
                }
                if now_s() - t0 > timeout_s {
                    timed_out = true;
                    break;
                }
            }
            if timed_out {
                while out.len() < chunks.len() {
                    out.push(Err("request_did_not_complete_in_time".into()));
                }
                return out;
            }
            if debug_ring {
                let in_order = done
                    .iter()
                    .enumerate()
                    .all(|(i, &r)| r == (wave_start + i) % QM_Q_DEPTH);
                eprintln!(
                    "native: wave rings {}..{} ({} reqs): completions in_order={} arrival={:?}",
                    wave_start,
                    (wave_start + n) % QM_Q_DEPTH,
                    n,
                    in_order,
                    done
                );
            }
            for (j, chunk) in wave.iter().enumerate() {
                let ring = (wave_start + j) % QM_Q_DEPTH;
                out.push(q.read_result(ring, chunk.len()));
            }
            k += n;
        }
        out
    }

    /// The streaming counterpart of [`NativePump::request_batch`]: claim
    /// the queue's next ring position for `chunk` and submit it, returning
    /// the SQE ring index it was submitted at, or None when every input
    /// buffer is still occupied. Ring positions stay claimed until
    /// [`NativePump::release`], so a completion's buffer survives until its
    /// caller has read it — completions pair by the CQE's `sq_head`, never
    /// by arrival order. The doorbell defers to `batch` policy; call
    /// [`NativePump::flush_doorbell`] once per refill round.
    pub fn try_submit(&mut self, batch: usize, chunk: &[u8]) -> Option<usize> {
        self.queues[0].try_submit(batch, chunk)
    }

    /// The produce-pass counterpart of [`NativePump::try_submit`]: claim
    /// the queue's next ring position and hand back its input buffer to
    /// fill (up to the request size) — no byte moves on the pump side.
    /// Announce with [`NativePump::submit_claimed`], or hand an
    /// unsubmitted position back with [`NativePump::release`].
    pub fn claim(&mut self) -> Option<(usize, &mut [u8])> {
        let ring = self.queues[0].claim_slot()?;
        let buf = self.queues[0].slot_buf_mut(ring);
        Some((ring, buf))
    }

    /// Announce a claimed position whose buffer the caller filled with
    /// `len` bytes; the doorbell defers to `batch` policy
    /// ([`NativePump::flush_doorbell`] once per refill round).
    pub fn submit_claimed(&mut self, ring: usize, len: usize, batch: usize) {
        self.queues[0].submit_claimed(ring, len, batch)
    }

    /// A still-claimed position's input as filled — its first `len`
    /// bytes. Until the position is released nothing rewrites the buffer,
    /// so in the produce pass these are the only bytes the rebuild can
    /// take the block's literals from.
    pub fn slot_input(&self, ring: usize, len: usize) -> &[u8] {
        self.queues[0].slot_buf(ring, len)
    }

    /// Hand a streaming position back: the buffer at `ring` is refillable.
    pub fn release(&mut self, ring: usize) {
        self.queues[0].release(ring)
    }

    /// Harvest every completed CQE, appending its SQE ring index to `rings`
    /// in arrival order; returns the count. Non-blocking.
    pub fn harvest_completions(&mut self, rings: &mut Vec<usize>) -> usize {
        self.queues[0].harvest_rings(Some(rings))
    }

    /// Copy a completed request's triples into `seqs` (cleared first; the
    /// literal area is not copied — see [`Queue::read_seqs`]).
    pub fn read_seqs(&self, ring: usize, in_len: usize, seqs: &mut Vec<u8>) -> Result<(), String> {
        self.queues[0].read_seqs(ring, in_len, seqs)
    }

    /// Copy a completed request's full sample (triples + literal area) —
    /// the debug verifier's view of a streaming completion.
    pub fn read_sample(&self, ring: usize, in_len: usize) -> Result<Lz77Sample, String> {
        self.queues[0].read_result(ring, in_len)
    }

    /// Requests still in flight.
    pub fn busy(&self) -> bool {
        self.queues[0].outstanding != 0
    }

    /// Input buffers per queue — the width of a streaming session.
    pub fn depth(&self) -> usize {
        self.queues[0].src.len()
    }

    /// Announce streaming submits whose doorbell `batch` is still open.
    pub fn flush_doorbell(&mut self) {
        self.queues[0].flush_doorbell();
    }

    /// One wait step for a streaming loop: poll(2) on the queue fds in
    /// poll mode (returns on the first completion or after the poll
    /// timeout), a spin hint in spin mode. Call when requests are in
    /// flight and the last harvest came up empty.
    pub fn wait_once(&self) {
        if self.queues[0].c_flag != 0 {
            self.poll_once(100);
        } else {
            std::hint::spin_loop();
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
    pub fn run(&mut self, seconds: f64) -> NativeStats {
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
        let mut st = NativeStats {
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
/// `cargo test -- --ignored`. These validate the native pump's engine dialog
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

    /// A random 256-byte page tiled to size. Each stateless request only
    /// matches against its own bytes, so the page must stay far below the
    /// request size — a request of pure noise completes as negacompress
    /// (status 0xd), which the pump counts as a hardware error.
    fn compressible(n: usize, seed: u64) -> Vec<u8> {
        let page = seeded(256, seed);
        let mut v = vec![0u8; n];
        for (i, b) in v.iter_mut().enumerate() {
            *b = page[i % page.len()];
        }
        v
    }

    fn one_engine_run(alg: NativeAlg) -> NativeStats {
        let devices = zip_devices();
        assert!(!devices.is_empty(), "needs /dev/hisi_zip-* uacce nodes");
        let mut pump =
            NativePump::open(&devices, 1, 4096, 8, 8, false, alg).expect("open native queue");
        pump.load_data(&compressible(4096 * 16, 0xDEAD_BEEF));
        let st = pump.run(0.5);
        assert!(st.reqs_done > 0, "engine completed no requests");
        assert_eq!(st.hw_errors, 0, "engine reported hardware errors");
        st
    }

    #[test]
    #[ignore = "requires /dev/hisi_zip-* uacce nodes: run with cargo test -- --ignored"]
    fn hw_deflate_requests_complete() {
        let st = one_engine_run(NativeAlg::Deflate);
        assert!(st.bytes_out > 0, "deflate should produce a stream per request");
    }

    #[test]
    #[ignore = "requires /dev/hisi_zip-* uacce nodes: run with cargo test -- --ignored"]
    fn hw_lz77_requests_complete() {
        let st = one_engine_run(NativeAlg::Lz77);
        assert!(st.bytes_out > 0, "lz77 triples should not be empty");
    }

    /// The end-to-end output check: compress one request on hardware,
    /// rebuild the triples into a standard LZ4 block, decode it, and
    /// require the original bytes back. The triple layout this exercises
    /// is pinned offline by the rebuild module's known-answer tests.
    #[test]
    #[ignore = "requires /dev/hisi_zip-* uacce nodes: run with cargo test -- --ignored"]
    fn hw_lz77_sample_rebuilds_to_decodable_lz4() {
        let devices = zip_devices();
        assert!(!devices.is_empty(), "needs /dev/hisi_zip-* uacce nodes");
        let req = 4096;
        let mut pump = NativePump::open(&devices, 1, req, 8, 8, false, NativeAlg::Lz77)
            .expect("open native queue");
        pump.run(0.2); // settle the queue the same way a bench run does
        let data = compressible(req, 0xDEAD_BEEF);
        let sample = pump.sample_triples(&data).expect("sample request");

        let mut block = Vec::new();
        let n = crate::rebuild::lz77_block(&data, &sample.seqs, &mut block).expect("rebuild");
        let mut dst = vec![0u8; data.len()];
        let decoded =
            crate::lz4dec::decode_block(&block[..n], &mut dst).expect("decode");
        assert_eq!(decoded, data.len());
        assert_eq!(&dst[..decoded], &data[..], "rebuild output must decode to the input");
    }

    /// The two multi-page shapes the bench requests hit at page
    /// boundaries: a text page pair with a 17-byte pattern period, and a
    /// text -> zeros page pair (the parse switches from a periodic offset
    /// to offset-1 runs). The vendor oracle is kaelz4's own LZ4 block for
    /// the same input: the patched liblz4 compresses through the same
    /// engine and converts its triples 1:1 (kaelz4_triples_rebuild), so
    /// both blocks encode the same kind of parse. The asserted contract:
    ///
    /// - our rebuild decodes back to the input;
    /// - our parse equals the vendor's parse once same-offset matches
    ///   merge into runs, literal gaps included. Byte equality is not
    ///   required — the engine splits a long same-offset run at a
    ///   different point on every request (one input, three consecutive
    ///   parses: 252|48|252x14|248, 204|252x15|92, 44|192|252x15|60 over
    ///   the same 20..4096 run) and sometimes emits a few redundant
    ///   literals mid-run, so no two blocks of one input chop the runs
    ///   the same way;
    /// - a resample of the same request normalizes to the same runs.
    ///
    /// This one test loads the vendor stack; the other native tests keep
    /// the process free of it. Everything is printed before any assert so
    /// one on-target run reads out the rule at fault.
    #[test]
    #[ignore = "requires /dev/hisi_zip-* uacce nodes: run with cargo test -- --ignored"]
    fn hw_lz77_page_boundary_rebuilds_to_decodable_lz4() {
        let devices = zip_devices();
        assert!(!devices.is_empty(), "needs /dev/hisi_zip-* uacce nodes");
        let req = 8192;
        let mut pump = NativePump::open(&devices, 1, req, 8, 8, false, NativeAlg::Lz77)
            .expect("open native queue");
        pump.run(0.2); // settle the queue the same way a bench run does
        let mut data = vec![0u8; 20 * 4096];
        crate::data::fill_pages(&mut data);
        let vendor = crate::KaeLz4::open_hw().expect("open the patched liblz4 stack");
        let hx = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();

        let mut problems = Vec::new();
        for first_page in [9, 18] {
            let chunk = &data[first_page * 4096..first_page * 4096 + req];
            // sample the same request twice: consecutive parses of one
            // input may split matches differently, never cover new spans
            let sample = pump.sample_triples(chunk).expect("sample request");
            let resample = pump.sample_triples(chunk).expect("resample request");
            eprintln!(
                "== pages {first_page}-{}: seqs={} lit={} ll_overflow_cnt={} pos={}",
                first_page + 1,
                sample.seqs.len() / 8,
                sample.lit,
                sample.ll_overflow_cnt,
                sample.ll_overflow_pos
            );
            eprintln!(
                "native: lit area {}B: {}",
                sample.lits.len(),
                hx(&sample.lits)
            );
            eprintln!("native: run 1 triples:");
            for line in crate::rebuild::trace_triples(chunk.len(), &sample.seqs).lines() {
                eprintln!("native: {line}");
            }
            eprintln!("native: run 2 triples:");
            for line in crate::rebuild::trace_triples(chunk.len(), &resample.seqs).lines() {
                eprintln!("native: {line}");
            }
            if sample.seqs == resample.seqs {
                eprintln!("native: run 2 triples identical to run 1");
            }

            let mut block = Vec::new();
            let n = crate::rebuild::lz77_block(chunk, &sample.seqs, &mut block).expect("rebuild");
            let mut dst = vec![0u8; chunk.len()];
            let mut decoded = false;
            match crate::lz4dec::decode_block(&block[..n], &mut dst) {
                Ok(m) if &dst[..m] == chunk => {
                    eprintln!("native: rebuild decodes to the input ({n}B)");
                    decoded = true;
                }
                Ok(m) => {
                    let d = dst[..m].iter().zip(chunk).position(|(a, b)| a != b);
                    problems.push(format!(
                        "pages {first_page}: rebuild decodes to {m}B, first diff at {d:?}"
                    ));
                }
                Err(e) => problems.push(format!(
                    "pages {first_page}: rebuild decode failed: {e}"
                )),
            }

            let vblock = vendor.compress_vec(chunk).expect("vendor compress");
            let mut vdst = vec![0u8; chunk.len()];
            let vn = crate::lz4dec::decode_block(&vblock, &mut vdst)
                .expect("vendor block decodes");
            assert_eq!(&vdst[..vn], chunk, "vendor oracle sanity");
            eprintln!("native: vendor block {}B:", vblock.len());
            for line in lz4_sequences(&vblock) {
                eprintln!("vendor: {line}");
            }
            if !decoded {
                continue;
            }

            // Same engine either side, so the parses must agree; only the
            // chop of a long same-offset run is free to differ.
            let ours = walk_lz4_block(&block[..n]);
            let theirs = walk_lz4_block(&vblock);
            let our_runs = match_runs(&ours);
            if our_runs != match_runs(&theirs) || ours.tail_len != theirs.tail_len {
                let at = runs_mismatch(&our_runs, &match_runs(&theirs)).unwrap_or_else(|| {
                    format!("tail {}B vs {}B", ours.tail_len, theirs.tail_len)
                });
                problems.push(format!(
                    "pages {first_page}: parse diverges from the vendor oracle ({at})"
                ));
                continue;
            }

            // the resample may chop differently but must cover the same runs
            let mut rblock = Vec::new();
            let rn = crate::rebuild::lz77_block(chunk, &resample.seqs, &mut rblock)
                .expect("rebuild resample");
            let mut rdst = vec![0u8; chunk.len()];
            match crate::lz4dec::decode_block(&rblock[..rn], &mut rdst) {
                Ok(m) if &rdst[..m] == chunk => {
                    let resampled = match_runs(&walk_lz4_block(&rblock[..rn]));
                    if resampled != our_runs {
                        let at = runs_mismatch(&resampled, &our_runs)
                            .expect("mismatch reports the differing run");
                        problems.push(format!(
                            "pages {first_page}: resample parse diverges from run 1 ({at})"
                        ));
                    }
                }
                Ok(m) => problems.push(format!(
                    "pages {first_page}: resample rebuild decodes to {m}B, not the input"
                )),
                Err(e) => problems.push(format!(
                    "pages {first_page}: resample rebuild decode failed: {e}"
                )),
            }

            if block[..n] == vblock[..] {
                eprintln!("native: rebuild matches the vendor oracle byte-for-byte");
                continue;
            }
            let d = block[..n].iter().zip(vblock.iter()).position(|(a, b)| a != b);
            eprintln!(
                "native: rebuild {n}B vs vendor {}B, first diff at {d:?} \
                 (parse equal across {} runs; split variance only)",
                vblock.len(),
                our_runs.len()
            );
            if let Some(d) = d {
                let lo = d.saturating_sub(16);
                eprintln!("native: rebuild: {}", hx(&block[lo..(d + 16).min(n)]));
                eprintln!(
                    "native: vendor: {}",
                    hx(&vblock[lo..(d + 16).min(vblock.len())])
                );
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("; "));
    }

    /// A block walked end to end: every sequence as (literal length,
    /// literal position in the output, match offset, match length), then
    /// the unmatched tail literal run.
    struct Lz4Walk {
        seqs: Vec<(usize, usize, usize, usize)>,
        tail_at: usize,
        tail_len: usize,
    }

    /// Walk a well-formed LZ4 block sequence by sequence; a block always
    /// ends with a literal run, recorded as the tail.
    fn walk_lz4_block(block: &[u8]) -> Lz4Walk {
        let mut seqs = Vec::new();
        let (mut i, mut out) = (0usize, 0usize);
        while i < block.len() {
            let tok = block[i];
            i += 1;
            let mut lit = (tok >> 4) as usize;
            if lit == 15 {
                loop {
                    let b = *block.get(i).expect("literal extension") as usize;
                    i += 1;
                    lit += b;
                    if b != 255 {
                        break;
                    }
                }
            }
            let lit_at = out;
            i += lit;
            out += lit;
            if i == block.len() {
                return Lz4Walk {
                    seqs,
                    tail_at: lit_at,
                    tail_len: lit,
                };
            }
            let off = u16::from_le_bytes(block[i..i + 2].try_into().unwrap()) as usize;
            i += 2;
            let mut ml = (tok & 15) as usize + 4;
            if (tok & 15) == 15 {
                loop {
                    let b = *block.get(i).expect("match extension") as usize;
                    i += 1;
                    ml += b;
                    if b != 255 {
                        break;
                    }
                }
            }
            seqs.push((lit, lit_at, off, ml));
            out += ml;
        }
        unreachable!("block ends mid-sequence at {i}");
    }

    /// The chop-invariant form of a parse: same-offset matches that
    /// continue a run merge into one (start, end, offset) run, including
    /// across the few redundant literals the engine sometimes drops
    /// mid-run. Two parses of one input compare equal exactly when they
    /// cover the same spans at the same offsets; callers assert both
    /// blocks decode to the input first, which is what makes a mid-run
    /// literal gap redundant rather than a parse change.
    fn match_runs(walk: &Lz4Walk) -> Vec<(usize, usize, usize)> {
        let mut runs: Vec<(usize, usize, usize)> = Vec::new();
        for &(lit, lit_at, off, ml) in &walk.seqs {
            let start = lit_at + lit;
            let extends = runs
                .last()
                .is_some_and(|&(_, end, run_off)| run_off == off && start >= end);
            if extends {
                runs.last_mut().unwrap().1 = start + ml;
            } else {
                runs.push((start, start + ml, off));
            }
        }
        runs
    }

    /// First divergence between two run lists, for a failure message.
    fn runs_mismatch(a: &[(usize, usize, usize)], b: &[(usize, usize, usize)]) -> Option<String> {
        let i = a
            .iter()
            .zip(b)
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len()));
        (i != a.len() || a.len() != b.len())
            .then(|| format!("run {i}: {:?} vs {:?}", a.get(i), b.get(i)))
    }

    /// Walk a well-formed LZ4 block and describe each sequence: the
    /// literal run's input span, then the match's offset, length, and
    /// output span. One line each, so a hardware-run log carries the
    /// vendor's exact parse without wrapped hex.
    fn lz4_sequences(block: &[u8]) -> Vec<String> {
        let walk = walk_lz4_block(block);
        let mut lines = Vec::new();
        for &(lit, lit_at, off, ml) in &walk.seqs {
            let at = lit_at + lit;
            lines.push(format!(
                "seq: lit {lit} at {lit_at}..{at} match off {off} ml {ml} span {at}..{}",
                at + ml
            ));
        }
        lines.push(format!(
            "final: lit {} at {}..{}",
            walk.tail_len,
            walk.tail_at,
            walk.tail_at + walk.tail_len
        ));
        lines
    }

    /// The oracle comparison normalizes parses through `walk_lz4_block`
    /// plus `match_runs`; both must see one split sequence and one merged
    /// run as the same coverage. No hardware needed.
    #[test]
    fn lz4_walk_merges_same_offset_splits_into_runs() {
        // one LZ77_ONLY triple: [u32 litLength][u16 offBase][u16 mlBase]
        let triple = |off: usize, lit: u32, ml_base: u16| {
            let mut t = [0u8; 8];
            t[0..4].copy_from_slice(&lit.to_le_bytes());
            t[4..6].copy_from_slice(&(off.saturating_sub(1) as u16).to_le_bytes());
            t[6..8].copy_from_slice(&ml_base.to_le_bytes());
            t
        };
        let input = b"abababababababab";
        let block = |seqs: &[u8]| {
            let mut out = Vec::new();
            let n = crate::rebuild::lz77_block(input, seqs, &mut out).expect("rebuild");
            out.truncate(n);
            out
        };
        // one 12-byte match at offset 2, and the same coverage chopped in
        // two same-offset matches: different blocks, one run
        let merged = walk_lz4_block(&block(&[triple(2, 2, 9)].concat()));
        let split = walk_lz4_block(&block(&[triple(2, 2, 3), triple(2, 0, 3)].concat()));
        assert_eq!(merged.seqs, [(2, 0, 2, 12)]);
        assert_eq!(split.seqs, [(2, 0, 2, 6), (0, 8, 2, 6)]);
        assert_eq!((merged.tail_at, merged.tail_len), (14, 2));
        assert_eq!((split.tail_at, split.tail_len), (14, 2));
        assert_eq!(match_runs(&merged), vec![(2, 14, 2)]);
        assert_eq!(match_runs(&split), match_runs(&merged));
        // a same-offset continuation stays in the run even across the
        // redundant literals the engine drops mid-run; a different
        // offset starts a new run
        let gapped = walk_lz4_block(&block(&[triple(2, 2, 3), triple(2, 2, 3)].concat()));
        assert_eq!(gapped.seqs, [(2, 0, 2, 6), (2, 8, 2, 6)]);
        assert_eq!(match_runs(&gapped), vec![(2, 16, 2)]);
        let apart = walk_lz4_block(&block(&[triple(2, 2, 3), triple(4, 2, 3)].concat()));
        assert_eq!(match_runs(&apart), vec![(2, 8, 2), (10, 16, 4)]);
    }
}

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
        let b = sqe_bytes(|s| fill_lz77_sqe(s, 0x1234_5678_9abc_def0, 0xfeed_0000_0000_1000, 65536, out_cap(NativeAlg::Lz77, 65536)));
        let dw = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(dw(9) & 0xff, HW_LZ77_ZSTD_NOREPCODE); // algorithm
        assert_eq!(dw(9) >> WINDOWS_SIZE_SHIFT & 0xf, WS_16K);
        assert_eq!(dw(13), 65536 - MFLIMIT as u32 + 16); // literals capacity
        assert_eq!(
            dw(14),
            out_cap(NativeAlg::Lz77, 65536) as u32 - (65536 - MFLIMIT as u32 + 16)
        ); // seq capacity
        assert_eq!(dw(4), 65536 - MFLIMIT as u32); // parse length: tail reserved
        assert_eq!(dw(7), (0b101 << 25) | (0b11 << 28)); // NEW/stateless/FINISH/BD3
        // source at dwords 18/19
        assert_eq!((dw(19), dw(18)), (0x1234_5678, 0x9abc_def0));
        // literals (cipher_key pair) at the output start: dwords 24/25
        assert_eq!((dw(25), dw(24)), (0xfeed_0000, 0x0000_1000));
        // sequences after the literal area: dest pair at dwords 20/21
        let seq = 0xfeed_0000_0000_1000u64 + 65536 - MFLIMIT as u64 + 16;
        assert_eq!((dw(21), dw(20)), ((seq >> 32) as u32, seq as u32));
    }

    #[test]
    fn deflate_sqe_known_answer() {
        let cap = out_cap(NativeAlg::Deflate, 65536);
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
            held: vec![false; 2],
            c_flag: 0,
            alg: NativeAlg::Lz77,
            req_size: 4096,
            ocap: out_cap(NativeAlg::Lz77, 4096),
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
