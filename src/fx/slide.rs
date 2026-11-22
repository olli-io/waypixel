//! slide: the new wallpaper comes in from the left, in bands of rows, one
//! band after the other.

use super::*;

/// Rows of cells in one band.
pub(super) const BAND_H: usize = 6;
/// Number of steps for which one band moves.
pub(super) const SLIDE_STEPS: u32 = 24;
/// Steps between the starts of two bands: (numerator, denominator).
const STAGGER: (usize, usize) = (3, 4);

/// The step before band `band` starts to move.
fn start(band: usize) -> u32 {
    (band * STAGGER.0 / STAGGER.1) as u32
}

/// The step at which the last band is complete.
pub(super) fn last_step() -> u32 {
    start(GRID_H.div_ceil(BAND_H) - 1) + SLIDE_STEPS
}

impl Effect {
    pub(super) fn slide(&mut self) {
        self.t += 1;
        for band in 0..GRID_H.div_ceil(BAND_H) {
            // band b starts to move at step start(b) + 1
            let Some(local) = self.t.checked_sub(start(band)) else {
                break;
            };
            if local == 0 || local > SLIDE_STEPS {
                continue;
            }
            // the number of columns that the band covers
            let edge = (in_out_quad(local as f64 / SLIDE_STEPS as f64) * GRID_W as f64) as usize;
            let code = match local == SLIDE_STEPS {
                true => NEW,
                false => SHIFTED | (GRID_W - edge) as u32,
            };
            for y in band * BAND_H..((band + 1) * BAND_H).min(GRID_H) {
                for x in 0..edge.min(GRID_W) {
                    self.set(y * GRID_W + x, code);
                }
            }
        }
    }
}
