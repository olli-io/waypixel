//! `pixelwp still`: writes the wallpaper of one output as a PNG.
//!
//! A session lock hides the wallpaper surface. A lock screen can show this file.
//! Pixel art is written at the size of the output, with the integer scale of
//! the daemon. Other images are written at their stored size.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use image::ImageEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};

use crate::cache::{self, Cache};
use crate::diff::Rect;
use crate::scale::{Geom, upscale_rect};
use crate::{client, ipc};

/// One line of the `query` reply.
#[derive(Debug, PartialEq)]
struct Line<'a> {
    name: &'a str,
    size: (u32, u32),
    /// a file path or `color #RRGGBB`
    what: &'a str,
}

fn parse_line(line: &str) -> Option<Line<'_>> {
    let (name, rest) = line.split_once(' ')?;
    let (size, rest) = rest.split_once(' ')?;
    let (what, _state) = rest.rsplit_once(' ')?;
    let (w, h) = size.split_once('x')?;
    Some(Line {
        name,
        size: (w.parse().ok()?, h.parse().ok()?),
        what,
    })
}

/// Returns the pixels (XRGB8888) and the size.
fn render(line: &Line) -> Result<(Vec<u32>, u32, u32), String> {
    if let Some(color) = line.what.strip_prefix("color ") {
        let color = ipc::parse_color(color).ok_or("bad reply from the daemon")?;
        return Ok((vec![color], 1, 1));
    }
    let hash = cache::hash_file(line.what.as_ref()).map_err(|e| format!("{}: {e}", line.what))?;
    let cache = Cache::open(&cache::path_for(&hash))?;
    let mut canvas = vec![0u32; cache.w as usize * cache.h as usize];
    let mut scratch = vec![0u8; cache.scratch_len];
    cache.apply(0, &mut canvas, &mut scratch)?;
    if !cache.pixel_art {
        return Ok((canvas, cache.w, cache.h));
    }
    let (sw, sh) = line.size;
    if sw == 0 || sh == 0 {
        return Err(format!("output {} has no size", line.name));
    }
    let geom = Geom::new((cache.w, cache.h), (sw, sh));
    let mut out = vec![0u32; sw as usize * sh as usize];
    upscale_rect(&canvas, &mut out, &geom, Rect::new(0, 0, cache.w, cache.h));
    Ok((out, sw, sh))
}

fn write_png(dst: &Path, pixels: &[u32], w: u32, h: u32) -> Result<(), String> {
    let rgb: Vec<u8> = pixels
        .iter()
        .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8])
        .collect();
    // a rename, so a reader never sees a partial file
    let tmp = dst.with_extension(format!("tmp{}", std::process::id()));
    let result = File::create(&tmp)
        .map_err(|e| e.to_string())
        .and_then(|file| {
            PngEncoder::new_with_quality(
                BufWriter::new(file),
                CompressionType::Fast,
                FilterType::Adaptive,
            )
            .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .map_err(|e| e.to_string())
        })
        .and_then(|()| std::fs::rename(&tmp, dst).map_err(|e| e.to_string()));
    if result.is_err() {
        _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Returns the exit code.
pub fn run(dst: &Path, output: Option<&str>) -> i32 {
    let body = match client::fetch(&["query"]) {
        Ok(body) => body,
        Err(code) => return code,
    };
    let lines = body.lines().filter_map(parse_line);
    let line = match output {
        Some(name) => lines.into_iter().find(|l| l.name == name),
        None => lines.max_by_key(|l| l.size.0 as u64 * l.size.1 as u64),
    };
    let result = line
        .ok_or_else(|| match output {
            Some(name) => format!("no output {name}"),
            None => "no outputs".into(),
        })
        .and_then(|line| render(&line))
        .and_then(|(pixels, w, h)| write_png(dst, &pixels, w, h));
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("pixelwp: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_lines() {
        assert_eq!(
            parse_line("DP-2 3840x2160 /a b/c d.gif playing"),
            Some(Line {
                name: "DP-2",
                size: (3840, 2160),
                what: "/a b/c d.gif",
            })
        );
        assert_eq!(
            parse_line("X 1x2 color #202020 static").map(|l| l.what),
            Some("color #202020")
        );
        assert_eq!(parse_line("DP-2 3840 /a.gif playing"), None);
    }
}
