//! Integer upscale of 1× rects into a screen-size buffer, with u32 fill.

use crate::diff::Rect;
use crate::pixelart::integer_scale;

/// How a 1× image maps to a screen buffer: screen pixel (x, y) shows art pixel
/// ((x + ox) / k, (y + oy) / k).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geom {
    /// art size (1×)
    pub w: usize,
    pub h: usize,
    /// screen buffer size
    pub sw: usize,
    pub sh: usize,
    pub k: usize,
    pub ox: usize,
    pub oy: usize,
}

impl Geom {
    pub fn new(art: (u32, u32), screen: (u32, u32)) -> Self {
        let k = integer_scale(art, screen) as usize;
        let (w, h) = (art.0 as usize, art.1 as usize);
        let (sw, sh) = (screen.0 as usize, screen.1 as usize);
        Self {
            w,
            h,
            sw,
            sh,
            k,
            ox: (k * w - sw) / 2,
            oy: (k * h - sh) / 2,
        }
    }

    /// Maps a 1× rect to the screen, clipped. Returns (x0, y0, x1, y1), or `None`
    /// when the rect is fully off-screen.
    pub fn map(&self, r: Rect) -> Option<(usize, usize, usize, usize)> {
        let (x, y, w, h) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        let x0 = (x * self.k).saturating_sub(self.ox);
        let y0 = (y * self.k).saturating_sub(self.oy);
        let x1 = ((x + w) * self.k).saturating_sub(self.ox).min(self.sw);
        let y1 = ((y + h) * self.k).saturating_sub(self.oy).min(self.sh);
        (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
    }
}

/// Draws the 1× rect `r` of `src` into `dst`, scaled by `g.k`.
///
/// For each 1× row, the first screen row is built with `fill` on runs of k pixels,
/// then copied k − 1 times. There is no division per pixel.
pub fn upscale_rect(src: &[u32], dst: &mut [u32], g: &Geom, r: Rect) {
    let Some((x0, y0, x1, y1)) = g.map(r) else {
        return;
    };
    let Geom { k, ox, oy, sw, .. } = *g;
    let (sx0, sx1) = ((x0 + ox) / k, (x1 - 1 + ox) / k);
    let (sy0, sy1) = ((y0 + oy) / k, (y1 - 1 + oy) / k);
    for sy in sy0..=sy1 {
        let ya = (sy * k).saturating_sub(oy).max(y0);
        let yb = ((sy + 1) * k - oy).min(y1);
        let row = ya * sw;
        let srow = &src[sy * g.w..(sy + 1) * g.w];
        let mut a = x0;
        for (sx, px) in srow.iter().enumerate().take(sx1 + 1).skip(sx0) {
            let b = ((sx + 1) * k - ox).min(x1);
            dst[row + a..row + b].fill(*px);
            a = b;
        }
        for y in ya + 1..yb {
            dst.copy_within(row + x0..row + x1, y * sw + x0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixelart::tests::Rng;

    /// Reference render: one division per pixel.
    pub fn full(src: &[u32], g: &Geom) -> Vec<u32> {
        let mut out = vec![0; g.sw * g.sh];
        for y in 0..g.sh {
            for x in 0..g.sw {
                out[y * g.sw + x] = src[(y + g.oy) / g.k * g.w + (x + g.ox) / g.k];
            }
        }
        out
    }

    /// Measurement for SPEC §15: a full 4K upscale into a real shm buffer.
    /// Run: `cargo test --release bench_fill_4k -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_fill_4k() {
        use rustix::mm::{MapFlags, ProtFlags, mmap};
        let (sw, sh) = (3840usize, 2160usize);
        let fd = rustix::fs::memfd_create(c"bench", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&fd, (sw * sh * 4) as u64).unwrap();
        let prot = ProtFlags::READ | ProtFlags::WRITE;
        // SAFETY: a new shared mapping of sw × sh pixels, used only in this test
        let dst = unsafe {
            let ptr = mmap(
                core::ptr::null_mut(),
                sw * sh * 4,
                prot,
                MapFlags::SHARED,
                &fd,
                0,
            )
            .unwrap();
            core::slice::from_raw_parts_mut(ptr.cast::<u32>(), sw * sh)
        };
        let mut rng = Rng(1);
        for (w, h) in [(320u32, 180u32), (640, 360)] {
            let g = Geom::new((w, h), (sw as u32, sh as u32));
            let src: Vec<u32> = (0..w * h).map(|_| rng.next()).collect();
            let all = Rect::new(0, 0, w, h);
            upscale_rect(&src, dst, &g, all); // page faults
            let runs = 50;
            let start = std::time::Instant::now();
            for _ in 0..runs {
                upscale_rect(&src, dst, &g, all);
            }
            let ms = start.elapsed().as_secs_f64() * 1000.0 / runs as f64;
            println!("full 4K fill from {w}x{h} (k = {}): {ms:.2} ms", g.k);
        }
    }

    #[test]
    fn geom_table() {
        let g = Geom::new((7, 5), (30, 17));
        assert_eq!((g.k, g.ox, g.oy), (5, 2, 4));
        let g = Geom::new((8, 8), (4, 2));
        assert_eq!((g.k, g.ox, g.oy), (1, 2, 3));
    }

    #[test]
    fn full_frame_matches_reference() {
        let mut rng = Rng(7);
        for k in 1..=13usize {
            let (w, h) = (9 + rng.below(8), 7 + rng.below(8));
            // sizes that give scale k with odd and even offsets
            let sw = w * k as u32 - rng.below(k as u32);
            let sh = (h * k as u32)
                .saturating_sub(rng.below(3 * k as u32))
                .max(1);
            let g = Geom::new((w, h), (sw, sh));
            assert_eq!(g.k, k);
            let src: Vec<u32> = (0..w * h).map(|_| rng.next()).collect();
            let mut dst = vec![0; g.sw * g.sh];
            let all = Rect::new(0, 0, w, h);
            upscale_rect(&src, &mut dst, &g, all);
            assert_eq!(dst, full(&src, &g), "k = {k}");
        }
    }

    #[test]
    fn dirty_rect_equals_full_render() {
        let mut rng = Rng(42);
        for k in 1..=13usize {
            for _ in 0..40 {
                let (w, h) = (5 + rng.below(20), 5 + rng.below(20));
                let sw = w * k as u32 - rng.below(k as u32);
                let sh = (h * k as u32)
                    .saturating_sub(rng.below(2 * k as u32))
                    .max(1);
                let g = Geom::new((w, h), (sw, sh));
                let prev: Vec<u32> = (0..w * h).map(|_| rng.next()).collect();
                let mut dst = full(&prev, &g);

                // frame k: change pixels only inside a random rect
                let rx = rng.below(w);
                let ry = rng.below(h);
                let rw = 1 + rng.below(w - rx);
                let rh = 1 + rng.below(h - ry);
                let mut cur = prev.clone();
                for y in ry..ry + rh {
                    for x in rx..rx + rw {
                        cur[(y * w + x) as usize] = rng.next();
                    }
                }
                upscale_rect(&cur, &mut dst, &g, Rect::new(rx, ry, rw, rh));
                assert_eq!(dst, full(&cur, &g), "k = {k}, geom = {g:?}");
            }
        }
    }
}
