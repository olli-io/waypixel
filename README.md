# waypixel

> [!IMPORTANT]
> **A minimal wallpaper daemon for pixel-perfect pixel art** on wlroots-type. Wayland compositors (`zwlr_layer_shell_v1`: Sway, Hyprland, niri).
> No feature requests, the goal is to keep it minimal with no feature creep. Feel absolute free to fork it for more features.

waypixel accepts only true pixel art: N× integer upscales or a 1x pixel art with
max 640 px per side and 256 colours. For non-pixelart images, waypixel prints an
error and the wallpaper does not change.

Performance:
- **Optimized cache.** GIFs are downscaled to 1× and only changed pixels are stored.
- **Minimal daemon** A separate process decodes the image and exits.
- **Animation-optimized** Only changed screen regions are redrawn.
- **Integer scaling.** No filters.

Benchmark againt awww 0.12.1: `kirokaze-pastime.gif` (960×540, 144 frames) on
2× 3840×2160@180 Hz and 1× 1920×1080@60 Hz. awww gets the GIF
upscaled 4× to 3840×2160 for the same sharp scaling.

| | waypixel | awww | Improvement |
|---|---|---|---|
| Daemon CPU (% of one core) | 0.49 | 9.38 | 19× |
| Compositor CPU (hyprland, % of one core) | 2.90 | 15.73 | 5.4× |
| Daemon RAM (PSS, MiB) | 38 | 74 | 1.9× |

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/olli-io/waypixel/main/install.sh | sh
```

Installs the latest binary (x86_64, aarch64) to `~/.local/bin` and starts the
systemd user unit. Set `BINDIR` or `VERSION=vX.Y.Z` to change.

Build from source (Rust 1.85+, `wayland`, `wayland-protocols`):

```sh
cargo install --locked --path .
```

## Use

```sh
waypixel daemon
waypixel img forest.gif                    # all outputs
waypixel img city.png --output DP-1        # one output
waypixel img forest.gif --fx wipe          # transition for this change only
waypixel img forest.gif --anchor bottom    # keep the bottom edge in view
waypixel clear --color 202020              # solid colour (default 000000)
waypixel check ~/wallpapers                # list images that are not pixel art
waypixel query                             # <name> <w>x<h> <file|color #RRGGBB> <state> [<anchor>]
waypixel still out.png [--output DP-1]     # first frame as PNG (default: largest output)
waypixel pause | resume | kill
```

- `<state>`: `playing`, `hidden`, `paused` or `static`.
- The image fills the screen; overflow is cut. `--anchor`: `center` (default),
  `top`, `bottom`, `left`, `right`, `topleft`, `topright`, `bottomleft`,
  `bottomright`.
- Exit codes: 0 ok, 1 error, 2 usage error, 3 daemon not running.

State: `$XDG_STATE_HOME/waypixel/state` (restored at start).
Cache: `$XDG_CACHE_HOME/waypixel/` (unused files are deleted).

## Transitions

| Name | Effect |
|---|---|
| `burn` (default) | Fire spreads from a random point. Smoke rises. |
| `sweep` | Grey shimmer right to left, colour shimmer back. |
| `slide` | New wallpaper comes in from the left, in bands. |
| `wipe` | Diagonal colour band, top left to bottom right. |
| `decrypt` | Green changing symbols flood the screen, then a second flood shows the new wallpaper. |
| `pour` | Tiles fall and fill the screen from the bottom. |
| `smoke` | Smoke floods the screen from a random point. |
| `vhstape` | Lines lose colour and jump, noise, then each line flashes to the new wallpaper. |
| `rain` | Drops fall and fill the screen from the bottom. |
| `disabled` | No transition. |

Default in `$XDG_CONFIG_HOME/waypixel/config` (read at each change; `--fx` has priority):

```
transition = sweep
```

## Lock screen

After each change, the daemon writes the first frame of the largest output to
`$XDG_CONFIG_HOME/.lockscreen.png`. Example for hyprlock:

```
background {
    path = ~/.config/.lockscreen.png
}
```

## Start at login

`install.sh` does this. Manually:

```sh
cp waypixel.service ~/.config/systemd/user/    # edit ExecStart if needed
systemctl --user enable --now waypixel.service
```

The compositor must export `WAYLAND_DISPLAY` to the systemd user environment.
Logs: `journalctl --user -u waypixel`.

## Tests

```sh
cargo test
cargo test --release bench_fill_4k -- --ignored --nocapture
```
