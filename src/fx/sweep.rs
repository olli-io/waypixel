//! sweep: a shimmer of greys goes from right to left and leaves the old
//! wallpaper grey. Then a shimmer of colours goes from left to right and
//! leaves the new wallpaper.

use super::*;

/// Number of steps of one sweep across the grid.
const SWEEP_STEPS: u32 = 40;
/// Number of steps for which a cell shimmers. The colour changes each 2 steps.
pub(super) const SHIMMER_STEPS: u8 = 4;
pub(super) const GREYS: [u32; 5] = [
    0xFFA0_A0A0,
    0xFF80_8080,
    0xFF40_4040,
    0xFF20_2020,
    0xFF10_1010,
];
pub(super) const SWEEP: [u32; 8] = gradient([0x8A008A, 0x4569C5, 0x00D1FF, 0x80E8FF, 0xFFFFFF]);

impl Effect {
    pub(super) fn sweep(&mut self) {
        self.run_active();
        if !self.second && self.groups == GRID_W {
            // The second sweep starts when the shimmer of the first one has stopped.
            if self.active.is_empty() {
                (self.second, self.groups, self.t) = (true, 0, 0);
            }
            return;
        }
        for group in self.eased_groups(GRID_W, SWEEP_STEPS) {
            // first from right to left, then from left to right
            let x = if self.second {
                group
            } else {
                GRID_W - 1 - group
            };
            for y in 0..GRID_H {
                self.start(y * GRID_W + x);
            }
        }
    }
}
