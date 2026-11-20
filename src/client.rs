//! Socket client: sends one request and prints the reply.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use crate::ipc;

/// Returns the exit code: 0 = ok, 1 = error, 3 = the daemon is not running.
pub fn request(args: &[&str]) -> i32 {
    let Ok(mut stream) = UnixStream::connect(ipc::socket_path()) else {
        eprintln!("pixelwp: the daemon is not running");
        return 3;
    };
    let mut reply = String::new();
    let io = stream
        .write_all(&ipc::encode(args))
        .and_then(|()| stream.shutdown(Shutdown::Write))
        .and_then(|()| stream.read_to_string(&mut reply));
    if let Err(e) = io {
        eprintln!("pixelwp: {e}");
        return 1;
    }
    let (status, body) = reply.split_once('\n').unwrap_or((&reply, ""));
    if status == "ok" {
        print!("{body}");
        0
    } else {
        let text = status
            .strip_prefix("error ")
            .unwrap_or("bad reply from the daemon");
        eprintln!("pixelwp: {text}");
        1
    }
}
