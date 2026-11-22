//! waypixel: animated pixel-art wallpapers for wlroots-type Wayland compositors.
//!
//! One binary with three roles: the daemon, the client, and the decode child.

mod cache;
mod check;
mod client;
mod daemon;
mod decode;
mod diff;
mod fx;
mod ipc;
mod pixelart;
mod scale;
mod still;

use std::process::ExitCode;

const USAGE: &str = "\
Usage:
  waypixel daemon                                 start the daemon in the foreground
  waypixel img <path> [--output <name|all>] [--fx <name>] [--anchor <name>]
                                                 set the wallpaper (default: all)
  waypixel clear [--color <hex>] [--output ...] [--fx ...]
                                                 solid colour (default: 000000)
  waypixel query                                  one line for each output
  waypixel check [<dir>]                          list the images that are not pixel art
                                                 (default: the current directory)
  waypixel still <file.png> [--output <name>]     write the wallpaper of one output as a PNG
                                                 (default: the largest output)
  waypixel pause | resume                         manual pause
  waypixel kill                                   stop the daemon

--fx is the transition: burn, sweep, slide, wipe, decrypt, pour,
smoke, vhstape, rain or disabled. The default is the `transition` key in
~/.config/waypixel/config, or else burn.

--anchor is the part of the image that stays in view when the scale cuts the
overflow: center (default), top, bottom, left, right, topleft, topright,
bottomleft or bottomright.

Exit codes: 0 = ok, 1 = error, 2 = usage error, 3 = the daemon is not running";

#[derive(Debug, PartialEq)]
enum Cmd {
    Daemon,
    Img {
        path: String,
        output: String,
        fx: String,
        anchor: String,
    },
    Clear {
        color: String,
        output: String,
        fx: String,
    },
    Query,
    Check {
        dir: String,
    },
    Still {
        path: String,
        output: Option<String>,
    },
    Pause,
    Resume,
    Kill,
    /// internal: the decode child
    Decode {
        src: String,
        dst: String,
    },
    Help,
}

/// Positional arguments and the values of the options.
#[derive(Default)]
struct Parsed {
    positional: Vec<String>,
    output: Option<String>,
    color: Option<String>,
    fx: Option<String>,
    anchor: Option<String>,
}

impl Parsed {
    /// The name of the transition. `config` (default): the daemon reads the config file.
    fn fx(&self) -> Result<String, String> {
        match self.fx.as_deref() {
            None => Ok("config".into()),
            Some(name) if fx::Kind::parse(name).is_some() => Ok(name.into()),
            Some(other) => Err(format!("bad transition {other}: use {}", fx::Kind::NAMES)),
        }
    }
}

fn parse_options(args: &[String], allowed: &[&str]) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut it = args.iter();
    let mut only_positional = false;
    while let Some(arg) = it.next() {
        if only_positional || !arg.starts_with('-') || arg == "-" {
            parsed.positional.push(arg.clone());
            continue;
        }
        if arg == "--" {
            only_positional = true;
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let name = match name {
            "-o" => "--output",
            "-c" => "--color",
            other => other,
        };
        if !allowed.contains(&name) {
            return Err(format!("unknown option {arg}"));
        }
        let value = match inline {
            Some(value) => value,
            None => it.next().ok_or(format!("{name} needs a value"))?.clone(),
        };
        match name {
            "--output" => parsed.output = Some(value),
            "--fx" => parsed.fx = Some(value),
            "--anchor" => parsed.anchor = Some(value),
            _ => parsed.color = Some(value),
        }
    }
    Ok(parsed)
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err("no command".into());
    };
    let no_args = |cmd: Cmd| {
        if rest.is_empty() {
            Ok(cmd)
        } else {
            Err(format!("{} takes no arguments", args[0]))
        }
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => Ok(Cmd::Help),
        "daemon" => no_args(Cmd::Daemon),
        "query" => no_args(Cmd::Query),
        "pause" => no_args(Cmd::Pause),
        "resume" => no_args(Cmd::Resume),
        "kill" => no_args(Cmd::Kill),
        "img" => {
            let p = parse_options(rest, &["--output", "--fx", "--anchor"])?;
            let [path] = p.positional.as_slice() else {
                return Err("img needs one path".into());
            };
            let anchor = p.anchor.clone().unwrap_or_else(|| "center".into());
            if scale::Anchor::parse(&anchor).is_none() {
                let names = scale::Anchor::NAMES;
                return Err(format!("bad anchor {anchor}: use {names}"));
            }
            Ok(Cmd::Img {
                path: path.clone(),
                anchor,
                fx: p.fx()?,
                output: p.output.unwrap_or_else(|| "all".into()),
            })
        }
        "check" => match rest {
            [] => Ok(Cmd::Check { dir: ".".into() }),
            [dir] => Ok(Cmd::Check { dir: dir.clone() }),
            _ => Err("check takes one directory".into()),
        },
        "still" => {
            let p = parse_options(rest, &["--output"])?;
            let [path] = p.positional.as_slice() else {
                return Err("still needs one path".into());
            };
            Ok(Cmd::Still {
                path: path.clone(),
                output: p.output,
            })
        }
        "clear" => {
            let p = parse_options(rest, &["--output", "--color", "--fx"])?;
            if !p.positional.is_empty() {
                return Err("clear takes no path".into());
            }
            let color = p.color.clone().unwrap_or_else(|| "000000".into());
            if ipc::parse_color(&color).is_none() {
                return Err(format!("bad colour {color}: use RRGGBB"));
            }
            Ok(Cmd::Clear {
                color,
                fx: p.fx()?,
                output: p.output.unwrap_or_else(|| "all".into()),
            })
        }
        "decode" => match rest {
            [src, dst] => Ok(Cmd::Decode {
                src: src.clone(),
                dst: dst.clone(),
            }),
            _ => Err("decode needs <src> <dst>".into()),
        },
        other => Err(format!("unknown command {other}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse(&args) {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("waypixel: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let code = match cmd {
        Cmd::Help => {
            println!("{USAGE}");
            0
        }
        Cmd::Daemon => daemon::run(),
        Cmd::Decode { src, dst } => {
            daemon::reset_signals();
            match decode::run(src.as_ref(), dst.as_ref()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        Cmd::Img {
            path,
            output,
            fx,
            anchor,
        } => {
            // the daemon has a different working directory
            match std::fs::canonicalize(&path) {
                Ok(abs) => match abs.to_str() {
                    Some(abs) => client::request(&["img", abs, &output, &fx, &anchor]),
                    None => {
                        eprintln!("waypixel: {path}: the path is not UTF-8");
                        1
                    }
                },
                Err(e) => {
                    eprintln!("waypixel: {path}: {e}");
                    1
                }
            }
        }
        Cmd::Clear { color, output, fx } => client::request(&["clear", &color, &output, &fx]),
        Cmd::Query => client::request(&["query"]),
        Cmd::Check { dir } => check::run(dir.as_ref()),
        Cmd::Still { path, output } => {
            // the daemon starts this child for the lock screen image
            daemon::reset_signals();
            still::run(path.as_ref(), output.as_deref())
        }
        Cmd::Pause => client::request(&["pause"]),
        Cmd::Resume => client::request(&["resume"]),
        Cmd::Kill => client::request(&["kill"]),
    };
    ExitCode::from(code as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Cmd, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn cli() {
        assert_eq!(p(&["daemon"]), Ok(Cmd::Daemon));
        assert_eq!(p(&["query"]), Ok(Cmd::Query));
        assert_eq!(p(&["kill"]), Ok(Cmd::Kill));
        let img = |path: &str, output: &str| {
            Ok(Cmd::Img {
                path: path.into(),
                output: output.into(),
                fx: "config".into(),
                anchor: "center".into(),
            })
        };
        assert_eq!(p(&["img", "a b.gif"]), img("a b.gif", "all"));
        assert!(matches!(
            p(&["img", "a.gif", "--anchor", "top-left"]),
            Ok(Cmd::Img { anchor, .. }) if anchor == "top-left"
        ));
        assert!(p(&["img", "a", "--anchor", "middle"]).is_err());
        assert!(p(&["clear", "--anchor", "top"]).is_err());
        assert_eq!(
            p(&["img", "a.gif", "--output", "DP-1"]),
            img("a.gif", "DP-1")
        );
        assert_eq!(p(&["img", "--output=DP-1", "a.gif"]), img("a.gif", "DP-1"));
        assert_eq!(
            p(&["img", "-o", "DP-1", "--", "-a.gif"]),
            img("-a.gif", "DP-1")
        );
        let clear = |color: &str, output: &str| {
            Ok(Cmd::Clear {
                color: color.into(),
                output: output.into(),
                fx: "config".into(),
            })
        };
        assert_eq!(p(&["clear"]), clear("000000", "all"));
        assert_eq!(
            p(&["clear", "--color", "ff8800", "--output", "DP-2"]),
            clear("ff8800", "DP-2")
        );

        assert_eq!(
            p(&["still", "a.png", "-o", "DP-1"]),
            Ok(Cmd::Still {
                path: "a.png".into(),
                output: Some("DP-1".into()),
            })
        );
        assert_eq!(p(&["check"]), Ok(Cmd::Check { dir: ".".into() }));
        assert_eq!(p(&["check", "art"]), Ok(Cmd::Check { dir: "art".into() }));
        assert!(p(&["check", "a", "b"]).is_err());
        assert!(p(&["still"]).is_err());
        assert!(p(&[]).is_err());
        assert!(p(&["img"]).is_err());
        assert!(p(&["img", "a", "b"]).is_err());
        assert!(p(&["img", "a", "--color", "000000"]).is_err());
        assert!(p(&["img", "a", "--output"]).is_err());
        assert!(p(&["img", "a", "--fx", "melt"]).is_err());
        assert!(
            matches!(p(&["clear", "--fx=disabled"]), Ok(Cmd::Clear { fx, .. }) if fx == "disabled")
        );
        assert!(p(&["clear", "--color", "red"]).is_err());
        assert!(p(&["query", "x"]).is_err());
        assert!(p(&["frobnicate"]).is_err());
    }
}
