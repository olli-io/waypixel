//! Decode child: image → XRGB → pixel-art detection → tile diff → cache file.
//!
//! This runs in a separate process (`pixelwp decode <src> <dst>`), so all decode
//! memory goes away at exit.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
use image::{AnimationDecoder, DynamicImage, Frames, ImageDecoder, ImageFormat, ImageReader};

use crate::cache::{MAX_SIDE, Writer};
use crate::diff::{Differ, Rect};
use crate::pixelart::{MAX_1X_SIDE, NOT_PIXEL_ART, Palette, factor, holds, shrink};

/// Frame delays under this value are replaced by `DEFAULT_DELAY_MS`.
const MIN_DELAY_MS: u32 = 20;
const DEFAULT_DELAY_MS: u32 = 100;

enum Source {
    Still(image::RgbaImage),
    Anim(Frames<'static>),
}

fn open(src: &Path) -> Result<Source, String> {
    let e = |e: image::ImageError| e.to_string();
    let reader = ImageReader::open(src)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    let format = reader.format().ok_or("unknown image format")?;
    let anim: Option<Frames<'static>> = match format {
        ImageFormat::Gif => Some(
            GifDecoder::new(reader.into_inner())
                .map_err(e)?
                .into_frames(),
        ),
        ImageFormat::Png => {
            let decoder = PngDecoder::new(reader.into_inner()).map_err(e)?;
            if decoder.is_apng().map_err(e)? {
                Some(decoder.apng().map_err(e)?.into_frames())
            } else {
                None
            }
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(reader.into_inner()).map_err(e)?;
            decoder.has_animation().then(|| decoder.into_frames())
        }
        _ => None,
    };
    if let Some(frames) = anim {
        return Ok(Source::Anim(frames));
    }

    let mut reader = ImageReader::with_format(
        BufReader::new(File::open(src).map_err(|e| e.to_string())?),
        format,
    );
    reader.no_limits();
    let mut decoder = reader.into_decoder().map_err(e)?;
    let orientation = decoder.orientation().map_err(e)?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(e)?;
    img.apply_orientation(orientation);
    if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
        img = img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle);
    }
    Ok(Source::Still(img.into_rgba8()))
}

/// RGBA → XRGB8888, composited over black.
fn to_xrgb(rgba: &[u8]) -> Vec<u32> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            let a = p[3] as u32;
            let c = |v: u8| v as u32 * a / 255;
            0xFF00_0000 | c(p[0]) << 16 | c(p[1]) << 8 | c(p[2])
        })
        .collect()
}

fn clamp_delay(ms: u32) -> u32 {
    if ms < MIN_DELAY_MS {
        DEFAULT_DELAY_MS
    } else {
        ms
    }
}

enum Pass {
    Done,
    /// A frame does not hold the block size. Decode again with this smaller size.
    Restart(u32),
}

struct Encoder<'a> {
    dst: &'a Path,
    /// block size; 0 = not known yet
    n: u32,
    size: (u32, u32),
    /// the colours of all frames; used only when N = 1
    palette: Palette,
    writer: Option<Writer>,
    prev: Vec<u32>,
    differ: Differ,
}

impl Encoder<'_> {
    /// Adds one full-size frame. Returns the smaller block size when a restart is necessary.
    fn push(&mut self, rgba: &image::RgbaImage, delay_ms: u32) -> Result<Option<u32>, String> {
        let e = |e: std::io::Error| format!("cannot write cache file: {e}");
        let (w, h) = rgba.dimensions();
        let xrgb = to_xrgb(rgba.as_raw());
        if self.n == 0 {
            self.n = factor(&xrgb, w, h, 0);
        }
        let n = self.n;
        if !holds(&xrgb, w, h, n) {
            return Ok(Some(factor(&xrgb, w, h, n)));
        }
        // N = 1: only small art with few colours is pixel art
        if n == 1 && !(w.max(h) <= MAX_1X_SIDE && self.palette.add(&xrgb)) {
            return Err(NOT_PIXEL_ART.into());
        }
        let small = if n > 1 { shrink(&xrgb, w, h, n) } else { xrgb };
        let (sw, sh) = (w / n, h / n);

        let Some(writer) = &mut self.writer else {
            let mut writer = Writer::create(self.dst, sw, sh).map_err(e)?;
            let full = [Rect::new(0, 0, sw, sh)];
            writer.add_frame(delay_ms, &full, &small).map_err(e)?;
            self.writer = Some(writer);
            self.size = (w, h);
            self.prev = small;
            return Ok(None);
        };
        if (w, h) != self.size {
            return Err("frames have different sizes".into());
        }
        let rects = self
            .differ
            .diff(&self.prev, &small, sw as usize, sh as usize);
        if rects.is_empty() {
            writer.add_delay(delay_ms);
        } else {
            writer.add_frame(delay_ms, &rects, &small).map_err(e)?;
            self.prev = small;
        }
        Ok(None)
    }
}

fn pass(src: &Path, dst: &Path, n: u32) -> Result<Pass, String> {
    let mut enc = Encoder {
        dst,
        n,
        size: (0, 0),
        palette: Palette::default(),
        writer: None,
        prev: Vec::new(),
        differ: Differ::default(),
    };
    match open(src)? {
        Source::Still(img) => {
            enc.push(&img, 0)?;
        }
        Source::Anim(frames) => {
            for frame in frames {
                let frame = frame.map_err(|e| e.to_string())?;
                let (num, den) = frame.delay().numer_denom_ms();
                let delay = clamp_delay(num / den.max(1));
                let img = frame.into_buffer();
                if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
                    return Err(format!("animation is larger than {MAX_SIDE} px"));
                }
                if let Some(n) = enc.push(&img, delay)? {
                    return Ok(Pass::Restart(n));
                }
            }
        }
    }
    let writer = enc.writer.ok_or("image has no frames")?;
    writer
        .finish()
        .map_err(|e| format!("cannot write cache file: {e}"))?;
    Ok(Pass::Done)
}

/// Decodes `src` and writes the cache file `dst`.
pub fn run(src: &Path, dst: &Path) -> Result<(), String> {
    let mut n = 0;
    loop {
        match pass(src, dst, n)? {
            Pass::Done => return Ok(()),
            Pass::Restart(smaller) => n = smaller,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::cache::{Cache, tests::temp};
    use crate::pixelart::tests::{Rng, checker, scaled};
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, RgbaImage};

    fn to_rgba(v: &[u32], w: u32, h: u32) -> RgbaImage {
        let raw = v
            .iter()
            .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255])
            .collect();
        RgbaImage::from_raw(w, h, raw).unwrap()
    }

    /// Writes a GIF from XRGB frames with (delay in ms).
    pub fn write_gif(path: &Path, frames: &[(Vec<u32>, u32)], w: u32, h: u32) {
        let mut enc = GifEncoder::new_with_speed(File::create(path).unwrap(), 1);
        enc.set_repeat(Repeat::Infinite).unwrap();
        for (f, delay) in frames {
            let delay = Delay::from_numer_denom_ms(*delay, 1);
            enc.encode_frame(Frame::from_parts(to_rgba(f, w, h), 0, 0, delay))
                .unwrap();
        }
    }

    /// 1× animation with a few colours: a pattern with small changes per frame.
    pub fn art_frames(w: usize, h: usize, count: usize, seed: u64) -> Vec<Vec<u32>> {
        const COLOURS: [u32; 4] = [0xFF10_2030, 0xFFE0_4040, 0xFF40_E040, 0xFF40_40E0];
        let mut rng = Rng(seed);
        let mut frames = vec![
            (0..w * h)
                .map(|_| COLOURS[rng.below(4) as usize])
                .collect::<Vec<u32>>(),
        ];
        for i in 1..count {
            let mut f = frames[i - 1].clone();
            for _ in 0..1 + rng.below(6) {
                // always a real change, so no two frames are identical
                let p = rng.below((w * h) as u32) as usize;
                let old = COLOURS.iter().position(|c| *c == f[p]).unwrap();
                f[p] = COLOURS[(old + 1 + rng.below(3) as usize) % 4];
            }
            frames.push(f);
        }
        frames
    }

    fn replay(cache: &Cache) -> Vec<Vec<u32>> {
        let mut canvas = vec![0u32; (cache.w * cache.h) as usize];
        let mut scratch = vec![0u8; cache.scratch_len];
        (0..cache.frame_count())
            .map(|i| {
                cache.apply(i, &mut canvas, &mut scratch).unwrap();
                canvas.clone()
            })
            .collect()
    }

    #[test]
    fn gif_round_trip_is_bit_exact() {
        for (n, seed) in [(4usize, 11u64), (3, 12), (2, 13)] {
            let (w, h) = (24, 16);
            let art = art_frames(w, h, 10, seed);
            let big: Vec<(Vec<u32>, u32)> =
                art.iter().map(|f| (scaled(f, w, h, n).0, 70)).collect();
            let gif = temp(&format!("rt-{seed}.gif"));
            let pwp = temp(&format!("rt-{seed}-gif.pwp"));
            write_gif(&gif, &big, (w * n) as u32, (h * n) as u32);
            run(&gif, &pwp).unwrap();
            let cache = Cache::open(&pwp).unwrap();
                assert_eq!((cache.w as usize, cache.h as usize), (w, h));
            assert_eq!(replay(&cache), art);
            assert!((0..cache.frame_count()).all(|i| cache.delay_ms(i) == 70));
            // no temp file stays
            let leftovers = std::fs::read_dir(pwp.parent().unwrap())
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
                .count();
            assert_eq!(leftovers, 0);
        }
    }

    /// Frame 0 is a 4× grid, frame 1 holds only a 2× grid: the decode restarts with N = 2.
    #[test]
    fn restart_with_smaller_block() {
        let (f0, w, h) = scaled(&checker(5, 4), 5, 4, 4);
        let mut f1 = f0.clone();
        for y in 0..2 {
            for x in 0..2 {
                f1[y * w as usize + x] = 0xFF00_FF00;
            }
        }
        let gif = temp("restart.gif");
        let pwp = temp("restart.pwp");
        write_gif(&gif, &[(f0.clone(), 100), (f1.clone(), 100)], w, h);
        run(&gif, &pwp).unwrap();
        let cache = Cache::open(&pwp).unwrap();
        assert_eq!((cache.w, cache.h), (w / 2, h / 2));
        assert_eq!(cache.frame_count(), 2);
        let frames = replay(&cache);
        assert_eq!(frames[0], shrink(&f0, w, h, 2));
        assert_eq!(frames[1], shrink(&f1, w, h, 2));
    }

    #[test]
    fn delay_clamp_and_identical_merge() {
        let (a, w, h) = scaled(&checker(4, 4), 4, 4, 2);
        let mut b = a.clone();
        b[..2].fill(0xFF00_FF00);
        b[w as usize..w as usize + 2].fill(0xFF00_FF00);
        let gif = temp("delay.gif");
        let pwp = temp("delay.pwp");
        // 10 ms is under the limit and becomes 100 ms; the two `b` frames merge
        write_gif(&gif, &[(a, 10), (b.clone(), 40), (b, 50)], w, h);
        run(&gif, &pwp).unwrap();
        let cache = Cache::open(&pwp).unwrap();
        assert_eq!(cache.frame_count(), 2);
        assert_eq!((cache.delay_ms(0), cache.delay_ms(1)), (100, 90));
    }

    #[test]
    fn image_that_is_not_pixel_art_is_rejected() {
        // too many colours: 2 frames of 200 colours each
        let f0: Vec<u32> = (0..200).map(|i| 0xFF00_0000 | i).collect();
        let f1: Vec<u32> = (0..200).map(|i| 0xFF00_8000 | i).collect();
        let gif = temp("colours.gif");
        let pwp = temp("colours.pwp");
        write_gif(&gif, &[(f0, 100), (f1, 100)], 20, 10);
        assert_eq!(run(&gif, &pwp).err().unwrap(), NOT_PIXEL_ART);
        assert!(!pwp.exists());

        // too large for art stored at 1×
        let w = MAX_1X_SIDE + 1;
        let f0 = art_frames(w as usize, 3, 2, 5);
        let gif = temp("large.gif");
        let pwp = temp("large.pwp");
        write_gif(&gif, &[(f0[0].clone(), 100), (f0[1].clone(), 100)], w, 3);
        assert_eq!(run(&gif, &pwp).err().unwrap(), NOT_PIXEL_ART);
        assert!(!pwp.exists());
    }

    /// Art stored at 1× (N = 1): small, with few colours.
    #[test]
    fn art_at_1x() {
        let (w, h) = (23, 17);
        let art = art_frames(w, h, 6, 21);
        let frames: Vec<(Vec<u32>, u32)> = art.iter().map(|f| (f.clone(), 80)).collect();
        let gif = temp("1x.gif");
        let pwp = temp("1x-gif.pwp");
        write_gif(&gif, &frames, w as u32, h as u32);
        run(&gif, &pwp).unwrap();
        let cache = Cache::open(&pwp).unwrap();
        assert_eq!((cache.w as usize, cache.h as usize), (w, h));
        assert_eq!(replay(&cache), art);

        let png = temp("1x.png");
        let pwp = temp("1x-png.pwp");
        to_rgba(&art[0], w as u32, h as u32).save(&png).unwrap();
        run(&png, &pwp).unwrap();
        let cache = Cache::open(&pwp).unwrap();
        assert_eq!(replay(&cache)[0], art[0]);
    }

    #[test]
    fn stills() {
        // pixel-art PNG: stored at 1×
        let (v, w, h) = scaled(&checker(6, 5), 6, 5, 8);
        let png = temp("art.png");
        let pwp = temp("art.pwp");
        to_rgba(&v, w, h).save(&png).unwrap();
        run(&png, &pwp).unwrap();
        let cache = Cache::open(&pwp).unwrap();
        assert_eq!((cache.w, cache.h, cache.frame_count()), (6, 5, 1));
        assert_eq!(replay(&cache)[0], checker(6, 5));

        // normal image: rejected
        let mut rng = Rng(6);
        let v: Vec<u32> = (0..31 * 17)
            .map(|_| 0xFF00_0000 | rng.next() >> 8)
            .collect();
        let png = temp("photo.png");
        let pwp = temp("photo.pwp");
        to_rgba(&v, 31, 17).save(&png).unwrap();
        assert_eq!(run(&png, &pwp).err().unwrap(), NOT_PIXEL_ART);
        assert!(!pwp.exists());
    }

    #[test]
    fn transparent_pixels_compare_equal() {
        assert_eq!(to_xrgb(&[200, 100, 50, 0]), to_xrgb(&[1, 2, 3, 0]));
        assert_eq!(to_xrgb(&[255, 128, 0, 255]), [0xFFFF_8000]);
        assert_eq!(to_xrgb(&[255, 255, 255, 128]), [0xFF80_8080]);
    }

    #[test]
    fn bad_input_is_an_error() {
        let txt = temp("not-an-image.txt");
        std::fs::write(&txt, "hello").unwrap();
        assert!(run(&txt, &temp("x.pwp")).is_err());
        assert!(run(Path::new("/nonexistent.gif"), &temp("y.pwp")).is_err());
    }
}
