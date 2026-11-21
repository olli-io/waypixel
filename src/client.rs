//! Socket client: sends one request and prints the reply.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use crate::ipc;
use crate::pixelart::NOT_PIXEL_ART;

/// Returns the exit code: 0 = ok, 1 = error, 3 = the daemon is not running.
pub fn request(args: &[&str]) -> i32 {
    match fetch(args) {
        Ok(body) => {
            print!("{body}");
            0
        }
        Err(code) => code,
    }
}

/// Returns the reply body. On an error, prints the text and returns the exit code.
pub fn fetch(args: &[&str]) -> Result<String, i32> {
    let Ok(mut stream) = UnixStream::connect(ipc::socket_path()) else {
        eprintln!("waypixel: the daemon is not running");
        return Err(3);
    };
    let mut reply = String::new();
    let io = stream
        .write_all(&ipc::encode(args))
        .and_then(|()| stream.shutdown(Shutdown::Write))
        .and_then(|()| stream.read_to_string(&mut reply));
    if let Err(e) = io {
        eprintln!("waypixel: {e}");
        return Err(1);
    }
    let (status, body) = reply.split_once('\n').unwrap_or((&reply, ""));
    if status == "ok" {
        Ok(body.to_owned())
    } else {
        let text = status
            .strip_prefix("error ")
            .unwrap_or("bad reply from the daemon");
        eprintln!("waypixel: {text}");
        if text == NOT_PIXEL_ART {
            notify(args.get(1).copied().unwrap_or_default(), text);
        }
        Err(1)
    }
}

/// Shows a desktop notification with `notify-send`. Does nothing when that
/// program is not there.
fn notify(path: &str, text: &str) {
    let name = path.rsplit('/').next().unwrap_or(path);
    _ = Command::new("notify-send")
        .args(["--app-name=waypixel", "waypixel"])
        .arg(format!("{name}: {text}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
