# pixelwp: spec and plan

## Context
pixelawww (an awww fork) shows pixel art pixel-perfect, but the daemon uses about 150 MB of RAM. macawww uses about 10 MB. The causes:
- The client sends frames that are already scaled to screen size.
- The daemon keeps every compressed frame at screen size.
- The daemon always keeps 2 or more full-size buffers per output, and always damages the full surface.

A benchmark on a 9800X3D measured an integer upscale with u32 fill at 0.3 ms for a full 4K frame. So the daemon can keep the art at 1× and scale only the changed areas on each tick. The benchmark buffer fitted in the L3 cache, so the figure must be measured again on a real shm buffer (§15).

pixelwp is a new, minimal rewrite in a new repo, `~/git/pixelwp`. It drops transitions, filters, resize modes, namespaces, SVG, JXL and clap.

User decisions:
- Pixel art always gets an integer scale, with no aspect-fill fallback. The overflow is cut. This differs from macawww, which falls back to aspect fill when k > 1.25 × the fill scale.
- Other images are stills only. They use aspect fill done by the compositor through `wp_viewporter`, which filters smoothly on the GPU.
- No D-Bus. The only pause signals are missing frame callbacks (`hidden`) and manual `pause`.

---

## 1. Goal and non-goals
Goal: the lowest CPU, RAM and power use for animated pixel-art wallpapers on wlroots-type compositors (that is, compositors with `zwlr_layer_shell_v1`).

Non-goals (v1):
- Animations that are not pixel art.
- Transitions, playlists, filters, resize modes, namespaces.
- SVG, JXL, video.
- A file picker, an installer, a config file, a man page.
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
One Rust binary, `pixelwp`, with three roles:
- **Daemon** (`pixelwp daemon`). It owns the Wayland connection, the outputs, the players and the cache reader. It is single-threaded, with one `poll` loop on these fds: Wayland, the IPC listener, the IPC client connections, one timerfd, a signalfd, and one pidfd for each running decode child.
- **Client** (all other subcommands). It sends one request and prints the reply.
- **Decode child** (`pixelwp decode <src> <dst>`, internal). It runs the `image` crate and writes the cache file. All decode memory goes away when the child exits. The daemon never decodes.

Data flow:
```
file ─(child: image crate)─▶ pixel-art detect ─▶ 1× frames ─▶ tile diff + LZ4 ─▶ cache .pwp
                                                                     │ mmap (daemon)
                                                                     ▼
                     Player (1× canvas, frame k) ─▶ Target (one per output) ─u32 fill of dirty rects─▶ wl_shm buffer
```
- **Player:** there is one per distinct file, holding the mmap, the 1× canvas, the frame index and the deadline.
- **Target:** there is one per output. It holds the buffer pool, k and the offsets. A `wl_buffer` is never attached to more than one surface, because Wayland makes `wl_buffer.release` undefined in that case.

## 4. CLI
| Command | Action |
|---|---|
| `pixelwp daemon` | Start the daemon in the foreground. If a daemon already runs: print a message, exit 0. |
| `pixelwp img <path> [--output <name\|all>]` | Set the wallpaper. Default `all`. |
| `pixelwp clear [--color <hex>] [--output ...]` | Solid colour. Default `000000`. |
| `pixelwp query` | One line per output: `<name> <w>x<h> <file\|color #RRGGBB> <state>` |
| `pixelwp pause` / `resume` | Manual pause. |
| `pixelwp kill` | Stop the daemon. |

- `<name>` is the `wl_output` name, for example `DP-1`.
- `<state>` is one of `playing`, `hidden`, `paused` or `static`.
- Exit codes: 0 = ok, 1 = error, 2 = usage error, 3 = daemon not running.
- The argument parser is hand-written. There is no clap.
- To select a file from a directory, use a shell tool: `pixelwp img "$(fzf)"`.

## 5. IPC
- Socket: `$XDG_RUNTIME_DIR/pixelwp-<WAYLAND_DISPLAY basename>.sock`, mode 0600.
- One connection carries one request and one reply. Both are plain text.
- Request: the arguments, each ended by a NUL byte, for example `img\0/a b.gif\0all\0`. The client then shuts down its write side.
- Reply: the first line is `ok` or `error <text>`. For `query`, the output lines of §4 follow `ok`. The daemon formats them; the client prints them unchanged.
- An `img` reply waits until the file is decoded or found in the cache, and the first frame is committed. The daemon holds the connection open and replies when the decode child exits.

## 6. Decode (child) and pixel-art detection
1. Hash the file bytes with SHA-256 (`sha2`). The daemon does this before it starts the child. If a valid `<hash>.pwp` exists, the child is not started.
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
   - If N reaches 1, the image is not pixel art. An animation that is not pixel art is an error.
6. Diff each frame against the previous one in tiles of T = 8 (1× coordinates).
   - Merge the changed tiles into at most 8 rects: first row runs, then vertical merge, then a bounding-box merge of the smallest pairs (as in macawww `Diff.swift`).
   - If the dirty area is more than 50 % of the frame, store one full-frame rect.
   - Frame 0 is always a full frame. A still has only frame 0.
   - Identical frames merge: their delays add up.
7. Compress each rect, as tightly packed u32 rows, with `lz4_flex` block compression.
8. Write a temp file, `fsync` it, then `rename` it to the final name. On error, write one line to stderr and exit 1. The daemon puts that text in the reply.

## 7. Cache file `.pwp`
Path: `$XDG_CACHE_HOME/pixelwp/<sha256-hex>.pwp`. All values are little-endian.
```
Header (32 B): u32 magic 'PXWP', u16 version 1, u16 flags (bit0 pixelArt),
               u32 width, u32 height   (stored size: 1× for pixel art)
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
  - A layer surface on the `background` layer, anchored to all four edges, exclusive zone -1, no keyboard focus, namespace `pixelwp`.
  - A `wp_viewport`, and a fractional-scale object when the global exists.
- **Scale:**
  - Fractional scale (n/120) has priority over `wl_output.scale`.
  - The physical size is the logical size × scale, rounded: `(w·n + 60) / 120`.
- **Pixel art:**
  - The buffer is the physical size.
  - The viewport destination is the logical size, so 1 buffer pixel = 1 panel pixel.
  - k = `max(1, ceil(W/w), ceil(H/h))`.
  - The offset is `ox = (k·w − W)/2` and `oy = (k·h − H)/2`, with integer division.
- **Other stills (aspect fill):**
  - The buffer is the stored size. The daemon decompresses frame 0 straight into it.
  - The viewport source is the aspect-fill crop rect in `wl_fixed`, and the destination is the logical size. The compositor scales with its normal filter.
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

## 10. Pause rules
There is no pause timer. Two rules apply:

- **Frame callback rule (`hidden`):** an output is *ready* when the frame callback of its last commit has arrived.
  - At a deadline, the player advances and renders only its ready outputs. An output that is not ready skips the frame. The dirty-set rule of §9 brings it up to date later.
  - If no output of the player is ready, the player does not advance and has no deadline. Its state is `hidden`. This covers occlusion, DPMS off, the lock screen and disabled outputs.
  - When a frame callback arrives for a `hidden` player, the player sets its deadline to now.
- **Manual pause (`paused`):** `pause` removes the deadlines of all players. `resume` sets each deadline to now + delay.

When no player has a deadline, the timer is disarmed. The current frame stays on screen. The buffers stay valid, so no full render is necessary on resume.

## 11. State and start at login
- **State:** `$XDG_STATE_HOME/pixelwp/state`, one line per target: `<name|all> img <path>` or `<name|all> color <hex>`.
  - It is written with a temp file + rename on each change, and read at daemon start.
  - A new output uses its own line, or else the `all` line.
- **Start at login:** the repo contains `pixelwp.service`, a systemd user unit. The README tells the user how to copy and enable it. Its settings:
  - `Type=simple`.
  - `ExecStart=%h/.cargo/bin/pixelwp daemon`.
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
~/git/pixelwp/
  Cargo.toml  build.rs  SPEC.md  README.md  pixelwp.service
  protocols/wlr-layer-shell-unstable-v1.xml
  src/main.rs        CLI parse, dispatch, exit codes
  src/client.rs      socket client
  src/ipc.rs         socket path, request and reply format
  src/decode.rs      child: image → XRGB → pixelart → diff → cache write
  src/pixelart.rs    factor / holds / shrink / integer_scale (port of pixelawww)
  src/diff.rs        tile diff, rect merge
  src/cache.rs       .pwp writer/reader, validation, cleanup
  src/scale.rs       upscale_rect (u32 fill)
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

1. **Scaffold.** `cargo new ~/git/pixelwp`, copy the protocol XML and build.rs, get the generated bindings to compile, and implement the CLI parser and exit codes.
2. **Core algorithms (pure, unit-tested).**
   - `pixelart.rs`: port it to `u32` and keep all its tests.
   - `scale.rs`: `upscale_rect`.
   - `diff.rs`: tiles, merge to 8 or fewer rects, the 50 % rule.
3. **Cache + decode child.** Write and read `.pwp`, decode, validate, clean up. The `pixelwp decode` subcommand.
4. **Daemon skeleton.**
   - Poll loop with signalfd, timerfd and the IPC server.
   - Wayland globals, outputs and layer surfaces.
   - Colour wallpaper with a 1×1 buffer and viewport.
   - `query`, `kill`, `pause` and `resume`.
5. **Static images.** Start the child, wait on its pidfd, mmap the cache. Pixel-art Target with a full upscale; aspect-fill Target with the viewport source. Buffer release and munmap when idle.
6. **Animation.** Player canvas, ring, timer. Target dirty sets, per-rect damage, buffer age.
7. **Pause and output changes.** The frame callback rule; a new Target on scale, size or output change; per-output state file and restore at start.
8. **Docs.** README and `pixelwp.service`.

## 15. Verification
- **Unit tests:**
  - **pixelart:** solid 48×32 gives 16; a 3× checker gives 3; noise gives 1; `limit` works; the decode restart with a smaller N works.
  - **Integer scale table:** 320×208 on 2560×1664 gives 8, on 2940×1912 gives 10, on 1920×1080 gives 6; 100×100 on 160×160 gives 2; art larger than the screen gives 1.
  - **`upscale_rect`:** a dirty-rect render on top of frame k−1 equals a full render of frame k, bit-exact, for random rects, k from 1 to 13, and odd offsets.
  - **Diff + LZ4 round trip:** every frame is bit-exact, for synthetic frames and fixture GIFs.
  - **Buffer age:** 1, 2 and 3 buffers over 3 loops of 1-, 2- and 3-frame animations give the same pixels as a full render. The same holds when an output skips frames.
  - **Cache:** bad magic, bad version and a truncated file are rejected. Delay clamp; identical-frame merge. An animation that is not pixel art is rejected.
- **Integration (headless):**
  - Run `sway` with `WLR_BACKENDS=headless` and an output of 3840×2160, plus a second output at scale 1.5.
  - Run `pixelwp daemon` and `pixelwp img fixture-4x.gif`.
  - Capture with `grim`. Every k×k block must be uniform, and the image must match a reference render.
- **Measurements:**
  - Fill: the time of a full 4K `upscale_rect` into a real shm buffer that the compositor reads.
  - RSS: `grep Rss /proc/$(pidof pixelwp)/smaps_rollup`.
  - CPU: `pidstat -p <pid> 1`.
  - Wakeups while paused: `perf stat -e 'syscalls:sys_enter_*'` or `strace -c`.
  - The results must meet the targets in §2. Compare with pixelawww on the same GIF.
