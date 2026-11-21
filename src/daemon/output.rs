//! One output: its surface, viewport, scale, and the Target that draws the wallpaper.

use waybackend::types::ObjectId;

use super::player::Player;
use super::pool::{MAX_BUFFERS, Pool};
use super::wayland::{Obj, Wl, req, zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use crate::cache::{Cache, Hash};
use crate::diff::Rect;
use crate::fx::{Area, Canvas, Effect, GRID_H, GRID_W, Kind, Layers, grey};
use crate::scale::{Anchor, Geom, upscale_rect};

/// What an output shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Wall {
    /// XRGB colour
    Color(u32),
    /// a still image; the cache file is opened only for a render
    Still {
        hash: Hash,
        path: String,
        anchor: Anchor,
    },
    /// a pixel-art animation, drawn from the player with this id
    Anim {
        player: u32,
        path: String,
        anchor: Anchor,
    },
}

/// What a buffer shows during a transition.
#[derive(Clone, Copy, PartialEq)]
struct Held {
    /// the step of the effect
    step: u32,
    /// the frames (player `seq`) of the old and the new wallpaper; 0 for no player
    old: u64,
    new: u64,
}

/// The part of a transition that is specific to one output.
struct Fx {
    /// id of the transition
    id: u32,
    /// the wallpaper before the transition, at the start of the transition
    old: Canvas,
    /// the player of the old wallpaper, which has the current pixels of `old`
    old_player: Option<u32>,
    /// the old wallpaper in grey, for sweep
    grey: Box<[u32]>,
    /// the frame of the old player that `grey` shows
    grey_seq: u64,
    old_geom: Geom,
    new_geom: Geom,
    /// grid of the effect → screen
    geom: Geom,
    /// what each buffer of the pool shows
    held: [Option<Held>; MAX_BUFFERS],
    last_committed: Held,
}

fn find(players: &[Player], id: Option<u32>) -> Option<&Player> {
    players.iter().find(|p| Some(p.id) == id)
}

/// The current pixels of a wallpaper: from its player, or else from the canvas.
fn pixels<'a>(player: Option<&'a Player>, canvas: &'a Canvas) -> &'a [u32] {
    player.map_or(&canvas.px[..], Player::canvas)
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

    /// The player that this output draws from. During a transition, this is
    /// `None`: the steps of the transition draw the frames.
    pub fn playing(&self) -> Option<u32> {
        self.player().filter(|_| self.fx.is_none())
    }

    /// True when the output shows frames of this player: as its wallpaper, or
    /// as the old wallpaper of its transition.
    pub fn uses(&self, player: u32) -> bool {
        let old = self.fx.as_ref().and_then(|fx| fx.old_player);
        self.player() == Some(player) || old == Some(player)
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
        let Some(_) = self.logical else {
            return Ok(());
        };
        if let Some(pool) = self.pool.take() {
            pool.destroy(wl);
        }
        self.callback = None;
        self.geom = None;
        self.last_committed = None;
        self.fx = None;
        let result = self.build(wl, players);
        if result.is_err() {
            let black = self.color_pool(wl, 0xFF00_0000);
            self.commit_full(wl, black, false);
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
    ) -> Result<(), String> {
        let (pw, ph) = self.physical();
        let shm = |e: rustix::io::Errno| format!("cannot create a shm buffer: {e}");
        match &self.wall {
            Wall::Color(color) => {
                let pool = self.color_pool(wl, *color);
                self.commit_full(wl, pool, false);
            }
            Wall::Still { hash, anchor, .. } => {
                let cache = Cache::open(&crate::cache::path_for(hash))?;
                let mut canvas = vec![0u32; cache.w as usize * cache.h as usize];
                cache.apply(0, &mut canvas, &mut [])?;
                let g = Geom::new((cache.w, cache.h), (pw, ph), *anchor);
                let mut pool = Pool::new(wl, pw, ph).map_err(shm)?;
                upscale_rect(
                    &canvas,
                    pool.pixels(0),
                    &g,
                    Rect::new(0, 0, cache.w, cache.h),
                );
                self.commit_full(wl, Some(pool), false);
            }
            Wall::Anim { player, anchor, .. } => {
                let player = players
                    .iter()
                    .find(|p| p.id == *player)
                    .ok_or("the player does not exist")?;
                let g = Geom::new(player.size(), (pw, ph), *anchor);
                let mut pool = Pool::new(wl, pw, ph).map_err(shm)?;
                player.render(pool.pixels(0), &g, None);
                pool.bufs[0].holds = Some(player.seq);
                self.geom = Some(g);
                self.last_committed = Some(player.seq);
                self.commit_full(wl, Some(pool), true);
            }
        }
        Ok(())
    }

    /// Attaches buffer 0 of the pool with full damage and commits.
    fn commit_full(
        &mut self,
        wl: &mut Wl,
        pool: Option<Pool>,
        frame: bool,
    ) {
        let Some(mut pool) = pool else {
            return;
        };
        let Some((lw, lh)) = self.logical else {
            return;
        };
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
    ///
    /// `old_player` is the player of the old wallpaper, if it is an animation.
    pub fn start_fx(
        &mut self,
        wl: &mut Wl,
        (id, effect): (u32, &Effect),
        (old, old_player): (Canvas, Option<u32>),
        new: &Canvas,
        players: &[Player],
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
        let (old_live, new_live) = (find(players, old_player), find(players, self.player()));
        let now = Held {
            step: effect.step,
            old: old_live.map_or(0, |p| p.seq),
            new: new_live.map_or(0, |p| p.seq),
        };
        let mut held = [None; MAX_BUFFERS];
        held[0] = Some(now);
        let mut fx = Fx {
            id,
            old_geom: old.geom(screen),
            new_geom: new.geom(screen),
            geom: Geom::new((GRID_W as u32, GRID_H as u32), screen, Anchor::default()),
            grey: Box::new([]),
            grey_seq: now.old,
            old,
            old_player,
            held,
            last_committed: now,
        };
        if effect.kind == Kind::Sweep {
            // `old` is the current frame of its player
            fx.grey = fx.old.grey();
        }
        let layers = Layers {
            old: (pixels(old_live, &fx.old), &fx.old_geom),
            new: (pixels(new_live, new), &fx.new_geom),
            grey: &fx.grey,
        };
        effect.render(pool.pixels(0), &fx.geom, &layers, None);
        self.fx = Some(fx);
        self.commit_full(wl, Some(pool), true);
        true
    }

    /// Draws the current step of the transition, with the current frames of
    /// the two wallpapers: only the cells that changed. There is no allocation
    /// when a buffer is free.
    pub fn fx_tick(&mut self, wl: &mut Wl, effect: &Effect, new: &Canvas, players: &[Player]) {
        let new_player = find(players, self.player());
        let (Some(pool), Some(fx)) = (&mut self.pool, &mut self.fx) else {
            return;
        };
        let old_player = find(players, fx.old_player);
        let now = Held {
            step: effect.step,
            old: old_player.map_or(0, |p| p.seq),
            new: new_player.map_or(0, |p| p.seq),
        };
        let last = fx.last_committed;
        if last == now {
            return;
        }
        let Some(i) = pool.acquire(wl) else {
            return;
        };

        // the grey of the rects that changed in the old wallpaper
        if let Some(p) = old_player
            && !fx.grey.is_empty()
        {
            let (src, dst, w) = (p.canvas(), &mut fx.grey, fx.old.w as usize);
            p.dirty(Some(fx.grey_seq), |r| {
                for y in r.y as usize..(r.y + r.h) as usize {
                    let row = y * w + r.x as usize..y * w + (r.x + r.w) as usize;
                    for (g, s) in dst[row.clone()].iter_mut().zip(&src[row]) {
                        *g = grey(*s);
                    }
                }
            });
            fx.grey_seq = p.seq;
        }

        let layers = Layers {
            old: (pixels(old_player, &fx.old), &fx.old_geom),
            new: (pixels(new_player, new), &fx.new_geom),
            grey: &fx.grey,
        };
        // (player, it is the new wallpaper, its geometry)
        let sides = [
            (old_player, false, &fx.old_geom),
            (new_player, true, &fx.new_geom),
        ];
        let held = fx.held[i];
        let dst = pool.pixels(i);
        effect.render(dst, &fx.geom, &layers, held.map(|h| h.step));
        if let Some(held) = held {
            for (player, is_new, _) in sides {
                let Some(p) = player else { continue };
                let since = if is_new { held.new } else { held.old };
                p.dirty(Some(since), |r| {
                    effect.render_layer(dst, &fx.geom, &layers, is_new, r)
                });
            }
        }
        fx.held[i] = Some(now);
        fx.last_committed = now;

        // The damage is relative to the last commit.
        let surface = self.surface;
        let mut damaged = false;
        let mut damage = |wl: &mut Wl, (x0, y0, x1, y1): Area| {
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
            damaged = true;
        };
        if last.step != now.step && last.step + 1 != now.step {
            // steps were skipped
            damage(wl, (0, 0, fx.geom.sw, fx.geom.sh));
        } else {
            if last.step != now.step
                && let Some(area) = effect.changed().and_then(|r| fx.geom.map(r))
            {
                damage(wl, area);
            }
            for (player, is_new, geom) in sides {
                let Some(p) = player else { continue };
                let since = if is_new { last.new } else { last.old };
                p.dirty(Some(since), |r| {
                    if let Some(area) = effect.layer_damage(&fx.geom, geom, is_new, r) {
                        damage(wl, area);
                    }
                });
            }
        }
        if !damaged {
            // no change on this screen
            return;
        }
        let buffer = &mut pool.bufs[i];
        buffer.busy = true;
        req!(wl, wl_surface::req::attach(surface, Some(buffer.id), 0, 0));
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
