# waypixel

> [!IMPORTANT]
> **waypixel is a minimal wallpaper daemon for pixel-perfect pixel art.**
> It is for wlroots-type Wayland compositors (`zwlr_layer_shell_v1`: Sway, Hyprland, niri).
> No feature requests and no support. Fork it.

> [!NOTE]
> waypixel accepts only true pixel art: an N× integer upscale, or a 1:1 image of
> no more than 640 px per side and 256 colours. It rejects all other images:
> `waypixel img` prints an error and the wallpaper does not change.
> `waypixel check <dir>` lists the images in a folder that it rejects.

Performance features:
- **Small files.** GIFs are automatically downscaled and cached at 1x size. Only the pixels that change between frames are saved.
- **Low memory.** A separate process decodes the image and then exits. The daemon keeps only the optimized cache.
- **Low CPU.** On each frame, only the changed regions of the screen are redrawn.
- **Fast runtime scale to target resolution.** Pixel art needs no filters, true integer scaling.

The full design is in [SPEC.md](SPEC.md).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/olli-io/waypixel/main/install.sh | sh
```

The script downloads the latest released binary (x86_64 or aarch64), copies it
to `~/.local/bin`, and installs, enables and starts the systemd user unit.
Set `BINDIR` for a different directory. Set `VERSION=vX.Y.Z` for a specific release.

## Build

Requirements: Rust 1.85 or later, `wayland` and `wayland-protocols`.

```sh
cargo install --locked --path .    # installs to ~/.cargo/bin
```

## Use

```sh
waypixel daemon &                          # start the daemon (foreground process)
waypixel img ~/wallpapers/forest.gif       # all outputs
waypixel img city.png --output DP-1        # one output
waypixel img "$(fzf)"                      # select a file with a shell tool
waypixel clear --color 202020              # solid colour (default 000000)
waypixel img forest.gif --fx wipe          # transition for this change only
waypixel img forest.gif --anchor bottom    # keep the bottom edge in view
waypixel check ~/wallpapers                # list the images that are not pixel art
waypixel query                             # <name> <w>x<h> <file|color #RRGGBB> <state> [<anchor>]
waypixel still ~/lock.png                  # first frame of the largest output, as a PNG
waypixel still ~/lock.png --output DP-1    # first frame of one output
waypixel pause
waypixel resume
waypixel kill
```

`<state>` is `playing`, `hidden`, `paused` or `static`.

The scale fills the screen and cuts the overflow. `--anchor <name>` sets the part
of the image that stays in view: `center` (default), `top`, `bottom`, `left`,
`right`, `topleft`, `topright`, `bottomleft` or `bottomright`. `query` shows an
anchor only when it is not `center`.

## Transitions

| Name | Effect |
|---|---|
| `burn` (default) | A fire spreads from a random point. Smoke rises. |
| `sweep` | A grey shimmer goes from right to left. A colour shimmer comes back and shows the new wallpaper. |
| `slide` | The new wallpaper comes in from the left, in bands. |
| `wipe` | A diagonal colour band goes from the top left to the bottom right. |
| `disabled` | No transition. |

Set the default in `~/.config/waypixel/config` (`$XDG_CONFIG_HOME/waypixel/config`):

```
transition = sweep
```

The daemon reads the file at each change, so no restart is necessary. `--fx <name>`
has priority over the file.

An animation plays during the transition, for the old and for the new
wallpaper.

Exit codes: 0 = ok, 1 = error, 2 = usage error, 3 = the daemon is not running.

The daemon keeps the last wallpaper of each output in
`$XDG_STATE_HOME/waypixel/state` and shows it again at the next start. Decoded
images are in `$XDG_CACHE_HOME/waypixel/`. The daemon deletes the cache files
that no output uses.

## Lock screen

A session lock hides the wallpaper, and lock screens such as hyprlock do not
play a GIF. `waypixel still` writes the first frame as a PNG, with the same
integer scale as the wallpaper. Run it after each `waypixel img`, and give the
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
cp waypixel.service ~/.config/systemd/user/    # edit ExecStart if the binary is not in ~/.local/bin
systemctl --user daemon-reload
systemctl --user enable --now waypixel.service
```

The unit starts `~/.local/bin/waypixel daemon` with `graphical-session.target`.
The compositor must put `WAYLAND_DISPLAY` into the systemd user environment.
Logs: `journalctl --user -u waypixel`.

## Tests and measurements

```sh
cargo test                       # unit tests + tests against a mock compositor
cargo test --release bench_fill_4k -- --ignored --nocapture
grep Rss /proc/$(pidof waypixel)/smaps_rollup
pidstat -p $(pidof waypixel) 1
```
