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
pixelwp img forest.gif --fx wipe          # transition for this change only
pixelwp img forest.gif --anchor bottom    # keep the bottom edge in view
pixelwp query                             # <name> <w>x<h> <file|color #RRGGBB> <state> [<anchor>]
pixelwp still ~/lock.png                  # first frame of the largest output, as a PNG
pixelwp still ~/lock.png --output DP-1    # first frame of one output
pixelwp pause
pixelwp resume
pixelwp kill
```

`<state>` is `playing`, `hidden`, `paused` or `static`.

The scale fills the screen and cuts the overflow. `--anchor <name>` sets the part
of the image that stays in view: `center` (default), `top`, `bottom`, `left`,
`right`, `topleft`, `topright`, `bottomleft` or `bottomright`. `query` shows an
anchor only when it is not `center`.

## Transitions

A change of wallpaper shows a transition, after the effects of
[ttfx](https://github.com/omacom/ttfx). A transition runs on a grid of
540 × 270 cells, which gets an integer scale as pixel art does.

| Name | Effect |
|---|---|
| `burn` (default) | A fire spreads from a random point. Smoke rises. |
| `sweep` | A grey shimmer goes from right to left. A colour shimmer comes back and shows the new wallpaper. |
| `slide` | The new wallpaper comes in from the left, in bands. |
| `wipe` | A diagonal colour band goes from the top left to the bottom right. |
| `disabled` | No transition. |

Set the default in `~/.config/pixelwp/config` (`$XDG_CONFIG_HOME/pixelwp/config`):

```
transition = sweep
```

The daemon reads the file at each change, so no restart is necessary. `--fx <name>`
has priority over the file. There is no transition when the old or the new
image is not pixel art.

An animation plays during the transition, for the old and for the new
wallpaper.

Exit codes: 0 = ok, 1 = error, 2 = usage error, 3 = the daemon is not running.

The daemon keeps the last wallpaper of each output in
`$XDG_STATE_HOME/pixelwp/state` and shows it again at the next start. Decoded
images are in `$XDG_CACHE_HOME/pixelwp/`. The daemon deletes the cache files
that no output uses.

## Lock screen

A session lock hides the wallpaper, and lock screens such as hyprlock do not
play a GIF. `pixelwp still` writes the first frame as a PNG, with the same
integer scale as the wallpaper. Run it after each `pixelwp img`, and give the
file to the lock screen:

```
# hyprlock.conf
background {
    path = ~/lock.png
}
```

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
