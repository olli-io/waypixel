//! smoke: smoke floods a random spanning tree from a random cell, in the
//! order of the depth in the tree.

use super::*;

/// Number of steps until all cells have smoke.
const FLOOD_STEPS: usize = 60;
/// Number of steps for which a cell has smoke.
pub(super) const FLOOD_CELL_STEPS: u8 = 16;
pub(super) const FLOOD: [u32; 8] = gradient([0x242424, 0xFFFFFF, 0xFFFFFF, 0x00D1FF, 0x8A008A]);

impl Effect {
    pub(super) fn smoke(&mut self) {
        self.run_active();
        let rate = self.order.len().div_ceil(FLOOD_STEPS);
        for _ in 0..rate.min(self.order.len() - self.head) {
            let cell = self.order[self.head] as usize;
            self.head += 1;
            self.start(cell);
        }
    }
}
