//! Transitions between two wallpapers, on a grid of pixel-art cells.
//!
//! These are effects of ttfx, changed from terminal symbols to cells:
//! - burn: a fire starts at a random cell and spreads in the order of a random
//!   spanning tree (the simple Prim's algorithm). Some cells emit smoke.
//! - sweep: a shimmer of greys goes from right to left and leaves the old
//!   wallpaper grey. Then a shimmer of colours goes from left to right and
//!   leaves the new wallpaper.
//! - slide: the new wallpaper comes in from the left, in bands of rows, one
//!   band after the other.
//! - wipe: a diagonal band of colours goes from the top left to the bottom
//!   right and leaves the new wallpaper.
//!
//! The grid has no Wayland code. Each output maps it to its screen with an
//! integer scale, as for an animation.

use crate::diff::Rect;
use crate::scale::{Anchor, Geom, upscale_screen};

/// Grid size in cells.
pub const GRID_W: usize = 540;
pub const GRID_H: usize = 270;
/// Time of one step.
pub const STEP_NS: u64 = 33_000_000;

// ---- burn
/// Approximate number of steps until all cells have started to burn.
const IGNITE_STEPS: usize = 60;
/// Number of steps for which a cell burns. This sets the width of the fire.
const FIRE_STEPS: u8 = 11;
const FIRE: [u32; 8] = gradient([0xFFFFFF, 0xFFF75D, 0xFE650D, 0x8A003C, 0x510100]);
const SMOKE: [u32; 8] = gradient([0x504F4F, 0x6E6D6D, 0x8C8B8B, 0xA9A9A9, 0xC7C7C7]);
/// Number of steps for which smoke rises, one cell for each step.
const SMOKE_STEPS: usize = 16;
const SMOKE_MAX: usize = 2000;
/// A cell that stops burning emits smoke with a probability of 1 / this.
const SMOKE_ONE_IN: u32 = 64;

// ---- sweep
/// Number of steps of one sweep across the grid.
const SWEEP_STEPS: u32 = 40;
/// Number of steps for which a cell shimmers. The colour changes each 2 steps.
const SHIMMER_STEPS: u8 = 4;
const GREYS: [u32; 5] = [
    0xFFA0_A0A0,
    0xFF80_8080,
    0xFF40_4040,
    0xFF20_2020,
    0xFF10_1010,
];
const SWEEP: [u32; 8] = gradient([0x8A008A, 0x4569C5, 0x00D1FF, 0x80E8FF, 0xFFFFFF]);

// ---- slide
/// Rows of cells in one band.
const BAND_H: usize = 6;
/// Number of steps for which one band moves.
const SLIDE_STEPS: u32 = 24;

// ---- wipe
/// Number of steps of the wipe across the grid.
const WIPE_STEPS: u32 = 50;
/// Number of steps for which a cell shows the colours of the wipe.
const WIPE_CELL_STEPS: u8 = 4;
const WIPE: [u32; 8] = gradient([0x833AB4, 0xC02C69, 0xFD1D1D, 0xFC6731, 0xFCB045]);

/// Display codes. A code with the top byte 0xFF is an XRGB colour.
const OLD: u32 = 0;
const NEW: u32 = 1;
/// the old wallpaper in grey
const GREY: u32 = 2;
/// `SHIFTED | n`: the new wallpaper, moved n cells to the left
const SHIFTED: u32 = 0x0100_0000;

/// Cell stages: 0 = no change, 1..=steps = shows colours, DONE = complete.
const DONE: u8 = u8::MAX;

/// 8 colours along 5 stops.
const fn gradient(stops: [u32; 5]) -> [u32; 8] {
    let mut out = [0; 8];
    let mut i = 0;
    while i < 8 {
        // position in 1/7 of the range, which has 4 segments
        let (seg, t) = (i * 4 / 7, (i * 4 % 7) as u32);
        let (a, b) = (stops[seg], stops[if seg < 4 { seg + 1 } else { 4 }]);
        let mut c = 0xFF00_0000;
        let mut shift = 0;
        while shift < 24 {
            let (ca, cb) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
            c |= ((ca * (7 - t) + cb * t) / 7) << shift;
            shift += 8;
        }
        out[i] = c;
        i += 1;
    }
    out
}

fn in_out_quad(t: f64) -> f64 {
    if t < 0.5 {
        2.0 * t * t
    } else {
        let u = 2.0 - 2.0 * t;
        1.0 - u * u / 2.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Burn,
    Sweep,
    Slide,
    Wipe,
}

impl Kind {
    pub const NAMES: &str = "burn, sweep, slide, wipe or disabled";

    /// Parses the name of a transition. `disabled` gives `Some(None)`.
    pub fn parse(name: &str) -> Option<Option<Self>> {
        Some(Some(match name {
            "burn" => Self::Burn,
            "sweep" => Self::Sweep,
            "slide" => Self::Slide,
            "wipe" => Self::Wipe,
            "disabled" => return Some(None),
            _ => return None,
        }))
    }

    /// Reads the `transition` key of the config text. The default is burn.
    pub fn from_config(text: &str) -> Result<Option<Self>, String> {
        let mut kind = Some(Self::Burn);
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "transition" {
                let value = value.trim().trim_matches('"');
                kind = Self::parse(value)
                    .ok_or(format!("bad transition {value}: use {}", Self::NAMES))?;
            }
        }
        Ok(kind)
    }
}

/// A 1× image. A colour is an image of 1 × 1.
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub px: Box<[u32]>,
    pub anchor: Anchor,
}

impl Canvas {
    pub fn color(color: u32) -> Self {
        Self {
            w: 1,
            h: 1,
            px: Box::new([color]),
            anchor: Anchor::default(),
        }
    }

    pub fn geom(&self, screen: (u32, u32)) -> Geom {
        Geom::new((self.w, self.h), screen, self.anchor)
    }

    /// The pixels in grey.
    pub fn grey(&self) -> Box<[u32]> {
        self.px.iter().map(|p| grey(*p)).collect()
    }
}

/// One pixel in grey.
pub fn grey(p: u32) -> u32 {
    let luma = (((p >> 16) & 0xFF) * 77 + ((p >> 8) & 0xFF) * 150 + (p & 0xFF) * 29) >> 8;
    0xFF00_0000 | (luma * 0x01_0101)
}

/// A screen area: (x0, y0, x1, y1).
pub type Area = (usize, usize, usize, usize);

/// The images of one output for a render. Each has its own scale for the screen.
pub struct Layers<'a> {
    pub old: (&'a [u32], &'a Geom),
    pub new: (&'a [u32], &'a Geom),
    /// the old image in grey; only sweep uses it
    pub grey: &'a [u32],
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (((self.0 >> 32) * n as u64) >> 32) as u32
    }
}

struct Smoke {
    x: i16,
    y: i16,
    age: u16,
}

pub struct Effect {
    pub kind: Kind,
    rng: Rng,
    stage: Box<[u8]>,
    /// the cells that show colours
    active: Vec<u32>,
    /// what each cell shows: a display code
    disp: Box<[u32]>,
    /// the step at which `disp` of the cell changed last
    stamp: Box<[u32]>,
    /// count of steps since the start
    pub step: u32,
    /// bounds of the cells that changed in this step: (x0, y0, x1, y1)
    bbox: Option<(u16, u16, u16, u16)>,
    /// burn: the cells in the order in which they start to burn
    order: Vec<u32>,
    head: usize,
    smoke: Vec<Smoke>,
    /// sweep, wipe: count of columns or diagonals that have started
    groups: usize,
    /// sweep, wipe, slide: count of steps in this phase
    t: u32,
    /// sweep: the second sweep runs
    second: bool,
}

/// The order in which the simple Prim's algorithm links the cells.
fn prims_order(rng: &mut Rng) -> Vec<u32> {
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
    pub fn new(kind: Kind, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let n = GRID_W * GRID_H;
        let burn = kind == Kind::Burn;
        Self {
            kind,
            order: if burn {
                prims_order(&mut rng)
            } else {
                Vec::new()
            },
            rng,
            head: 0,
            stage: vec![0; n].into(),
            // each cell is in the list one time at most
            active: Vec::with_capacity(if kind == Kind::Slide { 0 } else { n }),
            smoke: Vec::with_capacity(if burn { SMOKE_MAX } else { 0 }),
            disp: vec![OLD; n].into(),
            stamp: vec![0; n].into(),
            step: 0,
            bbox: None,
            groups: 0,
            t: 0,
            second: false,
        }
    }

    fn set(&mut self, cell: usize, code: u32) {
        if self.disp[cell] == code {
            return;
        }
        self.disp[cell] = code;
        self.stamp[cell] = self.step;
        let (x, y) = ((cell % GRID_W) as u16, (cell / GRID_W) as u16);
        let b = self.bbox.get_or_insert((x, y, x, y));
        *b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
    }

    /// (number of steps for which a cell shows colours, what it shows then)
    fn cell_plan(&self) -> (u8, u32) {
        match self.kind {
            Kind::Burn => (FIRE_STEPS, NEW),
            Kind::Wipe => (WIPE_CELL_STEPS, NEW),
            Kind::Sweep if self.second => (SHIMMER_STEPS, NEW),
            Kind::Sweep => (SHIMMER_STEPS, GREY),
            Kind::Slide => (0, NEW),
        }
    }

    /// What the cell shows at its stage, when no smoke is on it.
    fn base(&mut self, cell: usize) -> u32 {
        let (steps, end) = self.cell_plan();
        let s = self.stage[cell];
        if s == 0 {
            return OLD;
        }
        if s == DONE {
            return if self.second { GREY } else { end };
        }
        let i = (s - 1) as usize;
        match self.kind {
            Kind::Burn => FIRE[i * FIRE.len() / steps as usize],
            Kind::Wipe => WIPE[i * WIPE.len() / steps as usize],
            // a random colour, which stays for 2 steps
            _ if i % 2 == 1 => self.disp[cell],
            _ if self.second => SWEEP[self.rng.below(SWEEP.len() as u32) as usize],
            _ => GREYS[self.rng.below(GREYS.len() as u32) as usize],
        }
    }

    fn start(&mut self, cell: usize) {
        self.stage[cell] = 1;
        self.active.push(cell as u32);
        let code = self.base(cell);
        self.set(cell, code);
    }

    /// Moves each active cell one stage on.
    fn run_active(&mut self) {
        let (steps, end) = self.cell_plan();
        let mut i = 0;
        while i < self.active.len() {
            let cell = self.active[i] as usize;
            self.stage[cell] += 1;
            if self.stage[cell] <= steps {
                i += 1;
                let code = self.base(cell);
                self.set(cell, code);
                continue;
            }
            self.stage[cell] = DONE;
            self.active.swap_remove(i);
            self.set(cell, end);
            if self.kind == Kind::Burn
                && self.smoke.len() < SMOKE_MAX
                && self.rng.below(SMOKE_ONE_IN) == 0
            {
                self.smoke.push(Smoke {
                    x: (cell % GRID_W) as i16,
                    y: (cell / GRID_W) as i16,
                    age: 0,
                });
            }
        }
    }

    /// The groups that start in this step, for a move with `in_out_quad`
    /// across `total` groups in `steps` steps.
    fn eased_groups(&mut self, total: usize, steps: u32) -> core::ops::Range<usize> {
        self.t += 1;
        let p = in_out_quad((self.t as f64 / steps as f64).min(1.0));
        let target = ((p * total as f64).round() as usize).clamp(self.groups, total);
        let range = self.groups..target;
        self.groups = target;
        range
    }

    /// Goes to the next step. Returns false when the transition is complete:
    /// all cells show the new wallpaper. There is no allocation.
    pub fn advance(&mut self) -> bool {
        let idle = self.active.is_empty();
        let done = match self.kind {
            Kind::Burn => self.head == self.order.len() && idle && self.smoke.is_empty(),
            Kind::Sweep => self.second && self.groups == GRID_W && idle,
            Kind::Wipe => self.groups == GRID_W + GRID_H - 1 && idle,
            Kind::Slide => self.t >= GRID_H.div_ceil(BAND_H) as u32 - 1 + SLIDE_STEPS,
        };
        if done {
            return false;
        }
        self.step += 1;
        self.bbox = None;
        match self.kind {
            Kind::Burn => self.burn(),
            Kind::Sweep => self.sweep(),
            Kind::Wipe => self.wipe(),
            Kind::Slide => self.slide(),
        }
        true
    }

    fn burn(&mut self) {
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

    fn sweep(&mut self) {
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

    fn wipe(&mut self) {
        self.run_active();
        for d in self.eased_groups(GRID_W + GRID_H - 1, WIPE_STEPS) {
            // the diagonal x + y = d
            for x in d.saturating_sub(GRID_H - 1)..=d.min(GRID_W - 1) {
                self.start((d - x) * GRID_W + x);
            }
        }
    }

    fn slide(&mut self) {
        self.t += 1;
        // band b starts to move at step b + 1
        let first = self.t.saturating_sub(SLIDE_STEPS) as usize;
        let last = (self.t as usize).min(GRID_H.div_ceil(BAND_H));
        for band in first..last {
            let local = self.t - band as u32;
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

    /// The cells that changed in the last step.
    pub fn changed(&self) -> Option<Rect> {
        let (x0, y0, x1, y1) = self.bbox?;
        Some(Rect::new(
            x0 as u32,
            y0 as u32,
            (x1 - x0) as u32 + 1,
            (y1 - y0) as u32 + 1,
        ))
    }

    /// Calls `f(code, area)` for each run of cells in a row that have the same
    /// display code and for which `pick` is true. Only the part of the cells
    /// inside the screen area `clip` is used.
    fn runs(
        &self,
        g: &Geom,
        (ax0, ay0, ax1, ay1): Area,
        pick: impl Fn(usize) -> bool,
        mut f: impl FnMut(u32, Area),
    ) {
        let Geom { k, ox, oy, .. } = *g;
        let (cx0, cx1) = ((ax0 + ox) / k, (ax1 - 1 + ox) / k);
        let (cy0, cy1) = ((ay0 + oy) / k, (ay1 - 1 + oy) / k);
        for cy in cy0..=cy1 {
            let y0 = (cy * k).saturating_sub(oy).max(ay0);
            let y1 = ((cy + 1) * k - oy).min(ay1);
            let row = cy * GRID_W;
            let mut cx = cx0;
            while cx <= cx1 {
                if !pick(row + cx) {
                    cx += 1;
                    continue;
                }
                let code = self.disp[row + cx];
                let start = cx;
                while cx <= cx1 && self.disp[row + cx] == code && pick(row + cx) {
                    cx += 1;
                }
                let x0 = (start * k).saturating_sub(ox).max(ax0);
                let x1 = (cx * k - ox).min(ax1);
                f(code, (x0, y0, x1, y1));
            }
        }
    }

    /// Brings a screen buffer that holds step `holds` up to the current step.
    /// This does not include the changes of the images: see `render_layer`.
    ///
    /// `g` maps the grid to the screen.
    pub fn render(&self, dst: &mut [u32], g: &Geom, layers: &Layers, holds: Option<u32>) {
        let stale = |cell: usize| holds.is_none_or(|h| self.stamp[cell] > h);
        self.runs(g, (0, 0, g.sw, g.sh), stale, |code, area| {
            draw(dst, g, layers, code, area)
        });
    }

    /// Calls `f(code, area)` for the cells that show the 1× rect `r` of an
    /// image. `layer` maps the image to the screen. `new` selects the new image.
    fn layer_runs(&self, g: &Geom, layer: &Geom, new: bool, r: Rect, mut f: impl FnMut(u32, Area)) {
        let Some(area) = layer.map(r) else {
            return;
        };
        let shows = |code: u32| match new {
            true => code == NEW,
            false => code == OLD || code == GREY,
        };
        self.runs(g, area, |cell| shows(self.disp[cell]), &mut f);
        if new && self.kind == Kind::Slide {
            // A moved band shows the rect at a different x. The full width
            // of the band rows is drawn again.
            let strip = (0, area.1, g.sw, area.3);
            let moved = |cell: usize| self.disp[cell] >> 24 == SHIFTED >> 24;
            self.runs(g, strip, moved, &mut f);
        }
    }

    /// Draws again the cells that show the 1× rect `r` of an image that
    /// changed: the old image (and its grey) or, with `new`, the new image.
    pub fn render_layer(&self, dst: &mut [u32], g: &Geom, layers: &Layers, new: bool, r: Rect) {
        let layer = if new { layers.new.1 } else { layers.old.1 };
        self.layer_runs(g, layer, new, r, |code, area| {
            draw(dst, g, layers, code, area)
        });
    }

    /// The bounds of the screen area that `render_layer` draws for this rect.
    pub fn layer_damage(&self, g: &Geom, layer: &Geom, new: bool, r: Rect) -> Option<Area> {
        let mut bounds: Option<Area> = None;
        self.layer_runs(g, layer, new, r, |_, (x0, y0, x1, y1)| {
            let b = bounds.get_or_insert((x0, y0, x1, y1));
            *b = (b.0.min(x0), b.1.min(y0), b.2.max(x1), b.3.max(y1));
        });
        bounds
    }
}

/// Draws the screen area of a run of cells with the display code `code`.
fn draw(dst: &mut [u32], g: &Geom, layers: &Layers, code: u32, area: Area) {
    let (x0, y0, x1, y1) = area;
    let sw = g.sw;
    let (old, new) = (layers.old, layers.new);
    match code {
        OLD => upscale_screen(old.0, dst, old.1, area),
        NEW => upscale_screen(new.0, dst, new.1, area),
        GREY => upscale_screen(layers.grey, dst, old.1, area),
        color if color >> 24 == 0xFF => {
            for y in y0..y1 {
                dst[y * sw + x0..y * sw + x1].fill(color);
            }
        }
        shifted => {
            // The grid can be wider than the screen. The limit
            // keeps the source inside the image.
            let shift = ((shifted & 0xFFFF) as usize * g.k).min(sw - x1);
            let mut moved = *new.1;
            moved.ox += shift;
            upscale_screen(new.0, dst, &moved, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [Kind; 4] = [Kind::Burn, Kind::Sweep, Kind::Slide, Kind::Wipe];

    fn reference(px: &[u32], g: &Geom) -> Vec<u32> {
        let mut out = vec![0; g.sw * g.sh];
        for y in 0..g.sh {
            for x in 0..g.sw {
                out[y * g.sw + x] = px[(y + g.oy) / g.k * g.w + (x + g.ox) / g.k];
            }
        }
        out
    }

    fn art(w: u32, h: u32, seed: u64) -> Canvas {
        let mut rng = Rng(seed);
        Canvas {
            w,
            h,
            px: (0..w * h).map(|_| 0xFF00_0000 | rng.below(200)).collect(),
            anchor: Anchor::default(),
        }
    }

    #[test]
    fn gradients() {
        assert_eq!((FIRE[0], FIRE[7]), (0xFFFF_FFFF, 0xFF51_0100));
        assert_eq!((SMOKE[0], SMOKE[7]), (0xFF50_4F4F, 0xFFC7_C7C7));
        assert_eq!(Canvas::color(0xFFFF_FFFF).grey()[0], 0xFFFF_FFFF);
        assert_eq!(Canvas::color(0xFF00_FF00).grey()[0], 0xFF95_9595);
    }

    #[test]
    fn easing() {
        let f = in_out_quad;
        assert_eq!((f(0.0), f(0.5), f(1.0)), (0.0, 0.5, 1.0));
    }

    #[test]
    fn config() {
        assert_eq!(Kind::from_config(""), Ok(Some(Kind::Burn)));
        assert_eq!(
            Kind::from_config("# x\ntransition = wipe # y\n"),
            Ok(Some(Kind::Wipe))
        );
        assert_eq!(
            Kind::from_config("transition=\"slide\""),
            Ok(Some(Kind::Slide))
        );
        assert_eq!(
            Kind::from_config("other = 1\ntransition = disabled"),
            Ok(None)
        );
        assert!(Kind::from_config("transition = melt").is_err());
    }

    #[test]
    fn order_has_each_cell_one_time() {
        let mut order = prims_order(&mut Rng(3));
        assert_eq!(order.len(), GRID_W * GRID_H);
        order.sort_unstable();
        assert!(order.iter().enumerate().all(|(i, c)| i == *c as usize));
    }

    /// Buffers in rotation, with skipped steps, show the same pixels as a full
    /// render. The first and the last step show only the old and the new image.
    #[test]
    fn render_matches_full_render() {
        let cases = [
            ((1100u32, 520u32), 1, 1),
            ((641, 359), 2, 1),
            ((300, 400), 3, 4),
        ];
        for (kind, (screen, nbufs, skip)) in KINDS.iter().flat_map(|k| cases.map(|c| (*k, c))) {
            let mut fx = Effect::new(kind, 11);
            let g = Geom::new((GRID_W as u32, GRID_H as u32), screen, Anchor::default());
            let (old, new) = (art(64, 36, 1), art(90, 45, 2));
            let (og, ng, grey) = (old.geom(screen), new.geom(screen), old.grey());
            let layers = Layers {
                old: (&old.px, &og),
                new: (&new.px, &ng),
                grey: &grey,
            };
            let n = g.sw * g.sh;
            let mut bufs: Vec<(Vec<u32>, Option<u32>)> = vec![(vec![0xDEAD_BEEF; n], None); nbufs];
            let mut full = vec![0; n];
            fx.render(&mut full, &g, &layers, None);
            let (first, last) = (reference(&old.px, &og), reference(&new.px, &ng));
            assert!(full == first);
            let mut mixed = false;
            while fx.advance() {
                if !fx.step.is_multiple_of(skip) {
                    continue;
                }
                let (buf, holds) = &mut bufs[(fx.step / skip) as usize % nbufs];
                fx.render(buf, &g, &layers, *holds);
                *holds = Some(fx.step);
                fx.render(&mut full, &g, &layers, None);
                assert!(*buf == full, "{kind:?}, step {}", fx.step);
                mixed |= full != first && full != last;
            }
            assert!(mixed);
            assert!((50..200).contains(&fx.step), "{kind:?}: {} steps", fx.step);
            fx.render(&mut full, &g, &layers, None);
            assert!(full == last, "{kind:?}");
        }
    }

    /// The two images change during the transition, as with two animations.
    /// Buffers in rotation show the same pixels as a full render.
    #[test]
    fn live_layers_match_full_render() {
        for (kind, screen) in KINDS
            .iter()
            .flat_map(|k| [(1100, 520), (300, 400)].map(|s| (*k, s)))
        {
            let mut fx = Effect::new(kind, 7);
            let g = Geom::new((GRID_W as u32, GRID_H as u32), screen, Anchor::default());
            let (mut old, mut new) = (art(64, 36, 1), art(90, 45, 2));
            let (og, ng) = (old.geom(screen), new.geom(screen));
            let mut grey = old.grey();
            let mut rng = Rng(kind as u64 + 40);
            // the rects that changed at each frame: (old, new)
            let mut frames: Vec<(Rect, Rect)> = Vec::new();
            let n_px = g.sw * g.sh;
            let mut bufs = vec![(vec![0xDEAD_BEEF; n_px], None::<(u32, usize)>); 2];
            let mut full = vec![0; n_px];
            let mut turn = 0;
            while fx.advance() {
                // a frame comes each 2 steps; a render comes each 3 steps
                if fx.step.is_multiple_of(2) {
                    let mut change = |c: &mut Canvas| {
                        let (w, h) = (1 + rng.below(c.w / 2), 1 + rng.below(c.h / 2));
                        let r = Rect::new(rng.below(c.w - w), rng.below(c.h - h), w, h);
                        for y in r.y..r.y + r.h {
                            for x in r.x..r.x + r.w {
                                c.px[y as usize * c.w as usize + x as usize] =
                                    0xFF00_0000 | rng.below(200);
                            }
                        }
                        r
                    };
                    frames.push((change(&mut old), change(&mut new)));
                    grey = old.grey();
                }
                if !fx.step.is_multiple_of(3) {
                    continue;
                }
                let layers = Layers {
                    old: (&old.px, &og),
                    new: (&new.px, &ng),
                    grey: &grey,
                };
                turn += 1;
                let (buf, holds) = &mut bufs[turn % 2];
                fx.render(buf, &g, &layers, holds.map(|h| h.0));
                if let Some((_, frame)) = *holds {
                    for (o, n) in &frames[frame..] {
                        for (is_new, r, lg) in [(false, *o, &og), (true, *n, &ng)] {
                            let before = buf.clone();
                            fx.render_layer(buf, &g, &layers, is_new, r);
                            // the damage covers the pixels that changed
                            let d = fx.layer_damage(&g, lg, is_new, r).unwrap_or_default();
                            for i in (0..n_px).filter(|i| before[*i] != buf[*i]) {
                                let (x, y) = (i % g.sw, i / g.sw);
                                assert!(x >= d.0 && x < d.2 && y >= d.1 && y < d.3);
                            }
                        }
                    }
                }
                *holds = Some((fx.step, frames.len()));
                fx.render(&mut full, &g, &layers, None);
                assert!(*buf == full, "{kind:?}, step {}", fx.step);
            }
        }
    }

    /// The cells outside `changed` have the stamp of an earlier step.
    #[test]
    fn changed_covers_the_step() {
        for kind in KINDS {
            let mut fx = Effect::new(kind, 5);
            while fx.advance() {
                let r = fx.changed().unwrap_or_default();
                for (cell, stamp) in fx.stamp.iter().enumerate() {
                    let (x, y) = ((cell % GRID_W) as u16, (cell / GRID_W) as u16);
                    let inside = x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
                    assert!(inside || *stamp < fx.step);
                }
            }
        }
    }
}
