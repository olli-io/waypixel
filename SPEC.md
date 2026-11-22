# waypixel: spec and plan

## Context
pixelawww (an awww fork) shows pixel art pixel-perfect, but the daemon uses about 150 MB of RAM. macawww uses about 10 MB. The causes:
- The client sends frames that are already scaled to screen size.
- The daemon keeps every compressed frame at screen size.
- The daemon always keeps 2 or more full-size buffers per output, and always damages the full surface.

A benchmark on a 9800X3D measured an integer upscale with u32 fill at 0.3 ms for a full 4K frame. So the daemon can keep the art at 1× and scale only the changed areas on each tick. The benchmark buffer fitted in the L3 cache, so the figure must be measured again on a real shm buffer (§15).

waypixel is a new, minimal rewrite in a new repo, `~/git/waypixel`. It drops transitions (but for those of §9a), filters, resize modes, namespaces, SVG, JXL and clap.

User decisions:
- Pixel art always gets an integer scale, with no aspect-fill fallback. The overflow is cut. This differs from macawww, which falls back to aspect fill when k > 1.25 × the fill scale.
- Other images are rejected. The client prints the error and runs `notify-send` to show a desktop notification. If `notify-send` is not installed, there is no notification.
- No D-Bus in waypixel. The only pause signals are missing frame callbacks (`hidden`) and manual `pause`.

---

## 1. Goal and non-goals
Goal: the lowest CPU, RAM and power use for animated pixel-art wallpapers on wlroots-type compositors (that is, compositors with `zwlr_layer_shell_v1`).

Non-goals (v1):
- Images and animations that are not pixel art.
- Playlists, filters, resize modes, namespaces. Transitions other than those of §9a.
- SVG, JXL, video.
- A file picker, an installer, a man page. A config file with more than the `transition` key.
- GNOME, which has no layer shell.

## 2. Targets
| Item | Target |
|---|---|
| Static image, idle | 0 % CPU, < 8 MB RSS after the compositor releases the buffer |
| 4K pixel-art GIF, small diffs | < 0.5 % CPU, < 45 MB RSS (1 buffer of 33 MB; 2 buffers worst case) |
| Paused / hidden | 0 wakeups |
| Tick path | no allocation; one `sendmsg` and one `timerfd_settime` for each tick |
| Binary | < 3 MB, release, stripped |

## 3. Architecture
One Rust binary, `waypixel`, with three roles:
- **Daemon** (`waypixel daemon`). It owns the Wayland connection, the outputs, the players and the cache reader. It is single-threaded, with one `poll` loop on these fds: Wayland, the IPC listener, the IPC client connections, one timerfd, a signalfd, and one pidfd for each running decode child.
- **Client** (all other subcommands). It sends one request and prints the reply.
- **Decode child** (`waypixel decode <src> <dst>`, internal). It runs the `image` crate and writes the cache file. All decode memory goes away when the child exits. The daemon never decodes.

Data flow:
```
file ─(child: image crate)─▶ pixel-art detect ─▶ 1× frames ─▶ tile diff + LZ4 ─▶ cache .wpx
                                                                     │ mmap (daemon)
                                                                     ▼
                     Player (1× canvas, frame k) ─▶ Target (one per output) ─u32 fill of dirty rects─▶ wl_shm buffer
```
- **Player:** there is one per distinct file, holding the mmap, the 1× canvas, the frame index and the deadline.
- **Target:** there is one per output. It holds the buffer pool, k and the offsets. A `wl_buffer` is never attached to more than one surface, because Wayland makes `wl_buffer.release` undefined in that case.

## 4. CLI
| Command | Action |
|---|---|
| `waypixel daemon` | Start the daemon in the foreground. If a daemon already runs: print a message, exit 0. |
| `waypixel img <path> [--output <name\|all>] [--fx <name>] [--anchor <name>]` | Set the wallpaper. Default `all`. `--fx`: see §9a. `--anchor`: see §8. |
| `waypixel clear [--color <hex>] [--output ...] [--fx ...]` | Solid colour. Default `000000`. |
| `waypixel query` | One line per output: `<name> <w>x<h> <file\|color #RRGGBB> <state> [<anchor>]`. The anchor is shown only when it is not `center`. |
| `waypixel check [<dir>]` | One line for each image of the directory that `img` rejects: `<path>: <reason>`. Default: the current directory. Not recursive. The client does this, without the daemon and without a cache file. Exit code 1 when a line is printed. |
| `waypixel still <file.png> [--output <name>]` | Write frame 0 of one output as a PNG. Default: the largest output. The client does this: it reads `query` and the cache file. An image has the output size and the integer scale; a colour is 1×1. |
| `waypixel pause` / `resume` | Manual pause. |
| `waypixel kill` | Stop the daemon. |

- `<name>` is the `wl_output` name, for example `DP-1`.
- `<state>` is one of `playing`, `hidden`, `paused` or `static`.
- Exit codes: 0 = ok, 1 = error, 2 = usage error, 3 = daemon not running.
- The argument parser is hand-written. There is no clap.
- To select a file from a directory, use a shell tool: `waypixel img "$(fzf)"`.

## 5. IPC
- Socket: `$XDG_RUNTIME_DIR/waypixel-<WAYLAND_DISPLAY basename>.sock`, mode 0600.
- One connection carries one request and one reply. Both are plain text.
- Request: the arguments, each ended by a NUL byte, for example `img\0/a b.gif\0all\0config\0center\0`. The arguments after the target are the transition name (or `config`) and the anchor. The client then shuts down its write side.
- Reply: the first line is `ok` or `error <text>`. For `query`, the output lines of §4 follow `ok`. The daemon formats them; the client prints them unchanged.
- An `img` reply waits until the file is decoded or found in the cache, and the first frame is committed. The daemon holds the connection open and replies when the decode child exits.

## 6. Decode (child) and pixel-art detection
1. Hash the file bytes with SHA-256 (`sha2`). The daemon does this before it starts the child. If a valid `<hash>.wpx` exists, the child is not started.
2. Decode with `image` 0.25 (features gif, png, jpeg, webp).
   - GIF, APNG and animated WebP give composited full-canvas RGBA frames.
   - Still images apply the EXIF orientation.
   - The maximum size is 16384 px per side. A larger still is scaled down once. A larger animation is an error.
3. Convert each pixel to XRGB8888 as a `u32` (`0xFF<<24 | r<<16 | g<<8 | b`), composited over black (`c·a/255`). After this step, all fully transparent pixels compare equal.
4. Frame delay: a delay under 20 ms becomes 100 ms. The loop count is ignored: an animation always loops.
5. Pixel art, using the code in pixelawww `client/src/pixelart.rs`, adapted to `&[u32]`:
   - N = the GCD of all run lengths of equal pixels in the rows and columns of frame 0.
   - If N > 1, keep one pixel per N×N block.
   - Check each later frame with `holds(N)`. If a frame fails, set N = `factor(frame, limit = N)` and restart the decode.
   - If N reaches 1, the image is pixel art stored at 1× when no side is longer than 640 px and all frames together have 256 colours or fewer. If not, the image is not pixel art, which is the error `the image is not true pixel art`.
6. Diff each frame against the previous one in tiles of T = 8 (1× coordinates).
   - Merge the changed tiles into at most 8 rects: first row runs, then vertical merge, then a bounding-box merge of the smallest pairs (as in macawww `Diff.swift`).
   - If the dirty area is more than 50 % of the frame, store one full-frame rect.
   - Frame 0 is always a full frame. A still has only frame 0.
   - Identical frames merge: their delays add up.
7. Compress each rect, as tightly packed u32 rows, with `lz4_flex` block compression.
8. Write a temp file, `fsync` it, then `rename` it to the final name. On error, write one line to stderr and exit 1. The daemon puts that text in the reply.

## 7. Cache file `.wpx`
Path: `$XDG_CACHE_HOME/waypixel/<sha256-hex>.wpx`. All values are little-endian.
```
Header (32 B): u32 magic 'WPXL', u16 version 3, u16 flags (0),
               u32 width, u32 height   (stored size: 1×)
               u32 frameCount, u32 reserved (0), u64 indexOffset
Payload: LZ4 blocks, 16-byte aligned
Index (frameCount entries): u32 delayMs, u32 rectCount,
               rectCount × { u16 x, u16 y, u16 w, u16 h, u64 offset, u32 size }
```
- The daemon reads the file with `mmap` (`PROT_READ`, `MAP_PRIVATE`). It checks magic, version and bounds. A bad file is deleted and decoded again.
- **Cleanup:** at daemon start (after the state is restored) and after each successful `img`, the daemon deletes every cache file that no output uses.
- Bump the version for any format change.

## 8. Wayland surfaces
- Required globals: `wl_compositor`, `wl_shm`, `zwlr_layer_shell_v1` and `wp_viewporter`. Optional: `wp_fractional_scale_manager_v1`. `wl_output` must be version 4 or higher.
- Code generation: `waybackend` + `waybackend-scanner`. Copy `daemon/build.rs`, `daemon/src/wayland.rs` and `protocols/wlr-layer-shell-unstable-v1.xml` from pixelawww.
- **Per output:**
  - A `wl_surface` with an empty input region.
  - A layer surface on the `background` layer, anchored to all four edges, exclusive zone -1, no keyboard focus, namespace `waypixel`.
  - A `wp_viewport`, and a fractional-scale object when the global exists.
- **Scale:**
  - Fractional scale (n/120) has priority over `wl_output.scale`.
  - The physical size is the logical size × scale, rounded: `(w·n + 60) / 120`.
- **Pixel art:**
  - The buffer is the physical size.
  - The viewport destination is the logical size, so 1 buffer pixel = 1 panel pixel.
  - k = `max(1, ceil(W/w), ceil(H/h))`.
  - The offset is `ox = (k·w − W)·ax/2` and `oy = (k·h − H)·ay/2`, with integer division. The anchor gives `ax` and `ay`: 0 = left / top, 1 = centre, 2 = right / bottom.
- **Anchor:** `--anchor <center|top|bottom|left|right|topleft|topright|bottomleft|bottomright>`. The default is `center`. `top-left` and `top_left` are also correct. The anchor is part of the wallpaper of an output: a transition uses it, and `still` reads it from `query`.
- **Colour:** a 1×1 buffer with the viewport destination at the logical size. It uses almost no memory.
- **Changes:** on `configure` or a scale event, the daemon builds the Target of that output again and renders it in full. On `global_remove`, it destroys the Target. There is no debounce, because Wayland events come in batches ending in `done` or `configure`.

## 9. Playback
### Player
- `canvas: Box<[u32]>` holds frame k at the stored size. It exists only for animations and is small, for example 320×180 = 230 KB.
- A ring with the rect lists of the last R = 8 frames.
- To advance to frame k: decompress the rects of diff k into `canvas`. Frame 0 is full. Use `lz4_flex::block::decompress_into` into a scratch buffer that is allocated once at the size of the largest rect, then copy the rows. A rect that is full-width goes straight into the canvas.

### Target buffers (simplified `bump_pool.rs`)
- One memfd pool. Buffers are added only when the compositor still holds all the existing ones. Wlroots normally releases shm buffers right after upload, so 1 buffer is normal.
- Each buffer stores `holds: Option<frame index>`.
- **Still or colour:** when the compositor sends `release`, destroy the `wl_buffer`s and munmap the pool. The player releases its mmap. If the output changes later, the daemon opens the cache file again.

### Render a Target for frame k
The fill is fast, so the dirty rects do not save much daemon CPU. Their purpose is small damage, which keeps the texture upload in the compositor small.

1. Take a free buffer `b`.
2. Find the dirty set:
   - If `b.holds = j` and `k − j ≤ R`, dirty = the union of the rects of frames j+1..k from the ring.
   - Otherwise, dirty = the full frame.
3. For each dirty rect (1× coordinates):
   - Map it to the screen: `[x·k − ox, (x+w)·k − ox)`, clipped to `[0, W)`. Do the same for y.
   - Call `upscale_rect`, which uses u32 fill:
     - For each 1× row in the rect, build the first screen row of the rect width with `fill` on runs of k pixels. The first and last runs are partial at the screen edges.
     - Then `copy_within` that row for the next k − 1 rows, clipped.
     - There is no division per pixel.
4. Damage with `wl_surface.damage_buffer` for each rect. The damage covers the union of the rects since the last committed frame of the surface (full when the ring cannot cover it). Then attach the buffer, request a frame callback, and commit.
5. Set `b.holds = k`.

### Timer
- One timerfd (`CLOCK_MONOTONIC`, absolute time) set to the earliest deadline of all running players.
- The next deadline = the previous deadline + the delay, so the timing does not drift. If the daemon is more than one delay late, it uses now + delay.

## 9a. Transitions
`img` and `clear` show the change with a transition. The transitions are effects of ttfx, changed from terminal symbols to pixel-art cells. A restore at daemon start has no transition.

- **Selection:** `--fx <burn|sweep|slide|wipe|decrypt|pour|smoke|vhstape|rain|disabled>`. Without `--fx`, the daemon reads the key `transition = <name>` from `$XDG_CONFIG_HOME/waypixel/config` at each request. The default is `burn`. A bad value gives a log line and `burn`. Lines without `=` and text after `#` are ignored.
- **Sweep:** a shimmer of random greys goes from right to left in 40 steps (`in_out_quad`) and leaves the old wallpaper in grey. Then a shimmer of random colours (`8A008A` … `FFFFFF`) goes from left to right and leaves the new wallpaper. A cell shimmers for 4 steps.
- **Slide:** bands of 6 rows. Band b starts at step ⌊3b / 4⌋ + 1 and moves for 24 steps (`in_out_quad`): the new wallpaper comes in from the left.
- **Wipe:** the diagonals start from the top left to the bottom right in 50 steps (`in_out_quad`). A cell shows the colours `833AB4` … `FCB045` for 4 steps, then the new wallpaper.
- **Decrypt:** a symbol is 4 × 6 cells: a glyph of 3 × 5 cells on black, with a gap of 1 cell at the right and at the bottom. The symbols start in the order of their depth in a random spanning tree of the symbol grid, as in smoke, at the same rate, in 30 steps. A new symbol shows a random glyph in white for 3 steps, then in one of the greens `008000`, `00CB00`, `00FF00`. A green symbol gets a new glyph and green with a probability of 1/4 for each step. The end steps come from a second such order from a different random symbol: from step 16, at the same rate, in 40 steps. A symbol ends 14 steps after its start at the earliest. Before its end step the glyph of a symbol is white for 3 steps. Then the symbol shows the new wallpaper.
- **Pour:** tiles of 15 × 15 cells, from the bottom row up. Each row has the opposite direction to the row below. 7 tiles start in each step. A tile falls from above the top row for 10 to 15 steps (`in_quad`). It shows the new wallpaper of its position, moved up. At its position it shows the new wallpaper.
- **Smoke:** a random spanning tree grows from a random cell, each time to the adjacent cell with the lowest random weight. The cells start in the order of their depth in the tree, at a rate that starts all cells in 60 steps. A cell shows 8 colours (`242424`, `FFFFFF`, `00D1FF`, `8A008A`) for 16 steps, then the new wallpaper.
- **Vhstape:** lines of 6 rows. For 50 steps: a wave of 3 lines (moved 24, 42 and 24 cells to the left) goes down in most steps, and starts again in the top half when it gets to the bottom. With a probability of 1/6 for each step, a random line jumps 12 to 60 cells to the left for 8 to 25 steps (3 lines at most). A moved line shows the old wallpaper in grey, and black at its end. While it moves (10 cells for each step), it shows only the red, green or blue channel. With a probability of 1/30 for each step, noise (6 greys, runs of 1 to 4 cells, new each 2 steps) covers all lines for 6 steps. Then all lines go back, noise covers all lines for 14 steps. Then each line gets a random end step in a range of 24 steps. Before its end step a line is white for 3 steps. Then the line shows the new wallpaper. The other lines continue to show noise.
- **Rain:** tiles of 6 × 6 cells, from the bottom row up, in a random order across 3 rows. 68 drops start in each step. A drop is one column of 1 to 5 cells. It shows the new wallpaper of its tile, moved up. It falls from the top to its tile for 10 to 17 steps (`in_quart`). Then the tile shows the new wallpaper.

The items below describe burn, and the parts that all transitions share.

- **Grid:** 540 × 270 cells. Each output maps the grid to its screen as for pixel art: integer scale, centred, overflow cut.
- **Order:** the fire starts at a random cell. The cells start to burn in the link order of the simple Prim's algorithm (a random spanning tree), at a rate that burns all cells in approximately 2 s.
- **Cell:** old wallpaper → 8 fire colours (`FFFFFF` … `510100`, 11 steps in total) → new wallpaper. One burnt cell in 64 emits a smoke cell that rises for 16 steps (`504F4F` … `C7C7C7`).
- **Step:** 33 ms, on the timerfd. A step has no allocation.
- **Images:** the old and the new wallpaper are 1× canvases, each with its own integer scale. A colour is a 1 × 1 canvas. An animation plays during the transition: the old one keeps its player until the last step, and the new one starts at frame 0 with the transition. Thus two players can run for the time of a transition. The players only advance their canvases; the steps draw the frames. A player of a transition does not become `hidden`.
- **Render:** each cell has a display code (old, new, old in grey, new moved by n cells, old in grey moved by n cells with a tint, or a colour) and the step of its last change. A buffer records the step and the frame of each player that it holds. A buffer that holds step j gets the cells that changed after j. For each rect that changed in an image after the frame that the buffer holds, it also gets the cells that show that image there. For slide, a rect of the new image draws again the full width of the moved bands in its rows. For vhstape, a rect of the old image does the same for the moved lines. The damage is the bounding box of the cells that changed in the step, and one bounding box for each such rect.
- **Grey:** for sweep and vhstape, the grey image gets the rects that changed in the old image.
- **Output:** during the transition, the output has a buffer pool of the physical size. A `configure` or a scale change ends the transition on that output. After the last step, the output builds its normal Target.
- Manual pause does not stop a transition. An output that is not ready skips steps.
- **Block:** the daemon does not switch wallpapers during a transition. The daemon ignores an `img` or `clear` for a target with an output in a transition, and an `img` whose decode ends during such a transition. The reply is `ok`. The state does not change. A restore is not ignored.

## 10. Pause rules
There is no pause timer. Two rules apply:

- **Frame callback rule (`hidden`):** an output is *ready* when the frame callback of its last commit has arrived.
  - At a deadline, the player advances and renders only its ready outputs. An output that is not ready skips the frame. The dirty-set rule of §9 brings it up to date later.
  - If no output of the player is ready, the player does not advance and has no deadline. Its state is `hidden`. This covers occlusion, DPMS off, the lock screen and disabled outputs.
  - When a frame callback arrives for a `hidden` player, the player sets its deadline to now.
- **Manual pause (`paused`):** `pause` removes the deadlines of all players. `resume` sets each deadline to now + delay.

When no player has a deadline, the timer is disarmed. The current frame stays on screen. The buffers stay valid, so no full render is necessary on resume.

## 11. State and start at login
- **Lock screen image:** after each `img` or `clear` that changes the state, the daemon starts `waypixel still $XDG_CONFIG_HOME/.lockscreen.png` as a child. One child runs at a time. A change during its run starts a new child when it exits. A restore at start does not write the file.
- **State:** `$XDG_STATE_HOME/waypixel/state`, one line per target: `<name|all> img <anchor> <path>` or `<name|all> color <hex>`. A line without an anchor (the path starts with `/`) has the anchor `center`.
  - It is written with a temp file + rename on each change, and read at daemon start.
  - A new output uses its own line, or else the `all` line.
- **Start at login:** the repo contains `waypixel.service`, a systemd user unit. The README tells the user how to copy and enable it. Its settings:
  - `Type=simple`.
  - `ExecStart=%h/.local/bin/waypixel daemon`.
  - `Restart=on-failure`.
  - `PartOf=` and `After=graphical-session.target`.
  - `WantedBy=graphical-session.target`.
- **Signals:** SIGINT, SIGTERM and SIGHUP through signalfd lead to a clean exit, which unlinks the socket.

## 12. Errors and logs
- Client: one line on stderr, and the exit code from §4.
- Daemon: log to stderr, which goes to the journal under systemd. Nothing is logged on the tick path.
- Decode error: keep the old wallpaper and reply with the error.

## 13. Source layout and dependencies
```
~/git/waypixel/
  Cargo.toml  build.rs  SPEC.md  README.md  waypixel.service
  protocols/wlr-layer-shell-unstable-v1.xml
  src/main.rs        CLI parse, dispatch, exit codes
  src/client.rs      socket client
  src/ipc.rs         socket path, request and reply format
  src/decode.rs      child: image → XRGB → pixelart → diff → cache write
  src/pixelart.rs    factor / holds / shrink / integer_scale (port of pixelawww)
  src/diff.rs        tile diff, rect merge
  src/cache.rs       .wpx writer/reader, validation, cleanup
  src/scale.rs       upscale_rect (u32 fill)
  src/fx.rs          transitions: cell grid, the 10 effects, render
  src/daemon/mod.rs      poll loop, IPC server, state file, signals
  src/daemon/wayland.rs  generated protocol include (from pixelawww)
  src/daemon/output.rs   surfaces, viewport, scale, Target
  src/daemon/pool.rs     shm pool, buffers, release
  src/daemon/player.rs   canvas, ring, timer, pause
```
- Dependencies: `waybackend`, `waybackend-scanner` (build), `rustix`, `image` (gif, png, jpeg, webp only), `lz4_flex` and `sha2`. No clap, no serde, no C liblz4, no libc.
- Copy from pixelawww: `common/src/mmap.rs` (memfd), `daemon/src/wayland.rs` and `daemon/build.rs`. Port `client/src/pixelart.rs` and its tests.

---

## 14. Implementation plan
Each phase ends with passing `cargo test` and `cargo clippy`.

1. **Scaffold.** `cargo new ~/git/waypixel`, copy the protocol XML and build.rs, get the generated bindings to compile, and implement the CLI parser and exit codes.
2. **Core algorithms (pure, unit-tested).**
   - `pixelart.rs`: port it to `u32` and keep all its tests.
   - `scale.rs`: `upscale_rect`.
   - `diff.rs`: tiles, merge to 8 or fewer rects, the 50 % rule.
3. **Cache + decode child.** Write and read `.wpx`, decode, validate, clean up. The `waypixel decode` subcommand.
4. **Daemon skeleton.**
   - Poll loop with signalfd, timerfd and the IPC server.
   - Wayland globals, outputs and layer surfaces.
   - Colour wallpaper with a 1×1 buffer and viewport.
   - `query`, `kill`, `pause` and `resume`.
5. **Static images.** Start the child, wait on its pidfd, mmap the cache. Pixel-art Target with a full upscale. Buffer release and munmap when idle.
6. **Animation.** Player canvas, ring, timer. Target dirty sets, per-rect damage, buffer age.
7. **Pause and output changes.** The frame callback rule; a new Target on scale, size or output change; per-output state file and restore at start.
8. **Docs.** README and `waypixel.service`.

## 15. Verification
- **Unit tests:**
  - **pixelart:** solid 48×32 gives 16; a 3× checker gives 3; noise gives 1; `limit` works; the colour limit of art stored at 1× works; the decode restart with a smaller N works.
  - **Integer scale table:** 320×208 on 2560×1664 gives 8, on 2940×1912 gives 10, on 1920×1080 gives 6; 100×100 on 160×160 gives 2; art larger than the screen gives 1.
  - **`upscale_rect`:** a dirty-rect render on top of frame k−1 equals a full render of frame k, bit-exact, for random rects, k from 1 to 13, and odd offsets.
  - **Diff + LZ4 round trip:** every frame is bit-exact, for synthetic frames and fixture GIFs.
  - **Buffer age:** 1, 2 and 3 buffers over 3 loops of 1-, 2- and 3-frame animations give the same pixels as a full render. The same holds when an output skips frames.
  - **Cache:** bad magic, bad version and a truncated file are rejected. Delay clamp; identical-frame merge. An image that is not pixel art is rejected. Art stored at 1× is accepted.
- **Integration (headless):**
  - Run `sway` with `WLR_BACKENDS=headless` and an output of 3840×2160, plus a second output at scale 1.5.
  - Run `waypixel daemon` and `waypixel img fixture-4x.gif`.
  - Capture with `grim`. Every k×k block must be uniform, and the image must match a reference render.
- **Measurements:**
  - Fill: the time of a full 4K `upscale_rect` into a real shm buffer that the compositor reads.
  - RSS: `grep Rss /proc/$(pidof waypixel)/smaps_rollup`.
  - CPU: `pidstat -p <pid> 1`.
  - Wakeups while paused: `perf stat -e 'syscalls:sys_enter_*'` or `strace -c`.
  - The results must meet the targets in §2. Compare with pixelawww on the same GIF.
