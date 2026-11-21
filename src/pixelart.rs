//! Pixel-art detection and lossless downscale to 1×.
//!
//! An image is pixel art when every aligned N×N block has one colour, with N > 1.
//! N is the GCD of all run lengths of equal pixels, along rows and columns. Runs
//! that touch the edge count too, so N always divides the width and the height.
//!
//! An image with N = 1 is pixel art stored at 1× when it is small and has few
//! colours: see `MAX_1X_SIDE` and `Palette`.

use std::collections::HashSet;

/// The decode error for an image that is not pixel art.
pub const NOT_PIXEL_ART: &str = "the image is not true pixel art";

/// Art stored at 1× has no side longer than this.
pub const MAX_1X_SIDE: u32 = 640;
/// Art stored at 1× has no more colours than this, counted across all frames.
pub const MAX_1X_COLOURS: usize = 256;

/// The colours of the frames of an image, counted up to `MAX_1X_COLOURS`.
#[derive(Default)]
pub struct Palette(HashSet<u32>);

impl Palette {
    /// Adds the colours of `buf`. False when there are too many colours.
    pub fn add(&mut self, buf: &[u32]) -> bool {
        let mut last = None;
        for p in buf {
            if last != Some(*p) {
                last = Some(*p);
                self.0.insert(*p);
                if self.0.len() > MAX_1X_COLOURS {
                    return false;
                }
            }
        }
        true
    }
}

/// GCD of all run lengths of equal pixels in rows and columns.
///
/// When `limit > 0`, the result is a divisor of `limit`. Returns 1 as soon as the
/// GCD becomes 1.
pub fn factor(buf: &[u32], w: u32, h: u32, limit: u32) -> u32 {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 {
        return 1;
    }
    let mut g = limit as usize;
    // vertical run length of each column
    let mut vrun = vec![1usize; w];

    for y in 0..h {
        let row = &buf[y * w..(y + 1) * w];
        if y > 0 {
            let prev = &buf[(y - 1) * w..y * w];
            if row == prev {
                // same row: horizontal runs are the same as before, columns continue
                vrun.iter_mut().for_each(|r| *r += 1);
                continue;
            }
            for (x, r) in vrun.iter_mut().enumerate() {
                if row[x] == prev[x] {
                    *r += 1;
                } else {
                    g = gcd(g, *r);
                    *r = 1;
                    if g == 1 {
                        return 1;
                    }
                }
            }
        }

        let mut run = 1;
        for x in 1..w {
            if row[x] == row[x - 1] {
                run += 1;
            } else {
                g = gcd(g, run);
                run = 1;
                if g == 1 {
                    return 1;
                }
            }
        }
        g = gcd(g, run);
        if g == 1 {
            return 1;
        }
    }

    for r in vrun {
        g = gcd(g, r);
        if g == 1 {
            return 1;
        }
    }
    g as u32
}

/// True when every aligned n×n block of `buf` has one colour.
pub fn holds(buf: &[u32], w: u32, h: u32, n: u32) -> bool {
    if n <= 1 {
        return true;
    }
    if !w.is_multiple_of(n) || !h.is_multiple_of(n) {
        return false;
    }
    let (w, h, n) = (w as usize, h as usize, n as usize);
    for by in (0..h).step_by(n) {
        let first = &buf[by * w..(by + 1) * w];
        for seg in first.chunks_exact(n) {
            if !seg.iter().all(|p| *p == seg[0]) {
                return false;
            }
        }
        for y in by + 1..by + n {
            if &buf[y * w..(y + 1) * w] != first {
                return false;
            }
        }
    }
    true
}

/// Takes the top-left pixel of each n×n block. Lossless when `holds(n)` is true.
pub fn shrink(buf: &[u32], w: u32, h: u32, n: u32) -> Vec<u32> {
    let (w, h, n) = (w as usize, h as usize, n as usize);
    let (sw, sh) = (w / n, h / n);
    let mut out = Vec::with_capacity(sw * sh);
    for sy in 0..sh {
        let row = &buf[sy * n * w..];
        out.extend((0..sw).map(|sx| row[sx * n]));
    }
    out
}

/// Smallest integer scale at which the art covers the output. Never less than 1.
pub fn integer_scale(art: (u32, u32), out: (u32, u32)) -> u32 {
    let kx = out.0.div_ceil(art.0.max(1));
    let ky = out.1.div_ceil(art.1.max(1));
    kx.max(ky).max(1)
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Builds an image from a 1× pattern scaled by n.
    pub fn scaled(pattern: &[u32], pw: usize, ph: usize, n: usize) -> (Vec<u32>, u32, u32) {
        let (w, h) = (pw * n, ph * n);
        let mut v = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                v.push(pattern[(y / n) * pw + x / n]);
            }
        }
        (v, w as u32, h as u32)
    }

    pub fn checker(pw: usize, ph: usize) -> Vec<u32> {
        (0..pw * ph)
            .map(|i| {
                if (i % pw + i / pw).is_multiple_of(2) {
                    0xFFFF_0000
                } else {
                    0xFF00_00FF
                }
            })
            .collect()
    }

    /// Small deterministic generator for tests.
    pub struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        pub fn below(&mut self, n: u32) -> u32 {
            self.next() % n
        }
    }

    #[test]
    fn factor_solid() {
        let v = vec![7u32; 48 * 32];
        assert_eq!(factor(&v, 48, 32, 0), 16);
    }

    #[test]
    fn factor_checker() {
        let (v, w, h) = scaled(&checker(5, 4), 5, 4, 3);
        assert_eq!(factor(&v, w, h, 0), 3);
        let (v, w, h) = scaled(&checker(5, 4), 5, 4, 6);
        assert_eq!(factor(&v, w, h, 0), 6);
        assert_eq!(factor(&v, w, h, 4), 2);
    }

    #[test]
    fn factor_noise() {
        let mut rng = Rng(1);
        let v: Vec<u32> = (0..20 * 20).map(|_| rng.next()).collect();
        assert_eq!(factor(&v, 20, 20, 0), 1);
    }

    #[test]
    fn holds_and_shrink() {
        let pat = checker(5, 4);
        let (mut v, w, h) = scaled(&pat, 5, 4, 4);
        assert!(holds(&v, w, h, 4));
        assert!(holds(&v, w, h, 2));
        assert!(!holds(&v, w, h, 3));
        assert_eq!(shrink(&v, w, h, 4), pat);

        // break the 4× grid but keep the 2× grid
        for y in 0..2 {
            for x in 0..2 {
                v[y * w as usize + x] = 0xFF00_FF00;
            }
        }
        assert!(!holds(&v, w, h, 4));
        assert_eq!(factor(&v, w, h, 4), 2);
        assert!(holds(&v, w, h, 2));
    }

    #[test]
    fn palette_limit() {
        let mut p = Palette::default();
        let a: Vec<u32> = (0..200).collect();
        assert!(p.add(&a));
        assert!(p.add(&a));
        let b: Vec<u32> = (144..256).collect();
        assert!(p.add(&b));
        assert!(!p.add(&[256]));
    }

    #[test]
    fn integer_scale_table() {
        assert_eq!(integer_scale((320, 208), (2560, 1664)), 8);
        assert_eq!(integer_scale((320, 208), (2940, 1912)), 10);
        assert_eq!(integer_scale((320, 208), (1920, 1080)), 6);
        assert_eq!(integer_scale((100, 100), (160, 160)), 2);
        assert_eq!(integer_scale((100, 100), (159, 159)), 2);
        assert_eq!(integer_scale((3000, 200), (2560, 1664)), 9);
        assert_eq!(integer_scale((4000, 3000), (1920, 1080)), 1);
    }
}
