//! One memfd shm pool with buffers of one size.

use rustix::fd::OwnedFd;
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
use rustix::io;
use rustix::mm::{MapFlags, MremapFlags, ProtFlags, mmap, mremap, munmap};
use waybackend::types::ObjectId;

use super::wayland::{Obj, Wl, req};

/// A compositor that never releases buffers does not get more than this.
const MAX_BUFFERS: usize = 3;

pub struct Buf {
    pub id: ObjectId,
    /// true from the commit until the compositor sends `release`
    pub busy: bool,
    /// the frame (player `seq`) that the pixels show
    pub holds: Option<u64>,
}

pub struct Pool {
    id: ObjectId,
    fd: OwnedFd,
    ptr: *mut core::ffi::c_void,
    map_len: usize,
    pub w: u32,
    pub h: u32,
    pub bufs: Vec<Buf>,
}

impl Pool {
    /// Creates a pool with one buffer of `w` × `h` pixels (XRGB8888).
    pub fn new(wl: &mut Wl, w: u32, h: u32) -> io::Result<Self> {
        let len = w as usize * h as usize * 4;
        if len == 0 || len > i32::MAX as usize {
            return Err(io::Errno::INVAL);
        }
        let fd = memfd_create(c"pixelwp", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
        ftruncate(&fd, len as u64)?;
        // The seal lets the compositor skip its SIGBUS protection. An error is not important.
        _ = fcntl_add_seals(&fd, SealFlags::SHRINK | SealFlags::SEAL);
        // SAFETY: a new shared mapping; it is unmapped in `destroy`
        let ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )?
        };
        let id = wl.objman.create(Obj::ShmPool);
        req!(wl, wl_shm::req::create_pool(wl.shm, id, &fd, len as i32));
        let mut pool = Self {
            id,
            fd,
            ptr,
            map_len: len,
            w,
            h,
            bufs: Vec::with_capacity(MAX_BUFFERS),
        };
        pool.add_buffer(wl);
        Ok(pool)
    }

    fn buf_len(&self) -> usize {
        self.w as usize * self.h as usize * 4
    }

    fn add_buffer(&mut self, wl: &mut Wl) {
        let id = wl.objman.create(Obj::Buffer);
        let offset = (self.bufs.len() * self.buf_len()) as i32;
        let (w, h) = (self.w as i32, self.h as i32);
        req!(
            wl,
            wl_shm_pool::req::create_buffer(
                self.id,
                id,
                offset,
                w,
                h,
                w * 4,
                super::wayland::wl_shm::Format::xrgb8888
            )
        );
        self.bufs.push(Buf {
            id,
            busy: false,
            holds: None,
        });
    }

    /// Returns the index of a free buffer. Adds a buffer only when the compositor
    /// holds all the existing ones. Returns `None` at the buffer limit.
    pub fn acquire(&mut self, wl: &mut Wl) -> Option<usize> {
        if let Some(i) = self.bufs.iter().position(|b| !b.busy) {
            return Some(i);
        }
        if self.bufs.len() == MAX_BUFFERS {
            return None;
        }
        let new_len = (self.bufs.len() + 1) * self.buf_len();
        if new_len > i32::MAX as usize {
            return None;
        }
        ftruncate(&self.fd, new_len as u64).ok()?;
        // SAFETY: the old range is our mapping; no slice into it is alive
        self.ptr = unsafe { mremap(self.ptr, self.map_len, new_len, MremapFlags::MAYMOVE).ok()? };
        self.map_len = new_len;
        req!(wl, wl_shm_pool::req::resize(self.id, new_len as i32));
        self.add_buffer(wl);
        Some(self.bufs.len() - 1)
    }

    pub fn pixels(&mut self, index: usize) -> &mut [u32] {
        let n = self.w as usize * self.h as usize;
        assert!(index < self.bufs.len());
        // SAFETY: the mapping is page-aligned and has `bufs.len()` buffers of n pixels
        unsafe { core::slice::from_raw_parts_mut(self.ptr.cast::<u32>().add(index * n), n) }
    }

    /// Handles `wl_buffer.release`. Returns false when the buffer is not from this pool.
    pub fn release(&mut self, id: ObjectId) -> bool {
        match self.bufs.iter_mut().find(|b| b.id == id) {
            Some(buf) => {
                buf.busy = false;
                true
            }
            None => false,
        }
    }

    pub fn all_free(&self) -> bool {
        self.bufs.iter().all(|b| !b.busy)
    }

    /// Destroys the buffers and the pool, and unmaps the memory.
    ///
    /// This is permitted while the compositor holds a buffer, because the storage
    /// is not used again.
    pub fn destroy(self, wl: &mut Wl) {
        for buf in &self.bufs {
            req!(wl, wl_buffer::req::destroy(buf.id));
        }
        req!(wl, wl_shm_pool::req::destroy(self.id));
        // The fd is in the message buffer until the flush, if the pool is new.
        if let Err(e) = wl.flush() {
            wl.error.get_or_insert(e);
        }
        // SAFETY: the mapping came from mmap/mremap with this length
        _ = unsafe { munmap(self.ptr, self.map_len) };
    }
}
