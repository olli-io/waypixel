//! rain and pour, which have the same code.
//! - pour: tiles fall from the top and fill the grid from the bottom, each row
//!   in the opposite direction to the row below.
//! - rain: drops fall from the top. Each drop fills one tile, from the bottom
//!   row of tiles up.

use super::*;

pub(super) const POUR_TILE: usize = 15;
/// Number of tiles that start to fall in one step.
const POUR_RATE: usize = 7;
/// Number of steps of a fall: the minimum, and the size of the random range.
const POUR_STEPS: (u32, u32) = (10, 6);

pub(super) const RAIN_TILE: usize = 6;
/// Number of drops that start to fall in one step.
const RAIN_RATE: usize = 68;
/// The drops come at random from this number of the lowest empty tile rows.
pub(super) const RAIN_ROWS: usize = 3;
const RAIN_STEPS: (u32, u32) = (10, 8);
/// Maximum length of a drop in cells.
const RAIN_LEN: i16 = 5;

/// A drop or a tile that falls. It covers the rows `tail..=y`.
pub(super) struct Drop {
    x: i16,
    y: i16,
    tail: i16,
    /// the row at which the fall stops
    to: i16,
    age: u8,
    /// number of steps of the fall
    steps: u8,
}

impl Effect {
    /// Sets the cells of a drop to the new wallpaper at its tile. Without
    /// `show`, the cells show their stage again.
    fn drop_cells(&mut self, i: usize, show: bool) {
        let Drop { x, y, tail, to, .. } = self.drops[i];
        let w = if self.kind == Kind::Pour {
            POUR_TILE
        } else {
            1
        };
        debug_assert!(y <= to);
        let lifted = LIFTED | (to - y) as u32;
        for cy in tail.max(0)..=y {
            for cell in cy as usize * GRID_W + x as usize..cy as usize * GRID_W + x as usize + w {
                let code = if show { lifted } else { self.base(cell) };
                self.set(cell, code);
            }
        }
    }

    pub(super) fn fall(&mut self) {
        let pour = self.kind == Kind::Pour;
        let (tile, rate, steps) = match pour {
            true => (POUR_TILE, POUR_RATE, POUR_STEPS),
            false => (RAIN_TILE, RAIN_RATE, RAIN_STEPS),
        };
        for i in 0..self.drops.len() {
            self.drop_cells(i, false);
        }

        // new drops start above the top row
        for _ in 0..rate.min(self.order.len() - self.head) {
            let t = self.order[self.head] as usize;
            self.head += 1;
            let (tx, ty) = (t % (GRID_W / tile), t / (GRID_W / tile));
            let to = ty * tile + tile - 1;
            let x = if pour { 0 } else { self.rng.below(tile as u32) };
            self.drops.push(Drop {
                x: (tx * tile) as i16 + x as i16,
                y: -1,
                tail: 0,
                to: to as i16,
                age: 0,
                steps: (steps.0 + self.rng.below(steps.1)) as u8,
            });
        }

        let mut i = 0;
        while i < self.drops.len() {
            let d = &mut self.drops[i];
            d.age += 1;
            // pour: in_quad. rain: in_quart, as in ttfx
            let p = d.age as f64 / d.steps as f64;
            let p = if pour { p * p } else { p * p * p * p };
            let y = (p * (d.to + 1) as f64) as i16 - 1;
            d.tail = match pour {
                true => y + 1 - tile as i16,
                false => (d.y + 1).max(y + 1 - RAIN_LEN).min(y),
            };
            d.y = y;
            if d.age < d.steps {
                self.drop_cells(i, true);
                i += 1;
                continue;
            }
            // the drop is at its tile
            let (x, y) = (d.x as usize / tile * tile, d.to as usize + 1 - tile);
            self.drops.swap_remove(i);
            for cell in (y..y + tile).flat_map(|cy| cy * GRID_W + x..cy * GRID_W + x + tile) {
                self.stage[cell] = DONE;
                self.set(cell, NEW);
            }
        }
    }
}
