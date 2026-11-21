//! The `.wpx` cache file: writer (decode child), reader (daemon), cleanup.
//!
//! All values are little-endian.
//! ```text
//! Header (32 B): u32 magic 'WPXL', u16 version, u16 flags (0),
//!                u32 width, u32 height, u32 frameCount, u32 reserved, u64 indexOffset
//! Payload: LZ4 blocks, 16-byte aligned
//! Index (frameCount entries): u32 delayMs, u32 rectCount,
//!                rectCount × { u16 x, u16 y, u16 w, u16 h, u64 offset, u32 size }
//! ```

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::diff::{MAX_RECTS, Rect};

#[cfg(target_endian = "big")]
compile_error!("waypixel supports little-endian targets only");

const MAGIC: [u8; 4] = *b"WPXL";
const VERSION: u16 = 3;
const HEADER: usize = 32;
const ALIGN: u64 = 16;
/// Maximum image size per side.
pub const MAX_SIDE: u32 = 16384;

pub type Hash = [u8; 32];

pub fn bytes(v: &[u32]) -> &[u8] {
    // SAFETY: u8 has no alignment requirement and every bit pattern is valid
    unsafe { core::slice::from_raw_parts(v.as_ptr().cast(), size_of_val(v)) }
}

pub fn bytes_mut(v: &mut [u32]) -> &mut [u8] {
    // SAFETY: as above; every bit pattern is also a valid u32
    unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr().cast(), size_of_val(v)) }
}

/// `$XDG_CACHE_HOME/waypixel`
pub fn dir() -> PathBuf {
    crate::ipc::xdg_dir("XDG_CACHE_HOME", ".cache").join("waypixel")
}

pub fn hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn path_for(hash: &Hash) -> PathBuf {
    dir().join(format!("{}.wpx", hex(hash)))
}

/// SHA-256 of the file bytes.
pub fn hash_file(path: &Path) -> io::Result<Hash> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        match file.read(&mut buf)? {
            0 => return Ok(hasher.finalize().into()),
            n => hasher.update(&buf[..n]),
        }
    }
}

/// Deletes every file in the cache directory that does not belong to a hash in `keep`.
pub fn cleanup(keep: &[Hash]) {
    let keep: Vec<String> = keep.iter().map(hex).collect();
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // temp files of running decode children start with the hash too
        if !keep.iter().any(|k| name.starts_with(k.as_str())) {
            _ = std::fs::remove_file(entry.path());
        }
    }
}

struct IndexRect {
    rect: Rect,
    offset: u64,
    size: u32,
}

/// Writes a cache file to a temp file, then renames it.
pub struct Writer {
    file: BufWriter<File>,
    tmp: PathBuf,
    dst: PathBuf,
    pos: u64,
    w: usize,
    h: usize,
    frames: Vec<(u32, Vec<IndexRect>)>,
    packed: Vec<u8>,
    comp: Vec<u8>,
    done: bool,
}

impl Writer {
    pub fn create(dst: &Path, w: u32, h: u32) -> io::Result<Self> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp = dst.as_os_str().to_owned();
        tmp.push(format!(".tmp.{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let mut file = BufWriter::new(File::create(&tmp)?);
        file.write_all(&[0; HEADER])?;
        Ok(Self {
            file,
            tmp,
            dst: dst.to_owned(),
            pos: HEADER as u64,
            w: w as usize,
            h: h as usize,
            frames: Vec::new(),
            packed: Vec::new(),
            comp: Vec::new(),
            done: false,
        })
    }

    /// Adds a frame: the pixels of `frame` inside each rect.
    pub fn add_frame(&mut self, delay_ms: u32, rects: &[Rect], frame: &[u32]) -> io::Result<()> {
        assert_eq!(frame.len(), self.w * self.h);
        let mut index = Vec::with_capacity(rects.len());
        for r in rects {
            let (x, y, w, h) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
            let input = if w == self.w {
                bytes(&frame[y * self.w..(y + h) * self.w])
            } else {
                self.packed.clear();
                for row in y..y + h {
                    let a = row * self.w + x;
                    self.packed.extend_from_slice(bytes(&frame[a..a + w]));
                }
                &self.packed
            };
            self.comp
                .resize(lz4_flex::block::get_maximum_output_size(input.len()), 0);
            let size =
                lz4_flex::block::compress_into(input, &mut self.comp).map_err(io::Error::other)?;
            let size32 = u32::try_from(size).map_err(io::Error::other)?;
            self.file.write_all(&self.comp[..size])?;
            index.push(IndexRect {
                rect: *r,
                offset: self.pos,
                size: size32,
            });
            self.pos += size as u64;
            let pad = self.pos.next_multiple_of(ALIGN) - self.pos;
            self.file.write_all(&[0; ALIGN as usize][..pad as usize])?;
            self.pos += pad;
        }
        self.frames.push((delay_ms, index));
        Ok(())
    }

    /// Adds time to the last frame. Used when the next frame is identical.
    pub fn add_delay(&mut self, delay_ms: u32) {
        if let Some((d, _)) = self.frames.last_mut() {
            *d = d.saturating_add(delay_ms);
        }
    }

    /// Writes the index and the header, syncs, and renames to the final name.
    pub fn finish(mut self) -> io::Result<()> {
        let index_offset = self.pos;
        for (delay, rects) in &self.frames {
            self.file.write_all(&delay.to_le_bytes())?;
            self.file.write_all(&(rects.len() as u32).to_le_bytes())?;
            for r in rects {
                for v in [r.rect.x, r.rect.y, r.rect.w, r.rect.h] {
                    self.file.write_all(&v.to_le_bytes())?;
                }
                self.file.write_all(&r.offset.to_le_bytes())?;
                self.file.write_all(&r.size.to_le_bytes())?;
            }
        }
        let mut header = Vec::with_capacity(HEADER);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&(self.w as u32).to_le_bytes());
        header.extend_from_slice(&(self.h as u32).to_le_bytes());
        header.extend_from_slice(&(self.frames.len() as u32).to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&index_offset.to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        std::fs::rename(&self.tmp, &self.dst)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.done {
            _ = std::fs::remove_file(&self.tmp);
        }
    }
}

struct Frame {
    delay: u32,
    first: u32,
    count: u32,
}

struct Entry {
    rect: Rect,
    offset: usize,
    size: usize,
}

/// A validated, memory-mapped cache file.
pub struct Cache {
    ptr: *mut core::ffi::c_void,
    len: usize,
    pub w: u32,
    pub h: u32,
    frames: Vec<Frame>,
    entries: Vec<Entry>,
    /// byte size of the largest rect that is not full-width
    pub scratch_len: usize,
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let (head, rest) = self.0.split_first_chunk::<N>().ok_or("truncated file")?;
        self.0 = rest;
        Ok(*head)
    }
    fn u16(&mut self) -> Result<u16, String> {
        self.take().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Result<u32, String> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Result<u64, String> {
        self.take().map(u64::from_le_bytes)
    }
}

impl Cache {
    pub fn open(path: &Path) -> Result<Self, String> {
        use rustix::mm::{MapFlags, ProtFlags, mmap};
        let file = File::open(path).map_err(|e| e.to_string())?;
        let len = file.metadata().map_err(|e| e.to_string())?.len() as usize;
        if len < HEADER {
            return Err("truncated file".into());
        }
        // SAFETY: a new private read-only mapping; it is unmapped in `drop`
        let ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                &file,
                0,
            )
        }
        .map_err(|e| e.to_string())?;
        let mut cache = Self {
            ptr,
            len,
            w: 0,
            h: 0,
            frames: Vec::new(),
            entries: Vec::new(),
            scratch_len: 0,
        };
        cache.parse()?;
        Ok(cache)
    }

    fn data(&self) -> &[u8] {
        // SAFETY: the mapping is valid for `len` bytes until `drop`
        unsafe { core::slice::from_raw_parts(self.ptr.cast(), self.len) }
    }

    fn parse(&mut self) -> Result<(), String> {
        let data = self.data();
        let mut c = Cursor(data);
        if c.take::<4>()? != MAGIC {
            return Err("bad magic".into());
        }
        if c.u16()? != VERSION {
            return Err("bad version".into());
        }
        let _flags = c.u16()?;
        let (w, h) = (c.u32()?, c.u32()?);
        let frame_count = c.u32()? as usize;
        let _reserved = c.u32()?;
        let index_offset = c.u64()?;
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return Err("bad size".into());
        }
        if index_offset < HEADER as u64 || index_offset > data.len() as u64 {
            return Err("truncated file".into());
        }
        let index_offset = index_offset as usize;
        // each index entry has 8 bytes or more
        if frame_count == 0 || frame_count > (data.len() - index_offset) / 8 {
            return Err("truncated file".into());
        }

        let mut c = Cursor(&data[index_offset..]);
        let mut frames = Vec::with_capacity(frame_count);
        let mut entries = Vec::new();
        let mut scratch_len = 0;
        for i in 0..frame_count {
            let delay = c.u32()?;
            let count = c.u32()?;
            if count == 0 || count as usize > MAX_RECTS {
                return Err("bad rect count".into());
            }
            frames.push(Frame {
                delay,
                first: entries.len() as u32,
                count,
            });
            for _ in 0..count {
                let rect = Rect {
                    x: c.u16()?,
                    y: c.u16()?,
                    w: c.u16()?,
                    h: c.u16()?,
                };
                let offset = c.u64()?;
                let size = c.u32()? as u64;
                if rect.w == 0
                    || rect.h == 0
                    || rect.x as u32 + rect.w as u32 > w
                    || rect.y as u32 + rect.h as u32 > h
                {
                    return Err("bad rect".into());
                }
                if offset < HEADER as u64 || offset + size > index_offset as u64 {
                    return Err("bad payload offset".into());
                }
                if i == 0 && (count != 1 || rect != Rect::new(0, 0, w, h)) {
                    return Err("frame 0 is not a full frame".into());
                }
                if rect.w as u32 != w {
                    scratch_len = scratch_len.max(rect.area() * 4);
                }
                entries.push(Entry {
                    rect,
                    offset: offset as usize,
                    size: size as usize,
                });
            }
        }
        self.w = w;
        self.h = h;
        self.frames = frames;
        self.entries = entries;
        self.scratch_len = scratch_len;
        Ok(())
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn delay_ms(&self, frame: usize) -> u32 {
        self.frames[frame].delay
    }

    pub fn rects(&self, frame: usize) -> impl Iterator<Item = Rect> + '_ {
        let f = &self.frames[frame];
        self.entries[f.first as usize..(f.first + f.count) as usize]
            .iter()
            .map(|e| e.rect)
    }

    /// Decompresses the rects of `frame` into `canvas` (stored size).
    ///
    /// `scratch` must have `scratch_len` bytes. A full-width rect goes straight
    /// into the canvas. There is no allocation.
    pub fn apply(
        &self,
        frame: usize,
        canvas: &mut [u32],
        scratch: &mut [u8],
    ) -> Result<(), String> {
        let w = self.w as usize;
        if canvas.len() != w * self.h as usize {
            return Err("bad canvas size".into());
        }
        let f = &self.frames[frame];
        let data = self.data();
        for e in &self.entries[f.first as usize..(f.first + f.count) as usize] {
            let src = &data[e.offset..e.offset + e.size];
            let r = e.rect;
            let (x, y, rw, rh) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
            let bad = |e: lz4_flex::block::DecompressError| format!("corrupt cache file: {e}");
            if rw == w {
                let dst = bytes_mut(&mut canvas[y * w..(y + rh) * w]);
                if lz4_flex::block::decompress_into(src, dst).map_err(bad)? != dst.len() {
                    return Err("corrupt cache file: short rect".into());
                }
            } else {
                let dst = scratch
                    .get_mut(..rw * rh * 4)
                    .ok_or("scratch buffer too small")?;
                if lz4_flex::block::decompress_into(src, dst).map_err(bad)? != dst.len() {
                    return Err("corrupt cache file: short rect".into());
                }
                for (i, row) in dst.chunks_exact(rw * 4).enumerate() {
                    let a = (y + i) * w + x;
                    bytes_mut(&mut canvas[a..a + rw]).copy_from_slice(row);
                }
            }
        }
        Ok(())
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        // SAFETY: the mapping came from `mmap` with this length; no slice outlives self
        _ = unsafe { rustix::mm::munmap(self.ptr, self.len) };
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::diff::Differ;
    use crate::pixelart::tests::Rng;

    pub fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("waypixel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Frames with small random changes.
    pub fn synthetic(w: usize, h: usize, count: usize, seed: u64) -> Vec<Vec<u32>> {
        let mut rng = Rng(seed);
        let mut frames = vec![(0..w * h).map(|_| rng.below(5)).collect::<Vec<u32>>()];
        for i in 1..count {
            let mut f = frames[i - 1].clone();
            let n = if i % 4 == 0 {
                w * h
            } else {
                1 + rng.below(12) as usize
            };
            for _ in 0..n {
                f[rng.below((w * h) as u32) as usize] = rng.next();
            }
            frames.push(f);
        }
        frames
    }

    /// Writes frames to a cache file the same way as the decode child.
    pub fn write(path: &Path, frames: &[Vec<u32>], w: usize, h: usize, delay: u32) {
        let mut writer = Writer::create(path, w as u32, h as u32).unwrap();
        let mut differ = Differ::default();
        let full = [Rect::new(0, 0, w as u32, h as u32)];
        writer.add_frame(delay, &full, &frames[0]).unwrap();
        for pair in frames.windows(2) {
            let rects = differ.diff(&pair[0], &pair[1], w, h);
            if rects.is_empty() {
                writer.add_delay(delay);
            } else {
                writer.add_frame(delay, &rects, &pair[1]).unwrap();
            }
        }
        writer.finish().unwrap();
    }

    #[test]
    fn round_trip_is_bit_exact() {
        for (w, h, seed) in [(64, 40, 1), (37, 23, 2), (8, 8, 3), (130, 3, 4)] {
            let frames = synthetic(w, h, 12, seed);
            let path = temp(&format!("rt-{seed}.wpx"));
            write(&path, &frames, w, h, 50);
            let cache = Cache::open(&path).unwrap();
            assert_eq!((cache.w as usize, cache.h as usize), (w, h));
            assert_eq!(cache.frame_count(), frames.len());
            let mut canvas = vec![0u32; w * h];
            let mut scratch = vec![0u8; cache.scratch_len];
            // two loops: frame 0 must restore the full frame
            for _ in 0..2 {
                for (i, f) in frames.iter().enumerate() {
                    cache.apply(i, &mut canvas, &mut scratch).unwrap();
                    assert_eq!(&canvas, f, "frame {i}");
                    assert_eq!(cache.delay_ms(i), 50);
                }
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn identical_frames_merge() {
        let a = vec![1u32; 16 * 16];
        let mut b = a.clone();
        b[5] = 2;
        let path = temp("merge.wpx");
        write(&path, &[a.clone(), a.clone(), b.clone(), b], 16, 16, 30);
        let cache = Cache::open(&path).unwrap();
        assert_eq!(cache.frame_count(), 2);
        assert_eq!((cache.delay_ms(0), cache.delay_ms(1)), (60, 60));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bad_files_are_rejected() {
        let frames = synthetic(32, 32, 4, 9);
        let path = temp("bad.wpx");
        write(&path, &frames, 32, 32, 50);
        let good = std::fs::read(&path).unwrap();
        assert!(Cache::open(&path).is_ok());

        let mut bad = good.clone();
        bad[0] = b'Q';
        std::fs::write(&path, &bad).unwrap();
        assert_eq!(Cache::open(&path).err().unwrap(), "bad magic");

        let mut bad = good.clone();
        bad[4] = 9;
        std::fs::write(&path, &bad).unwrap();
        assert_eq!(Cache::open(&path).err().unwrap(), "bad version");

        for cut in [0, 10, HEADER, good.len() / 2, good.len() - 1] {
            std::fs::write(&path, &good[..cut]).unwrap();
            assert!(Cache::open(&path).is_err(), "cut at {cut}");
        }
        std::fs::remove_file(path).unwrap();
    }
}
