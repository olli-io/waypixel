//! Tile diff between two frames and merge of the changed tiles into rects.

use std::collections::HashMap;

/// Tile size in 1× pixels.
pub const TILE: usize = 8;
/// Maximum number of rects for one frame.
pub const MAX_RECTS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self {
            x: x as u16,
            y: y as u16,
            w: w as u16,
            h: h as u16,
        }
    }

    pub fn area(&self) -> usize {
        self.w as usize * self.h as usize
    }
}

/// Rect in tile units.
#[derive(Clone, Copy)]
struct TRect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl TRect {
    fn union(self, o: Self) -> Self {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        Self {
            x,
            y,
            w: (self.x + self.w).max(o.x + o.w) - x,
            h: (self.y + self.h).max(o.y + o.h) - y,
        }
    }

    fn area(self) -> usize {
        self.w * self.h
    }
}

/// Marks the changed tiles in `grid` (cols × rows). Returns the count of changed tiles.
fn tiles(prev: &[u32], cur: &[u32], w: usize, h: usize, grid: &mut Vec<bool>) -> usize {
    let (cols, rows) = (w.div_ceil(TILE), h.div_ceil(TILE));
    grid.clear();
    grid.resize(cols * rows, false);
    let mut dirty = 0;
    for ty in 0..rows {
        let (y0, y1) = (ty * TILE, ((ty + 1) * TILE).min(h));
        for tx in 0..cols {
            let (x0, x1) = (tx * TILE, ((tx + 1) * TILE).min(w));
            let d = (y0..y1).any(|y| prev[y * w + x0..y * w + x1] != cur[y * w + x0..y * w + x1]);
            grid[ty * cols + tx] = d;
            dirty += d as usize;
        }
    }
    dirty
}

/// Changed tiles → at most `MAX_RECTS` pixel rects.
fn merge(grid: &[bool], w: usize, h: usize) -> Vec<Rect> {
    let (cols, rows) = (w.div_ceil(TILE), h.div_ceil(TILE));
    // Row runs, then vertical merge of runs with the same x and width.
    let mut rects: Vec<TRect> = Vec::new();
    let mut open: HashMap<(usize, usize), usize> = HashMap::new();
    for ty in 0..rows {
        let mut next = HashMap::new();
        let mut tx = 0;
        while tx < cols {
            if !grid[ty * cols + tx] {
                tx += 1;
                continue;
            }
            let s = tx;
            while tx < cols && grid[ty * cols + tx] {
                tx += 1;
            }
            let key = (s, tx - s);
            let i = match open.get(&key) {
                Some(&i) => {
                    rects[i].h += 1;
                    i
                }
                None => {
                    rects.push(TRect {
                        x: s,
                        y: ty,
                        w: tx - s,
                        h: 1,
                    });
                    rects.len() - 1
                }
            };
            next.insert(key, i);
        }
        open = next;
    }
    // Too many rects for the pair merge: one bounding box per band of tile rows.
    if rects.len() > 64 {
        rects.clear();
        let bh = rows.div_ceil(32);
        for by in (0..rows).step_by(bh) {
            let mut bbox: Option<TRect> = None;
            for ty in by..(by + bh).min(rows) {
                for tx in (0..cols).filter(|tx| grid[ty * cols + tx]) {
                    let t = TRect {
                        x: tx,
                        y: ty,
                        w: 1,
                        h: 1,
                    };
                    bbox = Some(bbox.map_or(t, |b| b.union(t)));
                }
            }
            rects.extend(bbox);
        }
    }
    // Merge the pair with the smallest bounding box until there are few enough.
    while rects.len() > MAX_RECTS {
        let (mut bi, mut bj, mut best) = (0, 1, usize::MAX);
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let a = rects[i].union(rects[j]).area();
                if a < best {
                    (bi, bj, best) = (i, j, a);
                }
            }
        }
        rects[bi] = rects[bi].union(rects[bj]);
        rects.remove(bj);
    }
    rects
        .iter()
        .map(|r| Rect {
            x: (r.x * TILE) as u16,
            y: (r.y * TILE) as u16,
            w: (((r.x + r.w) * TILE).min(w) - r.x * TILE) as u16,
            h: (((r.y + r.h) * TILE).min(h) - r.y * TILE) as u16,
        })
        .collect()
}

/// Reusable diff state.
#[derive(Default)]
pub struct Differ {
    grid: Vec<bool>,
}

impl Differ {
    /// Rects that cover all changes from `prev` to `cur`.
    ///
    /// Empty when the frames are identical. One full-frame rect when the dirty area
    /// is more than 50 % of the frame.
    pub fn diff(&mut self, prev: &[u32], cur: &[u32], w: usize, h: usize) -> Vec<Rect> {
        if tiles(prev, cur, w, h, &mut self.grid) == 0 {
            return Vec::new();
        }
        let rects = merge(&self.grid, w, h);
        if rects.iter().map(Rect::area).sum::<usize>() * 2 > w * h {
            return vec![Rect::new(0, 0, w as u32, h as u32)];
        }
        rects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixelart::tests::Rng;

    fn apply(prev: &[u32], cur: &[u32], w: usize, rects: &[Rect]) -> Vec<u32> {
        let mut out = prev.to_vec();
        for r in rects {
            for y in r.y as usize..(r.y + r.h) as usize {
                let a = y * w + r.x as usize;
                out[a..a + r.w as usize].copy_from_slice(&cur[a..a + r.w as usize]);
            }
        }
        out
    }

    #[test]
    fn identical_is_empty() {
        let v = vec![1u32; 100 * 50];
        assert!(Differ::default().diff(&v, &v, 100, 50).is_empty());
    }

    #[test]
    fn one_pixel_gives_one_tile() {
        let prev = vec![0u32; 100 * 50];
        let mut cur = prev.clone();
        cur[20 * 100 + 97] = 1;
        let rects = Differ::default().diff(&prev, &cur, 100, 50);
        // the last column tile is only 4 pixels wide
        assert_eq!(rects, [Rect::new(96, 16, 4, 8)]);
    }

    #[test]
    fn vertical_merge() {
        let prev = vec![0u32; 64 * 64];
        let mut cur = prev.clone();
        for y in 8..40 {
            cur[y * 64 + 17] = 1;
        }
        let rects = Differ::default().diff(&prev, &cur, 64, 64);
        assert_eq!(rects, [Rect::new(16, 8, 8, 32)]);
    }

    #[test]
    fn half_rule() {
        let prev = vec![0u32; 64 * 64];
        let cur = vec![1u32; 64 * 64];
        let rects = Differ::default().diff(&prev, &cur, 64, 64);
        assert_eq!(rects, [Rect::new(0, 0, 64, 64)]);
    }

    #[test]
    fn random_changes_are_covered() {
        let mut rng = Rng(3);
        let mut differ = Differ::default();
        for round in 0..200 {
            let (w, h) = (1 + rng.below(150) as usize, 1 + rng.below(150) as usize);
            let prev: Vec<u32> = (0..w * h).map(|_| rng.below(4)).collect();
            let mut cur = prev.clone();
            // few changes in early rounds, many in later rounds
            for _ in 0..rng.below(1 + round) {
                let i = rng.below((w * h) as u32) as usize;
                cur[i] = rng.below(4);
            }
            let rects = differ.diff(&prev, &cur, w, h);
            assert!(rects.len() <= MAX_RECTS);
            for r in &rects {
                assert!(r.w > 0 && r.h > 0);
                assert!((r.x + r.w) as usize <= w && (r.y + r.h) as usize <= h);
            }
            assert_eq!(apply(&prev, &cur, w, &rects), cur);
        }
    }
}
