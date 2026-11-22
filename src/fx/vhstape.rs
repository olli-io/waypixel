//! vhstape: bands of rows lose their colour and jump to the side, a wave of
//! such bands moves down, and noise comes in bursts. Then noise covers all
//! and each band, at a random step, flashes white and shows the new wallpaper.

use super::*;

/// Rows of cells in one line.
pub(super) const LINE_H: usize = 6;
/// Number of steps of the phase with lines that jump.
const VHS_GLITCH: u32 = 50;
/// The shifts of the 3 lines of the wave, in cells.
const VHS_WAVE: [i16; 3] = [24, 42, 24];
/// Cells that a line moves in one step.
const VHS_SPEED: i16 = 10;
/// A line starts to jump with a probability of 1 / this for each step.
const VHS_LINE_ONE_IN: u32 = 6;
/// A burst of noise starts with a probability of 1 / this for each step.
const VHS_NOISE_ONE_IN: u32 = 30;
/// Number of steps of a burst of noise, and of the noise before the end.
const VHS_BURST: u32 = 6;
const VHS_NOISE: u32 = 14;
/// The lines get the new wallpaper at random steps in this range.
const VHS_END: u32 = 24;
/// Number of steps for which a line is white before the new wallpaper.
const VHS_FLASH: u8 = 3;
const NOISE: [u32; 6] = [
    0xFF1E_1E1F,
    0xFF3C_3B3D,
    0xFF6D_6C70,
    0xFFA2_A1A6,
    0xFFCB_C9CF,
    0xFFFF_FFFF,
];

/// A band of `LINE_H` rows.
#[derive(Clone, Copy, Default)]
pub(super) struct Line {
    /// cells that the line is moved to the left
    shift: i16,
    /// the shift of a jump, and the steps that it stays
    jump: i16,
    /// At the end: the steps until the line shows the new wallpaper.
    hold: u8,
}

impl Effect {
    pub(super) fn vhstape(&mut self) {
        let n = self.lines.len();
        let glitch = self.step <= VHS_GLITCH;
        self.t = self.t.saturating_sub(1);
        if glitch {
            // The wave moves in 1 of 2 steps: down in most cases. From the
            // bottom, it goes to a random line in the top half.
            if self.rng.below(2) == 0 {
                self.head = match self.rng.below(10) < 7 {
                    true => self.head + 1,
                    false => self.head.saturating_sub(1),
                };
                if self.head + VHS_WAVE.len() > n {
                    self.head = self.rng.below(n as u32 / 2) as usize;
                }
            }
            // 3 lines at most jump at the same time
            let jumps = self.lines.iter().filter(|l| l.hold > 0).count();
            if jumps < 3 && self.rng.below(VHS_LINE_ONE_IN) == 0 {
                let (jump, hold) = (12 + self.rng.below(49), 8 + self.rng.below(18));
                let line = &mut self.lines[self.rng.below(n as u32) as usize];
                if line.hold == 0 && line.shift == 0 {
                    (line.jump, line.hold) = (jump as i16, hold as u8);
                }
            }
            if self.t == 0 && self.rng.below(VHS_NOISE_ONE_IN) == 0 {
                self.t = VHS_BURST;
            }
        }
        let end = self.second && self.t == 0;
        let noise = self.second || self.t > 0;
        let mut rest = true;
        for i in 0..n {
            let line = &mut self.lines[i];
            let wave = glitch && (self.head..self.head + VHS_WAVE.len()).contains(&i);
            let goal = match (glitch, wave, line.hold > 0) {
                (true, true, _) => VHS_WAVE[i - self.head],
                (true, false, true) => line.jump,
                _ => 0,
            };
            let moves = line.shift != goal;
            if !moves && !self.second {
                line.hold = line.hold.saturating_sub(1);
            }
            line.shift += (goal - line.shift).clamp(-VHS_SPEED, VHS_SPEED);
            let shift = line.shift as usize;
            rest &= shift == 0;

            let mut left = u8::MAX;
            if end {
                if line.hold == 0 {
                    continue;
                }
                line.hold -= 1;
                left = line.hold;
            }
            let cells = i * LINE_H * GRID_W..(i + 1) * LINE_H * GRID_W;
            if left <= VHS_FLASH {
                // the line is white, then shows the new wallpaper
                let code = if left == 0 { NEW } else { WHITE };
                self.groups += (left == 0) as usize;
                cells.for_each(|cell| self.set(cell, code));
            } else if noise {
                // The noise changes each 2 steps, in runs of 1 to 4 cells.
                let mut left = 0;
                let mut code = 0;
                let even = self.step.is_multiple_of(2);
                for cell in cells.filter(|_| even) {
                    if left == 0 || cell % GRID_W == 0 {
                        left = 1 + self.rng.below(4);
                        code = NOISE[self.rng.below(NOISE.len() as u32) as usize];
                    }
                    left -= 1;
                    self.set(cell, code);
                }
            } else {
                // A line that moves has a tint, which changes each step.
                let tint = if moves { 1 + self.step % 3 } else { 0 };
                let code = match shift {
                    0 => OLD,
                    _ => GLITCH | tint << 16 | shift as u32,
                };
                for cell in cells {
                    let end = cell % GRID_W + shift >= GRID_W;
                    self.set(cell, if end { BLACK } else { code });
                }
            }
        }
        if !glitch && rest && !self.second {
            (self.second, self.t) = (true, VHS_NOISE);
            for line in &mut self.lines {
                line.hold = 1 + VHS_FLASH + self.rng.below(VHS_END) as u8;
            }
        }
    }
}
