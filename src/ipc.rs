//! Socket path, request and reply format.
//!
//! One connection carries one request and one reply. The request is the arguments,
//! each ended by a NUL byte. The reply is text: the first line is `ok` or
//! `error <text>`.

use std::path::{Path, PathBuf};

/// `$<var>`, or `$HOME/<fallback>` when the variable is not set.
pub fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback),
    }
}

pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(format!("/run/user/{}", rustix::process::getuid().as_raw())),
    }
}

pub fn wayland_display() -> std::ffi::OsString {
    match std::env::var_os("WAYLAND_DISPLAY") {
        Some(name) if !name.is_empty() => name,
        _ => "wayland-0".into(),
    }
}

/// `$XDG_RUNTIME_DIR/waypixel-<WAYLAND_DISPLAY basename>.sock`
pub fn socket_path() -> PathBuf {
    let display = wayland_display();
    let base = Path::new(&display)
        .file_name()
        .unwrap_or(display.as_os_str())
        .to_string_lossy()
        .into_owned();
    runtime_dir().join(format!("waypixel-{base}.sock"))
}

pub fn encode(args: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in args {
        out.extend_from_slice(arg.as_bytes());
        out.push(0);
    }
    out
}

/// Returns `None` when the request is empty, is not UTF-8, or has no final NUL.
pub fn decode(bytes: &[u8]) -> Option<Vec<&str>> {
    let body = bytes.strip_suffix(&[0])?;
    body.split(|b| *b == 0)
        .map(|arg| std::str::from_utf8(arg).ok())
        .collect()
}

/// Parses `RRGGBB` or `#RRGGBB` to XRGB8888.
pub fn parse_color(hex: &str) -> Option<u32> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(|c| 0xFF00_0000 | c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let bytes = encode(&["img", "/a b.gif", "all"]);
        assert_eq!(bytes, b"img\0/a b.gif\0all\0");
        assert_eq!(decode(&bytes).unwrap(), ["img", "/a b.gif", "all"]);
        assert!(decode(b"").is_none());
        assert!(decode(b"img").is_none());
        assert!(decode(b"\xff\0").is_none());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("000000"), Some(0xFF00_0000));
        assert_eq!(parse_color("#1a2B3c"), Some(0xFF1A_2B3C));
        assert_eq!(parse_color("fff"), None);
        assert_eq!(parse_color("+12345"), None);
        assert_eq!(parse_color("gggggg"), None);
    }
}
