//! End-to-end tests of the daemon against a small mock Wayland compositor.
//!
//! The mock speaks the wire protocol by hand. Like a real compositor, it copies
//! only the damaged rects of a committed buffer into its texture. So a texture
//! that equals a reference render also proves that the damage is correct.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustix::fd::OwnedFd;

const BIN: &str = env!("CARGO_BIN_EXE_waypixel");

// ---- mock compositor ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Display,
    Registry,
    Compositor,
    Shm,
    Pool,
    Buffer,
    Surface,
    Region,
    Output,
    LayerShell,
    LayerSurface(u32),
    Viewporter,
    Viewport(u32),
    FracManager,
    Frac,
}

struct OutputCfg {
    name: &'static str,
    logical: (u32, u32),
    /// fractional scale in 1/120
    scale120: u32,
}

struct Pool {
    fd: OwnedFd,
    len: usize,
}

#[derive(Clone, Copy)]
struct Buffer {
    pool: u32,
    offset: usize,
    w: usize,
    h: usize,
}

#[derive(Default)]
struct Surface {
    output: usize,
    layer_surface: u32,
    configured_serial: u32,
    acked: bool,
    pending_buffer: Option<u32>,
    pending_damage: Vec<[i32; 4]>,
    pending_frames: Vec<u32>,
    pending_src: Option<[i32; 4]>,
    pending_dst: Option<(i32, i32)>,
    /// what the compositor shows
    texture: Vec<u32>,
    tex_size: (usize, usize),
    src: Option<[i32; 4]>,
    dst: Option<(i32, i32)>,
    /// commits with a buffer
    commits: u32,
    last_damage: Vec<[i32; 4]>,
    held_frames: Vec<u32>,
    /// the buffer that the compositor holds (not released)
    held_buffer: Option<u32>,
}

struct Mock {
    outputs: Vec<OutputCfg>,
    stream: Option<UnixStream>,
    objects: HashMap<u32, Kind>,
    pools: HashMap<u32, Pool>,
    buffers: HashMap<u32, Buffer>,
    surfaces: HashMap<u32, Surface>,
    /// when true, frame callbacks are not sent: the surfaces are hidden
    hold_frames: bool,
    /// when false, the compositor keeps each buffer until the next commit
    release_at_commit: bool,
    serial: u32,
    errors: Vec<String>,
    /// wl_buffer objects that exist
    live_buffers: usize,
}

struct Msg<'a> {
    data: &'a [u8],
}

impl Msg<'_> {
    fn u32(&mut self) -> u32 {
        let (head, rest) = self.data.split_at(4);
        self.data = rest;
        u32::from_ne_bytes(head.try_into().unwrap())
    }
    fn i32(&mut self) -> i32 {
        self.u32() as i32
    }
    fn string(&mut self) -> String {
        let len = self.u32() as usize;
        let padded = len.next_multiple_of(4);
        let (head, rest) = self.data.split_at(padded);
        self.data = rest;
        String::from_utf8(head[..len - 1].to_vec()).unwrap()
    }
}

enum Arg<'a> {
    U(u32),
    S(&'a str),
}

impl Mock {
    fn send(&mut self, obj: u32, op: u16, args: &[Arg]) {
        let mut body = Vec::new();
        for arg in args {
            match arg {
                Arg::U(v) => body.extend_from_slice(&v.to_ne_bytes()),
                Arg::S(s) => {
                    let len = s.len() + 1;
                    body.extend_from_slice(&(len as u32).to_ne_bytes());
                    body.extend_from_slice(s.as_bytes());
                    body.resize(body.len() + len.next_multiple_of(4) - s.len(), 0);
                }
            }
        }
        let mut msg = Vec::new();
        msg.extend_from_slice(&obj.to_ne_bytes());
        msg.extend_from_slice(&(((body.len() as u32 + 8) << 16) | op as u32).to_ne_bytes());
        msg.extend_from_slice(&body);
        if let Some(stream) = &mut self.stream {
            _ = stream.write_all(&msg);
        }
    }

    fn delete(&mut self, id: u32) {
        self.objects.remove(&id);
        self.send(1, 1, &[Arg::U(id)]);
    }

    fn error(&mut self, text: String) {
        eprintln!("mock: {text}");
        self.errors.push(text);
    }

    fn reset_connection(&mut self, stream: UnixStream) {
        self.stream = Some(stream);
        self.objects = HashMap::from([(1, Kind::Display)]);
        self.pools.clear();
        self.buffers.clear();
        self.surfaces.clear();
        self.live_buffers = 0;
    }

    fn surface_of_output(&self, output: usize) -> Option<&Surface> {
        self.surfaces
            .values()
            .find(|s| s.output == output && s.layer_surface != 0)
    }

    fn configure(&mut self, output: usize, logical: (u32, u32)) {
        self.outputs[output].logical = logical;
        self.serial += 1;
        let serial = self.serial;
        let Some((_, s)) = self
            .surfaces
            .iter_mut()
            .find(|(_, s)| s.output == output && s.layer_surface != 0)
        else {
            return;
        };
        s.configured_serial = serial;
        s.acked = false;
        let layer = s.layer_surface;
        self.send(
            layer,
            0,
            &[Arg::U(serial), Arg::U(logical.0), Arg::U(logical.1)],
        );
    }

    /// Sends the frame callbacks that were held.
    fn show(&mut self) {
        self.hold_frames = false;
        let held: Vec<u32> = self
            .surfaces
            .values_mut()
            .flat_map(|s| s.held_frames.drain(..))
            .collect();
        for cb in held {
            self.send(cb, 0, &[Arg::U(0)]);
            self.delete(cb);
        }
    }

    fn read_buffer(&self, id: u32) -> Option<(Vec<u32>, usize, usize)> {
        use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
        let b = *self.buffers.get(&id)?;
        let pool = self.pools.get(&b.pool)?;
        if b.offset + b.w * b.h * 4 > pool.len {
            return None;
        }
        // SAFETY: a read-only mapping of the pool, unmapped below
        unsafe {
            let ptr = mmap(
                core::ptr::null_mut(),
                pool.len,
                ProtFlags::READ,
                MapFlags::SHARED,
                &pool.fd,
                0,
            )
            .ok()?;
            let px = core::slice::from_raw_parts(
                ptr.cast::<u8>().add(b.offset).cast::<u32>(),
                b.w * b.h,
            )
            .to_vec();
            munmap(ptr, pool.len).ok()?;
            Some((px, b.w, b.h))
        }
    }

    fn commit(&mut self, id: u32) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if s.layer_surface != 0 && s.configured_serial == 0 {
            // first commit: send the first configure
            let output = s.output;
            if s.pending_buffer.is_some() {
                self.error("buffer attached before the first configure".into());
            }
            let logical = self.outputs[output].logical;
            self.configure(output, logical);
            return;
        }
        let frames = std::mem::take(&mut s.pending_frames);
        if let Some(src) = s.pending_src.take() {
            s.src = (src[0] != -256).then_some(src);
        }
        if let Some(dst) = s.pending_dst.take() {
            s.dst = Some(dst);
        }
        let damage = std::mem::take(&mut s.pending_damage);
        let buffer = s.pending_buffer.take();
        let acked = s.acked;
        if let Some(buffer) = buffer {
            if !acked {
                self.error("commit with a buffer before ack_configure".into());
            }
            if self
                .surfaces
                .values()
                .any(|s| s.held_buffer == Some(buffer))
            {
                self.error(format!(
                    "buffer {buffer} committed while the compositor holds it"
                ));
            }
            let Some((px, w, h)) = self.read_buffer(buffer) else {
                self.error(format!("cannot read buffer {buffer}"));
                return;
            };
            let s = self.surfaces.get_mut(&id).unwrap();
            if s.tex_size != (w, h) {
                s.texture = px;
                s.tex_size = (w, h);
            } else {
                for [x, y, dw, dh] in &damage {
                    let (x0, y0) = ((*x).max(0) as usize, (*y).max(0) as usize);
                    let x1 = ((*x as i64 + *dw as i64).max(0) as usize).min(w);
                    let y1 = ((*y as i64 + *dh as i64).max(0) as usize).min(h);
                    for row in y0..y1 {
                        s.texture[row * w + x0..row * w + x1]
                            .copy_from_slice(&px[row * w + x0..row * w + x1]);
                    }
                }
            }
            s.commits += 1;
            s.last_damage = damage;
            let (src, tex_size) = (s.src, s.tex_size);
            if let Some([x, y, cw, ch]) = src
                && (x < 0
                    || y < 0
                    || cw <= 0
                    || ch <= 0
                    || x + cw > tex_size.0 as i32 * 256
                    || y + ch > tex_size.1 as i32 * 256)
            {
                self.error(format!(
                    "viewport source {src:?} is outside the buffer {tex_size:?}"
                ));
            }
            // the buffer that was held until this commit
            let old = self.surfaces.get_mut(&id).unwrap().held_buffer.take();
            if let Some(old) = old {
                self.release(old);
            }
            if self.release_at_commit {
                self.release(buffer);
            } else {
                self.surfaces.get_mut(&id).unwrap().held_buffer = Some(buffer);
            }
        }
        for cb in frames {
            if self.hold_frames {
                self.surfaces.get_mut(&id).unwrap().held_frames.push(cb);
            } else {
                self.send(cb, 0, &[Arg::U(0)]);
                self.delete(cb);
            }
        }
    }

    fn release(&mut self, buffer: u32) {
        if self.objects.get(&buffer) == Some(&Kind::Buffer) {
            self.send(buffer, 0, &[]);
        }
    }

    fn handle(&mut self, obj: u32, op: u16, mut m: Msg, fds: &mut VecDeque<OwnedFd>) {
        let Some(kind) = self.objects.get(&obj).copied() else {
            self.error(format!("request for unknown object {obj}"));
            return;
        };
        match (kind, op) {
            (Kind::Display, 0) => {
                let cb = m.u32();
                self.send(cb, 0, &[Arg::U(0)]);
                self.send(1, 1, &[Arg::U(cb)]);
            }
            (Kind::Display, 1) => {
                let registry = m.u32();
                self.objects.insert(registry, Kind::Registry);
                let globals = [
                    (1, "wl_compositor", 6),
                    (2, "wl_shm", 1),
                    (3, "zwlr_layer_shell_v1", 4),
                    (4, "wp_viewporter", 1),
                    (5, "wp_fractional_scale_manager_v1", 1),
                    (6, "wl_seat", 9),
                ];
                for (name, interface, version) in globals {
                    self.send(
                        registry,
                        0,
                        &[Arg::U(name), Arg::S(interface), Arg::U(version)],
                    );
                }
                for i in 0..self.outputs.len() {
                    self.send(
                        registry,
                        0,
                        &[Arg::U(10 + i as u32), Arg::S("wl_output"), Arg::U(4)],
                    );
                }
            }
            (Kind::Registry, 0) => {
                let name = m.u32();
                let interface = m.string();
                let version = m.u32();
                let id = m.u32();
                let kind = match interface.as_str() {
                    "wl_compositor" => Kind::Compositor,
                    "wl_shm" => Kind::Shm,
                    "zwlr_layer_shell_v1" => Kind::LayerShell,
                    "wp_viewporter" => Kind::Viewporter,
                    "wp_fractional_scale_manager_v1" => Kind::FracManager,
                    "wl_output" => Kind::Output,
                    other => {
                        self.error(format!("bind of {other}"));
                        return;
                    }
                };
                self.objects.insert(id, kind);
                if kind == Kind::Output {
                    if version != 4 {
                        self.error(format!("wl_output bound with version {version}"));
                    }
                    let cfg_name = self.outputs[name as usize - 10].name;
                    self.send(id, 3, &[Arg::U(1)]);
                    self.send(id, 4, &[Arg::S(cfg_name)]);
                    self.send(id, 2, &[]);
                    // remember which output this object is
                    self.surfaces.entry(id).or_default().output = name as usize - 10;
                }
            }
            (Kind::Output, 0) => {
                self.surfaces.remove(&obj);
                self.delete(obj);
            }
            (Kind::Compositor, 0) => {
                let id = m.u32();
                self.objects.insert(id, Kind::Surface);
                self.surfaces.insert(id, Surface::default());
            }
            (Kind::Compositor, 1) => {
                let id = m.u32();
                self.objects.insert(id, Kind::Region);
            }
            (Kind::Region, 0) => self.delete(obj),
            (Kind::Region, 1 | 2) => {}
            (Kind::Surface, 0) => {
                self.surfaces.remove(&obj);
                self.delete(obj);
            }
            (Kind::Surface, 1) => {
                let buffer = m.u32();
                let s = self.surfaces.get_mut(&obj).unwrap();
                s.pending_buffer = (buffer != 0).then_some(buffer);
                if buffer != 0 && !self.buffers.contains_key(&buffer) {
                    self.error(format!("attach of unknown buffer {buffer}"));
                }
            }
            (Kind::Surface, 3) => {
                let cb = m.u32();
                self.surfaces.get_mut(&obj).unwrap().pending_frames.push(cb);
            }
            (Kind::Surface, 5) => {}
            (Kind::Surface, 6) => self.commit(obj),
            (Kind::Surface, 9) => {
                let rect = [m.i32(), m.i32(), m.i32(), m.i32()];
                self.surfaces
                    .get_mut(&obj)
                    .unwrap()
                    .pending_damage
                    .push(rect);
            }
            (Kind::LayerShell, 0) => {
                let id = m.u32();
                let surface = m.u32();
                let output = m.u32();
                let layer = m.u32();
                let namespace = m.string();
                if layer != 0 || namespace != "waypixel" {
                    self.error(format!("layer {layer}, namespace {namespace}"));
                }
                let index = self
                    .surfaces
                    .get(&output)
                    .map(|o| o.output)
                    .unwrap_or(usize::MAX);
                self.objects.insert(id, Kind::LayerSurface(surface));
                let s = self.surfaces.get_mut(&surface).unwrap();
                s.layer_surface = id;
                s.output = index;
            }
            (Kind::LayerSurface(_), 0..=4) => {}
            (Kind::LayerSurface(surface), 6) => {
                let serial = m.u32();
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.acked = serial == s.configured_serial;
                }
            }
            (Kind::LayerSurface(_), 7) => self.delete(obj),
            (Kind::Viewporter, 1) => {
                let id = m.u32();
                let surface = m.u32();
                self.objects.insert(id, Kind::Viewport(surface));
            }
            (Kind::Viewport(_), 0) => self.delete(obj),
            (Kind::Viewport(surface), 1) => {
                let src = [m.i32(), m.i32(), m.i32(), m.i32()];
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.pending_src = Some(src);
                }
            }
            (Kind::Viewport(surface), 2) => {
                let dst = (m.i32(), m.i32());
                if let Some(s) = self.surfaces.get_mut(&surface) {
                    s.pending_dst = Some(dst);
                }
            }
            (Kind::FracManager, 1) => {
                let id = m.u32();
                let surface = m.u32();
                self.objects.insert(id, Kind::Frac);
                let output = self.surfaces[&surface].output;
                let scale = self.outputs.get(output).map_or(120, |o| o.scale120);
                self.send(id, 0, &[Arg::U(scale)]);
            }
            (Kind::Frac, 0) => self.delete(obj),
            (Kind::Shm, 0) => {
                let id = m.u32();
                let len = m.i32() as usize;
                let fd = fds.pop_front().expect("create_pool without fd");
                self.objects.insert(id, Kind::Pool);
                self.pools.insert(id, Pool { fd, len });
            }
            (Kind::Pool, 0) => {
                let id = m.u32();
                let (offset, w, h, stride, format) = (m.i32(), m.i32(), m.i32(), m.i32(), m.u32());
                if stride != w * 4 || format != 1 {
                    self.error(format!(
                        "buffer stride {stride}, width {w}, format {format}"
                    ));
                }
                self.objects.insert(id, Kind::Buffer);
                self.live_buffers += 1;
                let buffer = Buffer {
                    pool: obj,
                    offset: offset as usize,
                    w: w as usize,
                    h: h as usize,
                };
                self.buffers.insert(id, buffer);
            }
            (Kind::Pool, 1) => {
                // the buffers of a destroyed pool stay valid in a real compositor;
                // the daemon destroys them first, so the mock can drop the pool
                self.pools.remove(&obj);
                self.delete(obj);
            }
            (Kind::Pool, 2) => {
                let len = m.i32() as usize;
                self.pools.get_mut(&obj).unwrap().len = len;
            }
            (Kind::Buffer, 0) => {
                self.buffers.remove(&obj);
                for s in self.surfaces.values_mut() {
                    if s.held_buffer == Some(obj) {
                        s.held_buffer = None;
                    }
                }
                self.live_buffers -= 1;
                self.delete(obj);
            }
            other => self.error(format!("request {other:?} is not expected")),
        }
    }
}

fn serve(listener: UnixListener, mock: Arc<Mutex<Mock>>) {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        mock.lock()
            .unwrap()
            .reset_connection(stream.try_clone().unwrap());
        let mut data: Vec<u8> = Vec::new();
        let mut fds: VecDeque<OwnedFd> = VecDeque::new();
        loop {
            let mut chunk = [0u8; 4096];
            let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(8))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let mut iov = [std::io::IoSliceMut::new(&mut chunk)];
            let n = match recvmsg(&stream, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC) {
                Ok(msg) if msg.bytes > 0 => msg.bytes,
                _ => break,
            };
            for msg in control.drain() {
                if let RecvAncillaryMessage::ScmRights(rights) = msg {
                    fds.extend(rights);
                }
            }
            data.extend_from_slice(&chunk[..n]);
            let mut mock = mock.lock().unwrap();
            while data.len() >= 8 {
                let obj = u32::from_ne_bytes(data[..4].try_into().unwrap());
                let word = u32::from_ne_bytes(data[4..8].try_into().unwrap());
                let (len, op) = ((word >> 16) as usize, word as u16);
                if data.len() < len {
                    break;
                }
                mock.handle(
                    obj,
                    op,
                    Msg {
                        data: &data[8..len],
                    },
                    &mut fds,
                );
                data.drain(..len);
            }
        }
        mock.lock().unwrap().stream = None;
    }
}

// ---- test environment -----------------------------------------------------------

struct Env {
    dir: PathBuf,
    mock: Arc<Mutex<Mock>>,
    daemon: Option<Child>,
}

impl Env {
    fn new(name: &str, outputs: Vec<OutputCfg>) -> Self {
        // a short path: a socket path has a limit of 108 bytes
        let dir = PathBuf::from(format!("/tmp/wpx-{}-{name}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("rt")).unwrap();
        let listener = UnixListener::bind(dir.join("rt/wayland-test")).unwrap();
        let mock = Arc::new(Mutex::new(Mock {
            outputs,
            stream: None,
            objects: HashMap::new(),
            pools: HashMap::new(),
            buffers: HashMap::new(),
            surfaces: HashMap::new(),
            hold_frames: false,
            release_at_commit: true,
            serial: 0,
            errors: Vec::new(),
            live_buffers: 0,
        }));
        let shared = mock.clone();
        std::thread::spawn(move || serve(listener, shared));
        Self {
            dir,
            mock,
            daemon: None,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env("XDG_RUNTIME_DIR", self.dir.join("rt"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("WAYLAND_DISPLAY", "wayland-test")
            // a `notify-send` of the test can be in `bin`; the real one is never used
            .env("PATH", self.dir.join("bin"))
            .env_remove("WAYLAND_SOCKET");
        cmd
    }

    fn start_daemon(&mut self) {
        self.daemon = Some(self.command().arg("daemon").spawn().unwrap());
        let socket = self.dir.join("rt/waypixel-wayland-test.sock");
        wait("the daemon socket", || UnixStream::connect(&socket).is_ok());
    }

    fn pid(&self) -> u32 {
        self.daemon.as_ref().unwrap().id()
    }

    /// Runs a client command. Returns (exit code, stdout, stderr).
    fn cli(&self, args: &[&str]) -> (i32, String, String) {
        // Without `--fx`, a test gets no transition: the reply comes with the final pixels.
        let set = matches!(args.first(), Some(&"img" | &"clear"));
        let no_fx: &[&str] = if set && !args.contains(&"--fx") && args.len() > 1 {
            &["--fx", "disabled"]
        } else {
            &[]
        };
        let out = self
            .command()
            .args(args)
            .args(no_fx)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (
            out.status.code().unwrap(),
            text(&out.stdout),
            text(&out.stderr),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, stdout, stderr) = self.cli(args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        stdout
    }

    fn mock(&self) -> MutexGuard<'_, Mock> {
        self.mock.lock().unwrap()
    }

    fn texture(&self, output: usize) -> (Vec<u32>, (usize, usize)) {
        let mock = self.mock();
        let s = mock
            .surface_of_output(output)
            .expect("no surface for the output");
        (s.texture.clone(), s.tex_size)
    }

    fn commits(&self, output: usize) -> u32 {
        self.mock()
            .surface_of_output(output)
            .map_or(0, |s| s.commits)
    }

    fn cache_files(&self) -> Vec<String> {
        let Ok(dir) = std::fs::read_dir(self.dir.join("cache/waypixel")) else {
            return Vec::new();
        };
        dir.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn assert_no_errors(&self) {
        let mock = self.mock();
        assert!(mock.errors.is_empty(), "mock errors: {:?}", mock.errors);
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            _ = daemon.kill();
            _ = daemon.wait();
        }
        _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timeout: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---- fixtures -------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32 % n
    }
}

const COLOURS: [u32; 4] = [0xFF10_2030, 0xFFE0_4040, 0xFF40_E040, 0xFF40_40E0];

/// 1× frames with small changes. The last frame differs from frame 0 only a little.
fn art_frames(w: usize, h: usize, count: usize, seed: u64) -> Vec<Vec<u32>> {
    let mut rng = Rng(seed);
    let mut frames = vec![
        (0..w * h)
            .map(|_| COLOURS[rng.below(4) as usize])
            .collect::<Vec<u32>>(),
    ];
    for i in 1..count {
        // each frame changes a few pixels of frame 0, in a different place
        let mut f = frames[0].clone();
        for j in 0..4 {
            let p = (i * 97 + j * 31 + rng.below(50) as usize) % (w * h);
            let old = COLOURS.iter().position(|c| *c == f[p]).unwrap();
            f[p] = COLOURS[(old + 1) % 4];
        }
        frames.push(f);
    }
    frames
}

fn to_rgba(frame: &[u32], w: usize, h: usize, n: usize) -> image::RgbaImage {
    image::RgbaImage::from_fn((w * n) as u32, (h * n) as u32, |x, y| {
        let p = frame[y as usize / n * w + x as usize / n];
        image::Rgba([(p >> 16) as u8, (p >> 8) as u8, p as u8, 255])
    })
}

/// Writes a GIF of the frames, scaled by n.
fn write_gif(path: &Path, frames: &[Vec<u32>], w: usize, h: usize, n: usize, delay_ms: u32) {
    use image::codecs::gif::{GifEncoder, Repeat};
    let mut enc = GifEncoder::new_with_speed(std::fs::File::create(path).unwrap(), 10);
    enc.set_repeat(Repeat::Infinite).unwrap();
    for f in frames {
        let delay = image::Delay::from_numer_denom_ms(delay_ms, 1);
        enc.encode_frame(image::Frame::from_parts(to_rgba(f, w, h, n), 0, 0, delay))
            .unwrap();
    }
}

/// Reference render of 1× art on a screen: integer scale, centred, overflow cut.
fn reference(art: &[u32], (w, h): (usize, usize), (sw, sh): (usize, usize)) -> Vec<u32> {
    let k = sw.div_ceil(w).max(sh.div_ceil(h)).max(1);
    let (ox, oy) = ((k * w - sw) / 2, (k * h - sh) / 2);
    let mut out = Vec::with_capacity(sw * sh);
    for y in 0..sh {
        for x in 0..sw {
            out.push(art[(y + oy) / k * w + (x + ox) / k]);
        }
    }
    out
}

/// Returns the index of the frame that the texture of the output shows.
fn shown_frame(env: &Env, output: usize, refs: &[Vec<u32>]) -> usize {
    let (texture, _) = env.texture(output);
    refs.iter()
        .position(|r| *r == texture)
        .unwrap_or_else(|| panic!("the texture of output {output} is not equal to a frame"))
}

fn ctxt_switches(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .filter(|l| l.contains("ctxt_switches"))
        .map(|l| l.split_whitespace().last().unwrap().parse::<u64>().unwrap())
        .sum()
}

fn rss_kb(pid: u32) -> u64 {
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap();
    let line = rollup.lines().find(|l| l.starts_with("Rss:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    // utime and stime are fields 14 and 15 of the full line
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

// ---- tests ----------------------------------------------------------------------

#[test]
fn client_without_daemon() {
    let env = Env::new("nodaemon", vec![]);
    let (code, _, stderr) = env.cli(&["query"]);
    assert_eq!(code, 3, "{stderr}");
    let (code, _, _) = env.cli(&["frobnicate"]);
    assert_eq!(code, 2);
    let (code, _, _) = env.cli(&["img"]);
    assert_eq!(code, 2);
}

#[test]
fn full_session() {
    let outputs = vec![
        OutputCfg {
            name: "DP-1",
            logical: (400, 225),
            scale120: 120,
        },
        // scale 1.5: physical 384 × 216
        OutputCfg {
            name: "HDMI-A-1",
            logical: (256, 144),
            scale120: 180,
        },
    ];
    let mut env = Env::new("session", outputs);
    env.start_daemon();
    let phys = [(400usize, 225usize), (384, 216)];

    // -- start: black, 1×1 buffer, viewport at the logical size
    wait("first commits", || {
        env.commits(0) >= 1 && env.commits(1) >= 1
    });
    assert_eq!(
        env.ok(&["query"]),
        "DP-1 400x225 color #000000 static\nHDMI-A-1 384x216 color #000000 static\n"
    );
    assert_eq!(env.texture(0), (vec![0xFF00_0000], (1, 1)));
    assert_eq!(
        env.mock().surface_of_output(1).unwrap().dst,
        Some((256, 144))
    );
    // a still needs its buffer one time only
    wait("colour buffers destroyed", || env.mock().live_buffers == 0);

    // a second daemon prints a message and exits with 0
    let (code, stdout, _) = env.cli(&["daemon"]);
    assert_eq!((code, stdout.contains("already running")), (0, true));

    // -- colour
    env.ok(&["clear", "--color", "ff8800", "--output", "DP-1"]);
    assert_eq!(env.texture(0).0, [0xFFFF_8800]);
    assert_eq!(env.texture(1).0, [0xFF00_0000]);
    assert!(
        env.ok(&["query"])
            .starts_with("DP-1 400x225 color #FF8800 static\n")
    );
    let (code, _, stderr) = env.cli(&["clear", "--output", "DP-9"]);
    assert_eq!((code, stderr.trim()), (1, "waypixel: unknown output DP-9"));

    // -- pixel-art animation on all outputs
    let (w, h) = (64, 36);
    let frames = art_frames(w, h, 5, 1);
    let gif = env.dir.join("anim 1.gif");
    write_gif(&gif, &frames, w, h, 4, 40);
    let gif = gif.to_str().unwrap();
    env.ok(&["img", gif]);
    let refs: Vec<Vec<Vec<u32>>> = phys
        .iter()
        .map(|p| frames.iter().map(|f| reference(f, (w, h), *p)).collect())
        .collect();
    // the reply comes after the first frame is committed
    for o in 0..2 {
        assert_eq!(env.texture(o).1, phys[o]);
        shown_frame(&env, o, &refs[o]);
        assert_eq!(env.mock().surface_of_output(o).unwrap().src, None);
    }
    assert_eq!(
        env.ok(&["query"]),
        format!("DP-1 400x225 {gif} playing\nHDMI-A-1 384x216 {gif} playing\n")
    );
    assert_eq!(env.cache_files().len(), 1);

    // every sample is a frame, bit-exact; all frames come; both outputs show the same file
    let mut seen = [[false; 5]; 2];
    let start = Instant::now();
    while seen.iter().flatten().any(|s| !s) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "not all frames were shown: {seen:?}"
        );
        for o in 0..2 {
            seen[o][shown_frame(&env, o, &refs[o])] = true;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    // small damage: a tick does not damage the full surface, also not at the loop
    let c0 = env.commits(0);
    wait("two loops", || env.commits(0) >= c0 + 10);
    for _ in 0..200 {
        let area: i64 = {
            let mock = env.mock();
            let s = mock.surface_of_output(0).unwrap();
            s.last_damage
                .iter()
                .map(|d| d[2] as i64 * d[3] as i64)
                .sum()
        };
        assert!(area > 0 && area < 400 * 225 / 4, "damage area {area}");
        std::thread::sleep(Duration::from_millis(2));
    }
    // one buffer for each output is sufficient when the compositor releases at once
    assert_eq!(env.mock().live_buffers, 2);

    // -- hidden: no frame callbacks, so no commits and no wakeups
    env.mock().hold_frames = true;
    wait("hidden", || {
        env.ok(&["query"]).matches(" hidden\n").count() == 2
    });
    let (c0, c1) = (env.commits(0), env.commits(1));
    let switches = ctxt_switches(env.pid());
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(ctxt_switches(env.pid()), switches, "wakeups while hidden");
    assert_eq!((env.commits(0), env.commits(1)), (c0, c1));
    env.mock().show();
    wait("playing again", || {
        env.commits(0) > c0 + 3 && env.commits(1) > c1 + 3
    });
    shown_frame(&env, 0, &refs[0]);
    assert!(env.ok(&["query"]).contains(" playing\n"));

    // -- one output hidden: the other plays; the hidden one comes up to date later
    {
        let mut mock = env.mock();
        mock.hold_frames = true;
    }
    wait("both hidden", || {
        env.ok(&["query"]).matches(" hidden\n").count() == 2
    });
    {
        // show only output 0: send its held callback, keep the callback of output 1
        let mut mock = env.mock();
        let id = *mock
            .surfaces
            .iter()
            .find(|(_, s)| s.output == 0 && s.layer_surface != 0)
            .unwrap()
            .0;
        let held = std::mem::take(&mut mock.surfaces.get_mut(&id).unwrap().held_frames);
        for cb in held {
            mock.send(cb, 0, &[Arg::U(0)]);
            mock.delete(cb);
        }
    }
    // output 0 gets one frame and is hidden again; output 1 skipped that frame
    let (c0, c1) = (env.commits(0), env.commits(1));
    wait("one more frame on output 0", || env.commits(0) == c0 + 1);
    assert_eq!(env.commits(1), c1);
    env.mock().show();
    wait("output 1 plays again", || env.commits(1) > c1 + 12);
    for _ in 0..50 {
        shown_frame(&env, 0, &refs[0]);
        shown_frame(&env, 1, &refs[1]);
        std::thread::sleep(Duration::from_millis(3));
    }

    // -- manual pause
    env.ok(&["pause"]);
    assert_eq!(env.ok(&["query"]).matches(" paused\n").count(), 2);
    std::thread::sleep(Duration::from_millis(100));
    let (c0, switches) = (env.commits(0), ctxt_switches(env.pid()));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(ctxt_switches(env.pid()), switches, "wakeups while paused");
    assert_eq!(env.commits(0), c0);
    shown_frame(&env, 0, &refs[0]);
    env.ok(&["resume"]);
    wait("resumed", || env.commits(0) > c0 + 3);
    assert!(env.ok(&["query"]).contains(" playing\n"));

    // -- a compositor that keeps the buffer until the next commit: two buffers
    env.mock().release_at_commit = false;
    let c0 = env.commits(0);
    wait("frames with held buffers", || env.commits(0) > c0 + 12);
    for _ in 0..50 {
        shown_frame(&env, 0, &refs[0]);
        shown_frame(&env, 1, &refs[1]);
        std::thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(env.mock().live_buffers, 4);
    env.mock().release_at_commit = true;

    // -- output change: new size, the Target is built again
    env.mock().configure(0, (300, 200));
    wait("new size", || env.texture(0).1 == (300, 200));
    let refs0: Vec<Vec<u32>> = frames
        .iter()
        .map(|f| reference(f, (w, h), (300, 200)))
        .collect();
    let c0 = env.commits(0);
    wait("frames at the new size", || env.commits(0) > c0 + 6);
    for _ in 0..50 {
        shown_frame(&env, 0, &refs0);
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(env.ok(&["query"]).starts_with("DP-1 300x200 "));
    assert_eq!(
        env.mock().surface_of_output(0).unwrap().dst,
        Some((300, 200))
    );

    // -- pixel-art still on one output: integer scale, no player for that output
    let art = art_frames(40, 30, 1, 2).remove(0);
    let png = env.dir.join("art.png");
    to_rgba(&art, 40, 30, 3).save(&png).unwrap();
    let png = png.to_str().unwrap();
    env.ok(&["img", png, "--output", "HDMI-A-1"]);
    assert_eq!(
        env.texture(1),
        (reference(&art, (40, 30), phys[1]), phys[1])
    );
    assert_eq!(
        env.ok(&["query"]),
        format!("DP-1 300x200 {gif} playing\nHDMI-A-1 384x216 {png} static\n")
    );
    // an anchor moves the cut: 40×30 at k = 10 is 400×300, the overflow is 16×84
    env.ok(&["img", png, "-o", "HDMI-A-1", "--anchor", "bottomright"]);
    let want: Vec<u32> = (0..216 * 384)
        .map(|i| art[(i / 384 + 84) / 10 * 40 + (i % 384 + 16) / 10])
        .collect();
    assert_eq!(env.texture(1), (want, phys[1]));
    assert!(
        env.ok(&["query"])
            .ends_with(&format!("{png} static bottomright\n"))
    );
    let state = std::fs::read_to_string(env.dir.join("state/waypixel/state")).unwrap();
    assert!(state.ends_with(&format!("HDMI-A-1 img bottomright {png}\n")));
    env.ok(&["img", png, "-o", "HDMI-A-1"]);
    assert_eq!(
        env.texture(1),
        (reference(&art, (40, 30), phys[1]), phys[1])
    );
    assert_eq!(env.cache_files().len(), 2);

    // -- an image that is not pixel art: an error and a notification
    let mut rng = Rng(9);
    let photo = image::RgbaImage::from_fn(90, 40, |_, _| {
        image::Rgba([rng.below(256) as u8, rng.below(256) as u8, 7, 255])
    });
    let photo_path = env.dir.join("photo.png");
    photo.save(&photo_path).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = env.dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            env.dir.join("notified").display()
        );
        std::fs::write(bin.join("notify-send"), script).unwrap();
        let mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(bin.join("notify-send"), mode).unwrap();
    }
    let (code, _, stderr) = env.cli(&["img", photo_path.to_str().unwrap(), "-o", "HDMI-A-1"]);
    assert_eq!(
        (code, stderr.trim()),
        (1, "waypixel: the image is not true pixel art")
    );
    assert_eq!(
        std::fs::read_to_string(env.dir.join("notified")).unwrap(),
        "--app-name=waypixel\nwaypixel\nphoto.png: the image is not true pixel art\n"
    );
    std::fs::remove_file(env.dir.join("notified")).unwrap();
    env.ok(&["img", png, "-o", "HDMI-A-1"]);
    assert_eq!(env.cache_files().len(), 2);
    wait("still buffer destroyed", || env.mock().live_buffers == 1);

    // -- errors keep the old wallpaper
    let txt = env.dir.join("bad.gif");
    std::fs::write(&txt, "this is not an image").unwrap();
    let (code, _, stderr) = env.cli(&["img", txt.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.starts_with("waypixel: "), "{stderr}");
    let (code, _, _) = env.cli(&["img", "/nonexistent/file.gif"]);
    assert_eq!(code, 1);
    let noise = env.dir.join("noise.gif");
    // N = 1, and too wide for art stored at 1×
    let n0: Vec<u32> = (0..641 * 4)
        .map(|_| COLOURS[rng.below(4) as usize])
        .collect();
    let mut n1 = n0.clone();
    n1[5] = 0xFFFF_FFFF;
    write_gif(&noise, &[n0, n1], 641, 4, 1, 50);
    let (code, _, stderr) = env.cli(&["img", noise.to_str().unwrap()]);
    assert_eq!(
        (code, stderr.trim()),
        (1, "waypixel: the image is not true pixel art")
    );
    assert!(env.dir.join("notified").exists());
    assert_eq!(
        env.ok(&["query"]),
        format!("DP-1 300x200 {gif} playing\nHDMI-A-1 384x216 {png} static\n")
    );
    assert_eq!(env.cache_files().len(), 2);

    // -- kill; the state comes back at the next start
    env.assert_no_errors();
    let state = std::fs::read_to_string(env.dir.join("state/waypixel/state")).unwrap();
    assert_eq!(
        state,
        format!("all img center {gif}\nHDMI-A-1 img center {png}\n")
    );
    env.ok(&["kill"]);
    assert_eq!(env.daemon.take().unwrap().wait().unwrap().code(), Some(0));
    assert!(!env.dir.join("rt/waypixel-wayland-test.sock").exists());
    assert_eq!(env.cli(&["query"]).0, 3);

    env.mock().outputs[0].logical = (400, 225);
    env.start_daemon();
    wait("restore", || {
        env.ok(&["query"]) == format!("DP-1 400x225 {gif} playing\nHDMI-A-1 384x216 {png} static\n")
    });
    wait("restored frames", || {
        env.mock()
            .surface_of_output(0)
            .is_some_and(|s| s.tex_size == phys[0])
    });
    shown_frame(&env, 0, &refs[0]);
    assert_eq!(env.texture(1).1, phys[1]);
    assert_eq!(env.cache_files().len(), 2);

    // -- an output goes away; SIGTERM is a clean exit
    env.mock().send(2, 1, &[Arg::U(11)]);
    wait("output removed", || env.ok(&["query"]).lines().count() == 1);
    env.ok(&["clear"]);
    assert_eq!(env.ok(&["query"]), "DP-1 400x225 color #000000 static\n");
    wait("cache cleanup", || env.cache_files().is_empty());
    env.assert_no_errors();
    Command::new("kill")
        .arg(env.pid().to_string())
        .status()
        .unwrap();
    assert_eq!(env.daemon.take().unwrap().wait().unwrap().code(), Some(0));
    assert!(!env.dir.join("rt/waypixel-wayland-test.sock").exists());
}

/// SPEC §2 targets on a 4K output. Only the daemon is measured; the mock
/// compositor does not have the cost of a real texture upload.
#[test]
fn targets_4k() {
    let outputs = vec![OutputCfg {
        name: "DP-1",
        logical: (3840, 2160),
        scale120: 120,
    }];
    let mut env = Env::new("4k", outputs);
    env.start_daemon();
    let screen = (3840, 2160);
    let ticks_per_s = 100.0;

    // static: 0 % CPU, < 8 MB after the compositor released the buffer
    let art = art_frames(320, 180, 1, 3).remove(0);
    let png = env.dir.join("still.png");
    to_rgba(&art, 320, 180, 4).save(&png).unwrap();
    env.ok(&["img", png.to_str().unwrap()]);
    assert!(env.texture(0).0 == reference(&art, (320, 180), screen));
    wait("still buffer destroyed", || env.mock().live_buffers == 0);
    let (ticks, switches) = (cpu_ticks(env.pid()), ctxt_switches(env.pid()));
    std::thread::sleep(Duration::from_millis(500));
    let rss = rss_kb(env.pid());
    println!("static: RSS {rss} kB");
    assert!(rss < 8 * 1024, "static RSS {rss} kB");
    assert_eq!(cpu_ticks(env.pid()), ticks);
    assert_eq!(ctxt_switches(env.pid()), switches);

    // animation with small diffs: < 0.5 % CPU, < 45 MB
    let (w, h) = (320, 180);
    let frames = art_frames(w, h, 8, 4);
    let gif = env.dir.join("anim.gif");
    write_gif(&gif, &frames, w, h, 4, 100);
    env.ok(&["img", gif.to_str().unwrap()]);
    let refs: Vec<Vec<u32>> = frames
        .iter()
        .map(|f| reference(f, (w, h), screen))
        .collect();
    let c0 = env.commits(0);
    let (ticks, start) = (cpu_ticks(env.pid()), Instant::now());
    let mut seen = [false; 8];
    while start.elapsed() < Duration::from_secs(3) {
        let (texture, _) = env.texture(0);
        let i = refs
            .iter()
            .position(|r| *r == texture)
            .expect("texture is not a frame");
        seen[i] = true;
        std::thread::sleep(Duration::from_millis(40));
    }
    let secs = start.elapsed().as_secs_f64();
    let cpu = (cpu_ticks(env.pid()) - ticks) as f64 / ticks_per_s / secs * 100.0;
    let rss = rss_kb(env.pid());
    let frames_shown = env.commits(0) - c0;
    println!("animation: {frames_shown} frames in {secs:.1} s, CPU {cpu:.2} %, RSS {rss} kB");
    assert!(seen.iter().all(|s| *s));
    assert!(
        (25..=32).contains(&frames_shown),
        "{frames_shown} frames in 3 s at 100 ms"
    );
    assert!(cpu < 0.5, "CPU {cpu} %");
    assert!(rss < 45 * 1024, "animation RSS {rss} kB");
    assert_eq!(env.mock().live_buffers, 1);
    env.assert_no_errors();
}

/// A transition: the texture goes from the old wallpaper, through a mix of
/// the two, to the new wallpaper. The animation plays during the transition
/// and after it.
#[test]
fn burn_transition() {
    let outputs = vec![
        OutputCfg {
            name: "DP-1",
            logical: (1280, 720),
            scale120: 120,
        },
        OutputCfg {
            name: "HDMI-A-1",
            logical: (256, 144),
            scale120: 180,
        },
    ];
    let mut env = Env::new("burn", outputs);
    env.start_daemon();
    let phys = [(1280usize, 720usize), (384, 216)];
    let old = 0xFF01_0203;
    env.ok(&["clear", "--color", "010203"]);

    let (w, h) = (64, 36);
    let frames = art_frames(w, h, 3, 9);
    let gif = env.dir.join("anim.gif");
    write_gif(&gif, &frames, w, h, 4, 50);
    let start = Instant::now();
    // the transition comes from the config file
    std::fs::create_dir_all(env.dir.join("config/waypixel")).unwrap();
    std::fs::write(
        env.dir.join("config/waypixel/config"),
        "transition = slide\n",
    )
    .unwrap();
    let status = env.command().args(["img", gif.to_str().unwrap()]).status();
    assert!(status.unwrap().success());
    // the reply comes with the first step: the old wallpaper at the screen size
    assert!(env.ok(&["query"]).ends_with("anim.gif playing\n"));
    let refs: Vec<Vec<Vec<u32>>> = phys
        .iter()
        .map(|p| frames.iter().map(|f| reference(f, (w, h), *p)).collect())
        .collect();
    let mut mixed = [false; 2];
    let mut done = [false; 2];
    // the frames of the new wallpaper that were in a mix
    let mut live = [false; 3];
    // the pixels of each frame that are different from frame 0
    let marks: Vec<Vec<usize>> = (refs[0].iter())
        .map(|r| (0..r.len()).filter(|i| r[*i] != refs[0][0][*i]).collect())
        .collect();
    while !(done[0] && done[1]) {
        assert!(start.elapsed() < Duration::from_secs(20), "no end");
        for o in 0..2 {
            let (texture, size) = env.texture(o);
            assert_eq!(size, phys[o]);
            if refs[o].contains(&texture) {
                done[o] = true;
                continue;
            }
            // only the animation changes the texture after the transition
            assert!(!done[o], "output {o}: bad texture after the transition");
            let n = texture.len();
            let count = |f: &dyn Fn(usize) -> bool| (0..n).filter(|i| f(*i)).count();
            let old_px = count(&|i| texture[i] == old);
            let new_px = count(&|i| refs[o].iter().any(|r| texture[i] == r[i]));
            assert!(old_px + new_px > n / 2, "output {o}: {old_px} + {new_px}");
            mixed[o] |= old_px > n / 10 && new_px > n / 10;
            for f in 1..3 {
                live[f] |= o == 0 && marks[f].iter().all(|i| texture[*i] == refs[0][f][*i]);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(mixed, [true; 2]);
    assert_eq!(live, [false, true, true], "the animation did not play");
    let secs = start.elapsed().as_secs_f64();
    assert!((2.0..6.0).contains(&secs), "the transition took {secs} s");
    let mut seen = [false; 3];
    wait("all frames after the transition", || {
        seen[shown_frame(&env, 0, &refs[0])] = true;
        seen.iter().all(|s| *s)
    });

    // to a colour, on one output: the result is the 1×1 buffer
    env.ok(&[
        "clear", "--color", "405060", "-o", "HDMI-A-1", "--fx", "burn",
    ]);
    assert_eq!(env.texture(1).1, phys[1]);
    wait("colour after the transition", || {
        env.texture(1) == (vec![0xFF40_5060], (1, 1))
    });
    shown_frame(&env, 0, &refs[0]);

    // `disabled` in the config file: the reply comes with the final pixels
    std::fs::write(
        env.dir.join("config/waypixel/config"),
        "transition = disabled\n",
    )
    .unwrap();
    let args = ["clear", "--color", "112233", "-o", "HDMI-A-1"];
    assert!(env.command().args(args).status().unwrap().success());
    assert_eq!(env.texture(1), (vec![0xFF11_2233], (1, 1)));
    for fx in ["sweep", "wipe"] {
        env.ok(&["clear", "--color", "405060", "-o", "HDMI-A-1", "--fx", fx]);
        assert_eq!(env.texture(1).1, phys[1]);
        env.ok(&["clear", "--color", "112233", "-o", "HDMI-A-1"]);
    }
    let (code, _, _) = env.cli(&["clear", "--fx", "melt"]);
    assert_eq!(code, 2);
    env.ok(&["clear", "--color", "405060", "-o", "HDMI-A-1"]);
    assert_eq!(
        env.ok(&["query"]),
        format!(
            "DP-1 1280x720 {} playing\nHDMI-A-1 384x216 color #405060 static\n",
            gif.display()
        )
    );
    wait("one buffer", || env.mock().live_buffers == 1);
    env.assert_no_errors();
}
