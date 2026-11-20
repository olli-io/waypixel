//! pixelwp: animated pixel-art wallpapers for wlroots-type Wayland compositors.
//!
//! One binary with three roles: the daemon, the client, and the decode child.

mod cache;
mod client;
mod daemon;
mod decode;
mod diff;
mod ipc;
mod pixelart;
mod scale;

use std::process::ExitCode;

const USAGE: &str = "\
Usage:
  pixelwp daemon                                 start the daemon in the foreground
  pixelwp img <path> [--output <name|all>]       set the wallpaper (default: all)
  pixelwp clear [--color <hex>] [--output ...]   solid colour (default: 000000)
  pixelwp query                                  one line for each output
  pixelwp pause | resume                         manual pause
  pixelwp kill                                   stop the daemon

Exit codes: 0 = ok, 1 = error, 2 = usage error, 3 = the daemon is not running";

#[derive(Debug, PartialEq)]
enum Cmd {
    Daemon,
    Img {
        path: String,
        output: String,
    },
    Clear {
        color: String,
        output: String,
    },
    Query,
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

/// Positional arguments and the values of `--output` and `--color`.
#[derive(Default)]
struct Parsed {
    positional: Vec<String>,
    output: Option<String>,
    color: Option<String>,
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
            let p = parse_options(rest, &["--output"])?;
            let [path] = p.positional.as_slice() else {
                return Err("img needs one path".into());
            };
            Ok(Cmd::Img {
                path: path.clone(),
                output: p.output.unwrap_or_else(|| "all".into()),
            })
        }
        "clear" => {
            let p = parse_options(rest, &["--output", "--color"])?;
            if !p.positional.is_empty() {
                return Err("clear takes no path".into());
            }
            let color = p.color.unwrap_or_else(|| "000000".into());
            if ipc::parse_color(&color).is_none() {
                return Err(format!("bad colour {color}: use RRGGBB"));
            }
            Ok(Cmd::Clear {
                color,
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
            eprintln!("pixelwp: {e}\n{USAGE}");
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
        Cmd::Img { path, output } => {
            // the daemon has a different working directory
            match std::fs::canonicalize(&path) {
                Ok(abs) => match abs.to_str() {
                    Some(abs) => client::request(&["img", abs, &output]),
                    None => {
                        eprintln!("pixelwp: {path}: the path is not UTF-8");
                        1
                    }
                },
                Err(e) => {
                    eprintln!("pixelwp: {path}: {e}");
                    1
                }
            }
        }
        Cmd::Clear { color, output } => client::request(&["clear", &color, &output]),
        Cmd::Query => client::request(&["query"]),
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
            })
        };
        assert_eq!(p(&["img", "a b.gif"]), img("a b.gif", "all"));
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
            })
        };
        assert_eq!(p(&["clear"]), clear("000000", "all"));
        assert_eq!(
            p(&["clear", "--color", "ff8800", "--output", "DP-2"]),
            clear("ff8800", "DP-2")
        );

        assert!(p(&[]).is_err());
        assert!(p(&["img"]).is_err());
        assert!(p(&["img", "a", "b"]).is_err());
        assert!(p(&["img", "a", "--color", "000000"]).is_err());
        assert!(p(&["img", "a", "--output"]).is_err());
        assert!(p(&["clear", "--color", "red"]).is_err());
        assert!(p(&["query", "x"]).is_err());
        assert!(p(&["frobnicate"]).is_err());
    }
}
