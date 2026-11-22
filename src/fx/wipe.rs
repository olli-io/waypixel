//! wipe: a diagonal band of colours goes from the top left to the bottom
//! right and leaves the new wallpaper.

use super::*;

/// Number of steps of the wipe across the grid.
const WIPE_STEPS: u32 = 50;
/// Number of steps for which a cell shows the colours of the wipe.
pub(super) const WIPE_CELL_STEPS: u8 = 4;
pub(super) const WIPE: [u32; 8] = gradient([0x833AB4, 0xC02C69, 0xFD1D1D, 0xFC6731, 0xFCB045]);

impl Effect {
    pub(super) fn wipe(&mut self) {
        self.run_active();
        for d in self.eased_groups(GRID_W + GRID_H - 1, WIPE_STEPS) {
            // the diagonal x + y = d
            for x in d.saturating_sub(GRID_H - 1)..=d.min(GRID_W - 1) {
                self.start((d - x) * GRID_W + x);
            }
        }
    }
}
