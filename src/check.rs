//! `waypixel check`: lists the images that `waypixel img` rejects.

use std::path::{Path, PathBuf};

use crate::decode;

/// The file name extensions of the image formats that waypixel decodes.
const EXTENSIONS: [&str; 6] = ["gif", "png", "apng", "webp", "jpg", "jpeg"];

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// The image files in the directory `dir`, sorted. A file gives only itself.
fn images(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if dir.is_file() {
        return Ok(vec![dir.to_owned()]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_image(p) && p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

/// The rejected images of `dir`, each with the reason.
fn rejected(dir: &Path) -> std::io::Result<Vec<(PathBuf, String)>> {
    Ok(images(dir)?
        .into_iter()
        .filter_map(|p| decode::check(&p).err().map(|e| (p, e)))
        .collect())
}

/// Prints one line for each rejected image. Returns the exit code: 0 = all
/// images are pixel art, 1 = some are not, or an error.
pub fn run(dir: &Path) -> i32 {
    match rejected(dir) {
        Ok(bad) => {
            for (path, reason) in &bad {
                println!("{}: {reason}", path.display());
            }
            !bad.is_empty() as i32
        }
        Err(e) => {
            eprintln!("waypixel: {}: {e}", dir.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::tests::temp;
    use crate::pixelart::NOT_PIXEL_ART;
    use crate::pixelart::tests::{Rng, checker, scaled};
    use image::RgbaImage;

    fn save(path: &Path, v: &[u32], w: u32, h: u32) {
        let raw = v
            .iter()
            .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255])
            .collect();
        RgbaImage::from_raw(w, h, raw).unwrap().save(path).unwrap();
    }

    #[test]
    fn lists_only_the_rejected_images() {
        let dir = temp("check-dir");
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (art, w, h) = scaled(&checker(6, 5), 6, 5, 8);
        save(&dir.join("art.PNG"), &art, w, h);
        let mut rng = Rng(6);
        let photo: Vec<u32> = (0..31 * 17)
            .map(|_| 0xFF00_0000 | rng.next() >> 8)
            .collect();
        save(&dir.join("photo.png"), &photo, 31, 17);
        std::fs::write(dir.join("broken.gif"), "hello").unwrap();
        // not an image name: ignored
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();

        let bad = rejected(&dir).unwrap();
        let names: Vec<_> = bad
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["broken.gif", "photo.png"]);
        assert_eq!(bad[1].1, NOT_PIXEL_ART);
        assert_eq!(run(&dir), 1);
        assert_eq!(run(&dir.join("art.PNG")), 0);
        assert_eq!(run(&dir.join("missing")), 1);
    }
}
