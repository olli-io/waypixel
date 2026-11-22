//! decrypt: symbols on black flood a random spanning tree of the symbols, as
//! in smoke. They flash white, become green and change at random. A second
//! flood, from a different symbol, starts before the first one stops. It
//! makes each symbol flash white and show the new wallpaper.

use super::*;

const CIPHER: [u32; 3] = [0xFF00_8000, 0xFF00_CB00, 0xFF00_FF00];
/// Size of a symbol in cells. The glyph is 3 × 5; the remainder is a gap.
const SYM_W: usize = 4;
const SYM_H: usize = 6;
const SYM_COLS: usize = GRID_W / SYM_W;
const SYM_ROWS: usize = GRID_H / SYM_H;
pub(super) const SYMS: usize = SYM_COLS * SYM_ROWS;
/// Glyphs of 3 × 5 cells. Each octal digit is a row, from the top.
const GLYPHS: [u16; 32] = [
    0o02720, 0o55255, 0o75557, 0o57575, 0o07070, 0o11244, 0o44211, 0o12421, 0o42124, 0o72222,
    0o22227, 0o64446, 0o31113, 0o74444, 0o11117, 0o55755, 0o25552, 0o71247, 0o74717, 0o20702,
    0o27222, 0o22272, 0o74747, 0o71717, 0o55222, 0o22555, 0o70707, 0o55555, 0o52525, 0o71111,
    0o44447, 0o55557,
];
/// Number of steps until all symbols have started.
const APPEAR_STEPS: usize = 30;
/// The symbols get their solution from step `END_FROM`, in `END_STEPS` steps,
/// in the order of the second flood.
const END_FROM: u32 = 16;
const END_STEPS: u32 = 40;
/// Minimum number of steps from the start to the solution of a symbol.
const LIFE_MIN: u32 = 14;
/// A green symbol changes with a probability of 1 / this for each step.
const SHUFFLE_ONE_IN: u32 = 4;
/// Number of steps for which a symbol is white: when it is new, and when it
/// has a solution.
const FLASH_STEPS: u8 = 3;

/// The cells of a symbol: (cell, the cell is a part of `glyph`).
fn sym_cells(sym: usize, glyph: u16) -> impl Iterator<Item = (usize, bool)> {
    let origin = sym / SYM_COLS * SYM_H * GRID_W + sym % SYM_COLS * SYM_W;
    (0..SYM_W * SYM_H).map(move |i| {
        let (x, y) = (i % SYM_W, i / SYM_W);
        let on = x < 3 && y < 5 && glyph >> (14 - y * 3 - x) & 1 == 1;
        (origin + y * GRID_W + x, on)
    })
}

/// One flood for the start, and one from a different symbol for the end.
/// The second half has the end step of each symbol.
pub(super) fn order(rng: &mut Rng) -> Vec<u32> {
    let mut syms = flood_order(rng, (SYM_COLS, SYM_ROWS));
    let mut end = vec![0; SYMS];
    let rate = SYMS.div_ceil(APPEAR_STEPS);
    for (r, sym) in syms.iter().enumerate() {
        end[*sym as usize] = (r / rate) as u32 + 1 + LIFE_MIN;
    }
    for (r, sym) in flood_order(rng, (SYM_COLS, SYM_ROWS)).iter().enumerate() {
        let e = &mut end[*sym as usize];
        *e = (*e).max(END_FROM + (r * END_STEPS as usize / SYMS) as u32);
    }
    syms.extend(end);
    syms
}

impl Effect {
    fn glyph(&mut self, sym: usize, glyph: u16, color: u32) {
        for (cell, on) in sym_cells(sym, glyph) {
            self.set(cell, if on { color } else { BLACK });
        }
    }

    fn green(&mut self) -> u32 {
        CIPHER[self.rng.below(CIPHER.len() as u32) as usize]
    }

    /// Draws a random glyph.
    fn cipher(&mut self, sym: usize, color: u32) {
        let glyph = GLYPHS[self.rng.below(GLYPHS.len() as u32) as usize];
        self.glyph(sym, glyph, color);
    }

    /// Gives the colour to the glyph that the symbol shows.
    fn tint(&mut self, sym: usize, color: u32) {
        for (cell, _) in sym_cells(sym, 0) {
            if self.disp[cell] != BLACK {
                self.set(cell, color);
            }
        }
    }

    /// Stages of a symbol: 1..=`FLASH_STEPS` = white when new, then green
    /// until its end flash, `DONE` = the new wallpaper.
    pub(super) fn decrypt(&mut self) {
        for i in 0..self.head {
            let sym = self.order[i] as usize;
            let s = self.stage[sym];
            if s == DONE {
                continue;
            }
            // the second half of `order` has the end steps
            let left = self.order[SYMS + sym].saturating_sub(self.step);
            if left == 0 {
                self.stage[sym] = DONE;
                self.groups += 1;
                for (cell, _) in sym_cells(sym, 0) {
                    self.set(cell, NEW);
                }
            } else if left <= FLASH_STEPS as u32 {
                if left == FLASH_STEPS as u32 {
                    self.tint(sym, WHITE);
                }
            } else if s < FLASH_STEPS {
                self.stage[sym] = s + 1;
            } else if s == FLASH_STEPS {
                self.stage[sym] = s + 1;
                let color = self.green();
                self.tint(sym, color);
            } else if self.rng.below(SHUFFLE_ONE_IN) == 0 {
                let color = self.green();
                self.cipher(sym, color);
            }
        }
        // the first half of `order` is the flood of the start
        let end = (self.head + SYMS.div_ceil(APPEAR_STEPS)).min(SYMS);
        for i in self.head..end {
            let sym = self.order[i] as usize;
            self.stage[sym] = 1;
            self.cipher(sym, WHITE);
        }
        self.head = end;
    }
}
