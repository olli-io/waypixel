//! burn: a fire starts at a random cell and spreads in the order of a random
//! spanning tree (the simple Prim's algorithm). Some cells emit smoke.

use super::*;

/// Approximate number of steps until all cells have started to burn.
const IGNITE_STEPS: usize = 60;
/// Number of steps for which a cell burns. This sets the width of the fire.
pub(super) const FIRE_STEPS: u8 = 11;
pub(super) const FIRE: [u32; 8] = gradient([0xFFFFFF, 0xFFF75D, 0xFE650D, 0x8A003C, 0x510100]);
pub(super) const SMOKE: [u32; 8] = gradient([0x504F4F, 0x6E6D6D, 0x8C8B8B, 0xA9A9A9, 0xC7C7C7]);
/// Number of steps for which smoke rises, one cell for each step.
const SMOKE_STEPS: usize = 16;
pub(super) const SMOKE_MAX: usize = 2000;
/// A cell that stops burning emits smoke with a probability of 1 / this.
const SMOKE_ONE_IN: u32 = 64;

pub(super) struct Smoke {
    x: i16,
    y: i16,
    age: u16,
}

/// The order in which the simple Prim's algorithm links the cells.
pub(super) fn prims_order(rng: &mut Rng) -> Vec<u32> {
    let n = GRID_W * GRID_H;
    let mut linked = vec![false; n];
    let unlinked = |linked: &[bool], cell: u32, out: &mut [u32; 4]| {
        let (x, y) = (cell as usize % GRID_W, cell as usize / GRID_W);
        let mut count = 0;
        let mut add = |ok: bool, c: usize| {
            if ok && !linked[c] {
                out[count] = c as u32;
                count += 1;
            }
        };
        let c = cell as usize;
        add(y > 0, c.wrapping_sub(GRID_W));
        add(x + 1 < GRID_W, c + 1);
        add(y + 1 < GRID_H, c + GRID_W);
        add(x > 0, c.wrapping_sub(1));
        count
    };
    let start = rng.below(n as u32);
    let mut order = Vec::with_capacity(n);
    order.push(start);
    let mut edges = vec![start];
    let mut near = [0; 4];
    while !edges.is_empty() {
        // The edge list has no order, so `swap_remove` is sufficient.
        let current = edges.swap_remove(rng.below(edges.len() as u32) as usize);
        let count = unlinked(&linked, current, &mut near);
        if count == 0 {
            continue;
        }
        let next = near[rng.below(count as u32) as usize];
        linked[current as usize] = true;
        linked[next as usize] = true;
        order.push(next);
        if count > 1 {
            edges.push(current);
        }
        if unlinked(&linked, next, &mut near) > 0 {
            edges.push(next);
        }
    }
    order
}

impl Effect {
    /// A cell that stops burning can emit smoke.
    pub(super) fn emit_smoke(&mut self, cell: usize) {
        if self.smoke.len() < SMOKE_MAX && self.rng.below(SMOKE_ONE_IN) == 0 {
            self.smoke.push(Smoke {
                x: (cell % GRID_W) as i16,
                y: (cell / GRID_W) as i16,
                age: 0,
            });
        }
    }

    pub(super) fn burn(&mut self) {
        // remove the smoke from the cells
        for i in 0..self.smoke.len() {
            let cell = self.smoke[i].y as usize * GRID_W + self.smoke[i].x as usize;
            let code = self.base(cell);
            self.set(cell, code);
        }
        self.run_active();

        // new cells start to burn
        let rate = self.order.len().div_ceil(IGNITE_STEPS) * (2 + self.rng.below(3) as usize) / 3;
        for _ in 0..rate.min(self.order.len() - self.head) {
            let cell = self.order[self.head] as usize;
            self.head += 1;
            self.start(cell);
        }

        // the smoke rises
        let mut i = 0;
        while i < self.smoke.len() {
            let s = &mut self.smoke[i];
            s.age += 1;
            s.y -= 1;
            s.x += self.rng.below(3) as i16 - 1;
            let age = s.age as usize * SMOKE.len() / SMOKE_STEPS;
            if age >= SMOKE.len() || s.y < 0 || s.x < 0 || s.x >= GRID_W as i16 {
                self.smoke.swap_remove(i);
                continue;
            }
            let cell = s.y as usize * GRID_W + s.x as usize;
            self.set(cell, SMOKE[age]);
            i += 1;
        }
    }
}
