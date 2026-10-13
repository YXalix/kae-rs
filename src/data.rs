//! Deterministic synthetic data with a 4K-page-interleaved layout:
//! 50% all-zero pages + 20% random pages + 30% text pages, approximating
//! the compressibility profile of a VM memory snapshot. Random pages come
//! from a fixed-seed xorshift64* so results are comparable across machines
//! and runs.

use std::fs;
use std::io::Write;
use std::path::Path;

/// Page granularity of the interleaved layout.
const PAGE: usize = 4096;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

fn text_page(i: usize, out: &mut [u8]) {
    let pattern = format!("snapshot-page-{} ", i % 97);
    let pat = pattern.as_bytes();
    for (pos, b) in out.iter_mut().enumerate() {
        *b = pat[pos % pat.len()];
    }
}

/// Generate the synthetic dataset of `size_mib` MiB and write it to `path`.
pub fn generate(size_mib: usize, path: &Path) -> std::io::Result<()> {
    let total = size_mib * 1024 * 1024;
    let npages = total / PAGE;
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    let file = fs::File::create(path)?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut page = vec![0u8; PAGE];
    for i in 0..npages {
        let c = i % 10;
        if c < 5 {
            page.iter_mut().for_each(|b| *b = 0);
        } else if c < 7 {
            rng.fill(&mut page);
        } else {
            text_page(i, &mut page);
        }
        w.write_all(&page)?;
    }
    w.flush()?;
    Ok(())
}
