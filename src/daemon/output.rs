//! One output: its surface, viewport, scale, and the Target that draws the wallpaper.

use waybackend::types::{ObjectId, WlFixed};

use super::player::Player;
use super::pool::Pool;
use super::wayland::{Obj, Wl, req, zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use crate::cache::{Cache, Hash};
use crate::diff::Rect;
use crate::fx::{Canvas, Effect, GRID_H, GRID_W, Kind, Layers};
use crate::scale::{Geom, upscale_rect};

/// What an output shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Wall {
    /// XRGB colour
    Color(u32),
    /// a still image; the cache file is opened only for a render
    Still { hash: Hash, path: String },
    /// a pixel-art animation, drawn from the player with this id
    Anim { player: u32, path: String },
}

/// The part of a transition that is specific to one output.
struct Fx {
    /// id of the transition
    id: u32,
    /// the wallpaper before the transition
    old: Canvas,
    /// `old` in grey, for sweep
    grey: Box<[u32]>,
    old_geom: Geom,
    new_geom: Geom,
    /// grid of the effect → screen
    geom: Geom,
    /// the step of the last commit
    last_committed: u32,
}

impl Fx {
    fn layers<'a>(&'a self, new: &'a Canvas) -> Layers<'a> {
        Layers {
            old: (&self.old.px, &self.old_geom),
            new: (&new.px, &self.new_geom),
            grey: &self.grey,
        }
    }
}

pub struct Output {
    /// name of the `wl_output` global in the registry
    pub global: u32,
    pub wl_output: ObjectId,
    /// for example `DP-1`
    pub name: String,
    surface: ObjectId,
    pub layer_surface: ObjectId,
    viewport: ObjectId,
    pub fractional: Option<ObjectId>,
    /// `wl_output.scale`
    pub int_scale: u32,
    /// preferred fractional scale in 1/120; it has priority over `int_scale`
    pub frac_scale: Option<u32>,
    /// logical size from the last `configure`
    pub logical: Option<(u32, u32)>,
    /// frame callback of the last commit, until it arrives
    pub callback: Option<ObjectId>,
    pub wall: Wall,
    /// the Target must be built again
    pub dirty: bool,
    pool: Option<Pool>,
    geom: Option<Geom>,
    /// the frame (player `seq`) of the last commit
    last_committed: Option<u64>,
    fx: Option<Fx>,
}

impl Output {
    pub fn new(
        wl: &mut Wl,
        global: u32,
        wl_output: ObjectId,
        name: String,
        int_scale: u32,
        wall: Wall,
    ) -> Self {
        use zwlr_layer_surface_v1::Anchor;
        let surface = wl.objman.create(Obj::Surface);
        req!(
            wl,
            wl_compositor::req::create_surface(wl.compositor, surface)
        );

        let region = wl.objman.create(Obj::Region);
        req!(wl, wl_compositor::req::create_region(wl.compositor, region));
        req!(wl, wl_surface::req::set_input_region(surface, Some(region)));
        req!(wl, wl_region::req::destroy(region));

        let layer_surface = wl.objman.create(Obj::LayerSurface);
        req!(
            wl,
            zwlr_layer_shell_v1::req::get_layer_surface(
                wl.layer_shell,
                layer_surface,
                surface,
                Some(wl_output),
                zwlr_layer_shell_v1::Layer::background,
                "pixelwp"
            )
        );
        let viewport = wl.objman.create(Obj::Viewport);
        req!(
            wl,
            wp_viewporter::req::get_viewport(wl.viewporter, viewport, surface)
        );
        let fractional = wl.fractional.map(|manager| {
            let id = wl.objman.create(Obj::FractionalScale);
            req!(
                wl,
                wp_fractional_scale_manager_v1::req::get_fractional_scale(manager, id, surface)
            );
            id
        });

        req!(
            wl,
            zwlr_layer_surface_v1::req::set_anchor(
                layer_surface,
                Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT
            )
        );
        req!(
            wl,
            zwlr_layer_surface_v1::req::set_exclusive_zone(layer_surface, -1)
        );
        req!(
            wl,
            zwlr_layer_surface_v1::req::set_keyboard_interactivity(
                layer_surface,
                zwlr_layer_surface_v1::KeyboardInteractivity::None
            )
        );
        req!(
            wl,
            zwlr_layer_surface_v1::req::set_size(layer_surface, 0, 0)
        );
        // the compositor answers this commit with the first `configure`
        req!(wl, wl_surface::req::commit(surface));

        Self {
            global,
            wl_output,
            name,
            surface,
            layer_surface,
            viewport,
            fractional,
            int_scale,
            frac_scale: None,
            logical: None,
            callback: None,
            wall,
            dirty: false,
            pool: None,
            geom: None,
            last_committed: None,
            fx: None,
        }
    }

    /// Physical size: the logical size × scale, rounded.
    pub fn physical(&self) -> (u32, u32) {
        let Some((w, h)) = self.logical else {
            return (0, 0);
        };
        let n = self.frac_scale.unwrap_or(self.int_scale * 120) as u64;
        let mul = |v: u32| (((v as u64 * n + 60) / 120) as u32).max(1);
        (mul(w), mul(h))
    }

    pub fn player(&self) -> Option<u32> {
        match self.wall {
            Wall::Anim { player, .. } => Some(player),
            _ => None,
        }
    }

    /// The transition that this output shows.
    pub fn fx(&self) -> Option<u32> {
        self.fx.as_ref().map(|fx| fx.id)
    }

    /// The player that this output draws from. During a transition, this is `None`.
    pub fn playing(&self) -> Option<u32> {
        self.player().filter(|_| self.fx.is_none())
    }

    /// True when the frame callback of the last commit has arrived.
    pub fn ready(&self) -> bool {
        self.logical.is_some() && self.callback.is_none()
    }

    /// Builds the Target again and renders it in full.
    ///
    /// On error, the output shows black and the wall does not change.
    pub fn rebuild(&mut self, wl: &mut Wl, players: &[Player]) -> Result<(), String> {
        self.dirty = false;
        let Some((lw, lh)) = self.logical else {
            return Ok(());
        };
        if let Some(pool) = self.pool.take() {
            pool.destroy(wl);
        }
        self.callback = None;
        self.geom = None;
        self.last_committed = None;
        self.fx = None;
        let result = self.build(wl, players, (lw, lh));
        if result.is_err() {
            let black = self.color_pool(wl, 0xFF00_0000);
            self.commit_full(wl, black, None, false);
        }
        result
    }

    fn color_pool(&mut self, wl: &mut Wl, color: u32) -> Option<Pool> {
        let mut pool = Pool::new(wl, 1, 1).ok()?;
        pool.pixels(0)[0] = color;
        Some(pool)
    }

    fn build(
        &mut self,
        wl: &mut Wl,
        players: &[Player],
        logical: (u32, u32),
    ) -> Result<(), String> {
        let (pw, ph) = self.physical();
        let shm = |e: rustix::io::Errno| format!("cannot create a shm buffer: {e}");
        match &self.wall {
            Wall::Color(color) => {
                let pool = self.color_pool(wl, *color);
                self.commit_full(wl, pool, None, false);
            }
            Wall::Still { hash, .. } => {
                let cache = Cache::open(&crate::cache::path_for(hash))?;
                if cache.pixel_art {
                    let mut canvas = vec![0u32; cache.w as usize * cache.h as usize];
                    cache.apply(0, &mut canvas, &mut [])?;
                    let g = Geom::new((cache.w, cache.h), (pw, ph));
                    let mut pool = Pool::new(wl, pw, ph).map_err(shm)?;
                    upscale_rect(
                        &canvas,
                        pool.pixels(0),
                        &g,
                        Rect::new(0, 0, cache.w, cache.h),
                    );
                    self.commit_full(wl, Some(pool), None, false);
                } else {
                    // aspect fill: the compositor scales the stored image
                    let mut pool = Pool::new(wl, cache.w, cache.h).map_err(shm)?;
                    if let Err(e) = cache.apply(0, pool.pixels(0), &mut []) {
                        // not a plain drop: the fd of the pool is in the message buffer
                        pool.destroy(wl);
                        return Err(e);
                    }
                    let crop = fill_crop((cache.w, cache.h), logical);
                    self.commit_full(wl, Some(pool), Some(crop), false);
                }
            }
            Wall::Anim { player, .. } => {
                let player = players
                    .iter()
                    .find(|p| p.id == *player)
                    .ok_or("the player does not exist")?;
                let g = Geom::new(player.size(), (pw, ph));
                let mut pool = Pool::new(wl, pw, ph).map_err(shm)?;
                player.render(pool.pixels(0), &g, None);
                pool.bufs[0].holds = Some(player.seq);
                self.geom = Some(g);
                self.last_committed = Some(player.seq);
                self.commit_full(wl, Some(pool), None, true);
            }
        }
        Ok(())
    }

    /// Attaches buffer 0 of the pool with full damage and commits.
    ///
    /// `crop` is the viewport source in 1/256 of a buffer pixel: (x, y, w, h).
    fn commit_full(
        &mut self,
        wl: &mut Wl,
        pool: Option<Pool>,
        crop: Option<[i32; 4]>,
        frame: bool,
    ) {
        let Some(mut pool) = pool else {
            return;
        };
        let Some((lw, lh)) = self.logical else {
            return;
        };
        let fixed = |v: i32| WlFixed::from(v as f64 / 256.0);
        let [x, y, w, h] = crop.map_or([WlFixed::from(-1); 4], |c| c.map(fixed));
        req!(wl, wp_viewport::req::set_source(self.viewport, x, y, w, h));
        req!(
            wl,
            wp_viewport::req::set_destination(self.viewport, lw as i32, lh as i32)
        );
        req!(
            wl,
            wl_surface::req::attach(self.surface, Some(pool.bufs[0].id), 0, 0)
        );
        req!(
            wl,
            wl_surface::req::damage_buffer(self.surface, 0, 0, pool.w as i32, pool.h as i32)
        );
        if frame {
            self.request_frame(wl);
        }
        req!(wl, wl_surface::req::commit(self.surface));
        pool.bufs[0].busy = true;
        self.pool = Some(pool);
    }

    fn request_frame(&mut self, wl: &mut Wl) {
        let callback = wl.objman.create(Obj::Callback);
        req!(wl, wl_surface::req::frame(self.surface, callback));
        self.callback = Some(callback);
    }

    /// Draws the current frame of the player: only the dirty rects, with damage
    /// for each rect. There is no allocation when a buffer is free.
    pub fn tick(&mut self, wl: &mut Wl, player: &Player) {
        let (Some(pool), Some(g)) = (&mut self.pool, &self.geom) else {
            return;
        };
        if self.last_committed == Some(player.seq) {
            return;
        }
        let Some(i) = pool.acquire(wl) else {
            return;
        };
        let holds = pool.bufs[i].holds;
        player.render(pool.pixels(i), g, holds);
        pool.bufs[i].holds = Some(player.seq);
        pool.bufs[i].busy = true;

        let surface = self.surface;
        req!(
            wl,
            wl_surface::req::attach(surface, Some(pool.bufs[i].id), 0, 0)
        );
        player.dirty(self.last_committed, |r| {
            if let Some((x0, y0, x1, y1)) = g.map(r) {
                req!(
                    wl,
                    wl_surface::req::damage_buffer(
                        surface,
                        x0 as i32,
                        y0 as i32,
                        (x1 - x0) as i32,
                        (y1 - y0) as i32
                    )
                );
            }
        });
        self.last_committed = Some(player.seq);
        self.request_frame(wl);
        req!(wl, wl_surface::req::commit(surface));
    }

    /// Starts a transition from `old` to `new`, which is the image of the
    /// wall of this output. Returns false when the output cannot show it.
    pub fn start_fx(
        &mut self,
        wl: &mut Wl,
        id: u32,
        effect: &Effect,
        old: Canvas,
        new: &Canvas,
    ) -> bool {
        if self.logical.is_none() {
            return false;
        }
        let screen = self.physical();
        let Ok(mut pool) = Pool::new(wl, screen.0, screen.1) else {
            return false;
        };
        if let Some(old_pool) = self.pool.take() {
            old_pool.destroy(wl);
        }
        self.callback = None;
        self.geom = None;
        self.last_committed = None;
        let fx = Fx {
            id,
            old_geom: old.geom(screen),
            new_geom: new.geom(screen),
            geom: Geom::new((GRID_W as u32, GRID_H as u32), screen),
            grey: match effect.kind {
                Kind::Sweep => old.grey(),
                _ => Box::new([]),
            },
            old,
            last_committed: effect.step,
        };
        effect.render(pool.pixels(0), &fx.geom, &fx.layers(new), None);
        pool.bufs[0].holds = Some(effect.step as u64);
        self.fx = Some(fx);
        self.commit_full(wl, Some(pool), None, true);
        true
    }

    /// Draws the current step of the transition: only the cells that changed.
    /// There is no allocation when a buffer is free.
    pub fn fx_tick(&mut self, wl: &mut Wl, effect: &Effect, new: &Canvas) {
        let (Some(pool), Some(fx)) = (&mut self.pool, &mut self.fx) else {
            return;
        };
        if fx.last_committed == effect.step {
            return;
        }
        let damage = if fx.last_committed + 1 == effect.step {
            let Some(area) = effect.changed().and_then(|r| fx.geom.map(r)) else {
                // no change on this screen
                fx.last_committed = effect.step;
                return;
            };
            area
        } else {
            (0, 0, fx.geom.sw, fx.geom.sh)
        };
        let Some(i) = pool.acquire(wl) else {
            return;
        };
        let holds = pool.bufs[i].holds.map(|step| step as u32);
        effect.render(pool.pixels(i), &fx.geom, &fx.layers(new), holds);
        pool.bufs[i].holds = Some(effect.step as u64);
        pool.bufs[i].busy = true;
        fx.last_committed = effect.step;

        let surface = self.surface;
        let (x0, y0, x1, y1) = damage;
        req!(
            wl,
            wl_surface::req::attach(surface, Some(pool.bufs[i].id), 0, 0)
        );
        req!(
            wl,
            wl_surface::req::damage_buffer(
                surface,
                x0 as i32,
                y0 as i32,
                (x1 - x0) as i32,
                (y1 - y0) as i32
            )
        );
        self.request_frame(wl);
        req!(wl, wl_surface::req::commit(surface));
    }

    /// Handles `wl_buffer.release`. Returns false when the buffer is not from this output.
    ///
    /// A still or a colour needs its buffer only one time: the pool is destroyed
    /// when the compositor has released it.
    pub fn released(&mut self, wl: &mut Wl, buffer: ObjectId) -> bool {
        let animated = self.player().is_some() || self.fx.is_some();
        let Some(pool) = &mut self.pool else {
            return false;
        };
        if !pool.release(buffer) {
            return false;
        }
        if !animated && pool.all_free() {
            self.pool.take().unwrap().destroy(wl);
        }
        true
    }

    pub fn destroy(mut self, wl: &mut Wl) {
        if let Some(pool) = self.pool.take() {
            pool.destroy(wl);
        }
        req!(wl, wp_viewport::req::destroy(self.viewport));
        if let Some(fractional) = self.fractional {
            req!(wl, wp_fractional_scale_v1::req::destroy(fractional));
        }
        req!(wl, zwlr_layer_surface_v1::req::destroy(self.layer_surface));
        req!(wl, wl_surface::req::destroy(self.surface));
        req!(wl, wl_output::req::release(self.wl_output));
    }
}

/// Aspect-fill crop of a `buf` image for a `logical` surface, in 1/256 of a
/// buffer pixel: (x, y, w, h). The rect is always inside the buffer.
pub fn fill_crop(buf: (u32, u32), logical: (u32, u32)) -> [i32; 4] {
    let (bw, bh) = (buf.0 as u64 * 256, buf.1 as u64 * 256);
    let (lw, lh) = (logical.0.max(1) as u64, logical.1.max(1) as u64);
    let (cw, ch) = if bw * lh > bh * lw {
        // the image is wider than the surface: cut left and right
        ((bh * lw / lh).clamp(1, bw), bh)
    } else {
        (bw, (bw * lh / lw).clamp(1, bh))
    };
    [(bw - cw) / 2, (bh - ch) / 2, cw, ch].map(|v| v as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_crop_table() {
        // same aspect: no crop
        assert_eq!(
            fill_crop((1920, 1080), (1280, 720)),
            [0, 0, 1920 * 256, 1080 * 256]
        );
        // wider image: cut the sides
        assert_eq!(
            fill_crop((4000, 1000), (1000, 1000)),
            [1500 * 256, 0, 1000 * 256, 1000 * 256]
        );
        // taller image: cut top and bottom
        assert_eq!(
            fill_crop((1000, 3000), (2000, 1000)),
            [0, 1250 * 256, 1000 * 256, 500 * 256]
        );
        // the rect stays inside the buffer for odd sizes
        for (b, l) in [
            ((1, 1), (3840, 2160)),
            ((16384, 3), (7, 5000)),
            ((333, 777), (1707, 1067)),
        ] {
            let [x, y, w, h] = fill_crop(b, l);
            assert!(x >= 0 && y >= 0 && w > 0 && h > 0);
            assert!(x + w <= b.0 as i32 * 256 && y + h <= b.1 as i32 * 256);
        }
    }
}
