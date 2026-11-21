//! Daemon: poll loop, IPC server, state file, signals.
//!
//! The daemon is single-threaded. It never decodes an image: a decode child
//! writes the cache file, and the daemon reads it with `mmap`.

pub mod output;
pub mod player;
pub mod pool;
pub mod wayland;

use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use rustix::time::{
    ClockId, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, clock_gettime,
    timerfd_create, timerfd_settime,
};
use waybackend::types::ObjectId;

use crate::cache::{self, Cache, Hash};
use crate::fx::{Canvas, Effect, Kind, STEP_NS};
use crate::ipc;
use crate::scale::Anchor;
use output::{Output, Wall};
use player::Player;
use wayland::{Obj, Wl, req};

const BLACK: u32 = 0xFF00_0000;
const MAX_CLIENTS: usize = 16;
const MAX_JOBS: usize = 8;
const MAX_REQUEST: usize = 1 << 16;

// ---- signals ----------------------------------------------------------------
// rustix has no signalfd. These functions come from the C library that std links.

#[repr(C)]
struct SigSet([u64; 16]);

unsafe extern "C" {
    fn sigemptyset(set: *mut SigSet) -> i32;
    fn sigaddset(set: *mut SigSet, signal: i32) -> i32;
    fn sigprocmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
    fn signalfd(fd: i32, mask: *const SigSet, flags: i32) -> i32;
}

const SIG_BLOCK: i32 = 0;
const SIG_SETMASK: i32 = 2;
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SFD_CLOEXEC: i32 = 0o2000000;
const SFD_NONBLOCK: i32 = 0o4000;

/// Blocks SIGINT, SIGTERM and SIGHUP and returns a signalfd for them.
fn signal_fd() -> Result<OwnedFd, String> {
    let mut set = SigSet([0; 16]);
    // SAFETY: `set` is a valid sigset_t (128 bytes on Linux); the fd is new and owned
    unsafe {
        sigemptyset(&mut set);
        for signal in [SIGHUP, SIGINT, SIGTERM] {
            sigaddset(&mut set, signal);
        }
        if sigprocmask(SIG_BLOCK, &set, core::ptr::null_mut()) != 0 {
            return Err("sigprocmask failed".into());
        }
        match signalfd(-1, &set, SFD_CLOEXEC | SFD_NONBLOCK) {
            -1 => Err("signalfd failed".into()),
            fd => Ok(OwnedFd::from_raw_fd(fd)),
        }
    }
}

/// Unblocks all signals. The decode child gets the signal mask of the daemon.
pub fn reset_signals() {
    let mut set = SigSet([0; 16]);
    // SAFETY: `set` is a valid sigset_t
    unsafe {
        sigemptyset(&mut set);
        sigprocmask(SIG_SETMASK, &set, core::ptr::null_mut());
    }
}

fn now_ns() -> u64 {
    let t = clock_gettime(ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

// ---- state file -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Spec {
    Color(u32),
    /// `hash` is known when the cache file of the image is valid
    Img {
        path: String,
        hash: Option<Hash>,
        anchor: Anchor,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// output name, or `all`
    target: String,
    spec: Spec,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    path: PathBuf,
}

impl State {
    fn load() -> Self {
        let path = ipc::xdg_dir("XDG_STATE_HOME", ".local/state").join("pixelwp/state");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Self {
            entries: Self::parse(&text),
            path,
        }
    }

    fn parse(text: &str) -> Vec<Entry> {
        let mut entries: Vec<Entry> = Vec::new();
        for line in text.lines() {
            let mut parts = line.splitn(3, ' ');
            let (Some(target), Some(kind), Some(value)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let spec = match kind {
                "img" => {
                    // a line of an older version has no anchor; a path starts with `/`
                    let (anchor, path) = match value.split_once(' ') {
                        Some((anchor, path)) if !value.starts_with('/') => (anchor, path),
                        _ => ("center", value),
                    };
                    let Some(anchor) = Anchor::parse(anchor) else {
                        continue;
                    };
                    Spec::Img {
                        path: path.to_owned(),
                        hash: None,
                        anchor,
                    }
                }
                "color" => match ipc::parse_color(value) {
                    Some(color) => Spec::Color(color),
                    None => continue,
                },
                _ => continue,
            };
            entries.retain(|e| e.target != target);
            entries.push(Entry {
                target: target.to_owned(),
                spec,
            });
        }
        entries
    }

    fn format(&self) -> String {
        let mut text = String::new();
        for e in &self.entries {
            match &e.spec {
                Spec::Color(c) => text += &format!("{} color {:06x}\n", e.target, c & 0xFF_FFFF),
                Spec::Img { path, anchor, .. } => {
                    text += &format!("{} img {} {}\n", e.target, anchor.name(), path)
                }
            }
        }
        text
    }

    /// Writes a temp file, then renames it.
    fn save(&self) {
        let write = || {
            std::fs::create_dir_all(self.path.parent().unwrap())?;
            let tmp = self.path.with_extension("tmp");
            std::fs::write(&tmp, self.format())?;
            std::fs::rename(&tmp, &self.path)
        };
        if let Err(e) = write() {
            eprintln!("pixelwp: cannot write {}: {e}", self.path.display());
        }
    }

    /// `all` replaces all lines. An output name replaces the line of that output.
    fn set(&mut self, target: &str, spec: Spec) {
        if target == "all" {
            self.entries.clear();
        } else {
            self.entries.retain(|e| e.target != target);
        }
        self.entries.push(Entry {
            target: target.to_owned(),
            spec,
        });
    }

    /// The line of this output, or else the `all` line.
    fn lookup(&self, name: &str) -> Option<&Entry> {
        let find = |target: &str| self.entries.iter().find(|e| e.target == target);
        find(name).or_else(|| find("all"))
    }
}

// ---- daemon -----------------------------------------------------------------

/// The transition of a request. `config` is the `transition` key of
/// `$XDG_CONFIG_HOME/pixelwp/config`. The file is read for each request, so a
/// change needs no restart. A bad file gives a log line and the default.
fn transition(name: &str) -> Result<Option<Kind>, String> {
    if name != "config" {
        return Kind::parse(name).ok_or(format!("bad transition {name}"));
    }
    let path = ipc::xdg_dir("XDG_CONFIG_HOME", ".config").join("pixelwp/config");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(Kind::from_config(&text).unwrap_or_else(|e| {
        eprintln!("pixelwp: {}: {e}", path.display());
        Some(Kind::Burn)
    }))
}

/// An output for which `wl_output.done` has not arrived.
struct Pending {
    global: u32,
    wl_output: ObjectId,
    name: Option<String>,
    scale: u32,
}

struct Client {
    id: u64,
    stream: UnixStream,
    buf: Vec<u8>,
    /// the request is complete and the reply waits for a decode child
    waiting: bool,
}

/// A running decode child.
struct Job {
    child: Child,
    pidfd: OwnedFd,
    stderr: File,
    hash: Hash,
    path: String,
    anchor: Anchor,
    target: String,
    client: Option<u64>,
    /// false for a restore: the state file does not change
    persist: bool,
    /// show the change with this transition
    fx: Option<Kind>,
}

/// A transition to one wall. All outputs that got the wall with one
/// request show the same transition.
struct Transition {
    id: u32,
    wall: Wall,
    effect: Effect,
    /// the image of `wall`
    new: Canvas,
    /// `CLOCK_MONOTONIC` time in ns of the next step
    deadline: u64,
}

struct Daemon {
    wl: Wl,
    pending: Vec<Pending>,
    outputs: Vec<Output>,
    players: Vec<Player>,
    next_player: u32,
    transitions: Vec<Transition>,
    next_transition: u32,
    paused: bool,
    /// exit code, when the daemon must stop
    exit: Option<i32>,
    state: State,
    /// true during the restore at start: no cache cleanup
    restoring: bool,
    jobs: Vec<Job>,
    clients: Vec<Client>,
    next_client: u64,
    timer: OwnedFd,
    /// the deadline that the timerfd has; `None` after the timer expired
    armed: Option<Option<u64>>,
}

impl Daemon {
    fn bind_output(&mut self, global: u32, version: u32) {
        if version < 4 {
            eprintln!("pixelwp: wl_output must be version 4 or higher");
            return;
        }
        let wl_output = self.wl.objman.create(Obj::Output);
        req!(
            self.wl,
            wl_registry::req::bind(
                self.wl.registry,
                global,
                wl_output,
                wayland::wl_output::NAME,
                4
            )
        );
        self.pending.push(Pending {
            global,
            wl_output,
            name: None,
            scale: 1,
        });
    }

    fn remove_output(&mut self, index: usize) {
        self.outputs.remove(index).destroy(&mut self.wl);
        self.gc();
    }

    /// Returns the wall for an image with a valid cache file. Makes a player for
    /// an animation, if there is none for this file.
    fn wall_for(&mut self, hash: &Hash, path: &str, anchor: Anchor) -> Result<Wall, String> {
        let path = path.to_owned();
        let anim = |player: u32| Wall::Anim {
            player,
            path: path.clone(),
            anchor,
        };
        if let Some(p) = self.players.iter().find(|p| p.hash == *hash) {
            return Ok(anim(p.id));
        }
        let cache = Cache::open(&cache::path_for(hash))?;
        if cache.frame_count() == 1 {
            return Ok(Wall::Still {
                hash: *hash,
                path,
                anchor,
            });
        }
        let mut player = Player::new(self.next_player, *hash, cache)?;
        self.next_player += 1;
        if !self.paused {
            player.deadline = Some(now_ns() + player.delay_ns());
        }
        let id = player.id;
        self.players.push(player);
        Ok(anim(id))
    }

    /// The 1× image of a wall. `None` when the cache file or the player is not there.
    fn canvas_of(&self, wall: &Wall) -> Option<Canvas> {
        match wall {
            Wall::Color(color) => Some(Canvas::color(*color)),
            Wall::Still { hash, anchor, .. } => {
                let cache = Cache::open(&cache::path_for(hash)).ok()?;
                let mut px = vec![0u32; cache.w as usize * cache.h as usize];
                cache.apply(0, &mut px, &mut []).ok()?;
                Some(Canvas {
                    w: cache.w,
                    h: cache.h,
                    px: px.into(),
                    anchor: *anchor,
                })
            }
            Wall::Anim { player, anchor, .. } => {
                let p = self.players.iter().find(|p| p.id == *player)?;
                let (w, h) = p.size();
                Some(Canvas {
                    w,
                    h,
                    px: p.canvas().into(),
                    anchor: *anchor,
                })
            }
        }
    }

    /// Starts a transition from `old` to the wall of the output. Returns
    /// false when there is no transition: an image is not there.
    fn start_fx(&mut self, index: usize, old: &Wall, kind: Kind) -> bool {
        let old_player = match old {
            Wall::Anim { player, .. } => Some(*player),
            _ => None,
        };
        let Some(old) = self.canvas_of(old) else {
            return false;
        };
        let wall = &self.outputs[index].wall;
        // the transition that an earlier output of the same request started
        let found = self
            .transitions
            .iter()
            .position(|t| t.effect.step == 0 && t.effect.kind == kind && t.wall == *wall);
        let t = match found {
            Some(t) => t,
            None => {
                let Some(new) = self.canvas_of(wall) else {
                    return false;
                };
                let now = now_ns();
                self.transitions.push(Transition {
                    id: self.next_transition,
                    wall: wall.clone(),
                    effect: Effect::new(kind, now),
                    new,
                    deadline: now + STEP_NS,
                });
                self.next_transition += 1;
                self.transitions.len() - 1
            }
        };
        let t = &self.transitions[t];
        let started = self.outputs[index].start_fx(
            &mut self.wl,
            (t.id, &t.effect),
            (old, old_player),
            &t.new,
            &self.players,
        );
        if started {
            self.wake(index);
        }
        started
    }

    /// Starts the hidden players that the output uses.
    fn wake(&mut self, index: usize) {
        if self.paused {
            return;
        }
        let output = &self.outputs[index];
        let now = now_ns();
        for p in &mut self.players {
            if p.deadline.is_none() && output.uses(p.id) {
                p.deadline = Some(now + p.delay_ns());
            }
        }
    }

    /// Advances the transitions whose deadline has come, and renders their
    /// ready outputs. When a transition is complete, its outputs show the wall,
    /// and the player of the old wall is removed.
    ///
    /// This is a tick path: there is no allocation and no log, but for the last step.
    fn run_transitions(&mut self) {
        if self.transitions.is_empty() {
            return;
        }
        let now = now_ns();
        let mut i = 0;
        while i < self.transitions.len() {
            let t = &mut self.transitions[i];
            if t.deadline > now {
                i += 1;
                continue;
            }
            if t.effect.advance() {
                for o in &mut self.outputs {
                    if o.fx() == Some(t.id) && o.ready() {
                        o.fx_tick(&mut self.wl, &t.effect, &t.new, &self.players);
                    }
                }
                let late = now - t.deadline > STEP_NS;
                t.deadline = if late { now } else { t.deadline } + STEP_NS;
                i += 1;
                continue;
            }
            let id = self.transitions.swap_remove(i).id;
            for index in 0..self.outputs.len() {
                if self.outputs[index].fx() == Some(id) {
                    self.rebuild(index);
                }
            }
            self.gc_players();
        }
    }

    /// Sets the wall of an output from the state and renders it. With `fx`, a
    /// transition shows the change.
    fn assign(&mut self, index: usize, fx: Option<Kind>) {
        let spec = self
            .state
            .lookup(&self.outputs[index].name)
            .map(|e| e.spec.clone());
        let wall = match spec {
            Some(Spec::Color(color)) => Wall::Color(color),
            Some(Spec::Img {
                path,
                hash: Some(hash),
                anchor,
            }) => self.wall_for(&hash, &path, anchor).unwrap_or_else(|e| {
                eprintln!("pixelwp: {path}: {e}");
                Wall::Color(BLACK)
            }),
            _ => Wall::Color(BLACK),
        };
        let old = std::mem::replace(&mut self.outputs[index].wall, wall);
        if let Some(kind) = fx
            && old != self.outputs[index].wall
            && self.start_fx(index, &old, kind)
        {
            return;
        }
        self.rebuild(index);
    }

    fn rebuild(&mut self, index: usize) {
        let output = &mut self.outputs[index];
        let Err(e) = output.rebuild(&mut self.wl, &self.players) else {
            self.wake(index);
            return;
        };
        eprintln!("pixelwp: {}: {e}", output.name);
        // A cache file that became bad is decoded again.
        if let Wall::Still { hash, path, anchor } = &output.wall
            && Cache::open(&cache::path_for(hash)).is_err()
            && let Some(entry) = self.state.lookup(&output.name)
        {
            let (path, target, anchor) = (path.clone(), entry.target.clone(), *anchor);
            if let Err(e) = self.start_img(&path, anchor, &target, None, false, None) {
                eprintln!("pixelwp: {e}");
            }
        }
    }

    /// Assigns again all outputs that use the state line `target`.
    fn apply_target(&mut self, target: &str, fx: Option<Kind>) {
        for index in 0..self.outputs.len() {
            let uses = self
                .state
                .lookup(&self.outputs[index].name)
                .is_some_and(|e| e.target == target);
            if uses {
                self.assign(index, fx);
            }
        }
    }

    /// Removes the players and the transitions that no output uses. The old
    /// wall of a transition keeps its player until the transition ends.
    fn gc_players(&mut self) {
        let outputs = &self.outputs;
        self.players
            .retain(|p| outputs.iter().any(|o| o.uses(p.id)));
        self.transitions
            .retain(|t| outputs.iter().any(|o| o.fx() == Some(t.id)));
    }

    /// Removes the players that no output uses, and the cache files that the
    /// state does not use.
    fn gc(&mut self) {
        self.gc_players();
        if self.restoring {
            return;
        }
        let mut keep: Vec<Hash> = self.jobs.iter().map(|j| j.hash).collect();
        keep.extend(self.state.entries.iter().filter_map(|e| match e.spec {
            Spec::Img { hash, .. } => hash,
            Spec::Color(_) => None,
        }));
        cache::cleanup(&keep);
    }

    fn check_target(&self, target: &str) -> Result<(), String> {
        let known = target == "all"
            || self.outputs.iter().any(|o| o.name == target)
            || self
                .pending
                .iter()
                .any(|p| p.name.as_deref() == Some(target));
        known
            .then_some(())
            .ok_or(format!("unknown output {target}"))
    }

    /// Sets an image. Returns `Ok(true)` when the cache was valid and the image
    /// is set, and `Ok(false)` when a decode child was started.
    fn start_img(
        &mut self,
        path: &str,
        anchor: Anchor,
        target: &str,
        client: Option<u64>,
        persist: bool,
        fx: Option<Kind>,
    ) -> Result<bool, String> {
        let hash =
            cache::hash_file(path.as_ref()).map_err(|e| format!("cannot read {path}: {e}"))?;
        let cache_path = cache::path_for(&hash);
        if Cache::open(&cache_path).is_ok() {
            self.apply_image(target, path, anchor, hash, persist, fx);
            return Ok(true);
        }
        _ = std::fs::remove_file(&cache_path);
        if self.jobs.len() >= MAX_JOBS {
            return Err("too many decodes are in progress".into());
        }
        let spawn = || -> std::io::Result<(Child, OwnedFd, File)> {
            // The child writes its error text to a memfd, so it cannot block on a full pipe.
            let memfd =
                rustix::fs::memfd_create(c"pixelwp-decode", rustix::fs::MemfdFlags::CLOEXEC)?;
            let stderr = File::from(memfd);
            // /proc/self/exe is this binary, also after a new version replaced the file
            let child = Command::new("/proc/self/exe")
                .arg("decode")
                .arg(path)
                .arg(&cache_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(stderr.try_clone()?)
                .spawn()?;
            let pid = rustix::process::Pid::from_child(&child);
            let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
            Ok((child, pidfd, stderr))
        };
        let (child, pidfd, stderr) =
            spawn().map_err(|e| format!("cannot start the decode: {e}"))?;
        self.jobs.push(Job {
            child,
            pidfd,
            stderr,
            hash,
            path: path.to_owned(),
            anchor,
            target: target.to_owned(),
            client,
            persist,
            fx,
        });
        Ok(false)
    }

    /// Makes an image with a valid cache file the wall of `target`.
    fn apply_image(
        &mut self,
        target: &str,
        path: &str,
        anchor: Anchor,
        hash: Hash,
        persist: bool,
        fx: Option<Kind>,
    ) {
        if persist {
            let spec = Spec::Img {
                path: path.to_owned(),
                hash: Some(hash),
                anchor,
            };
            self.state.set(target, spec);
            self.state.save();
        } else {
            // a restore: the line must be the same as at the start of the decode
            match self.state.entries.iter_mut().find(|e| e.target == target) {
                Some(Entry {
                    spec:
                        Spec::Img {
                            path: p,
                            hash: h,
                            anchor: a,
                        },
                    ..
                }) if p == path && *a == anchor => *h = Some(hash),
                _ => return,
            }
        }
        self.apply_target(target, fx);
        self.gc();
    }

    /// Handles the exit of a decode child.
    fn finish_job(&mut self, pid: u32) {
        let Some(index) = self.jobs.iter().position(|j| j.child.id() == pid) else {
            return;
        };
        let mut job = self.jobs.swap_remove(index);
        let ok = job.child.wait().is_ok_and(|status| status.success());
        let result = if ok && Cache::open(&cache::path_for(&job.hash)).is_ok() {
            self.apply_image(
                &job.target,
                &job.path,
                job.anchor,
                job.hash,
                job.persist,
                job.fx,
            );
            Ok(String::new())
        } else {
            let mut text = String::new();
            _ = job.stderr.rewind();
            _ = job.stderr.read_to_string(&mut text);
            let line = text.lines().find(|l| !l.trim().is_empty());
            Err(line.unwrap_or("the decode failed").trim().to_owned())
        };
        match job.client {
            Some(id) => self.reply(id, result),
            None => {
                if let Err(e) = result {
                    eprintln!("pixelwp: {}: {e}", job.path);
                }
            }
        }
    }

    fn query(&self) -> String {
        let mut text = String::new();
        for o in &self.outputs {
            let (w, h) = o.physical();
            let anchor = match &o.wall {
                Wall::Color(_) => Anchor::default(),
                Wall::Still { anchor, .. } | Wall::Anim { anchor, .. } => *anchor,
            };
            let (what, state) = match &o.wall {
                Wall::Color(c) => (format!("color #{:06X}", c & 0xFF_FFFF), "static"),
                Wall::Still { path, .. } => (path.clone(), "static"),
                Wall::Anim { player, path, .. } => {
                    let playing = self
                        .players
                        .iter()
                        .any(|p| p.id == *player && p.deadline.is_some());
                    let state = match (self.paused, playing) {
                        (true, _) => "paused",
                        (false, true) => "playing",
                        (false, false) => "hidden",
                    };
                    (path.clone(), state)
                }
            };
            text += &format!("{} {w}x{h} {what} {state}", o.name);
            // the default anchor is not shown
            if anchor != Anchor::default() {
                text += &format!(" {}", anchor.name());
            }
            text.push('\n');
        }
        text
    }

    /// Handles a request. Returns `None` when the reply comes later, from a decode child.
    fn handle(&mut self, client: u64, args: &[&str]) -> Option<Result<String, String>> {
        let ok = || Some(Ok(String::new()));
        match *args {
            ["query"] => Some(Ok(self.query())),
            ["pause"] => {
                self.paused = true;
                self.players.iter_mut().for_each(|p| p.deadline = None);
                ok()
            }
            ["resume"] => {
                if self.paused {
                    self.paused = false;
                    let now = now_ns();
                    for p in &mut self.players {
                        p.deadline = Some(now + p.delay_ns());
                    }
                }
                ok()
            }
            ["kill"] => {
                self.exit = Some(0);
                ok()
            }
            ["clear", color, target, fx] => {
                let fx = match transition(fx) {
                    Ok(fx) => fx,
                    Err(e) => return Some(Err(e)),
                };
                let Some(color) = ipc::parse_color(color) else {
                    return Some(Err(format!("bad colour {color}")));
                };
                if let Err(e) = self.check_target(target) {
                    return Some(Err(e));
                }
                self.state.set(target, Spec::Color(color));
                self.state.save();
                self.apply_target(target, fx);
                self.gc();
                ok()
            }
            ["img", path, target, fx, anchor] => {
                let fx = match transition(fx) {
                    Ok(fx) => fx,
                    Err(e) => return Some(Err(e)),
                };
                let Some(anchor) = Anchor::parse(anchor) else {
                    return Some(Err(format!("bad anchor {anchor}")));
                };
                if path.contains('\n') {
                    return Some(Err("the path contains a newline".into()));
                }
                if let Err(e) = self.check_target(target) {
                    return Some(Err(e));
                }
                match self.start_img(path, anchor, target, Some(client), true, fx) {
                    Ok(true) => ok(),
                    Ok(false) => None,
                    Err(e) => Some(Err(e)),
                }
            }
            _ => Some(Err("bad request".into())),
        }
    }

    /// Sends the reply and closes the connection.
    fn reply(&mut self, client: u64, result: Result<String, String>) {
        let Some(index) = self.clients.iter().position(|c| c.id == client) else {
            return;
        };
        let client = self.clients.swap_remove(index);
        let text = match result {
            Ok(body) => format!("ok\n{body}"),
            Err(e) => format!("error {e}\n"),
        };
        // NOSIGNAL: a client that went away must not stop the daemon
        _ = rustix::net::send(
            &client.stream,
            text.as_bytes(),
            rustix::net::SendFlags::NOSIGNAL,
        );
    }

    /// Reads from a client. Handles the request when the client has shut down its write side.
    fn read_client(&mut self, id: u64) {
        let Some(index) = self.clients.iter().position(|c| c.id == id) else {
            return;
        };
        let client = &mut self.clients[index];
        let mut chunk = [0u8; 4096];
        let complete = loop {
            match client.stream.read(&mut chunk) {
                Ok(0) => break true,
                Ok(n) => client.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break false,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    client.buf.clear();
                    break true;
                }
            }
            if client.buf.len() > MAX_REQUEST {
                client.buf.clear();
                break true;
            }
        };
        if !complete {
            return;
        }
        client.waiting = true;
        let buf = std::mem::take(&mut client.buf);
        let result = match ipc::decode(&buf) {
            Some(args) => self.handle(id, &args),
            None => Some(Err("bad request".into())),
        };
        if let Some(result) = result {
            self.reply(id, result);
        }
    }

    fn accept(&mut self, listener: &UnixListener) {
        while let Ok((stream, _)) = listener.accept() {
            if self.clients.len() >= MAX_CLIENTS || stream.set_nonblocking(true).is_err() {
                continue;
            }
            self.clients.push(Client {
                id: self.next_client,
                stream,
                buf: Vec::new(),
                waiting: false,
            });
            self.next_client += 1;
        }
    }

    /// Advances the players whose deadline has come, and renders their ready outputs.
    ///
    /// This is the tick path: there is no allocation and no log.
    fn run_players(&mut self) {
        if !self.players.iter().any(|p| p.deadline.is_some()) {
            return;
        }
        let now = now_ns();
        for p in &mut self.players {
            let Some(deadline) = p.deadline else {
                continue;
            };
            if deadline > now {
                continue;
            }
            let id = Some(p.id);
            // An output is ready when the frame callback of its last commit has arrived.
            // With no ready output, the player is hidden: it has no deadline.
            // A transition draws the frames at its steps, also on an output that
            // is not ready.
            let shown = |o: &Output| o.uses(p.id) && (o.fx().is_some() || o.ready());
            if !self.outputs.iter().any(shown) || p.advance().is_err() {
                p.deadline = None;
                continue;
            }
            for o in self
                .outputs
                .iter_mut()
                .filter(|o| o.playing() == id && o.ready())
            {
                o.tick(&mut self.wl, p);
            }
            let delay = p.delay_ns();
            // no drift; but when more than one delay late, start again from now
            p.deadline = Some(
                if now - deadline > delay {
                    now
                } else {
                    deadline
                } + delay,
            );
        }
    }

    /// Sets the timerfd to the earliest deadline, or disarms it.
    fn arm_timer(&mut self) {
        let steps = self.transitions.iter().map(|t| t.deadline);
        let want = self
            .players
            .iter()
            .filter_map(|p| p.deadline)
            .chain(steps)
            .min();
        if self.armed == Some(want) {
            return;
        }
        let ns = want.unwrap_or(0);
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: (ns / 1_000_000_000) as i64,
                // an absolute time of 0 would disarm the timer
                tv_nsec: ((ns % 1_000_000_000) as i64).max(want.is_some() as i64),
            },
        };
        // A new time also clears the expiry of the timerfd, so no read is necessary.
        if timerfd_settime(&self.timer, TimerfdTimerFlags::ABSTIME, &spec).is_ok() {
            self.armed = Some(want);
        }
    }

    fn output_by(&mut self, f: impl Fn(&Output) -> bool) -> Option<&mut Output> {
        self.outputs.iter_mut().find(|o| f(o))
    }
}

// ---- Wayland events -----------------------------------------------------------

impl wayland::wl_display::EvHandler for Daemon {
    fn error(&mut self, _: ObjectId, object_id: ObjectId, code: u32, message: &str) {
        eprintln!("pixelwp: Wayland protocol error: object {object_id}, code {code}: {message}");
        self.exit = Some(1);
    }

    fn delete_id(&mut self, _: ObjectId, id: u32) {
        self.wl.objman.remove(id);
    }
}

impl wayland::wl_registry::EvHandler for Daemon {
    fn global(&mut self, _: ObjectId, name: u32, interface: &str, version: u32) {
        if interface == wayland::wl_output::NAME {
            self.bind_output(name, version);
        }
    }

    fn global_remove(&mut self, _: ObjectId, name: u32) {
        if let Some(i) = self.outputs.iter().position(|o| o.global == name) {
            self.remove_output(i);
        } else if let Some(i) = self.pending.iter().position(|p| p.global == name) {
            let pending = self.pending.swap_remove(i);
            req!(self.wl, wl_output::req::release(pending.wl_output));
        }
    }
}

impl wayland::wl_output::EvHandler for Daemon {
    fn geometry(
        &mut self,
        _: ObjectId,
        _x: i32,
        _y: i32,
        _physical_width: i32,
        _physical_height: i32,
        _subpixel: wayland::wl_output::Subpixel,
        _make: &str,
        _model: &str,
        _transform: wayland::wl_output::Transform,
    ) {
    }

    fn mode(
        &mut self,
        _: ObjectId,
        _flags: wayland::wl_output::Mode,
        _width: i32,
        _height: i32,
        _refresh: i32,
    ) {
    }

    fn done(&mut self, sender: ObjectId) {
        let Some(i) = self.pending.iter().position(|p| p.wl_output == sender) else {
            return;
        };
        let p = self.pending.swap_remove(i);
        let name = p.name.unwrap_or_else(|| format!("output-{}", p.global));
        let output = Output::new(
            &mut self.wl,
            p.global,
            p.wl_output,
            name,
            p.scale,
            Wall::Color(BLACK),
        );
        self.outputs.push(output);
        self.assign(self.outputs.len() - 1, None);
    }

    fn scale(&mut self, sender: ObjectId, factor: i32) {
        let factor = factor.max(1) as u32;
        if let Some(p) = self.pending.iter_mut().find(|p| p.wl_output == sender) {
            p.scale = factor;
        } else if let Some(o) = self.output_by(|o| o.wl_output == sender)
            && o.int_scale != factor
        {
            o.int_scale = factor;
            o.dirty |= o.frac_scale.is_none();
        }
    }

    fn name(&mut self, sender: ObjectId, name: &str) {
        if let Some(p) = self.pending.iter_mut().find(|p| p.wl_output == sender) {
            p.name = Some(name.to_owned());
        }
    }

    fn description(&mut self, _: ObjectId, _description: &str) {}
}

impl wayland::zwlr_layer_surface_v1::EvHandler for Daemon {
    fn configure(&mut self, sender: ObjectId, serial: u32, width: u32, height: u32) {
        let Some(i) = self.outputs.iter().position(|o| o.layer_surface == sender) else {
            return;
        };
        req!(
            self.wl,
            zwlr_layer_surface_v1::req::ack_configure(sender, serial)
        );
        let o = &mut self.outputs[i];
        if width > 0 && height > 0 && o.logical != Some((width, height)) {
            o.logical = Some((width, height));
            o.dirty = true;
        }
    }

    fn closed(&mut self, sender: ObjectId) {
        if let Some(i) = self.outputs.iter().position(|o| o.layer_surface == sender) {
            self.remove_output(i);
        }
    }
}

impl wayland::wp_fractional_scale_v1::EvHandler for Daemon {
    fn preferred_scale(&mut self, sender: ObjectId, scale: u32) {
        if let Some(o) = self.output_by(|o| o.fractional == Some(sender))
            && scale > 0
            && o.frac_scale != Some(scale)
        {
            o.frac_scale = Some(scale);
            o.dirty = true;
        }
    }
}

impl wayland::wl_buffer::EvHandler for Daemon {
    fn release(&mut self, sender: ObjectId) {
        for o in &mut self.outputs {
            if o.released(&mut self.wl, sender) {
                break;
            }
        }
    }
}

impl wayland::wl_callback::EvHandler for Daemon {
    fn done(&mut self, sender: ObjectId, _callback_data: u32) {
        let Some(o) = self.outputs.iter_mut().find(|o| o.callback == Some(sender)) else {
            return;
        };
        o.callback = None;
        if self.paused {
            return;
        }
        // A hidden player starts again when one of its outputs is ready.
        let id = o.playing();
        if let Some(p) = self.players.iter_mut().find(|p| Some(p.id) == id)
            && p.deadline.is_none()
        {
            p.deadline = Some(now_ns());
        }
    }
}

impl wayland::wl_surface::EvHandler for Daemon {
    fn enter(&mut self, _: ObjectId, _output: ObjectId) {}
    fn leave(&mut self, _: ObjectId, _output: ObjectId) {}
    fn preferred_buffer_scale(&mut self, _: ObjectId, _factor: i32) {}
    fn preferred_buffer_transform(
        &mut self,
        _: ObjectId,
        _transform: wayland::wl_output::Transform,
    ) {
    }
}

impl wayland::wl_shm::EvHandler for Daemon {
    fn format(&mut self, _: ObjectId, _format: wayland::wl_shm::Format) {}
}

impl wayland::wl_compositor::EvHandler for Daemon {}
impl wayland::wl_shm_pool::EvHandler for Daemon {}
impl wayland::wl_region::EvHandler for Daemon {}
impl wayland::zwlr_layer_shell_v1::EvHandler for Daemon {}
impl wayland::wp_viewporter::EvHandler for Daemon {}
impl wayland::wp_viewport::EvHandler for Daemon {}
impl wayland::wp_fractional_scale_manager_v1::EvHandler for Daemon {}

/// Calls the event handler of the interface of the sender. An event that cannot
/// be decoded (for example a new enum value) is ignored.
macro_rules! dispatch {
    ($daemon:ident, $sender:ident, $msgs:ident, $(($variant:ident, $interface:ident)),* $(,)?) => {
        match $sender {
            $(Obj::$variant => _ = wayland::$interface::event(&mut $daemon, &mut $msgs),)*
        }
    };
}

// ---- main loop ----------------------------------------------------------------

/// Runs the daemon. Returns the exit code.
pub fn run() -> i32 {
    match run_inner() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pixelwp: {e}");
            1
        }
    }
}

fn run_inner() -> Result<i32, String> {
    let socket_path = ipc::socket_path();
    if UnixStream::connect(&socket_path).is_ok() {
        println!("pixelwp: the daemon is already running");
        return Ok(0);
    }

    // Wayland globals
    let (mut backend, mut objman, mut receiver) = wayland::connect()?;
    let registry = objman.create(Obj::Registry);
    let sync = objman.create(Obj::Callback);
    let mut output_globals = Vec::new();
    waybackend::roundtrip(
        &mut backend,
        &mut receiver,
        registry,
        sync,
        |backend, global| {
            use wayland::*;
            let (obj, max) = match global.interface() {
                wl_compositor::NAME => (Obj::Compositor, wl_compositor::MAX_VERSION),
                wl_shm::NAME => (Obj::Shm, wl_shm::MAX_VERSION),
                zwlr_layer_shell_v1::NAME => (Obj::LayerShell, zwlr_layer_shell_v1::MAX_VERSION),
                wp_viewporter::NAME => (Obj::Viewporter, wp_viewporter::MAX_VERSION),
                wp_fractional_scale_manager_v1::NAME => (
                    Obj::FractionalScaler,
                    wp_fractional_scale_manager_v1::MAX_VERSION,
                ),
                wl_output::NAME => {
                    output_globals.push((global.name(), global.version()));
                    return;
                }
                _ => return,
            };
            let id = objman.create(obj);
            let version = global.version().min(max);
            _ = wl_registry::req::bind(
                backend,
                registry,
                global.name(),
                id,
                global.interface(),
                version,
            );
        },
    )
    .map_err(|e| format!("Wayland roundtrip failed: {e}"))?;
    let wl = Wl::new(backend, objman, registry)?;

    // IPC socket, signals, timer
    _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| format!("cannot bind {}: {e}", socket_path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
    }
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let signals = signal_fd()?;
    let timer = timerfd_create(
        TimerfdClockId::Monotonic,
        TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
    )
    .map_err(|e| format!("timerfd_create failed: {e}"))?;

    let mut daemon = Daemon {
        wl,
        pending: Vec::new(),
        outputs: Vec::new(),
        players: Vec::new(),
        next_player: 1,
        transitions: Vec::new(),
        next_transition: 1,
        paused: false,
        exit: None,
        state: State::load(),
        restoring: true,
        jobs: Vec::new(),
        clients: Vec::new(),
        next_client: 1,
        timer,
        armed: Some(None),
    };
    for (global, version) in output_globals {
        daemon.bind_output(global, version);
    }

    // restore the state: find the cache file of each image, or decode it again
    let images: Vec<(String, String, Anchor)> = daemon
        .state
        .entries
        .iter()
        .filter_map(|e| match &e.spec {
            Spec::Img { path, anchor, .. } => Some((e.target.clone(), path.clone(), *anchor)),
            Spec::Color(_) => None,
        })
        .collect();
    for (target, path, anchor) in images {
        if let Err(e) = daemon.start_img(&path, anchor, &target, None, false, None) {
            eprintln!("pixelwp: {e}");
        }
    }
    daemon.restoring = false;
    daemon.gc();

    let mut fds: Vec<PollFd<'static>> = Vec::with_capacity(4 + MAX_CLIENTS + MAX_JOBS);
    let mut ready_clients: Vec<u64> = Vec::with_capacity(MAX_CLIENTS);
    let mut ready_jobs: Vec<u32> = Vec::with_capacity(MAX_JOBS);
    // SAFETY: `fds` is cleared and filled again before each poll, so no entry is
    // used after its fd is closed
    let borrow = |fd: BorrowedFd<'_>| unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };

    let code = loop {
        daemon.run_transitions();
        daemon.run_players();
        daemon.arm_timer();
        let flushed = daemon.wl.flush();
        if let Some(e) = flushed.err().or(daemon.wl.error) {
            eprintln!("pixelwp: the Wayland connection failed: {e}");
            break 1;
        }
        if let Some(code) = daemon.exit {
            break code;
        }

        fds.clear();
        fds.push(PollFd::from_borrowed_fd(
            borrow(daemon.wl.fd().as_fd()),
            PollFlags::IN,
        ));
        fds.push(PollFd::from_borrowed_fd(
            borrow(listener.as_fd()),
            PollFlags::IN,
        ));
        fds.push(PollFd::from_borrowed_fd(
            borrow(signals.as_fd()),
            PollFlags::IN,
        ));
        fds.push(PollFd::from_borrowed_fd(
            borrow(daemon.timer.as_fd()),
            PollFlags::IN,
        ));
        for c in &daemon.clients {
            // no events for a client that waits for its reply
            let flags = if c.waiting {
                PollFlags::empty()
            } else {
                PollFlags::IN
            };
            fds.push(PollFd::from_borrowed_fd(borrow(c.stream.as_fd()), flags));
        }
        for j in &daemon.jobs {
            fds.push(PollFd::from_borrowed_fd(
                borrow(j.pidfd.as_fd()),
                PollFlags::IN,
            ));
        }
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(format!("poll failed: {e}")),
        }

        let hit = |fd: &PollFd| {
            fd.revents()
                .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
        };
        let (wayland_hit, listener_hit, signal_hit, timer_hit) =
            (hit(&fds[0]), hit(&fds[1]), hit(&fds[2]), hit(&fds[3]));
        let wayland_gone = fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR);
        ready_clients.clear();
        ready_jobs.clear();
        let n = daemon.clients.len();
        ready_clients.extend(
            daemon
                .clients
                .iter()
                .zip(&fds[4..])
                .filter(|(_, fd)| hit(fd))
                .map(|(c, _)| c.id),
        );
        ready_jobs.extend(
            daemon
                .jobs
                .iter()
                .zip(&fds[4 + n..])
                .filter(|(_, fd)| hit(fd))
                .map(|(j, _)| j.child.id()),
        );

        if signal_hit {
            break 0;
        }
        if timer_hit {
            daemon.armed = None;
        }
        if wayland_hit {
            let mut msgs = match receiver.recv(daemon.wl.fd()) {
                Ok(msgs) => msgs,
                Err(e) => {
                    eprintln!("pixelwp: cannot read from the compositor: {e}");
                    break 1;
                }
            };
            while let Some(sender_id) = msgs.next() {
                let Some(sender) = sender_id.ok().and_then(|id| daemon.wl.objman.get(id)) else {
                    continue;
                };
                dispatch!(
                    daemon,
                    sender,
                    msgs,
                    (Display, wl_display),
                    (Registry, wl_registry),
                    (Callback, wl_callback),
                    (Compositor, wl_compositor),
                    (Shm, wl_shm),
                    (ShmPool, wl_shm_pool),
                    (Buffer, wl_buffer),
                    (Surface, wl_surface),
                    (Region, wl_region),
                    (Output, wl_output),
                    (LayerShell, zwlr_layer_shell_v1),
                    (LayerSurface, zwlr_layer_surface_v1),
                    (Viewporter, wp_viewporter),
                    (Viewport, wp_viewport),
                    (FractionalScaler, wp_fractional_scale_manager_v1),
                    (FractionalScale, wp_fractional_scale_v1),
                );
            }
            drop(msgs);
            if wayland_gone {
                eprintln!("pixelwp: the compositor closed the connection");
                break 1;
            }
            // Events come in batches, so one build for each changed output is sufficient.
            for i in 0..daemon.outputs.len() {
                if daemon.outputs[i].dirty {
                    daemon.rebuild(i);
                }
            }
        }
        if listener_hit {
            daemon.accept(&listener);
        }
        for id in &ready_clients {
            daemon.read_client(*id);
        }
        for pid in &ready_jobs {
            daemon.finish_job(*pid);
        }
    };

    for job in &mut daemon.jobs {
        _ = job.child.kill();
        _ = job.child.wait();
    }
    _ = std::fs::remove_file(&socket_path);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        let text = "all img /home/u/a b.gif\nDP-1 color ff8800\nbad line\nDP-2 what x\nDP-1 color 000001\n\
                    DP-3 img topleft /c d.png\nDP-4 img nowhere /e.png\n";
        let mut state = State {
            entries: State::parse(text),
            path: PathBuf::new(),
        };
        assert_eq!(state.entries.len(), 3);
        assert_eq!(
            state.format(),
            "all img center /home/u/a b.gif\nDP-1 color 000001\nDP-3 img topleft /c d.png\n"
        );
        assert_eq!(State::parse(&state.format()), state.entries);

        // a new output uses its own line, or else the `all` line
        assert_eq!(state.lookup("DP-1").unwrap().spec, Spec::Color(0xFF00_0001));
        assert_eq!(state.lookup("HDMI-A-1").unwrap().target, "all");

        state.set("DP-1", Spec::Color(BLACK));
        assert_eq!(state.entries.len(), 3);
        // `all` replaces all lines
        state.set("all", Spec::Color(BLACK));
        assert_eq!(state.format(), "all color 000000\n");
        assert!(State::parse("").is_empty());
    }
}
