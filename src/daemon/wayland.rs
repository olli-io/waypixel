//! Generated protocol code, the connection, and the request context.

#![allow(clippy::all, unused)]
include!(concat!(env!("OUT_DIR"), "/wayland_protocols.rs"));

use waybackend::{Waybackend, objman::ObjectManager, types::ObjectId, wire::Receiver};

/// The type of each Wayland object that the daemon has.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Obj {
    Display,
    Registry,
    Callback,
    Compositor,
    Shm,
    ShmPool,
    Buffer,
    Surface,
    Region,
    Output,
    LayerShell,
    LayerSurface,
    Viewporter,
    Viewport,
    FractionalScaler,
    FractionalScale,
}

/// The connection and the globals. All requests go through `req!`.
pub struct Wl {
    pub(super) backend: Waybackend,
    pub objman: ObjectManager<Obj>,
    /// true when the message buffer has requests that are not sent
    pub(super) dirty: bool,
    /// first error from a request
    pub error: Option<rustix::io::Errno>,
    pub registry: ObjectId,
    pub compositor: ObjectId,
    pub shm: ObjectId,
    pub viewporter: ObjectId,
    pub layer_shell: ObjectId,
    pub fractional: Option<ObjectId>,
}

impl Wl {
    pub fn new(
        backend: Waybackend,
        objman: ObjectManager<Obj>,
        registry: ObjectId,
    ) -> Result<Self, String> {
        let get = |obj, name: &str| {
            objman
                .get_first(obj)
                .ok_or(format!("the compositor does not have {name}"))
        };
        Ok(Self {
            registry,
            compositor: get(Obj::Compositor, wl_compositor::NAME)?,
            shm: get(Obj::Shm, wl_shm::NAME)?,
            viewporter: get(Obj::Viewporter, wp_viewporter::NAME)?,
            layer_shell: get(Obj::LayerShell, zwlr_layer_shell_v1::NAME)?,
            fractional: objman.get_first(Obj::FractionalScaler),
            backend,
            objman,
            dirty: false,
            error: None,
        })
    }

    pub fn fd(&self) -> &rustix::fd::OwnedFd {
        &self.backend.wayland_fd
    }

    /// Sends the queued requests, if there are any.
    pub fn flush(&mut self) -> rustix::io::Result<()> {
        if self.dirty {
            self.dirty = false;
            self.backend.flush()?;
        }
        Ok(())
    }
}

/// Sends a request and records the error, if there is one.
macro_rules! req {
    ($wl:expr, $($path:ident)::+ ( $($arg:expr),* $(,)? )) => {{
        $wl.dirty = true;
        if let Err(e) = $crate::daemon::wayland::$($path)::+(&mut $wl.backend, $($arg),*) {
            $wl.error.get_or_insert(e);
        }
    }};
}
pub(crate) use req;

pub fn connect() -> Result<(Waybackend, ObjectManager<Obj>, Receiver), String> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, socket_with};
    let display = crate::ipc::wayland_display();
    let path = crate::ipc::runtime_dir().join(&display);
    let addr = SocketAddrUnix::new(&path).map_err(|e| e.to_string())?;
    let socket = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| e.to_string())?;
    waybackend::connect_to(Obj::Display, socket, &addr)
        .map_err(|e| format!("cannot connect to {}: {e}", path.display()))
}
