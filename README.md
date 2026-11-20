# pixelwp

A minimal wallpaper daemon for animated pixel art on wlroots-type Wayland
compositors (compositors with `zwlr_layer_shell_v1`: Sway, Hyprland, river, niri, ...).

- Pixel art (GIF, APNG, animated WebP, or a still) always gets an integer scale.
  The image is centred and the overflow is cut.
- Other images are stills only. The compositor scales them (aspect fill).
- The daemon keeps the art at 1× and draws only the changed areas on each frame.
- The animation stops when the compositor does not show the wallpaper.

The full design is in [SPEC.md](SPEC.md).

## Install

Requirements: Rust, and `wayland-protocols` (for the build).

```sh
./install.sh
```

The script builds the release binary, copies it to `~/.cargo/bin` (set `BINDIR`
for a different directory), and installs, enables and starts the systemd user
unit. To install only the binary: `cargo install --path .`

## Use

```sh
pixelwp daemon &                          # start the daemon (foreground process)
pixelwp img ~/wallpapers/forest.gif       # all outputs
pixelwp img city.png --output DP-1        # one output
pixelwp img "$(fzf)"                      # select a file with a shell tool
pixelwp clear --color 202020              # solid colour (default 000000)
pixelwp query                             # <name> <w>x<h> <file|color #RRGGBB> <state>
pixelwp pause
pixelwp resume
pixelwp kill
```

`<state>` is `playing`, `hidden`, `paused` or `static`.

Exit codes: 0 = ok, 1 = error, 2 = usage error, 3 = the daemon is not running.

The daemon keeps the last wallpaper of each output in
`$XDG_STATE_HOME/pixelwp/state` and shows it again at the next start. Decoded
images are in `$XDG_CACHE_HOME/pixelwp/`. The daemon deletes the cache files
that no output uses.

## Start at login

`install.sh` does this. The manual procedure:

```sh
mkdir -p ~/.config/systemd/user
cp pixelwp.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now pixelwp.service
```

The unit starts `~/.cargo/bin/pixelwp daemon` with `graphical-session.target`.
The compositor must put `WAYLAND_DISPLAY` into the systemd user environment.
Logs: `journalctl --user -u pixelwp`.

## Tests and measurements

```sh
cargo test                       # unit tests + tests against a mock compositor
cargo test --release bench_fill_4k -- --ignored --nocapture
grep Rss /proc/$(pidof pixelwp)/smaps_rollup
pidstat -p $(pidof pixelwp) 1
```
