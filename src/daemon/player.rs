//! Player: the 1× canvas of one animation, its frame index and its deadline.
//!
//! The player has no Wayland code. A Target asks it for the rects that changed
//! since the frame that a buffer holds.

use crate::cache::{Cache, Hash};
use crate::diff::{MAX_RECTS, Rect};
use crate::scale::{Geom, upscale_rect};

/// Number of past frames for which the player keeps the rect lists.
pub const RING: usize = 8;

#[derive(Clone, Copy, Default)]
struct RectList {
    rects: [Rect; MAX_RECTS],
    len: usize,
}

pub struct Player {
    pub id: u32,
    pub hash: Hash,
    cache: Cache,
    /// frame `frame` at the stored size
    canvas: Box<[u32]>,
    scratch: Box<[u8]>,
    /// index of the current frame in the file
    frame: usize,
    /// count of frames shown since the start; it does not wrap at the loop
    pub seq: u64,
    /// rect lists of frames `seq − 7 ..= seq`, at index `seq % RING`
    ring: [RectList; RING],
    /// the rects that change from the last frame to frame 0, at the loop
    wrap: RectList,
    /// `CLOCK_MONOTONIC` time in ns of the next frame; `None` when hidden or paused
    pub deadline: Option<u64>,
}

impl Player {
    pub fn new(id: u32, hash: Hash, cache: Cache) -> Result<Self, String> {
        let mut player = Self {
            id,
            hash,
            canvas: vec![0; cache.w as usize * cache.h as usize].into(),
            scratch: vec![0; cache.scratch_len].into(),
            cache,
            frame: 0,
            seq: 0,
            ring: [RectList::default(); RING],
            wrap: RectList::default(),
            deadline: None,
        };
        player.load(0)?;
        player.find_wrap()?;
        Ok(player)
    }

    /// Finds the rects that change at the loop, so the loop does not damage the
    /// full surface. The file stores frame 0 only as a full frame. This plays
    /// the animation one time on the 1× canvas, at the start only.
    fn find_wrap(&mut self) -> Result<(), String> {
        let (w, h) = (self.cache.w as usize, self.cache.h as usize);
        let first = self.canvas.clone();
        for frame in 1..self.cache.frame_count() {
            self.cache
                .apply(frame, &mut self.canvas, &mut self.scratch)?;
        }
        for rect in crate::diff::Differ::default().diff(&self.canvas, &first, w, h) {
            self.wrap.rects[self.wrap.len] = rect;
            self.wrap.len += 1;
        }
        self.canvas = first;
        Ok(())
    }

    pub fn size(&self) -> (u32, u32) {
        (self.cache.w, self.cache.h)
    }

    /// Time on screen of the current frame.
    pub fn delay_ns(&self) -> u64 {
        self.cache.delay_ms(self.frame) as u64 * 1_000_000
    }

    fn load(&mut self, frame: usize) -> Result<(), String> {
        self.cache
            .apply(frame, &mut self.canvas, &mut self.scratch)?;
        self.frame = frame;
        let list = &mut self.ring[(self.seq % RING as u64) as usize];
        if frame == 0 && self.seq > 0 {
            *list = self.wrap;
            return Ok(());
        }
        list.len = 0;
        for rect in self.cache.rects(frame) {
            list.rects[list.len] = rect;
            list.len += 1;
        }
        Ok(())
    }

    /// Goes to the next frame. There is no allocation.
    pub fn advance(&mut self) -> Result<(), String> {
        self.seq += 1;
        self.load((self.frame + 1) % self.cache.frame_count())
    }

    /// Calls `f` for the 1× rects that changed after frame `since`. When the ring
    /// cannot cover the range, this is one full-frame rect.
    pub fn dirty(&self, since: Option<u64>, mut f: impl FnMut(Rect)) {
        match since {
            Some(j) if j <= self.seq && self.seq - j <= RING as u64 => {
                for s in j + 1..=self.seq {
                    let list = &self.ring[(s % RING as u64) as usize];
                    list.rects[..list.len].iter().for_each(|r| f(*r));
                }
            }
            _ => f(Rect::new(0, 0, self.cache.w, self.cache.h)),
        }
    }

    /// Brings a screen buffer that holds frame `holds` up to the current frame.
    pub fn render(&self, dst: &mut [u32], g: &Geom, holds: Option<u64>) {
        self.dirty(holds, |r| upscale_rect(&self.canvas, dst, g, r));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::tests::{synthetic, temp, write};

    fn full(src: &[u32], g: &Geom) -> Vec<u32> {
        let mut out = vec![0; g.sw * g.sh];
        for y in 0..g.sh {
            for x in 0..g.sw {
                out[y * g.sw + x] = src[(y + g.oy) / g.k * g.w + (x + g.ox) / g.k];
            }
        }
        out
    }

    struct Buf {
        px: Vec<u32>,
        holds: Option<u64>,
    }

    /// Plays `loops` loops on `nbufs` buffers in rotation. `shown(tick)` tells if
    /// the output is ready on that tick.
    fn play(frame_count: usize, nbufs: usize, loops: usize, shown: impl Fn(usize) -> bool) {
        let (w, h) = (40, 24);
        let frames = synthetic(w, h, frame_count, 100 + frame_count as u64);
        let path = temp(&format!(
            "age-{frame_count}-{nbufs}-{:p}.pwp",
            &shown as *const _
        ));
        write(&path, &frames, w, h, 50);
        let cache = Cache::open(&path).unwrap();
        let mut player = Player::new(1, [0; 32], cache).unwrap();
        let g = Geom::new((w as u32, h as u32), (131, 77));
        let mut bufs: Vec<Buf> = (0..nbufs)
            .map(|_| Buf {
                px: vec![0xDEAD_BEEF; g.sw * g.sh],
                holds: None,
            })
            .collect();
        let mut next = 0;
        for tick in 0..frame_count * loops {
            if tick > 0 {
                player.advance().unwrap();
            }
            if !shown(tick) {
                continue;
            }
            let buf = &mut bufs[next];
            next = (next + 1) % nbufs;
            player.render(&mut buf.px, &g, buf.holds);
            buf.holds = Some(player.seq);
            let want = full(&frames[tick % frame_count], &g);
            assert!(
                buf.px == want,
                "tick {tick}, {frame_count} frames, {nbufs} bufs"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn buffer_age() {
        for frame_count in 1..=3 {
            for nbufs in 1..=3 {
                play(frame_count, nbufs, 3, |_| true);
            }
        }
    }

    #[test]
    fn buffer_age_with_skipped_frames() {
        for frame_count in [1, 2, 3, 7] {
            for nbufs in 1..=3 {
                // skips of 1, 2 and many frames (more than the ring)
                play(frame_count, nbufs, 12, |t| t % 3 != 1);
                play(frame_count, nbufs, 12, |t| t % 5 < 2);
                play(frame_count, nbufs, 12, |t| t % 17 < 3);
            }
        }
    }

    #[test]
    fn dirty_sets() {
        let (w, h) = (40, 24);
        let frames = synthetic(w, h, 3, 55);
        let path = temp("dirty.pwp");
        write(&path, &frames, w, h, 50);
        let mut player = Player::new(1, [0; 32], Cache::open(&path).unwrap()).unwrap();
        let count = |p: &Player, since| {
            let mut n = 0;
            p.dirty(since, |_| n += 1);
            n
        };
        let whole = Rect::new(0, 0, w as u32, h as u32);
        // nothing changed since the current frame
        assert_eq!(count(&player, Some(0)), 0);
        player.dirty(None, |r| assert_eq!(r, whole));
        player.advance().unwrap();
        assert!(count(&player, Some(0)) >= 1);
        assert_eq!(count(&player, Some(1)), 0);
        for _ in 0..RING {
            player.advance().unwrap();
        }
        // too old for the ring: one full-frame rect
        assert_eq!(count(&player, Some(0)), 1);
        player.dirty(Some(0), |r| assert_eq!(r, whole));
        assert_eq!(player.delay_ns(), 50_000_000);
        std::fs::remove_file(path).unwrap();
    }
}
